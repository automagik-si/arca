//! The runner pool's controller (`templates/github-runner-pool.sh run`) against a fake GitHub API and a fake `docker`.
//!
//! The controller is the shipped script, unchanged: only what it reaches outside itself is replaced. GitHub is a small
//! HTTP server in this file that checks every credential it is given. `docker` and `systemctl` are scripts on the
//! controller's `PATH` (`tests/runner_pool/fake-docker.sh` plays a runner: it takes one job with the just-in-time
//! configuration it was started with, and exits). `curl` is the real one behind a wrapper that records its arguments.
//! Paths, pauses and the machine's memory come from the environment.
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
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

fn run_ok(command: &mut Command) {
    let output = command.output().expect("the command runs");
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    let verified = Command::new("openssl")
        .args(["dgst", "-sha256", "-verify"])
        .arg(&public)
        .arg("-signature")
        .arg(&signature_file)
        .arg(&signed)
        .output()
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

fn which(tool: &str) -> PathBuf {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .expect("sh");
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success() && !path.is_empty(),
        "this test needs bash, curl, jq, openssl and flock on this machine; {tool} is missing"
    );
    PathBuf::from(path)
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
        // The real curl, with every argument recorded: a credential on a command line would be seen here.
        executable(
            &pool.dir.join("bin/curl"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexec '{}' \"$@\"\n",
                pool.dir.join("curl.log").display(),
                curl.display()
            ),
        );
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
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
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
        self.child = Some(child);
    }

    /// Stops the controller as systemd does, with SIGTERM, and returns its exit code.
    fn stop(&mut self) -> Option<i32> {
        let mut child = self.child.take()?;
        run_ok(
            Command::new("kill")
                .arg("-TERM")
                .arg(child.id().to_string()),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().expect("the controller's status") {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "the controller did not stop:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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
                if path.ends_with("secrets") || path.ends_with("run") {
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
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", child.id()))
                .output();
            let _ = child.wait();
        }
        // A played runner ends when its container's directory is gone.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn wait_for(pool: &Pool, what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
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
        // The runner's bundled tools are mounted into the daemon only: the runner keeps its image's own, which it
        // can replace when GitHub tells it to update itself.
        assert_eq!(run.contains(":/home/runner/externals"), daemon, "{run}");
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

    // Stopping: the idle runners are taken off GitHub and nothing is left running.
    let idle: Vec<u64> = github.live().iter().map(|runner| runner.id).collect();
    assert_eq!(pool.stop(), Some(0));
    assert!(pool.output().ends_with("arca-runner-pool: stopped\n"));
    assert_eq!(pool.containers(), Vec::<String>::new());
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
    assert_eq!(github.state().live_max, 3);
    let mut slots: Vec<String> = pool.jobs().iter().map(|job| job[1].clone()).collect();
    slots.sort();
    slots.dedup();
    assert_eq!(
        slots,
        ["0", "1", "2", "3"],
        "every slot took its turn under the cap"
    );
    // The job file is bounded: past its limit it is set aside once, with its header written again, and the summary
    // still counts the jobs in both.
    wait_for(&pool, "the summary over both job files", || {
        pool.read("logs/status.txt")
            .contains("class small last 8 jobs:")
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
        assert!(
            pool.output()
                .contains("the pool does not start until its settings are right")
        );
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
    // and the reserve leave 4GiB of its 32GiB, since a virtual machine sees a little less memory than its size.
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
        arca::service::size_bytes("28GiB").expect("bytes")
    );
    // The same settings start the controller: it reads them, makes its slices and waits for a credential.
    let scratch = std::env::temp_dir().join(format!(
        "arca-runner-pool-gh-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&scratch).expect("scratch");
    let github = Github::start(&scratch, &format!("/orgs/{ORG}"));
    // What such a machine reports: a little less than 32GiB.
    let mut pool = Pool::new(settings, 32 * GIB - 700 * (1 << 20), 30 * GIB);
    pool.start(&github);
    wait_for(&pool, "the shipped settings load", || {
        pool.output().contains("pool pool1 id abcd1234 for https://github.com/example-org: 5 slots in 2 classes, at most 5 at once, 27972 MiB of memory for slots")
    });
    wait_for(&pool, "the wait for a credential", || {
        pool.output().contains("waiting for a credential")
    });
    assert!(
        pool.read("units/arcapool-s4.slice")
            .contains("MemoryMax=8589934592\nMemorySwapMax=0\nCPUQuota=800%")
    );
    assert_eq!(pool.stop(), Some(0));
    let _ = fs::remove_dir_all(scratch);
}

fn scratch_dir() -> PathBuf {
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
        pool.output().contains("registering runners in runner group 8 \"wide\", WARNING: open to every repository of the organisation (all), WARNING: public repositories admitted, so a pull request from anyone can run code in this pool")
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
