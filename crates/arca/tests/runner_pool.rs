//! The runner pool's controller (`templates/github-runner-pool.sh run`) against a fake GitHub API and a fake `docker`.
//!
//! The controller is the shipped script, unchanged: only what it reaches outside itself is replaced. GitHub is a small
//! HTTP server in this file that checks every credential it is given. `docker` and `systemctl` are scripts on the
//! controller's `PATH` (`tests/runner_pool/fake-docker.sh` plays a runner: it takes one job with the just-in-time
//! configuration it was started with, and exits). `curl` is the real one behind a wrapper that records its arguments.
//! Paths, pauses and the machine's memory come from the environment.
//!
//! The controller's `PATH` holds those stand-ins and the programs the script declares in `POOL_PROGRAMS`, and nothing
//! else. A program the script starts without declaring it is therefore not found, bash reports it to a function this
//! test hands it, and `Pool::stop` fails on the report. The first boot checks that same list after it has installed
//! its packages, so a program the controller needs cannot go unchecked. What no test here can prove is that the
//! packages of `POOL_PACKAGES` bring those programs on a real Debian: the first boot's own check says so on the machine.
//!
//! So this proves the controller's own logic: registration, one job per runner, replacement, admission by memory, the
//! cap, cleanup, and that no credential reaches any output or any command line. It does not prove Docker, a real
//! runner, cgroup accounting or GitHub itself; `docs/OPERATING.md` lists what stays unproven.
//!
//! The test needs `bash`, `curl`, `jq`, `openssl` and `flock` on the machine that runs it. Every credential is made here.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

const ORG: &str = "example-org";
const CLIENT_ID: &str = "Iv23lisyntheticclient";
const PAT: &str = "github_pat_synthetic0123456789abcdefghijklmnop";
const RUNNER_IMAGE: &str = "registry.example/runner@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const DOCKER_IMAGE: &str =
    "registry.example/dind@sha256:2222222222222222222222222222222222222222222222222222222222222222";
const GIB: u64 = 1 << 30;
static SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------------------------------------------------
// The fake GitHub
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Runner {
    id: u64,
    name: String,
    labels: Vec<String>,
    jit: String,
    status: &'static str,
    /// How many jobs this registration was handed. A just-in-time runner takes one.
    jobs: usize,
    done: bool,
    deleted: bool,
    connected: bool,
    /// Whether this runner has updated itself, when GitHub asks runners to.
    updated: bool,
}

#[derive(Clone, Debug)]
struct Group {
    id: u64,
    name: &'static str,
    default: bool,
    visibility: &'static str,
    public: bool,
}

#[derive(Clone, Debug)]
struct Job {
    labels: Vec<String>,
    name: String,
    seconds: f64,
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    authorization: String,
    body: String,
}

#[derive(Default)]
struct State {
    /// The scope the controller must address: `/orgs/<org>` or `/repos/<owner>/<repo>`.
    scope: String,
    /// The App's public key, when the App is how the pool signs in.
    public_key: Option<PathBuf>,
    /// The tokens the API takes: the fine-grained token, and every installation token minted.
    tokens: Vec<String>,
    minted: Vec<String>,
    jwts: Vec<String>,
    runners: Vec<Runner>,
    queue: VecDeque<Job>,
    seen: Vec<Seen>,
    next_id: u64,
    /// The most runners that were connected and not finished at one moment.
    live_max: usize,
    finished: Vec<String>,
    /// The organisation's runner groups.
    groups: Vec<Group>,
    /// Whether GitHub tells each runner to update itself before it gives it a job, as it does for a runner older than
    /// the newest release.
    update_required: bool,
    /// How long an installation token is taken, when not for ever. GitHub's last an hour.
    token_life: Option<Duration>,
    minted_at: Vec<(String, Instant)>,
    /// Whether the repository a repository-level pool serves is public.
    repo_public: bool,
    /// Whether a runner group is answered without its `default` field, as no real GitHub does.
    group_without_default: bool,
}

struct Github {
    state: Arc<Mutex<State>>,
    url: String,
    scratch: PathBuf,
}

impl Github {
    fn start(scratch: &Path, scope: &str) -> Github {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!("http://{}", listener.local_addr().expect("the address"));
        let state = Arc::new(Mutex::new(State {
            scope: scope.to_string(),
            next_id: 100,
            groups: vec![
                Group {
                    id: 1,
                    name: "Default",
                    default: true,
                    visibility: "all",
                    public: false,
                },
                Group {
                    id: 7,
                    name: "arca pool",
                    default: false,
                    visibility: "selected",
                    public: false,
                },
            ],
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        let dir = scratch.to_path_buf();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&shared);
                let dir = dir.clone();
                std::thread::spawn(move || serve(stream, &shared, &dir));
            }
        });
        Github {
            state,
            url,
            scratch: scratch.to_path_buf(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the fake GitHub")
    }

    fn queue(&self, labels: &[&str], name: &str, seconds: f64) {
        self.state().queue.push_back(Job {
            labels: labels.iter().map(|label| label.to_string()).collect(),
            name: name.to_string(),
            seconds,
        });
    }

    /// The runners that are registered, connected and still waiting for or running their job.
    fn live(&self) -> Vec<Runner> {
        self.state()
            .runners
            .iter()
            .filter(|runner| runner.connected && !runner.done && !runner.deleted)
            .cloned()
            .collect()
    }

    fn registrations(&self) -> Vec<Seen> {
        self.state()
            .seen
            .iter()
            .filter(|seen| seen.path.ends_with("/actions/runners/generate-jitconfig"))
            .cloned()
            .collect()
    }

    /// The App's key pair, made for this test alone. Returns the private key in PEM form.
    fn app_key(&self) -> String {
        let key = self.scratch.join("app-key.pem");
        let public = self.scratch.join("app-key.pub");
        run_ok(
            Command::new("openssl")
                .args(["genrsa", "-out"])
                .arg(&key)
                .arg("2048"),
        );
        run_ok(
            Command::new("openssl")
                .args(["rsa", "-in"])
                .arg(&key)
                .args(["-pubout", "-out"])
                .arg(&public),
        );
        self.state().public_key = Some(public);
        fs::read_to_string(&key).expect("the key")
    }
}

/// The seconds a helper program is given, and the seconds the whole file is given. No wait in this file is without a
/// bound: a test that hangs holds a runner of the pool until somebody cancels the job.
const HELPER_SECONDS: u64 = 60;
const FILE_SECONDS: u64 = 600;

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
        match receiver.recv_timeout(Duration::from_secs(10)) {
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

fn run_ok(command: &mut Command) {
    let output = finish(command, &format!("{command:?}"));
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Sends a signal to a process or, with a negative number, to a whole process group. bash's own `kill` does it and
/// not the `kill` program. That program, from procps-ng up to 4.0.4, reads only the first digit of a negative number
/// that follows a signal option: it signals some other group, or, for a number that begins with 1, every process of
/// the user. Upstream fixed it in 4.0.5. Ubuntu 24.04, which the pool's job image is, carries it (4.0.4-4ubuntu3), and
/// so does Debian 12 (4.0.2); Debian 13, which the pool's own machine is, does not (4.0.4-9 has the fix).
/// It is bash and not `sh`: dash's `kill -KILL -- -<group>` refuses the `--` and signals nothing.
fn signal(name: &str, target: i64) -> bool {
    try_finish(
        Command::new("bash")
            .arg("-c")
            .arg(format!("kill -{name} -- {target}")),
        HELPER_SECONDS,
    )
    .is_ok_and(|output| output.status.success())
}

/// Ends a controller with everything in its process group, and waits for it, but not for ever.
fn end_controller(child: &mut Child) {
    let id = child.id();
    signal("KILL", -i64::from(id));
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(10);
    while matches!(child.try_wait(), Ok(None)) {
        if Instant::now() >= deadline {
            eprintln!(
                "the controller {id} was still there 10 seconds after it was killed; it is left behind"
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
                "the runner pool tests were still running after {FILE_SECONDS} seconds: a wait in \
                 crates/arca/tests/runner_pool.rs has no bound. Ending the controllers and the tests"
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

fn serve(mut stream: TcpStream, state: &Mutex<State>, scratch: &Path) {
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
    let mut request = lines.next().unwrap_or("").split(' ');
    let method = request.next().unwrap_or("").to_string();
    let path = request.next().unwrap_or("").to_string();
    let mut authorization = String::new();
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "authorization" => authorization = value.trim().to_string(),
                "content-length" => length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    let (code, answer) = answer(state, scratch, &method, &path, &authorization, &body);
    let text = answer.map(|value| value.to_string()).unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

fn message(code: u16, text: &str) -> (u16, Option<Value>) {
    (code, Some(json!({ "message": text })))
}

/// Whether `jwt` is one the App's key signed, for the App's client ID, for at most ten minutes.
fn jwt_ok(state: &Mutex<State>, scratch: &Path, jwt: &str) -> bool {
    let Some(public) = state.lock().expect("state").public_key.clone() else {
        return false;
    };
    let parts: Vec<&str> = jwt.split('.').collect();
    let [head, claims, signature] = parts.as_slice() else {
        return false;
    };
    let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).ok();
    let (Some(head_bytes), Some(claims_bytes), Some(signature_bytes)) =
        (decode(head), decode(claims), decode(signature))
    else {
        return false;
    };
    let head_json: Value = serde_json::from_slice(&head_bytes).unwrap_or(Value::Null);
    let claims_json: Value = serde_json::from_slice(&claims_bytes).unwrap_or(Value::Null);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock")
        .as_secs() as i64;
    let (iat, exp) = (
        claims_json["iat"].as_i64().unwrap_or(i64::MAX),
        claims_json["exp"].as_i64().unwrap_or(0),
    );
    if head_json["alg"] != "RS256"
        || claims_json["iss"] != CLIENT_ID
        || iat > now
        || exp <= now
        || exp - iat > 600
    {
        return false;
    }
    let id = SEQ.fetch_add(1, Ordering::SeqCst);
    let signed = scratch.join(format!("jwt-{id}.txt"));
    let signature_file = scratch.join(format!("jwt-{id}.sig"));
    fs::write(&signed, format!("{head}.{claims}")).expect("the signed text");
    fs::write(&signature_file, signature_bytes).expect("the signature");
    let verified = try_finish(
        Command::new("openssl")
            .args(["dgst", "-sha256", "-verify"])
            .arg(&public)
            .arg("-signature")
            .arg(&signature_file)
            .arg(&signed),
        HELPER_SECONDS,
    )
    .is_ok_and(|output| output.status.success());
    let _ = fs::remove_file(signed);
    let _ = fs::remove_file(signature_file);
    verified
}

fn answer(
    state: &Mutex<State>,
    scratch: &Path,
    method: &str,
    path: &str,
    authorization: &str,
    body: &str,
) -> (u16, Option<Value>) {
    let bearer = authorization.strip_prefix("Bearer ").unwrap_or("");
    // The runner's own side, which is not GitHub's API: a runner asks for its job with its configuration.
    if path == "/_fake/poll" || path == "/_fake/done" || path == "/_fake/updated" {
        let mut state = state.lock().expect("state");
        let Some(index) = state.runners.iter().position(|runner| runner.jit == body) else {
            return (200, Some(json!({ "gone": true })));
        };
        if path == "/_fake/done" {
            state.runners[index].done = true;
            let name = state.runners[index].name.clone();
            state.finished.push(name);
            return (200, Some(json!({})));
        }
        if state.runners[index].deleted || state.runners[index].jobs > 0 {
            return (200, Some(json!({ "gone": true })));
        }
        state.runners[index].connected = true;
        state.runners[index].status = "online";
        let live = state
            .runners
            .iter()
            .filter(|runner| runner.connected && !runner.done && !runner.deleted)
            .count();
        state.live_max = state.live_max.max(live);
        if path == "/_fake/updated" {
            state.runners[index].updated = true;
            return (200, Some(json!({})));
        }
        if state.update_required && !state.runners[index].updated {
            return (200, Some(json!({ "update": true })));
        }
        let labels = state.runners[index].labels.clone();
        let Some(at) = state
            .queue
            .iter()
            .position(|job| job.labels.iter().all(|label| labels.contains(label)))
        else {
            return (200, Some(json!({ "wait": true })));
        };
        let job = state.queue.remove(at).expect("the job");
        state.runners[index].jobs += 1;
        return (
            200,
            Some(json!({ "job": job.name, "seconds": job.seconds, "result": "Succeeded" })),
        );
    }
    {
        let mut state = state.lock().expect("state");
        state.seen.push(Seen {
            method: method.to_string(),
            path: path.to_string(),
            authorization: authorization.to_string(),
            body: body.to_string(),
        });
    }
    let scope = state.lock().expect("state").scope.clone();
    // The App's own endpoints take a JWT the App's key signed.
    if path == format!("{scope}/installation") || path == "/app/installations/4242/access_tokens" {
        if !jwt_ok(state, scratch, bearer) {
            return message(401, "A JSON web token could not be decoded");
        }
        let mut state = state.lock().expect("state");
        state.jwts.push(bearer.to_string());
        if method == "GET" {
            return (200, Some(json!({ "id": 4242 })));
        }
        let token = format!("ghs_synthetic{:020}", SEQ.fetch_add(1, Ordering::SeqCst));
        state.tokens.push(token.clone());
        state.minted.push(token.clone());
        state.minted_at.push((token.clone(), Instant::now()));
        return (
            201,
            Some(json!({ "token": token, "expires_at": "2099-01-01T00:00:00Z" })),
        );
    }
    let mut state = state.lock().expect("state");
    let expired = state.token_life.is_some_and(|life| {
        state
            .minted_at
            .iter()
            .any(|(token, at)| token == bearer && at.elapsed() > life)
    });
    if expired || !state.tokens.iter().any(|token| token == bearer) {
        return message(401, "Bad credentials");
    }
    // An organisation's runner groups. A repository has none.
    let groups = format!("{scope}/actions/runner-groups");
    let group_json = |group: &Group| {
        json!({ "id": group.id, "name": group.name, "default": group.default,
            "visibility": group.visibility, "allows_public_repositories": group.public })
    };
    if method == "GET" && scope.starts_with("/repos/") && path == scope {
        return (
            200,
            Some(
                json!({ "id": 5150, "full_name": &scope["/repos/".len()..], "private": !state.repo_public }),
            ),
        );
    }
    if method == "GET" && scope.starts_with("/orgs/") {
        if path.starts_with(&format!("{groups}?")) {
            let listed: Vec<Value> = state.groups.iter().map(group_json).collect();
            return (
                200,
                Some(json!({ "total_count": listed.len(), "runner_groups": listed })),
            );
        }
        if let Some(id) = path.strip_prefix(&format!("{groups}/")) {
            return match state.groups.iter().find(|group| group.id.to_string() == id) {
                Some(group) if state.group_without_default => {
                    (200, Some(json!({ "id": group.id, "name": group.name })))
                }
                Some(group) => (200, Some(group_json(group))),
                None => message(404, "Not Found"),
            };
        }
    }
    let runners = format!("{scope}/actions/runners");
    if method == "POST" && path == format!("{runners}/generate-jitconfig") {
        let Ok(request) = serde_json::from_str::<Value>(body) else {
            return message(422, "Invalid request");
        };
        let labels: Vec<String> = request["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_string))
            .collect();
        let name = request["name"].as_str().unwrap_or("").to_string();
        if name.is_empty()
            || labels.is_empty()
            || !(if scope.starts_with("/orgs/") {
                state
                    .groups
                    .iter()
                    .any(|group| request["runner_group_id"] == group.id)
            } else {
                request["runner_group_id"] == 1
            })
            || request["work_folder"] != "_work"
            || state
                .runners
                .iter()
                .any(|runner| runner.name == name && !runner.deleted)
        {
            return message(422, "Invalid request");
        }
        state.next_id += 1;
        let id = state.next_id;
        let jit = format!("jitconfig-synthetic-{id}-{}", "q".repeat(40));
        state.runners.push(Runner {
            id,
            name: name.clone(),
            labels,
            jit: jit.clone(),
            status: "offline",
            jobs: 0,
            done: false,
            deleted: false,
            connected: false,
            updated: false,
        });
        return (
            201,
            Some(json!({ "runner": { "id": id, "name": name }, "encoded_jit_config": jit })),
        );
    }
    if method == "GET" && path.starts_with(&format!("{runners}?")) {
        let listed: Vec<Value> = state
            .runners
            .iter()
            .filter(|runner| !runner.deleted && !runner.done)
            .map(|runner| json!({ "id": runner.id, "name": runner.name, "status": runner.status, "busy": false }))
            .collect();
        return (
            200,
            Some(json!({ "total_count": listed.len(), "runners": listed })),
        );
    }
    if method == "DELETE"
        && let Some(id) = path
            .strip_prefix(&format!("{runners}/"))
            .and_then(|id| id.parse::<u64>().ok())
    {
        return match state.runners.iter_mut().find(|runner| runner.id == id) {
            Some(runner) => {
                runner.deleted = true;
                (204, None)
            }
            None => message(404, "Not Found"),
        };
    }
    message(404, "Not Found")
}

// ---------------------------------------------------------------------------------------------------------------------
// The pool under test
// ---------------------------------------------------------------------------------------------------------------------

struct Pool {
    dir: PathBuf,
    child: Option<Child>,
    /// More environment for the controller and the stand-ins, set before `start`.
    env: Vec<(&'static str, String)>,
}

/// The id every pool of these tests has: the controller keeps one it finds on its disk.
const POOL_ID: &str = "abcd1234";

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/github-runner-pool.sh")
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
        panic!(
            "this test needs bash, curl, jq, openssl and flock on this machine; {tool} is missing"
        )
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

/// The environment that makes bash write the name of every program it does not find into `record`. bash calls a
/// function of this name for a command that is not on the `PATH`, whatever the command's own output was sent to.
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

fn settings(classes: Value, max_slots: u64, url: &str) -> Value {
    json!({
        "url": url,
        "runnerGroupId": 7,
        "maxSlots": max_slots,
        "reserveMemory": "2GiB",
        "classes": classes,
        "dockerCache": "90GiB",
        "dependencyCache": "30GiB",
        "runnerImage": RUNNER_IMAGE,
        "dockerImage": DOCKER_IMAGE
    })
}

fn small(slots: u64) -> Value {
    json!({ "name": "small", "labels": ["self-hosted", "linux", "x64", "arca"], "cpu": 2, "memory": "4GiB", "slots": slots })
}

impl Pool {
    /// A scratch machine: the settings as cloud-init would have written them, an empty secrets directory, and the
    /// stand-ins on the controller's `PATH`. Nothing runs yet.
    fn new(settings: &Value, memory_total: u64, memory_available: u64) -> Pool {
        bound_the_file();
        for tool in ["bash", "jq", "openssl", "flock"] {
            which(tool);
        }
        let curl = which("curl");
        let dir = std::env::temp_dir().join(format!(
            "arca-runner-pool-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        for sub in ["bin", "secrets", "docker", "cgroup", "etc"] {
            fs::create_dir_all(dir.join(sub)).expect("scratch");
        }
        fs::write(dir.join("etc/settings.json"), settings.to_string()).expect("settings");
        fs::create_dir_all(dir.join("state")).expect("scratch");
        fs::write(dir.join("state/pool-id"), format!("{POOL_ID}\n")).expect("the pool id");
        let pool = Pool {
            dir,
            child: None,
            env: Vec::new(),
        };
        pool.memory(memory_total, memory_available);
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/runner_pool/fake-docker.sh"),
            pool.dir.join("bin/docker"),
        )
        .expect("the fake docker");
        fs::set_permissions(
            pool.dir.join("bin/docker"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("0755");
        executable(
            &pool.dir.join("bin/systemctl"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\n",
                pool.dir.join("systemctl.log").display()
            ),
        );
        // Layering the runner's files is a mount, which only root makes: a stand-in records it and makes the
        // directory, or fails as a machine without overlay would.
        executable(
            &pool.dir.join("bin/mount"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{0}'\n[ ! -e '{1}' ] || exit 32\nfor last; do :; done\nmkdir -p \"$last\"\n",
                pool.dir.join("mount.log").display(),
                pool.dir.join("no-overlay").display()
            ),
        );
        executable(
            &pool.dir.join("bin/umount"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\n",
                pool.dir.join("umount.log").display()
            ),
        );
        // The real curl, with every argument recorded: a credential on a command line would be seen here.
        executable(
            &pool.dir.join("bin/curl"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexec '{}' \"$@\"\n",
                pool.dir.join("curl.log").display(),
                curl.display()
            ),
        );
        // Every other program the script declares, and none it does not.
        declared_programs(&pool.dir.join("tools"), Some(&pool.dir.join("bin")), &[]);
        pool
    }

    fn memory(&self, total: u64, available: u64) {
        let text = format!(
            "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\n",
            total / 1024,
            available / 1024,
            available / 1024
        );
        fs::write(self.dir.join("meminfo.new"), text).expect("meminfo");
        fs::rename(self.dir.join("meminfo.new"), self.dir.join("meminfo")).expect("meminfo");
    }

    /// What Arca does at `instance_start`: one private file per secret.
    fn secret(&self, name: &str, value: &str) {
        let path = self.dir.join("secrets").join(name);
        fs::write(&path, value).expect("a secret");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("0600");
    }

    fn start(&mut self, github: &Github) {
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
            .env("POOL_CGROUP", self.dir.join("cgroup"))
            .env("POOL_MEMINFO", self.dir.join("meminfo"))
            .env("POOL_UNITS", self.dir.join("units"))
            .env("POOL_API_URL", &github.url)
            .env("POOL_TICK", "0.1")
            .env("POOL_POLL", "0.1")
            .env("POOL_STATUS_EVERY", "1")
            .env("POOL_READY_WAIT", "5")
            .env("POOL_BACKOFF_MAX", "1")
            .env("POOL_STOP_WAIT", "10")
            .env("POOL_REFUSE_EVERY", "1")
            .env("POOL_GROUP_EVERY", "1")
            .envs(report_missing_programs(&self.dir.join("not-found.log")))
            .env("FAKE_PATH", std::env::var("PATH").unwrap_or_default())
            .envs(self.env.iter().map(|(key, value)| (*key, value.as_str())))
            .env("FAKE_DOCKER_DIR", self.dir.join("docker"))
            .env("FAKE_CGROUP", self.dir.join("cgroup"))
            .env("FAKE_GITHUB", &github.url)
            .env("FAKE_RUNNER_IMAGE", RUNNER_IMAGE)
            .env("FAKE_REAL_CURL", which("curl"))
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
        let mut child = self.child.take()?;
        let sent = signal("TERM", i64::from(child.id()));
        let deadline = Instant::now() + Duration::from_secs(20);
        let code = loop {
            if let Some(status) = child.try_wait().expect("the controller's status") {
                break status.code();
            }
            if !sent || Instant::now() >= deadline {
                // It is not left behind, and the test does not wait for it any longer.
                end_controller(&mut child);
                panic!(
                    "the controller did not stop within 20 seconds of SIGTERM (signal sent: {sent}):\n{}",
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

    /// The job records, without the header line: one list of fields per job.
    fn jobs(&self) -> Vec<Vec<String>> {
        format!(
            "{}{}",
            self.read("logs/jobs.tsv.1"),
            self.read("logs/jobs.tsv")
        )
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

    /// Everything the controller wrote or ran: its output, its files, and the arguments of every command it started.
    fn everything_written(&self) -> String {
        fn walk(dir: &Path, out: &mut String) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                // The secrets are Arca's files, and the run directory is the controller's private memory (mode 0700
                // on a tmpfs in the machine): the minted token and a pasted key put back into lines live there.
                // The tools directory is this machine's own programs, linked.
                if path.ends_with("secrets") || path.ends_with("run") || path.ends_with("tools") {
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
        // A test that failed comes here with its controller still running. Nothing here may wait without a bound:
        // this once waited for a controller that a `kill` program had not killed, and the job never ended.
        if let Some(mut child) = self.child.take() {
            end_controller(&mut child);
        }
        // A played runner ends when its container's directory is gone.
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

/// The last status block's totals line.
fn totals(pool: &Pool) -> String {
    pool.read("logs/status.txt")
        .lines()
        .rev()
        .find(|line| line.contains(" totals "))
        .unwrap_or("")
        .to_string()
}

/// Asserts that nothing the controller wrote or ran holds one of `secrets`.
fn no_credential(pool: &Pool, secrets: &[String]) {
    let written = pool.everything_written();
    assert!(
        written.contains("generate-jitconfig"),
        "the record of curl's arguments is read"
    );
    for secret in secrets {
        assert!(secret.len() >= 12, "a credential to look for");
        assert!(
            !written.contains(secret.as_str()),
            "a credential was written or put on a command line: it begins {}",
            &secret[..6]
        );
    }
}

/// The lines of a PEM key that are the key itself.
fn key_lines(pem: &str) -> Vec<String> {
    pem.lines()
        .filter(|line| !line.starts_with("-----") && line.len() >= 20)
        .map(str::to_string)
        .collect()
}

#[test]
fn an_app_registers_a_fresh_runner_per_job_replaces_it_and_prints_no_credential() {
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    let key = github.app_key();
    let classes = json!([
        small(2),
        { "name": "large", "labels": ["self-hosted", "linux", "x64", "arca-large"], "cpu": 8, "memory": "8GiB", "slots": 1 }
    ]);
    let mut pool = Pool::new(
        &settings(classes, 3, &format!("https://github.com/{ORG}")),
        64 * GIB,
        60 * GIB,
    );
    // Without a credential the controller waits, says which secrets to set, and asks GitHub nothing.
    pool.start(&github);
    wait_for(&pool, "the wait for a credential", || {
        pool.output().contains("waiting for a credential: set GITHUB_APP_CLIENT_ID and GITHUB_APP_PRIVATE_KEY, or GITHUB_TOKEN, with service_secret_set, then run instance_start")
    });
    assert!(github.state().seen.is_empty());
    pool.secret("GITHUB_APP_CLIENT_ID", CLIENT_ID);
    pool.secret("GITHUB_APP_PRIVATE_KEY", &key);
    // A token is set too: the App is the one used, and the pool says so.
    pool.secret("GITHUB_TOKEN", PAT);

    // Every slot registers one runner, with its class's labels, and the runner connects.
    wait_for(&pool, "three idle runners", || github.live().len() == 3);
    let first = github.live();
    let mut labels: Vec<String> = first.iter().map(|runner| runner.labels.join(",")).collect();
    labels.sort();
    assert_eq!(
        labels,
        vec![
            "self-hosted,linux,x64,arca",
            "self-hosted,linux,x64,arca",
            "self-hosted,linux,x64,arca-large"
        ]
    );
    assert!(
        first
            .iter()
            .all(|runner| runner.name.starts_with("pool1-abcd1234-"))
    );
    let output = pool.output();
    assert!(
        output.contains("signing in as the GitHub App: GITHUB_TOKEN is set too and is not used")
    );
    assert!(output.contains(
        "registering runners in runner group 7 \"arca pool\", selected repositories only, public repositories off"
    ));
    // The App's key signed two requests, and one installation token serves every registration.
    {
        let state = github.state();
        assert_eq!(state.minted.len(), 1);
        assert_eq!(state.jwts.len(), 2);
        let paths: Vec<&str> = state.seen.iter().map(|seen| seen.path.as_str()).collect();
        assert_eq!(
            &paths[..2],
            [
                "/orgs/example-org/installation",
                "/app/installations/4242/access_tokens"
            ]
        );
    }
    for registration in github.registrations() {
        assert_eq!(
            registration.authorization,
            format!("Bearer {}", github.state().minted[0])
        );
        // What is asked of GitHub: a name of this pool, the runner group, the class's labels and the work folder.
        let asked: Value = serde_json::from_str(&registration.body).expect("the request");
        assert_eq!(registration.method, "POST");
        assert_eq!(
            asked
                .as_object()
                .expect("an object")
                .keys()
                .collect::<Vec<_>>(),
            ["labels", "name", "runner_group_id", "work_folder"]
        );
        assert!(
            asked["name"]
                .as_str()
                .unwrap()
                .starts_with("pool1-abcd1234-")
        );
        assert_eq!(asked["runner_group_id"], 7);
    }

    // Four jobs: three for the small class, one that only the large class's labels take.
    for name in ["build", "test", "lint"] {
        github.queue(&["self-hosted", "arca"], name, 0.4);
    }
    github.queue(&["arca-large"], "integration", 0.4);
    wait_for(&pool, "four finished jobs", || pool.jobs().len() == 4);
    // Each finished runner is replaced by a fresh registration, so three are idle again.
    wait_for(&pool, "three fresh idle runners", || {
        github.live().len() == 3 && github.state().runners.len() == 7
    });
    {
        let state = github.state();
        // One job per runner: no registration was handed a second one, and no name was used twice.
        assert!(state.runners.iter().all(|runner| runner.jobs <= 1));
        assert_eq!(
            state
                .runners
                .iter()
                .filter(|runner| runner.jobs == 1)
                .count(),
            4
        );
        let mut names: Vec<&str> = state
            .runners
            .iter()
            .map(|runner| runner.name.as_str())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 7);
        let mut configs: Vec<&str> = state
            .runners
            .iter()
            .map(|runner| runner.jit.as_str())
            .collect();
        configs.sort();
        configs.dedup();
        assert_eq!(configs.len(), 7);
        assert!(state.live_max <= 3);
    }
    // Each job's record: its slot's class and labels, its name and result, how long it took, its memory peak and its
    // CPU seconds (the numbers the fake slice reports).
    let mut jobs = pool.jobs();
    jobs.sort_by(|a, b| a[4].cmp(&b[4]));
    assert_eq!(
        jobs.iter().map(|job| job[4].as_str()).collect::<Vec<_>>(),
        ["build", "integration", "lint", "test"]
    );
    for job in &jobs {
        assert_eq!(job.len(), 10, "{job:?}");
        let large = job[4] == "integration";
        assert_eq!(job[2], if large { "large" } else { "small" });
        assert_eq!(
            job[3],
            if large {
                "self-hosted,linux,x64,arca-large"
            } else {
                "self-hosted,linux,x64,arca"
            }
        );
        assert_eq!(job[5], "Succeeded");
        assert!(job[6].parse::<u64>().expect("seconds") <= 5);
        assert_eq!(job[7], "1610612736");
        assert_eq!(job[8], "73.4");
        assert!(job[9].starts_with(&format!("pool1-abcd1234-{}-{}-", job[2], job[1])));
    }
    let output = pool.output();
    assert!(output.contains("name=\"integration\" result=Succeeded"));
    assert!(output.contains("peak_memory_mib=1536 cpu_seconds=73.4"));
    wait_for(&pool, "a status block after the jobs", || {
        totals(&pool).contains("busy=0 idle=3 starting=0 queued=0 jobs_done=4 failures=0")
    });
    let status = pool.read("logs/status.txt");
    assert!(status.contains("class small last 3 jobs:"), "{status}");
    // The finished jobs are repeated in the block, so service_logs still reaches them after many blocks, and the
    // block names the pool, its id and its runner group.
    assert_eq!(
        status.matches("recent job finished=").count(),
        4,
        "{status}"
    );
    assert!(
        status.contains("name=\"integration\" result=Succeeded")
            && status.contains("peak_memory_mib=1536 cpu_seconds=73.4"),
        "{status}"
    );
    assert!(
        status.contains("pool pool1 id abcd1234 for https://github.com/example-org, runner group 7 \"arca pool\", selected repositories only, public repositories off"),
        "{status}"
    );
    assert!(status.contains("runner_updates=0"), "{status}");
    assert!(
        status.contains("peak memory avg 1536 MiB max 1536 MiB"),
        "{status}"
    );
    assert!(
        status.contains("memory_reserved=16384MiB of 63488MiB slots_cap=3"),
        "{status}"
    );

    // Each slot's slice caps memory at the slot's size, without swap, and CPU at its count.
    let slice = pool.read("units/arcapool-s0.slice");
    assert!(
        slice.contains("MemoryMax=4294967296\nMemorySwapMax=0\nCPUQuota=200%"),
        "{slice}"
    );
    assert!(
        pool.read("units/arcapool-s2.slice")
            .contains("MemoryMax=8589934592\nMemorySwapMax=0\nCPUQuota=800%")
    );
    // The commands: only the slot's Docker daemon is privileged, both containers are in the slot's slice, the runner
    // shares the daemon's network, and after a job both are removed and the slice is stopped.
    let calls = pool.read("docker/calls.log");
    let runs: Vec<&str> = calls
        .lines()
        .filter(|line| line.starts_with("run "))
        .collect();
    assert_eq!(runs.len(), 14);
    for run in &runs {
        let daemon = run.contains(DOCKER_IMAGE);
        assert_eq!(run.contains("--privileged"), daemon, "{run}");
        assert_eq!(run.contains(RUNNER_IMAGE), !daemon, "{run}");
        assert!(run.contains("--cgroup-parent arcapool-s"), "{run}");
        assert!(run.contains("--label arca.pool=pool1"), "{run}");
        // The runner and its daemon share one layered copy of the runner's files, at the runner's own path: the
        // runner writes it and the daemon reads it. The tools are no mount of their own, so a runner can replace
        // them when GitHub tells it to update itself, and the daemon then sees the new ones.
        assert!(!run.contains(":/home/runner/externals"), "{run}");
        assert_eq!(
            run.contains("/layer/home:/home/runner:ro "),
            daemon,
            "{run}"
        );
        assert_eq!(run.contains("/layer/home:/home/runner "), !daemon, "{run}");
        if !daemon {
            assert!(run.contains("--network container:pool1-s"), "{run}");
            // The configuration is a file on the controller's run directory, read by the container's own command:
            // it is neither on the command line nor in the container's environment as Docker records it.
            assert!(!run.contains("ACTIONS_RUNNER_INPUT_JITCONFIG -e"), "{run}");
            assert!(
                run.contains("/run/jit/") && run.contains(":/run/arca/jitconfig:ro"),
                "{run}"
            );
            assert!(
                run.ends_with("/bin/bash -c ACTIONS_RUNNER_INPUT_JITCONFIG=$(cat /run/arca/jitconfig) && export ACTIONS_RUNNER_INPUT_JITCONFIG && exec /home/runner/run.sh"),
                "{run}"
            );
        }
    }
    assert!(calls.contains("rm -fv pool1-s0-run pool1-s0-dind"));
    assert!(
        pool.read("systemctl.log")
            .contains("stop arcapool-s0.slice")
    );
    // The layered copy: the pinned image's files below, a fresh layer for each job above, taken away after it.
    let state_dir = pool.dir.join("state");
    let mounts = pool.read("mount.log");
    let first = mounts.lines().next().expect("a mount");
    let lower = first
        .split("lowerdir=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .expect("a lower layer");
    assert!(
        lower.starts_with(&format!("{}/runner-home/", state_dir.display())),
        "{first}"
    );
    assert!(calls.contains(&format!("cp -a pool1-externals:/home/runner/. {lower}/")));
    for slot in 0..3 {
        let layer = format!("{}/slots/{slot}/layer", state_dir.display());
        assert!(
            mounts.contains(&format!(
                "-t overlay overlay -o lowerdir={lower},upperdir={layer}/upper,workdir={layer}/work {layer}/home"
            )),
            "{mounts}"
        );
        assert!(
            pool.read("umount.log")
                .contains(&format!("-l {layer}/home"))
        );
        // While its runner waits for a job the slot's layer is there.
        assert!(std::path::Path::new(&layer).join("home").is_dir());
    }
    assert_eq!(mounts.lines().count(), 7, "one fresh layer for each runner");

    // Stopping: the idle runners are taken off GitHub and nothing is left running.
    let idle: Vec<u64> = github.live().iter().map(|runner| runner.id).collect();
    assert_eq!(pool.stop(), Some(0));
    assert!(pool.output().ends_with("arca-runner-pool: stopped\n"));
    assert_eq!(pool.containers(), Vec::<String>::new());
    // Every slot's layer is taken away with its runner.
    for slot in 0..3 {
        assert!(!state_dir.join(format!("slots/{slot}/layer")).exists());
    }
    {
        let state = github.state();
        for id in idle {
            assert!(
                state
                    .runners
                    .iter()
                    .any(|runner| runner.id == id && runner.deleted)
            );
        }
    }

    // No credential anywhere: not the key, not a signed token, not the installation token, not a runner's
    // configuration. The controller's output, its files and every command line it started were read.
    let mut secrets = key_lines(&key);
    secrets.push(PAT.to_string());
    {
        let state = github.state();
        secrets.extend(state.minted.iter().cloned());
        secrets.extend(state.jwts.iter().cloned());
        secrets.extend(state.runners.iter().map(|runner| runner.jit.clone()));
        // The token that was not used was never sent either.
        assert!(
            state
                .seen
                .iter()
                .all(|seen| !seen.authorization.contains(PAT))
        );
    }
    no_credential(&pool, &secrets);
    // No configuration file is left behind in the controller's run directory.
    assert_eq!(
        fs::read_dir(pool.dir.join("run/jit"))
            .expect("the directory")
            .count(),
        0
    );
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn a_slot_starts_only_with_memory_for_its_whole_size_and_under_the_cap() {
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");

    // Four slots of 4GiB on a machine with 13GiB, 2GiB of it kept back: two fit, and a third would need 12 of 11.
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().tokens.push(PAT.to_string());
    let mut pool = Pool::new(
        &settings(json!([small(4)]), 4, &format!("https://github.com/{ORG}")),
        13 * GIB,
        12 * GIB,
    );
    pool.secret("GITHUB_TOKEN", PAT);
    pool.start(&github);
    wait_for(&pool, "two idle runners", || github.live().len() == 2);
    wait_for(&pool, "the status of the two that wait", || {
        totals(&pool).contains("busy=0 idle=2 starting=0 queued=2")
    });
    let status = pool.read("logs/status.txt");
    assert_eq!(
        status
            .matches("queued (waiting for 4096 MiB of unreserved memory)")
            .count(),
        2,
        "{status}"
    );
    assert!(
        status.contains("memory_reserved=8192MiB of 11264MiB slots_cap=4"),
        "{status}"
    );
    // Sizes that can never all run at once are said with their numbers, at the start and in every status block: a
    // machine reports less memory than its size, and a slot left queued beside idle ones is otherwise a riddle.
    let too_much = "arca-runner-pool: WARNING: the 4 slots ask for 16384 MiB together and the machine has 11264 MiB for slots (the 13312 MiB it reports, less reserveMemory 2048 MiB): 5120 MiB too much, so they never all run at once. A machine reports less memory than the size it was given. Make the slots or reserveMemory smaller, or the machine larger, in the template, and create the pool again\n";
    assert!(status.contains(too_much), "{status}");
    assert!(pool.output().matches(too_much).count() >= 2);
    // Jobs flow through the two that fit, and at no moment were more than two runners alive.
    for n in 0..6 {
        github.queue(&["arca"], &format!("job-{n}"), 0.2);
    }
    wait_for(&pool, "six finished jobs", || pool.jobs().len() == 6);
    wait_for(&pool, "two idle runners again", || github.live().len() == 2);
    assert_eq!(github.state().live_max, 2);
    assert_eq!(github.state().runners.len(), 8);
    // A fine-grained token is used as it is: the App's endpoints are never asked.
    assert!(
        github
            .state()
            .seen
            .iter()
            .all(|seen| !seen.path.contains("/app/")
                && !seen.path.ends_with("/installation")
                && seen.authorization == format!("Bearer {PAT}"))
    );
    assert_eq!(pool.stop(), Some(0));
    no_credential(&pool, &[PAT.to_string()]);
    drop(pool);

    // Memory that is free now counts too: with less free than one slot's size nothing starts, whatever is unreserved.
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().tokens.push(PAT.to_string());
    let mut pool = Pool::new(
        &settings(json!([small(2)]), 2, &format!("https://github.com/{ORG}")),
        64 * GIB,
        3 * GIB,
    );
    pool.secret("GITHUB_TOKEN", PAT);
    pool.start(&github);
    wait_for(&pool, "both slots waiting for free memory", || {
        pool.read("logs/status.txt")
            .matches("queued (waiting for 4096 MiB of free memory)")
            .count()
            == 2
    });
    assert!(github.registrations().is_empty());
    pool.memory(64 * GIB, 40 * GIB);
    wait_for(&pool, "both slots started once memory is free", || {
        github.live().len() == 2
    });
    assert_eq!(pool.stop(), Some(0));
    drop(pool);

    // A slot larger than all the memory there is for slots never starts. With more slots than places the pool says
    // nothing about all of them together, so this is said on its own, with the class (once) and the numbers.
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().tokens.push(PAT.to_string());
    let big = json!({ "name": "big", "labels": ["self-hosted", "linux", "x64", "arca-large"], "cpu": 2, "memory": "16GiB", "slots": 2 });
    let mut pool = Pool::new(
        &settings(
            json!([small(1), big]),
            1,
            &format!("https://github.com/{ORG}"),
        ),
        13 * GIB,
        12 * GIB,
    );
    pool.secret("GITHUB_TOKEN", PAT);
    pool.start(&github);
    wait_for(&pool, "the one runner that fits", || {
        github.live().len() == 1
    });
    let never = "arca-runner-pool: WARNING: a slot of these classes is larger than the 11264 MiB the machine has for slots (the 13312 MiB it reports, less reserveMemory 2048 MiB): big (16384 MiB). Such a slot never starts, and the jobs that ask for its labels wait for ever. Make the class or reserveMemory smaller, or the machine larger, in the template, and create the pool again\n";
    wait_for(&pool, "the warning in a status block", || {
        pool.read("logs/status.txt").contains(never)
    });
    assert!(pool.output().matches(never).count() >= 2);
    assert!(!pool.output().contains("slots ask for"));
    assert_eq!(pool.stop(), Some(0));
    drop(pool);

    // The cap: four slots that all fit in memory, and three places. The fourth waits for a place and takes its turn
    // when a job ends.
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().tokens.push(PAT.to_string());
    let mut pool = Pool::new(
        &settings(json!([small(4)]), 3, &format!("https://github.com/{ORG}")),
        64 * GIB,
        60 * GIB,
    );
    pool.secret("GITHUB_TOKEN", PAT);
    // A small bound on the job file, so it is set aside while the jobs run.
    pool.env.push(("POOL_JOBS_MAX", "600".to_string()));
    pool.start(&github);
    wait_for(&pool, "three idle runners", || github.live().len() == 3);
    wait_for(&pool, "the status of the one that waits", || {
        pool.read("logs/status.txt")
            .contains("queued (waiting for a place: 3 slots is the cap)")
    });
    for n in 0..8 {
        github.queue(&["arca"], &format!("job-{n}"), 0.2);
    }
    wait_for(&pool, "eight finished jobs", || pool.jobs().len() == 8);
    // The slot that waited gets the place a finished job frees. Whether its runner is ready before the other three
    // have taken all eight jobs is a matter of timing, so jobs are given one at a time until it has had one.
    let mut more = 0;
    wait_for(&pool, "every slot to take its turn under the cap", || {
        let mut slots: Vec<String> = pool.jobs().iter().map(|job| job[1].clone()).collect();
        slots.sort();
        slots.dedup();
        if slots == ["0", "1", "2", "3"] {
            return true;
        }
        if pool.jobs().len() == 8 + more {
            github.queue(&["arca"], &format!("more-{more}"), 0.2);
            more += 1;
        }
        false
    });
    wait_for(&pool, "the jobs that were given", || {
        pool.jobs().len() == 8 + more
    });
    assert_eq!(github.state().live_max, 3);
    // The job file is bounded: past its limit it is set aside once, with its header written again, and the summary
    // still counts the jobs in both.
    let counted = format!("class small last {} jobs:", 8 + more);
    wait_for(&pool, "the summary over both job files", || {
        pool.read("logs/status.txt").contains(&counted)
    });
    let aside = pool.read("logs/jobs.tsv.1");
    assert!(
        aside.starts_with("finished\tslot\t") && aside.lines().count() >= 3,
        "{aside}"
    );
    assert!(pool.read("logs/jobs.tsv").contains("finished\tslot\t"));
    assert!(pool.read("logs/jobs.tsv").len() < 1200);
    assert!(!pool.dir.join("logs/jobs.tsv.2").exists());
    assert_eq!(pool.stop(), Some(0));
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn failures_are_counted_said_without_the_credential_and_paused_and_leftovers_are_cleaned() {
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    // A repository's pool, an App whose key was pasted on one line with its line breaks as the two characters \n, and
    // what an earlier controller left behind: a container, and runners GitHub still lists.
    let github = Github::start(&scratch, "/repos/example-org/example-repo");
    let key = github.app_key();
    for (id, name, status) in [
        (900, "pool1-abcd1234-small-0-deadbeef", "offline"),
        // Another pool: another name, the same name with another id, and a name this pool's name is the beginning of.
        (901, "another-pool-small-0-deadbeef", "offline"),
        (903, "pool1-ffffffff-small-0-deadbeef", "offline"),
        (904, "pool1-small-0-deadbeef", "offline"),
        (905, "pool1-abcd1234-small-0-deadbeef-and-more", "offline"),
        (902, "pool1-abcd1234-small-1-cafecafe", "online"),
    ] {
        github.state().runners.push(Runner {
            id,
            name: name.to_string(),
            labels: vec![],
            jit: format!("stale-{id}"),
            status,
            jobs: 0,
            done: false,
            deleted: false,
            connected: false,
            updated: false,
        });
    }
    let mut pool = Pool::new(
        &settings(
            json!([small(1)]),
            1,
            "https://github.com/example-org/example-repo/",
        ),
        64 * GIB,
        60 * GIB,
    );
    fs::create_dir_all(pool.dir.join("docker/containers/pool1-s0-run")).expect("a leftover");
    pool.secret("GITHUB_APP_CLIENT_ID", CLIENT_ID);
    // A key that is not the App's: GitHub refuses the signed token.
    let other = scratch.join("other.pem");
    run_ok(
        Command::new("openssl")
            .args(["genrsa", "-out"])
            .arg(&other)
            .arg("2048"),
    );
    let wrong = fs::read_to_string(&other).expect("the other key");
    pool.secret("GITHUB_APP_PRIVATE_KEY", &wrong);
    pool.start(&github);
    wait_for(&pool, "the refusal of the wrong key", || {
        pool.output().contains("GitHub sign-in failed: the App is not installed on https://github.com/example-org/example-repo, or its client ID or key is wrong: GitHub answered 401 (A JSON web token could not be decoded). Trying again in 1 seconds")
    });
    // The pause holds: within the next half second GitHub is not asked again.
    let asked = github.state().seen.len();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(github.state().seen.len(), asked);
    assert!(github.registrations().is_empty());

    // The right key, on one line. It is what service_secret_set and the next instance_start would write.
    pool.secret(
        "GITHUB_APP_PRIVATE_KEY",
        &key.trim_end().replace('\n', "\\n"),
    );
    wait_for(&pool, "a registered runner", || github.live().len() == 1);
    {
        let state = github.state();
        let stale = |id: u64| {
            state
                .runners
                .iter()
                .find(|runner| runner.id == id)
                .unwrap()
                .deleted
        };
        assert!(stale(900), "this pool's offline runner is taken off GitHub");
        for other in [901, 903, 904, 905] {
            assert!(!stale(other), "another pool's runner is left ({other})");
        }
        assert!(!stale(902), "a runner that is online is left");
        assert_eq!(
            state
                .seen
                .iter()
                .filter(|seen| seen.method == "DELETE")
                .count(),
            1
        );
        assert!(state.seen.iter().any(|seen| seen.method == "DELETE"
            && seen.path == "/repos/example-org/example-repo/actions/runners/900"));
    }
    assert!(
        pool.read("docker/calls.log")
            .contains("rm -fv id-pool1-s0-run")
    );
    wait_for(&pool, "the failure in the totals", || {
        totals(&pool).contains("failures=")
    });
    assert!(!totals(&pool).contains("failures=0"));

    // A runner that goes away before it takes a job (GitHub removed its registration) is a failure of the pool, not
    // a job: it is counted, its slot is cleaned and a fresh runner takes its place.
    let gone = github.live()[0].clone();
    let failures_before = pool.output().matches(" failed: ").count();
    github
        .state()
        .runners
        .iter_mut()
        .find(|runner| runner.id == gone.id)
        .unwrap()
        .deleted = true;
    wait_for(&pool, "the lost runner said", || {
        pool.output()
            .contains("slot 0 (small) failed: its runner stopped (exit 1) before it took a job")
    });
    wait_for(&pool, "a fresh runner in its place", || {
        github.live().iter().any(|runner| runner.id != gone.id)
    });
    // That runner had its files layered and never reached a job. The pool cannot tell a runner GitHub removed from
    // one that could not start that way, so the next runners start with their image's own files, and it says so
    // once: one bad start never keeps the pool from running.
    assert_eq!(
        pool.output()
            .matches("A runner stopped before its first job with its files layered. Runners start with their image's own files from now on")
            .count(),
        1
    );
    let calls = pool.read("docker/calls.log");
    let last_runner = calls
        .lines()
        .filter(|line| line.starts_with("run ") && line.contains(RUNNER_IMAGE))
        .next_back()
        .expect("a runner");
    assert!(!last_runner.contains(":/home/runner "), "{last_runner}");
    let last_daemon = calls
        .lines()
        .filter(|line| line.starts_with("run ") && line.contains(DOCKER_IMAGE))
        .next_back()
        .expect("a daemon");
    assert!(
        last_daemon.contains("/externals:/home/runner/externals:ro"),
        "{last_daemon}"
    );
    // A pool for one repository has no runner group: it says whether the repository is private.
    assert!(pool.output().contains(
        "registering runners in one private repository (a repository has no runner groups)"
    ));
    assert_eq!(
        pool.output().matches(" failed: ").count(),
        failures_before + 1
    );
    assert_eq!(pool.jobs().len(), 0);
    // A job still runs after all of it.
    github.queue(&["arca"], "after the failures", 0.2);
    wait_for(&pool, "the job", || pool.jobs().len() == 1);
    assert_eq!(pool.jobs()[0][4], "after the failures");
    assert_eq!(pool.stop(), Some(0));

    let mut secrets = key_lines(&key);
    secrets.extend(key_lines(&wrong));
    {
        let state = github.state();
        secrets.extend(state.minted.iter().cloned());
        secrets.extend(state.jwts.iter().cloned());
        secrets.extend(
            state
                .runners
                .iter()
                .filter(|runner| runner.jit.starts_with("jitconfig"))
                .map(|runner| runner.jit.clone()),
        );
        // The refused token was signed and sent too.
        secrets.extend(
            state
                .seen
                .iter()
                .filter(|seen| seen.path.ends_with("/installation"))
                .filter_map(|seen| {
                    seen.authorization
                        .strip_prefix("Bearer ")
                        .map(str::to_string)
                }),
        );
    }
    no_credential(&pool, &secrets);
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn settings_that_cannot_be_used_are_said_in_words_and_nothing_is_registered() {
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().tokens.push(PAT.to_string());
    let good = settings(json!([small(1)]), 1, &format!("https://github.com/{ORG}"));
    let with = |key: &str, value: Value| {
        let mut settings = good.clone();
        settings[key] = value;
        settings
    };
    for (bad, why) in [
        (
            with("url", json!("github.com/example-org")),
            "settings: url must begin https://",
        ),
        (
            with("url", json!("https://github.com/")),
            "settings: url names an organisation",
        ),
        (
            with("url", json!("https://github.com/a/b/c")),
            "settings: url names an organisation",
        ),
        // The URL's host is where the credential goes: a plain host name, with nothing that could make another
        // host of it.
        (
            with(
                "url",
                json!("https://github.com@elsewhere.example/example-org"),
            ),
            "settings: url: its host must be a plain host name (letters, digits, dots and dashes), with no user, password, query or fragment",
        ),
        (
            with("url", json!("https://user:word@github.com/example-org")),
            "settings: url: its host must be a plain host name",
        ),
        (
            with(
                "url",
                json!("https://github.com#@elsewhere.example/example-org"),
            ),
            "settings: url: its host must be a plain host name",
        ),
        (
            with(
                "url",
                json!("https://github.com?x=elsewhere.example/example-org"),
            ),
            "settings: url: its host must be a plain host name",
        ),
        (
            with("url", json!("https://github.com:port/example-org")),
            "settings: url: its host must be a plain host name",
        ),
        (
            with("url", json!("https://.github.com/example-org")),
            "settings: url: its host must be a plain host name",
        ),
        // The credential goes to the API's host: github.com has one API, and another host is named on purpose.
        (
            with("apiUrl", json!("https://api.elsewhere.example")),
            "settings: apiUrl: the API of github.com is https://api.github.com and nothing else",
        ),
        (
            with("url", json!("https://git.example/example-org")),
            "settings: url is on git.example, not on github.com. For a GitHub Enterprise host, set enterprise to true",
        ),
        (
            {
                let mut settings = with("url", json!("https://git.example/example-org"));
                settings["enterprise"] = json!(true);
                settings["apiUrl"] = json!("http://git.example/api/v3");
                settings
            },
            "settings: apiUrl must begin https:// and be on git.example or api.git.example",
        ),
        (
            {
                let mut settings = with("url", json!("https://git.example/example-org"));
                settings["enterprise"] = json!(true);
                settings["apiUrl"] = json!("https://git.example.elsewhere.example/api/v3");
                settings
            },
            "settings: apiUrl must begin https:// and be on git.example or api.git.example",
        ),
        (
            with("maxSlots", json!("many")),
            "settings: runnerGroupId and maxSlots are whole numbers",
        ),
        (
            with("reserveMemory", json!("2 gigs")),
            "settings: reserveMemory, dockerCache and dependencyCache are sizes such as 4GiB",
        ),
        (
            with("runnerImage", Value::Null),
            "settings: runnerImage and dockerImage are required",
        ),
        // A size no arithmetic can hold, and a size of nothing. (A class larger than the memory there is for slots is
        // a warning, not a refusal: the other classes still run.)
        (
            with(
                "classes",
                json!([{ "name": "small", "labels": ["arca"], "cpu": 2, "memory": "8589934592GiB", "slots": 1 }]),
            ),
            "settings: class small needs a name",
        ),
        (
            with(
                "classes",
                json!([{ "name": "small", "labels": ["arca"], "cpu": 2, "memory": "0GiB", "slots": 1 }]),
            ),
            "settings: class small needs a name",
        ),
        (
            with(
                "classes",
                json!([{ "name": "small", "labels": ["arca"], "cpu": 2, "memory": "4GiB", "slots": "99999999999999999999" }]),
            ),
            "settings: class small needs a name",
        ),
        (
            with("maxSlots", json!("99999999999999999999")),
            "settings: runnerGroupId and maxSlots are whole numbers",
        ),
        (
            with("classes", json!([])),
            "settings: classes needs at least one class",
        ),
        (
            with(
                "classes",
                json!([{ "name": "small", "labels": ["has space"], "cpu": 2, "memory": "4GiB", "slots": 1 }]),
            ),
            "settings: class small needs a name",
        ),
        (
            with(
                "classes",
                json!([{ "name": "small", "labels": ["arca"], "cpu": 2, "memory": "lots", "slots": 1 }]),
            ),
            "settings: class small needs a name",
        ),
    ] {
        let mut pool = Pool::new(&bad, 64 * GIB, 60 * GIB);
        pool.secret("GITHUB_TOKEN", PAT);
        pool.start(&github);
        wait_for(&pool, why, || pool.output().contains(why));
        // Its next line, which is written a moment later: waited for, not read once.
        wait_for(&pool, "the line that says the pool does not start", || {
            pool.output()
                .contains("the pool does not start until its settings are right")
        });
        pool.stop();
    }
    assert!(github.state().seen.is_empty());
    // An enterprise host that is named as one is taken, with its API on the host itself.
    let mut enterprise = with("url", json!("https://git.example/example-org"));
    enterprise["enterprise"] = json!(true);
    enterprise["apiUrl"] = json!("https://api.git.example");
    let mut pool = Pool::new(&enterprise, 64 * GIB, 60 * GIB);
    pool.start(&github);
    wait_for(&pool, "the enterprise settings load", || {
        pool.output()
            .contains("pool pool1 id abcd1234 for https://git.example/example-org: 1 slots")
    });
    pool.stop();
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn the_shipped_template_is_a_virtual_machine_with_its_script_its_secrets_and_pins() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let template =
        arca::service::load_template(&dir, "github-runner-pool").expect("the template loads");
    assert!(template.is_vm());
    // The cloud variant of the public Debian 13 image: the one with cloud-init, which runs the first-boot script.
    assert_eq!(template.image, "images:debian/13/cloud");
    assert_eq!(template.digest, None);
    let limits = template.limits.as_ref().expect("limits");
    assert_eq!(
        (
            limits.cpu.as_deref(),
            limits.memory.as_deref(),
            template.root_size.as_deref()
        ),
        (Some("16"), Some("32GiB"), Some("200GiB"))
    );
    assert_eq!(
        template.secrets,
        [
            "GITHUB_APP_CLIENT_ID",
            "GITHUB_APP_PRIVATE_KEY",
            "GITHUB_TOKEN"
        ]
    );
    assert!(template.ports.is_empty() && template.provides.is_none());
    // The first-boot script is the controller itself, and `arca templates install` ships both files.
    let script = arca::service::first_boot(&template, &dir)
        .expect("the script")
        .expect("some");
    assert!(script.starts_with("#!/bin/bash\n"));
    for name in ["github-runner-pool.json", "github-runner-pool.sh"] {
        assert!(
            arca::cmd_templates::SHIPPED
                .iter()
                .any(|(shipped, _)| *shipped == name)
        );
    }
    let settings = template.settings.as_ref().expect("settings");
    assert_eq!(settings["url"], "https://github.com/example-org");
    // Both images are pinned by digest, and the description names the releases the digests are of.
    for key in ["runnerImage", "dockerImage"] {
        let image = settings[key].as_str().expect("an image");
        let (_, digest) = image.split_once("@sha256:").expect("a digest");
        assert!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "{image}"
        );
    }
    let description = template.description.as_deref().expect("a description");
    assert!(
        description.contains("actions/runner 2.338.2")
            && description.contains("docker 29.9.3-dind")
    );
    // The default slots fit the machine: the cap admits them all, their CPUs are the machine's 16, and their memory
    // fits what a 32GiB machine really has for slots once the reserve is kept back (27921MiB on the first pool).
    let bytes =
        |text: &Value| arca::service::size_bytes(text.as_str().expect("a size")).expect("bytes");
    let classes = settings["classes"].as_array().expect("classes");
    let (mut slots, mut cpus, mut memory) = (0, 0, 0);
    for class in classes {
        let count = class["slots"].as_u64().expect("slots");
        slots += count;
        cpus += count * class["cpu"].as_u64().expect("cpu");
        memory += u128::from(count) * bytes(&class["memory"]);
    }
    assert_eq!(slots, settings["maxSlots"].as_u64().expect("the cap"));
    assert_eq!(cpus, 16);
    assert_eq!(
        memory + bytes(&settings["reserveMemory"]),
        arca::service::size_bytes("30GiB").expect("bytes")
    );
    assert!(
        memory <= 27921 * (1 << 20),
        "the slots fit a real 32GiB machine"
    );
    assert_eq!(classes.len(), 2);
    assert_eq!(classes[1]["memory"], "9GiB");
    // The same settings start the controller: it reads them, makes its slices and waits for a credential.
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    // What such a machine reports: 27921MiB for slots after the 4GiB kept back.
    let mut pool = Pool::new(settings, (27921 + 4096) * (1 << 20), 30 * GIB);
    pool.start(&github);
    wait_for(&pool, "the shipped settings load", || {
        pool.output().contains("pool pool1 id abcd1234 for https://github.com/example-org: 4 slots in 2 classes, at most 4 at once, 27921 MiB of memory for slots")
    });
    wait_for(&pool, "the wait for a credential", || {
        pool.output().contains("waiting for a credential")
    });
    assert!(
        pool.read("units/arcapool-s3.slice")
            .contains("MemoryMax=9663676416\nMemorySwapMax=0\nCPUQuota=600%")
    );
    assert_eq!(pool.stop(), Some(0));
    let _ = fs::remove_dir_all(scratch);
}

/// The first boot, as far as a test can take it: it cannot run apt or write under /etc, so a stand-in `apt-get`
/// "installs" and two declared programs stay missing. The first boot must then stop, name exactly those two, and say
/// so on the machine's console as well as on its own output.
#[test]
fn a_first_boot_names_the_commands_that_are_missing_on_the_console_and_installs_the_docker_command()
{
    // What a first boot on Debian 13 taught: docker.io is the daemon alone, and recommended packages are not
    // installed, so the package of each command has to be named.
    let packages = declared("POOL_PACKAGES");
    for (package, why) in [
        ("docker.io", "the Docker daemon"),
        (
            "docker-cli",
            "the docker command, which docker.io only recommends",
        ),
        ("jq", "jq"),
        ("curl", "curl"),
        ("openssl", "openssl"),
        (
            "ca-certificates",
            "the certificates curl checks GitHub with",
        ),
        ("util-linux", "flock"),
        ("hostname", "hostname"),
    ] {
        assert!(
            packages.iter().any(|name| name == package),
            "the first boot installs {package} ({why}); it installs {packages:?}"
        );
    }
    let programs = declared("POOL_PROGRAMS");
    for program in ["docker", "jq", "curl", "openssl", "flock", "systemctl"] {
        assert!(programs.iter().any(|name| name == program), "{program}");
    }

    // The positive control of every other test in this file: bash does report a program it does not find.
    let scratch = scratch_dir();
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
        "arca-runner-pool: first boot failed: these commands are missing after the install: docker jq. The packages \
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
            "arca-runner-pool: first boot: installing {}\n",
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

fn scratch_dir() -> PathBuf {
    bound_the_file();
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    scratch
}

/// A pool of one small slot for the organisation, signed in with the token, with `change` applied to its settings.
fn one_slot_pool(github: &Github, change: impl FnOnce(&mut Value)) -> Pool {
    github.state().tokens.push(PAT.to_string());
    let mut settings = settings(json!([small(1)]), 1, &format!("https://github.com/{ORG}"));
    change(&mut settings);
    let pool = Pool::new(&settings, 64 * GIB, 60 * GIB);
    pool.secret("GITHUB_TOKEN", PAT);
    pool
}

#[test]
fn the_default_runner_group_is_refused_and_a_dedicated_one_is_described() {
    let scratch = scratch_dir();
    // The shipped default, group 1: every repository the default group admits could send jobs to the pool.
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    let mut pool = one_slot_pool(&github, |settings| settings["runnerGroupId"] = json!(1));
    pool.start(&github);
    let refusal = "the pool does not start on the organisation's default runner group (1 \"Default\"): every repository that group admits could send its jobs here, and a job here can reach the pool's GitHub credential. Create a runner group for this pool, limited to selected repositories and with public repositories off, and create the pool with its id as runnerGroupId. The organisation's runner groups are: 1 \"Default\" (the default group), 7 \"arca pool\"";
    wait_for(&pool, "the refusal of the default group", || {
        pool.output().contains(refusal)
    });
    // It is told again, so it stays on the console, and nothing is registered meanwhile.
    wait_for(&pool, "the refusal told again", || {
        pool.output().matches(refusal).count() >= 2
    });
    assert!(github.registrations().is_empty());
    assert_eq!(pool.stop(), Some(0));
    drop(pool);

    // A group that does not exist names the ones that do, so the owner reads the right number from service_logs.
    let mut pool = one_slot_pool(&github, |settings| settings["runnerGroupId"] = json!(99));
    pool.start(&github);
    wait_for(&pool, "the unknown group", || {
        pool.output().contains("settings: runnerGroupId 99 is not a runner group of https://github.com/example-org. Its runner groups are: 1 \"Default\" (the default group), 7 \"arca pool\"")
    });
    assert!(github.registrations().is_empty());
    pool.stop();
    drop(pool);

    // The default group only when the settings say so in as many words, and then with a warning in every status.
    let mut pool = one_slot_pool(&github, |settings| {
        settings["runnerGroupId"] = json!(1);
        settings["allowDefaultRunnerGroup"] = json!(true);
    });
    pool.start(&github);
    wait_for(&pool, "a runner in the default group", || {
        github.live().len() == 1
    });
    wait_for(&pool, "the warning in the status", || {
        pool.read("logs/status.txt").contains("runner group 1 \"Default\", WARNING: open to every repository of the organisation (all), public repositories off, WARNING: the default group, allowed by allowDefaultRunnerGroup")
    });
    pool.stop();
    drop(pool);

    // A dedicated group that is wider than it should be is used, and said.
    github.state().groups.push(Group {
        id: 8,
        name: "wide",
        default: false,
        visibility: "all",
        public: true,
    });
    let mut pool = one_slot_pool(&github, |settings| settings["runnerGroupId"] = json!(8));
    pool.start(&github);
    wait_for(&pool, "the wide group described", || {
        pool.output().contains("registering runners in runner group 8 \"wide\", WARNING: open to every repository of the organisation (all), WARNING: public repositories admitted, so a pull request from anyone, once it is approved to run, runs code in this pool")
    });
    pool.stop();
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn a_runner_github_tells_to_update_updates_inside_its_container_and_then_takes_its_job() {
    let scratch = scratch_dir();
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().update_required = true;
    let mut pool = one_slot_pool(&github, |_| {});
    pool.start(&github);
    // The runner is told to update before anything else. It can: its tools are its image's own files, not a mount.
    let said = "slot 0: GitHub told this runner to update itself, because the pool's pinned runner is older than GitHub's newest. It updates inside its own container and then takes its job";
    wait_for(&pool, "the update said", || pool.output().contains(said));
    wait_for(&pool, "the updated runner idle", || {
        github.live().iter().any(|runner| runner.updated)
    });
    github.queue(&["arca"], "after the update", 0.2);
    wait_for(&pool, "the job", || pool.jobs().len() == 1);
    assert_eq!(pool.jobs()[0][4], "after the update");
    assert_eq!(pool.jobs()[0][5], "Succeeded");
    // The next fresh runner updates too, and each is said once and counted.
    wait_for(&pool, "the second runner updated", || {
        github
            .state()
            .runners
            .iter()
            .filter(|runner| runner.updated)
            .count()
            == 2
    });
    wait_for(&pool, "the updates in the totals", || {
        totals(&pool).contains("runner_updates=2")
    });
    assert_eq!(pool.output().matches(said).count(), 2);
    assert!(totals(&pool).contains("failures=0"), "{}", totals(&pool));
    // The runner and its daemon shared one copy of the runner's files, so the daemon saw the updated tools.
    let calls = pool.read("docker/calls.log");
    assert!(calls.contains("/slots/0/layer/home:/home/runner:ro "));
    assert!(calls.contains("/slots/0/layer/home:/home/runner "));
    // A runner that finished its update and then went away is not said to have died while updating.
    let updated = github
        .live()
        .into_iter()
        .find(|runner| runner.updated)
        .expect("the idle, updated runner");
    wait_for(&pool, "the update seen as finished", || {
        pool.read("run/slots/0").starts_with("idle")
    });
    github
        .state()
        .runners
        .iter_mut()
        .find(|runner| runner.id == updated.id)
        .unwrap()
        .deleted = true;
    wait_for(&pool, "the runner that went away after its update", || {
        pool.output().contains("slot 0 (small) failed: its runner stopped (exit 1) after it had updated itself, before it took a job")
    });
    assert!(!pool.output().contains("while it was updating itself"));
    // An updated runner that went away is no sign that layering failed.
    assert!(!pool.output().contains("with its files layered"));
    assert_eq!(pool.stop(), Some(0));
    drop(pool);

    // An update that fails ends the runner before a job: a failure of the pool, said as what it is.
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().update_required = true;
    let mut pool = one_slot_pool(&github, |_| {});
    pool.env.push(("FAKE_UPDATE_FAILS", "1".to_string()));
    pool.start(&github);
    wait_for(&pool, "the failed update said", || {
        pool.output().contains("slot 0 (small) failed: its runner stopped (exit 1) while it was updating itself, before it took a job")
    });
    assert_eq!(pool.jobs().len(), 0);
    // The update replaces the runner's own files, and this runner had them layered: the next runners start with
    // their image's own files, said once, as after a mount that failed.
    wait_for(&pool, "the pool back to plain files", || {
        pool.output().contains("A runner stopped while it was updating itself with its files layered. Runners start with their image's own files from now on")
    });
    wait_for(&pool, "a runner without the layer", || {
        pool.read("docker/calls.log")
            .lines()
            .filter(|line| line.starts_with("run ") && line.contains(RUNNER_IMAGE))
            .next_back()
            .is_some_and(|run| !run.contains(":/home/runner "))
    });
    assert_eq!(pool.output().matches("with its files layered").count(), 1);
    pool.stop();
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn an_idle_runner_older_than_its_token_is_still_taken_off_github_when_the_pool_stops() {
    let scratch = scratch_dir();
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    let key = github.app_key();
    // Installation tokens that GitHub takes for two seconds, and a controller that uses one for one.
    github.state().token_life = Some(Duration::from_secs(2));
    let mut pool = Pool::new(
        &settings(json!([small(2)]), 2, &format!("https://github.com/{ORG}")),
        64 * GIB,
        60 * GIB,
    );
    pool.env.push(("POOL_TOKEN_TTL", "1".to_string()));
    pool.secret("GITHUB_APP_CLIENT_ID", CLIENT_ID);
    pool.secret("GITHUB_APP_PRIVATE_KEY", &key);
    pool.start(&github);
    wait_for(&pool, "two idle runners", || github.live().len() == 2);
    let idle: Vec<u64> = github.live().iter().map(|runner| runner.id).collect();
    let minted = github.state().minted.len();
    // The runners wait longer than the token they were registered with lasts.
    std::thread::sleep(Duration::from_millis(2600));
    assert_eq!(pool.stop(), Some(0));
    let state = github.state();
    for id in idle {
        assert!(
            state
                .runners
                .iter()
                .any(|runner| runner.id == id && runner.deleted),
            "runner {id} was left on GitHub"
        );
    }
    assert!(
        state.minted.len() > minted,
        "a fresh token was made for the removal"
    );
    // No request was refused for an old token.
    assert!(pool.output().ends_with("arca-runner-pool: stopped\n"));
    drop(state);
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn the_runner_group_is_read_again_and_one_that_becomes_refused_holds_new_slots() {
    let scratch = scratch_dir();
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    // An answer that does not say whether the group is the default one is no ground to start on.
    github.state().group_without_default = true;
    let mut pool = one_slot_pool(&github, |_| {});
    pool.start(&github);
    wait_for(&pool, "the answer without a default field refused", || {
        pool.output().contains("GitHub's answer for runner group 7 does not name that group or does not say whether it is the default one")
    });
    assert!(github.registrations().is_empty());
    // With a whole answer the pool starts.
    github.state().group_without_default = false;
    wait_for(&pool, "a runner once the answer is whole", || {
        github.live().len() == 1
    });

    // Later the group is opened to public repositories on GitHub: the next status says so.
    github.state().groups[1].public = true;
    wait_for(&pool, "the warning after the group changed", || {
        pool.read("logs/status.txt").contains("runner group 7 \"arca pool\", selected repositories only, WARNING: public repositories admitted, so a pull request from anyone, once it is approved to run, runs code in this pool")
    });
    // Later still it is made the organisation's default group: no new slot starts, and the pool says why.
    github.state().groups[1].default = true;
    wait_for(&pool, "the refusal after the group changed", || {
        pool.output().contains(
            "the pool does not start on the organisation's default runner group (7 \"arca pool\")",
        )
    });
    let registered = github.registrations().len();
    // The runner that waits is left; its job runs, and no runner takes its place while the group is refused.
    github.queue(&["arca"], "the last job", 0.2);
    wait_for(&pool, "the job of the runner that was waiting", || {
        pool.jobs().len() == 1
    });
    wait_for(&pool, "the slot held", || {
        let status = pool.read("logs/status.txt");
        status.contains("queued (held: its runner group is refused now (see the pool line))")
            && status.contains(
                "REFUSED: runner group 7 \"arca pool\" is the organisation's default group",
            )
    });
    assert_eq!(github.registrations().len(), registered);
    // Put right on GitHub, the pool goes on by itself.
    github.state().groups[1].default = false;
    wait_for(&pool, "a new runner once the group is put right", || {
        github.registrations().len() > registered
    });
    assert_eq!(pool.stop(), Some(0));
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn a_pool_for_a_public_repository_says_so_in_every_status() {
    let scratch = scratch_dir();
    let github = Github::start(&scratch, "/repos/example-org/example-repo");
    github.state().repo_public = true;
    github.state().tokens.push(PAT.to_string());
    let mut pool = Pool::new(
        &settings(
            json!([small(1)]),
            1,
            "https://github.com/example-org/example-repo",
        ),
        64 * GIB,
        60 * GIB,
    );
    pool.secret("GITHUB_TOKEN", PAT);
    pool.start(&github);
    let warning = "one repository, WARNING: it is public, so a pull request from anyone, once it is approved to run, runs code in this pool";
    wait_for(&pool, "the warning at the start", || {
        pool.output()
            .contains(&format!("registering runners in {warning}"))
    });
    wait_for(&pool, "the warning in the status", || {
        pool.read("logs/status.txt").contains(warning)
    });
    // A repository's runners are registered with GitHub's fixed group number, whatever the settings say.
    wait_for(&pool, "a runner", || github.live().len() == 1);
    let asked: Value = serde_json::from_str(&github.registrations()[0].body).expect("the request");
    assert_eq!(asked["runner_group_id"], 1);
    // Made private on GitHub, the next read says so.
    github.state().repo_public = false;
    wait_for(&pool, "the status after it is made private", || {
        pool.read("logs/status.txt")
            .contains("one private repository")
    });
    assert_eq!(pool.stop(), Some(0));
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn a_machine_that_cannot_layer_starts_runners_with_their_images_own_files() {
    let scratch = scratch_dir();
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    github.state().update_required = true;
    let mut pool = one_slot_pool(&github, |_| {});
    fs::write(pool.dir.join("no-overlay"), "").expect("the switch");
    pool.start(&github);
    wait_for(&pool, "the fallback said", || {
        pool.output().contains("The runner's files could not be layered on this machine (overlay mount). Runners start with their image's own files from now on; after a runner updates itself, a job that runs in a container (container:) still gets the pinned release's bundled tools")
    });
    // The runner still updates itself (its tools are its image's own files, not a mount) and takes its job.
    wait_for(&pool, "the updated runner idle", || {
        github.live().iter().any(|runner| runner.updated)
    });
    github.queue(&["arca"], "a job without layering", 0.2);
    wait_for(&pool, "the job", || pool.jobs().len() == 1);
    wait_for(&pool, "a second runner", || {
        github.state().runners.len() >= 2
    });
    assert_eq!(pool.stop(), Some(0));
    // The mount was tried once and said once; every runner had no home mount and every daemon the pinned tools.
    assert_eq!(pool.read("mount.log").lines().count(), 1);
    assert_eq!(pool.output().matches("could not be layered").count(), 1);
    let calls = pool.read("docker/calls.log");
    for run in calls.lines().filter(|line| line.starts_with("run ")) {
        let daemon = run.contains(DOCKER_IMAGE);
        assert!(!run.contains(":/home/runner "), "{run}");
        assert!(!run.contains(":/home/runner:ro"), "{run}");
        assert_eq!(
            run.contains("/externals:/home/runner/externals:ro"),
            daemon,
            "{run}"
        );
    }
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn a_running_job_is_shown_as_busy_and_is_timed_from_when_it_began() {
    let scratch = scratch_dir();
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    let mut pool = one_slot_pool(&github, |_| {});
    pool.start(&github);
    wait_for(&pool, "an idle runner", || github.live().len() == 1);
    // The runner waits a while before a job comes: that wait is no part of the job.
    std::thread::sleep(Duration::from_millis(3200));
    github.queue(&["arca"], "a long test", 2.0);
    // While the job runs, the slot is busy and names it, and the totals count it.
    wait_for(&pool, "the slot shown as busy", || {
        let state = pool.read("run/slots/0");
        state.starts_with("busy\t") && state.contains("job \"a long test\"")
    });
    wait_for(&pool, "the busy slot in the totals", || {
        totals(&pool).contains("busy=1 idle=0")
    });
    assert!(
        pool.read("logs/status.txt").contains("busy")
            && pool.read("logs/status.txt").contains("job \"a long test\""),
        "{}",
        pool.read("logs/status.txt")
    );
    wait_for(&pool, "the job", || pool.jobs().len() == 1);
    // Timed from the runner's own "Running job" line: about two seconds, not the five since the runner started.
    let seconds: u64 = pool.jobs()[0][6].parse().expect("seconds");
    assert!((1..=3).contains(&seconds), "{seconds}");
    // A job whose name holds a time and the runner's own words cannot set when it began: only the time the line
    // starts with is read. The name is kept as the runner printed it.
    let sneaky = "sneaky 2001-01-01 00:00:00Z: Running job: other";
    wait_for(&pool, "a fresh runner", || github.live().len() == 1);
    github.queue(&["arca"], sneaky, 1.0);
    wait_for(&pool, "the second job", || pool.jobs().len() == 2);
    let second = &pool.jobs()[1];
    assert_eq!(second[4], sneaky);
    let seconds: u64 = second[6].parse().expect("seconds");
    assert!(seconds <= 3, "{seconds}");
    assert_eq!(pool.stop(), Some(0));
    // The pool never asks Docker for a runner's processes: it reads the runner's own lines.
    assert!(
        !pool
            .read("docker/calls.log")
            .lines()
            .any(|call| call.starts_with("top "))
    );
    let _ = fs::remove_dir_all(scratch);
}
