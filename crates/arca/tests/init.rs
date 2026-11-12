//! `arca init`: a node goes from watch-only to able to change things, and back, against the real `arca serve` in its own
//! process over a fake Incus. Every file is in the test's own scratch directory; nothing here asks the machine's Incus,
//! its systemd or its desktop.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_incus::{FakeIncus, FakeState, IncusRead as _};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

/// A fresh directory under the default temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-init-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        Scratch(dir)
    }

    fn at(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
}

/// The binary, with no home directory and no variable to take a config or a token from.
fn arca() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arca"));
    command
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE")
        // No screen, unless a test says there is one: what `init` and `status` say next depends on it, and must not
        // depend on where the tests run.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    command
}

/// A token of `scope`, minted by the binary into `hash_dir`.
fn mint(scratch: &Scratch, hash_dir: &Path, scope: &str) -> String {
    let secret_file = scratch.at(&format!("{scope}.secret"));
    let out = arca()
        .args(["token", "mint", "--scope", scope, "--id"])
        .arg(format!("{scope}-1"))
        .arg("--hash-dir")
        .arg(hash_dir)
        .arg("--out")
        .arg(&secret_file)
        .output()
        .expect("mint a token");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(&secret_file)
        .expect("the secret")
        .trim_end()
        .to_string()
}

/// What a test's node is started through: a shell that closes every file it was handed but its three standard ones,
/// then becomes the program. A test process may hold files that are not its own to pass on, such as the descriptor of a
/// lock its runner took; a node that inherited one would keep it for as long as it lived.
const CLOSE_THEN_EXEC: &str = r#"for fd in /proc/$$/fd/*; do fd=${fd##*/}; case $fd in 0|1|2) ;; *) eval "exec $fd>&-" 2>/dev/null ;; esac; done; exec "$0" "$@""#;

/// `arca serve --config <config>` as a command to spawn, with no file of the test's process but its standard three.
fn serve_command(config: &Path) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(CLOSE_THEN_EXEC)
        .arg(env!("CARGO_BIN_EXE_arca"))
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE")
        .args(["serve", "--config"])
        .arg(config);
    command
}

/// A server process with its stderr in a file, killed if the test ends first.
struct Serve {
    child: Option<Child>,
    log: PathBuf,
}

impl Serve {
    fn start(config: &Path, log: PathBuf) -> Serve {
        let child = serve_command(config)
            .stdout(Stdio::null())
            .stderr(File::create(&log).expect("the server log"))
            .spawn()
            .expect("start arca serve");
        Serve {
            child: Some(child),
            log,
        }
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_for(&mut self, socket: &Path) {
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(socket).is_err() {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("running")
                .try_wait()
                .expect("poll")
            {
                panic!(
                    "the server exited before listening ({status}): {}",
                    self.log_text()
                );
            }
            assert!(Instant::now() < deadline, "the server listens in time");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn terminate(&mut self) -> Option<i32> {
        let mut child = self.child.take()?;
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = child.try_wait().expect("poll") {
                return status.code();
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One `tools/call` over the unix socket. Returns the JSON-RPC reply.
fn mcp(socket: &Path, secret: &str, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
    .to_string();
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("a deadline");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nAuthorization: Bearer {secret}\r\nMCP-Protocol-Version: 2025-11-25\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    let text = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    // A call the door refuses outright has no JSON-RPC answer: its HTTP status says so.
    if text.is_empty() {
        let status = reply
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("{name} answers HTTP: {reply}"));
        return json!({ "http": status });
    }
    serde_json::from_str(text).unwrap_or_else(|_| panic!("{name} answers JSON-RPC: {reply}"))
}

/// The structured result of a call that must succeed.
fn result(reply: &Value, what: &str) -> Value {
    assert!(reply.get("error").is_none(), "{what} succeeds: {reply}");
    assert_ne!(reply["result"]["isError"], true, "{what} succeeds: {reply}");
    reply["result"]["structuredContent"].clone()
}

fn instance_exists(fake: &FakeIncus, name: &str) -> bool {
    arca_incus::UnixIncus::new(fake.socket().to_path_buf())
        .get(&format!(
            "/1.0/instances/{name}?project={}",
            arca_netproject::PROJECT
        ))
        .is_ok()
}

/// A fake Incus as a machine that never had Arca: the default project and profile, and nothing else.
fn fresh_incus(scratch: &Scratch) -> FakeIncus {
    let fake = FakeIncus::start(
        &scratch.0,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("the fake Incus");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
                "devices": { "root": { "type": "disk", "pool": "default", "path": "/" } } }),
    );
    fake
}

/// The one template these tests create a service from.
fn nats_template(state: &Path) {
    let templates = state.join("templates");
    private_dir(&templates);
    fs::write(
        templates.join("nats.json"),
        json!({
            "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "ports": [4222], "env": {}, "publishable": false
        })
        .to_string(),
    )
    .expect("the template");
}

/// What going back to watch-only relies on: a node whose config no longer names a kernel project starts over the
/// desired state it wrote while it could, reports watch-only, keeps its services as they are and sends Incus no write.
#[test]
fn a_node_whose_kernel_projects_are_emptied_starts_as_watch_only_over_its_services_and_writes_nothing()
 {
    let scratch = Scratch::new("back");
    let state = scratch.at("state");
    private_dir(&state);
    let hash_dir = scratch.at("tokens");
    private_dir(&hash_dir);
    let doctor = mint(&scratch, &hash_dir, "doctor");
    let fake = fresh_incus(&scratch);
    nats_template(&state);
    let socket = scratch.at("arca.sock");
    let config = scratch.at("node.json");
    let write = |projects: Value| {
        fs::write(
            &config,
            json!({
                "stateDir": state.display().to_string(),
                "incus": { "socket": fake.socket().display().to_string(), "pools": [] },
                "listen": { "unix": socket.display().to_string() },
                "tokens": { "hashDir": hash_dir.display().to_string() },
                "kernel": {
                    "projects": projects, "droiddProjects": ["default"],
                    "serviceCidr": "192.0.2.0/24"
                }
            })
            .to_string(),
        )
        .expect("the config");
    };

    // A node that may change things, with `default` as the one project it names as not its own, runs a service.
    write(json!([arca_netproject::PROJECT]));
    let mut serve = Serve::start(&config, scratch.at("serve-1.log"));
    serve.wait_for(&socket);
    assert_eq!(
        result(
            &mcp(&socket, &doctor, "arca_status", json!({})),
            "arca_status"
        )["mode"],
        json!("kernel")
    );
    result(
        &mcp(
            &socket,
            &doctor,
            "service_create",
            json!({ "name": "nats", "template": "nats" }),
        ),
        "service_create",
    );
    assert!(instance_exists(&fake, "nats"), "nats runs");
    assert_eq!(serve.terminate(), Some(0));

    // The same config with no kernel project: the node starts, says watch-only, and the service is where it was.
    let writes = |fake: &FakeIncus| {
        fake.requests()
            .iter()
            .filter(|request| request.method != "GET")
            .count()
    };
    let before = writes(&fake);
    write(json!([]));
    let mut serve = Serve::start(&config, scratch.at("serve-2.log"));
    serve.wait_for(&socket);
    let status = result(
        &mcp(&socket, &doctor, "arca_status", json!({})),
        "arca_status",
    );
    assert_eq!(status["mode"], json!("watch-only"), "{status}");
    assert!(instance_exists(&fake, "nats"), "the service is still there");
    // A change is refused in words, and nothing reaches Incus for it.
    let refused = mcp(
        &socket,
        &doctor,
        "service_create",
        json!({ "name": "nats2", "template": "nats" }),
    );
    assert!(
        refused.get("error").is_some() || refused["result"]["isError"] == json!(true),
        "a watch-only node creates nothing: {refused}"
    );
    assert!(!instance_exists(&fake, "nats2"));
    // The watch reads on its own thread; give it a sweep, then count.
    let report = result(
        &mcp(&socket, &doctor, "watch_report", json!({})),
        "watch_report",
    );
    assert_eq!(serve.terminate(), Some(0), "{}", serve.log_text());
    assert_eq!(
        writes(&fake),
        before,
        "a watch-only node sent Incus a write; its log:\n{}\nits watch: {report}",
        serve.log_text()
    );
}

/// A fake Incus as a machine that never had Arca, with the bridge Incus itself makes on 10.47.0.0/24, which is the
/// first network init would try, and with `instances` in the default project.
fn machine(scratch: &Scratch, instances: Vec<Value>) -> FakeIncus {
    let fake = FakeIncus::start(
        &scratch.0,
        FakeState {
            server: json!({}),
            projects: vec![json!({ "name": "default", "description": "", "config": {} })],
            instances,
            networks: vec![json!({
                "name": "incusbr0", "project": "default", "type": "bridge",
                "config": { "ipv4.address": "10.47.0.1/24" }
            })],
            pools: vec![],
        },
    )
    .expect("the fake Incus");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
                "devices": { "root": { "type": "disk", "pool": "default", "path": "/" } } }),
    );
    // Incus lists that bridge among the machine's interfaces, with the address it holds.
    fake.set_host_interfaces(vec![(
        "incusbr0".to_string(),
        vec!["10.47.0.1".parse().expect("an address")],
    )]);
    fake
}

/// A node as the installer leaves it: watch-only, with the loopback door on a port that is free now.
struct Installed {
    scratch: Scratch,
    fake: FakeIncus,
    state: PathBuf,
    hash_dir: PathBuf,
    socket: PathBuf,
    config: PathBuf,
    port: u16,
}

impl Installed {
    fn new(tag: &str) -> Installed {
        Self::with(tag, Vec::new())
    }

    fn with(tag: &str, instances: Vec<Value>) -> Installed {
        let scratch = Scratch::new(tag);
        let state = scratch.at("state");
        private_dir(&state);
        let hash_dir = scratch.at("tokens");
        private_dir(&hash_dir);
        let fake = machine(&scratch, instances);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let node = Installed {
            socket: scratch.at("arca.sock"),
            config: scratch.at("node.json"),
            scratch,
            fake,
            state,
            hash_dir,
            port,
        };
        node.write(&node.installers());
        node
    }

    /// The installer's config for this node, as a value a test may bend.
    fn installers(&self) -> Value {
        json!({
            "stateDir": self.state.display().to_string(),
            "incus": { "socket": self.fake.socket().display().to_string() },
            "listen": {
                "unix": self.socket.display().to_string(),
                "loopback": format!("127.0.0.1:{}", self.port)
            },
            "tokens": { "hashDir": self.hash_dir.display().to_string() },
            "kernel": { "droiddProjects": [] }
        })
    }

    fn write(&self, config: &Value) {
        fs::write(&self.config, format!("{config:#}\n")).expect("the config");
        fs::set_permissions(&self.config, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }

    fn text(&self) -> String {
        fs::read_to_string(&self.config).expect("the config")
    }

    fn read(&self) -> Value {
        serde_json::from_str(&self.text()).expect("the config is JSON")
    }

    fn backup(&self) -> PathBuf {
        self.scratch.at("node.json.before-init")
    }

    /// `arca init <arguments> --config <this node's>`, with `answers` on its standard input.
    fn init(&self, arguments: &[&str], answers: &str) -> Said {
        let mut command = arca();
        self.run(command.arg("init").args(arguments), answers)
    }

    fn run(&self, command: &mut Command, answers: &str) -> Said {
        let mut child = command
            .arg("--config")
            .arg(&self.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start arca init");
        child
            .stdin
            .take()
            .expect("its standard input")
            .write_all(answers.as_bytes())
            .expect("answer");
        let out = child.wait_with_output().expect("arca init ends");
        Said {
            code: out.status.code(),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// What one run of init printed, and how it ended.
struct Said {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Said {
    fn all(&self) -> String {
        format!("{}\n{}", self.out, self.err)
    }

    fn ok(&self) -> &Said {
        assert_eq!(self.code, Some(0), "init failed:\n{}", self.all());
        self
    }

    fn says(&self, words: &str) -> &Said {
        assert!(
            self.all().contains(words),
            "init did not say {words:?}:\n{}",
            self.all()
        );
        self
    }

    /// The last `count` lines of what it printed: its closing report.
    fn report(&self, count: usize) -> Vec<&str> {
        let lines: Vec<&str> = self.out.lines().collect();
        lines[lines.len().saturating_sub(count)..].to_vec()
    }

    fn never_says(&self, words: &str) -> &Said {
        assert!(
            !self.all().contains(words),
            "init said {words:?}:\n{}",
            self.all()
        );
        self
    }
}

#[test]
fn init_takes_an_installed_node_to_one_that_changes_things_only_after_a_yes_and_back_again() {
    let node = Installed::new("there-and-back");
    let viewer = mint(&node.scratch, &node.hash_dir, "viewer");
    let before = node.text();

    // No answer, and an answer that is not yes, change nothing: the question's default is no.
    for answers in ["", "\n", "n\n", "maybe\n"] {
        let said = node.init(&["--enable-changes"], answers);
        assert_eq!(said.code, Some(2), "{answers:?}:\n{}", said.all());
        said.says("Let Arca make changes on this machine, each only after you approve it? [y/N]")
            .says("init changed nothing: the answer was not yes");
        assert_eq!(node.text(), before, "{answers:?}");
        assert!(!node.backup().exists(), "{answers:?}");
        assert!(!node.state.join("templates").exists(), "{answers:?}");
    }
    // Yes to the change, and no to the network it picked: nothing is written either.
    let said = node.init(&["--enable-changes"], "y\nn\n");
    assert_eq!(said.code, Some(2));
    said.says("is free. Use it? [Y/n]").says("--service-cidr");
    assert_eq!(node.text(), before);

    // Yes, and the network it picked: the four keys, and nothing else of the config, change.
    let said = node.init(&["--enable-changes"], "y\n\n");
    said.ok();
    let after = node.read();
    let cidr = after["kernel"]["serviceCidr"]
        .as_str()
        .expect("a service network")
        .to_string();
    let third: u32 = cidr
        .strip_prefix("10.47.")
        .and_then(|rest| rest.strip_suffix(".0/24"))
        .and_then(|third| third.parse().ok())
        .unwrap_or_else(|| panic!("{cidr} is one of init's own list"));
    assert_ne!(
        third, 0,
        "10.47.0.0/24 is Incus's own bridge on this machine"
    );
    let mut expected = node.installers();
    expected["kernel"] = json!({
        "projects": ["arcak-svc"], "droiddProjects": ["default"], "serviceCidr": cidr
    });
    expected["approvalOrigin"] = json!(format!("http://localhost:{}", node.port));
    assert_eq!(after, expected);
    assert_eq!(fs::read_to_string(node.backup()).expect("the copy"), before);
    for file in [&node.config, &node.backup()] {
        assert_eq!(
            fs::metadata(file).expect("metadata").permissions().mode() & 0o777,
            0o600,
            "{}",
            file.display()
        );
    }
    assert!(
        node.state.join("templates/nats.json").exists(),
        "the shipped templates"
    );

    // What it said: before the question, what was read to call the network free. After it, five lines: what
    // changed, who waits, ONE next step, where a database comes from, and one pointer to the rest.
    let config = node.config.display().to_string();
    said.says(&format!("kernel.serviceCidr: {cidr}"))
        .says("route(s) in /proc/net/route")
        .says("address(es) of this machine in /proc/net/fib_trie")
        .says("address(es) on the interfaces it lists")
        .says("nothing inside the tailnet range 100.64.0.0/10")
        // Nobody restarted the node, so the words do not claim it has taken the change up.
        .says("The config now lets Arca make changes on this machine. The node takes that up when it next starts.")
        .never_says("Arca may now make changes on this machine.")
        .never_says("import");
    assert_eq!(
        said.report(5),
        [
            "The config now lets Arca make changes on this machine. The node takes that up when it next starts.".to_string(),
            "No token of this node may do more than look and propose, so every change waits for your passkey.".to_string(),
            // The node was not restarted, so that is the one next step; the passkey comes after it.
            format!("Next: restart the node. This is not the installer's service, so init did not: stop the node and start it again with: arca serve --config {config}. Then `arca status --config {config}` says what comes next."),
            "A database: ask your assistant for the postgres-autopg service.".to_string(),
            format!("More: `arca status --config {config}` says what is open and what comes next; with --security, what Arca may touch and who can reach it. To go back to a node that only looks: arca init --watch-only --config {config}"),
        ],
        "{}",
        said.all()
    );
    // What is no longer on that screen: what Arca may touch, the page's cautions, the way from another computer,
    // the phone, where the old config is.
    for gone in [
        "It may touch",
        "the GPU devices",
        "every account is trusted",
        "ssh ",
        "phone",
        "before-init",
        "setup-code",
    ] {
        said.never_says(gone);
    }
    // Nothing was deleted: the one command the report names says each of them, from the config alone.
    let mut security = arca();
    let rest = node.run(security.args(["status", "--security"]), "");
    rest.ok()
        .says(&format!(
            "This node may change things. It may touch the Incus project arcak-svc and the bridge arcak-svcbr on {cidr}."
        ))
        .says("And, each only from a card you approve: snapshots")
        .says("and the GPU devices of its own instances. Nothing else.")
        .says("Arca never writes in \"default\"; that entry in kernel.droiddProjects only names it as not Arca's.")
        .says("A database comes from the postgres-autopg one.")
        .says("No token of this node may do more than look and propose, so every change waits for your passkey.")
        .says("It is for a machine where every account, and every agent that shares its network, is trusted")
        .says("check that `arca status` says the page is open")
        .says(&format!(
            "To open it from your own computer: ssh -o ExitOnForwardFailure=yes -L localhost:{0}:127.0.0.1:{0} <this machine>, then open http://localhost:{0}/approve in its browser.",
            node.port
        ))
        .says("If ssh warns that it could not bind the port, close it")
        .says("A phone cannot open this page. Approving from a phone needs the node reachable under a name, on a tailnet or a domain")
        .says(&format!(
            "arca init kept the config as it was before its last change at {}.",
            node.backup().display()
        ));

    // The node itself: it may change things, an assistant's token may only propose, and a proposal runs nothing.
    let mut serve = Serve::start(&node.config, node.scratch.at("serve-1.log"));
    serve.wait_for(&node.socket);
    assert_eq!(
        result(
            &mcp(&node.socket, &viewer, "arca_status", json!({})),
            "arca_status"
        )["mode"],
        json!("kernel")
    );
    let created = mcp(
        &node.socket,
        &viewer,
        "service_create",
        json!({ "name": "nats", "template": "nats" }),
    );
    assert_eq!(
        created,
        json!({ "http": 403 }),
        "an assistant's token creates nothing itself"
    );
    result(
        &mcp(
            &node.socket,
            &viewer,
            "action_propose",
            json!({ "action": "service_create", "args": { "name": "nats", "template": "nats" } }),
        ),
        "action_propose",
    );
    assert!(
        !instance_exists(&node.fake, "nats"),
        "a proposal runs nothing"
    );
    // A database, on the node init leaves. Init sets no image for it and none is needed: a proposed postgres_create
    // is the service the shipped template makes, with the image that template pins on the card. The same service
    // can be proposed by its template, and both wait like any change.
    let database = result(
        &mcp(
            &node.socket,
            &viewer,
            "action_propose",
            json!({ "action": "postgres_create", "args": { "name": "db0" } }),
        ),
        "action_propose",
    );
    assert_eq!(database["args"]["template"], "postgres-autopg");
    assert!(
        database["args"]["image"]
            .as_str()
            .is_some_and(|image| image.contains("@sha256:")),
        "the card's image is the template's, by digest: {database}"
    );
    assert!(
        database
            .to_string()
            .contains("with the password that template generates")
            && !database.to_string().contains("created stopped"),
        "the database's card: {database}"
    );
    assert!(
        !instance_exists(&node.fake, "db0"),
        "a proposal runs nothing"
    );
    let card = result(
        &mcp(
            &node.socket,
            &viewer,
            "action_propose",
            json!({ "action": "service_create", "args": { "name": "db1", "template": "postgres-autopg" } }),
        ),
        "action_propose",
    );
    // The card does not say "created stopped": the template init installed makes the database's password itself, so
    // the approved create starts, and nobody needs a token that sets secrets without a card.
    assert!(
        !card.to_string().contains("created stopped"),
        "the database's card: {card}"
    );
    assert!(
        !instance_exists(&node.fake, "db1"),
        "a proposal runs nothing"
    );
    assert_eq!(serve.terminate(), Some(0), "{}", serve.log_text());

    // A second run finds everything said already, asks nothing and writes nothing.
    let enabled = node.text();
    let again = node.init(&["--enable-changes"], "");
    again
        .ok()
        .says("it already says all of it: nothing is written")
        .never_says("[y/N]");
    assert_eq!(node.text(), enabled);

    // Back: the one key, and nothing else.
    let back = node.init(&["--watch-only"], "");
    back.ok()
        // Nobody restarted the node: the words must not say it only looks while it may still be changing things.
        .never_says("Arca now only looks on this machine")
        .never_says("service_list");
    assert_eq!(
        back.report(4),
        [
            "The config now says watch-only. The node that is running STILL MAKES CHANGES until it is restarted: it reads its config only when it starts.".to_string(),
            format!("Next: restart the node. This is not the installer's service, so init did not: stop the node and start it again with: arca serve --config {config}. Then `arca status --config {config}` says what comes next."),
            "Nothing was removed: the Incus project arcak-svc, its bridge, the services in it and your passkeys are as they were.".to_string(),
            format!("More: `arca status --config {config}`; with --security, what was kept and where. To let it make changes again: arca init --enable-changes --config {config}"),
        ],
        "{}",
        back.all()
    );
    // What left that report is where it points.
    let mut security = arca();
    node.run(security.args(["status", "--security"]), "")
        .ok()
        .says("This node only looks: it starts, stops, creates and removes nothing, and a proposal that waits cannot run.")
        .says("The reading tools still show what is there (service_list, service_status, service_logs and the others), and every answer says that the node only looks. Every tool that would change something is refused with that same sentence.")
        .says(&format!("{}.", node.backup().display()));
    let mut expected = serde_json::from_str::<Value>(&enabled).expect("JSON");
    expected["kernel"]["projects"] = json!([]);
    assert_eq!(node.read(), expected);
    assert_eq!(
        fs::read_to_string(node.backup()).expect("the copy"),
        enabled
    );
    let mut serve = Serve::start(&node.config, node.scratch.at("serve-2.log"));
    serve.wait_for(&node.socket);
    assert_eq!(
        result(
            &mcp(&node.socket, &viewer, "arca_status", json!({})),
            "arca_status"
        )["mode"],
        json!("watch-only")
    );
    // What `arca status --security` said of such a node is the node's own behaviour: a reading tool still answers,
    // and its answer carries the one sentence of a node that only looks.
    let listed = mcp(&node.socket, &viewer, "service_list", json!({}));
    result(&listed, "service_list");
    assert!(
        listed.to_string().contains(
            "this node only looks: nothing here can be changed until the node is running with changes turned on"
        ),
        "{listed}"
    );
    assert_eq!(serve.terminate(), Some(0));
    let watching = node.text();
    node.init(&["--watch-only"], "")
        .ok()
        .says("This node already only looks");
    assert_eq!(node.text(), watching);

    // And forward again: the network and the page it wrote before are kept, so only the project is added.
    node.init(&["--enable-changes", "--yes"], "").ok();
    assert_eq!(node.text(), enabled);
}

#[test]
fn init_names_every_token_that_may_change_things_without_the_page_before_it_asks() {
    let node = Installed::new("direct-tokens");
    mint(&node.scratch, &node.hash_dir, "viewer");
    mint(&node.scratch, &node.hash_dir, "doctor");
    let before = node.text();

    // Asked, and not answered: the tokens that do not wait for the page are named, and what they may do, before the
    // question; and the question no longer says "each only after you approve it", which would be false.
    let said = node.init(&["--enable-changes"], "");
    assert_eq!(said.code, Some(2), "{}", said.all());
    said.says("NOT every change waits for you. This node has 1 token(s) that may then change things directly,")
        .says("with no card and no passkey:")
        .says("    doctor-1 (doctor)")
        .says("Whoever holds one may create a service from a template, connect and disconnect services, set a")
        .says("service's secret, unpublish one, start an instance, link and unlink a database, create a database")
        .says("from an image the token names, remove a database's record, password and links, and use the Omni verbs.")
        .never_says("This node holds tenants")
        .says(&format!(
            "To take one away first: arca token revoke <id> --config {}",
            node.config.display()
        ))
        .says("Let Arca make changes on this machine: what is proposed after your approval, and directly for the tokens named above? [y/N]")
        .never_says("each only after you approve it")
        .never_says("every change waits for your passkey")
        .never_says("viewer-1");
    let all = said.all();
    assert!(
        said.out.contains("doctor-1 (doctor)") && said.err.contains("[y/N]"),
        "the tokens are in what it printed before it asked:\n{all}"
    );
    assert_eq!(node.text(), before);

    // --yes is not an answer for such a token: it stops, names the token again and changes nothing. This is what the
    // installer's --enable-changes relays where the owner has minted one.
    let said = node.init(&["--enable-changes", "--yes"], "");
    assert_eq!(said.code, Some(2), "{}", said.all());
    said.says("    doctor-1 (doctor)")
        .says("init changed nothing: --yes does not answer for the tokens named above")
        .says("Run it again without --yes and answer yourself, or take such tokens away first");
    assert_eq!(node.text(), before);
    assert!(!node.backup().exists());

    // After a yes the short report says it again, by name, on its second line: the passkey gates what is proposed,
    // not what such a token does. That line never leaves the first screen.
    let said = node.init(&["--enable-changes"], "y\n\n");
    said.ok().never_says("every change waits for your passkey");
    assert_eq!(
        said.report(5)[1],
        format!(
            "NOT every change waits for you: these tokens change things directly, with no card and no passkey: doctor-1 (doctor). To take one away: arca token revoke <id> --config {}",
            node.config.display()
        ),
        "{}",
        said.all()
    );
    // And the pointer's command says what such a token may do.
    let mut security = arca();
    node.run(security.args(["status", "--security"]), "")
        .ok()
        .says("NOT every change waits for you. This node has 1 token(s) that may change things directly,")
        .says("    doctor-1 (doctor)")
        .says("create a database");

    // With only tokens that look and propose, the plain sentence is true, and it is the one printed.
    let node = Installed::new("no-direct-tokens");
    mint(&node.scratch, &node.hash_dir, "viewer");
    node.init(&["--enable-changes"], "")
        .says("No token of this node may do more than look and propose, so every change waits for your passkey.")
        .says("Let Arca make changes on this machine, each only after you approve it? [y/N]")
        .never_says("NOT every change");
}

#[test]
fn a_token_directory_that_cannot_be_read_is_said_as_that_and_is_no_yes() {
    // The node's token directory is open to other users: the store refuses to read it, so init cannot say which
    // tokens there are. It says exactly that, asks without pointing at a list it did not print, and --yes stops.
    let node = Installed::new("tokens-unread");
    mint(&node.scratch, &node.hash_dir, "doctor");
    fs::set_permissions(&node.hash_dir, fs::Permissions::from_mode(0o777)).expect("chmod 0777");
    let before = node.text();
    let said = node.init(&["--enable-changes"], "");
    assert_eq!(said.code, Some(2), "{}", said.all());
    said.says(&format!(
        "The tokens in {} could not be read, so init cannot say whether one of them may change things",
        node.hash_dir.display()
    ))
    .says("directly, with no card and no passkey: a doctor or an operator token may.")
    .says("and directly for any doctor or operator token it has? [y/N]")
    .never_says("named above")
    .never_says("every change waits for your passkey")
    .never_says("doctor-1");
    let said = node.init(&["--enable-changes", "--yes"], "");
    assert_eq!(said.code, Some(2), "{}", said.all());
    said.says("init changed nothing: --yes does not answer for tokens it could not read");
    assert_eq!(node.text(), before);
    fs::set_permissions(&node.hash_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
}

#[test]
fn init_follows_no_link_and_overwrites_no_file_that_is_not_its_own() {
    // The config named through a link: init would replace the link and leave the file the node reads as it was.
    let node = Installed::new("links");
    let link = node.scratch.at("link.json");
    std::os::unix::fs::symlink(&node.config, &link).expect("a link");
    let before = node.text();
    let out = arca()
        .args(["init", "--enable-changes", "--yes", "--config"])
        .arg(&link)
        .output()
        .expect("run arca init");
    assert_eq!(out.status.code(), Some(2));
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains("is a link. Name the file it points to with --config"),
        "{said}"
    );
    assert_eq!(node.text(), before);
    assert!(
        fs::symlink_metadata(&link)
            .expect("the link")
            .file_type()
            .is_symlink()
    );

    // A link planted under the name of init's copy, pointing at a file of the owner's: init writes nothing through
    // it and changes nothing.
    let precious = node.scratch.at("precious");
    fs::write(&precious, "the owner's own file\n").expect("a file");
    std::os::unix::fs::symlink(&precious, node.backup()).expect("a planted link");
    let said = node.init(&["--enable-changes", "--yes"], "");
    assert_eq!(said.code, Some(2), "{}", said.all());
    said.says("is there and is not a file of init's (a link or a directory)");
    assert_eq!(
        fs::read_to_string(&precious).expect("the file"),
        "the owner's own file\n"
    );
    assert_eq!(node.text(), before);
    fs::remove_file(node.backup()).expect("remove the planted link");

    // A copy from an earlier run is a regular file of init's own: it is replaced, not appended to or followed.
    fs::write(node.backup(), "an older copy\n").expect("an earlier copy");
    node.init(&["--enable-changes", "--yes"], "").ok();
    assert_eq!(fs::read_to_string(node.backup()).expect("the copy"), before);
}

#[test]
fn init_changes_nothing_where_it_would_have_to_guess_open_a_door_or_overrule_the_owner() {
    let node = Installed::new("refusals");
    let refused = |what: &str, config: Value, arguments: &[&str], code: i32, words: &str| {
        node.write(&config);
        let before = node.text();
        let mut all = vec!["--enable-changes", "--yes"];
        all.extend_from_slice(arguments);
        let said = node.init(&all, "");
        assert_eq!(said.code, Some(code), "{what}:\n{}", said.all());
        said.says(words);
        assert_eq!(node.text(), before, "{what}: the config was changed");
        assert!(!node.backup().exists(), "{what}: a copy was written");
        assert!(
            !node.state.join("templates").exists(),
            "{what}: templates were written"
        );
    };
    let bent = |bend: &dyn Fn(&mut Value)| {
        let mut config = node.installers();
        bend(&mut config);
        config
    };

    // No door and no page: init opens no door, whether the key is missing or says null, the owner's no.
    for door in [None, Some(Value::Null)] {
        refused(
            "no loopback door",
            bent(&|config| match &door {
                None => {
                    config["listen"]
                        .as_object_mut()
                        .expect("listen")
                        .remove("loopback");
                }
                Some(null) => config["listen"]["loopback"] = null.clone(),
            }),
            &[],
            2,
            "this config has no approval page and no loopback door for one",
        );
    }
    refused(
        "a door the name localhost does not reach",
        bent(&|config| config["listen"]["loopback"] = json!(format!("127.0.0.2:{}", node.port))),
        &[],
        2,
        "the loopback door is not on 127.0.0.1 or [::1]",
    );
    // A node set up by hand for something else.
    refused(
        "projects of the owner's own",
        bent(&|config| {
            config["kernel"] = json!({ "projects": ["arcak-mine"], "droiddProjects": ["other"] })
        }),
        &[],
        2,
        "kernel.projects names projects and arcak-svc is not among them",
    );
    // A network that is not free, or is not one: the tailnet's range, Incus's own bridge, an address with host bits.
    refused(
        "a network inside the tailnet range",
        node.installers(),
        &["--service-cidr", "100.64.5.0/24"],
        2,
        "100.64.5.0/24 is not free on this machine: it is inside the tailnet range 100.64.0.0/10",
    );
    refused(
        "Incus's own network",
        node.installers(),
        &["--service-cidr", "10.47.0.0/24"],
        2,
        "10.47.0.0/24 is not free on this machine: 10.47.0.1 is an address of this machine",
    );
    // A network that is not a private one would pass every other check and then hide real addresses from the machine:
    // multicast, link-local, a public range, the unspecified one.
    for not_private in [
        "224.0.0.0/24",
        "169.254.7.0/24",
        "203.0.113.0/24",
        "0.0.0.0/24",
    ] {
        refused(
            "not a private network",
            node.installers(),
            &["--service-cidr", not_private],
            2,
            "is not a private network",
        );
    }
    for not_a_network in ["10.47.3.1/24", "10.47.0.0/16", "ten"] {
        refused(
            "not a /24 network",
            node.installers(),
            &["--service-cidr", not_a_network],
            2,
            "--service-cidr takes an IPv4 /24 network",
        );
    }
    // An Incus that does not answer.
    refused(
        "no Incus",
        bent(&|config| config["incus"]["socket"] = json!(node.scratch.at("no-such.socket"))),
        &[],
        1,
        "Incus does not answer on",
    );

    // The command line: one of the two forms, and the config by name.
    node.write(&node.installers());
    for arguments in [
        &[][..],
        &["--enable-changes", "--watch-only"][..],
        &["--watch-only", "--yes"][..],
    ] {
        let said = node.init(arguments, "");
        assert_eq!(said.code, Some(2), "{arguments:?}:\n{}", said.all());
    }
    let out = arca()
        .args(["init", "--enable-changes", "--yes"])
        .env("HOME", node.scratch.at("home"))
        .env("ARCA_CONFIG", &node.config)
        .output()
        .expect("run arca init");
    assert_eq!(out.status.code(), Some(2));
    // A variable that names another file than the default never picks the node for init: it would rewrite that node.
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "arca: ARCA_CONFIG names {config}, not the default config ({home}/.config/arca/node.json). This command hands out, removes or rewrites something on one node, so the variable alone does not choose that node, and nothing was done. To act on the node ARCA_CONFIG names, run the same command with: --config {config}\n",
            config = node.config.display(),
            home = node.scratch.at("home").display()
        )
    );
    assert_eq!(
        node.read(),
        node.installers(),
        "the variable alone did not choose the node, and nothing was written"
    );
}

#[test]
fn init_finds_its_config_as_every_command_does_and_says_which_before_it_writes() {
    let node = Installed::new("one-rule");
    // The node's config, where the installer writes it under this home. No unit is there, so nothing is restarted.
    let home = node.scratch.at("home");
    let installed = home.join(".config/arca");
    fs::create_dir_all(&installed).expect("the installer's directory");
    let default = installed.join("node.json");
    fs::copy(&node.config, &default).expect("the installer's config");
    let state = node.read()["stateDir"]
        .as_str()
        .expect("the state directory")
        .to_string();
    let run = |arguments: &[&str], variable: Option<&Path>| {
        let mut command = arca();
        command.env("HOME", &home).arg("init").args(arguments);
        if let Some(variable) = variable {
            command.env("ARCA_CONFIG", variable);
        }
        let out = command
            .stdin(Stdio::null())
            .output()
            .expect("run arca init");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    // No flag and no variable: the default config, named as the first line. Going back to watch-only cuts the node off
    // from what it changed, so it names the state directory too; enabling changes names the config.
    let said = run(&["--watch-only"], None);
    assert!(
        said.starts_with(&format!(
            "arca: using the config {} (the default); this node's state directory is {state}\n",
            default.display()
        )),
        "{said}"
    );
    let said = run(&["--enable-changes", "--yes"], None);
    assert!(
        said.starts_with(&format!(
            "arca: using the config {} (the default)\n",
            default.display()
        )),
        "{said}"
    );
    // The variable names the very file the default names: it chooses nothing, so init goes on and says so.
    let said = run(&["--watch-only"], Some(&default));
    assert!(
        said.starts_with(&format!(
            "arca: using the config {} (from ARCA_CONFIG); this node's state directory is {state}\n",
            default.display()
        )),
        "{said}"
    );
    // The config the flag names is not said back, and the file the test's node was made with was never touched.
    assert_eq!(node.read(), node.installers());
}

#[test]
fn init_does_not_run_where_another_manager_has_objects_and_names_what_it_saw() {
    let marked = json!({
        "name": "phone1", "project": "default", "type": "container", "status": "Running",
        "created_at": "2026-01-01T00:00:00Z", "profiles": ["default"],
        "config": { "volatile.uuid": "00000000-0000-4000-8000-0000000000b1", "user.droid.owner": "someone" },
        "devices": {}
    });
    let plain = json!({
        "name": "mine", "project": "default", "type": "container", "status": "Running",
        "created_at": "2026-01-01T00:00:00Z", "profiles": ["default"],
        "config": { "volatile.uuid": "00000000-0000-4000-8000-0000000000b2" },
        "devices": {}
    });
    let node = Installed::with("other-manager", vec![marked, plain.clone()]);
    let before = node.text();
    let said = node.init(&["--enable-changes", "--yes"], "");
    assert_eq!(said.code, Some(2), "{}", said.all());
    said.says("this machine holds objects with another manager's marks (default/phone1)")
        .says("List that manager's projects in kernel.droiddProjects")
        .never_says("default/mine");
    assert_eq!(node.text(), before);

    // An instance of the owner's own, with nobody's marks, is nobody else's: init goes on.
    let node = Installed::with("own-instances", vec![plain]);
    node.init(&["--enable-changes", "--yes"], "").ok();
    assert_eq!(node.read()["kernel"]["droiddProjects"], json!(["default"]));
}

#[test]
fn an_approval_page_the_config_already_names_is_kept_whichever_kind_it_is() {
    // An https page of the owner's own, with no loopback door at all: nothing about the page or the door is written.
    let node = Installed::new("page-https");
    let mut config = node.installers();
    config["listen"]
        .as_object_mut()
        .expect("listen")
        .remove("loopback");
    config["approvalOrigin"] = json!("https://arca.example.com");
    node.write(&config);
    let said = node.init(&["--enable-changes", "--yes"], "");
    said.ok()
        .says("https://arca.example.com/approve, the page this config already names: it is kept as it is")
        .never_says("localhost")
        .never_says("ssh ")
        .never_says("A phone cannot open this page")
        .never_says("That page is for a machine where every account is trusted");
    let after = node.read();
    assert_eq!(after["approvalOrigin"], json!("https://arca.example.com"));
    assert_eq!(after["listen"], config["listen"]);
    assert_eq!(after["kernel"]["projects"], json!(["arcak-svc"]));

    // A localhost page that is already there: kept too, and the keys the owner already wrote are not replaced.
    let node = Installed::new("page-local");
    let mut config = node.installers();
    config["approvalOrigin"] = json!(format!("http://localhost:{}", node.port));
    config["kernel"] = json!({ "droiddProjects": ["theirs"], "serviceCidr": "198.51.100.0/24" });
    node.write(&config);
    let said = node.init(&["--enable-changes", "--yes"], "");
    said.ok()
        .says("the page this config already names: it is kept as it is")
        .never_says("kernel.serviceCidr:")
        .never_says("kernel.droiddProjects:");
    let mut expected = config.clone();
    expected["kernel"]["projects"] = json!(["arcak-svc"]);
    assert_eq!(node.read(), expected);
}

#[test]
fn init_restarts_the_installers_own_service_for_this_config_and_no_other() {
    let node = Installed::new("restart");
    // A home as the installer leaves it, and a `systemctl` that only writes down what it was asked.
    let home = node.scratch.at("home");
    let units = home.join(".config/systemd/user");
    fs::create_dir_all(&units).expect("the unit directory");
    let bin = node.scratch.at("bin");
    private_dir(&bin);
    let asked = node.scratch.at("systemctl.log");
    fs::write(
        bin.join("systemctl"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
            asked.display()
        ),
    )
    .expect("the stand-in");
    fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o700)).expect("chmod");
    let unit = |config: &Path| {
        format!(
            "# Automagik Arca: written by scripts/install.sh. A second run rewrites this file.\n\n[Service]\nExecStart={} serve --config {}\n",
            home.join(".local/bin/arca").display(),
            config.display()
        )
    };
    let run = |arguments: &[&str]| {
        let mut command = arca();
        command
            .env("HOME", &home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .arg("init")
            .args(arguments);
        node.run(&mut command, "")
    };
    let calls = || fs::read_to_string(&asked).unwrap_or_default();

    // The installer's unit runs another config: this node is not its service, and nothing is restarted.
    fs::write(
        units.join("arca.service"),
        unit(&node.scratch.at("another.json")),
    )
    .expect("the unit");
    run(&["--enable-changes", "--yes"])
        .ok()
        .says("This is not the installer's service, so init did not");
    assert_eq!(calls(), "");

    // The installer's unit runs this config: it is restarted, once, and with no token there is nobody to ask.
    fs::write(units.join("arca.service"), unit(&node.config)).expect("the unit");
    run(&["--watch-only"])
        .ok()
        .says("The config now says watch-only, and the node was restarted. There is no token to ask it with, so take it that the node STILL MAKES CHANGES until")
        .never_says("Arca now only looks");
    assert_eq!(calls(), "--user restart arca.service\n");

    // A unit that is not the installer's, though it has the same name and runs this config, is the owner's.
    fs::write(
        units.join("arca.service"),
        unit(&node.config).replacen("# Automagik Arca: written by", "# written by", 1),
    )
    .expect("the unit");
    run(&["--enable-changes", "--yes"])
        .ok()
        .says("This is not the installer's service, so init did not");
    assert_eq!(
        calls(),
        "--user restart arca.service\n",
        "no second restart"
    );

    // The installer's own unit again, so the node is restarted; init has no token to ask it with, so nobody has seen
    // that the node holds its page. The next step is to ask the node, with or without a screen, and no setup code
    // is typed before that.
    fs::write(units.join("arca.service"), unit(&node.config)).expect("the unit");
    let config = node.config.display().to_string();
    let screen = |display: Option<&str>| {
        let mut command = arca();
        command
            .env("HOME", &home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .args(["init", "--enable-changes", "--yes"]);
        if let Some(display) = display {
            command.env("DISPLAY", display);
        }
        node.run(&mut command, "")
    };
    for display in [Some(":0"), None] {
        let said = screen(display);
        said.ok()
            .never_says("enrol your passkey")
            .never_says("ssh ");
        assert_eq!(
            said.report(5)[2],
            format!(
                "Next: ask the node whether its approval page is open, before you type a setup code or approve anything there: arca status --config {config} --token-file <the file of one of its tokens>. It then says what comes next."
            ),
            "{}",
            said.all()
        );
        assert_eq!(said.out.matches("Next: ").count(), 1, "{}", said.all());
    }
    assert_eq!(calls().lines().count(), 3, "one restart per run");
}

/// A shipped Postgres file exactly as release 0.12.0 shipped it, made from this release's file by undoing what
/// changed since. The caller checks its hash against `cmd_templates::EARLIER`, which proves it is that file byte for
/// byte.
fn postgres_file_of_0_12_0(name: &str) -> String {
    let now = arca::cmd_templates::SHIPPED
        .iter()
        .find(|(shipped, _)| *shipped == name)
        .map(|(_, text)| *text)
        .expect("a shipped file");
    let before = match name {
        "postgres-autopg.json" => now
            .replace(", which Arca makes at create and never shows", "")
            .replace("  \"generate\": { \"POSTGRES_PASSWORD\": \"\" },\n", ""),
        "postgres-autopg.sh" => now.replace(
            "is not set: this service was created from a template that did not make it (arca templates install, then create it again; or service_secret_set)",
            "is not set (service_secret_set)",
        ),
        other => panic!("{other} did not change since 0.12.0"),
    };
    let digest = arca_kernel::canon::Digest::of(before.as_bytes()).to_hex();
    assert!(
        arca::cmd_templates::EARLIER
            .iter()
            .any(|(earlier, hash)| *earlier == name && *hash == digest),
        "{name} as rebuilt here is not the file 0.12.0 shipped ({digest})"
    );
    before
}

/// A node whose templates were installed by 0.12.0: `arca init` brings the unedited ones up to this release and says
/// which, before its report, and the report is still its last five lines. A template the owner edited is kept and
/// not named.
#[test]
fn init_brings_templates_an_earlier_release_installed_up_to_date_and_says_which() {
    let node = Installed::new("old-templates");
    mint(&node.scratch, &node.hash_dir, "viewer");
    let templates = node.state.join("templates");
    fs::create_dir_all(&templates).expect("the templates directory");
    for name in ["postgres-autopg.json", "postgres-autopg.sh"] {
        fs::write(templates.join(name), postgres_file_of_0_12_0(name)).expect("the file of 0.12.0");
    }
    fs::write(templates.join("nats.json"), "{}").expect("the owner's edit");

    let said = node.init(&["--enable-changes", "--yes"], "");
    said.ok().says(
        "Templates brought up to this release (each was an earlier release's own file, not edited): postgres-autopg.json, postgres-autopg.sh.",
    );
    assert!(
        said.report(5)[0].starts_with("The config now lets Arca make changes on this machine."),
        "{}",
        said.all()
    );
    let installed =
        fs::read_to_string(templates.join("postgres-autopg.json")).expect("the template");
    assert!(
        installed.contains("\"generate\": { \"POSTGRES_PASSWORD\": \"\" }"),
        "{installed}"
    );
    assert_eq!(
        fs::read_to_string(templates.join("nats.json")).expect("the edit"),
        "{}",
        "an edited template is kept"
    );

    // A second run has nothing to bring up, and says nothing of templates.
    node.init(&["--enable-changes", "--yes"], "")
        .ok()
        .never_says("Templates brought up");
}

/// The one next step, from the real program on a node that `arca init` made and that says its page is open: with a
/// screen it is ONE line, the page and its button; with none, the code from this terminal, where to type it, the way
/// to the page from the person's own computer and what a phone needs.
#[test]
fn status_gives_one_next_step_for_the_first_passkey_and_it_depends_on_whether_there_is_a_screen() {
    let node = Installed::new("status-next");
    let viewer = mint(&node.scratch, &node.hash_dir, "viewer");
    node.init(&["--enable-changes", "--yes"], "").ok();
    let config = node.config.display().to_string();
    let page = format!("http://localhost:{}/approve", node.port);
    let mut serve = Serve::start(&node.config, node.scratch.at("serve.log"));
    serve.wait_for(&node.socket);
    // The node binds its page's two addresses after it starts: wait for its own word that the page is open.
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let answer = result(
            &mcp(&node.socket, &viewer, "arca_status", json!({})),
            "arca_status",
        );
        if answer["approvalPage"]["open"] == json!(true) {
            assert_eq!(answer["approvalPage"]["passkeys"], json!(0), "{answer}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the page did not open: {answer}\n{}",
            serve.log_text()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let status = |display: Option<&str>| {
        let mut command = arca();
        command
            .args(["status", "--config"])
            .arg(&node.config)
            .arg("--token-file")
            .arg(node.scratch.at("viewer.secret"));
        if let Some(display) = display {
            command.env("WAYLAND_DISPLAY", display);
        }
        let out = command.output().expect("run arca status");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let text = status(Some("wayland-0"));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[lines.len() - 2..],
        [
            format!(
                "Next: enrol your passkey: open {page} and press \"Get a setup code\". Until one is trusted, nothing can be approved."
            ),
            format!("More: arca status --security --config {config}"),
        ],
        "{text}"
    );
    assert!(
        !text.contains("ssh ") && !text.contains("phone") && !text.contains("setup-code"),
        "{text}"
    );

    let text = status(None);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[lines.len() - 4..],
        [
            format!("Next: enrol your passkey: this terminal has no screen, so print the setup code with: arca setup-code --config {config}, and type it on the page, {page}. Until one is trusted, nothing can be approved."),
            format!(
                "      To open the page from your own computer: ssh -o ExitOnForwardFailure=yes -L localhost:{0}:127.0.0.1:{0} <this machine>, then open {page} there.",
                node.port
            ),
            "      A phone cannot open this page: that needs the node reachable under a name, on a tailnet or a domain.".to_string(),
            format!("More: arca status --security --config {config}"),
        ],
        "{text}"
    );
    assert_eq!(text.matches("Next: ").count(), 1, "{text}");
    assert_eq!(serve.terminate(), Some(0), "{}", serve.log_text());
}

/// `arca status` on a node that may change things: a doctor token does not wait for the approval page, so status names
/// it, and does not print "Each one waits", which would be false.
#[test]
fn status_names_a_token_that_changes_things_without_the_page_and_does_not_say_each_one_waits() {
    let node = Installed::new("status-tokens");
    mint(&node.scratch, &node.hash_dir, "viewer");
    let mut config = node.installers();
    config["kernel"] = json!({
        "projects": ["arcak-svc"], "droiddProjects": ["default"], "serviceCidr": "192.0.2.0/24"
    });
    node.write(&config);
    let status = || {
        let out = arca()
            .args(["status", "--config"])
            .arg(&node.config)
            .arg("--token-file")
            .arg(node.scratch.at("viewer.secret"))
            .output()
            .expect("run arca status");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Only a viewer token: every change waits, and status says so with its reason.
    let mut serve = Serve::start(&node.config, node.scratch.at("serve-1.log"));
    serve.wait_for(&node.socket);
    let text = status();
    assert!(
        text.contains("Changes: on. Each one waits for the owner's approval with a passkey: no token of this node may do more than look and propose"),
        "{text}"
    );
    assert_eq!(serve.terminate(), Some(0));

    // A doctor token is minted: it is named, with the way to take it away, and the plain sentence is gone.
    mint(&node.scratch, &node.hash_dir, "doctor");
    let mut serve = Serve::start(&node.config, node.scratch.at("serve-2.log"));
    serve.wait_for(&node.socket);
    let text = status();
    assert!(
        text.contains("NOT every change does: these tokens change things directly, with no card and no passkey: doctor-1 (doctor)."),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "To take one away: arca token revoke <id> --config {}",
            node.config.display()
        )),
        "{text}"
    );
    assert!(!text.contains("Each one waits"), "{text}");
    assert!(!text.contains("viewer-1"), "{text}");
    assert_eq!(serve.terminate(), Some(0));
}
