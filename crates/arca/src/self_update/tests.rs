//! The updater against a fake release server, programs that are shell scripts, and a fake service manager.

use super::*;
use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::sync::Mutex;

/// The runs share the process's children, so they go one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "arca-update-{tag}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A program that says `version`, and on `templates install` leaves a mark beside itself.
fn program_text(version: &str) -> String {
    format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'Automagik Arca {version} (test, {arch}-unknown-linux-gnu)' ;;\n  templates) echo \"$*\" > \"$(dirname \"$0\")/templates-ran-{version}\"; echo '{{\"replaced\":[],\"kept\":[]}}' ;;\nesac\n",
        arch = std::env::consts::ARCH
    )
}

fn write_program_file(path: &Path, version: &str) {
    fs::create_dir_all(path.parent().expect("a directory")).expect("bin dir");
    fs::write(path, program_text(version)).expect("program");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mode");
}

/// A release server: one listing and the files it links to, over plain HTTP on loopback.
struct Server {
    base: String,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

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
        Server { base, files }
    }

    fn source(&self) -> String {
        format!("{}/repos/automagik-si/arca", self.base)
    }

    /// Publishes `version` (and an older pre-release and a draft, which the stable channel never takes). `sums_lie`
    /// gives the binary a wrong checksum.
    fn publish(&self, version: &str, sums_lie: bool) {
        let asset = asset_name(version, std::env::consts::ARCH);
        let binary = program_text(version).into_bytes();
        let installer = b"#!/bin/sh\necho install\n".to_vec();
        let binary_sum = if sums_lie {
            "0".repeat(64)
        } else {
            sha256_hex(&binary)
        };
        let sums = format!(
            "{binary_sum}  {asset}\n{}  install.sh\n",
            sha256_hex(&installer)
        );
        let link = |name: &str| format!("{}/dl/{version}/{name}", self.base);
        let listing = json!([
            { "tag_name": "v99.0.0", "draft": true, "prerelease": false, "assets": [] },
            { "tag_name": "v98.0.0-rc.1", "draft": false, "prerelease": true, "assets": [] },
            { "tag_name": format!("v{version}"), "draft": false, "prerelease": false, "assets": [
                { "name": asset, "browser_download_url": link(&asset) },
                { "name": "SHA256SUMS", "browser_download_url": link("SHA256SUMS") },
                { "name": "install.sh", "browser_download_url": link("install.sh") },
            ]},
            { "tag_name": "v0.1.0", "draft": false, "prerelease": false, "assets": [] },
        ]);
        let mut files = self.files.lock().expect("files");
        files.insert(
            "/repos/automagik-si/arca/releases".to_string(),
            listing.to_string().into_bytes(),
        );
        files.insert(format!("/dl/{version}/{asset}"), binary);
        files.insert(format!("/dl/{version}/SHA256SUMS"), sums.into_bytes());
        files.insert(format!("/dl/{version}/install.sh"), installer);
    }
}

/// A service manager whose units run the programs their paths name. A unit whose program says `broken` does not stay
/// up after its start.
struct FakeUnits {
    units: Vec<Unit>,
    running: Mutex<BTreeMap<String, (Running, Option<String>)>>,
    broken: Option<String>,
    restarts: Mutex<Vec<String>>,
}

impl FakeUnits {
    fn new(units: Vec<Unit>, broken: Option<&str>) -> FakeUnits {
        let fake = FakeUnits {
            units,
            running: Mutex::default(),
            broken: broken.map(str::to_string),
            restarts: Mutex::default(),
        };
        for unit in &fake.units {
            fake.start(unit);
        }
        fake.restarts.lock().expect("restarts").clear();
        fake
    }

    /// What the unit runs when it starts: its program, or the staged one its node hands over to.
    fn start(&self, unit: &Unit) {
        let mut version = program_version(&unit.program);
        if let Some(state_dir) = node_config(unit).map(|config| config.state_dir)
            && let Some(staged) = staged_program(&state_dir)
            && newer(&staged.version, version.as_deref().unwrap_or_default())
        {
            version = Some(staged.version);
        }
        let mut running = self.running.lock().expect("running");
        let pid = running.get(&unit.name).map_or(100, |(r, _)| r.pid + 1);
        let up = version.is_some() && version != self.broken;
        running.insert(
            unit.name.clone(),
            (
                Running {
                    active: up,
                    pid: if up { pid } else { 0 },
                    restarts: 0,
                },
                version,
            ),
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
        Ok(self.running.lock().expect("running")[name].0)
    }

    fn running_version(&self, name: &str, _running: &Running) -> Option<String> {
        self.running.lock().expect("running")[name].1.clone()
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

fn run_with(
    config: &Path,
    units: &dyn Units,
    lock: &Path,
    install_note: Option<PathBuf>,
    minute: u32,
) -> Outcome {
    let mut config = crate::config::load(config).expect("config loads");
    config.update.source = SOURCE.with(|source| source.borrow().clone());
    WINDOW.with(|window| config.update.window = window.borrow().clone());
    Run {
        config: &config,
        units,
        arch: std::env::consts::ARCH,
        health: Duration::from_secs(2),
        settle: Duration::from_millis(10),
        poll: Duration::from_millis(5),
        install_note,
        lock: lock.to_path_buf(),
        minute_of_day: minute,
    }
    .run(true)
}

thread_local! {
    static SOURCE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    static WINDOW: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
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

/// A machine like the orchestrator's: a shim and a node on one program, a link on another path, and a node of its
/// own whose program is newer than any release.
struct Machine {
    root: PathBuf,
    shared: PathBuf,
    routes: PathBuf,
    dev: PathBuf,
    config: PathBuf,
    state: PathBuf,
    dev_state: PathBuf,
    units: Vec<Unit>,
}

fn machine(tag: &str) -> Machine {
    let root = scratch(tag);
    let shared = root.join("bin/arca");
    let routes = root.join("lib/arca-routes/arca");
    let dev = root.join("dev/arca");
    write_program_file(&shared, "1.0.0");
    write_program_file(&routes, "1.0.0");
    write_program_file(&dev, "3.0.0");
    let (config, state) = node(&root, "watch");
    let (dev_config, dev_state) = node(&root, "dev");
    let config_text = config.to_string_lossy().into_owned();
    let dev_text = dev_config.to_string_lossy().into_owned();
    let units = vec![
        unit(
            "arca-watch.service",
            &shared,
            &["serve", "--config", &config_text],
        ),
        unit("arca-link.service", &routes, &["link", "serve"]),
        unit("arca-shim.service", &shared, &["shim", "--upstream", "/x"]),
        unit("arca-dev.service", &dev, &["serve", "--config", &dev_text]),
    ];
    Machine {
        root,
        shared,
        routes,
        dev,
        config,
        state,
        dev_state,
        units,
    }
}

#[test]
fn a_node_on_the_newest_release_changes_nothing_and_journals_the_look() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    server.publish("1.0.0", false);
    SOURCE.with(|source| *source.borrow_mut() = server.source());
    let m = machine("current");
    let fake = FakeUnits::new(m.units.clone(), None);
    let outcome = run_with(&m.config, &fake, &m.root.join("lock"), None, 0);
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
    let _ = fs::remove_dir_all(&m.root);
}

#[test]
fn a_new_release_replaces_every_program_behind_it_and_restarts_shims_then_nodes_then_links() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    server.publish("2.0.0", false);
    SOURCE.with(|source| *source.borrow_mut() = server.source());
    let m = machine("installs");
    fs::create_dir_all(m.state.join("templates")).expect("templates");
    // The installer's note names the shared program by its old checksum.
    let note = m.root.join("program.sha256");
    let old_sha = sha256_file(&m.shared).expect("sha");
    fs::write(&note, format!("{old_sha}  {}\n", m.shared.display())).expect("note");
    let fake = FakeUnits::new(m.units.clone(), None);

    let outcome = run_with(
        &m.config,
        &fake,
        &m.root.join("lock"),
        Some(note.clone()),
        0,
    );
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
            "arca-watch.service",
            "arca-link.service"
        ]
    );
    // Each unit's version was read back.
    let units = outcome.detail["units"].as_array().expect("units");
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
            "templates",
            "updated",
            "check"
        ]
    );
    // The other node of the machine records the run too.
    assert_eq!(
        read_state(&m.dev_state).expect("state")["result"],
        "updated"
    );
    let state = read_state(&m.state).expect("state");
    assert_eq!(state["result"], "updated");
    assert_eq!(state["available"], "2.0.0");
    assert!(state["updatedAtMs"].as_u64().is_some());

    // The next look finds every program on the release.
    let again = run_with(&m.config, &fake, &m.root.join("lock"), None, 0);
    assert_eq!(again.result, "up-to-date", "{again:?}");
    let _ = fs::remove_dir_all(&m.root);
}

#[test]
fn a_checksum_that_does_not_match_keeps_every_old_program_and_raises_the_alert() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    server.publish("2.0.0", true);
    SOURCE.with(|source| *source.borrow_mut() = server.source());
    let m = machine("checksum");
    let before = fs::read(&m.shared).expect("program");
    let fake = FakeUnits::new(m.units.clone(), None);
    let outcome = run_with(&m.config, &fake, &m.root.join("lock"), None, 0);
    assert_eq!(outcome.result, "failed", "{outcome:?}");
    assert!(
        outcome
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("checksum"),
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
    server.publish("1.0.0", false);
    let fixed = run_with(&m.config, &fake, &m.root.join("lock"), None, 0);
    assert_eq!(fixed.result, "up-to-date", "{fixed:?}");
    assert!(!alert_input(&m.state, 1).value.failed);
    let _ = fs::remove_dir_all(&m.root);
}

#[test]
fn a_unit_that_does_not_come_back_rolls_every_program_back_and_raises_the_alert() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    server.publish("2.0.0", false);
    SOURCE.with(|source| *source.borrow_mut() = server.source());
    let m = machine("rollback");
    let fake = FakeUnits::new(m.units.clone(), Some("2.0.0"));
    let outcome = run_with(&m.config, &fake, &m.root.join("lock"), None, 0);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert!(
        outcome
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("arca-shim.service"),
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
    let _ = fs::remove_dir_all(&m.root);
}

#[test]
fn a_waiting_owner_card_or_a_closed_window_puts_the_update_off() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    server.publish("2.0.0", false);
    SOURCE.with(|source| *source.borrow_mut() = server.source());
    let m = machine("deferred");
    let fake = FakeUnits::new(m.units.clone(), None);
    write_waiting(&m.state, 1);
    let outcome = run_with(&m.config, &fake, &m.root.join("lock"), None, 0);
    assert_eq!(outcome.result, "deferred", "{outcome:?}");
    assert!(
        outcome
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("arca-watch.service (1 waiting)"),
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
    WINDOW.with(|window| *window.borrow_mut() = Some("02:00-04:00".to_string()));
    let closed = run_with(&m.config, &fake, &m.root.join("lock"), None, 12 * 60);
    assert_eq!(closed.result, "waiting-for-window", "{closed:?}");
    let open = run_with(&m.config, &fake, &m.root.join("lock"), None, 3 * 60);
    WINDOW.with(|window| *window.borrow_mut() = None);
    assert_eq!(open.result, "updated", "{open:?}");
    let _ = fs::remove_dir_all(&m.root);
}

#[test]
fn a_program_the_updater_cannot_write_is_staged_for_its_node_to_hand_over_to() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if rustix::process::geteuid().is_root() {
        // Root writes a directory of mode 0555 all the same.
        return;
    }
    let server = Server::start();
    server.publish("2.0.0", false);
    SOURCE.with(|source| *source.borrow_mut() = server.source());
    let root = scratch("staged");
    // Arca OS: the node's program is in the image, which cannot be written.
    let image = root.join("image/arca");
    write_program_file(&image, "1.0.0");
    fs::set_permissions(root.join("image"), fs::Permissions::from_mode(0o555)).expect("mode");
    let (config, state) = node(&root, "os");
    let config_text = config.to_string_lossy().into_owned();
    let units = vec![unit(
        "arca.service",
        &image,
        &["serve", "--config", &config_text],
    )];
    let fake = FakeUnits::new(units.clone(), None);
    let outcome = run_with(&config, &fake, &root.join("lock"), None, 0);
    assert_eq!(outcome.result, "updated", "{outcome:?}");
    assert_eq!(
        program_version(&image).as_deref(),
        Some("1.0.0"),
        "the image is as it was"
    );
    let staged = staged_program(&state).expect("a staged program");
    assert_eq!(staged.version, "2.0.0");
    assert_eq!(program_version(&staged.path).as_deref(), Some("2.0.0"));
    // The next look counts the staged program as what the node runs.
    let again = run_with(&config, &fake, &root.join("lock"), None, 0);
    assert_eq!(again.result, "up-to-date", "{again:?}");

    // A staged release that does not come back is taken away again: the node runs the image's program.
    server.publish("3.0.0", false);
    let broken = FakeUnits::new(units, Some("3.0.0"));
    let outcome = run_with(&config, &broken, &root.join("lock"), None, 0);
    assert_eq!(outcome.result, "rolled-back", "{outcome:?}");
    assert_eq!(staged_program(&state).expect("staged").version, "2.0.0");

    fs::set_permissions(root.join("image"), fs::Permissions::from_mode(0o755)).expect("mode");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_second_run_while_one_holds_the_lock_is_busy() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = scratch("busy");
    let (config, _) = node(&root, "busy");
    let lock = root.join("lock");
    let held = File::create(&lock).expect("lock");
    held.try_lock().expect("held");
    let fake = FakeUnits::new(Vec::new(), None);
    let outcome = run_with(&config, &fake, &lock, None, 0);
    assert_eq!(outcome.result, "busy");
    let _ = fs::remove_dir_all(&root);
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
    // A node that started a run waits for it.
    assert!(!due(&update, None, Some(10 * hour), 10 * hour + 60_000));
}
