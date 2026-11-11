//! The Automagik workspace's keeper and first boot (`templates/automagik-workspace.sh`) against a fake `docker`.
//!
//! The script is the shipped one, unchanged: only what it reaches outside itself is replaced. `docker`, `systemctl`
//! and `apt-get` are scripts on its `PATH` (`tests/automagik_workspace/fake-docker.sh` keeps each container as a
//! directory, refuses a container that joins the network namespace of one that is not running, and plays the image's
//! health check with a code the test sets). The `PATH` holds the programs the script lists for itself and nothing
//! else, so a program it uses and does not list fails these tests. Paths and pauses come from the environment.
//!
//! So this proves the script's own logic: what each of the three containers is started with, that the gate and the
//! tunnel join the server's namespace and follow it, that the tunnel waits for its token and is the only part a new
//! token replaces, the health words, the refusals, and that the token reaches no output, no file outside the
//! machine's memory and no command line. It does not prove Docker, the two images, the gate, cloud-init or
//! Cloudflare; `docs/OPERATING.md` lists what stays unproven.
//!
//! The tests need `bash`, `jq` and the usual core utilities on the machine that runs them. The token is made here.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const SERVER_IMAGE: &str = "registry.example/server@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const TOKEN: &str = "eyJzeW50aGV0aWMiOiJ0dW5uZWwtdG9rZW4tZm9yLXRlc3RzLW9ubHkifQ";
const OTHER_TOKEN: &str = "eyJzeW50aGV0aWMiOiJhLXNlY29uZC10dW5uZWwtdG9rZW4ifQ";
const URL: &str = "https://team.example.org";
const ACCESS_TEAM: &str = "example.cloudflareaccess.com";
const ACCESS_AUDIENCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
static SEQ: AtomicU64 = AtomicU64::new(0);

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/automagik-workspace.sh")
}

fn which(tool: &str) -> PathBuf {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .expect("sh");
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success() && path.starts_with('/'),
        "this test needs bash, jq and the core utilities on this machine; {tool} is missing"
    );
    PathBuf::from(path)
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

/// This user and group, as `uid:gid`: the scratch machine's files are this user's, where the real one's are the
/// server's and the tunnel's.
fn me() -> String {
    let id = |flag: &str| {
        let output = Command::new(which("id")).arg(flag).output().expect("id");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    format!("{}:{}", id("-u"), id("-g"))
}

/// The programs the script says it starts: the words of its `WS_PROGRAMS`.
fn listed_programs() -> Vec<String> {
    let text = fs::read_to_string(script()).expect("the script");
    let list = text
        .split_once("WS_PROGRAMS='")
        .and_then(|(_, rest)| rest.split_once('\''))
        .expect("the script lists its programs")
        .0;
    list.split_whitespace().map(str::to_string).collect()
}

/// The connector's image, which the script fixes: the value of its `TUNNEL_IMAGE`.
fn tunnel_image() -> String {
    let text = fs::read_to_string(script()).expect("the script");
    text.lines()
        .find_map(|line| line.strip_prefix("TUNNEL_IMAGE="))
        .expect("the script pins the connector")
        .to_string()
}

/// A workspace's settings with a public address and no Cloudflare Access application.
fn settings(public_url: &str) -> Value {
    json!({ "publicUrl": public_url, "serverImage": SERVER_IMAGE })
}

/// The same, with the owner's Cloudflare Access application for that address.
fn settings_with_access(public_url: &str) -> Value {
    let mut value = settings(public_url);
    value["accessTeamDomain"] = json!(ACCESS_TEAM);
    value["accessAudience"] = json!(ACCESS_AUDIENCE);
    value["accessEmails"] = json!(["owner@example.com", "second.owner+ws@example.org"]);
    value
}

struct Workspace {
    dir: PathBuf,
    child: Option<Child>,
}

impl Workspace {
    /// A scratch machine: the settings as cloud-init would have written them, an empty secrets directory, and a
    /// `PATH` of the stand-ins and the script's listed programs only. Nothing runs yet.
    fn new(settings: &Value) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "arca-automagik-workspace-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        for sub in ["bin", "secrets", "docker", "etc"] {
            fs::create_dir_all(dir.join(sub)).expect("scratch");
        }
        fs::write(dir.join("etc/settings.json"), settings.to_string()).expect("settings");
        let workspace = Workspace { dir, child: None };
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/automagik_workspace/fake-docker.sh"),
            workspace.dir.join("bin/docker"),
        )
        .expect("the fake docker");
        fs::set_permissions(
            workspace.dir.join("bin/docker"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("0755");
        for (tool, code) in [("systemctl", 0), ("apt-get", 0)] {
            workspace.stand_in(tool, code);
        }
        for program in listed_programs() {
            let link = workspace.dir.join("bin").join(&program);
            if !link.exists() {
                std::os::unix::fs::symlink(which(&program), link).expect("a link");
            }
        }
        workspace
    }

    /// A stand-in that records its arguments in `<tool>.log` and exits with `code`.
    fn stand_in(&self, tool: &str, code: i32) {
        executable(
            &self.dir.join("bin").join(tool),
            &format!(
                "#!{}\nprintf '%s\\n' \"$*\" >>'{}'\nexit {code}\n",
                which("bash").display(),
                self.dir.join(format!("{tool}.log")).display()
            ),
        );
    }

    /// What Arca does at `instance_start`: one private file per secret.
    fn secret(&self, value: &str) {
        let path = self.dir.join("secrets/TUNNEL_TOKEN");
        let fresh = self.dir.join("secrets/TUNNEL_TOKEN.new");
        fs::write(&fresh, value).expect("a secret");
        fs::set_permissions(&fresh, fs::Permissions::from_mode(0o600)).expect("0600");
        fs::rename(fresh, path).expect("the secret");
    }

    fn command(&self, argument: Option<&str>) -> Command {
        let me = me();
        let mut command = Command::new(which("bash"));
        command.arg(script());
        if let Some(argument) = argument {
            command.arg(argument);
        }
        command
            .env_clear()
            .env("PATH", self.dir.join("bin"))
            .env("WS_NAME", "team1")
            .env("WS_SETTINGS", self.dir.join("etc/settings.json"))
            .env("WS_SECRETS", self.dir.join("secrets"))
            .env("WS_DATA", self.dir.join("data"))
            .env("WS_GATE_DATA", self.dir.join("gate"))
            .env("WS_RUN", self.dir.join("run"))
            .env("WS_CONSOLE", self.dir.join("console.log"))
            .env("WS_INSTALLED", self.dir.join("installed"))
            .env("WS_UNIT", self.dir.join("unit.service"))
            .env("WS_DOCKER_ETC", self.dir.join("docker-etc"))
            .env("WS_DATA_OWNER", &me)
            .env("WS_TUNNEL_OWNER", &me)
            .env("WS_TICK", "0.1")
            .env("WS_STATUS_EVERY", "1")
            .env("WS_REFUSE_EVERY", "1")
            .env("WS_BACKOFF_MAX", "1")
            .env("WS_APT_PAUSE", "0")
            .env("FAKE_DOCKER_DIR", self.dir.join("docker"));
        command
    }

    fn start(&mut self) {
        let out = fs::File::create(self.dir.join("out.log")).expect("the output");
        let child = self
            .command(Some("run"))
            .stdin(Stdio::null())
            .stdout(out.try_clone().expect("the output"))
            .stderr(out)
            .process_group(0)
            .spawn()
            .expect("the keeper starts");
        self.child = Some(child);
    }

    /// Stops the keeper as systemd does, with SIGTERM, and returns its exit code.
    fn stop(&mut self) -> Option<i32> {
        let mut child = self.child.take()?;
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()
            .expect("kill");
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().expect("the keeper's status") {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "the keeper did not stop:\n{}",
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

    /// The health code the image's own check finds in this container from now on.
    fn health(&self, name: &str, code: u16) {
        fs::write(
            self.dir.join(format!("docker/health-{name}")),
            format!("{code:03}\n"),
        )
        .expect("the health code");
    }

    /// Ends a container as if its program had exited with `code`.
    fn end(&self, name: &str, code: i32) {
        let dir = self.dir.join("docker/containers").join(name);
        fs::write(dir.join("exit"), format!("{code}\n")).expect("the exit code");
        fs::remove_file(dir.join("running")).expect("stopped");
    }

    /// The `docker run` calls that named this container, in order.
    fn runs(&self, name: &str) -> Vec<String> {
        self.read("docker/calls.log")
            .lines()
            .filter(|line| line.starts_with(&format!("run -d --name {name} ")))
            .map(str::to_string)
            .collect()
    }

    /// Everything the script wrote or ran, but for Arca's own secret files and the machine's memory (`/run`), where
    /// the token's copy lives.
    fn everything_written(&self) -> String {
        fn walk(dir: &Path, out: &mut String) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.ends_with("secrets") || path.ends_with("run") || path.ends_with("bin") {
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

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", child.id()))
                .output();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn wait_for(workspace: &Workspace, what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "waited for {what}; the keeper said:\n{}\nand docker was asked:\n{}",
            workspace.output(),
            workspace.read("docker/calls.log")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn says(workspace: &Workspace, words: &str) {
    wait_for(workspace, words, || workspace.output().contains(words));
}

#[test]
fn the_shipped_template_is_a_virtual_machine_with_its_script_its_one_secret_and_pins() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let template =
        arca::service::load_template(&dir, "automagik-workspace").expect("the template loads");
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
        (Some("2"), Some("4GiB"), Some("20GiB"))
    );
    // The tunnel's token is the only secret, under the name the cloudflare-tunnel template uses.
    assert_eq!(template.secrets, ["TUNNEL_TOKEN"]);
    let tunnel = arca::service::load_template(&dir, "cloudflare-tunnel").expect("the tunnel");
    assert_eq!(tunnel.secrets, template.secrets);
    // Nothing of it is offered to the service network or to the host: no port, no link, no host listener.
    assert!(template.ports.is_empty() && template.provides.is_none() && template.lan.is_none());
    // The first-boot script is the keeper itself, and `arca templates install` ships both files.
    let script = arca::service::first_boot(&template, &dir)
        .expect("the script")
        .expect("some");
    assert!(script.starts_with("#!/bin/bash\n"));
    for name in ["automagik-workspace.json", "automagik-workspace.sh"] {
        assert!(
            arca::cmd_templates::SHIPPED
                .iter()
                .any(|(shipped, _)| *shipped == name)
        );
    }
    // The server's image is one value in settings, by digest: this template's own to move (the gate runs from the
    // same value), since a workspace needs a server that has the gate. The connector is no setting: the script
    // fixes it, at the cloudflare-tunnel template's pin, digest and all. The settings are only what a workspace's
    // owner decides.
    let settings = template.settings.as_ref().expect("settings");
    let mut keys: Vec<&str> = settings
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "accessAudience",
            "accessEmails",
            "accessTeamDomain",
            "publicUrl",
            "serverImage"
        ]
    );
    let image = settings["serverImage"].as_str().expect("an image");
    let (repository, digest) = image.split_once("@sha256:").expect("a digest");
    assert!(repository.ends_with("/automagik-server"));
    assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
    let text = fs::read_to_string(dir.join("automagik-workspace.json")).expect("the template");
    assert_eq!(text.matches("@sha256:").count(), 1, "one value for the pin");
    assert_eq!(
        tunnel_image(),
        format!(
            "{}@{}",
            tunnel.image,
            tunnel.digest.as_deref().expect("a digest")
        )
    );
    let description = template.description.as_deref().expect("a description");
    assert!(description.contains("cloudflared 2026.10.7"));
    assert!(
        tunnel
            .description
            .as_deref()
            .is_some_and(|text| text.contains("cloudflared 2026.10.7"))
    );
    // The shipped address is nobody's and the Access settings ship empty. The same settings start the keeper, which
    // starts the server alone, with nothing of them in its environment, and says that no gate is started.
    assert_eq!(settings["publicUrl"], "https://workspace.example.com");
    assert_eq!(
        (
            &settings["accessTeamDomain"],
            &settings["accessAudience"],
            &settings["accessEmails"]
        ),
        (&json!(""), &json!(""), &json!([]))
    );
    let mut workspace = Workspace::new(settings);
    workspace.start();
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); no gate is started, because no public address is set (settings.publicUrl is the template's example); the tunnel waits for TUNNEL_TOKEN",
    );
    assert_eq!(
        workspace.runs("automagik-server"),
        [format!(
            "run -d --name automagik-server --restart no -v {}:/data {}",
            workspace.dir.join("data").display(),
            settings["serverImage"].as_str().expect("an image")
        )]
    );
    assert!(workspace.runs("automagik-gate").is_empty());
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn the_keeper_starts_the_three_parts_in_one_namespace_and_never_shows_the_token() {
    let mut workspace = Workspace::new(&settings_with_access(URL));
    workspace.start();
    says(
        &workspace,
        "workspace team1: the server, the gate on loopback port 3082 and the tunnel, in one network namespace; public address https://team.example.org, sign-in by Cloudflare Access (team example.cloudflareaccess.com, 2 e-mail address(es))",
    );
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel waits for TUNNEL_TOKEN (service_secret_set, then instance_start); public address",
    );
    // The server: a network namespace of its own, no published port and no host network, its data on the machine's
    // disk, and nothing in its environment. It is never told its public name, so it refuses every request under
    // one: a route that names its port instead of the gate's fails closed.
    let data = workspace.dir.join("data");
    let gate_data = workspace.dir.join("gate");
    assert_eq!(
        workspace.runs("automagik-server"),
        [format!(
            "run -d --name automagik-server --restart no -v {}:/data {SERVER_IMAGE}",
            data.display()
        )]
    );
    // The gate: the same image by the same digest with the gate's entrypoint, in the server's namespace, with a
    // file system it cannot write and no capability; turned on, with the exact origin, its port and the three Access
    // settings; its own data, and the server's read-only.
    assert_eq!(
        workspace.runs("automagik-gate"),
        [format!(
            "run -d --name automagik-gate --network container:automagik-server --restart no --read-only --cap-drop ALL --security-opt no-new-privileges:true --entrypoint /nodejs/bin/node -e AUTOMAGIK_GATE_ENABLED=1 -e AUTOMAGIK_GATE_ORIGIN={URL} -e AUTOMAGIK_GATE_PORT=3082 -e AUTOMAGIK_GATE_ACCESS_TEAM_DOMAIN={ACCESS_TEAM} -e AUTOMAGIK_GATE_ACCESS_AUD={ACCESS_AUDIENCE} -e AUTOMAGIK_GATE_ACCESS_EMAILS=owner@example.com,second.owner+ws@example.org -v {}:/data -v {}:/dsh:ro {SERVER_IMAGE} /opt/automagik/gate-entrypoint.mjs",
            gate_data.display(),
            data.display()
        )]
    );
    assert!(data.is_dir() && gate_data.is_dir());
    assert!(workspace.runs("automagik-tunnel").is_empty());
    // Health is the image's own check, run inside each container.
    let calls = workspace.read("docker/calls.log");
    for name in ["automagik-server", "automagik-gate"] {
        assert!(calls.contains(&format!(
            "exec {name} /nodejs/bin/node /opt/automagik/healthcheck.mjs\n"
        )));
    }

    // An empty token is not a token.
    workspace.secret("\n");
    says(&workspace, "the tunnel waits: TUNNEL_TOKEN is empty");
    assert!(workspace.runs("automagik-tunnel").is_empty());

    // The token arrives, as a file with a line break at its end. The tunnel joins the server's namespace too.
    workspace.secret(&format!("{TOKEN}\n"));
    says(&workspace, "the tunnel is started from");
    let token_dir = workspace.dir.join("run/tunnel");
    assert_eq!(
        workspace.runs("automagik-tunnel"),
        [format!(
            "run -d --name automagik-tunnel --network container:automagik-server --restart no --read-only --cap-drop ALL --security-opt no-new-privileges:true --user {} -v {}:/run/arca-tunnel:ro {} tunnel --no-autoupdate --metrics 127.0.0.1:20241 run --token-file /run/arca-tunnel/token",
            me(),
            token_dir.display(),
            tunnel_image()
        )]
    );
    // The container found the token in its file, without the line break, and the file is its user's alone.
    assert_eq!(
        workspace
            .read("docker/containers/automagik-tunnel/token.sha256")
            .trim(),
        sha256_hex(TOKEN.as_bytes())
    );
    assert_eq!(workspace.read("run/tunnel/token"), TOKEN);
    let mode = |path: &str| {
        fs::metadata(workspace.dir.join(path))
            .expect("there")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("run/tunnel/token"), 0o400);
    assert_eq!(mode("run/tunnel"), 0o700);
    assert_eq!(mode("run"), 0o700);

    // Running is not registered: that is said only once cloudflared has said it.
    says(
        &workspace,
        "the tunnel is running and has not registered with Cloudflare yet",
    );
    fs::write(
        workspace.dir.join("docker/containers/automagik-tunnel/log"),
        "2026-01-01T00:00:00Z INF Registered tunnel connection connIndex=0\n",
    )
    .expect("the tunnel's log");
    says(
        &workspace,
        "the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel is running and has registered with Cloudflare; public address https://team.example.org, sign-in by Cloudflare Access",
    );

    // No port is published and no container is on the host's network; nothing but the three was started; and the
    // token is in no output, no file outside the machine's memory and no command line.
    let calls = workspace.read("docker/calls.log");
    for forbidden in [
        " -p ",
        "--publish",
        "--network host",
        "--privileged",
        "AUTOMAGIK_PUBLIC_URL",
    ] {
        assert!(!calls.contains(forbidden), "{forbidden}");
    }
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("run "))
            .count(),
        3
    );
    let written = workspace.everything_written();
    assert!(!written.contains(TOKEN), "the token was written somewhere");
    assert!(written.contains("the tunnel is started from"));
    // The e-mail addresses are no secret, and still the console carries only how many there are.
    assert!(!workspace.output().contains("owner@example.com"));

    // The keeper stops with the machine; the containers are Docker's to stop.
    assert_eq!(workspace.stop(), Some(0));
    assert!(workspace.output().contains("the keeper stops"));
    assert!(
        workspace
            .dir
            .join("docker/containers/automagik-server/running")
            .exists()
    );
}

#[test]
fn a_new_token_replaces_the_tunnel_only_and_the_others_follow_the_server() {
    let mut workspace = Workspace::new(&settings(URL));
    workspace.secret(TOKEN);
    workspace.start();
    says(&workspace, "the tunnel is started from");
    says(
        &workspace,
        "public address https://team.example.org, with no Cloudflare Access settings, so the gate has no sign-in to offer",
    );
    // Without the Access settings the gate is still the door, and none of the three names is in its environment.
    let gate = workspace.runs("automagik-gate");
    assert_eq!(gate.len(), 1);
    assert!(gate[0].contains("-e AUTOMAGIK_GATE_ENABLED=1 -e AUTOMAGIK_GATE_ORIGIN=https://team.example.org -e AUTOMAGIK_GATE_PORT=3082 -v "));
    assert!(!gate[0].contains("ACCESS"));

    workspace.secret(OTHER_TOKEN);
    says(
        &workspace,
        "TUNNEL_TOKEN changed: the tunnel is started again with the new one",
    );
    wait_for(&workspace, "the second tunnel", || {
        workspace.runs("automagik-tunnel").len() == 2
            && workspace
                .read("docker/containers/automagik-tunnel/token.sha256")
                .trim()
                == sha256_hex(OTHER_TOKEN.as_bytes())
    });
    assert_eq!(workspace.runs("automagik-server").len(), 1);
    assert_eq!(workspace.runs("automagik-gate").len(), 1);

    // The tunnel ends, as it does when Cloudflare refuses its token: that is said with what to do, and it is started
    // again after a pause, with the same token.
    workspace.end("automagik-tunnel", 1);
    says(
        &workspace,
        "the tunnel stopped (exit code 1); it is started again after a pause. When it keeps stopping, the usual cause is a token Cloudflare does not accept: set TUNNEL_TOKEN again",
    );
    wait_for(&workspace, "the third tunnel", || {
        workspace.runs("automagik-tunnel").len() == 3
    });

    // The gate ends, as it does on a server image that has no gate: said, and started again; the others stay.
    workspace.end("automagik-gate", 1);
    says(
        &workspace,
        "the gate stopped (exit code 1); it is started again after a pause.",
    );
    wait_for(&workspace, "the second gate", || {
        workspace.runs("automagik-gate").len() == 2
    });
    assert_eq!(workspace.runs("automagik-server").len(), 1);
    assert_eq!(workspace.runs("automagik-tunnel").len(), 3);

    // The server ends, and its network namespace with it: the gate and the tunnel are started again beside the new
    // server, which the fake docker would refuse had they been started before it.
    workspace.end("automagik-server", 137);
    says(
        &workspace,
        "the server stopped (exit code 137); it is started again after a pause, and the gate and the tunnel with it",
    );
    wait_for(&workspace, "all three again", || {
        workspace.runs("automagik-server").len() == 2
            && workspace.runs("automagik-gate").len() == 3
            && workspace.runs("automagik-tunnel").len() == 4
    });
    let calls = workspace.read("docker/calls.log");
    let at = |needle: &str| calls.rfind(needle).expect("the call");
    assert!(at("run -d --name automagik-server") < at("run -d --name automagik-gate"));
    assert!(at("run -d --name automagik-server") < at("run -d --name automagik-tunnel"));
    assert!(!calls.contains("cannot join"));
    assert!(!workspace.output().contains("cannot join"));
    let written = workspace.everything_written();
    assert!(!written.contains(TOKEN) && !written.contains(OTHER_TOKEN));
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn a_part_that_is_not_healthy_and_a_start_that_fails_are_said_in_words() {
    // 200 without a sign-in is not health: the sign-in fence is what a healthy part shows.
    let mut workspace = Workspace::new(&settings(URL));
    workspace.health("automagik-server", 200);
    workspace.health("automagik-gate", 0);
    workspace.start();
    says(
        &workspace,
        "the server is NOT healthy: its loopback port 3081 answers HTTP 200 without a sign-in, so its sign-in fence is not up",
    );
    says(
        &workspace,
        "the gate is running and does not answer on its loopback port 3082 yet",
    );
    workspace.health("automagik-server", 503);
    says(
        &workspace,
        "the server is NOT healthy: its loopback port 3081 answers HTTP 503, and a healthy server answers 401",
    );
    workspace.health("automagik-server", 401);
    workspace.health("automagik-gate", 401);
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082)",
    );
    assert_eq!(workspace.stop(), Some(0));

    // Docker cannot start the server (its image cannot be pulled): Docker's last line is said, it is tried again,
    // and neither the gate nor the tunnel is started without it.
    let mut workspace = Workspace::new(&settings(URL));
    workspace.secret(TOKEN);
    fs::write(
        workspace.dir.join("docker/fail-automagik-server"),
        "Unable to find image locally\ndocker: Error response from daemon: manifest unknown\n",
    )
    .expect("the failure");
    workspace.start();
    says(
        &workspace,
        "the server did not start: docker: Error response from daemon: manifest unknown",
    );
    says(
        &workspace,
        "status: the server is not running; the gate is not running; the tunnel is not running",
    );
    wait_for(&workspace, "a second try", || {
        workspace.runs("automagik-server").len() >= 2
    });
    assert!(workspace.runs("automagik-gate").is_empty());
    assert!(workspace.runs("automagik-tunnel").is_empty());
    fs::remove_file(workspace.dir.join("docker/fail-automagik-server")).expect("mended");
    says(&workspace, "the gate is healthy (HTTP 401");
    says(&workspace, "the tunnel is started from");
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn settings_that_cannot_be_used_are_said_in_words_and_nothing_is_started() {
    let access = |team: &str, audience: &str, emails: Value| json!({ "accessTeamDomain": team, "accessAudience": audience, "accessEmails": emails });
    let cases: [(Value, &str); 18] = [
        (
            json!({ "serverImage": "registry.example/server:latest" }),
            "settings.serverImage is an image by digest",
        ),
        (
            json!({ "publicUrl": "http://team.example.org" }),
            "settings.publicUrl is the workspace's exact public origin: https://<host name>",
        ),
        // The gate takes an exact origin, as a browser writes it: a closing slash or a path is not one, nor is
        // https's own port written out, a port that is none, or one with a leading 0. The gate would stop on each.
        (
            json!({ "publicUrl": "https://team.example.org/" }),
            "with no path or closing slash",
        ),
        (
            json!({ "publicUrl": "https://team.example.org:443" }),
            "a port only when it is not 443",
        ),
        (
            json!({ "publicUrl": "https://team.example.org:99999" }),
            "(1 to 65535, written without a leading 0)",
        ),
        (
            json!({ "publicUrl": "https://team.example.org:08443" }),
            "(1 to 65535, written without a leading 0)",
        ),
        (
            json!({ "publicUrl": "https://team.example.org/?next=1" }),
            "settings.publicUrl is the workspace's exact public origin",
        ),
        (
            json!({ "publicUrl": "https://user@team.example.org" }),
            "settings.publicUrl is the workspace's exact public origin",
        ),
        (
            json!({ "publicUrl": "https://team.example.org -e OTHER=1" }),
            "settings.publicUrl is the workspace's exact public origin",
        ),
        // The Access settings: all three or none, each of its own shape, and only beside a public address.
        (
            json!({ "accessTeamDomain": ACCESS_TEAM }),
            "settings.accessTeamDomain, settings.accessAudience and settings.accessEmails go together: set all three, or none (the gate does not start with one or two)",
        ),
        (
            json!({ "accessAudience": ACCESS_AUDIENCE, "accessEmails": ["owner@example.com"] }),
            "go together: set all three, or none",
        ),
        (
            access(ACCESS_TEAM, ACCESS_AUDIENCE, json!([])),
            "go together: set all three, or none",
        ),
        (
            access(
                "https://example.cloudflareaccess.com",
                ACCESS_AUDIENCE,
                json!(["owner@example.com"]),
            ),
            "settings.accessTeamDomain is the Cloudflare Access team's host name alone, in lower case and without https://: <team>.cloudflareaccess.com",
        ),
        // A team domain is Cloudflare's, never another host: the gate fetches its signing keys from it.
        (
            access(
                "team.example.com",
                ACCESS_AUDIENCE,
                json!(["owner@example.com"]),
            ),
            "<team>.cloudflareaccess.com",
        ),
        (
            access(ACCESS_TEAM, "short", json!(["owner@example.com"])),
            "settings.accessAudience is the Access application's audience tag",
        ),
        // One entry is one address: two in one entry would be two for the gate and one in the count.
        (
            access(
                ACCESS_TEAM,
                ACCESS_AUDIENCE,
                json!(["owner@example.com,other@example.org"]),
            ),
            "settings.accessEmails is a list of 1 to 16 e-mail addresses, one in each entry",
        ),
        (
            access(
                ACCESS_TEAM,
                ACCESS_AUDIENCE,
                json!(["owner@example.com\nother@example.org"]),
            ),
            "settings.accessEmails is a list of 1 to 16 e-mail addresses, one in each entry",
        ),
        (
            {
                let mut value = access(ACCESS_TEAM, ACCESS_AUDIENCE, json!(["owner@example.com"]));
                value["publicUrl"] = json!("https://workspace.example.com");
                value
            },
            "the Cloudflare Access settings need settings.publicUrl",
        ),
    ];
    for (change, words) in cases {
        let mut value = settings(URL);
        for (key, replaced) in change.as_object().expect("an object") {
            value[key] = replaced.clone();
        }
        let mut workspace = Workspace::new(&value);
        workspace.secret(TOKEN);
        workspace.start();
        says(&workspace, words);
        says(
            &workspace,
            "workspace team1 does not start: its settings cannot be used: ",
        );
        says(&workspace, "They are fixed when the machine is created");
        assert_eq!(workspace.stop(), Some(0));
        assert_eq!(workspace.read("docker/calls.log"), "", "{words}");

        // The first boot says the same on the console, where service_logs reads, and installs the keeper so that
        // the reason is said again at every start.
        let output = workspace.command(None).output().expect("the first boot");
        assert!(output.status.success());
        let console = workspace.read("console.log");
        assert!(
            console.contains("first boot: the settings cannot be used: ")
                && console.contains(words),
            "{words}: {console}"
        );
        assert!(workspace.dir.join("installed").exists());
    }
    // No settings at all: said too.
    let mut workspace = Workspace::new(&json!({}));
    fs::remove_file(workspace.dir.join("etc/settings.json")).expect("removed");
    workspace.start();
    says(
        &workspace,
        "the settings are missing or are not one JSON object",
    );
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn a_port_in_the_public_origin_is_taken_when_it_is_not_443() {
    let mut workspace = Workspace::new(&settings("https://team.example.org:8443"));
    workspace.start();
    says(&workspace, "the gate is healthy (HTTP 401");
    assert!(
        workspace.runs("automagik-gate")[0]
            .contains("-e AUTOMAGIK_GATE_ORIGIN=https://team.example.org:8443 ")
    );
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn a_docker_that_does_not_answer_leaves_every_running_container_alone() {
    let mut workspace = Workspace::new(&settings(URL));
    workspace.secret(TOKEN);
    workspace.start();
    says(&workspace, "the tunnel is started from");
    says(&workspace, "the gate is healthy (HTTP 401");
    let before = workspace.read("docker/calls.log");
    let acted = |calls: &str| {
        calls
            .lines()
            .filter(|line| line.starts_with("run ") || line.starts_with("rm "))
            .count()
    };
    // The daemon stops answering for a while: that is said, and nothing is removed or started.
    fs::write(workspace.dir.join("docker/fail-inspect"), "").expect("the failure");
    says(
        &workspace,
        "status: Docker did not answer about the server; Docker did not answer about the gate; Docker did not answer about the tunnel",
    );
    std::thread::sleep(Duration::from_millis(600));
    fs::remove_file(workspace.dir.join("docker/fail-inspect")).expect("mended");
    wait_for(&workspace, "the healthy status again", || {
        workspace
            .output()
            .rsplit("Docker did not answer about the server")
            .next()
            .is_some_and(|after| after.contains("status: the server is healthy"))
    });
    assert_eq!(acted(&workspace.read("docker/calls.log")), acted(&before));
    for name in ["automagik-server", "automagik-gate", "automagik-tunnel"] {
        assert!(
            workspace
                .dir
                .join(format!("docker/containers/{name}/running"))
                .exists(),
            "{name}"
        );
        assert_eq!(workspace.runs(name).len(), 1, "{name}");
    }
    assert!(!workspace.output().contains("stopped (exit code"));
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn a_keeper_that_cannot_start_stays_up_and_says_why() {
    // A command the first boot should have installed is missing: the keeper says so at intervals and does not
    // exit, so systemd is not starting it over and over; when the command is there, it goes on.
    let mut workspace = Workspace::new(&settings(URL));
    let jq = workspace.dir.join("bin/jq");
    let aside = workspace.dir.join("jq.aside");
    fs::rename(&jq, &aside).expect("no jq");
    workspace.start();
    let words = "workspace team1 does not start: these commands are missing on this machine: jq.";
    wait_for(&workspace, "the reason, said twice", || {
        workspace.output().matches(words).count() >= 2
    });
    assert!(
        workspace
            .child
            .as_mut()
            .expect("the keeper")
            .try_wait()
            .expect("its status")
            .is_none(),
        "the keeper exited"
    );
    assert_eq!(workspace.read("docker/calls.log"), "");
    fs::rename(&aside, &jq).expect("jq again");
    says(&workspace, "the server is healthy (HTTP 401");
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn the_first_boot_installs_what_it_names_and_speaks_on_the_console() {
    let workspace = Workspace::new(&settings_with_access(URL));
    let output = workspace.command(None).output().expect("the first boot");
    let said = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{said}");
    // Debian 13 ships the docker command as its own package, which the daemon's package only recommends. Docker
    // Compose is not needed: the keeper starts the containers itself.
    assert_eq!(
        workspace.read("apt-get.log"),
        "update -q\ninstall -y -q --no-install-recommends docker.io docker-cli jq ca-certificates coreutils\n"
    );
    // The keeper is this script, installed, and a service that speaks on the console.
    assert_eq!(
        fs::read(workspace.dir.join("installed")).expect("installed"),
        fs::read(script()).expect("the script")
    );
    let unit = workspace.read("unit.service");
    assert!(unit.contains(&format!(
        "ExecStart={} run\n",
        workspace.dir.join("installed").display()
    )));
    assert!(unit.contains("Requires=docker.service\n") && unit.contains("Restart=always\n"));
    assert!(unit.contains("StandardOutput=journal+console\nStandardError=journal+console\n"));
    assert_eq!(
        workspace.read("systemctl.log"),
        "daemon-reload\nrestart docker\nenable --now arca-automagik-workspace.service\n"
    );
    assert!(
        workspace
            .read("docker-etc/daemon.json")
            .contains("\"max-size\": \"20m\"")
    );
    assert!(workspace.dir.join("data").is_dir() && workspace.dir.join("gate").is_dir());
    // What it says is on the console too, where service_logs reads. Settings that can be used are not remarked on.
    let console = workspace.read("console.log");
    for words in [
        "arca-automagik-workspace: first boot: installing docker.io docker-cli",
        "arca-automagik-workspace: first boot done: the keeper is installed and started",
    ] {
        assert!(console.contains(words) && said.contains(words), "{words}");
    }
    assert!(!console.contains("cannot be used"));
    // Nothing was started by the first boot itself.
    assert_eq!(workspace.read("docker/calls.log"), "");
}

#[test]
fn a_first_boot_that_fails_says_on_the_console_what_is_missing_or_did_not_start() {
    // The install ends well and the docker command is not there: the Debian 13 case.
    let workspace = Workspace::new(&settings(URL));
    fs::remove_file(workspace.dir.join("bin/docker")).expect("no docker");
    let output = workspace.command(None).output().expect("the first boot");
    assert_eq!(output.status.code(), Some(1));
    let console = workspace.read("console.log");
    assert!(
        console.contains(
            "first boot failed: these commands are missing after the install: docker. The packages asked for were: docker.io docker-cli jq ca-certificates coreutils."
        ),
        "{console}"
    );
    assert!(console.contains("The machine has no workspace"));
    assert!(
        !workspace.dir.join("installed").exists() && !workspace.dir.join("unit.service").exists()
    );

    // The package manager fails every time: each try is said, then what is missing.
    let workspace = Workspace::new(&settings(URL));
    workspace.stand_in("apt-get", 100);
    fs::remove_file(workspace.dir.join("bin/jq")).expect("no jq");
    let output = workspace.command(None).output().expect("the first boot");
    assert_eq!(output.status.code(), Some(1));
    let console = workspace.read("console.log");
    assert!(console.contains("the packages could not be installed (try 6 of 6)"));
    assert!(console.contains("these commands are missing after the install: jq."));

    // Docker's service does not start: said, with where to look.
    let workspace = Workspace::new(&settings(URL));
    workspace.stand_in("systemctl", 1);
    let output = workspace.command(None).output().expect("the first boot");
    assert_eq!(output.status.code(), Some(1));
    let console = workspace.read("console.log");
    assert!(
        console.contains(
            "first boot failed: Docker or the keeper's service did not start. Inside the machine, 'systemctl status docker arca-automagik-workspace' says why."
        ),
        "{console}"
    );
    assert!(!console.contains("first boot done"));
}
