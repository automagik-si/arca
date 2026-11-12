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
        .env_remove("ARCA_TOKEN_FILE");
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

/// A server process with its stderr in a file, killed if the test ends first.
struct Serve {
    child: Option<Child>,
    log: PathBuf,
}

impl Serve {
    fn start(config: &Path, log: PathBuf) -> Serve {
        let child = arca()
            .args(["serve", "--config"])
            .arg(config)
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

    // What it said: what was read to call the network free, what Arca may now touch, that the owner's passkey is
    // still the gate, that no passkey exists yet and how to get the first, and the one line that goes back.
    let config = node.config.display().to_string();
    said.says(&format!("kernel.serviceCidr: {cidr}"))
        .says("route(s) in /proc/net/route")
        .says("address(es) of this machine in /proc/net/fib_trie")
        .says("address(es) on the interfaces it lists")
        .says("nothing inside the tailnet range 100.64.0.0/10")
        // Nobody restarted the node, so the words do not claim it has taken the change up.
        .says("The config now lets Arca make changes on this machine. The node takes that up when it next starts.")
        .never_says("Arca may now make changes on this machine.")
        .says(&format!(
            "It may touch: the Incus project arcak-svc, and the bridge arcak-svcbr on {cidr}."
        ))
        .says("And, each only from a card you approve: snapshots")
        .says("and the GPU devices of its own instances. Nothing else.")
        .says("No token of this node may do more than look and propose, so every change waits for your passkey.")
        .says("Arca never writes in \"default\"; the entry in kernel.droiddProjects only names it as not Arca's.")
        .says("What an assistant proposes waits for you: nothing of it runs until you approve it with your")
        .says(&format!("passkey on the page: http://localhost:{}/approve", node.port))
        .says("No passkey is trusted yet, so nothing can be approved until you enrol the first one.")
        .says(&format!("arca setup-code --config {config}"))
        .says("That page is for a machine where every account is trusted.")
        .says("this is not the installer's service, so init did not")
        .says(&format!("arca serve --config {config}"))
        .says(&format!("arca init --watch-only --config {config}"))
        .never_says("import");

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
        .says("The config now says watch-only. The node that is running STILL MAKES CHANGES until it is restarted:")
        .never_says("Arca now only looks on this machine")
        .says("Nothing was removed.")
        .says("service_list answers \"no service project\"")
        .says(&format!("arca init --enable-changes --config {config}"));
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
        .says("service's secret, unpublish one, start an instance, link and unlink a database, and use the Omni verbs.")
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

    // After a yes the closing words say it again: the passkey gates what is proposed, not what such a token does.
    let said = node.init(&["--enable-changes"], "y\n\n");
    said.ok()
        .says("What an assistant proposes waits for you: nothing of it runs until you approve it with your")
        .says("  NOT every change waits for you. This node has 1 token(s) that may then change things directly,")
        .says("      doctor-1 (doctor)")
        .never_says("every change waits for your passkey");

    // With only tokens that look and propose, the plain sentence is true, and it is the one printed.
    let node = Installed::new("no-direct-tokens");
    mint(&node.scratch, &node.hash_dir, "viewer");
    node.init(&["--enable-changes"], "")
        .says("No token of this node may do more than look and propose, so every change waits for your passkey.")
        .says("Let Arca make changes on this machine, each only after you approve it? [y/N]")
        .never_says("NOT every change");
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
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "--config is required: init writes a node's config, so it reads no default config"
        ),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        node.read(),
        node.installers(),
        "neither a variable nor a default names the config"
    );
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
        .says("passkey on the page: https://arca.example.com/approve")
        .never_says("localhost")
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
        .says("this is not the installer's service, so init did not");
    assert_eq!(calls(), "");

    // The installer's unit runs this config: it is restarted, once, and with no token there is nobody to ask.
    fs::write(units.join("arca.service"), unit(&node.config)).expect("the unit");
    run(&["--watch-only"])
        .ok()
        .says("The node was restarted. There is no token to ask it with");
    assert_eq!(calls(), "--user restart arca.service\n");

    // A unit that is not the installer's, though it has the same name and runs this config, is the owner's.
    fs::write(
        units.join("arca.service"),
        unit(&node.config).replacen("# Automagik Arca: written by", "# written by", 1),
    )
    .expect("the unit");
    run(&["--enable-changes", "--yes"])
        .ok()
        .says("this is not the installer's service, so init did not");
    assert_eq!(
        calls(),
        "--user restart arca.service\n",
        "no second restart"
    );
}
