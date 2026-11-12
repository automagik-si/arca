//! The GitLab pool's controller (`templates/gitlab-runner-pool.sh run`) against a fake GitLab and a fake `docker`.
//!
//! The controller is the shipped script, unchanged: only what it reaches outside itself is replaced. GitLab is a small
//! HTTP server in this file that answers the one call the controller makes, the token check, and keeps what it was
//! sent. `docker` is a script on the controller's `PATH` (`tests/gitlab_runner_pool/fake-docker.sh`) that keeps
//! containers as directories; it plays no GitLab Runner, so a test writes the runner's output lines and makes the
//! jobs' containers itself. `curl` is the real one behind a wrapper that records its arguments and can end with an
//! exit status a test names, for the failures a test cannot make for real (a name that does not resolve, a
//! certificate that is refused, a connection that times out). `df` and `getent` are stand-ins too.
//!
//! The controller's `PATH` holds those stand-ins and the programs the script declares in `POOL_PROGRAMS`, and nothing
//! else. A program the script starts without declaring it is therefore not found, bash reports it to a function this
//! test hands it, and `Pool::stop` fails on the report.
//!
//! So this proves the controller's own logic: the settings and their refusals, the sum of the jobs' memory against
//! the machine, the tokens (which registration needs which secret, a refused one, a changed one), what is written to
//! `config.toml` and with which modes, the words for a GitLab that cannot be reached, the status block, and that no
//! credential reaches any output or any command line. It does not prove Docker, GitLab Runner, GitLab or a tailnet;
//! `docs/OPERATING.md` lists what stays unproven.
//!
//! The test needs `bash`, `curl` and `jq` on the machine that runs it. Every credential is made here.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const RUNNER_IMAGE: &str = "registry.example/gitlab-runner@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const HELPER_IMAGE: &str = "registry.example/gitlab-runner-helper@sha256:2222222222222222222222222222222222222222222222222222222222222222";
const TAILNET_IMAGE: &str = "registry.example/tailscale@sha256:3333333333333333333333333333333333333333333333333333333333333333";
const RUNNER: &str = "pool1-gitlab-runner";
const TAILNET: &str = "pool1-tailnet";
const MIB: u64 = 1 << 20;
/// What a machine of 32GiB reports: 27921MiB are left for jobs once the 4GiB of `reserveMemory` are kept back.
const REPORTED_32GIB: u64 = (27921 + 4096) * MIB;
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A runner authentication token as GitLab makes them, with a tail of this test's own.
fn token(tail: &str) -> String {
    format!("glrt-t3_synthetic0123456789{tail}")
}

// ---------------------------------------------------------------------------------------------------------------------
// The fake GitLab
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// The tokens GitLab takes.
    accepted: Vec<String>,
    /// A status every token check is answered with, whatever the token.
    status: Option<u16>,
    /// Take the connection and say nothing.
    silent: bool,
    /// Every token check: the token and the system id it was sent with.
    seen: Vec<(String, String)>,
    /// Requests that were not the token check.
    other: Vec<String>,
    /// Whether GitLab keeps every token check without an answer: a signal sent meanwhile finds the controller's curl
    /// running. `held` counts the checks that came to wait, `passed` the ones let go.
    hold: bool,
    held: usize,
    passed: usize,
    /// A check was kept so long that it was answered without the test's word: the test was too slow, not the pool.
    hold_ran_out: bool,
}

struct Gitlab {
    url: String,
    port: u16,
    state: Arc<Mutex<State>>,
}

impl Gitlab {
    fn start(accepted: &[String]) -> Gitlab {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("the address").port();
        let state = Arc::new(Mutex::new(State {
            accepted: accepted.to_vec(),
            ..State::default()
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Gitlab {
            url: format!("http://127.0.0.1:{port}/api/v4"),
            port,
            state,
        }
    }

    fn change(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().expect("the state"));
    }

    fn seen(&self) -> Vec<(String, String)> {
        self.state.lock().expect("the state").seen.clone()
    }

    /// How many token checks have come to wait for the test's word.
    fn held(&self) -> usize {
        self.state.lock().expect("the state").held
    }

    /// Lets every check that waits now have its answer; the ones that come later wait again.
    fn let_go(&self) {
        self.change(|state| state.passed = state.held);
    }

    /// Keeps no check any longer.
    fn release(&self) {
        self.change(|state| {
            assert!(
                !state.hold_ran_out,
                "a token check was held for {HOLD_SECONDS} seconds before the test went on: this test was too slow, \
                 the pool was not"
            );
            state.hold = false;
        });
    }
}

/// The seconds a token check is kept at most: less than the thirty the controller's curl gives an answer.
const HOLD_SECONDS: u64 = 25;

/// Keeps a token check the test wants kept until the test lets it go, and not for ever.
fn hold(state: &Mutex<State>) {
    let deadline = Instant::now() + Duration::from_secs(HOLD_SECONDS);
    let mut place = None;
    loop {
        {
            let mut state = state.lock().expect("the state");
            if !state.hold {
                return;
            }
            if place.is_none() {
                state.held += 1;
                place = Some(state.held);
            }
            if place.is_some_and(|place| place <= state.passed) {
                return;
            }
            if Instant::now() >= deadline {
                state.hold_ran_out = true;
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let request = lines.next().unwrap_or("").to_string();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    if state.lock().expect("the state").silent {
        // Longer than the controller waits in these tests, and not for ever.
        std::thread::sleep(Duration::from_secs(4));
        return;
    }
    if request == "POST /api/v4/runners/verify HTTP/1.1" {
        hold(state);
    }
    let (code, answer) = {
        let mut state = state.lock().expect("the state");
        if request != "POST /api/v4/runners/verify HTTP/1.1" {
            state.other.push(request);
            (404, json!({ "error": "404 Not Found" }))
        } else {
            let sent: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let token = sent["token"].as_str().unwrap_or("").to_string();
            let system_id = sent["system_id"].as_str().unwrap_or("").to_string();
            state.seen.push((token.clone(), system_id));
            match state.status {
                Some(code) => (code, json!({ "message": "the answer this test asked for" })),
                // GitLab's answer repeats the token: the controller must not keep it.
                None if state.accepted.contains(&token) => (
                    200,
                    json!({ "id": 12345, "token": token, "token_expires_at": null }),
                ),
                None => (403, json!({ "message": "403 Forbidden" })),
            }
        }
    };
    let text = answer.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// Bounded waits
// ---------------------------------------------------------------------------------------------------------------------

/// The seconds a helper program is given, and the seconds the whole file is given. No wait in this file is without a
/// bound: a test that hangs holds a runner of the pool that runs it until somebody cancels the job.
const HELPER_SECONDS: u64 = 60;
const FILE_SECONDS: u64 = 600;

/// The seconds a controller is given to stop after SIGTERM. It is a bound and no judgement of speed: the controller
/// ends the pass it is in before it reads the signal, and a pass starts many programs, so on a loaded machine a stop
/// that is in order took longer than the twenty seconds this once was. A controller that never stops still fails here.
const STOP_SECONDS: u64 = 120;

/// The seconds a killed controller is given to be gone, and an ended helper's output to end. Bounds as well: each
/// was ten seconds.
const GONE_SECONDS: u64 = 60;

/// The controllers that are running, by process id, so the bound on the whole file can end them.
static CONTROLLERS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Runs a helper program to its end and returns what it wrote, or says why not once `seconds` have passed.
fn try_finish(command: &mut Command, seconds: u64) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("did not start: {error}"))?;
    let (sender, receiver) = mpsc::channel();
    let mut stdout = child.stdout.take().expect("the output");
    let mut stderr = child.stderr.take().expect("the errors");
    let errors = sender.clone();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = sender.send((true, bytes));
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        let _ = errors.send((false, bytes));
    });
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("could not be waited for: {error}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.try_wait();
            return Err(format!("did not end within {seconds} seconds"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    for _ in 0..2 {
        match receiver.recv_timeout(Duration::from_secs(GONE_SECONDS)) {
            Ok((true, bytes)) => output.stdout = bytes,
            Ok((false, bytes)) => output.stderr = bytes,
            Err(_) => {
                return Err(
                    "ended, and its output did not: something it started still holds it"
                        .to_string(),
                );
            }
        }
    }
    Ok(output)
}

/// Runs a helper program to its end, or fails the test with `what` and the reason.
fn finish(command: &mut Command, what: &str) -> Output {
    try_finish(command, HELPER_SECONDS).unwrap_or_else(|why| panic!("{what} {why}"))
}

/// Sends a signal to a process or, with a negative number, to a whole process group. bash's own `kill` does it and
/// not the `kill` program: that program, from procps-ng up to 4.0.4, reads only the first digit of a negative number
/// that follows a signal option, and signals some other group or every process of the user.
fn signal(name: &str, target: i64) -> bool {
    try_finish(
        Command::new("bash")
            .arg("-c")
            .arg(format!("kill -{name} -- {target}")),
        HELPER_SECONDS,
    )
    .is_ok_and(|output| output.status.success())
}

/// Whether a controller still holds its stop line open: whether the one program it holds the line through, a `sleep`
/// that is the controller's own child, is still running. The controller's work is a child of it too, and starts no
/// `sleep` of its own: its pauses are reads of the line.
fn holds_the_stop_line(controller: u32) -> bool {
    let parent = controller.to_string();
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            // "pid (name) state parent ...": the name may hold anything, so the fields are read from its end.
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                return false;
            };
            let Some((head, rest)) = stat.rsplit_once(") ") else {
                return false;
            };
            let name = head.split_once('(').map_or("", |(_, name)| name);
            let mut fields = rest.split(' ');
            let state = fields.next();
            // One that has ended and is not yet collected has let go already.
            name == "sleep" && state != Some("Z") && fields.next() == Some(parent.as_str())
        })
}

/// Ends a controller with everything in its process group, and waits for it, but not for ever.
fn end_controller(child: &mut Child) {
    let id = child.id();
    signal("KILL", -i64::from(id));
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(GONE_SECONDS);
    while matches!(child.try_wait(), Ok(None)) {
        if Instant::now() >= deadline {
            eprintln!(
                "the controller {id} was still there {GONE_SECONDS} seconds after it was killed; it is left behind"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    CONTROLLERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|pid| *pid != id);
}

/// The bound on the whole file. Every wait below has its own bound and its own words; this one is for a wait nobody
/// thought of. It ends the controllers that are running and then the test program, and says so.
fn bound_the_file() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(FILE_SECONDS));
            eprintln!(
                "the GitLab pool tests were still running after {FILE_SECONDS} seconds: a wait in \
                 crates/arca/tests/gitlab_runner_pool.rs has no bound. Ending the controllers and the tests"
            );
            let running = CONTROLLERS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            for pid in running {
                signal("KILL", -i64::from(pid));
            }
            std::process::exit(1);
        });
    });
}

// ---------------------------------------------------------------------------------------------------------------------
// The pool under test
// ---------------------------------------------------------------------------------------------------------------------

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/gitlab-runner-pool.sh")
}

/// Where this machine has `tool`, if it has it.
fn on_this_machine(tool: &str) -> Option<PathBuf> {
    let output = finish(
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}")),
        "sh, asked where a program is,",
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && path.starts_with('/')).then(|| PathBuf::from(path))
}

fn which(tool: &str) -> PathBuf {
    on_this_machine(tool).unwrap_or_else(|| {
        panic!("this test needs bash, curl and jq on this machine; {tool} is missing")
    })
}

/// A list the script declares as `NAME='word word ...'`: `POOL_PROGRAMS`, every program it starts, or `POOL_PACKAGES`,
/// the packages its first boot installs.
fn declared(list: &str) -> Vec<String> {
    let text = fs::read_to_string(script()).expect("the script");
    let opening = format!("\n{list}='");
    let start = text
        .find(&opening)
        .unwrap_or_else(|| panic!("the script declares {list}"))
        + opening.len();
    let end = start + text[start..].find('\'').expect("the closing quote");
    text[start..end]
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Fills `tools` with the programs the script declares, and bash for the script itself: this machine's own program
/// where it has one, a stand-in that does nothing where it has not. Programs in `without` and programs that `beside`
/// already holds a stand-in for are left out.
fn declared_programs(tools: &Path, beside: Option<&Path>, without: &[&str]) {
    fs::create_dir_all(tools).expect("the tools directory");
    for program in declared("POOL_PROGRAMS") {
        if without.contains(&program.as_str())
            || beside.is_some_and(|dir| dir.join(&program).exists())
        {
            continue;
        }
        match on_this_machine(&program) {
            Some(real) => std::os::unix::fs::symlink(real, tools.join(&program)).expect("a link"),
            None => executable(&tools.join(&program), "#!/bin/sh\n"),
        }
    }
    std::os::unix::fs::symlink(which("bash"), tools.join("bash")).expect("a link");
}

/// The environment that makes bash write the name of every program it does not find into `record`.
fn report_missing_programs(record: &Path) -> [(&'static str, String); 2] {
    [
        (
            "BASH_FUNC_command_not_found_handle%%",
            "() { printf '%s\\n' \"$1\" >>\"$TEST_NOT_FOUND\"; return 127\n}".to_string(),
        ),
        ("TEST_NOT_FOUND", record.display().to_string()),
    ]
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).expect("a script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("0755");
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// The settings of a pool for `https://gitlab.example.com`, with these registrations.
fn settings(runners: Value, concurrent: u64) -> Value {
    json!({
        "url": "https://gitlab.example.com",
        "concurrent": concurrent,
        "jobCpu": 2,
        "jobMemory": "2GiB",
        "serviceMemory": "512MiB",
        "jobServices": 1,
        "runners": runners,
        "memoryOvercommitPercent": 100,
        "reach": "direct",
        "gitlabNode": "",
        "serviceCpu": 1,
        "reserveMemory": "4GiB",
        "freeDisk": "40GiB",
        "privileged": false,
        "jobNetworks": "10.213.0.0/16",
        "runnerImage": RUNNER_IMAGE,
        "helperImage": HELPER_IMAGE,
        "tailnetImage": TAILNET_IMAGE
    })
}

fn one_runner() -> Value {
    json!([{ "name": "pool", "limit": 0, "jobCpu": 0, "jobMemory": "" }])
}

/// One `[[runners]]` section as the controller writes it.
fn section(
    name: &str,
    token: &str,
    limit: u64,
    cpu: u64,
    memory_mib: u64,
    privileged: bool,
    pin: Option<&str>,
) -> String {
    let mut text = format!(
        "\n[[runners]]\n  name = \"{name}\"\n  url = \"https://gitlab.example.com\"\n  token = \"{token}\"\n  executor = \"docker\"\n  limit = {limit}\n  [runners.feature_flags]\n    FF_NETWORK_PER_BUILD = true\n  [runners.docker]\n    helper_image = \"{HELPER_IMAGE}\"\n    privileged = {privileged}\n    services_privileged = {privileged}\n    disable_cache = true\n    volumes = []\n    pull_policy = [\"always\", \"if-not-present\"]\n    memory = \"{memory_mib}m\"\n    memory_swap = \"{memory_mib}m\"\n    cpus = \"{cpu}\"\n    services_limit = 1\n    service_memory = \"512m\"\n    service_memory_swap = \"512m\"\n    service_cpus = \"1\"\n"
    );
    if let Some(pin) = pin {
        text.push_str(&format!(
            "    extra_hosts = [\"gitlab.example.com:{pin}\"]\n"
        ));
    }
    text
}

fn head(concurrent: u64) -> String {
    format!(
        "concurrent = {concurrent}\ncheck_interval = 3\nshutdown_timeout = 30\nlog_format = \"json\"\nlog_level = \"info\"\n"
    )
}

struct Pool {
    dir: PathBuf,
    child: Option<Child>,
    /// More environment for the controller and the stand-ins, set before `start`.
    env: Vec<(&'static str, String)>,
}

impl Pool {
    /// A scratch machine: the settings as cloud-init would have written them, an empty secrets directory, and the
    /// stand-ins on the controller's `PATH`. Nothing runs yet.
    fn new(settings: &Value, memory_total: u64) -> Pool {
        bound_the_file();
        which("jq");
        let curl = which("curl");
        let dir = std::env::temp_dir().join(format!(
            "arca-gitlab-pool-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        for sub in ["bin", "secrets", "docker", "etc", "hosts"] {
            fs::create_dir_all(dir.join(sub)).expect("scratch");
        }
        fs::write(dir.join("etc/settings.json"), settings.to_string()).expect("settings");
        fs::write(
            dir.join("meminfo"),
            format!(
                "MemTotal:       {0} kB\nMemFree:        {0} kB\nMemAvailable:   {0} kB\n",
                memory_total / 1024
            ),
        )
        .expect("meminfo");
        let pool = Pool {
            dir,
            child: None,
            env: Vec::new(),
        };
        let at = |name: &str| pool.dir.join(name).display().to_string();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/gitlab_runner_pool/fake-docker.sh"),
            pool.dir.join("bin/docker"),
        )
        .expect("the fake docker");
        fs::set_permissions(
            pool.dir.join("bin/docker"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("0755");
        // The real curl, with every argument recorded: a credential on a command line would be seen here. When the
        // test names an exit status, it ends with it instead, as a curl that never made a connection does.
        executable(
            &pool.dir.join("bin/curl"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nif [ -e '{1}' ]; then printf '000 0.000000'; exit \"$(cat '{1}')\"; fi\nexec '{2}' \"$@\"\n",
                at("curl.log"),
                at("curl-exit"),
                curl.display()
            ),
        );
        // The free space of the disk Docker keeps its images on: what the test wrote, and another number once the
        // unused images have been removed.
        executable(
            &pool.dir.join("bin/df"),
            &format!(
                "#!/bin/sh\nfree=$(cat '{0}' 2>/dev/null || echo 107374182400)\nif [ -e '{1}' ] && [ -e '{2}' ]; then free=$(cat '{2}'); fi\nprintf '       Avail\\n%s\\n' \"$free\"\n",
                at("disk-free"),
                at("docker/pruned"),
                at("disk-free-after")
            ),
        );
        // The machine's own lookup of a name: an address when the test wrote one, and none otherwise.
        executable(
            &pool.dir.join("bin/getent"),
            &format!(
                "#!/bin/sh\n[ \"$1\" = hosts ] && [ -e \"{0}/$2\" ] || exit 2\nprintf '%s %s\\n' \"$(cat \"{0}/$2\")\" \"$2\"\n",
                at("hosts")
            ),
        );
        // Every other program the script declares, and none it does not.
        declared_programs(&pool.dir.join("tools"), Some(&pool.dir.join("bin")), &[]);
        pool
    }

    /// What Arca does at `instance_start`: one private file per secret.
    fn secret(&self, name: &str, value: &str) {
        let path = self.dir.join("secrets").join(name);
        fs::write(self.dir.join("secrets/.new"), value).expect("a secret");
        fs::set_permissions(
            self.dir.join("secrets/.new"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("0600");
        fs::rename(self.dir.join("secrets/.new"), path).expect("a secret");
    }

    fn start(&mut self, api: &str) {
        // The stand-ins and the declared programs only: what the script starts without declaring is not found.
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            self.dir.join("tools").display()
        );
        let out = fs::File::create(self.dir.join("out.log")).expect("the output");
        let child = Command::new("bash")
            .arg(script())
            .arg("run")
            .env("PATH", path)
            .env("POOL_NAME", "pool1")
            .env("POOL_SETTINGS", self.dir.join("etc/settings.json"))
            .env("POOL_SECRETS", self.dir.join("secrets"))
            .env("POOL_STATE", self.dir.join("state"))
            .env("POOL_LOGS", self.dir.join("logs"))
            .env("POOL_RUN", self.dir.join("run"))
            .env("POOL_CONFIG", self.dir.join("config"))
            .env("POOL_MEMINFO", self.dir.join("meminfo"))
            .env("POOL_DOCKER_ROOT", self.dir.join("docker"))
            .env("POOL_DOCKER_SOCKET", "/var/run/docker.sock")
            .env("POOL_API_URL", api)
            .env("POOL_CPUS", "16")
            .env("POOL_TICK", "0.1")
            .env("POOL_STATUS_EVERY", "1")
            .env("POOL_BACKOFF_MAX", "1")
            .env("POOL_STOP_WAIT", "10")
            .env("POOL_REFUSE_EVERY", "1")
            .env("POOL_RECHECK", "1")
            .env("POOL_READY_WAIT", "5")
            .envs(report_missing_programs(&self.dir.join("not-found.log")))
            .env("FAKE_PATH", std::env::var("PATH").unwrap_or_default())
            .env("FAKE_DOCKER_DIR", self.dir.join("docker"))
            .envs(self.env.iter().map(|(key, value)| (*key, value.as_str())))
            .stdin(Stdio::null())
            .stdout(out.try_clone().expect("the output"))
            .stderr(out)
            .process_group(0)
            .spawn()
            .expect("the controller starts");
        CONTROLLERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(child.id());
        self.child = Some(child);
    }

    /// Stops the controller as systemd does, with SIGTERM, and returns its exit code.
    fn stop(&mut self) -> Option<i32> {
        self.child.as_ref()?;
        let sent = self.term();
        self.stopped(sent)
    }

    /// Sends the controller SIGTERM, as systemd does, says whether it was sent, and returns once the stop has reached
    /// the pool. The process that takes the signal is not the one that does the pool's work: it lets go of the line
    /// the work reads its stop from, and that is one step after the signal. A test that goes on the moment the signal
    /// is sent would race that step, so the step itself is waited for, with a bound.
    fn term(&self) -> bool {
        let child = self.child.as_ref().expect("a controller that runs");
        let sent = signal("TERM", i64::from(child.id()));
        let deadline = Instant::now() + Duration::from_secs(GONE_SECONDS);
        while sent && holds_the_stop_line(child.id()) {
            assert!(
                Instant::now() < deadline,
                "the controller was sent SIGTERM and still held its stop line open {GONE_SECONDS} seconds later:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        sent
    }

    /// Waits for a controller that was sent SIGTERM, and returns its exit code.
    fn stopped(&mut self, sent: bool) -> Option<i32> {
        let mut child = self.child.take()?;
        let deadline = Instant::now() + Duration::from_secs(STOP_SECONDS);
        let code = loop {
            if let Some(status) = child.try_wait().expect("the controller's status") {
                break status.code();
            }
            if !sent || Instant::now() >= deadline {
                // It is not left behind, and the test does not wait for it any longer.
                end_controller(&mut child);
                panic!(
                    "the controller did not stop within {STOP_SECONDS} seconds of SIGTERM (signal sent: {sent}):\n{}",
                    self.output()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // What it left in its process group goes too: a pause it had started outlives it otherwise.
        signal("KILL", -i64::from(child.id()));
        CONTROLLERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|pid| *pid != child.id());
        let strays = self.read("not-found.log");
        assert!(
            strays.is_empty(),
            "the controller started programs its script does not declare in POOL_PROGRAMS, so the first boot neither \
             installs nor checks them:\n{strays}"
        );
        code
    }

    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.dir.join(file)).unwrap_or_default()
    }

    fn output(&self) -> String {
        self.read("out.log")
    }

    fn config(&self) -> String {
        self.read("config/config.toml")
    }

    /// The job records, without the header line: one list of fields per job.
    fn jobs(&self) -> Vec<Vec<String>> {
        self.read("logs/jobs.tsv")
            .lines()
            .filter(|line| !line.starts_with("finished\t"))
            .map(|line| line.split('\t').map(str::to_string).collect())
            .collect()
    }

    fn containers(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.dir.join("docker/containers"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A container GitLab Runner made for a job: a directory with the label it gives every container, and the job.
    fn job_container(&self, name: &str, job: u64) {
        let dir = self.dir.join("docker/containers").join(name);
        fs::create_dir_all(&dir).expect("a container");
        fs::write(
            dir.join("labels"),
            "com.gitlab.gitlab-runner.managed=true\n",
        )
        .expect("labels");
        fs::write(dir.join("job"), format!("{job}\n")).expect("the job");
    }

    /// A line GitLab Runner wrote.
    fn runner_says(&self, line: &str) {
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(self.dir.join("docker/containers").join(RUNNER).join("log"))
            .expect("the runner's output");
        writeln!(log, "{line}").expect("a line");
    }

    /// Everything the controller wrote or ran, outside the three places a credential belongs: its output, its
    /// files, and the arguments of every command it started.
    fn everything_written(&self) -> String {
        fn walk(dir: &Path, out: &mut String) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                // The secrets are Arca's files; the configuration directory is where GitLab Runner reads its tokens
                // (0600 in 0700, which a test checks); the run directory is the controller's private memory (0700
                // on a tmpfs in the machine), where the tailnet key's copy is. The tools are this machine's own.
                if ["secrets", "config", "run", "tools"]
                    .iter()
                    .any(|name| path.ends_with(name))
                {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, out);
                } else if let Ok(bytes) = fs::read(&path) {
                    out.push_str(&String::from_utf8_lossy(&bytes));
                    out.push('\n');
                }
            }
        }
        let mut out = String::new();
        walk(&self.dir, &mut out);
        out
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // A test that failed comes here with its controller still running. Nothing here may wait without a bound.
        if let Some(mut child) = self.child.take() {
            end_controller(&mut child);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn wait_for(pool: &Pool, what: &str, mut ready: impl FnMut() -> bool) {
    // The bound only keeps a test from waiting for ever: what is waited for is the same, however long the machine
    // takes. Thirty seconds was once too short on a loaded pool.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; the controller wrote:\n{}",
            pool.output()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Waits until these are the machine's containers and GitLab Runner's is complete: the fake docker makes a
/// container's directory first and writes what it was started with afterwards, the count of its starts last.
fn wait_for_runner(pool: &Pool, containers: &[&str]) {
    wait_for(pool, "GitLab Runner's container", || {
        pool.containers() == containers && !pool.read(&format!("docker/runs.{RUNNER}")).is_empty()
    });
}

fn wait_for_words(pool: &Pool, words: &str) {
    wait_for(pool, words, || pool.output().contains(words));
}

/// The last status block's totals line.
fn totals(pool: &Pool) -> String {
    pool.read("logs/status.txt")
        .lines()
        .rev()
        .find(|line| line.contains(" totals "))
        .unwrap_or("")
        .to_string()
}

/// Asserts that nothing the controller wrote or ran, outside the places a credential belongs, holds one of `secrets`.
fn no_credential(pool: &Pool, secrets: &[String]) {
    let written = pool.everything_written();
    assert!(
        written.contains("runners/verify"),
        "the record of curl's arguments is read"
    );
    for secret in secrets {
        assert!(secret.len() >= 12, "a credential to look for");
        assert!(
            !written.contains(secret.as_str()),
            "a credential was written or put on a command line: it begins {}",
            &secret[..8]
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn several_registrations_run_under_one_cap_and_no_token_reaches_an_output_or_a_command_line() {
    let tokens = [token("build"), token("test"), token("docs")];
    let gitlab = Gitlab::start(&tokens);
    let mut pool = Pool::new(
        &settings(
            json!([
                { "name": "build", "limit": 2, "jobCpu": 4, "jobMemory": "4GiB" },
                { "name": "test", "limit": 0, "jobCpu": 0, "jobMemory": "" },
                { "name": "docs", "limit": 1 }
            ]),
            6,
        ),
        REPORTED_32GIB,
    );
    for (place, token) in tokens.iter().enumerate() {
        pool.secret(&format!("GITLAB_RUNNER_TOKEN_{}", place + 1), token);
    }
    pool.start(&gitlab.url);
    wait_for_runner(&pool, &[RUNNER]);
    // The worst case fills the six places with the largest jobs first: two of build, then four of the next.
    assert!(pool.output().contains(
        "arca-gitlab-pool: pool pool1 for https://gitlab.example.com: 3 registrations, at most 6 jobs at once, 19456 MiB of the 27921 MiB the machine has for jobs (2 of build at 4608 MiB, 4 of test at 2560 MiB)\n"
    ), "{}", pool.output());
    assert!(pool.output().contains(
        "arca-gitlab-pool: GitLab takes the tokens of 3 of 3 registrations; starting GitLab Runner\n"
    ));

    // The configuration, to the letter: every registration with its own token, limit and size, the one cap, and for
    // every job no privileged container, no volume, a network of its own and the helper by digest.
    assert_eq!(
        pool.config(),
        head(6)
            + &section("build", &tokens[0], 2, 4, 4096, false, None)
            + &section("test", &tokens[1], 0, 2, 2048, false, None)
            + &section("docs", &tokens[2], 1, 2, 2048, false, None)
    );
    // No job gets the daemon's socket or a path of the machine: the configuration names neither.
    assert!(!pool.config().contains("docker.sock") && !pool.config().contains("/var/run"));
    assert_eq!(mode(&pool.dir.join("config/config.toml")), 0o600);
    assert_eq!(mode(&pool.dir.join("config")), 0o700);
    // GitLab Runner found them so when it started, and it is the one container with the daemon's socket.
    let runner = format!("docker/containers/{RUNNER}");
    assert_eq!(pool.read(&format!("{runner}/modes")), "700\n600\n");
    assert_eq!(
        pool.read(&format!("{runner}/args")),
        format!(
            "-d --name {RUNNER} --label arca.pool=pool1 --restart no -v {}:/etc/gitlab-runner -v /var/run/docker.sock:/var/run/docker.sock {RUNNER_IMAGE}\n",
            pool.dir.join("config").display()
        )
    );
    // GitLab was asked about each token once, with the one system id GitLab Runner is given too.
    let seen = gitlab.seen();
    assert_eq!(
        seen.iter()
            .map(|(token, _)| token.clone())
            .collect::<Vec<_>>(),
        tokens
    );
    let system_id = pool.read("config/.runner_system_id");
    assert!(
        system_id.starts_with("r_")
            && system_id.trim_end().len() == 14
            && system_id[2..]
                .trim_end()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric()),
        "{system_id}"
    );
    assert!(seen.iter().all(|(_, id)| format!("{id}\n") == system_id));
    assert_eq!(pool.read(&format!("{runner}/system-id")), system_id);
    assert_eq!(pool.read("state/system-id"), system_id);

    // Two jobs run: one has its own container and a service, the other only GitLab Runner's helper so far.
    pool.job_container("runner-abc-project-45-concurrent-0-build", 101);
    pool.job_container("runner-abc-project-45-concurrent-0-postgres-0", 101);
    pool.job_container("runner-def-project-46-concurrent-1-predefined", 102);
    wait_for(&pool, "two running jobs in the status", || {
        totals(&pool).contains(" jobs_now=2 of 6 ")
    });
    // They finish, as GitLab Runner's own output says it; between them are lines that are not a job's end.
    pool.runner_says("Runtime platform                                    arch=amd64 os=linux");
    pool.runner_says(r#"{"job":101,"job-status":"running","level":"info","msg":"Appending trace to coordinator...ok","runner":"t3_synthe","runner_name":"build","time":"2026-11-11T20:00:30Z"}"#);
    pool.runner_says(r#"{"duration_s":61.42,"job":101,"job-status":"success","job_name":"build","level":"info","msg":"Job succeeded","pipeline_id":7,"project":45,"project_full_path":"example-group/example-project","runner":"t3_synthe","runner_name":"build","time":"2026-11-11T20:01:01Z"}"#);
    pool.runner_says(r#"{"duration_s":3.9,"error":"exit code 1","exit_code":1,"failure_reason":"script_failure","job":102,"job-status":"failed","job_name":"unit tests","level":"warning","msg":"Job failed: exit code 1","project":46,"project_full_path":"example-group/another-project","runner":"t3_synthe","runner_name":"test","time":"2026-11-11T20:01:05Z"}"#);
    pool.runner_says(r#"{"duration_s":2,"job":103,"job-status":"success","job_name":"odd\tname","level":"info","msg":"Job succeeded","project":47,"runner_name":"someone-else","time":"2026-11-11T20:01:06Z"}"#);
    fs::remove_dir_all(
        pool.dir
            .join("docker/containers/runner-abc-project-45-concurrent-0-build"),
    )
    .expect("the job's container goes");
    fs::remove_dir_all(
        pool.dir
            .join("docker/containers/runner-abc-project-45-concurrent-0-postgres-0"),
    )
    .expect("the job's service goes");
    fs::remove_dir_all(
        pool.dir
            .join("docker/containers/runner-def-project-46-concurrent-1-predefined"),
    )
    .expect("the job's helper goes");
    wait_for(&pool, "the finished jobs in the status", || {
        totals(&pool)
            .contains(" jobs_now=0 of 6 jobs_done=3 jobs_failed=1 runners=3 of 3 failures=0 ")
    });
    // The fake docker returns the whole output at every look, as overlapping looks would: each job is recorded once.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        pool.jobs(),
        [
            [
                "2026-11-11T20:01:01Z",
                "build",
                "45",
                "101",
                "build",
                "success",
                "-",
                "61"
            ],
            [
                "2026-11-11T20:01:05Z",
                "test",
                "46",
                "102",
                "unit tests",
                "failed",
                "script_failure",
                "3"
            ],
            [
                "2026-11-11T20:01:06Z",
                "unknown",
                "47",
                "103",
                "odd name",
                "success",
                "-",
                "2"
            ],
        ]
    );
    let output = pool.output();
    for line in [
        "arca-gitlab-pool: job runner=build project=45 job=101 name=\"build\" result=success reason=- seconds=61\n",
        "arca-gitlab-pool: job runner=test project=46 job=102 name=\"unit tests\" result=failed reason=script_failure seconds=3\n",
        "arca-gitlab-pool: runner 1 build limit=2 job=4cpu/4096MiB jobs_done=1 jobs_failed=0\n",
        "arca-gitlab-pool: runner 2 test limit=none job=2cpu/2048MiB jobs_done=1 jobs_failed=1\n",
        "arca-gitlab-pool: runner 3 docs limit=1 job=2cpu/2048MiB jobs_done=0 jobs_failed=0\n",
        "arca-gitlab-pool: recent job finished=2026-11-11T20:01:05Z runner=test project=46 job=102 name=\"unit tests\" result=failed reason=script_failure seconds=3\n",
        "arca-gitlab-pool: pool pool1 for https://gitlab.example.com, reach direct, at most 6 jobs at once, 19456 MiB of the 27921 MiB the machine has for jobs (2 of build at 4608 MiB, 4 of test at 2560 MiB)\n",
    ] {
        assert!(output.contains(line), "{line} is missing from:\n{output}");
    }
    assert_eq!(
        output
            .matches("arca-gitlab-pool: job runner=build project=45 job=101 ")
            .count(),
        1
    );
    // What GitLab Runner says beyond the job is not copied: a project's path, or the token's first characters.
    assert!(!output.contains("example-group") && !output.contains("t3_synthe"));
    // A status when Docker does not answer says so, and names no number.
    fs::write(pool.dir.join("docker/ps-fails"), "").expect("a flag");
    wait_for(&pool, "a status without Docker's answer", || {
        totals(&pool).contains(" jobs_now=unknown of 6 ")
    });
    fs::remove_file(pool.dir.join("docker/ps-fails")).expect("the flag goes");

    assert_eq!(pool.stop(), Some(0));
    let output = pool.output();
    assert!(output.ends_with(
        "arca-gitlab-pool: stopping: GitLab Runner takes no new job, and the jobs that run now get 10 seconds to finish before they are lost\narca-gitlab-pool: stopped\n"
    ), "{output}");
    // GitLab Runner is told to finish its jobs (SIGQUIT), given the seconds, and then removed.
    assert!(
        pool.read("docker/calls.log")
            .contains(&format!("stop --signal QUIT -t 10 {RUNNER}\nlogs "))
    );
    assert!(pool.containers().is_empty());
    assert_eq!(pool.read(&format!("docker/runs.{RUNNER}")), "x");
    no_credential(&pool, &tokens);
    // The one call to GitLab: the token is in none of its arguments.
    assert!(pool.read("curl.log").lines().all(|line| line
        == format!(
            "-sS -o /dev/null -w %{{http_code}} %{{time_connect}} --connect-timeout 10 --max-time 30 -X POST -H Content-Type: application/json -H User-Agent: arca-gitlab-pool --data-binary @- {}/runners/verify",
            gitlab.url
        )));
    assert!(gitlab.state.lock().expect("the state").other.is_empty());
}

#[test]
fn a_registration_without_its_secret_is_named_and_nothing_starts_before_it_is_set() {
    let tokens = [token("one"), token("two"), token("three")];
    let gitlab = Gitlab::start(&tokens);
    let mut pool = Pool::new(
        &settings(
            json!([{ "name": "build" }, { "name": "test" }, { "name": "docs" }]),
            3,
        ),
        REPORTED_32GIB,
    );
    pool.secret("GITLAB_RUNNER_TOKEN_1", &tokens[0]);
    // A secret for a place runners does not have.
    pool.secret("GITLAB_RUNNER_TOKEN_5", &token("five"));
    pool.start(&gitlab.url);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: waiting for secrets: runners 2 (test) needs GITLAB_RUNNER_TOKEN_2; runners 3 (docs) needs GITLAB_RUNNER_TOKEN_3. Set each with service_secret_set, then run instance_start\n",
    );
    pool.secret("GITLAB_RUNNER_TOKEN_3", &tokens[2]);
    std::thread::sleep(Duration::from_millis(400));
    // Nothing ran: no image was pulled, GitLab was not asked, and no configuration holds the one token there is.
    assert!(pool.read("docker/calls.log").is_empty() && pool.read("curl.log").is_empty());
    assert!(gitlab.seen().is_empty() && pool.config().is_empty());
    pool.secret("GITLAB_RUNNER_TOKEN_2", &tokens[1]);
    wait_for_runner(&pool, &[RUNNER]);
    assert!(pool.output().contains(
        "arca-gitlab-pool: set and not used, since runners has 3 entries: GITLAB_RUNNER_TOKEN_5\n"
    ));
    assert_eq!(
        pool.config(),
        head(3)
            + &section("build", &tokens[0], 0, 2, 2048, false, None)
            + &section("test", &tokens[1], 0, 2, 2048, false, None)
            + &section("docs", &tokens[2], 0, 2, 2048, false, None)
    );
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[tokens.to_vec(), vec![token("five")]].concat());
}

#[test]
fn a_gitlab_that_cannot_be_reached_is_said_in_one_sentence_for_each_thing_in_between() {
    // The failures no test can make for real, by the exit status curl ends with for each. GitLab has a name here,
    // which the machine's own lookup knows or does not know.
    let good = token("reach");
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    let dir = pool.dir.clone();
    let exit =
        move |status: &str| fs::write(dir.join("curl-exit"), status).expect("curl's exit status");
    exit("6");
    pool.start("https://gitlab.example.com:8443/api/v4");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: the name gitlab.example.com does not resolve from this machine, so its DNS does not know it. A GitLab on a private network or a tailnet has a name only that network's DNS answers: the machine's network must lead there, or the pool is created with reach \"tailnet\". Trying again in ",
    );
    exit("28");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: the name gitlab.example.com does not resolve from this machine (the lookup timed out), so its DNS does not know it or does not answer. A GitLab on a private network or a tailnet has a name only that network's DNS answers: the machine's network must lead there, or the pool is created with reach \"tailnet\". Trying again in ",
    );
    fs::write(pool.dir.join("hosts/gitlab.example.com"), "192.0.2.10").expect("the name's address");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: gitlab.example.com resolves to 192.0.2.10 and the connection to port 8443 timed out after 10 seconds. The name is known and nothing answers: a firewall or an access rule between this machine and GitLab drops it (on a tailnet, the rule that lets this machine, or the node it goes out through, reach GitLab). Trying again in ",
    );
    exit("7");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: gitlab.example.com resolves to 192.0.2.10 and refuses the connection on port 8443. Nothing listens there, or a firewall answers for it. Trying again in ",
    );
    exit("60");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: gitlab.example.com answers on port 8443 and its certificate is refused (curl exit 60). It is not signed by an authority this machine trusts, or it is for another name. This pool trusts the public authorities only: a GitLab with a certificate of a private authority is not served by it. Trying again in ",
    );
    exit("35");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: gitlab.example.com answers on port 8443 and no TLS connection could be made (curl exit 35). What listens there is not HTTPS, or offers no TLS this machine accepts. Trying again in ",
    );
    // GitLab Runner is not started while GitLab cannot be asked, and no configuration is written.
    assert!(pool.containers().is_empty() && pool.config().is_empty());
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, std::slice::from_ref(&good));
    drop(pool);

    // The failures the real curl makes against this test's own GitLab, and the token GitLab does not take.
    let gitlab = Gitlab::start(std::slice::from_ref(&good));
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.local_addr().expect("the address").port()
    };
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    pool.env.push(("POOL_HTTP_WAIT", "1".into()));
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    pool.start(&format!("http://127.0.0.1:{closed}/api/v4"));
    wait_for_words(
        &pool,
        &format!(
            "arca-gitlab-pool: GitLab cannot be reached: 127.0.0.1 resolves to 127.0.0.1 and refuses the connection on port {closed}. Nothing listens there, or a firewall answers for it. Trying again in "
        ),
    );
    assert_eq!(pool.stop(), Some(0));
    drop(pool);

    let wrong = token("wrong");
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    pool.env.push(("POOL_HTTP_WAIT", "1".into()));
    pool.secret("GITLAB_RUNNER_TOKEN_1", &wrong);
    gitlab.change(|state| state.silent = true);
    pool.start(&gitlab.url);
    wait_for_words(
        &pool,
        &format!(
            "arca-gitlab-pool: GitLab cannot be reached: 127.0.0.1 accepts the connection on port {} and did not answer within 1 seconds. It is reached and does not answer: GitLab itself, or a proxy in front of it, is stuck or overloaded. Trying again in ",
            gitlab.port
        ),
    );
    gitlab.change(|state| {
        state.silent = false;
        state.status = Some(502);
    });
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab answered 502 to the token check at https://gitlab.example.com: it is reached and does not answer as a GitLab that is well. Trying again in ",
    );
    gitlab.change(|state| state.status = Some(404));
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab answered 404 to the token check at https://gitlab.example.com: url is not the address of a GitLab. It is the address of the GitLab itself, not of a group or a project. Trying again in ",
    );
    gitlab.change(|state| state.status = None);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: runners 1 (pool) is left out: GitLab answers and does not take GITLAB_RUNNER_TOKEN_1 (403): the token is not the one GitLab holds for a runner, it was reset, or its runner was deleted. Create the runner again in GitLab, or reset its token there, and set the new one (service_secret_set, then instance_start)\n",
    );
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab takes none of the pool's tokens, so no job can come. Asking again in ",
    );
    assert!(pool.containers().is_empty() && pool.config().is_empty());
    // A token of the older kind is not sent anywhere: it is refused by what it begins with.
    let old = "GR1348941synthetic0123456789".to_string();
    let asked = gitlab.seen().len();
    pool.secret("GITLAB_RUNNER_TOKEN_1", &old);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: runners 1 (pool) is left out: GITLAB_RUNNER_TOKEN_1 is not a runner authentication token. Such a token begins glrt-, and GitLab shows it once, when the runner is created (CI/CD, Runners, New runner); a registration token of the older kind is not taken. Set the right one (service_secret_set, then instance_start)\n",
    );
    assert!(
        gitlab.seen()[asked..]
            .iter()
            .all(|(token, _)| *token != old)
    );
    // The right token ends it, with no restart of anything.
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    wait_for_runner(&pool, &[RUNNER]);
    assert!(
        pool.output()
            .contains("arca-gitlab-pool: runners 1 (pool): GitLab takes its token now\n")
    );
    assert_eq!(
        pool.config(),
        head(4) + &section("pool", &good, 0, 2, 2048, false, None)
    );
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[good, wrong, old]);
}

#[test]
fn a_refused_token_leaves_its_registration_out_and_a_new_secret_brings_it_in_without_a_restart() {
    let tokens = [token("first"), token("second")];
    let gitlab = Gitlab::start(&tokens);
    let mut pool = Pool::new(
        &settings(json!([{ "name": "build" }, { "name": "test" }]), 2),
        REPORTED_32GIB,
    );
    pool.env.push(("POOL_VERIFY_EVERY", "1".into()));
    pool.secret("GITLAB_RUNNER_TOKEN_1", &tokens[0]);
    pool.secret("GITLAB_RUNNER_TOKEN_2", &token("stale"));
    pool.start(&gitlab.url);
    wait_for_runner(&pool, &[RUNNER]);
    // The registration GitLab takes runs; the other is left out of the configuration and named in every status.
    assert_eq!(
        pool.config(),
        head(2) + &section("build", &tokens[0], 0, 2, 2048, false, None)
    );
    assert!(pool.output().contains(
        "arca-gitlab-pool: GitLab takes the tokens of 1 of 2 registrations; starting GitLab Runner\n"
    ));
    wait_for(&pool, "the refusal in the status", || {
        pool.read("logs/status.txt").contains(
            "arca-gitlab-pool: runner 2 test LEFT OUT: GitLab answers and does not take GITLAB_RUNNER_TOKEN_2 (403)",
        ) && totals(&pool).contains(" runners=1 of 2 failures=1 ")
    });
    // GitLab is asked again and again, and the same refusal is said once.
    wait_for(&pool, "GitLab asked again", || gitlab.seen().len() >= 6);
    assert_eq!(
        pool.output()
            .matches("runners 2 (test) is left out")
            .count(),
        1
    );

    // The owner sets the right token: it comes in, and GitLab Runner, which reads its configuration again by itself,
    // is not started a second time.
    pool.secret("GITLAB_RUNNER_TOKEN_2", &tokens[1]);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: runners 2 (test): GitLab takes its token now\narca-gitlab-pool: GitLab Runner's configuration was written again: 2 of 2 registrations\n",
    );
    assert_eq!(
        pool.config(),
        head(2)
            + &section("build", &tokens[0], 0, 2, 2048, false, None)
            + &section("test", &tokens[1], 0, 2, 2048, false, None)
    );
    assert_eq!(mode(&pool.dir.join("config/config.toml")), 0o600);
    assert_eq!(pool.read(&format!("docker/runs.{RUNNER}")), "x");

    // GitLab stops answering: the registrations stay as they were, it is said once, and said when it is over.
    gitlab.change(|state| state.status = Some(503));
    wait_for_words(
        &pool,
        "arca-gitlab-pool: WARNING: GitLab answered 503 to the token check at https://gitlab.example.com: it is reached and does not answer as a GitLab that is well. The registrations stay as they were, and GitLab is asked again\n",
    );
    wait_for(&pool, "the warning in the status", || {
        pool.read("logs/status.txt")
            .contains("WARNING: GitLab answered 503")
    });
    assert!(pool.config().contains("name = \"test\""));
    gitlab.change(|state| state.status = None);
    wait_for_words(&pool, "arca-gitlab-pool: GitLab answers again\n");

    // GitLab Runner stops by itself: what its jobs left is removed, and it is started again.
    pool.job_container("runner-abc-project-45-concurrent-0-build", 101);
    fs::write(
        pool.dir.join(format!("docker/containers/{RUNNER}/exit")),
        "1",
    )
    .expect("the exit");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab Runner stopped (exit 1) and is started again; the jobs it ran are lost. Should it stop again, the next start is in 1 seconds\n",
    );
    wait_for(&pool, "the second GitLab Runner", || {
        pool.read(&format!("docker/runs.{RUNNER}")) == "xx" && pool.containers() == [RUNNER]
    });
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[tokens.to_vec(), vec![token("stale")]].concat());
}

/// Starts a pool with these settings, waits for the words and stops it. Nothing may have been started.
fn refused(settings: &Value, memory: u64, words: &str) {
    let mut pool = Pool::new(settings, memory);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &token("refused"));
    pool.start("http://127.0.0.1:9/api/v4");
    wait_for_words(&pool, words);
    assert!(
        pool.read("docker/calls.log").is_empty() && pool.read("curl.log").is_empty(),
        "nothing is pulled, started or asked for settings that are refused"
    );
    assert!(pool.config().is_empty());
    // A controller that refuses stays up, so the reason stays on the console; stopped, it says it did not run.
    assert_eq!(pool.stop(), Some(1));
}

#[test]
fn settings_that_cannot_be_used_are_refused_in_words_and_nothing_is_started() {
    let with = |change: &dyn Fn(&mut Value)| {
        let mut settings = settings(one_runner(), 4);
        change(&mut settings);
        settings
    };
    let again = "\narca-gitlab-pool: the pool does not start until its settings are right: they were fixed when the machine was created, so create the pool again with the right ones\n";
    let host = "arca-gitlab-pool: settings: url: its host must be a plain host name (letters, digits, dots and dashes), with no user, password, query or fragment: the pool's tokens are sent to it";
    let entry = |n: u64| {
        format!(
            "arca-gitlab-pool: settings: runners {n} needs a name (a-z, 0-9 and dashes, beginning with a letter, at most 32) and may give limit (jobs at once for this registration; 0 for no limit of its own), jobCpu (0 for the pool's) and jobMemory (a size such as 2GiB; \"\" for the pool's)"
        )
    };
    let numbers = "arca-gitlab-pool: settings: concurrent (jobs at once, 1 or more), jobCpu and serviceCpu (CPUs, 1 or more) and jobServices (services a job may start, 0 or more) are whole numbers";
    let sizes = "arca-gitlab-pool: settings: jobMemory, serviceMemory, reserveMemory and freeDisk are sizes such as 2GiB";
    let overcommit = "arca-gitlab-pool: settings: memoryOvercommitPercent is a whole number from 100 (the jobs' memory caps add up to no more than the machine has for jobs) to 200 (to twice that)";
    let node_reach = "arca-gitlab-pool: settings: gitlabNode goes with reach \"tailnet\": it names GitLab's node on the tailnet the machine joins. With reach \"direct\" leave it empty";
    let node_name = "arca-gitlab-pool: settings: gitlabNode is the name of GitLab's node on your tailnet (its machine name there, or its full tailnet name): letters, digits, dots and dashes";
    let path = "arca-gitlab-pool: settings: url is the address of the GitLab itself, such as https://gitlab.example.com or https://example.com/gitlab: its path is letters, digits, dots, dashes and slashes";
    let networks = "arca-gitlab-pool: settings: jobNetworks is the block of private addresses the jobs' own networks are cut from, such as 10.213.0.0/16: an IPv4 block of /8 to /20";
    let pinned = |key: &str| {
        format!(
            "arca-gitlab-pool: settings: {key} must name its image by digest, <name>@sha256:<64 hex digits>: the pool runs no image it cannot name exactly"
        )
    };
    let many: Vec<Value> = (1..=13)
        .map(|n| json!({ "name": format!("r{n}") }))
        .collect();
    let cases: Vec<(Value, String)> = vec![
        (
            // The template ships url empty: a pool created without one says what it is for.
            with(&|s| s["url"] = json!("")),
            "arca-gitlab-pool: settings: url is not set. It is the address of your GitLab, for example https://gitlab.example.com: create the pool with it in settings".into(),
        ),
        (
            with(&|s| s["url"] = json!("http://gitlab.example.com")),
            "arca-gitlab-pool: settings: url must begin https://, for example https://gitlab.example.com".into(),
        ),
        (with(&|s| s["url"] = json!("https://user@gitlab.example.com")), host.into()),
        (with(&|s| s["url"] = json!("https://gitlab.example.com:port")), host.into()),
        (
            with(&|s| s["url"] = json!("https://gitlab.example.com/group?x=1")),
            "arca-gitlab-pool: settings: url is the address of the GitLab itself, such as https://gitlab.example.com or https://example.com/gitlab: its path is letters, digits, dots, dashes and slashes".into(),
        ),
        (with(&|s| s["concurrent"] = json!(0)), numbers.into()),
        (with(&|s| s["concurrent"] = json!("many")), numbers.into()),
        (with(&|s| s["jobCpu"] = json!(0)), numbers.into()),
        (with(&|s| s["jobServices"] = json!(-1)), numbers.into()),
        (with(&|s| s["jobMemory"] = json!("lots")), sizes.into()),
        (with(&|s| s["serviceMemory"] = json!("0MiB")), sizes.into()),
        (with(&|s| s["reserveMemory"] = json!(4)), sizes.into()),
        (with(&|s| s["memoryOvercommitPercent"] = json!(99)), overcommit.into()),
        (with(&|s| s["memoryOvercommitPercent"] = json!(201)), overcommit.into()),
        (with(&|s| s["memoryOvercommitPercent"] = json!(1.5)), overcommit.into()),
        (
            with(&|s| s["reach"] = json!("vpn")),
            "arca-gitlab-pool: settings: reach is how the machine reaches GitLab: \"direct\" (over the machine's own network; also when it is left out) or \"tailnet\" (the machine joins your tailnet with TAILSCALE_AUTHKEY)".into(),
        ),
        (
            // Privileged containers are asked for with the JSON value and nothing that looks like it.
            with(&|s| s["privileged"] = json!("true")),
            "arca-gitlab-pool: settings: privileged is true or false. It is false unless a template sets it: a privileged job container can take over the machine, and with it every token of the pool".into(),
        ),
        (with(&|s| s["runnerImage"] = json!("registry.example/gitlab-runner:v19.5.3")), pinned("runnerImage")),
        (with(&|s| s["helperImage"] = json!("registry.example/helper@sha256:abc")), pinned("helperImage")),
        (with(&|s| s["tailnetImage"] = json!("")), pinned("tailnetImage")),
        (
            with(&|s| s["jobNetworks"] = json!("10.213.0.0/24")),
            "arca-gitlab-pool: settings: jobNetworks is the block of private addresses the jobs' own networks are cut from, such as 10.213.0.0/16: an IPv4 block of /8 to /20".into(),
        ),
        (with(&|s| s["gitlabNode"] = json!("gitlab-vm")), node_reach.into()),
        (
            with(&|s| {
                s["reach"] = json!("tailnet");
                s["gitlabNode"] = json!("gitlab vm");
            }),
            node_name.into(),
        ),
        (
            with(&|s| {
                s["reach"] = json!("tailnet");
                s["gitlabNode"] = json!("gitlab\"\nvm");
            }),
            node_name.into(),
        ),
        (with(&|s| s["gitlabNode"] = json!(7)), node_name.into()),
        // A quote, a backslash and a line break, in every setting that is written into GitLab Runner's
        // configuration or onto a command line: each is refused by what the setting may hold, before anything runs.
        (with(&|s| s["url"] = json!("https://gitlab.exa\"mple.com")), host.into()),
        (with(&|s| s["url"] = json!("https://gitlab.example.com\\evil")), host.into()),
        (with(&|s| s["url"] = json!("https://gitlab.example.com\n[[runners]]")), host.into()),
        (with(&|s| s["url"] = json!("https://gitlab.example.com/a\"b")), path.into()),
        (with(&|s| s["url"] = json!("https://gitlab.example.com/a\\b")), path.into()),
        (
            with(&|s| s["url"] = json!("https://gitlab.example.com/a\"\n[[runners]]\n  token = \"x")),
            path.into(),
        ),
        (with(&|s| s["runners"] = json!([{ "name": "bu\"ild" }])), entry(1)),
        (with(&|s| s["runners"] = json!([{ "name": "bu\\ild" }])), entry(1)),
        (
            with(&|s| s["runners"] = json!([{ "name": "build\"\n  token = \"x" }])),
            entry(1),
        ),
        (
            with(&|s| s["runners"] = json!([{ "name": "build", "jobMemory": "2GiB\"\n" }])),
            entry(1),
        ),
        (
            with(&|s| {
                s["helperImage"] = json!(format!(
                    "registry.example/helper\"\n@sha256:{}",
                    "2".repeat(64)
                ))
            }),
            pinned("helperImage"),
        ),
        (with(&|s| s["jobMemory"] = json!("2GiB\"")), sizes.into()),
        (with(&|s| s["jobNetworks"] = json!("10.213.0.0/16\"")), networks.into()),
        (
            with(&|s| s["runners"] = json!([])),
            "arca-gitlab-pool: settings: runners needs at least one entry: one for each runner you created in GitLab, each with a name".into(),
        ),
        (with(&|s| s["runners"] = json!([{ "name": "build" }, { "limit": 2 }])), entry(2)),
        (with(&|s| s["runners"] = json!([{ "name": "Build" }])), entry(1)),
        (with(&|s| s["runners"] = json!([{ "name": "9lives" }])), entry(1)),
        (with(&|s| s["runners"] = json!([{ "name": "build", "jobMemory": "big" }])), entry(1)),
        (with(&|s| s["runners"] = json!([{ "name": "build", "limit": "two" }])), entry(1)),
        (with(&|s| s["runners"] = json!(["build"])), entry(1)),
        (
            with(&|s| s["runners"] = json!([{ "name": "build" }, { "name": "test" }, { "name": "build" }])),
            "arca-gitlab-pool: settings: runners 3 has the name of an earlier entry (build): each registration has a name of its own".into(),
        ),
        (
            with(&|s| s["runners"] = json!(many)),
            "arca-gitlab-pool: settings: runners has 13 entries and one pool takes at most 12: each has a secret of its own, GITLAB_RUNNER_TOKEN_1 to GITLAB_RUNNER_TOKEN_12. Create a second pool for the others".into(),
        ),
    ];
    for (settings, words) in &cases {
        refused(settings, REPORTED_32GIB, &format!("{words}{again}"));
    }
    // Settings that are no object at all.
    refused(
        &json!(["url"]),
        REPORTED_32GIB,
        &format!("settings.json is missing or is not a JSON object{again}"),
    );
}

#[test]
fn jobs_that_do_not_fit_the_machine_are_refused_with_the_numbers_the_machine_reports() {
    // The first user's old runner: eight jobs of 4 CPUs and 8GiB. A machine of 32GiB does not hold them, and the
    // refusal says every number: what the jobs could take, what the machine reports, the reserve, and the difference.
    let old_runner = |concurrent: u64| {
        let mut settings = settings(one_runner(), concurrent);
        settings["jobCpu"] = json!(4);
        settings["jobMemory"] = json!("8GiB");
        settings
    };
    refused(
        &old_runner(8),
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: its jobs could take 69632 MiB at once (8 of pool at 8704 MiB) and the machine has 27921 MiB for jobs (the 32017 MiB it reports, less reserveMemory 4096 MiB): 41711 MiB too much. A job takes its jobMemory and, for each of the 1 services it may start, serviceMemory; a machine reports less memory than the size it was given. Lower concurrent, a registration's limit, jobMemory or serviceMemory and create the pool again; a larger machine is gitlab-runner-pool-large, or a copy of the template with larger limits\n",
    );
    // Sizes that add up to the machine's size do not fit either: it reports less than it was given.
    let mut exact = settings(one_runner(), 7);
    exact["jobMemory"] = json!("3584MiB");
    refused(
        &exact,
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: its jobs could take 28672 MiB at once (7 of pool at 4096 MiB) and the machine has 27921 MiB for jobs (the 32017 MiB it reports, less reserveMemory 4096 MiB): 751 MiB too much.",
    );
    // A registration's limit counts: one large job and the places that are left for the small ones.
    let mixed = |concurrent: u64| {
        settings(
            json!([
                { "name": "small" },
                { "name": "large", "limit": 1, "jobCpu": 8, "jobMemory": "16GiB" }
            ]),
            concurrent,
        )
    };
    refused(
        &mixed(6),
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: its jobs could take 29696 MiB at once (1 of large at 16896 MiB, 5 of small at 2560 MiB) and the machine has 27921 MiB for jobs (the 32017 MiB it reports, less reserveMemory 4096 MiB): 1775 MiB too much.",
    );
    // A size too large to count with: nine jobs of six digits of the largest unit would wrap the sum around to one
    // that seems to fit. No size is multiplied before it is known to be at most the machine's whole memory.
    let mut absurd = settings(one_runner(), 9);
    absurd["jobMemory"] = json!("999999TiB");
    absurd["jobServices"] = json!(0);
    refused(
        &absurd,
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: runners 1 (pool) gives each job 1048574951424 MiB of memory (jobMemory) and the whole machine reports 32017 MiB. Give a job at most what the machine has for jobs, 27921 MiB, and create the pool again\n",
    );
    let mut absurd = settings(
        json!([{ "name": "small" }, { "name": "huge", "jobMemory": "999999TiB" }]),
        9,
    );
    absurd["memoryOvercommitPercent"] = json!(200);
    refused(
        &absurd,
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: runners 2 (huge) gives each job 1048574951424 MiB of memory (jobMemory) and the whole machine reports 32017 MiB.",
    );
    let mut absurd = settings(one_runner(), 9);
    absurd["serviceMemory"] = json!("999999TiB");
    refused(
        &absurd,
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: serviceMemory gives each service 1048574951424 MiB and the whole machine reports 32017 MiB\n",
    );
    let mut absurd = settings(one_runner(), 9);
    absurd["reserveMemory"] = json!("64GiB");
    refused(
        &absurd,
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: reserveMemory keeps 65536 MiB back and the whole machine reports 32017 MiB, so nothing is left for jobs\n",
    );
    // More CPUs for a job than the machine has: Docker would refuse the container of every job.
    let mut wide = settings(one_runner(), 2);
    wide["jobCpu"] = json!(32);
    refused(
        &wide,
        REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: runners 1 (pool) gives each job 32 CPUs and the machine has 16, so Docker would refuse every job's container. Give jobCpu at most 16 and create the pool again\n",
    );

    // What does fit starts, and says the same numbers: three of the old runner's jobs in 32GiB, six in 64GiB (what
    // such a machine reports is taken as the same share of its size).
    let gitlab = Gitlab::start(&[token("fits")]);
    for (concurrent, memory, words) in [
        (
            3,
            REPORTED_32GIB,
            "1 registrations, at most 3 jobs at once, 26112 MiB of the 27921 MiB the machine has for jobs (3 of pool at 8704 MiB)\n",
        ),
        (
            6,
            2 * REPORTED_32GIB,
            "1 registrations, at most 6 jobs at once, 52224 MiB of the 59938 MiB the machine has for jobs (6 of pool at 8704 MiB)\n",
        ),
    ] {
        let mut pool = Pool::new(&old_runner(concurrent), memory);
        pool.secret("GITLAB_RUNNER_TOKEN_1", &token("fits"));
        pool.start(&gitlab.url);
        wait_for_words(&pool, words);
        wait_for_runner(&pool, &[RUNNER]);
        assert!(
            pool.config().contains(
                "    memory = \"8192m\"\n    memory_swap = \"8192m\"\n    cpus = \"4\"\n"
            )
        );
        assert_eq!(pool.stop(), Some(0));
    }
    // Two lanes in the large pool, as a user with two sizes of job has them: each lane counts its own limit times
    // its own size, under the one cap. Three large and seven standard jobs are more than a 64GiB machine has.
    let lanes = |standard: u64, overcommit: u64| {
        let mut settings = settings(
            json!([
                { "name": "large", "limit": 3, "jobCpu": 8, "jobMemory": "9728MiB" },
                { "name": "standard", "limit": standard, "jobCpu": 4, "jobMemory": "4608MiB" }
            ]),
            10,
        );
        settings["memoryOvercommitPercent"] = json!(overcommit);
        settings
    };
    refused(
        &lanes(7, 100),
        2 * REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: its jobs could take 66560 MiB at once (3 of large at 10240 MiB, 7 of standard at 5120 MiB) and the machine has 59938 MiB for jobs (the 64034 MiB it reports, less reserveMemory 4096 MiB): 6622 MiB too much.",
    );
    // A machine that may be overcommitted is still refused past its factor, and the refusal names it.
    let mut one_cap = settings(one_runner(), 9);
    one_cap["jobMemory"] = json!("9728MiB");
    one_cap["memoryOvercommitPercent"] = json!(150);
    refused(
        &one_cap,
        2 * REPORTED_32GIB,
        "arca-gitlab-pool: the pool does not start: its jobs could take 92160 MiB at once (9 of pool at 10240 MiB) and the machine has 59938 MiB for jobs (the 64034 MiB it reports, less reserveMemory 4096 MiB), which memoryOvercommitPercent 150 makes 89907 MiB: 2253 MiB too much.",
    );
    let warning = |percent: &str, times: &str| {
        format!(
            "arca-gitlab-pool: WARNING: memoryOvercommitPercent is {percent}: the jobs' memory caps may add up to {times} times the 59938 MiB the machine has for jobs. Every container keeps its own cap, so a job that passes its cap is the one that is stopped, not its neighbours. But if many large jobs peak together the machine itself can run out of memory, and the kernel then picks what it stops\n"
        )
    };
    one_cap["concurrent"] = json!(8);
    for (settings, words, risk) in [
        // The lanes fit with five standard jobs: the limits, not the cap, say how many run.
        (
            lanes(5, 100),
            "2 registrations, at most 8 jobs at once, 56320 MiB of the 59938 MiB the machine has for jobs (3 of large at 10240 MiB, 5 of standard at 5120 MiB)\n".to_string(),
            None,
        ),
        // Or with all ten, when the owner lets the caps add up to more than the machine has: said at the start and
        // in every status, with the factor and what is risked.
        (
            lanes(7, 115),
            "2 registrations, at most 10 jobs at once, 66560 MiB where the machine has 59938 MiB for jobs (3 of large at 10240 MiB, 7 of standard at 5120 MiB): more than it has, as memoryOvercommitPercent 115 allows (up to 68928 MiB)\n".to_string(),
            Some(warning("115", "1.15")),
        ),
        // The fallback for jobs that cannot be told apart: one cap, the largest job's, for eight jobs.
        (
            one_cap.clone(),
            "1 registrations, at most 8 jobs at once, 81920 MiB where the machine has 59938 MiB for jobs (8 of pool at 10240 MiB): more than it has, as memoryOvercommitPercent 150 allows (up to 89907 MiB)\n".to_string(),
            Some(warning("150", "1.50")),
        ),
    ] {
        let mut pool = Pool::new(&settings, 2 * REPORTED_32GIB);
        pool.secret("GITLAB_RUNNER_TOKEN_1", &token("fits"));
        pool.secret("GITLAB_RUNNER_TOKEN_2", &token("fits"));
        pool.start(&gitlab.url);
        wait_for_words(&pool, &words);
        wait_for(&pool, "a status block", || !totals(&pool).is_empty());
        match risk {
            Some(risk) => {
                assert!(pool.output().contains(&format!("{words}{risk}")), "{}", pool.output());
                assert!(pool.read("logs/status.txt").contains(&risk));
            }
            None => assert!(!pool.output().contains("memoryOvercommitPercent")),
        }
        assert_eq!(pool.stop(), Some(0));
    }
    // Limits that add up to less than the cap: the status says how many jobs can really run.
    let mut pool = Pool::new(&mixed(5), REPORTED_32GIB);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &token("fits"));
    pool.secret("GITLAB_RUNNER_TOKEN_2", &token("fits"));
    pool.start(&gitlab.url);
    wait_for_words(
        &pool,
        "2 registrations, at most 5 jobs at once, 27136 MiB of the 27921 MiB the machine has for jobs (1 of large at 16896 MiB, 4 of small at 2560 MiB)\n",
    );
    assert_eq!(pool.stop(), Some(0));
}

/// What `tailscale status --json` answers on a node that is on the tailnet: itself, and every node the tailnet shows
/// it, each with the name its machine reported (which is not what a node is found by), its full name on the tailnet,
/// which ends with a dot, and its addresses.
fn tailnet_status(nodes: &[(&str, &str, &[&str])]) -> String {
    let peers: serde_json::Map<String, Value> = nodes
        .iter()
        .enumerate()
        .map(|(n, (host_name, dns_name, addresses))| {
            (
                format!("nodekey:{n:064x}"),
                json!({ "HostName": host_name, "DNSName": dns_name, "TailscaleIPs": addresses, "Online": true }),
            )
        })
        .collect();
    json!({
        "BackendState": "Running",
        "Self": {
            "HostName": "pool1",
            "DNSName": "pool1.tail0example.ts.net.",
            "TailscaleIPs": ["100.64.0.7", "fd7a:115c:a1e0::7"]
        },
        "Peer": peers
    })
    .to_string()
}

const KEY: &str = "tskey-auth-synthetic0123456789-abcdefghijklmnopqrstuvwxyz";
const SPENT_KEY: &str = "tskey-auth-spent0123456789-abcdefghijklmnopqrstuvwxyz";
/// Another node of the tailnet, which is not GitLab.
const DATABASE: (&str, &str, &[&str]) = ("db", "db.tail0example.ts.net.", &["100.64.0.3"]);

#[test]
fn reach_tailnet_joins_with_the_key_as_a_file_and_finds_gitlab_by_its_full_tailnet_name() {
    let good = token("tailnet");
    let gitlab = Gitlab::start(std::slice::from_ref(&good));
    // The url is GitLab's full name on the tailnet, the name its certificate is for.
    let name = "gitlab.tail0example.ts.net";
    let named = |text: String| text.replace("gitlab.example.com", name);
    let mut settings = settings(one_runner(), 4);
    settings["reach"] = json!("tailnet");
    settings["url"] = json!(format!("https://{name}"));
    let mut pool = Pool::new(&settings, REPORTED_32GIB);
    let status = pool.dir.join("docker/tailnet-status.json");
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    // The tailnet does not take the first key.
    fs::write(&status, r#"{"BackendState":"NeedsLogin"}"#).expect("the node's state");
    pool.start(&gitlab.url);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: waiting for secrets: reach \"tailnet\" needs TAILSCALE_AUTHKEY. Set each with service_secret_set, then run instance_start\n",
    );
    assert!(pool.read("docker/calls.log").is_empty());
    pool.secret("TAILSCALE_AUTHKEY", &format!("{SPENT_KEY}\n"));
    wait_for_words(
        &pool,
        "arca-gitlab-pool: the tailnet did not take TAILSCALE_AUTHKEY: the key was used already, has expired or was revoked. Set another (service_secret_set, then instance_start); trying again in ",
    );
    assert!(
        gitlab.seen().is_empty(),
        "GitLab is not asked before the machine is on the tailnet"
    );
    // The next key is taken, and GitLab is a node of the tailnet. The tailnet writes its full name with a dot at
    // the end and in its own case, and lists an IPv6 address first; the name the machine itself reported is another.
    // The address is this test's own GitLab: curl connects where it is told, so the token check proves it is used.
    pool.secret("TAILSCALE_AUTHKEY", &format!("{KEY}\n"));
    fs::write(
        &status,
        tailnet_status(&[
            DATABASE,
            (
                "ip-10-0-0-5",
                "GitLab.tail0example.ts.net.",
                &["fd7a:115c:a1e0::9", "127.0.0.1"],
            ),
        ]),
    )
    .expect("the node's state");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: the machine is on the tailnet as 100.64.0.7; the url's name gitlab.tail0example.ts.net is the name of a node of this tailnet, at 127.0.0.1: GitLab Runner and every job are told that address\n",
    );
    wait_for_runner(&pool, &[RUNNER, TAILNET]);
    // The node is in the machine's own network with the capability and the device an interface needs, and it is
    // told where its key is, not what it is.
    let run = pool.dir.join("run/tailnet");
    assert_eq!(
        pool.read(&format!("docker/containers/{TAILNET}/args")),
        format!(
            "-d --name {TAILNET} --label arca.pool=pool1 --network host --restart no --cap-add NET_ADMIN --device /dev/net/tun -v {}:/run/arca-tailnet:ro -v {}:/var/lib/tailscale -e TS_AUTHKEY=file:/run/arca-tailnet/authkey -e TS_AUTH_ONCE=true -e TS_USERSPACE=false -e TS_STATE_DIR=/var/lib/tailscale -e TS_SOCKET=/tmp/tailscaled.sock -e TS_HOSTNAME=pool1 -e TS_ACCEPT_DNS=false {TAILNET_IMAGE}\n",
            run.display(),
            pool.dir.join("state/tailnet").display()
        )
    );
    assert_eq!(
        fs::read_to_string(run.join("authkey")).expect("the key's copy"),
        KEY
    );
    assert_eq!(mode(&run.join("authkey")), 0o400);
    assert_eq!(mode(&run), 0o700);
    assert_eq!(mode(&pool.dir.join("state/tailnet")), 0o700);
    // The node is asked for its status and for nothing else: a name is never given to its own lookup.
    assert!(
        pool.read("docker/calls.log")
            .lines()
            .filter(|line| line.starts_with("exec "))
            .all(|line| line
                == format!("exec {TAILNET} tailscale --socket /tmp/tailscaled.sock status --json"))
    );
    // GitLab's address on the tailnet is told to the token check, to GitLab Runner and to every job, for the url's
    // own name: the certificate is still checked against that name.
    assert!(
        pool.read("curl.log")
            .lines()
            .all(|line| line.ends_with(&format!(
                "--data-binary @- --resolve 127.0.0.1:{}:127.0.0.1 {}/runners/verify",
                gitlab.port, gitlab.url
            )))
    );
    assert!(
        pool.read(&format!("docker/containers/{RUNNER}/args"))
            .contains(&format!(" --add-host {name}:127.0.0.1 "))
    );
    assert_eq!(
        pool.config(),
        named(head(4) + &section("pool", &good, 0, 2, 2048, false, Some("127.0.0.1")))
    );
    wait_for(&pool, "the reach in the status", || {
        pool.read("logs/status.txt").contains(
            "arca-gitlab-pool: pool pool1 for https://gitlab.tail0example.ts.net, reach tailnet (GitLab at 127.0.0.1 on the tailnet, a node by the url's own name), at most 4 jobs at once",
        )
    });

    // The node stops, and GitLab is no longer a node the tailnet shows: the node is started again, the console says
    // that the name is now left to DNS, and GitLab Runner is started again without the address.
    fs::write(&status, tailnet_status(&[DATABASE])).expect("GitLab leaves");
    fs::write(
        pool.dir.join(format!("docker/containers/{TAILNET}/exit")),
        "1",
    )
    .expect("the exit");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: the tailnet node is started again: it stopped, or TAILSCALE_AUTHKEY changed\narca-gitlab-pool: the machine is on the tailnet as 100.64.0.7; gitlab.tail0example.ts.net is not the name of a node of this tailnet, so it is looked up in DNS like any name, and must resolve there to an address this machine reaches. A name that only the tailnet's own DNS knows needs settings.gitlabNode\n",
    );
    wait_for(&pool, "GitLab Runner without the pinned address", || {
        pool.read(&format!("docker/runs.{RUNNER}")) == "xx"
            && pool.config() == named(head(4) + &section("pool", &good, 0, 2, 2048, false, None))
    });
    assert!(
        !pool
            .read(&format!("docker/containers/{RUNNER}/args"))
            .contains("--add-host")
    );
    wait_for(&pool, "the reach in the status", || {
        pool.read("logs/status.txt")
            .contains("reach tailnet (GitLab's name looked up in DNS), at most 4 jobs at once")
    });
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[good, KEY.into(), SPENT_KEY.into()]);
}

#[test]
fn a_gitlab_only_the_tailnets_dns_knows_is_found_by_the_node_the_settings_name() {
    let good = token("node");
    let gitlab = Gitlab::start(std::slice::from_ref(&good));
    // The url is a name of the owner's own domain, which no node has and ordinary DNS does not answer: the settings
    // say which node of the tailnet GitLab is, by its machine name there.
    let mut settings = settings(one_runner(), 4);
    settings["reach"] = json!("tailnet");
    settings["gitlabNode"] = json!("gitlab-vm");
    let mut pool = Pool::new(&settings, REPORTED_32GIB);
    let status = pool.dir.join("docker/tailnet-status.json");
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    pool.secret("TAILSCALE_AUTHKEY", KEY);
    // The tailnet does not show that node yet. A node whose machine reported that name is not it.
    fs::write(
        &status,
        tailnet_status(&[
            DATABASE,
            ("gitlab-vm", "old-box.tail0example.ts.net.", &["100.64.0.4"]),
        ]),
    )
    .expect("the node's state");
    pool.start(&gitlab.url);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: the machine is on the tailnet as 100.64.0.7, and the tailnet shows it no node named gitlab-vm (settings.gitlabNode). The nodes it shows: db, old-box. The name is the node's machine name on the tailnet, or its full tailnet name; and a node is shown only when the tailnet's access rules let this machine reach it. Looking again in ",
    );
    std::thread::sleep(Duration::from_millis(300));
    // Nothing goes on without it: GitLab is not asked and GitLab Runner is not started.
    assert!(gitlab.seen().is_empty() && pool.read("curl.log").is_empty());
    assert_eq!(pool.containers(), [TAILNET]);
    // The node appears. It is found without the tailnet node being started again.
    fs::write(
        &status,
        tailnet_status(&[
            DATABASE,
            (
                "ip-10-0-0-5",
                "gitlab-vm.tail0example.ts.net.",
                &["127.0.0.1", "fd7a:115c:a1e0::9"],
            ),
        ]),
    )
    .expect("the node's state");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: the machine is on the tailnet as 100.64.0.7; settings.gitlabNode names the tailnet's node gitlab-vm, at 127.0.0.1: GitLab Runner and every job are told that gitlab.example.com is at that address\n",
    );
    wait_for_runner(&pool, &[RUNNER, TAILNET]);
    assert_eq!(pool.read(&format!("docker/runs.{TAILNET}")), "x");
    // The address is given for the url's name, so the certificate is checked against the url's name.
    assert!(
        pool.read("curl.log")
            .lines()
            .all(|line| line.ends_with(&format!(
                "--data-binary @- --resolve 127.0.0.1:{}:127.0.0.1 {}/runners/verify",
                gitlab.port, gitlab.url
            )))
    );
    assert!(
        pool.read(&format!("docker/containers/{RUNNER}/args"))
            .contains(" --add-host gitlab.example.com:127.0.0.1 ")
    );
    assert_eq!(
        pool.config(),
        head(4) + &section("pool", &good, 0, 2, 2048, false, Some("127.0.0.1"))
    );
    wait_for(&pool, "the reach in the status", || {
        pool.read("logs/status.txt").contains(
            "reach tailnet (GitLab at 127.0.0.1 on the tailnet, the node settings.gitlabNode names), at most 4 jobs at once",
        )
    });
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[good.clone(), KEY.into()]);
    drop(pool);

    // A pool on the tailnet whose url is neither a node's name nor a name DNS answers is not told to join the
    // tailnet, which it has: it is told about settings.gitlabNode.
    settings["gitlabNode"] = json!("");
    let mut pool = Pool::new(&settings, REPORTED_32GIB);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    pool.secret("TAILSCALE_AUTHKEY", KEY);
    fs::write(
        pool.dir.join("docker/tailnet-status.json"),
        tailnet_status(&[DATABASE]),
    )
    .expect("the node's state");
    fs::write(pool.dir.join("curl-exit"), "6").expect("curl's exit status");
    pool.start("https://gitlab.example.com/api/v4");
    wait_for_words(
        &pool,
        "arca-gitlab-pool: GitLab cannot be reached: the name gitlab.example.com does not resolve from this machine, so its DNS does not know it. The machine is on the tailnet, and the name is neither the name of one of its nodes nor one that ordinary DNS answers. If only the tailnet's own DNS knows it, create the pool with settings.gitlabNode, the name of GitLab's node on the tailnet. Trying again in ",
    );
    assert!(!pool.output().contains("created with reach"));
    assert_eq!(pool.containers(), [TAILNET]);
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[good, KEY.into()]);
}

#[test]
fn a_short_disk_has_its_unused_images_removed_and_privileged_is_said_in_every_status() {
    let good = token("disk");
    let gitlab = Gitlab::start(std::slice::from_ref(&good));
    let mut settings = settings(one_runner(), 4);
    // What only a copy of the template can set: a create cannot.
    settings["privileged"] = json!(true);
    let mut pool = Pool::new(&settings, REPORTED_32GIB);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    let dir = pool.dir.clone();
    let free = move |file: &str, mib: u64| {
        fs::write(dir.join(file), (mib * MIB).to_string()).expect("the free space");
    };
    free("disk-free", 50_000);
    pool.start(&gitlab.url);
    let warning = "arca-gitlab-pool: WARNING: privileged is on: a job's container can take over this machine, read every token of the pool and reach whatever the machine reaches\n";
    wait_for(&pool, "a status with the free space", || {
        totals(&pool).ends_with(" disk_free=50000MiB")
    });
    assert!(pool.output().starts_with(&format!(
        "arca-gitlab-pool: pool pool1 for https://gitlab.example.com: 1 registrations, at most 4 jobs at once, 10240 MiB of the 27921 MiB the machine has for jobs (4 of pool at 2560 MiB)\n{warning}"
    )));
    assert!(pool.read("logs/status.txt").contains(warning));
    assert_eq!(
        pool.config(),
        head(4) + &section("pool", &good, 0, 2, 2048, true, None)
    );
    assert!(!pool.read("docker/calls.log").contains("image prune"));

    // Less than freeDisk (40GiB) is free: the unused images go, and the status has the space there is now.
    free("disk-free-after", 90_000);
    free("disk-free", 30_000);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: disk: 30000 MiB were free, less than freeDisk (40960 MiB), so the images no job uses were removed: 90000 MiB are free now\n",
    );
    assert!(pool.read("docker/calls.log").contains("image prune -af\n"));
    wait_for(&pool, "the free space after", || {
        totals(&pool).ends_with(" disk_free=90000MiB")
    });
    // A disk that stays short is the jobs' own files: it is said in the status, as a warning.
    free("disk-free-after", 31_000);
    wait_for(&pool, "the warning of a short disk", || {
        pool.read("logs/status.txt").contains(
            "arca-gitlab-pool: WARNING: 31000 MiB of disk are free, less than freeDisk (40960 MiB), after the unused images were removed: the jobs that run now fill it, and a job that needs more fails\n",
        ) && totals(&pool).ends_with(" disk_free=31000MiB")
    });
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[good]);
}

#[test]
fn the_shipped_templates_are_two_sizes_of_one_pool_with_its_script_its_secrets_and_pins() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let read = |name: &str| -> Value {
        serde_json::from_str(
            &fs::read_to_string(dir.join(format!("{name}.json"))).expect("the file"),
        )
        .expect("JSON")
    };
    let mut secrets: Vec<String> = (1..=12)
        .map(|n| format!("GITLAB_RUNNER_TOKEN_{n}"))
        .collect();
    secrets.push("TAILSCALE_AUTHKEY".into());
    // What a machine of each size reports: 27921MiB for jobs on the first 32GiB pool, and the same share of 64GiB.
    for (name, cpu, memory, disk, concurrent, reported) in [
        (
            "gitlab-runner-pool",
            "16",
            "32GiB",
            "200GiB",
            10,
            REPORTED_32GIB,
        ),
        (
            "gitlab-runner-pool-large",
            "32",
            "64GiB",
            "300GiB",
            20,
            2 * REPORTED_32GIB,
        ),
    ] {
        let template = arca::service::load_template(&dir, name).expect("the template loads");
        assert!(template.is_vm());
        // The cloud variant of the public Debian 13 image: the one with cloud-init, which runs the first-boot script.
        assert_eq!(template.image, "images:debian/13/cloud");
        let limits = template.limits.as_ref().expect("limits");
        assert_eq!(
            (
                limits.cpu.as_deref(),
                limits.memory.as_deref(),
                template.root_size.as_deref()
            ),
            (Some(cpu), Some(memory), Some(disk))
        );
        assert_eq!(template.secrets, secrets);
        assert!(template.ports.is_empty() && template.provides.is_none());
        assert_eq!(
            template.create_settings,
            [
                "url",
                "concurrent",
                "jobCpu",
                "jobMemory",
                "serviceMemory",
                "jobServices",
                "runners",
                "memoryOvercommitPercent",
                "reach",
                "gitlabNode"
            ]
        );
        // Both sizes boot the one script, which is the controller itself, and `arca templates install` ships it.
        let script = arca::service::first_boot(&template, &dir)
            .expect("the script")
            .expect("some");
        assert_eq!(
            script,
            fs::read_to_string(self::script()).expect("the script")
        );
        assert!(script.starts_with("#!/bin/bash\n"));
        for file in [format!("{name}.json"), "gitlab-runner-pool.sh".to_string()] {
            assert!(
                arca::cmd_templates::SHIPPED
                    .iter()
                    .any(|(shipped, _)| *shipped == file)
            );
        }
        let settings = template.settings.as_ref().expect("settings");
        // No address ships: a self-hosted GitLab has a host of its own, and a create gives it.
        assert_eq!(settings["url"], "");
        assert_eq!(settings["privileged"], false);
        assert_eq!(settings["memoryOvercommitPercent"], 100);
        assert_eq!(settings["gitlabNode"], "");
        assert_eq!(settings["reach"], "direct");
        assert_eq!(settings["concurrent"], concurrent);
        for key in ["runnerImage", "helperImage", "tailnetImage"] {
            let image = settings[key].as_str().expect("an image");
            let (_, digest) = image.split_once("@sha256:").expect("a digest");
            assert!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "{image}"
            );
        }
        // The description names the release the digest is of and says, in so many words, what the boundary is,
        // what does not belong in a pool, and how capacity is added.
        let description = template.description.as_deref().expect("a description");
        for words in [
            "GitLab Runner 19.5.3 by digest",
            "the machine is the boundary, not the container",
            "keep a signing or attested runner in a pool of its own or outside",
            "More capacity is a second pool with the same tokens",
            &format!("{cpu} CPUs, {memory}, {disk}"),
        ] {
            assert!(description.contains(words), "{words}: {description}");
        }
        // The default jobs fit what such a machine really has once the reserve is kept back.
        let bytes = |key: &str| {
            u64::try_from(
                arca::service::size_bytes(settings[key].as_str().expect("a size")).expect("bytes"),
            )
            .expect("a size that fits")
        };
        let job = bytes("jobMemory")
            + settings["jobServices"].as_u64().expect("a count") * bytes("serviceMemory");
        assert!(
            concurrent * job + bytes("reserveMemory") <= reported,
            "{name}"
        );
        assert!(settings["jobCpu"].as_u64().expect("CPUs") <= cpu.parse::<u64>().expect("CPUs"));

        // A create gives the address and the registrations, as the first user would: nine registrations of the
        // old runner's size. The kernel takes them; a setting a create may not give is refused by name.
        let nine: Vec<Value> = (1..=9)
            .map(|n| json!({ "name": format!("runner-{n}"), "limit": 2, "jobCpu": 4, "jobMemory": "8GiB" }))
            .collect();
        let given = json!({ "url": "https://gitlab.example.com", "concurrent": 3, "runners": nine, "reach": "tailnet" });
        let merged = arca::service::create_settings(&template, Some(&given))
            .expect("the create's settings are taken")
            .merged
            .expect("merged");
        assert_eq!(merged["runners"].as_array().expect("runners").len(), 9);
        assert_eq!(merged["runnerImage"], settings["runnerImage"]);
        for (key, value) in [
            ("privileged", json!(true)),
            ("runnerImage", json!("registry.example/other@sha256:00")),
        ] {
            let refusal = arca::service::create_settings(&template, Some(&json!({ key: value })))
                .expect_err("not a create's to set");
            assert!(
                refusal.message.starts_with(&format!(
                    "settings.{key}: template {name} does not let a create set it"
                )),
                "{}",
                refusal.message
            );
        }

        // The shipped settings, with an address, start the controller: it reads them, counts the jobs against the
        // machine and waits for the one token.
        let mut with_url = settings.clone();
        with_url["url"] = json!("https://gitlab.example.com");
        let mut pool = Pool::new(&with_url, reported);
        pool.start("http://127.0.0.1:9/api/v4");
        wait_for_words(
            &pool,
            &format!(
                "arca-gitlab-pool: pool pool1 for https://gitlab.example.com: 1 registrations, at most {concurrent} jobs at once, {} MiB of the {} MiB the machine has for jobs ({concurrent} of pool at 2560 MiB)\narca-gitlab-pool: waiting for secrets: runners 1 (pool) needs GITLAB_RUNNER_TOKEN_1. Set each with service_secret_set, then run instance_start\n",
                concurrent * 2560,
                reported / MIB - 4096
            ),
        );
        assert_eq!(pool.stop(), Some(0));
    }

    // The large pool is the same definition: only its name, the size of its machine (in the limits and in the
    // description's own words) and the jobs it runs at once differ.
    let (mut standard, mut large) = (read("gitlab-runner-pool"), read("gitlab-runner-pool-large"));
    assert_eq!(
        large["description"].as_str().expect("a description"),
        standard["description"]
            .as_str()
            .expect("a description")
            .replace("16 CPUs, 32GiB, 200GiB", "32 CPUs, 64GiB, 300GiB")
    );
    for template in [&mut standard, &mut large] {
        let object = template.as_object_mut().expect("an object");
        for key in ["name", "description", "limits", "rootSize"] {
            object.remove(key).expect("the key");
        }
        template["settings"]
            .as_object_mut()
            .expect("settings")
            .remove("concurrent")
            .expect("concurrent");
    }
    assert_eq!(standard, large);
}

/// The first boot, as far as a test can take it: it cannot run apt or write under /etc, so a stand-in `apt-get`
/// "installs" and two declared programs stay missing. The first boot must then stop, name exactly those two, and say
/// so on the machine's console as well as on its own output.
#[test]
fn a_first_boot_names_the_commands_that_are_missing_on_the_console() {
    let packages = declared("POOL_PACKAGES");
    for (package, why) in [
        ("docker.io", "the Docker daemon"),
        (
            "docker-cli",
            "the docker command, which docker.io only recommends",
        ),
        ("jq", "jq"),
        ("curl", "curl"),
        (
            "ca-certificates",
            "the certificates curl checks GitLab with",
        ),
        ("libc-bin", "getent"),
        ("coreutils", "timeout, nproc and df"),
    ] {
        assert!(
            packages.iter().any(|name| name == package),
            "the first boot installs {package} ({why}); it installs {packages:?}"
        );
    }
    let programs = declared("POOL_PROGRAMS");
    for program in [
        "docker",
        "jq",
        "curl",
        "systemctl",
        "getent",
        "timeout",
        "df",
    ] {
        assert!(programs.iter().any(|name| name == program), "{program}");
    }

    // The positive control of every other test in this file: bash does report a program it does not find.
    let scratch = std::env::temp_dir().join(format!(
        "arca-gitlab-pool-boot-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    let record = scratch.join("not-found.log");
    let control = finish(
        Command::new(which("bash"))
            .arg("-c")
            .arg("no-such-program-of-this-test 2>/dev/null; true")
            .envs(report_missing_programs(&record)),
        "bash, asked for a program that does not exist,",
    );
    assert!(control.status.success());
    assert_eq!(
        fs::read_to_string(&record).unwrap_or_default(),
        "no-such-program-of-this-test\n"
    );

    let uid = finish(Command::new("id").arg("-u"), "id");
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        eprintln!(
            "as root the first boot is not run here: it would install itself on this machine"
        );
        let _ = fs::remove_dir_all(scratch);
        return;
    }
    let tools = scratch.join("tools");
    declared_programs(&tools, None, &["docker", "jq", "apt-get"]);
    executable(
        &tools.join("apt-get"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\n",
            scratch.join("apt.log").display()
        ),
    );
    let console = scratch.join("console");
    let boot = finish(
        Command::new(tools.join("bash"))
            .arg(script())
            .env("PATH", &tools)
            .env("POOL_CONSOLE", &console),
        "the first boot",
    );
    let said = String::from_utf8_lossy(&boot.stdout).into_owned();
    let refusal = format!(
        "arca-gitlab-pool: first boot failed: these commands are missing after the install: docker jq. The packages \
         asked for were: {}. The package manager's own words are in /var/log/cloud-init-output.log inside the \
         machine. The machine has no pool; service_remove it and create it again once it can reach its package mirror\n",
        packages.join(" ")
    );
    assert_eq!(boot.status.code(), Some(1), "{said}");
    assert!(said.ends_with(&refusal), "{said}");
    assert!(!said.contains("first boot done"), "{said}");
    // The console has the first boot's own lines, all of them: that is what service_logs returns.
    assert_eq!(fs::read_to_string(&console).expect("the console"), said);
    assert!(
        said.starts_with(&format!(
            "arca-gitlab-pool: first boot: installing {}\n",
            packages.join(" ")
        )),
        "{said}"
    );
    // The install names every package and takes no recommended one.
    assert_eq!(
        fs::read_to_string(scratch.join("apt.log")).expect("apt-get's arguments"),
        format!(
            "update -q\ninstall -y -q --no-install-recommends {}\n",
            packages.join(" ")
        )
    );
    let _ = fs::remove_dir_all(scratch);
}

/// A pool whose pauses after a refusal are five minutes, as the shipped ones are: a controller that does not obey a stop
/// then passes the bound `stop` gives it, and one that obeys is not slowed.
fn long_pauses(pool: &mut Pool) {
    for name in ["POOL_BACKOFF_MAX", "POOL_REFUSE_EVERY"] {
        pool.env.push((name, "300".into()));
    }
}

/// A stop sent while the controller pauses ends the pause, and the controller then begins nothing: no image is pulled
/// again, the tailnet node is not asked again and GitLab is not asked again. Every loop that pauses is stopped here,
/// in its pause.
#[test]
fn a_stop_during_a_pause_ends_it_and_nothing_is_begun_after() {
    let good = token("pause");
    let gitlab = Gitlab::start(std::slice::from_ref(&good));

    // Jobs that do not fit the machine: the reason is told again every five minutes.
    let mut large = settings(one_runner(), 8);
    large["jobCpu"] = json!(4);
    large["jobMemory"] = json!("8GiB");
    let mut pool = Pool::new(&large, REPORTED_32GIB);
    long_pauses(&mut pool);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    pool.start(&gitlab.url);
    wait_for_words(
        &pool,
        "arca-gitlab-pool: the pool does not start: its jobs could take ",
    );
    assert_eq!(pool.stop(), Some(1));
    drop(pool);

    // A registration without its secret: it looks again every five seconds.
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    pool.start(&gitlab.url);
    wait_for_words(&pool, "arca-gitlab-pool: waiting for secrets: ");
    assert_eq!(pool.stop(), Some(0));
    assert!(pool.read("docker/calls.log").is_empty() && gitlab.seen().is_empty());
    drop(pool);

    // An image that cannot be pulled: thirty seconds, and no pull after the stop.
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    fs::write(pool.dir.join("docker/no-images"), "").expect("no images");
    pool.start(&gitlab.url);
    wait_for_words(&pool, " could not be pulled; trying again in 30 seconds\n");
    let calls = pool.read("docker/calls.log");
    assert_eq!(pool.stop(), Some(0));
    assert_eq!(pool.read("docker/calls.log"), calls);
    assert!(gitlab.seen().is_empty());
    drop(pool);

    // A tailnet that does not show the node the settings name: the node is looked for again after a pause, and not
    // after a stop.
    let mut on_tailnet = settings(one_runner(), 4);
    on_tailnet["reach"] = json!("tailnet");
    on_tailnet["gitlabNode"] = json!("gitlab-vm");
    let mut pool = Pool::new(&on_tailnet, REPORTED_32GIB);
    long_pauses(&mut pool);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    pool.secret("TAILSCALE_AUTHKEY", KEY);
    fs::write(
        pool.dir.join("docker/tailnet-status.json"),
        tailnet_status(&[DATABASE]),
    )
    .expect("the node's state");
    pool.start(&gitlab.url);
    wait_for_words(&pool, ". Looking again in 5 seconds\n");
    let calls = pool.read("docker/calls.log");
    assert_eq!(pool.stop(), Some(0));
    assert_eq!(pool.read("docker/calls.log"), calls);
    assert!(gitlab.seen().is_empty());
    drop(pool);

    // A token GitLab does not take: it is asked again after a pause, and not after a stop.
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    long_pauses(&mut pool);
    pool.secret("GITLAB_RUNNER_TOKEN_1", &token("stale"));
    pool.start(&gitlab.url);
    wait_for_words(&pool, "Asking again in 5 seconds\n");
    let asked = gitlab.seen().len();
    assert_eq!(pool.stop(), Some(0));
    assert_eq!(gitlab.seen().len(), asked);
    drop(pool);

    // The pool at work, five minutes from its next pass.
    let mut pool = Pool::new(&settings(one_runner(), 4), REPORTED_32GIB);
    pool.env.push(("POOL_TICK", "300".into()));
    pool.secret("GITLAB_RUNNER_TOKEN_1", &good);
    pool.start(&gitlab.url);
    wait_for_runner(&pool, &[RUNNER]);
    wait_for(&pool, "the first status", || !totals(&pool).is_empty());
    assert_eq!(pool.stop(), Some(0));
    assert!(pool.output().ends_with("arca-gitlab-pool: stopped\n"));
}

/// A stop sent while the controller waits for GitLab's answer is read when the answer is there, and the pause that
/// would follow is not begun. GitLab keeps the check until the signal has been sent, so the signal finds the
/// controller's curl running.
#[test]
fn a_stop_while_gitlab_is_asked_is_obeyed_when_the_answer_comes_and_no_pause_follows() {
    let good = token("asked");

    // Three registrations: GitLab is asked about each token in turn, and each answer may take as long as GitLab is
    // given. A stop that comes during the first is read before the second is asked.
    let three = settings(
        json!([{ "name": "build" }, { "name": "test" }, { "name": "docs" }]),
        3,
    );
    let secrets = |pool: &Pool, value: &str| {
        for place in 1..=3 {
            pool.secret(&format!("GITLAB_RUNNER_TOKEN_{place}"), value);
        }
    };

    // Before the start: tokens GitLab does not take. Nobody waits to be told "asking again" now, so it is not said.
    let gitlab = Gitlab::start(std::slice::from_ref(&good));
    gitlab.change(|state| state.hold = true);
    let mut pool = Pool::new(&three, REPORTED_32GIB);
    long_pauses(&mut pool);
    secrets(&pool, &token("stale"));
    pool.start(&gitlab.url);
    wait_for(&pool, "the token check on its way", || gitlab.held() == 1);
    let sent = pool.term();
    gitlab.release();
    assert_eq!(pool.stopped(sent), Some(0));
    let output = pool.output();
    assert!(
        !output.contains("Asking again") && !output.contains("Trying again"),
        "{output}"
    );
    assert_eq!(gitlab.seen().len(), 1);
    assert_eq!(pool.containers(), Vec::<String>::new());
    drop(pool);

    // The pool at work: a pass that asks GitLab about the tokens again, and five minutes until the next pass. The
    // checks of the start are let through; the first one after GitLab Runner is started is the pass's.
    let gitlab = Gitlab::start(std::slice::from_ref(&good));
    gitlab.change(|state| state.hold = true);
    let mut pool = Pool::new(&three, REPORTED_32GIB);
    pool.env.push(("POOL_TICK", "300".into()));
    pool.env.push(("POOL_VERIFY_EVERY", "0".into()));
    secrets(&pool, &good);
    pool.start(&gitlab.url);
    let mut start = 0;
    wait_for(&pool, "the start", || {
        // Counted before the output is read: a check counted here came before whatever the output lacks.
        let held = gitlab.held();
        if pool.output().contains("; starting GitLab Runner\n") {
            return true;
        }
        start = held;
        gitlab.let_go();
        false
    });
    wait_for(&pool, "the pass that asks GitLab again", || {
        gitlab.held() > start
    });
    let sent = pool.term();
    gitlab.release();
    assert_eq!(pool.stopped(sent), Some(0));
    let output = pool.output();
    assert!(output.ends_with("arca-gitlab-pool: stopped\n"), "{output}");
    // The pass asked about one token and no more, and a check cut short is no failure of GitLab's.
    assert_eq!(gitlab.seen().len(), start + 1);
    assert!(!output.contains("WARNING: "), "{output}");
}
