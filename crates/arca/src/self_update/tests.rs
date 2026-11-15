//! The updater against a fake release server, programs that are shell scripts, and a fake service manager.

use super::*;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::sync::Mutex;

/// The runs share the process's children, so they go one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A scratch directory, removed again when the test ends, whatever modes its directories were left with.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        for entry in walk(&self.0) {
            let _ = fs::set_permissions(&entry, fs::Permissions::from_mode(0o755));
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![dir.to_path_buf()];
    let mut index = 0;
    while index < dirs.len() {
        let _ = fs::set_permissions(&dirs[index], fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(&dirs[index]) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    dirs.push(entry.path());
                }
            }
        }
        index += 1;
    }
    dirs
}

fn scratch(tag: &str) -> Scratch {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "arca-update-{tag}-{}-{}-{}",
        std::process::id(),
        now_ms(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("scratch");
    Scratch(dir)
}

/// The releases' test key, and a key nobody trusts.
fn key() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).expect("a key")
}

fn stranger() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).expect("a key")
}

fn public(pair: &Ed25519KeyPair) -> [u8; 32] {
    pair.public_key().as_ref().try_into().expect("32 bytes")
}

/// A program that says `version`, and on `templates install` leaves a mark beside itself.
fn program_text(version: &str) -> String {
    format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'Automagik Arca {version} (test, {arch}-unknown-linux-gnu)' ;;\n  templates) echo \"$*\" > \"$(dirname \"$0\")/templates-ran-{version}\"; echo '{{\"replaced\":[],\"kept\":[]}}' ;;\nesac\n",
        arch = std::env::consts::ARCH
    )
}

fn write_program_file(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("a directory")).expect("bin dir");
    fs::write(path, text).expect("program");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mode");
}

/// How a published release lies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lie {
    None,
    /// `SHA256SUMS` gives the binary a wrong checksum.
    Sums,
    /// `SHA256SUMS` gives `install.sh` a wrong checksum.
    Installer,
    /// The binary says another version than its release.
    Says,
    /// The release has no `SHA256SUMS.sig`.
    Unsigned,
    /// `SHA256SUMS.sig` is made with a key nobody trusts.
    Stranger,
}

/// A release server: the releases listing, each release by its tag, and the files they link to, over plain HTTP on
/// loopback.
struct Server {
    base: String,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    releases: Mutex<BTreeMap<String, Value>>,
}

const REPO: &str = "/repos/automagik-si/arca";

impl Server {
    fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
        let served = Arc::clone(&files);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let path = path.split('?').next().unwrap_or_default().to_string();
                let body = served.lock().expect("files").get(&path).cloned();
                let mut stream = stream;
                let _ = match body {
                    Some(body) => stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .and_then(|()| stream.write_all(&body)),
                    None => stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ),
                };
            }
        });
        Server {
            base,
            files,
            releases: Mutex::default(),
        }
    }

    fn source(&self) -> String {
        format!("{}{REPO}", self.base)
    }

    /// Publishes `version`, signed with the test key unless the lie says otherwise. The listing always holds a newer
    /// draft and a newer pre-release, which the stable channel never takes, and an old release with no files.
    fn publish(&self, version: &str, lie: Lie) {
        let asset = asset_name(version, std::env::consts::ARCH);
        let binary = program_text(if lie == Lie::Says { "9.9.9" } else { version }).into_bytes();
        let installer = b"#!/bin/sh\necho install\n".to_vec();
        let wrong = "0".repeat(64);
        let sums = format!(
            "{}  {asset}\n{}  install.sh\n",
            if lie == Lie::Sums {
                wrong.clone()
            } else {
                sha256_hex(&binary)
            },
            if lie == Lie::Installer {
                wrong
            } else {
                sha256_hex(&installer)
            },
        );
        let link = |name: &str| format!("{}/dl/{version}/{name}", self.base);
        let mut assets = vec![
            json!({ "name": asset, "browser_download_url": link(&asset) }),
            json!({ "name": "SHA256SUMS", "browser_download_url": link("SHA256SUMS") }),
            json!({ "name": "install.sh", "browser_download_url": link("install.sh") }),
        ];
        let mut files = self.files.lock().expect("files");
        if lie != Lie::Unsigned {
            let signer = if lie == Lie::Stranger {
                stranger()
            } else {
                key()
            };
            files.insert(
                format!("/dl/{version}/SHA256SUMS.sig"),
                signer.sign(sums.as_bytes()).as_ref().to_vec(),
            );
            assets.push(
                json!({ "name": "SHA256SUMS.sig", "browser_download_url": link("SHA256SUMS.sig") }),
            );
        }
        let release = json!({
            "tag_name": format!("v{version}"), "draft": false, "prerelease": false, "assets": assets,
        });
        files.insert(
            format!("{REPO}/releases/tags/v{version}"),
            release.to_string().into_bytes(),
        );
        files.insert(format!("/dl/{version}/{asset}"), binary);
        files.insert(format!("/dl/{version}/SHA256SUMS"), sums.into_bytes());
        files.insert(format!("/dl/{version}/install.sh"), installer);
        let mut releases = self.releases.lock().expect("releases");
        releases.insert(version.to_string(), release);
        Self::list(&mut files, &releases);
    }

    /// Takes a release away again.
    fn withdraw(&self, version: &str) {
        let mut files = self.files.lock().expect("files");
        let mut releases = self.releases.lock().expect("releases");
        releases.remove(version);
        files.remove(&format!("{REPO}/releases/tags/v{version}"));
        Self::list(&mut files, &releases);
    }

    fn list(files: &mut BTreeMap<String, Vec<u8>>, releases: &BTreeMap<String, Value>) {
        let mut listing = vec![
            json!({ "tag_name": "v99.0.0", "draft": true, "prerelease": false, "assets": [] }),
            json!({ "tag_name": "v98.0.0-rc.1", "draft": false, "prerelease": true, "assets": [] }),
        ];
        listing.extend(releases.values().cloned());
        listing.push(
            json!({ "tag_name": "v0.1.0", "draft": false, "prerelease": false, "assets": [] }),
        );
        files.insert(
            format!("{REPO}/releases"),
            Value::Array(listing).to_string().into_bytes(),
        );
    }
}

/// What goes wrong with a unit once it runs a version.
#[derive(Clone, PartialEq, Eq)]
enum Fault {
    /// It is not running after its start.
    Down,
    /// It runs, and is restarted by its manager after this many looks.
    DiesAfter(u32),
    /// A node that never answers on its socket.
    Silent,
    /// Its process says this version.
    Says(String),
}

/// One unit as the fake manager runs it.
#[derive(Clone)]
struct State {
    running: Running,
    version: Option<String>,
    looks: u32,
}

/// A service manager whose units run the programs their paths name, or the release staged for that program when its
/// role hands over to one, as [`hand_over`] does.
struct FakeUnits {
    units: Vec<Unit>,
    stage_root: Option<PathBuf>,
    state: Mutex<BTreeMap<String, State>>,
    fault: Option<(String, Fault)>,
    restarts: Mutex<Vec<String>>,
}

impl FakeUnits {
    fn new(units: Vec<Unit>, stage_root: Option<&Path>, fault: Option<(&str, Fault)>) -> FakeUnits {
        let fake = FakeUnits {
            units,
            stage_root: stage_root.map(Path::to_path_buf),
            state: Mutex::default(),
            fault: fault.map(|(version, fault)| (version.to_string(), fault)),
            restarts: Mutex::default(),
        };
        for unit in fake.units.iter().filter(|unit| unit.active) {
            fake.start(unit);
        }
        fake.restarts.lock().expect("restarts").clear();
        fake
    }

    fn fault_of(&self, version: Option<&str>) -> Option<&Fault> {
        self.fault
            .as_ref()
            .filter(|(at, _)| Some(at.as_str()) == version)
            .map(|(_, fault)| fault)
    }

    fn start(&self, unit: &Unit) {
        let mut version = program_version(&unit.program);
        let file = fs::canonicalize(&unit.program).unwrap_or_else(|_| unit.program.clone());
        if unit.role().hands_over()
            && let Some(root) = &self.stage_root
            && let Some(staged) = staged_program(&staging_dir(root, &file))
            && should_hand_over(version.as_deref().unwrap_or_default(), &staged.version)
        {
            version = Some(staged.version);
        }
        let up = version.is_some() && self.fault_of(version.as_deref()) != Some(&Fault::Down);
        let mut state = self.state.lock().expect("state");
        let pid = state
            .get(&unit.name)
            .map_or(100, |s| s.running.pid.max(100) + 1);
        state.insert(
            unit.name.clone(),
            State {
                running: Running {
                    active: up,
                    pid: if up { pid } else { 0 },
                    restarts: 0,
                },
                version,
                looks: 0,
            },
        );
        self.restarts
            .lock()
            .expect("restarts")
            .push(unit.name.clone());
    }

    fn restarted(&self) -> Vec<String> {
        self.restarts.lock().expect("restarts").clone()
    }
}

impl Units for FakeUnits {
    fn list(&self) -> Result<Vec<Unit>, String> {
        Ok(self.units.clone())
    }

    fn restart(&self, name: &str) -> Result<(), String> {
        let unit = self
            .units
            .iter()
            .find(|unit| unit.name == name)
            .ok_or("no such unit")?;
        self.start(unit);
        Ok(())
    }

    fn running(&self, name: &str) -> Result<Running, String> {
        let mut state = self.state.lock().expect("state");
        let unit = state.get_mut(name).ok_or("not loaded")?;
        unit.looks += 1;
        let dies = matches!(
            self.fault_of(unit.version.as_deref()),
            Some(Fault::DiesAfter(after)) if unit.looks > *after
        );
        if dies && unit.running.restarts == 0 {
            unit.running.pid += 1000;
            unit.running.restarts += 1;
        }
        Ok(unit.running)
    }

    fn running_version(&self, name: &str, _running: &Running) -> Option<String> {
        let state = self.state.lock().expect("state");
        let version = state.get(name)?.version.clone();
        match self.fault_of(version.as_deref()) {
            Some(Fault::Says(said)) => Some(said.clone()),
            _ => version,
        }
    }

    fn answers(&self, unit: &Unit) -> bool {
        let state = self.state.lock().expect("state");
        let version = state.get(&unit.name).and_then(|s| s.version.clone());
        state.get(&unit.name).is_some_and(|s| s.running.active)
            && self.fault_of(version.as_deref()) != Some(&Fault::Silent)
    }
}

/// A node: its config file and state directory.
fn node(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let state = root.join(format!("{name}-state"));
    fs::create_dir_all(&state).expect("state");
    let config = root.join(format!("{name}.json"));
    fs::write(
        &config,
        json!({
            "stateDir": state,
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": root.join(format!("{name}.sock")) },
            "tokens": { "hashDir": root.join("tokens") },
            "kernel": { "droiddProjects": [] }
        })
        .to_string(),
    )
    .expect("config");
    (config, state)
}

fn unit(name: &str, program: &Path, args: &[&str]) -> Unit {
    Unit {
        name: name.to_string(),
        program: program.to_path_buf(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        active: true,
    }
}

/// One run's outside: what the test changes from the defaults.
struct Opts {
    note: Option<PathBuf>,
    minute: u32,
    keys: Vec<[u8; 32]>,
    stage_root: Option<PathBuf>,
    window: Option<String>,
    install: bool,
}

impl Opts {
    fn new(root: &Path) -> Opts {
        Opts {
            note: None,
            minute: 0,
            keys: vec![public(&key())],
            stage_root: Some(root.join("stage")),
            window: None,
            install: true,
        }
    }
}

fn run_opts(
    server: &Server,
    config: &Path,
    units: &dyn Units,
    root: &Path,
    opts: &Opts,
) -> Outcome {
    let mut config = crate::config::load(config).expect("config loads");
    config.update.source = server.source();
    config.update.window = opts.window.clone();
    Run {
        config: &config,
        units,
        arch: std::env::consts::ARCH,
        keys: &opts.keys,
        health: Duration::from_millis(150),
        poll: Duration::from_millis(5),
        install_note: opts.note.clone(),
        lock: root.join("lock"),
        stage_root: opts.stage_root.clone(),
        minute_of_day: opts.minute,
    }
    .run(opts.install)
}

fn journal_steps(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join(UPDATE_DIR).join(JOURNAL_FILE))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("a journal line is JSON")["step"]
                .as_str()
                .expect("a step")
                .to_string()
        })
        .collect()
}

fn reason(outcome: &Outcome) -> &str {
    outcome.reason.as_deref().unwrap_or_default()
}

/// A machine like a hand-made install: a shim and two nodes on one program, a link on a program of its own, and a node
/// whose own program is newer than any release.
struct Machine {
    scratch: Scratch,
    shared: PathBuf,
    routes: PathBuf,
    dev: PathBuf,
    config: PathBuf,
    state: PathBuf,
    second_state: PathBuf,
    dev_state: PathBuf,
    units: Vec<Unit>,
}

impl Machine {
    fn root(&self) -> &Path {
        &self.scratch.0
    }

    fn opts(&self) -> Opts {
        Opts::new(self.root())
    }

    fn run(&self, server: &Server, units: &dyn Units) -> Outcome {
        run_opts(server, &self.config, units, self.root(), &self.opts())
    }

    fn fake(&self, fault: Option<(&str, Fault)>) -> FakeUnits {
        FakeUnits::new(self.units.clone(), Some(&self.root().join("stage")), fault)
    }
}

fn machine(tag: &str) -> Machine {
    let scratch = scratch(tag);
    let root = scratch.0.clone();
    let shared = root.join("bin/arca");
    let routes = root.join("lib/arca-routes/arca");
    let dev = root.join("dev/arca");
    write_program_file(&shared, &program_text("1.0.0"));
    write_program_file(&routes, &program_text("1.0.0"));
    write_program_file(&dev, &program_text("3.0.0"));
    let (config, state) = node(&root, "watch");
    let (second_config, second_state) = node(&root, "second");
    let (dev_config, dev_state) = node(&root, "dev");
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let units = vec![
        unit(
            "arca-watch.service",
            &shared,
            &["serve", "--config", &text(&config)],
        ),
        unit("arca-link.service", &routes, &["link", "serve"]),
        unit("arca-shim.service", &shared, &["shim", "--upstream", "/x"]),
        unit(
            "arca-second.service",
            &shared,
            &["serve", &format!("--config={}", text(&second_config))],
        ),
        unit(
            "arca-dev.service",
            &dev,
            &["serve", "--config", &text(&dev_config)],
        ),
    ];
    Machine {
        scratch,
        shared,
        routes,
        dev,
        config,
        state,
        second_state,
        dev_state,
        units,
    }
}

/// A server with 1.0.0 published, the release the machine's programs are.
fn server_on(version: &str) -> Server {
    let server = Server::start();
    server.publish("1.0.0", Lie::None);
    if version != "1.0.0" {
        server.publish(version, Lie::None);
    }
    server
}

#[test]
fn a_node_on_the_newest_release_changes_nothing_and_journals_the_look() {
    let _serial = serial();
    let server = server_on("1.0.0");
    let m = machine("current");
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "up-to-date", "{outcome:?}");
    assert_eq!(outcome.available.as_deref(), Some("1.0.0"));
    assert!(fake.restarted().is_empty(), "nothing restarts");
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert_eq!(journal_steps(&m.state), ["check"]);
    let state = read_state(&m.state).expect("a state");
    assert_eq!(state["result"], "up-to-date");
    assert_eq!(state["failed"], false);
    assert!(!alert_input(&m.state, 1).value.failed);
    let status = status_json(&UpdateConfig::default(), &m.state);
    assert_eq!(status["available"], "1.0.0");
    assert_eq!(status["lastResult"], "up-to-date");
    assert_eq!(status["current"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn a_new_release_replaces_every_program_behind_it_and_restarts_shims_then_nodes_then_links() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("installs");
    fs::create_dir_all(m.state.join("templates")).expect("templates");
    // The installer's note names the shared program by its old checksum.
    let note = m.root().join("program.sha256");
    let old_sha = sha256_file(&m.shared).expect("sha");
    fs::write(&note, format!("{old_sha}  {}\n", m.shared.display())).expect("note");
    let fake = m.fake(None);
    let opts = Opts {
        note: Some(note.clone()),
        ..m.opts()
    };

    let outcome = run_opts(&server, &m.config, &fake, m.root(), &opts);
    assert_eq!(outcome.result, "updated", "{outcome:?}");
    assert_eq!(program_version(&m.shared).as_deref(), Some("2.0.0"));
    assert_eq!(program_version(&m.routes).as_deref(), Some("2.0.0"));
    // The old programs are kept beside them.
    assert_eq!(
        program_version(&previous_of(&m.shared)).as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        program_version(&previous_of(&m.routes)).as_deref(),
        Some("1.0.0")
    );
    // A program newer than the release is never replaced, and its unit never restarts.
    assert_eq!(program_version(&m.dev).as_deref(), Some("3.0.0"));
    assert_eq!(
        fake.restarted(),
        [
            "arca-shim.service",
            "arca-second.service",
            "arca-watch.service",
            "arca-link.service"
        ]
    );
    // Each unit's version was read back.
    let units = outcome.detail["units"].as_array().expect("units");
    assert_eq!(units.len(), 4);
    assert!(units.iter().all(|unit| unit["to"] == "2.0.0"), "{units:?}");
    // The node's templates were brought up to the release by the new program.
    assert!(m.shared.with_file_name("templates-ran-2.0.0").exists());
    // The installer's note names the new program.
    let new_sha = sha256_file(&m.shared).expect("sha");
    assert_eq!(
        fs::read_to_string(&note).expect("note"),
        format!("{new_sha}  {}\n", m.shared.display())
    );
    assert_eq!(
        journal_steps(&m.state),
        [
            "download",
            "install",
            "install",
            "restart",
            "restart",
            "restart",
            "restart",
            "templates",
            "updated",
            "check"
        ]
    );
    // Every node on the machine records the run, the second node of the shared program too, and the pending record
    // is gone.
    for state in [&m.state, &m.second_state, &m.dev_state] {
        assert_eq!(read_state(state).expect("state")["result"], "updated");
        assert!(read_pending(state).is_none());
    }
    let state = read_state(&m.state).expect("state");
    assert_eq!(state["available"], "2.0.0");
    assert!(state["updatedAtMs"].as_u64().is_some());
    // Nothing is left beside the programs but the programs and what they kept.
    let mut names: Vec<String> = fs::read_dir(m.shared.parent().expect("dir"))
        .expect("dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["arca", "arca.previous", "templates-ran-2.0.0"]);

    // The next look finds every program on the release.
    let again = m.run(&server, &fake);
    assert_eq!(again.result, "up-to-date", "{again:?}");
}

#[test]
fn a_checksum_that_does_not_match_keeps_every_old_program_and_raises_the_alert() {
    let _serial = serial();
    let server = server_on("1.0.0");
    server.publish("2.0.0", Lie::Sums);
    let m = machine("checksum");
    let before = fs::read(&m.shared).expect("program");
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "failed", "{outcome:?}");
    assert!(
        reason(&outcome).contains("checksum of arca-2.0.0"),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read(&m.shared).expect("program"),
        before,
        "the program is the same file"
    );
    assert_eq!(program_version(&m.routes).as_deref(), Some("1.0.0"));
    assert!(!previous_of(&m.shared).exists(), "nothing was moved");
    assert!(fake.restarted().is_empty());
    assert!(
        alert_input(&m.state, 1).value.failed,
        "the alert reads a failure"
    );
    assert_eq!(journal_steps(&m.state), ["download", "failed", "check"]);
    // A later run that ends up to date clears it.
    server.withdraw("2.0.0");
    let fixed = m.run(&server, &fake);
    assert_eq!(fixed.result, "up-to-date", "{fixed:?}");
    assert!(!alert_input(&m.state, 1).value.failed);
}

#[test]
fn an_installer_whose_checksum_does_not_match_keeps_every_old_program() {
    let _serial = serial();
    let server = server_on("1.0.0");
    server.publish("2.0.0", Lie::Installer);
    let m = machine("installer");
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "failed", "{outcome:?}");
    assert!(
        reason(&outcome).contains("checksum of install.sh"),
        "{outcome:?}"
    );
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert!(fake.restarted().is_empty());
}

#[test]
fn a_binary_that_says_another_version_than_its_release_is_never_put_in_place() {
    let _serial = serial();
    let server = server_on("1.0.0");
    server.publish("2.0.0", Lie::Says);
    let m = machine("says");
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "failed", "{outcome:?}");
    assert!(
        reason(&outcome).contains("says it is 9.9.9, not 2.0.0"),
        "{outcome:?}"
    );
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert!(fake.restarted().is_empty());
    // The checked copy is removed again: only the program is in its directory.
    let names: Vec<_> = fs::read_dir(m.shared.parent().expect("dir"))
        .expect("dir")
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(names, ["arca"]);
    assert!(read_pending(&m.state).is_none());
}

#[test]
fn without_a_release_key_or_a_good_signature_nothing_is_installed() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("unsigned");
    let fake = m.fake(None);

    // This program holds no key: it says what is available and installs nothing, and that is no failure.
    let keyless = Opts {
        keys: Vec::new(),
        ..m.opts()
    };
    let outcome = run_opts(&server, &m.config, &fake, m.root(), &keyless);
    assert_eq!(outcome.result, "unsigned", "{outcome:?}");
    assert_eq!(outcome.available.as_deref(), Some("2.0.0"));
    assert!(!alert_input(&m.state, 1).value.failed);
    assert_eq!(journal_steps(&m.state), ["check"], "nothing was downloaded");
    let check = run_opts(
        &server,
        &m.config,
        &fake,
        m.root(),
        &Opts {
            install: false,
            keys: Vec::new(),
            ..m.opts()
        },
    );
    assert_eq!(check.result, "available", "{check:?}");

    // A release with no signature is not installed either.
    server.publish("2.0.1", Lie::Unsigned);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "unsigned", "{outcome:?}");
    assert!(reason(&outcome).contains("not signed"), "{outcome:?}");

    // A signature made with another key is a failure.
    server.publish("2.0.2", Lie::Stranger);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "failed", "{outcome:?}");
    assert!(reason(&outcome).contains("does not verify"), "{outcome:?}");
    assert!(alert_input(&m.state, 1).value.failed);

    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert!(fake.restarted().is_empty());
}

#[test]
fn signatures_verify_only_against_the_keys_given() {
    let sums = b"abc  arca\n";
    let signature = key().sign(sums);
    assert!(signed_by(&[public(&key())], sums, signature.as_ref()));
    assert!(signed_by(
        &[public(&stranger()), public(&key())],
        sums,
        signature.as_ref()
    ));
    assert!(!signed_by(&[public(&stranger())], sums, signature.as_ref()));
    assert!(!signed_by(&[], sums, signature.as_ref()));
    assert!(!signed_by(
        &[public(&key())],
        b"abd  arca\n",
        signature.as_ref()
    ));
    assert!(!signed_by(
        &[public(&key())],
        sums,
        &signature.as_ref()[..63]
    ));
    assert!(
        RELEASE_KEYS.is_empty(),
        "no release key exists yet; see RELEASE_KEYS"
    );
}

#[test]
fn a_unit_that_does_not_come_back_rolls_every_program_back_and_the_release_is_held_back() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("rollback");
    let fake = m.fake(Some(("2.0.0", Fault::Down)));
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert!(
        reason(&outcome).contains("arca-shim.service"),
        "{outcome:?}"
    );
    // Both programs are the old ones again, and the unit that failed runs the old one.
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert_eq!(program_version(&m.routes).as_deref(), Some("1.0.0"));
    assert_eq!(fake.restarted(), ["arca-shim.service", "arca-shim.service"]);
    let readback = outcome.detail["units"].as_array().expect("readback");
    assert_eq!(readback[0]["version"], "1.0.0", "{readback:?}");
    assert!(alert_input(&m.state, 1).value.failed);
    let state = read_state(&m.state).expect("state");
    assert_eq!(state["result"], "rolled-back");
    assert_eq!(state["rolledBack"], "2.0.0");
    assert!(read_pending(&m.state).is_none());
    assert_eq!(
        journal_steps(&m.state),
        [
            "download",
            "install",
            "install",
            "restart",
            "rolled-back",
            "check"
        ]
    );

    // The release it rolled back is not tried again: the alert stays.
    let again = m.run(&server, &fake);
    assert_eq!(again.result, "held-back", "{again:?}");
    assert_eq!(fake.restarted().len(), 2, "nothing restarted");
    assert!(alert_input(&m.state, 1).value.failed);

    // A newer release is.
    server.publish("2.0.1", Lie::None);
    let newer = m.run(&server, &fake);
    assert_eq!(newer.result, "updated", "{newer:?}");
    assert_eq!(program_version(&m.shared).as_deref(), Some("2.0.1"));
    let state = read_state(&m.state).expect("state");
    assert!(state["rolledBack"].is_null(), "{state}");
    assert!(!alert_input(&m.state, 1).value.failed);
}

#[test]
fn a_unit_that_restarts_within_the_health_time_rolls_back() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("dies");
    let fake = m.fake(Some(("2.0.0", Fault::DiesAfter(3))));
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert!(
        reason(&outcome).contains("stopped or restarted"),
        "{outcome:?}"
    );
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
}

#[test]
fn a_unit_that_comes_back_on_another_version_rolls_back() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("readback");
    let fake = m.fake(Some(("2.0.0", Fault::Says("1.5.0".to_string()))));
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert!(reason(&outcome).contains("it runs 1.5.0"), "{outcome:?}");
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
}

#[test]
fn a_node_that_does_not_answer_on_its_socket_rolls_back() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("silent");
    let fake = m.fake(Some(("2.0.0", Fault::Silent)));
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert!(
        reason(&outcome).contains(
            "arca-second.service did not come back on 2.0.0: it did not answer on its socket"
        ),
        "{outcome:?}"
    );
    // The shim came back, the node did not: both are restarted again on the old program.
    assert_eq!(
        fake.restarted(),
        [
            "arca-shim.service",
            "arca-second.service",
            "arca-shim.service",
            "arca-second.service"
        ]
    );
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
}

#[test]
fn a_waiting_owner_card_or_a_closed_window_puts_the_update_off() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("deferred");
    let fake = m.fake(None);
    write_waiting(&m.state, 1);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "deferred", "{outcome:?}");
    assert!(
        reason(&outcome).contains("arca-watch.service (1 waiting)"),
        "{outcome:?}"
    );
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert!(fake.restarted().is_empty());
    assert!(
        !alert_input(&m.state, 1).value.failed,
        "a put-off update is no failure"
    );

    // With no card waiting, a window that is closed now puts it off too; an open one lets it run.
    write_waiting(&m.state, 0);
    let window = Some("02:00-04:00".to_string());
    let closed = run_opts(
        &server,
        &m.config,
        &fake,
        m.root(),
        &Opts {
            window: window.clone(),
            minute: 12 * 60,
            ..m.opts()
        },
    );
    assert_eq!(closed.result, "waiting-for-window", "{closed:?}");
    let open = run_opts(
        &server,
        &m.config,
        &fake,
        m.root(),
        &Opts {
            window,
            minute: 3 * 60,
            ..m.opts()
        },
    );
    assert_eq!(open.result, "updated", "{open:?}");
}

#[test]
fn a_waiting_count_its_node_stopped_writing_puts_nothing_off() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("stale");
    let fake = m.fake(None);
    let stale = json!({ "count": 2, "atMs": now_ms() - WAITING_MAX_AGE_MS - 60_000 });
    write_private(
        &m.state.join(UPDATE_DIR).join(WAITING_FILE),
        &stale.to_string(),
    )
    .expect("waiting");
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "updated", "{outcome:?}");
}

#[test]
fn a_stopped_unit_is_not_started_and_its_waiting_count_puts_nothing_off() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let mut m = machine("stopped");
    for unit in &mut m.units {
        if unit.name == "arca-link.service" || unit.name == "arca-watch.service" {
            unit.active = false;
        }
    }
    write_waiting(&m.state, 3);
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "updated", "{outcome:?}");
    // The program of a stopped unit is updated all the same, so it starts on the release.
    assert_eq!(program_version(&m.routes).as_deref(), Some("2.0.0"));
    assert_eq!(
        fake.restarted(),
        ["arca-shim.service", "arca-second.service"]
    );
}

#[test]
fn a_build_from_a_branch_is_kept() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("branch");
    // The shared program says 1.0.0 and is not 1.0.0's published binary; the link's says a version never published.
    let branch = format!("{}# a build from a branch\n", program_text("1.0.0"));
    write_program_file(&m.shared, &branch);
    write_program_file(&m.routes, &program_text("1.5.0"));
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "branch-build", "{outcome:?}");
    assert!(reason(&outcome).contains("is kept"), "{outcome:?}");
    assert!(!alert_input(&m.state, 1).value.failed);
    assert_eq!(fs::read_to_string(&m.shared).expect("program"), branch);
    assert_eq!(program_version(&m.routes).as_deref(), Some("1.5.0"));
    assert!(fake.restarted().is_empty());

    // A release build beside it is updated, and the branch build's units are left alone.
    write_program_file(&m.routes, &program_text("1.0.0"));
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "updated", "{outcome:?}");
    assert_eq!(program_version(&m.routes).as_deref(), Some("2.0.0"));
    assert_eq!(fs::read_to_string(&m.shared).expect("program"), branch);
    assert_eq!(fake.restarted(), ["arca-link.service"]);
}

#[test]
fn a_program_the_updater_cannot_write_is_staged_and_every_unit_hands_over_to_it() {
    let _serial = serial();
    if rustix::process::geteuid().is_root() {
        // Root writes a directory of mode 0555 all the same.
        return;
    }
    let server = server_on("2.0.0");
    let scratch = scratch("staged");
    let root = scratch.0.clone();
    // A program in a directory this user may not write: a shim, a node and a link run it.
    let image = root.join("image/arca");
    write_program_file(&image, &program_text("1.0.0"));
    let image = fs::canonicalize(&image).expect("canonical");
    fs::set_permissions(root.join("image"), fs::Permissions::from_mode(0o555)).expect("mode");
    let (config, state) = node(&root, "os");
    let config_text = config.to_string_lossy().into_owned();
    let units = vec![
        unit("arca.service", &image, &["serve", "--config", &config_text]),
        unit("arca-shim.service", &image, &["shim", "--upstream", "/x"]),
        unit("arca-link.service", &image, &["link", "serve"]),
    ];
    let stage = root.join("stage");
    let fake = FakeUnits::new(units.clone(), Some(&stage), None);
    let outcome = run_opts(&server, &config, &fake, &root, &Opts::new(&root));
    assert_eq!(outcome.result, "updated", "{outcome:?}");
    assert_eq!(
        program_version(&image).as_deref(),
        Some("1.0.0"),
        "the program is as it was"
    );
    let staged = staged_program(&staging_dir(&stage, &image)).expect("a staged program");
    assert_eq!(staged.version, "2.0.0");
    assert_eq!(program_version(&staged.path).as_deref(), Some("2.0.0"));
    assert_eq!(
        fake.restarted(),
        ["arca-shim.service", "arca.service", "arca-link.service"]
    );
    // The next look counts the staged program as what the units run.
    let again = run_opts(&server, &config, &fake, &root, &Opts::new(&root));
    assert_eq!(again.result, "up-to-date", "{again:?}");

    // A staged release that does not come back is taken away again: the units run the one staged before.
    server.publish("3.0.0", Lie::None);
    let broken = FakeUnits::new(units, Some(&stage), Some(("3.0.0", Fault::Down)));
    let outcome = run_opts(&server, &config, &broken, &root, &Opts::new(&root));
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert_eq!(
        staged_program(&staging_dir(&stage, &image))
            .expect("staged")
            .version,
        "2.0.0"
    );
    assert!(read_pending(&state).is_none());
}

#[test]
fn a_program_that_cannot_be_written_or_staged_is_left_to_the_os_or_refused() {
    let _serial = serial();
    if rustix::process::geteuid().is_root() {
        return;
    }
    let server = server_on("2.0.0");
    let scratch = scratch("os");
    let root = scratch.0.clone();
    let image = root.join("image/arca");
    write_program_file(&image, &program_text("1.0.0"));
    fs::set_permissions(root.join("image"), fs::Permissions::from_mode(0o555)).expect("mode");
    let (config, state) = node(&root, "os");
    let config_text = config.to_string_lossy().into_owned();
    let node_unit = unit("arca.service", &image, &["serve", "--config", &config_text]);

    // No place a release could run from: the program is the OS's, and that is no failure.
    let fake = FakeUnits::new(vec![node_unit.clone()], None, None);
    let outcome = run_opts(
        &server,
        &config,
        &fake,
        &root,
        &Opts {
            stage_root: None,
            ..Opts::new(&root)
        },
    );
    assert_eq!(outcome.result, "os-managed", "{outcome:?}");
    assert!(!alert_input(&state, 1).value.failed);
    assert_eq!(journal_steps(&state), ["check"], "nothing was downloaded");

    // A unit of the program that would not hand over: refused before anything changes.
    let watch = unit("arca-watcher.service", &image, &["watch"]);
    let fake = FakeUnits::new(vec![node_unit, watch], Some(&root.join("stage")), None);
    let outcome = run_opts(&server, &config, &fake, &root, &Opts::new(&root));
    assert_eq!(outcome.result, "failed", "{outcome:?}");
    assert!(
        reason(&outcome).contains("arca-watcher.service would not run a staged release"),
        "{outcome:?}"
    );
    assert!(fake.restarted().is_empty());
    assert!(!root.join("stage").exists(), "nothing was staged");
}

#[test]
fn where_a_release_goes_follows_the_directory_the_staging_root_and_the_units() {
    let node = unit("arca.service", Path::new("/p/arca"), &["serve"]);
    let shim = unit("arca-shim.service", Path::new("/p/arca"), &["shim"]);
    let mut watch = unit("arca-watch.service", Path::new("/p/arca"), &["watch"]);
    let units = [node.clone(), shim.clone()];
    assert_eq!(
        placement(&DirWrite::Writable, false, &units),
        Placement::InPlace
    );
    assert!(matches!(
        placement(&DirWrite::ReadOnly, true, &units),
        Placement::OsManaged(_)
    ));
    assert!(matches!(
        placement(&DirWrite::Denied, false, &units),
        Placement::OsManaged(_)
    ));
    assert_eq!(
        placement(&DirWrite::Denied, true, &units),
        Placement::Staged
    );
    let with_watch = [node.clone(), watch.clone()];
    assert!(matches!(
        placement(&DirWrite::Denied, true, &with_watch),
        Placement::Refused(_)
    ));
    // A stopped unit that would not hand over does not hold the others back.
    watch.active = false;
    assert_eq!(
        placement(&DirWrite::Denied, true, &[node, watch]),
        Placement::Staged
    );
    assert!(matches!(
        placement(&DirWrite::Failed("x".to_string()), true, &units),
        Placement::Refused(_)
    ));
    let scratch = scratch("probe");
    assert_eq!(probe_write(&scratch.0), DirWrite::Writable);
    assert_eq!(
        fs::read_dir(&scratch.0).expect("dir").count(),
        0,
        "the probe is removed"
    );
    assert!(
        stage_runs(&scratch.0.join("not-yet")),
        "an ancestor answers"
    );
}

#[test]
fn a_staged_program_runs_only_when_it_is_this_users_alone_and_its_content_is_the_manifests() {
    let scratch = scratch("staged-file");
    let dir = scratch.0.join("stage");
    fs::create_dir_all(&dir).expect("dir");
    let text = program_text("2.0.0");
    let file = dir.join("arca-2.0.0");
    write_program_file(&file, &text);
    let manifest =
        json!({ "file": "arca-2.0.0", "version": "2.0.0", "sha256": sha256_hex(text.as_bytes()) });
    write_private(&dir.join(STAGED_FILE), &manifest.to_string()).expect("manifest");
    assert_eq!(staged_program(&dir).expect("staged").version, "2.0.0");
    // Writable by its group: never.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o775)).expect("mode");
    assert!(staged_program(&dir).is_none());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("mode");
    // Other content than the manifest's: never.
    write_program_file(&file, &program_text("2.0.1"));
    assert!(staged_program(&dir).is_none());
    // A link to the right content: never.
    let elsewhere = scratch.0.join("elsewhere");
    write_program_file(&elsewhere, &text);
    fs::remove_file(&file).expect("remove");
    std::os::unix::fs::symlink(&elsewhere, &file).expect("link");
    assert!(staged_program(&dir).is_none());
    // A manifest that names a file outside its directory: never.
    let outside = json!({ "file": "../elsewhere", "version": "2.0.0", "sha256": sha256_hex(text.as_bytes()) });
    write_private(&dir.join(STAGED_FILE), &outside.to_string()).expect("manifest");
    assert!(staged_program(&dir).is_none());
}

#[test]
fn a_program_hands_over_only_to_a_newer_staged_release() {
    assert!(should_hand_over("1.0.0", "2.0.0"));
    assert!(!should_hand_over("2.0.0", "2.0.0"));
    assert!(!should_hand_over("3.0.0", "2.0.0"));
    assert!(!should_hand_over("1.0.0", "garbage"));
    for role in [Role::Shim, Role::Node, Role::Link] {
        assert!(role.hands_over());
    }
    assert!(!Role::Other.hands_over());
    // Two programs of one user never share a staging directory.
    let root = Path::new("/s");
    assert_ne!(
        staging_dir(root, Path::new("/a/arca")),
        staging_dir(root, Path::new("/b/arca"))
    );
    assert_eq!(
        staging_dir(root, Path::new("/a/arca")),
        staging_dir(root, Path::new("/a/arca"))
    );
}

#[test]
fn staging_removes_only_old_staged_releases() {
    let scratch = scratch("prune");
    let dir = &scratch.0;
    for name in [
        "arca-1.0.0",
        "arca-2.0.0",
        "arca-3.0.0",
        "update.lock",
        "arca-update.lock",
        "arca-notes",
    ] {
        fs::write(dir.join(name), name).expect("file");
    }
    let manifest = |file: &str, version: &str| {
        json!({ "file": file, "version": version, "sha256": "x" }).to_string()
    };
    write_private(&dir.join(STAGED_FILE), &manifest("arca-3.0.0", "3.0.0")).expect("current");
    write_private(
        &dir.join(STAGED_PREVIOUS_FILE),
        &manifest("arca-2.0.0", "2.0.0"),
    )
    .expect("previous");
    prune_staged(dir);
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "arca-2.0.0",
            "arca-3.0.0",
            "arca-notes",
            "arca-update.lock",
            "current.json",
            "previous.json",
            "update.lock"
        ]
    );
}

/// The pending record a run that stopped after it changed the shared program leaves.
fn left_pending(m: &Machine) -> Value {
    write_program_file(&previous_of(&m.shared), &program_text("1.0.0"));
    write_program_file(&m.shared, &program_text("2.0.0"));
    let units: Vec<Value> = m
        .units
        .iter()
        .filter(|unit| unit.program == m.shared)
        .map(|unit| json!({ "name": unit.name, "program": unit.program, "args": unit.args, "from": "1.0.0" }))
        .collect();
    let change = Changed::InPlace {
        path: m.shared.clone(),
        previous: previous_of(&m.shared),
    };
    let pending =
        json!({ "release": "2.0.0", "atMs": 1, "changes": [change.json()], "units": units });
    write_private(
        &m.state.join(UPDATE_DIR).join(PENDING_FILE),
        &pending.to_string(),
    )
    .expect("pending");
    pending
}

#[test]
fn an_update_left_pending_whose_units_run_the_release_is_finished() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("finish");
    left_pending(&m);
    let fake = m.fake(None);
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "updated", "{outcome:?}");
    assert_eq!(program_version(&m.shared).as_deref(), Some("2.0.0"));
    assert!(
        fake.restarted().is_empty(),
        "every unit was already healthy on the release"
    );
    assert!(read_pending(&m.state).is_none());
    assert_eq!(journal_steps(&m.state), ["finished", "check"]);
    // The second node of the program records it too.
    assert_eq!(
        read_state(&m.second_state).expect("state")["result"],
        "updated"
    );
}

#[test]
fn an_update_left_pending_whose_units_are_not_healthy_is_rolled_back() {
    let _serial = serial();
    let server = server_on("2.0.0");
    let m = machine("unfinished");
    left_pending(&m);
    let fake = m.fake(Some(("2.0.0", Fault::Silent)));
    let outcome = m.run(&server, &fake);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert!(
        reason(&outcome).contains("arca-second.service"),
        "{outcome:?}"
    );
    assert_eq!(program_version(&m.shared).as_deref(), Some("1.0.0"));
    assert!(read_pending(&m.state).is_none());
    assert_eq!(read_state(&m.state).expect("state")["rolledBack"], "2.0.0");
    // Every unit the update named is restarted on the old program.
    let mut restarted = fake.restarted();
    restarted.sort();
    assert_eq!(
        restarted,
        [
            "arca-second.service",
            "arca-shim.service",
            "arca-watch.service"
        ]
    );
}

#[test]
fn a_second_run_while_one_holds_the_lock_is_busy() {
    let _serial = serial();
    let server = Server::start();
    let scratch = scratch("busy");
    let (config, _) = node(&scratch.0, "busy");
    let held = File::create(scratch.0.join("lock")).expect("lock");
    held.try_lock().expect("held");
    let fake = FakeUnits::new(Vec::new(), None, None);
    let outcome = run_opts(&server, &config, &fake, &scratch.0, &Opts::new(&scratch.0));
    assert_eq!(outcome.result, "busy");
}

#[test]
fn the_lock_without_a_runtime_directory_is_one_no_staging_step_removes() {
    // Either name is one `prune_staged` keeps: neither is `arca-<version>`.
    let lock = lock_path(Path::new("/state"));
    let name = lock
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a name");
    assert!(matches!(name, LOCK_FILE | STAGE_LOCK_FILE), "{lock:?}");
    for name in [LOCK_FILE, STAGE_LOCK_FILE] {
        assert!(Version::parse(name.strip_prefix("arca-").unwrap_or(name)).is_none());
    }
}

#[test]
fn the_newest_release_of_the_channel_is_picked() {
    let listing = json!([
        { "tag_name": "v0.31.0-rc.1", "draft": false, "prerelease": true, "assets": [] },
        { "tag_name": "v0.32.0", "draft": true, "prerelease": false, "assets": [] },
        { "tag_name": "v0.30.0", "draft": false, "prerelease": false, "assets": [
            { "name": "SHA256SUMS", "browser_download_url": "https://example.invalid/s" }
        ]},
        { "tag_name": "nightly", "draft": false, "prerelease": false, "assets": [] },
        { "tag_name": "v0.29.0", "draft": false, "prerelease": false, "assets": [] },
    ])
    .to_string();
    let stable = newest_release(&listing, UpdateChannel::Stable)
        .expect("parses")
        .expect("one");
    assert_eq!(stable.version_text, "0.30.0");
    assert_eq!(stable.assets["SHA256SUMS"], "https://example.invalid/s");
    let pre = newest_release(&listing, UpdateChannel::Prerelease)
        .expect("parses")
        .expect("one");
    assert_eq!(pre.version_text, "0.31.0-rc.1");
    assert!(
        newest_release("[]", UpdateChannel::Stable)
            .expect("parses")
            .is_none()
    );
    assert!(newest_release("{}", UpdateChannel::Stable).is_err());
}

#[test]
fn checksums_versions_and_exec_lines_are_read_as_the_release_and_systemd_write_them() {
    let sums = format!(
        "{}  arca-1.0.0-linux-x86_64\n{} *install.sh\n",
        "a".repeat(64),
        "B".repeat(64)
    );
    assert_eq!(
        checksum_of(&sums, "arca-1.0.0-linux-x86_64"),
        Some("a".repeat(64))
    );
    assert_eq!(checksum_of(&sums, "install.sh"), Some("b".repeat(64)));
    assert_eq!(checksum_of(&sums, "arca"), None);
    assert_eq!(
        said_version("Automagik Arca 0.29.0 (5712f3b2, x86_64-unknown-linux-gnu)").as_deref(),
        Some("0.29.0")
    );
    assert_eq!(said_version("Something 1.0.0"), None);
    let exec = "{ path=/home/u/.local/lib/arca-routes/arca ; argv[]=/home/u/.local/lib/arca-routes/arca serve --config /home/u/.config/arca/second.json ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
    let (path, args) = exec_start(exec).expect("parsed");
    assert_eq!(path, PathBuf::from("/home/u/.local/lib/arca-routes/arca"));
    assert_eq!(
        args,
        ["serve", "--config", "/home/u/.config/arca/second.json"]
    );
    let unit = Unit {
        name: "arca-second.service".to_string(),
        program: path,
        args,
        active: true,
    };
    assert_eq!(unit.role(), Role::Node);
    assert_eq!(
        unit.config(),
        Some(PathBuf::from("/home/u/.config/arca/second.json"))
    );
    assert!(may_be_arca(Path::new("/usr/local/bin/arca")));
    assert!(!may_be_arca(Path::new("/usr/bin/incus")));
    let blocks =
        show_blocks("Id=a.service\nActiveState=active\n\nId=b.service\nActiveState=failed\n");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1]["ActiveState"], "failed");
    assert!(newer("0.30.0", "0.29.0"));
    assert!(!newer("0.30.0", "0.30.0"));
    assert!(!newer("0.30.0", "garbage"));
}

#[test]
fn a_socket_answers_only_when_something_speaks_http_on_it() {
    use std::os::unix::net::UnixListener;
    let scratch = scratch("socket");
    let path = scratch.0.join("node.sock");
    assert!(!socket_answers(&path), "no socket");
    let listener = UnixListener::bind(&path).expect("bind");
    let server = std::thread::spawn(move || {
        for reply in [
            &b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"[..],
            b"SSH-2.0-x\r\n",
        ] {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 512];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(reply);
        }
    });
    assert!(socket_answers(&path));
    assert!(!socket_answers(&path));
    server.join().expect("server");
}

#[test]
fn the_window_and_the_schedule_decide_when_a_look_runs() {
    assert!(in_window(None, 5));
    assert!(in_window(Some((120, 240)), 180));
    assert!(!in_window(Some((120, 240)), 240));
    // Over midnight.
    assert!(in_window(Some((23 * 60, 60)), 23 * 60 + 30));
    assert!(in_window(Some((23 * 60, 60)), 30));
    assert!(!in_window(Some((23 * 60, 60)), 120));

    let update = UpdateConfig::default();
    let hour = 3_600_000;
    assert!(
        due(&update, None, None, 10 * hour),
        "a node with no record looks"
    );
    let state = |result: &str, at: u64| json!({ "result": result, "checkedAtMs": at });
    assert!(!due(
        &update,
        Some(&state("up-to-date", 10 * hour)),
        None,
        15 * hour
    ));
    assert!(due(
        &update,
        Some(&state("up-to-date", 10 * hour)),
        None,
        16 * hour
    ));
    // A put-off update is tried again within fifteen minutes.
    assert!(due(
        &update,
        Some(&state("deferred", 10 * hour)),
        None,
        10 * hour + 900_000
    ));
    assert!(!due(
        &update,
        Some(&state("held-back", 10 * hour)),
        None,
        10 * hour + 900_000
    ));
    // A node that started a run waits for it.
    assert!(!due(&update, None, Some(10 * hour), 10 * hour + 60_000));
}
