//! The installer (`scripts/install.sh`) against a fake release, a fake `arca` and a fake `systemctl`.
//!
//! The script is the shipped one, unchanged: only what it reaches outside itself is replaced. `curl`, `systemctl`,
//! `uname` and `getconf` are scripts on its `PATH` (`tests/install_script/fake-curl.sh` serves a release from a
//! directory and plays Incus on its socket; `fake-systemctl.sh` knows the one unit). The release's program is
//! `fake-arca.sh`, so the installer downloads it, checks it, installs it and then runs it. The `PATH` holds those
//! stand-ins and the programs the script lists for itself and nothing else, so a program it uses and does not list
//! fails these tests, and so would `sudo`. The home, the runtime directory and the Incus socket come from the
//! environment. The script's standard input is a pipe, never a terminal, and each stand-in records a call that could
//! read it.
//!
//! So this proves the script's own logic: the checks and their reasons, the one named release and its checksum, what
//! is written where and at which mode, that a config the script did not write is not changed and that no node is
//! started for one, the HTTP door's port and the connect lines, that the script's own earlier config gains the door
//! and loses it again under an older program, the service's commands in order, the report and how the script ends,
//! the uninstall, and that the token reaches no output. It does not prove GitHub, a real download, the
//! real `arca`, systemd, Incus or a machine that never had Arca.
//!
//! The tests need `sh`, `bash` and the usual core utilities on the machine that runs them, and a user who is not
//! root, since the script refuses root. The token is made here.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// A token of the real shape, which is no node's.
const TOKEN: &str = "arca_c3ludGhldGljLXRva2VuLWZvci10ZXN0cy1vbmx5LTAwMDA";
const NEXT: &str = "0.1.0-alpha.99";
static SEQ: AtomicU64 = AtomicU64::new(0);

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn script() -> PathBuf {
    repo().join("scripts/install.sh")
}

fn script_text() -> String {
    fs::read_to_string(script()).expect("the script")
}

fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/install_script")
        .join(name)
}

fn find(tool: &str) -> Option<PathBuf> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .expect("sh");
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && path.starts_with('/')).then(|| PathBuf::from(path))
}

fn which(tool: &str) -> PathBuf {
    find(tool).unwrap_or_else(|| {
        panic!("this test needs sh, bash and the core utilities on this machine; {tool} is missing")
    })
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).expect("a script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("0755");
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|_| panic!("{} is there", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

/// The value of a `NAME=value` or `NAME='value'` line of the script.
fn constant(name: &str) -> String {
    script_text()
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("the script sets {name}"))
        .trim_matches('\'')
        .to_string()
}

/// The release the script installs when it is given no `--version`.
fn current() -> String {
    constant("ARCA_VERSION")
}

/// The first release whose program has the HTTP door, as the script names it.
fn door_since() -> String {
    constant("DOOR_SINCE")
}

/// Whether the program of release `version` has the HTTP door: the release that brought it, or a later one. A
/// pre-release comes before its release. These tests hold on both sides of that release, so each says which side its
/// program is on.
fn has_door(version: &str) -> bool {
    let numbers = |text: &str| -> (Vec<u64>, bool) {
        let (core, pre) = match text.split_once('-') {
            Some((core, _)) => (core, true),
            None => (text, false),
        };
        let core = core
            .split('.')
            .map(|part| part.parse().expect("a number in a version"))
            .collect();
        (core, pre)
    };
    let (have, pre) = numbers(version);
    let (since, _) = numbers(&door_since());
    have > since || (have == since && !pre)
}

/// A release older than the door, whatever release the script installs today.
const BEFORE_DOOR: &str = "0.10.0";

fn uid() -> String {
    let output = Command::new(which("id")).arg("-u").output().expect("id");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The release's program for `version`: the fake `arca`, which says that version.
fn program(version: &str) -> String {
    fs::read_to_string(fake("fake-arca.sh"))
        .expect("the fake arca")
        .replace("@VERSION@", version)
}

/// A directory's entries by name: the mode, and a file's bytes.
type Tree = BTreeMap<String, (u32, Option<Vec<u8>>)>;

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Run {
    fn all(&self) -> String {
        format!("{}\n{}", self.out, self.err)
    }

    fn ok(&self) -> &Run {
        assert_eq!(self.code, Some(0), "the installer failed:\n{}", self.all());
        self
    }

    fn refused(&self) -> &Run {
        assert_eq!(
            self.code,
            Some(1),
            "the installer did not refuse:\n{}",
            self.all()
        );
        self
    }

    /// The program is installed and the script started no node: it ends with 3.
    fn not_started(&self) -> &Run {
        assert_eq!(
            self.code,
            Some(3),
            "the installer did not end with 3:\n{}",
            self.all()
        );
        self.says("This script started no node.")
    }

    fn never_says(&self, words: &str) -> &Run {
        assert!(
            !self.all().contains(words),
            "the installer said \"{words}\":\n{}",
            self.all()
        );
        self
    }

    fn says(&self, words: &str) -> &Run {
        assert!(
            self.all().contains(words),
            "the installer did not say \"{words}\":\n{}",
            self.all()
        );
        self
    }
}

/// A scratch machine: a home, a runtime directory, an Incus socket, a release to download and a `PATH` of the
/// stand-ins and the script's listed programs only.
struct Machine {
    dir: PathBuf,
    /// Whether the shell that runs the script has `XDG_RUNTIME_DIR`.
    session: bool,
    _incus: UnixListener,
}

impl Machine {
    fn new() -> Machine {
        let dir = std::env::temp_dir().join(format!(
            "arca-install-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        assert_ne!(
            uid(),
            "0",
            "the installer refuses root, so these tests cannot run as root"
        );
        let _ = fs::remove_dir_all(&dir);
        for sub in ["bin", "home", "run", "fake", "tmp", "cwd"] {
            fs::create_dir_all(dir.join(sub)).expect("scratch");
        }
        // The directory the script is started in and the system's temporary directory are not its to write: here
        // it cannot, so a script that tried would fail.
        for sub in ["tmp", "cwd"] {
            fs::set_permissions(dir.join(sub), fs::Permissions::from_mode(0o555)).expect("0555");
        }
        // A runtime directory is its user's alone.
        fs::set_permissions(dir.join("run"), fs::Permissions::from_mode(0o700)).expect("0700");
        let incus = UnixListener::bind(dir.join("incus.socket"))
            .expect("a unix socket; is the temporary directory's path short enough for one?");
        let machine = Machine {
            dir,
            session: true,
            _incus: incus,
        };
        for tool in ["curl", "systemctl"] {
            let path = machine.dir.join("bin").join(tool);
            fs::copy(fake(&format!("fake-{tool}.sh")), &path).expect("a stand-in");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("0755");
        }
        machine.uname("Linux", "x86_64");
        machine.glibc("glibc 2.34");
        for program in constant("INSTALL_PROGRAMS").split_whitespace() {
            let link = machine.dir.join("bin").join(program);
            if !link.exists() {
                std::os::unix::fs::symlink(which(program), link).expect("a link");
            }
        }
        machine.release(&current());
        machine
    }

    fn uname(&self, system: &str, hardware: &str) {
        executable(
            &self.dir.join("bin/uname"),
            &format!(
                "#!{}\ncase $1 in -s) echo {system} ;; -m) echo {hardware} ;; *) exit 1 ;; esac\n",
                which("bash").display()
            ),
        );
    }

    /// Makes `id` say that this is another user: `-u` answers `number` and `-un` answers `name`.
    fn id(&self, number: &str, name: &str) {
        let path = self.dir.join("bin/id");
        // The name is a link to the real program; the stand-in takes its place, it is not written through it.
        fs::remove_file(&path).expect("the link to id");
        executable(
            &path,
            &format!(
                "#!{}\ncase $1 in -u) echo {number} ;; -un) echo {name} ;; *) exit 1 ;; esac\n",
                which("bash").display()
            ),
        );
    }

    /// The user's runtime directory where the system keeps it, for a shell with no `XDG_RUNTIME_DIR`.
    fn make_runtime_dir(&self) {
        fs::create_dir_all(self.runtime_dir()).expect("the runtime directory");
        fs::set_permissions(self.runtime_dir(), fs::Permissions::from_mode(0o700)).expect("0700");
    }

    /// How many times the installer ran `arca` with arguments that start with `prefix`.
    fn arca_calls(&self, prefix: &str) -> usize {
        self.log("arca")
            .iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    }

    /// What `getconf GNU_LIBC_VERSION` answers; nothing, and a failure, when `answer` is empty.
    fn glibc(&self, answer: &str) {
        let body = if answer.is_empty() {
            "exit 1".to_string()
        } else {
            format!("echo '{answer}'")
        };
        executable(
            &self.dir.join("bin/getconf"),
            &format!("#!{}\n{body}\n", which("bash").display()),
        );
    }

    /// Publishes `version`: its program, and a `SHA256SUMS` that also lists a file the installer does not download.
    fn release(&self, version: &str) {
        let asset = format!("arca-{version}-linux-x86_64");
        let text = program(version);
        let sums = format!(
            "{}  install.sh\n{}  {asset}\n",
            sha256_hex(b"another file of the release"),
            sha256_hex(text.as_bytes())
        );
        self.release_with(version, &text, &sums);
    }

    fn release_with(&self, version: &str, program: &str, sums: &str) {
        let dir = self.dir.join("fake/release").join(format!("v{version}"));
        fs::create_dir_all(&dir).expect("the release");
        fs::write(dir.join(format!("arca-{version}-linux-x86_64")), program).expect("the asset");
        fs::write(dir.join("SHA256SUMS"), sums).expect("the checksums");
    }

    /// The machine's listening TCP sockets, as the kernel's two tables list them: `v4` ports on 127.0.0.1 and `v6`
    /// ports on the IPv6 wildcard. A socket that is connected, not listening, on the door's first port is listed too:
    /// it takes no port from a listener.
    fn listening(&self, v4: &[u16], v6: &[u16]) {
        let mut tcp = String::from(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        );
        tcp.push_str(
            "   0: 0100007F:1D2F 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 11 1 0 20 4 30 10 -1\n",
        );
        for (row, port) in v4.iter().enumerate() {
            tcp.push_str(&format!(
                "{:4}: 0100007F:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12 1 0 100 0 0 10 0\n",
                row + 1
            ));
        }
        let mut tcp6 = String::from(
            "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        );
        for (row, port) in v6.iter().enumerate() {
            tcp6.push_str(&format!(
                "{row:4}: 00000000000000000000000000000000:{port:04X} 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 13 1 0 100 0 0 10 0\n"
            ));
        }
        fs::write(self.dir.join("fake/tcp"), tcp).expect("the IPv4 table");
        fs::write(self.dir.join("fake/tcp6"), tcp6).expect("the IPv6 table");
    }

    /// Makes or removes one of the stand-ins' switches, a file under `fake/`.
    fn switch(&self, name: &str, on: bool) {
        let path = self.dir.join("fake").join(name);
        if on {
            fs::write(path, "").expect("a switch");
        } else {
            let _ = fs::remove_file(path);
        }
    }

    fn home(&self, path: &str) -> PathBuf {
        self.dir.join("home").join(path)
    }

    fn runtime_dir(&self) -> PathBuf {
        if self.session {
            self.dir.join("run")
        } else {
            self.dir.join("run-root").join(uid())
        }
    }

    fn command(&self, shell: &str) -> Command {
        let mut command = Command::new(which(shell));
        command
            .env_clear()
            .current_dir(self.dir.join("cwd"))
            .env("PATH", self.dir.join("bin"))
            .env("HOME", self.dir.join("home"))
            .env("TMPDIR", self.dir.join("tmp"))
            .env("INCUS_SOCKET", self.dir.join("incus.socket"))
            .env("ARCA_INSTALL_WAIT", "1")
            .env("ARCA_INSTALL_RUN_ROOT", self.dir.join("run-root"))
            .env(
                "ARCA_INSTALL_TCP_TABLES",
                format!(
                    "{} {}",
                    self.dir.join("fake/tcp").display(),
                    self.dir.join("fake/tcp6").display()
                ),
            )
            .env("FAKE_DIR", self.dir.join("fake"))
            .env("FAKE_PATH", std::env::var_os("PATH").expect("PATH"))
            .env("FAKE_ARCA_SECRET", TOKEN);
        if self.session {
            command.env("XDG_RUNTIME_DIR", self.dir.join("run"));
        }
        command
    }

    fn finish(mut command: Command) -> Run {
        let output = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("the installer runs");
        Run {
            code: output.status.code(),
            out: String::from_utf8_lossy(&output.stdout).to_string(),
            err: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }

    /// `sh install.sh <arguments>`, with a pipe on its standard input.
    fn run(&self, arguments: &[&str]) -> Run {
        let mut command = self.command("sh");
        command.arg(script()).args(arguments).stdin(Stdio::piped());
        Machine::finish(command)
    }

    /// `<shell> -s -- <arguments>` with the script on its standard input, as `curl ... | sh` gives it.
    fn piped(&self, shell: &str, arguments: &[&str]) -> Run {
        let mut command = self.command(shell);
        command
            .arg("-s")
            .arg("--")
            .args(arguments)
            .stdin(fs::File::open(script()).expect("the script"));
        Machine::finish(command)
    }

    fn log(&self, tool: &str) -> Vec<String> {
        fs::read_to_string(self.dir.join(format!("fake/{tool}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// What the installer asked `systemctl --user` to do, without the questions it asks first.
    fn service_calls(&self) -> Vec<String> {
        self.log("systemctl")
            .into_iter()
            .filter(|line| !line.contains("show-environment") && !line.contains("is-active"))
            .collect()
    }

    /// Every file and directory under the home, with its mode and, for a file, its bytes.
    fn home_tree(&self) -> Tree {
        fn walk(root: &Path, dir: &Path, out: &mut Tree) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let name = path
                    .strip_prefix(root)
                    .expect("under the home")
                    .display()
                    .to_string();
                let meta = fs::symlink_metadata(&path).expect("metadata");
                let bits = meta.permissions().mode() & 0o7777;
                if meta.is_dir() {
                    out.insert(name, (bits, None));
                    walk(root, &path, out);
                } else {
                    out.insert(name, (bits, fs::read(&path).ok()));
                }
            }
        }
        let mut out = BTreeMap::new();
        let root = self.dir.join("home");
        walk(&root, &root, &mut out);
        out
    }

    /// The names under the home, for a message.
    fn home_names(&self) -> Vec<String> {
        self.home_tree().into_keys().collect()
    }

    /// Nothing was written where the installer may not write: the directory it was started in and the system's
    /// temporary directory are empty, and no call left its standard input open to a stand-in.
    fn wrote_only_where_it_may(&self) {
        for sub in ["cwd", "tmp"] {
            let left: Vec<_> = fs::read_dir(self.dir.join(sub))
                .expect("scratch")
                .flatten()
                .map(|entry| entry.path())
                .collect();
            assert!(left.is_empty(), "the installer wrote {left:?}");
        }
        assert_eq!(
            self.log("stdin-read"),
            Vec::<String>::new(),
            "a program the installer started could read the installer's standard input"
        );
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The config of the quickstart (`docs/QUICKSTART.md`), for this machine.
/// The config the script writes: with the HTTP door on `door`, or with none.
fn quickstart_config(machine: &Machine, door: Option<u16>) -> Value {
    let mut config = json!({
        "stateDir": machine.home(".local/state/arca-node"),
        "incus": { "socket": machine.dir.join("incus.socket") },
        "listen": { "unix": machine.runtime_dir().join("arca.sock") },
        "tokens": { "hashDir": machine.home(".config/arca/tokens") },
        "kernel": { "droiddProjects": [] }
    });
    if let Some(port) = door {
        config["listen"]["loopback"] = json!(format!("127.0.0.1:{port}"));
    }
    config
}

fn config_on_disk(machine: &Machine) -> Value {
    serde_json::from_str(
        &fs::read_to_string(machine.home(".config/arca/node.json")).expect("the config"),
    )
    .expect("the config is JSON")
}

/// The report of a node whose door is on `port`: where it is, the one line for Claude Code, which reads the token
/// from its file, the entry for any other client, which holds no token, and what the client then keeps.
fn says_the_door(machine: &Machine, run: &Run, port: u16) {
    let secret = machine.home(".config/arca/assistant-1.secret");
    let url = format!("http://127.0.0.1:{port}/mcp");
    run.says(&format!("  door      {url}, from this machine only"))
        .says(&format!(
            "claude mcp add --transport http arca {url} --header \"Authorization: Bearer $(cat '{}')\"",
            secret.display()
        ))
        // One line says where the cautions are; the report itself carries none of them. They are the program's to
        // say when asked (`arca status --security`), and the guide's.
        .says(&format!(
            "Before you share this machine or that token with anyone, read: {} status --security --config {}",
            machine.home(".local/bin/arca").display(),
            machine.home(".config/arca/node.json").display()
        ))
        .never_says("in clear text")
        .never_says("arguments for a moment")
        .never_says("not the name localhost")
        .never_says("another account can take")
        .never_says("listen.tailnet");
    let entry = run
        .out
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("{ \"mcpServers\""))
        .unwrap_or_else(|| panic!("the entry for other clients:\n{}", run.all()));
    let entry: Value = serde_json::from_str(entry).expect("the entry is JSON");
    assert_eq!(
        entry,
        json!({ "mcpServers": { "arca": {
            "type": "http",
            "url": url,
            "headers": { "Authorization": "Bearer <token>" }
        } } })
    );
}

/// What a finished install leaves, whichever way the script was started.
fn is_the_quickstarts_node(machine: &Machine, run: &Run, version: &str) {
    run.ok();
    let bin = machine.home(".local/bin/arca");
    assert_eq!(
        fs::read_to_string(&bin).expect("the program"),
        program(version)
    );
    assert_eq!(mode(&bin), 0o755);
    assert_eq!(
        fs::read_dir(machine.home(".local/bin"))
            .expect("bin")
            .count(),
        1,
        "the download's directory was left behind: {:?}",
        machine.home_names()
    );

    // The door is on its first port: nothing listens on this machine.
    let door = has_door(version).then_some(7471);
    assert_eq!(config_on_disk(machine), quickstart_config(machine, door));
    assert_eq!(mode(&machine.home(".config/arca/tokens")), 0o700);
    assert_eq!(mode(&machine.home(".local/state/arca-node")), 0o700);
    // What the script makes is this user's alone, but for the program and the unit, whose modes it sets.
    for private in [
        ".config/arca/node.json",
        ".config/arca",
        ".local/state/arca-install",
    ] {
        assert_eq!(mode(&machine.home(private)) & 0o077, 0, "{private}");
    }
    assert_eq!(
        mode(&machine.home(".config/systemd/user/arca.service")),
        0o644
    );

    let secret = machine.home(".config/arca/assistant-1.secret");
    assert_eq!(
        fs::read_to_string(&secret).expect("the token"),
        format!("{TOKEN}\n")
    );
    assert_eq!(
        mode(&secret) & 0o077,
        0,
        "the token file is this user's only"
    );
    assert!(
        machine
            .home(".config/arca/tokens/assistant-1.hash")
            .exists()
    );

    let unit =
        fs::read_to_string(machine.home(".config/systemd/user/arca.service")).expect("the unit");
    assert!(
        unit.contains(&format!(
            "ExecStart={} serve --config {}\n",
            bin.display(),
            machine.home(".config/arca/node.json").display()
        )),
        "{unit}"
    );
    assert!(unit.contains("WantedBy=default.target"), "{unit}");

    run.says("node      mode watch-only")
        .says(&format!("version {version}"))
        .says(&secret.display().to_string())
        .says(&format!(
            "{} call arca_status --config {} --token-file {}",
            bin.display(),
            machine.home(".config/arca/node.json").display(),
            secret.display()
        ));
    match door {
        Some(port) => says_the_door(machine, run, port),
        None => {
            run.says("door      none: this program is older than")
                .says("listen.tailnet")
                .says("Authorization: Bearer <the content of")
                .never_says("claude mcp add");
        }
    }
    assert!(
        !run.all().contains(TOKEN),
        "the token was printed:\n{}",
        run.all()
    );
    assert!(
        !machine
            .log("arca")
            .iter()
            .any(|line| line.starts_with("serve")),
        "the installer started the node itself: {:?}",
        machine.log("arca")
    );
    machine.wrote_only_where_it_may();
}

#[test]
fn a_clean_install_leaves_the_quickstarts_node_as_a_service_and_never_shows_the_token() {
    let machine = Machine::new();
    let version = current();
    let run = machine.run(&[]);
    is_the_quickstarts_node(&machine, &run, &version);

    // The one named release, and nothing else from the network: never the "latest release" address.
    let base = format!("https://github.com/automagik-si/arca/releases/download/v{version}");
    let downloads: Vec<String> = machine
        .log("curl")
        .into_iter()
        .filter(|line| !line.contains("--unix-socket"))
        .map(|line| line.rsplit(' ').next().expect("an address").to_string())
        .collect();
    assert_eq!(
        downloads,
        vec![
            format!("{base}/arca-{version}-linux-x86_64"),
            format!("{base}/SHA256SUMS")
        ]
    );
    assert!(
        machine
            .log("curl")
            .iter()
            .all(|line| !line.contains("latest"))
    );

    // Each download fails on an HTTP error, takes https only, and cannot hang: the flags as curl was given them.
    for line in machine
        .log("curl")
        .iter()
        .filter(|line| !line.contains("--unix-socket"))
    {
        let flags: Vec<&str> = line.split(' ').collect();
        let short = |letter: char| {
            flags.iter().any(|flag| {
                flag.starts_with('-') && !flag.starts_with("--") && flag.contains(letter)
            })
        };
        assert!(short('f'), "no -f: {line}");
        assert!(
            line.contains(" --proto =https "),
            "no --proto '=https': {line}"
        );
        assert!(
            line.contains(" --connect-timeout ") && line.contains(" --max-time "),
            "{line}"
        );
    }

    // One token, minted before the node starts; the unit written, enabled and started, in that order.
    let arca = machine.log("arca");
    assert_eq!(
        arca.iter()
            .filter(|line| line.starts_with("token mint"))
            .count(),
        1
    );
    assert!(
        arca.iter()
            .any(|line| line.starts_with("token mint --scope viewer --id assistant-1 "))
    );
    assert_eq!(
        machine.service_calls(),
        vec![
            "--user daemon-reload",
            "--user enable arca.service",
            "--user restart arca.service"
        ]
    );
    run.says("written now")
        .says("minted now, scope viewer")
        .says("enabled and started");

    // Everything it wrote is in the home; the runtime directory holds only what the node itself puts there.
    let names = machine.home_names();
    let expected = [
        ".config",
        ".config/arca",
        ".config/arca/assistant-1.secret",
        ".config/arca/node.json",
        ".config/arca/tokens",
        ".config/arca/tokens/assistant-1.hash",
        ".config/systemd",
        ".config/systemd/user",
        ".config/systemd/user/arca.service",
        ".config/systemd/user/default.target.wants",
        ".config/systemd/user/default.target.wants/arca.service",
        ".local",
        ".local/bin",
        ".local/bin/arca",
        ".local/state",
        ".local/state/arca-install",
        ".local/state/arca-install/program.sha256",
        ".local/state/arca-node",
    ];
    assert_eq!(names, expected);
    assert_eq!(
        fs::read_dir(machine.dir.join("run")).expect("run").count(),
        0
    );
}

#[test]
fn a_second_run_upgrades_the_program_in_place_and_restarts_the_service() {
    let machine = Machine::new();
    machine.run(&[]).ok();
    let before = machine.home_tree();

    // The same release again: the program is put in place again, and nothing else changes.
    let again = machine.run(&[]);
    again.ok().says("installed again").says("kept as it was");
    assert_eq!(machine.home_tree(), before);

    // A newer release, named with or without the tag's `v`.
    machine.release(NEXT);
    let upgrade = machine.run(&["--version", &format!("v{NEXT}")]);
    is_the_quickstarts_node(&machine, &upgrade, NEXT);
    upgrade
        .says(&format!("replaced: Automagik Arca {NEXT}"))
        .says(&format!("it was: Automagik Arca {}", current()));
    let mut after = machine.home_tree();
    let mut expected = before.clone();
    let mut changes = vec![
        ".local/bin/arca",
        ".local/state/arca-install/program.sha256",
    ];
    // The script's own config follows the program across the release that brought the door (the helper above has
    // checked which config is there now); between two programs on the same side of it, the config stays.
    if has_door(&current()) != has_door(NEXT) {
        changes.push(".config/arca/node.json");
    }
    for changed in changes {
        after.remove(changed);
        expected.remove(changed);
    }
    assert_eq!(after, expected, "only the program changed");

    // One token in three runs, and the service started again each time.
    assert_eq!(
        machine
            .log("arca")
            .iter()
            .filter(|line| line.starts_with("token mint"))
            .count(),
        1
    );
    let restarts = machine
        .service_calls()
        .iter()
        .filter(|line| *line == "--user restart arca.service")
        .count();
    assert_eq!(restarts, 3);

    // `--version=<v>` is the same flag.
    machine
        .run(&[&format!("--version={}", current())])
        .ok()
        .says("replaced");
    machine
        .run(&["--version"])
        .refused()
        .says("--version needs a value");
    machine
        .run(&["--version", "1.0 beta"])
        .refused()
        .says("is not a release version");
    machine
        .run(&["--no-such-flag"])
        .refused()
        .says("unknown argument");
}

#[test]
fn a_config_this_script_did_not_write_is_not_changed_and_no_node_is_started_for_it() {
    let machine = Machine::new();
    fs::create_dir_all(machine.home(".config/arca")).expect("the config directory");
    let config = machine.home(".config/arca/node.json");
    let text =
        "{ \"stateDir\": \"/somewhere/else\",\n  \"kernel\": { \"projects\": [\"mine\"] } }\n";
    fs::write(&config, text).expect("a config");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).expect("0640");
    let written = fs::metadata(&config)
        .expect("metadata")
        .modified()
        .expect("a time");
    let start = format!(
        "  {} serve --config {}\n",
        machine.home(".local/bin/arca").display(),
        config.display()
    );
    let untouched = |run: &Run| {
        assert_eq!(fs::read_to_string(&config).expect("the config"), text);
        assert_eq!(mode(&config), 0o640);
        assert_eq!(
            fs::metadata(&config)
                .expect("metadata")
                .modified()
                .expect("a time"),
            written
        );
        // No service for it, and no word that the node only reads: the script cannot know what this config does.
        assert!(!machine.home(".config/systemd").exists(), "{}", run.all());
        assert_eq!(machine.log("systemctl"), Vec::<String>::new());
        run.never_says("read-only node")
            .never_says("enabled and started")
            .never_says(TOKEN);
        assert_eq!(machine.arca_calls("serve"), 0);
        machine.wrote_only_where_it_may();
    };

    // No token: the node cannot be asked what it is, so nothing is started, nothing is waited for, and the script
    // ends with 3 and the one command.
    let run = machine.run(&[]);
    run.not_started()
        .says("kept as it was: this script changes nothing in a config it did not write")
        .says("this script mints no token for a config that exists")
        .says("No node was started or restarted")
        .says("it starts no node it cannot ask")
        .says(&start)
        .says("not asked: there is no token to ask it with");
    untouched(&run);
    assert_eq!(
        machine.arca_calls("call"),
        0,
        "the node was asked, or waited for, with no token"
    );
    assert_eq!(machine.arca_calls("token"), 0);
    // Nothing beside the config either: no token, no token directory, no state directory.
    let beside: Vec<String> = machine
        .home_names()
        .into_iter()
        .filter(|name| name.starts_with(".config"))
        .collect();
    assert_eq!(
        beside,
        [".config", ".config/arca", ".config/arca/node.json"]
    );
    assert!(!machine.home(".local/state/arca-node").exists());
    // The program is still this script's to install.
    assert_eq!(
        fs::read_to_string(machine.home(".local/bin/arca")).expect("the program"),
        program(&current())
    );

    // A token under the script's own name, and no node running: one question, no waiting, still nothing started.
    let secret = machine.home(".config/arca/assistant-1.secret");
    fs::write(&secret, format!("{TOKEN}\n")).expect("a token");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).expect("0600");
    let run = machine.run(&[]);
    run.not_started()
        .says("none written: this script starts no node for a config it did not write")
        .says("No node was started: this script did not write")
        .says(&start)
        .says("it has not answered `arca_status`");
    untouched(&run);
    assert_eq!(machine.arca_calls("call"), 1, "one question, not a wait");

    // The user's own node runs, and it is one that writes: the script reports it as it is and starts nothing.
    machine.switch("node-running", true);
    fs::write(machine.dir.join("fake/node-mode"), "kernel\n").expect("the mode");
    let run = machine.run(&[]);
    run.ok()
        .says("none written: a node already answers for this config, and this script did not start it")
        .says("node      mode kernel")
        .says("This node is not read-only")
        .says("This script started nothing and stopped nothing.")
        .never_says("stop that node and run this script again");
    untouched(&run);
}

#[test]
fn a_program_with_the_door_gets_it_on_loopback_and_the_report_gives_the_connect_lines() {
    let version = door_since();
    for shell in ["sh", "bash"] {
        let machine = Machine::new();
        machine.release(&version);
        // From a pipe, as `curl ... | sh` runs it: the door's steps read nothing from it either.
        let run = machine.piped(shell, &["--version", &version]);
        is_the_quickstarts_node(&machine, &run, &version);
        says_the_door(&machine, &run, 7471);
        run.never_says("was in use")
            .never_says("This script started no node");
        assert!(
            !machine.dir.join("fake/stdin-read.log").exists(),
            "something the script started could read its standard input"
        );
        // The door is loopback and nothing else: the config names one address, and it is 127.0.0.1.
        assert_eq!(
            config_on_disk(&machine)["listen"],
            json!({
                "unix": machine.runtime_dir().join("arca.sock"),
                "loopback": "127.0.0.1:7471"
            })
        );

        // A second run keeps the config and its port, though the node itself now listens there.
        machine.listening(&[7471], &[]);
        let before = machine.home_tree();
        let again = machine.piped(shell, &["--version", &version]);
        again
            .ok()
            .says("kept as it was: the config this script writes, from an earlier run");
        says_the_door(&machine, &again, 7471);
        again.never_says("was in use");
        assert_eq!(machine.home_tree(), before);
    }
}

#[test]
fn the_door_takes_the_next_free_port_and_says_so_or_stays_shut_when_none_is_free() {
    let version = door_since();

    // 7471 is taken on IPv4 and 7472 on the IPv6 wildcard, which holds the IPv4 port too: 7473 is the door.
    let machine = Machine::new();
    machine.release(&version);
    machine.listening(&[7471], &[7472]);
    let run = machine.run(&["--version", &version]);
    run.ok();
    assert_eq!(
        config_on_disk(&machine),
        quickstart_config(&machine, Some(7473))
    );
    says_the_door(&machine, &run, 7473);
    run.says("port 7471 was in use, so the next free one was taken")
        .never_says(TOKEN);
    // The next run finds its own config on that port and keeps it.
    let again = machine.run(&["--version", &version]);
    again.ok().says("kept as it was");
    says_the_door(&machine, &again, 7473);
    assert_eq!(
        config_on_disk(&machine),
        quickstart_config(&machine, Some(7473))
    );

    // Every port of the script's list is taken: the node is installed without a door, and the report says why.
    let machine = Machine::new();
    machine.release(&version);
    let ports: Vec<u16> = constant("DOOR_PORTS")
        .split_whitespace()
        .map(|port| port.parse().expect("a port"))
        .collect();
    assert_eq!(ports.first(), Some(&7471));
    assert!(ports.len() >= 2 && ports.windows(2).all(|pair| pair[1] == pair[0] + 1));
    machine.listening(&ports, &[]);
    let run = machine.run(&["--version", &version]);
    run.ok()
        .says(&format!(
            "door      none: something listens on every port from 7471 to {}",
            ports.last().expect("a port")
        ))
        .says("node      mode watch-only")
        .never_says("claude mcp add");
    assert_eq!(config_on_disk(&machine), quickstart_config(&machine, None));
}

#[test]
fn the_scripts_own_earlier_config_gains_the_door_and_loses_it_under_an_older_program() {
    let version = door_since();
    let machine = Machine::new();
    machine.release(BEFORE_DOOR);
    machine.release(&version);
    assert!(!has_door(BEFORE_DOOR) && has_door(&version));

    // An install from before the door: the config every earlier release of the script wrote, letter for letter.
    let first = machine.run(&["--version", BEFORE_DOOR]);
    is_the_quickstarts_node(&machine, &first, BEFORE_DOOR);
    let config = machine.home(".config/arca/node.json");
    let earlier = fs::read_to_string(&config).expect("the config");
    assert_eq!(
        earlier,
        format!(
            "{{\n  \"stateDir\": \"{}\",\n  \"incus\": {{ \"socket\": \"{}\" }},\n  \"listen\": {{ \"unix\": \"{}\" }},\n  \"tokens\": {{ \"hashDir\": \"{}\" }},\n  \"kernel\": {{ \"droiddProjects\": [] }}\n}}\n",
            machine.home(".local/state/arca-node").display(),
            machine.dir.join("incus.socket").display(),
            machine.runtime_dir().join("arca.sock").display(),
            machine.home(".config/arca/tokens").display(),
        )
    );
    let token = fs::read(machine.home(".config/arca/assistant-1.secret")).expect("the token");

    // The upgrade: the same config with the door, the same token, and the service on the new program.
    let upgrade = machine.run(&["--version", &version]);
    is_the_quickstarts_node(&machine, &upgrade, &version);
    upgrade
        .says("written again: it was this script's own, from before the HTTP door; the door is all that was added")
        .says("enabled and started");
    assert_eq!(
        mode(&config) & 0o077,
        0,
        "the config is still this user's alone"
    );
    assert_eq!(
        fs::read(machine.home(".config/arca/assistant-1.secret")).expect("the token"),
        token
    );
    assert_eq!(machine.arca_calls("token"), 1, "one token in two runs");
    let restarts = machine
        .service_calls()
        .iter()
        .filter(|line| *line == "--user restart arca.service")
        .count();
    assert_eq!(restarts, 2, "the node was started again on the new config");

    // Back to the older program, which refuses a config that names the door: the earlier text, letter for letter.
    let back = machine.run(&["--version", BEFORE_DOOR]);
    is_the_quickstarts_node(&machine, &back, BEFORE_DOOR);
    back.says("written again without the HTTP door");
    assert_eq!(fs::read_to_string(&config).expect("the config"), earlier);

    // A config of the user's own is neither: one changed letter, and the script leaves it alone.
    let machine = Machine::new();
    machine.release(BEFORE_DOOR);
    machine.release(&version);
    machine.run(&["--version", BEFORE_DOOR]).ok();
    let config = machine.home(".config/arca/node.json");
    let own = fs::read_to_string(&config)
        .expect("the config")
        .replace("\"droiddProjects\": []", "\"droiddProjects\": [ ]");
    fs::write(&config, &own).expect("the user's config");
    let run = machine.run(&["--version", &version]);
    run.ok()
        .says("kept as it was: this script changes nothing in a config it did not write")
        .never_says("door      ")
        .never_says("claude mcp add")
        .says("listen.loopback");
    assert_eq!(fs::read_to_string(&config).expect("the config"), own);
}

/// The config with the owner's no: the key is there, and it says null.
fn config_without_a_door(machine: &Machine) -> Value {
    let mut config = quickstart_config(machine, None);
    config["listen"]["loopback"] = Value::Null;
    config
}

#[test]
fn no_door_is_a_choice_the_config_states_and_no_later_run_or_upgrade_undoes() {
    let version = door_since();
    let later = "99.0.0";
    assert!(has_door(later));
    let machine = Machine::new();
    machine.release(&version);
    machine.release(later);

    // --no-door on a machine that never had Arca: the node runs, and its config says there is no door.
    let run = machine.run(&["--version", &version, "--no-door"]);
    run.ok()
        .says("node      mode watch-only")
        .says("door      none: the config says so (\"loopback\": null). To open it: sh install.sh --door")
        .never_says("claude mcp add")
        .never_says("http://127.0.0.1:");
    assert_eq!(config_on_disk(&machine), config_without_a_door(&machine));
    let said_no = fs::read_to_string(machine.home(".config/arca/node.json")).expect("the config");
    assert!(said_no.contains("\"loopback\": null"), "{said_no}");

    // A run with no flag, and an upgrade with no flag, keep the no: the config is not touched.
    for release in [version.as_str(), later] {
        let run = machine.run(&["--version", release]);
        run.ok()
            .says("kept as it was: the config this script writes, from an earlier run")
            .says("door      none: the config says so")
            .never_says("written again")
            .never_says("claude mcp add");
        assert_eq!(
            fs::read_to_string(machine.home(".config/arca/node.json")).expect("the config"),
            said_no
        );
    }

    // --door is the owner's yes: the door opens, and stays open on the runs after it.
    let run = machine.run(&["--version", later, "--door"]);
    run.ok()
        .says("written again with the HTTP door: --door asked for it");
    says_the_door(&machine, &run, 7471);
    assert_eq!(
        config_on_disk(&machine),
        quickstart_config(&machine, Some(7471))
    );
    let run = machine.run(&["--version", later]);
    run.ok().says("kept as it was");
    says_the_door(&machine, &run, 7471);

    // --no-door on a node that has the door: it is shut, in the config's own words.
    let run = machine.run(&["--version", later, "--no-door"]);
    run.ok()
        .says("written again with no HTTP door (\"loopback\": null): --no-door asked for none")
        .says("enabled and started")
        .never_says("claude mcp add");
    assert_eq!(config_on_disk(&machine), config_without_a_door(&machine));

    // The two flags together are refused before anything is changed.
    let before = machine.home_tree();
    machine
        .run(&["--door", "--no-door"])
        .refused()
        .says("--door and --no-door are not both given.");
    assert_eq!(machine.home_tree(), before);
}

#[test]
fn a_door_the_owner_took_out_by_hand_is_not_put_back() {
    let version = door_since();
    let later = "99.0.0";
    for by_hand in ["the key removed", "the key set to null"] {
        let machine = Machine::new();
        machine.release(&version);
        machine.release(later);
        let first = machine.run(&["--version", &version]);
        first.ok();
        says_the_door(&machine, &first, 7471);

        // The owner edits the config this script wrote: the key goes, or says null. Either way the text is one this
        // script could have written itself, which is exactly why it must not read it as "from before the door".
        let config = machine.home(".config/arca/node.json");
        let with_door = fs::read_to_string(&config).expect("the config");
        let edited = if by_hand == "the key removed" {
            with_door.replace(", \"loopback\": \"127.0.0.1:7471\"", "")
        } else {
            with_door.replace("\"loopback\": \"127.0.0.1:7471\"", "\"loopback\": null")
        };
        assert_ne!(edited, with_door);
        fs::write(&config, &edited).expect("the edited config");

        // The same release again, and an upgrade: the program in place already had the door, so neither run is the
        // upgrade that brings it, and the config stays as the owner left it.
        for release in [version.as_str(), later] {
            let run = machine.run(&["--version", release]);
            run.ok()
                .says("kept as it was: the config this script writes, from an earlier run")
                .says("door      none: the config")
                .says("sh install.sh --door")
                .never_says("written again")
                .never_says("claude mcp add");
            assert_eq!(
                fs::read_to_string(&config).expect("the config"),
                edited,
                "{by_hand}, then {release}"
            );
        }
    }
}

#[test]
fn a_no_written_by_hand_survives_a_downgrade_to_a_program_from_before_the_door_and_the_upgrade_back()
 {
    let version = door_since();
    for by_hand in ["the key removed", "the key set to null"] {
        let machine = Machine::new();
        machine.release(BEFORE_DOOR);
        machine.release(&version);
        machine.run(&["--version", &version]).ok();
        let config = machine.home(".config/arca/node.json");
        let with_door = fs::read_to_string(&config).expect("the config");
        let edited = if by_hand == "the key removed" {
            with_door.replace(", \"loopback\": \"127.0.0.1:7471\"", "")
        } else {
            with_door.replace("\"loopback\": \"127.0.0.1:7471\"", "\"loopback\": null")
        };
        assert_ne!(edited, with_door);
        fs::write(&config, &edited).expect("the edited config");

        // The older program refuses a config that names the key, so a null is dropped from it. The no is not: it
        // goes into the script's note, as --no-door's does.
        machine.run(&["--version", BEFORE_DOOR]).ok();
        assert_eq!(
            config_on_disk(&machine),
            quickstart_config(&machine, None),
            "{by_hand}"
        );
        assert!(
            machine.home(".local/state/arca-install/no-door").exists(),
            "{by_hand}: the no is kept in the note"
        );

        // Back on a program with the door: the config says the no again, and no door is opened.
        let run = machine.run(&["--version", &version]);
        run.ok()
            .says("door      none: the config says so")
            .never_says("claude mcp add")
            .never_says("the door is all that was added");
        assert_eq!(
            config_on_disk(&machine),
            config_without_a_door(&machine),
            "{by_hand}"
        );
    }

    // A door that was open makes the same trip and comes back open: a downgrade is not a no.
    let machine = Machine::new();
    machine.release(BEFORE_DOOR);
    machine.release(&version);
    machine.run(&["--version", &version]).ok();
    machine.run(&["--version", BEFORE_DOOR]).ok();
    assert!(!machine.home(".local/state/arca-install/no-door").exists());
    let run = machine.run(&["--version", &version]);
    run.ok();
    says_the_door(&machine, &run, 7471);
}

#[test]
fn a_no_said_under_a_program_from_before_the_door_is_kept_by_the_upgrade_that_brings_it() {
    let version = door_since();
    let machine = Machine::new();
    machine.release(BEFORE_DOOR);
    machine.release(&version);

    // The older program refuses a config that names the key, so the config cannot say null yet: the no is kept in
    // the script's own note.
    let run = machine.run(&["--version", BEFORE_DOOR, "--no-door"]);
    run.ok().says("door      none: this program is older than");
    assert_eq!(config_on_disk(&machine), quickstart_config(&machine, None));
    assert!(machine.home(".local/state/arca-install/no-door").exists());

    // The upgrade that would add the door to this script's earlier config does not: the config now says the no.
    let run = machine.run(&["--version", &version]);
    run.ok()
        .says("door      none: the config says so")
        .never_says("claude mcp add")
        .never_says("the door is all that was added");
    assert_eq!(config_on_disk(&machine), config_without_a_door(&machine));

    // And --door takes the no back, in the note too.
    let run = machine.run(&["--version", &version, "--door"]);
    run.ok();
    says_the_door(&machine, &run, 7471);
    assert!(!machine.home(".local/state/arca-install/no-door").exists());
}

#[test]
fn a_flag_changes_nothing_in_a_config_the_script_did_not_write_and_a_closed_door_is_said() {
    let version = door_since();
    let machine = Machine::new();
    machine.release(&version);
    machine.run(&["--version", &version]).ok();

    // The node reports that another program holds its port: the report says the door is closed and why, and that
    // the token must not be sent there meanwhile. The node itself runs, so the script still ends with 0.
    machine.switch("node-door-closed", true);
    let run = machine.run(&["--version", &version]);
    run.ok()
        .says("door      CLOSED: the node could not take http://127.0.0.1:7471/mcp")
        .says("(address in use: another program holds this port). It runs without the door and keeps trying the port.")
        .says("The HTTP door is CLOSED: the node could not take port 7471 (address in use: another program holds this port).")
        .says("run this script again then, and it prints the line.")
        .says("call arca_status --config")
        .says("node      mode watch-only")
        // Nothing that would send the token to whatever holds the port: no line to run, no entry to paste, no URL
        // to copy out of one.
        .never_says("claude mcp add")
        .never_says("mcpServers")
        .never_says("Authorization: Bearer")
        .never_says(TOKEN);
    // Once the node has the port, the same run prints the line again.
    machine.switch("node-door-closed", false);
    let run = machine.run(&["--version", &version]);
    run.ok();
    says_the_door(&machine, &run, 7471);
    machine.switch("node-door-closed", true);
    machine
        .run(&["--version", &version])
        .ok()
        .never_says("claude mcp add");
    machine.switch("node-door-closed", false);

    // A config with one letter of the owner's own: neither flag touches it, and the report says so.
    let config = machine.home(".config/arca/node.json");
    let own = fs::read_to_string(&config)
        .expect("the config")
        .replace("\"droiddProjects\": []", "\"droiddProjects\": [ ]");
    fs::write(&config, &own).expect("the user's config");
    for flag in ["--no-door", "--door"] {
        let run = machine.run(&["--version", &version, flag]);
        run.ok()
            .says("kept as it was: this script changes nothing in a config it did not write")
            .says(&format!(
                "{flag} changed nothing: this config is not one this script wrote."
            ))
            .says("\"loopback\": null");
        assert_eq!(fs::read_to_string(&config).expect("the config"), own);
    }
}

#[test]
fn enable_changes_runs_the_programs_own_step_for_the_service_it_started_and_says_plainly_when_it_was_not_done()
 {
    let version = door_since();
    let init_calls = |machine: &Machine| -> Vec<String> {
        machine
            .log("arca")
            .into_iter()
            .filter(|line| line.starts_with("init"))
            .collect()
    };

    // Without the flag the step is never taken: the node only looks.
    let machine = Machine::new();
    machine.release(&version);
    let config = machine.home(".config/arca/node.json");
    machine.run(&["--version", &version]).ok();
    assert_eq!(init_calls(&machine), Vec::<String>::new());

    // With it: after the node runs as this script's service, the program's own step is run once, with the yes the
    // flag is and the config by name, and its words are the report's. The script reads no standard input for it.
    let run = machine.piped("sh", &["--version", &version, "--enable-changes"]);
    run.ok()
        .says("--enable-changes: `arca init` ran, and these are its words.")
        .says("    Arca may now make changes on this machine.")
        .says("    No passkey is trusted yet, so nothing can be approved until you enrol the first one.")
        .says(&format!("      arca setup-code --config {}", config.display()))
        .says(&format!("      arca init --watch-only --config {}", config.display()))
        .says("node      mode kernel")
        .says("This node may now make changes: --enable-changes asked for it. Its words above say what waits for you.")
        .never_says("its config, which this script did not write, says so")
        .never_says(TOKEN);
    assert_eq!(
        init_calls(&machine),
        vec![format!(
            "init --enable-changes --yes --config {}",
            config.display()
        )]
    );
    let log = machine.log("arca");
    let step = log
        .iter()
        .position(|line| line.starts_with("init"))
        .expect("the step");
    assert!(
        log[..step]
            .iter()
            .any(|line| line.starts_with("call arca_status")),
        "the node was asked before the step: {log:?}"
    );
    assert!(
        !machine.dir.join("fake/stdin-read.log").exists(),
        "something the script started could read its standard input"
    );

    // The step refuses: the script says it was not done, gives its words and the command, and does not end with 0.
    let machine = Machine::new();
    machine.release(&version);
    machine.switch("init-refuses", true);
    let run = machine.run(&["--version", &version, "--enable-changes"]);
    run.refused()
        .says("--enable-changes was NOT done: `arca init` stopped, and the node still only looks. Its words:")
        .says("    arca: init changed nothing: this config has no approval page and no loopback door for one")
        .says("To take the step yourself once that is settled:")
        .says("init --enable-changes --yes --config")
        .says("node      mode watch-only")
        .never_says("may now make changes");

    // A program from before the step: it is not run at all, and the script says why.
    let machine = Machine::new();
    machine.release(&version);
    machine.switch("program-without-init", true);
    let run = machine.run(&["--version", &version, "--enable-changes"]);
    run.refused()
        .says("--enable-changes was NOT done, and the node still only looks.")
        .says("This program has no `arca init`: it is from a release before that step.");
    assert_eq!(init_calls(&machine), Vec::<String>::new());

    // A config of the owner's own with no service of this script's: no node was started, so there is none to
    // change. The step is not run.
    let machine = Machine::new();
    machine.release(&version);
    fs::create_dir_all(machine.home(".config/arca")).expect("the config directory");
    fs::write(
        machine.home(".config/arca/node.json"),
        "{ \"stateDir\": \"/somewhere/else\" }\n",
    )
    .expect("a config");
    let run = machine.run(&["--version", &version, "--enable-changes"]);
    assert_ne!(run.code, Some(0), "{}", run.all());
    run.says("--enable-changes was NOT done, and the node still only looks.")
        .says("This script runs `arca init` only for the node it runs as a service, and it started none here.");
    assert_eq!(init_calls(&machine), Vec::<String>::new());
}

#[test]
fn the_connect_block_is_the_line_the_entry_and_one_pointer_and_a_closed_door_still_prints_neither()
{
    let version = door_since();
    let machine = Machine::new();
    machine.release(&version);
    let run = machine.run(&["--version", &version]);
    run.ok();
    says_the_door(&machine, &run, 7471);
    // The block a person reads to connect: its heading, the two lines to copy, their two lead-ins and the pointer.
    let block: Vec<&str> = run
        .out
        .lines()
        .skip_while(|line| !line.starts_with("  1. An assistant that takes an MCP server URL"))
        .take_while(|line| !line.trim().is_empty())
        .collect();
    assert!(
        (5..=8).contains(&block.len()),
        "the connect block is short: {} lines\n{}",
        block.len(),
        block.join("\n")
    );

    // A program from before `status --security`: the pointer names the guide, not a command that does not exist.
    let machine = Machine::new();
    machine.release(&version);
    machine.switch("program-without-security", true);
    let run = machine.run(&["--version", &version]);
    run.ok()
        .says("Before you share this machine or that token with anyone, read: https://github.com/automagik-si/arca/blob/dev/docs/OPERATING.md#loopback-listener")
        .never_says("status --security");

    // What did not move: with the door closed there is no line and no entry, and the report says why.
    let machine = Machine::new();
    machine.release(&version);
    machine.run(&["--version", &version]).ok();
    machine.switch("node-door-closed", true);
    let run = machine.run(&["--version", &version]);
    run.ok()
        .says("The HTTP door is CLOSED: the node could not take port 7471 (address in use: another program holds this port).")
        .never_says("claude mcp add")
        .never_says("mcpServers")
        .never_says("status --security");
}

#[test]
fn a_service_from_an_earlier_run_is_restarted_only_where_the_node_can_be_asked() {
    let restarts = |machine: &Machine| {
        machine
            .service_calls()
            .iter()
            .filter(|line| *line == "--user restart arca.service")
            .count()
    };

    // The user turned a listener on in the config this script wrote. The service is still this script's, so a
    // second run puts it on the new program, and the node's own answer is the proof.
    let machine = Machine::new();
    machine.run(&[]).ok();
    let config = machine.home(".config/arca/node.json");
    let mut edited: Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("the config")).expect("JSON");
    edited["listen"]["tailnet"] = json!("100.64.0.1:7777");
    let text = format!("{edited:#}\n");
    fs::write(&config, &text).expect("the edited config");
    machine.release(NEXT);
    let run = machine.run(&["--version", NEXT]);
    run.ok()
        .says("kept as it was: this script changes nothing in a config it did not write")
        .says("enabled and started")
        .says(&format!("node      mode watch-only, version {NEXT}"))
        .never_says("read-only node");
    assert_eq!(fs::read_to_string(&config).expect("the config"), text);
    assert_eq!(restarts(&machine), 2);

    // The token's file is gone: the node cannot be asked, so the service is neither rewritten nor restarted.
    fs::remove_file(machine.home(".config/arca/assistant-1.secret")).expect("the token");
    let unit = fs::read(machine.home(".config/systemd/user/arca.service")).expect("the unit");
    let calls = machine.log("systemctl").len();
    let asked = machine.arca_calls("call");
    let run = machine.run(&["--version", NEXT]);
    run.not_started()
        .says("No node was started or restarted")
        .says("A node that is running still uses the program it started with.");
    assert_eq!(
        machine.log("systemctl").len(),
        calls,
        "systemctl was called"
    );
    assert_eq!(machine.arca_calls("call"), asked);
    assert_eq!(
        machine.arca_calls("token"),
        1,
        "no second token for a config that exists"
    );
    assert_eq!(
        fs::read(machine.home(".config/systemd/user/arca.service")).expect("the unit"),
        unit
    );
    assert_eq!(fs::read_to_string(&config).expect("the config"), text);
}

#[test]
fn a_wrong_checksum_stops_before_anything_is_installed() {
    let version = current();
    let asset = format!("arca-{version}-linux-x86_64");
    let nothing_installed = |machine: &Machine, run: &Run| {
        run.refused().says("Nothing was installed.");
        assert_eq!(
            machine.home_names(),
            [".local", ".local/bin"],
            "the installer left something:\n{}",
            run.all()
        );
        assert_eq!(machine.service_calls(), Vec::<String>::new());
        assert!(
            machine.log("arca").is_empty(),
            "the download was run before it was checked: {:?}",
            machine.log("arca")
        );
        machine.wrote_only_where_it_may();
    };

    // The file is not the one the release's checksums name.
    let machine = Machine::new();
    let sums = format!("{}  {asset}\n", sha256_hex(b"another program"));
    machine.release_with(&version, &program(&version), &sums);
    let run = machine.run(&[]);
    nothing_installed(&machine, &run);
    run.says(&format!("the checksum of the downloaded {asset} is wrong"))
        .says(&sha256_hex(b"another program"))
        .says(&sha256_hex(program(&version).as_bytes()));

    // The checksums do not name this file at all.
    let machine = Machine::new();
    let sums = format!(
        "{}  some-other-file\n",
        sha256_hex(program(&version).as_bytes())
    );
    machine.release_with(&version, &program(&version), &sums);
    let run = machine.run(&[]);
    nothing_installed(&machine, &run);
    run.says(&format!("has no checksum for {asset}"));

    // A release that does not exist.
    let machine = Machine::new();
    let run = machine.run(&["--version", "9.9.9"]);
    nothing_installed(&machine, &run);
    run.says("could not download").says("v9.9.9");

    // A wrong checksum on a second run leaves the program that was there.
    let machine = Machine::new();
    machine.run(&[]).ok();
    let before = machine.home_tree();
    machine.release_with(
        NEXT,
        &program(NEXT),
        &format!("{}  arca-{NEXT}-linux-x86_64\n", "0".repeat(64)),
    );
    machine
        .run(&["--version", NEXT])
        .refused()
        .says("is wrong")
        .says("Nothing was installed.");
    assert_eq!(machine.home_tree(), before);
}

#[test]
fn no_incus_access_is_refused_with_its_reason_and_nothing_is_changed() {
    let nothing_changed = |machine: &Machine, run: &Run| {
        run.refused().says("nothing was changed");
        assert_eq!(machine.home_names(), Vec::<String>::new(), "{}", run.all());
        assert!(
            machine
                .log("curl")
                .iter()
                .all(|line| line.contains("--unix-socket")),
            "something was downloaded: {:?}",
            machine.log("curl")
        );
        assert_eq!(machine.log("systemctl"), Vec::<String>::new());
        machine.wrote_only_where_it_may();
    };
    let socket = |machine: &Machine| machine.dir.join("incus.socket");

    // No socket: Incus is not installed, or not running.
    let machine = Machine::new();
    fs::remove_file(socket(&machine)).expect("the socket");
    let run = machine.run(&[]);
    nothing_changed(&machine, &run);
    run.says(&format!(
        "there is no Incus socket at {}",
        socket(&machine).display()
    ))
    .says("Install Incus and start it")
    .says("INCUS_SOCKET=<path>");

    // A socket this user may not open. The superuser may open anything, so there is nothing to refuse for it.
    if uid() != "0" {
        let machine = Machine::new();
        fs::set_permissions(socket(&machine), fs::Permissions::from_mode(0o000)).expect("0000");
        let run = machine.run(&[]);
        nothing_changed(&machine, &run);
        run.says(&format!(
            "may not open the Incus socket {}",
            socket(&machine).display()
        ))
        .says("to the group that owns the socket")
        .says("log in again");
        assert!(machine.log("curl").is_empty());
    }

    // A socket nothing answers on.
    let machine = Machine::new();
    machine.switch("incus-down", true);
    let run = machine.run(&[]);
    nothing_changed(&machine, &run);
    run.says("nothing answers on it").says("Start Incus");

    // The script only reads Incus: one GET, and it never offers to repair the machine itself.
    let machine = Machine::new();
    machine.run(&[]).ok();
    let asked: Vec<String> = machine
        .log("curl")
        .into_iter()
        .filter(|line| line.contains("--unix-socket"))
        .collect();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].ends_with("http://incus/1.0"), "{asked:?}");
}

#[test]
fn a_shell_with_no_runtime_directory_uses_the_users_own_or_is_refused_with_the_reason() {
    // A coding agent's shell: no XDG_RUNTIME_DIR, and the user's runtime directory is where the system keeps it.
    let mut machine = Machine::new();
    machine.session = false;
    machine.make_runtime_dir();
    let run = machine.run(&[]);
    // The stand-in for systemctl answers only when XDG_RUNTIME_DIR reached it, as the real one does.
    is_the_quickstarts_node(&machine, &run, &current());
    run.says("This shell had no XDG_RUNTIME_DIR").says(&format!(
        "export XDG_RUNTIME_DIR={}",
        machine.runtime_dir().display()
    ));
    assert!(
        machine
            .service_calls()
            .contains(&"--user restart arca.service".to_string())
    );

    // No such directory: the user has no session, and the script says so and stops.
    let mut machine = Machine::new();
    machine.session = false;
    let run = machine.run(&[]);
    run.refused()
        .says(&format!(
            "XDG_RUNTIME_DIR is not set and {} does not exist",
            machine.runtime_dir().display()
        ))
        .says("Log in as this user")
        .says("nothing was changed");
    assert_eq!(machine.home_names(), Vec::<String>::new());
    assert_eq!(machine.log("systemctl"), Vec::<String>::new());

    // A runtime directory whose socket path a unix socket cannot hold.
    let machine = Machine::new();
    let long = machine.dir.join("run").join("d".repeat(100));
    fs::create_dir_all(&long).expect("a long directory");
    fs::set_permissions(&long, fs::Permissions::from_mode(0o700)).expect("0700");
    let mut command = machine.command("sh");
    command
        .arg(script())
        .env("XDG_RUNTIME_DIR", &long)
        .stdin(Stdio::piped());
    let run = Machine::finish(command);
    run.refused().says("is longer than 100 characters");
    assert_eq!(machine.home_names(), Vec::<String>::new());
}

#[test]
fn a_user_or_a_runtime_directory_that_is_not_the_users_own_is_refused() {
    let nothing_changed = |machine: &Machine, run: &Run| {
        run.refused().says("nothing was changed");
        assert_eq!(machine.home_names(), Vec::<String>::new(), "{}", run.all());
        assert!(
            machine
                .log("curl")
                .iter()
                .all(|line| line.contains("--unix-socket"))
        );
        assert_eq!(machine.log("systemctl"), Vec::<String>::new());
    };

    // A runtime directory that others may write: whoever else writes there could take the socket's place.
    for open in [0o770, 0o707, 0o777, 0o720] {
        let machine = Machine::new();
        fs::set_permissions(machine.dir.join("run"), fs::Permissions::from_mode(open))
            .expect("a mode");
        let run = machine.run(&[]);
        nothing_changed(&machine, &run);
        run.says(&format!(
            "the runtime directory {} is not this user's alone",
            machine.dir.join("run").display()
        ));
    }
    // A group or others that may only read it are no danger to the socket.
    let machine = Machine::new();
    fs::set_permissions(machine.dir.join("run"), fs::Permissions::from_mode(0o755)).expect("0755");
    machine.run(&[]).ok();

    // Root: the node belongs to an ordinary user.
    let machine = Machine::new();
    machine.id("0", "root");
    let run = machine.run(&[]);
    nothing_changed(&machine, &run);
    run.says("this is root, and Arca's node belongs to an ordinary user")
        .says("without sudo");

    // Another user in this user's home and runtime directory, as under `sudo -u` with the caller's HOME kept.
    let machine = Machine::new();
    machine.id("4242", "somebody");
    let run = machine.run(&[]);
    nothing_changed(&machine, &run);
    run.says(&format!(
        "the home directory {} belongs to another user, not to somebody",
        machine.dir.join("home").display()
    ))
    .says(&format!(
        "the runtime directory {} is not this user's alone",
        machine.dir.join("run").display()
    ));
}

#[test]
fn an_older_release_is_not_put_over_a_newer_program_unless_the_version_is_named() {
    let version = current();
    // Versions around the script's own, in the order of semantic versioning.
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core.to_string(), Some(pre.to_string())),
        None => (version.clone(), None),
    };
    let mut newer = vec!["999.0.0".to_string()];
    let mut older = vec!["0.0.1".to_string(), "0.0.0-alpha.1".to_string()];
    if core.split('.').count() == 3 {
        let (head, patch) = core.rsplit_once('.').expect("three numbers");
        let patch: u64 = patch.parse().expect("a number");
        newer.push(format!("{head}.{}", patch + 1));
        newer.push(format!("{head}.{}-alpha.1", patch + 1));
    }
    if let Some(pre) = &pre {
        // The release is newer than its own pre-release; `beta` is newer than `alpha`; 10 is newer than 9.
        newer.push(core.clone());
        newer.push(format!("{version}.1"));
        newer.push(format!("{core}-{}", pre.replace("alpha", "beta")));
        if let Some((word, number)) = pre.rsplit_once('.')
            && let Ok(number) = number.parse::<u64>()
        {
            let digits = number.to_string().len();
            newer.push(format!("{core}-{word}.{}", 10u64.pow(digits as u32)));
            newer.push(format!("{core}-{word}.{}", number + 1));
            if number > 0 {
                older.push(format!("{core}-{word}.{}", number - 1));
            }
            older.push(format!("{core}-{word}"));
        }
    } else {
        older.push(format!("{core}-rc.1"));
    }
    newer.retain(|other| *other != version);

    let with = |installed: &str| {
        let machine = Machine::new();
        fs::create_dir_all(machine.home(".local/bin")).expect("bin");
        executable(&machine.home(".local/bin/arca"), &program(installed));
        machine
    };

    for installed in &newer {
        let machine = with(installed);
        let before = machine.home_tree();
        machine
            .run(&[])
            .refused()
            .says(&format!("is {installed}, which is newer than {version}"))
            .says(&format!("sh install.sh --version {version}"))
            .says("nothing was changed");
        assert_eq!(machine.home_tree(), before, "{installed} was replaced");
        assert!(
            machine
                .log("curl")
                .iter()
                .all(|line| line.contains("--unix-socket"))
        );
    }
    for installed in older.iter().chain([&version]) {
        let machine = with(installed);
        let run = machine.run(&[]);
        is_the_quickstarts_node(&machine, &run, &version);
    }

    // Named, the older release goes over the newer program, and the report says which was there.
    let machine = with("999.0.0");
    let run = machine.run(&["--version", &version]);
    is_the_quickstarts_node(&machine, &run, &version);
    run.says(&format!("replaced: Automagik Arca {version}"))
        .says("it was: Automagik Arca 999.0.0");

    // A program that does not say its version in the product's words is not taken for a newer one.
    let machine = Machine::new();
    fs::create_dir_all(machine.home(".local/bin")).expect("bin");
    executable(
        &machine.home(".local/bin/arca"),
        &format!("#!{}\necho 'arca 999.0.0'\n", which("bash").display()),
    );
    let run = machine.run(&[]);
    is_the_quickstarts_node(&machine, &run, &version);
}

#[test]
fn uninstall_removes_the_program_and_the_unit_and_keeps_the_rest() {
    /// What `--uninstall` takes: the program, the unit, the link that starts it, and the script's own note.
    const GONE: [&str; 5] = [
        ".local/bin/arca",
        ".config/systemd/user/arca.service",
        ".config/systemd/user/default.target.wants/arca.service",
        ".local/state/arca-install",
        ".local/state/arca-install/program.sha256",
    ];
    let machine = Machine::new();
    machine.run(&[]).ok();
    let mut kept = machine.home_tree();
    for gone in GONE {
        assert!(kept.remove(gone).is_some(), "{gone} was installed");
    }

    let run = machine.run(&["--uninstall"]);
    run.ok()
        .says("arca.service was stopped and disabled")
        .says(&format!(
            "{} was removed",
            machine.home(".local/bin/arca").display()
        ))
        .says(&format!(
            "config   {}",
            machine.home(".config/arca/node.json").display()
        ))
        .says(&format!(
            "tokens   {} and {}",
            machine.home(".config/arca/tokens").display(),
            machine.home(".config/arca/assistant-1.secret").display()
        ))
        .says(&format!(
            "state    {}",
            machine.home(".local/state/arca-node").display()
        ));
    assert_eq!(
        machine.home_tree(),
        kept,
        "exactly what the script installed went"
    );
    let calls = machine.service_calls();
    assert_eq!(
        calls[calls.len() - 2..],
        ["--user disable --now arca.service", "--user daemon-reload"]
    );
    assert!(!machine.dir.join("fake/node-running").exists());
    assert!(!run.all().contains(TOKEN));
    // It downloads nothing and asks Incus nothing.
    let downloads = machine.log("curl").len();
    machine.wrote_only_where_it_may();

    // A second uninstall finds nothing and says so.
    machine
        .run(&["--uninstall"])
        .ok()
        .says("nothing to remove")
        .says("Kept, because they are yours");
    assert_eq!(machine.home_tree(), kept);
    assert_eq!(machine.log("curl").len(), downloads);

    // The installer brings the same node back: the same config, the same token.
    let run = machine.run(&[]);
    is_the_quickstarts_node(&machine, &run, &current());
    assert_eq!(machine.arca_calls("token mint"), 1);

    // A unit of that name that this script did not write is not its to remove, nor to replace.
    let unit = machine.home(".config/systemd/user/arca.service");
    fs::write(&unit, "[Service]\nExecStart=/bin/true\n").expect("a unit");
    machine
        .run(&["--uninstall"])
        .ok()
        .says("was not written by this script; it is left alone");
    assert_eq!(
        fs::read_to_string(&unit).expect("the unit"),
        "[Service]\nExecStart=/bin/true\n"
    );
    machine
        .run(&[])
        .refused()
        .says("exists and this script did not write it");
    assert_eq!(
        fs::read_to_string(&unit).expect("the unit"),
        "[Service]\nExecStart=/bin/true\n"
    );
    assert!(!machine.home(".local/bin/arca").exists());
}

#[test]
fn uninstall_takes_the_program_only_while_it_is_the_file_this_script_installed() {
    // A program this script never installed: there is no note of it, so it stays.
    let machine = Machine::new();
    fs::create_dir_all(machine.home(".local/bin")).expect("bin");
    executable(&machine.home(".local/bin/arca"), "their own build");
    let before = machine.home_tree();
    machine
        .run(&["--uninstall"])
        .ok()
        .says("is left in place: this script has no note of having installed it");
    assert_eq!(machine.home_tree(), before);

    // A program that was replaced by hand after the install: it is no longer this script's file.
    let machine = Machine::new();
    machine.run(&[]).ok();
    executable(&machine.home(".local/bin/arca"), "a build of their own");
    machine
        .run(&["--uninstall"])
        .ok()
        .says("is left in place: it is no longer the file this script installed")
        .says("arca.service was stopped and disabled");
    assert_eq!(
        fs::read_to_string(machine.home(".local/bin/arca")).expect("the program"),
        "a build of their own"
    );
    assert!(!machine.home(".config/systemd/user/arca.service").exists());

    // No user manager to ask, as in a shell with no session: the unit goes, and so does the link that would start
    // it at the next login.
    let machine = Machine::new();
    machine.run(&[]).ok();
    let link = machine.home(".config/systemd/user/default.target.wants/arca.service");
    assert!(fs::symlink_metadata(&link).is_ok(), "enable made the link");
    machine.switch("no-user-manager", true);
    machine
        .run(&["--uninstall"])
        .ok()
        .says("was removed, with the link that starts it at login")
        .says("a node that is running was not")
        .says(&format!(
            "{} was removed",
            machine.home(".local/bin/arca").display()
        ));
    assert!(
        fs::symlink_metadata(&link).is_err(),
        "the link is still there"
    );
    assert!(!machine.home(".config/systemd/user/arca.service").exists());
    assert!(!machine.home(".local/bin/arca").exists());
    assert!(!machine.home(".local/state/arca-install").exists());
    assert!(machine.home(".config/arca/node.json").exists());
}

#[test]
fn the_script_runs_from_a_pipe_and_reads_nothing_from_it() {
    for shell in ["sh", "bash"] {
        let machine = Machine::new();
        let run = machine.piped(shell, &[]);
        is_the_quickstarts_node(&machine, &run, &current());

        machine.release(NEXT);
        let run = machine.piped(shell, &["--version", NEXT]);
        is_the_quickstarts_node(&machine, &run, NEXT);

        machine
            .piped(shell, &["--uninstall"])
            .ok()
            .says("was removed");
        assert!(!machine.home(".local/bin/arca").exists());
        machine.wrote_only_where_it_may();
    }

    // A download that was cut short does nothing: the script is one function, called on its last line.
    let text = script_text();
    assert!(
        text.trim_end().ends_with("\nmain \"$@\""),
        "the last line calls main"
    );
    let machine = Machine::new();
    let cut = machine.dir.join("cut.sh");
    let middle = text
        .find("Connect an assistant")
        .expect("the report's words");
    fs::write(&cut, &text[..middle]).expect("a cut script");
    let mut command = machine.command("sh");
    command
        .arg("-s")
        .stdin(fs::File::open(&cut).expect("the cut script"));
    let run = Machine::finish(command);
    assert_ne!(run.code, Some(0), "{}", run.all());
    assert_eq!(machine.home_names(), Vec::<String>::new(), "{}", run.all());
    assert!(machine.log("curl").is_empty() && machine.log("systemctl").is_empty());
}

#[test]
fn without_a_user_manager_it_prints_the_one_command_and_starts_nothing() {
    let start = |machine: &Machine| {
        format!(
            "  {} serve --config {}\n",
            machine.home(".local/bin/arca").display(),
            machine.home(".config/arca/node.json").display()
        )
    };
    let not_started = |machine: &Machine, run: &Run| {
        run.not_started()
            .says("The node is not running")
            .says("Start it yourself with this one command")
            .says(&start(machine))
            .says("it has not answered `arca_status`");
        assert!(!machine.home(".config/systemd").exists());
        assert_eq!(machine.arca_calls("serve"), 0);
        assert_eq!(machine.arca_calls("call"), 1, "one question, not a wait");
        assert!(machine.home(".local/bin/arca").exists());
        assert!(machine.home(".config/arca/node.json").exists());
        assert!(!run.all().contains(TOKEN));
        machine.wrote_only_where_it_may();
    };

    // No systemctl at all.
    let machine = Machine::new();
    fs::remove_file(machine.dir.join("bin/systemctl")).expect("systemctl");
    let run = machine.run(&[]);
    not_started(&machine, &run);

    // A systemctl whose user manager does not answer, as in a container or under `su`.
    let machine = Machine::new();
    machine.switch("no-user-manager", true);
    let run = machine.run(&[]);
    not_started(&machine, &run);
    assert_eq!(machine.service_calls(), Vec::<String>::new());

    // The user started it with that command: the next run finds it and ends with 0.
    machine.switch("node-running", true);
    machine.run(&[]).ok().says("node      mode watch-only");
}

#[test]
fn a_started_node_that_does_not_answer_or_cannot_read_incus_is_a_failure() {
    // A service that was started and a node that does not answer, with where to look.
    let machine = Machine::new();
    machine.switch("node-does-not-start", true);
    let run = machine.run(&[]);
    run.refused()
        .says("the node did not answer within 1 seconds")
        .says("journalctl --user -u arca.service -n 50")
        .never_says("Connect an assistant")
        .never_says(TOKEN);

    // A node that answers and says Incus does not answer it: it runs, and it can read nothing.
    let machine = Machine::new();
    machine.switch("node-cannot-reach-incus", true);
    let run = machine.run(&[]);
    run.refused()
        .says("INCUS DOES NOT ANSWER THE NODE")
        .says("Incus does not answer the node: it can read nothing")
        .never_says("Connect an assistant")
        .never_says(TOKEN);
    // The same from a node this script did not start.
    let machine = Machine::new();
    machine.switch("no-user-manager", true);
    machine.switch("node-running", true);
    machine.switch("node-cannot-reach-incus", true);
    machine
        .run(&[])
        .refused()
        .says("Incus does not answer the node");
}

#[test]
fn a_node_that_already_runs_is_left_as_it_is() {
    // The operating guide's units: this user runs Arca as a service already, and the program is that service's.
    for unit in ["arca-watch.service", "arca-shim.service"] {
        let machine = Machine::new();
        let dir = machine.home(".config/systemd/user");
        fs::create_dir_all(&dir).expect("the unit directory");
        fs::write(dir.join(unit), "[Service]\n").expect("a unit");
        let before = machine.home_tree();
        machine
            .run(&[])
            .refused()
            .says(&format!("{} is there", dir.join(unit).display()))
            .says("nothing was changed");
        assert_eq!(machine.home_tree(), before);

        // And `--uninstall` does not take the program from under that service.
        fs::create_dir_all(machine.home(".local/bin")).expect("bin");
        fs::write(machine.home(".local/bin/arca"), "their program").expect("a program");
        machine.run(&["--uninstall"]).ok().says("is left in place");
        assert_eq!(
            fs::read_to_string(machine.home(".local/bin/arca")).expect("the program"),
            "their program"
        );
    }

    // A node started some other way, which answers for this config: no second one is started beside it.
    let machine = Machine::new();
    machine.run(&[]).ok();
    machine.run(&["--uninstall"]).ok();
    machine.switch("node-running", true);
    let run = machine.run(&[]);
    run.ok()
        .says("none written: a node already answers for this config")
        .says("stop that node and run this script again")
        .says("node      mode watch-only");
    assert!(!machine.home(".config/systemd/user/arca.service").exists());
    assert!(
        !machine
            .service_calls()
            .iter()
            .rev()
            .take(1)
            .any(|line| line.contains("restart"))
    );
}

#[test]
fn a_machine_it_cannot_run_on_is_refused_with_every_reason_at_once() {
    let machine = Machine::new();
    machine.uname("Darwin", "arm64");
    machine.glibc("");
    fs::remove_file(machine.dir.join("bin/sha256sum")).expect("sha256sum");
    let run = machine.run(&[]);
    run.refused()
        .says("this is Darwin, and Arca's published program is for Linux only")
        .says("this machine is arm64, and Arca's published program is for x86_64 only")
        .says("does not say which glibc it has")
        .says("the program `sha256sum` is not on PATH")
        .says("nothing was changed");
    assert_eq!(machine.home_names(), Vec::<String>::new());
    assert!(
        machine
            .log("curl")
            .iter()
            .all(|line| line.contains("--unix-socket"))
    );

    for (answer, accepted) in [
        ("glibc 2.33", false),
        ("glibc 2.17", false),
        ("glibc 1.99", false),
        ("glibc 2.34", true),
        ("glibc 2.43", true),
        ("glibc 3.0", true),
    ] {
        let machine = Machine::new();
        machine.glibc(answer);
        let run = machine.run(&[]);
        if accepted {
            run.ok().says(&format!("{answer},"));
        } else {
            run.refused().says(&format!(
                "this system has {answer}, and Arca's published program needs 2.34 or newer"
            ));
            assert_eq!(machine.home_names(), Vec::<String>::new());
        }
    }

    // Without curl there is no download; the script says so before it asks Incus anything.
    let machine = Machine::new();
    fs::remove_file(machine.dir.join("bin/curl")).expect("curl");
    machine
        .run(&[])
        .refused()
        .says("the program `curl` is not on PATH");
    assert_eq!(machine.home_names(), Vec::<String>::new());

    // A home whose path would not survive being written into the config.
    let machine = Machine::new();
    let odd = machine.dir.join("a home");
    fs::create_dir_all(&odd).expect("a home");
    let mut command = machine.command("sh");
    command
        .arg(script())
        .env("HOME", &odd)
        .stdin(Stdio::piped());
    let run = Machine::finish(command);
    run.refused()
        .says("is not a path this script can write into the config");
    assert_eq!(fs::read_dir(&odd).expect("the home").count(), 0);
}

#[test]
fn the_script_is_what_the_quickstart_and_the_hard_rules_say() {
    let text = script_text();

    // The release it installs is the repository's one version: scripts/version.sh writes this line with every other
    // field, and a release whose installer names another version fails. The quickstart (docs/QUICKSTART.md, "By
    // hand") writes no version by hand any more: it asks GitHub for the newest release.
    assert_eq!(
        current(),
        env!("CARGO_PKG_VERSION"),
        "scripts/install.sh installs the version the repository carries"
    );
    let quickstart =
        fs::read_to_string(repo().join("docs/QUICKSTART.md")).expect("docs/QUICKSTART.md");
    let by_hand = quickstart
        .lines()
        .find_map(|line| line.strip_prefix("v="))
        .expect("the quickstart says which release it downloads");
    assert!(
        by_hand.contains("https://github.com/automagik-si/arca/releases/latest"),
        "docs/QUICKSTART.md asks for the newest release and names none: {by_hand}"
    );
    assert!(
        quickstart.contains("scripts/install.sh"),
        "docs/QUICKSTART.md shows the installer"
    );

    // The install line a person sees is the short address, in the README and in the quickstart alike. That address
    // is a redirect to the release's own `install.sh`, and the long address is written once: where the quickstart
    // says what the short one is.
    let one_line = "curl -fsSL https://get.automagik.si/arca | sh";
    let readme = fs::read_to_string(repo().join("README.md")).expect("README.md");
    for (name, page) in [("README.md", &readme), ("docs/QUICKSTART.md", &quickstart)] {
        assert!(
            page.lines().any(|line| line == one_line),
            "{name} shows the one-line install: {one_line}"
        );
    }
    assert!(
        !readme.contains("releases/latest/download"),
        "README.md shows the short install address only"
    );
    assert_eq!(
        quickstart.matches("releases/latest/download").count(),
        1,
        "docs/QUICKSTART.md writes the long install address in one place"
    );

    // It never becomes another user, and never uses the address that skips pre-releases.
    // (The tests' PATH holds no such program either, so a script that tried would fail every one of them.)
    assert!(!text.contains("releases/latest"));
    let code: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    for program in ["sudo", "su", "doas", "pkexec"] {
        for line in &code {
            let runs = line.starts_with(&format!("{program} "))
                || ["| ", "&& ", "; ", "$(", "exec "]
                    .iter()
                    .any(|before| line.contains(&format!("{before}{program} ")));
            assert!(!runs, "the script runs {program}: {line}");
        }
    }
    assert!(text.contains("\nset -eu\n"), "set -eu");
    assert!(
        code.contains(&"umask 077"),
        "what it makes is the user's alone"
    );
    // A file that is written and then renamed gets its first name from mktemp, never a fixed one.
    assert!(!text.contains(".new"), "a fixed temporary name");

    // Every command that removes or replaces a file names it through ${var:?}.
    let destructive: Vec<&&str> = code
        .iter()
        .filter(|line| line.starts_with("rm ") || line.starts_with("mv "))
        .collect();
    assert!(destructive.len() >= 6, "{destructive:?}");
    for line in destructive {
        assert!(line.contains(":?}"), "not guarded: {line}");
    }

    // shellcheck, where the machine has it, for both shells the quickstart names.
    match find("shellcheck") {
        Some(shellcheck) => {
            for shell in ["sh", "bash"] {
                let output = Command::new(&shellcheck)
                    .arg("-s")
                    .arg(shell)
                    .arg(script())
                    .output()
                    .expect("shellcheck");
                assert!(
                    output.status.success(),
                    "shellcheck -s {shell}:\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }
        }
        None => {
            eprintln!("shellcheck is not on this machine; the script was not checked with it here")
        }
    }
}
