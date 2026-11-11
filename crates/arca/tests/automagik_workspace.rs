//! The Automagik workspace's keeper and first boot (`templates/automagik-workspace.sh`) against a fake `docker`.
//!
//! The script is the shipped one, unchanged: only what it reaches outside itself is replaced. `docker`, `systemctl`
//! and `apt-get` are scripts on its `PATH` (`tests/automagik_workspace/fake-docker.sh` keeps each container as a
//! directory, refuses a container that joins the network namespace of one that is not running, and plays the image's
//! health check with a code the test sets). The `PATH` holds the programs the script lists for itself and nothing
//! else, so a program it uses and does not list fails these tests. Paths and pauses come from the environment.
//!
//! So this proves the script's own logic: what each of the three containers is started with, for both reaches (a
//! Cloudflare Tunnel connector or a tailnet node), that the gate and that part join the server's namespace and
//! follow it, that the tunnel waits for its token and is the only part a new
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
const TAILNET_URL: &str = "https://workspace1.tail0example.ts.net";
const AUTHKEY: &str = "synthetic-tailnet-auth-key-for-these-tests-only-0123456789";
const OTHER_AUTHKEY: &str = "another-synthetic-tailnet-auth-key-for-these-tests-9876543210";
const CLAIM_CODE: &str = "5ynthet1cc1a1mc0de5ynthet1cc1a1m";
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

/// The tailnet node's image, which the script fixes: the value of its `TAILNET_IMAGE`.
fn tailnet_image() -> String {
    let text = fs::read_to_string(script()).expect("the script");
    text.lines()
        .find_map(|line| line.strip_prefix("TAILNET_IMAGE="))
        .expect("the script pins the tailnet node")
        .to_string()
}

/// A workspace reached on a tailnet, under the machine's own tailnet name.
fn tailnet_settings() -> Value {
    json!({ "reach": "tailnet", "publicUrl": TAILNET_URL, "serverImage": SERVER_IMAGE })
}

/// What `tailscale status --json` says, as far as the keeper reads it.
fn tailnet_status(workspace: &Workspace, backend: &str, name: &str) {
    let fresh = workspace.dir.join("docker/tailnet-status.json.new");
    fs::write(
        &fresh,
        json!({ "BackendState": backend, "Self": { "DNSName": name } }).to_string(),
    )
    .expect("the status");
    fs::rename(fresh, workspace.dir.join("docker/tailnet-status.json")).expect("the status");
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
        self.secret_named("TUNNEL_TOKEN", value);
    }

    fn secret_named(&self, name: &str, value: &str) {
        let path = self.dir.join("secrets").join(name);
        let fresh = self.dir.join("secrets").join(format!("{name}.new"));
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
            .env("WS_TAILNET_OWNER", &me)
            .env("WS_TAILNET_STATE", self.dir.join("tailnet"))
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
fn the_shipped_template_is_a_virtual_machine_with_its_script_its_secrets_and_pins() {
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
    // One secret for each reach: the tunnel's token, under the name the cloudflare-tunnel template uses, and the
    // tailnet node's auth key.
    assert_eq!(template.secrets, ["TUNNEL_TOKEN", "TAILSCALE_AUTHKEY"]);
    let tunnel = arca::service::load_template(&dir, "cloudflare-tunnel").expect("the tunnel");
    assert_eq!(tunnel.secrets, template.secrets[..1]);
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
            "reach",
            "serverImage"
        ]
    );
    // The reach it ships with is the Cloudflare Tunnel. The tailnet node is no setting either: the script fixes it,
    // by digest, from Tailscale's own image.
    assert_eq!(settings["reach"], "cloudflare");
    let node = tailnet_image();
    let (repository, digest) = node.split_once("@sha256:").expect("a digest");
    assert_eq!(repository, "docker.io/tailscale/tailscale");
    assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        template
            .description
            .as_deref()
            .is_some_and(|text| text.contains("Tailscale 1.104.4"))
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
    // A create gives a workspace its own reach, address and Access settings, and nothing else: the image a
    // workspace runs is the template's to pin, never a create's to choose. The address ships empty, since a default
    // that is an https address would hold every create to its host.
    assert_eq!(
        template.create_settings,
        [
            "reach",
            "publicUrl",
            "accessTeamDomain",
            "accessAudience",
            "accessEmails"
        ]
    );
    let created = arca::service::create_settings(
        &template,
        Some(&json!({
            "reach": "tailnet",
            "publicUrl": TAILNET_URL,
            "accessEmails": ["owner@example.com"]
        })),
    )
    .expect("a create may give these")
    .merged
    .expect("the merged settings");
    assert_eq!(
        (
            &created["reach"],
            &created["publicUrl"],
            &created["accessEmails"]
        ),
        (
            &json!("tailnet"),
            &json!(TAILNET_URL),
            &json!(["owner@example.com"])
        )
    );
    assert_eq!(created["serverImage"], settings["serverImage"]);
    assert!(
        arca::service::create_settings(&template, Some(&json!({ "serverImage": SERVER_IMAGE })))
            .is_err()
    );
    // The address and the Access settings ship empty. The same settings start the keeper, which starts the server
    // alone, with nothing of them in its environment, and says that no gate is started.
    assert_eq!(settings["publicUrl"], "");
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
        "status: the server is healthy (HTTP 401 on its loopback port 3081); no gate is started, because no public address is set (settings.publicUrl is empty); the tunnel waits for TUNNEL_TOKEN",
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
        "workspace team1: the server, the gate on loopback port 3082 and the tunnel, in one network namespace; reach cloudflare: public address https://team.example.org, sign-in by Cloudflare Access (team example.cloudflareaccess.com, 2 e-mail address(es))",
    );
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel waits for TUNNEL_TOKEN (service_secret_set, then instance_start); reach cloudflare: public address",
    );
    // The server: a network namespace of its own, no published port and no host network, its data on the machine's
    // disk, and nothing in its environment. It is never told its public name, so it does not trust a request
    // that comes under one: a route that names its port instead of the gate's gives nobody the workspace.
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
        "the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel is running and has registered with Cloudflare; reach cloudflare: public address https://team.example.org, sign-in by Cloudflare Access",
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
    let mut workspace = Workspace::new(&settings_with_access(URL));
    workspace.secret(TOKEN);
    workspace.start();
    says(&workspace, "the tunnel is started from");

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
    let mut workspace = Workspace::new(&settings_with_access(URL));
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
    let cases: [(Value, &str); 25] = [
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
                value["publicUrl"] = json!("");
                value
            },
            "the Cloudflare Access settings need settings.publicUrl",
        ),
        // An address that is not text is not "not set".
        (
            json!({ "publicUrl": 5 }),
            "settings.publicUrl is the workspace's exact public origin",
        ),
        // The gate takes no IP address as its origin, and no passkey belongs to one.
        (
            json!({ "publicUrl": "https://192.0.2.10" }),
            "settings.publicUrl is a name, not an IP address",
        ),
        // The reach is one of two words.
        (
            json!({ "reach": "lan" }),
            "settings.reach is how the workspace is reached: \"cloudflare\" (a Cloudflare Tunnel; also when it is left out) or \"tailnet\"",
        ),
        // On a tailnet the address is the machine's own tailnet name, on port 443 and no other, and never the
        // empty address the template ships; and the gate signs its owner in itself, so the Access settings have no
        // place.
        (
            json!({ "reach": "tailnet" }),
            "with settings.reach \"tailnet\", settings.publicUrl is the machine's own tailnet address: https://<machine>.<tailnet>.ts.net",
        ),
        (
            json!({ "reach": "tailnet", "publicUrl": "https://workspace1.tail0example.ts.net:8443" }),
            "in lower case, with no port and no path",
        ),
        (
            json!({ "reach": "tailnet", "publicUrl": "" }),
            "settings.publicUrl is the machine's own tailnet address",
        ),
        (
            {
                let mut value = access(ACCESS_TEAM, ACCESS_AUDIENCE, json!(["owner@example.com"]));
                value["reach"] = json!("tailnet");
                value["publicUrl"] = json!(TAILNET_URL);
                value
            },
            "the Cloudflare Access settings do not go with settings.reach \"tailnet\"",
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
fn with_reach_tailnet_the_node_takes_the_tunnels_place_and_the_gate_takes_its_first_claim_on_its_main_port()
 {
    let mut workspace = Workspace::new(&tailnet_settings());
    // A tunnel token that is there all the same is not this reach's secret: no connector is started with it.
    workspace.secret(TOKEN);
    workspace.start();
    says(
        &workspace,
        "workspace team1: the server and the gate on loopback port 3082 in one network namespace, and the tailnet node in one of its own, reaching the gate through the relay alone; reach tailnet: https://workspace1.tail0example.ts.net, sign-in by passkey",
    );
    says(
        &workspace,
        "the tailnet node waits for TAILSCALE_AUTHKEY (service_secret_set, then instance_start)",
    );
    // The server gets no environment. The gate gets its origin, its port and the remote claim, and none of the
    // three Access settings.
    let data = workspace.dir.join("data");
    let gate_data = workspace.dir.join("gate");
    assert_eq!(
        workspace.runs("automagik-server"),
        [format!(
            "run -d --name automagik-server --restart no -v {}:/data {SERVER_IMAGE}",
            data.display()
        )]
    );
    assert_eq!(
        workspace.runs("automagik-gate"),
        [format!(
            "run -d --name automagik-gate --network container:automagik-server --restart no --read-only --cap-drop ALL --security-opt no-new-privileges:true --entrypoint /nodejs/bin/node -e AUTOMAGIK_GATE_ENABLED=1 -e AUTOMAGIK_GATE_ORIGIN={TAILNET_URL} -e AUTOMAGIK_GATE_PORT=3082 -e AUTOMAGIK_GATE_CLAIM=remote -v {}:/data -v {}:/dsh:ro {SERVER_IMAGE} /opt/automagik/gate-entrypoint.mjs",
            gate_data.display(),
            data.display()
        )]
    );
    assert!(workspace.runs("automagik-tailnet").is_empty());
    // The one way from the node to the workspace: a private network that is internal, the server's place on it,
    // and the relay in the server's namespace, listening on that one address and copying bytes to the gate's
    // loopback port. It is the server image's own node, read-only, with no capability.
    says(
        &workspace,
        "the relay to the gate is started: 10.88.0.2:3082 on the private network, to the gate's loopback port",
    );
    let calls = workspace.read("docker/calls.log");
    let relay = workspace.runs("automagik-relay");
    assert_eq!(relay.len(), 1);
    assert!(relay[0].starts_with(&format!(
        "run -d --name automagik-relay --network container:automagik-server --restart no --read-only --cap-drop ALL --security-opt no-new-privileges:true --entrypoint /nodejs/bin/node {SERVER_IMAGE} -e const net=require(\"net\");"
    )));
    assert!(relay[0].ends_with(".listen(Number(port),ip) 10.88.0.2 3082"));
    assert!(relay[0].contains("net.connect(Number(port),\"127.0.0.1\")"));
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("network c"))
            .collect::<Vec<_>>(),
        [
            "network create --internal automagik-relay",
            "network connect automagik-relay automagik-server"
        ]
    );

    // The gate, as the test plays it, has written its claim code into its own data. The status says that a first
    // claim is waited for, from the file being there; the code in it is never read.
    fs::write(gate_data.join("claim-code"), format!("{CLAIM_CODE}\n")).expect("the claim code");
    says(
        &workspace,
        "reach tailnet: https://workspace1.tail0example.ts.net, sign-in by passkey, waiting for its first claim",
    );

    // The key arrives, as a file with a line break at its end.
    workspace.secret_named("TAILSCALE_AUTHKEY", &format!("{AUTHKEY}\n"));
    says(&workspace, "the tailnet node is started from");
    let key_dir = workspace.dir.join("run/tailnet");
    assert_eq!(
        workspace.runs("automagik-tailnet"),
        [format!(
            "run -d --name automagik-tailnet --network bridge --restart no --cap-drop ALL --security-opt no-new-privileges:true -e TS_AUTHKEY=file:/run/arca-tailnet/authkey -e TS_AUTH_ONCE=true -e TS_USERSPACE=true -e TS_STATE_DIR=/var/lib/tailscale -e TS_SOCKET=/tmp/tailscaled.sock -e TS_HOSTNAME=workspace1 -e TS_SERVE_CONFIG=/run/arca-tailnet/serve.json -e TS_ACCEPT_DNS=false -v {}:/var/lib/tailscale -v {}:/run/arca-tailnet:ro {}",
            workspace.dir.join("tailnet").display(),
            key_dir.display(),
            tailnet_image()
        )]
    );
    // The node has a network namespace of its own: it never joins the server's, where a Tailscale node in userspace
    // networking would hand every port of the workspace's loopback to the tailnet. Its one way in is the private
    // network, which it joins after it is made.
    assert!(!workspace.runs("automagik-tailnet")[0].contains("container:"));
    assert!(
        workspace
            .read("docker/calls.log")
            .contains("network connect automagik-relay automagik-tailnet\n")
    );
    assert_eq!(
        workspace
            .read("docker/containers/automagik-tailnet/netmode")
            .trim(),
        "bridge"
    );
    // The node found the key in its file, without the line break; the file is its user's alone, and so is its
    // state, which is on the machine's disk and outlives the container.
    assert_eq!(
        workspace
            .read("docker/containers/automagik-tailnet/token.sha256")
            .trim(),
        sha256_hex(AUTHKEY.as_bytes())
    );
    assert_eq!(workspace.read("run/tailnet/authkey"), AUTHKEY);
    let mode = |path: &str| {
        fs::metadata(workspace.dir.join(path))
            .expect("there")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("run/tailnet/authkey"), 0o400);
    assert_eq!(mode("run/tailnet"), 0o700);
    assert_eq!(mode("tailnet"), 0o700);
    // All it serves: HTTPS on port 443 of its own tailnet name, every path to the relay's address, which is not
    // on any loopback. No other port, no Funnel.
    let serve = workspace.read("run/tailnet/serve.json");
    assert_eq!(
        serve,
        "{\"TCP\":{\"443\":{\"HTTPS\":true}},\"Web\":{\"${TS_CERT_DOMAIN}:443\":{\"Handlers\":{\"/\":{\"Proxy\":\"http://10.88.0.2:3082\"}}}}}\n"
    );
    assert!(!serve.contains("127.0.0.1") && !serve.contains("localhost"));
    let parsed: Value = serde_json::from_str(&serve).expect("the serve file is JSON");
    assert_eq!(parsed.as_object().expect("an object").len(), 2);
    assert!(parsed.get("AllowFunnel").is_none());

    // Started is not logged in: that is said once the node says it, with the name it serves.
    says(
        &workspace,
        "the tailnet node is running and is not logged in yet",
    );
    tailnet_status(&workspace, "Running", "workspace1.tail0example.ts.net.");
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tailnet node is logged in and serves https://workspace1.tail0example.ts.net; reach tailnet: https://workspace1.tail0example.ts.net, sign-in by passkey, waiting for its first claim",
    );
    // A code older than the gate keeps one good is said, from the file's age alone.
    fs::File::options()
        .write(true)
        .open(gate_data.join("claim-code"))
        .expect("the claim code")
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(16 * 60))
        .expect("an older time");
    says(
        &workspace,
        "waiting for its first claim, with a claim code that is past its 15 minutes: a new one has to be made",
    );
    // The owner claims: the gate removes its code, and the status says claimed.
    fs::remove_file(gate_data.join("claim-code")).expect("claimed");
    says(&workspace, "sign-in by passkey, claimed");

    // A new key replaces the node only.
    workspace.secret_named("TAILSCALE_AUTHKEY", OTHER_AUTHKEY);
    says(
        &workspace,
        "TAILSCALE_AUTHKEY changed: the tailnet node is started again with the new one",
    );
    wait_for(&workspace, "the second node", || {
        workspace.runs("automagik-tailnet").len() == 2
            && workspace
                .read("docker/containers/automagik-tailnet/token.sha256")
                .trim()
                == sha256_hex(OTHER_AUTHKEY.as_bytes())
    });
    assert_eq!(workspace.runs("automagik-server").len(), 1);
    assert_eq!(workspace.runs("automagik-gate").len(), 1);
    assert_eq!(workspace.runs("automagik-relay").len(), 1);

    // No connector, no published port, no host network, no Access setting; the gate's claim-only port is named
    // nowhere; the claim code's file is never read; and neither secret nor the code is in any output or command.
    assert!(workspace.runs("automagik-tunnel").is_empty());
    let calls = workspace.read("docker/calls.log");
    for forbidden in [
        " -p ",
        "--publish",
        "--network host",
        "--privileged",
        "ACCESS",
        "3083",
        "claim-code",
        "run -d --name automagik-tunnel",
    ] {
        assert!(!calls.contains(forbidden), "{forbidden}");
    }
    assert!(!serve.contains("3083"));
    let output = workspace.output();
    for secret in [AUTHKEY, OTHER_AUTHKEY, TOKEN, CLAIM_CODE] {
        assert!(!output.contains(secret) && !calls.contains(secret));
    }
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn a_tailnet_node_that_is_not_logged_in_or_has_another_name_says_so() {
    let mut workspace = Workspace::new(&tailnet_settings());
    workspace.secret_named("TAILSCALE_AUTHKEY", AUTHKEY);
    // The tailnet did not take the key.
    tailnet_status(&workspace, "NeedsLogin", "");
    workspace.start();
    says(
        &workspace,
        "the tailnet node is NOT logged in: the tailnet did not take its key (used already, expired or revoked). Set TAILSCALE_AUTHKEY again (service_secret_set, then instance_start)",
    );
    // The key is good and the node waits for a person's approval.
    tailnet_status(&workspace, "NeedsMachineAuth", "");
    says(
        &workspace,
        "the tailnet node is NOT logged in yet: it waits for approval in the tailnet's admin console",
    );
    // The name was taken on the tailnet, so the node got another: the gate would refuse every request.
    tailnet_status(&workspace, "Running", "workspace1-1.tail0example.ts.net.");
    says(
        &workspace,
        "the tailnet node is logged in as workspace1-1.tail0example.ts.net, which is NOT the name in settings.publicUrl",
    );
    assert!(!workspace.output().contains("logged in and serves"));
    // The node ends: said, and started again; the server and the gate stay.
    workspace.end("automagik-tailnet", 1);
    says(
        &workspace,
        "the tailnet node stopped (exit code 1); it is started again after a pause. When it keeps stopping, the usual cause is a key the tailnet does not take (used already, expired or revoked): set TAILSCALE_AUTHKEY again",
    );
    wait_for(&workspace, "the second node", || {
        workspace.runs("automagik-tailnet").len() == 2
    });
    assert_eq!(workspace.runs("automagik-server").len(), 1);
    assert_eq!(workspace.runs("automagik-gate").len(), 1);
    assert!(!workspace.output().contains(AUTHKEY));
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn the_claim_code_has_one_reader_that_nothing_calls() {
    // The script names the one place that reads the gate's claim code, and calls it nowhere: its name appears in
    // its definition and in comments only.
    let text = fs::read_to_string(script()).expect("the script");
    let uses: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("read_claim_code") && !line.trim_start().starts_with('#'))
        .collect();
    assert_eq!(uses, ["read_claim_code() {"]);
    // It reads exactly the gate's own file, inside the gate's container.
    let body = text
        .split_once("read_claim_code() {")
        .and_then(|(_, rest)| rest.split_once("\n}\n"))
        .expect("the function")
        .0;
    assert!(body.contains("docker exec \"$GATE\" \"$IMAGE_NODE\""));
    assert!(body.contains("readFileSync('$CLAIM_CODE_FILE','utf8')"));
    assert!(text.contains("\nCLAIM_CODE_FILE=/data/claim-code "));
    // The claim state the keeper prints comes from the file's name and age, never from what is in it.
    assert!(!text.contains("cat \"$WS_GATE_DATA"));
    // The first boot of a tailnet workspace makes the node's state directory, its user's alone.
    let workspace = Workspace::new(&tailnet_settings());
    let output = workspace.command(None).output().expect("the first boot");
    assert!(output.status.success());
    assert_eq!(
        fs::metadata(workspace.dir.join("tailnet"))
            .expect("the state directory")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn without_the_access_settings_no_connector_is_started_beside_the_gate() {
    // A gate without the Access settings signs nobody in through the tunnel, and waits for a first claim on a port
    // of its own on the loopback a connector would share. So no connector is started, token or not: no route can
    // then reach that port, or anything else.
    let mut workspace = Workspace::new(&settings(URL));
    workspace.secret(TOKEN);
    workspace.start();
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel is not started: without the Cloudflare Access settings nobody could sign in through it; reach cloudflare: public address https://team.example.org, with no Cloudflare Access settings, so nobody can sign in through the tunnel",
    );
    // The gate is still the server's only door, with none of the three Access names and no claim mode of ours.
    let gate = workspace.runs("automagik-gate");
    assert_eq!(gate.len(), 1);
    assert!(gate[0].contains("-e AUTOMAGIK_GATE_ENABLED=1 -e AUTOMAGIK_GATE_ORIGIN=https://team.example.org -e AUTOMAGIK_GATE_PORT=3082 -v "));
    assert!(!gate[0].contains("ACCESS") && !gate[0].contains("CLAIM"));
    std::thread::sleep(Duration::from_millis(500));
    assert!(workspace.runs("automagik-tunnel").is_empty());
    assert!(!workspace.dir.join("run/tunnel").exists());
    assert_eq!(workspace.stop(), Some(0));
}

#[test]
fn a_tailnet_node_outside_a_namespace_of_its_own_is_removed_and_never_called_serving() {
    // A Tailscale node in userspace networking hands every port it does not serve to the loopback it lives on. So a
    // node that joined the server's namespace (or the machine's) is removed on sight, with the reason, before
    // anything is said of what it serves.
    let mut workspace = Workspace::new(&tailnet_settings());
    tailnet_status(&workspace, "Running", "workspace1.tail0example.ts.net.");
    // A network of the relay's name that is not internal is not the keeper's own: it is made again.
    fs::create_dir_all(workspace.dir.join("docker/networks/automagik-relay")).expect("a network");
    workspace.start();
    says(
        &workspace,
        "a Docker network named automagik-relay is there and is not internal: it is removed and made again",
    );
    says(&workspace, "the relay to the gate is started");
    let calls = workspace.read("docker/calls.log");
    assert!(
        calls.find("network rm automagik-relay\n").expect("removed")
            < calls
                .find("network create --internal automagik-relay\n")
                .expect("made again")
    );
    assert!(
        workspace
            .dir
            .join("docker/networks/automagik-relay/internal")
            .exists()
    );
    let node = workspace.dir.join("docker/containers/automagik-tailnet");
    let plant = |mode: &str| {
        fs::create_dir_all(&node).expect("a node left from elsewhere");
        fs::write(node.join("netmode"), format!("{mode}\n")).expect("its network");
        fs::write(node.join("running"), "").expect("running");
    };
    // One on the machine's own network, found before any key is there: removed.
    plant("host");
    says(
        &workspace,
        "the tailnet node is not in a network namespace of its own (host), where every loopback port it shares would be open to the tailnet: it is removed",
    );
    wait_for(&workspace, "its removal", || !node.exists());

    // One in the server's namespace that Docker does not let go of, with its key there, logged in and under the
    // right name: everything that would make a node serving. It is said to be NOT serving, for as long as it is
    // there, and no node is started beside it.
    fs::write(workspace.dir.join("docker/fail-rm-automagik-tailnet"), "").expect("the failure");
    plant("container:automagik-server");
    workspace.secret_named("TAILSCALE_AUTHKEY", AUTHKEY);
    says(
        &workspace,
        "the tailnet node is not in a network namespace of its own (container:automagik-server)",
    );
    says(
        &workspace,
        "the tailnet node is NOT in a network namespace of its own, so it is not serving: it is removed and started again",
    );
    std::thread::sleep(Duration::from_millis(600));
    assert!(!workspace.output().contains("serves"));
    assert!(node.exists());
    assert!(
        workspace
            .runs("automagik-tailnet")
            .iter()
            .all(|line| line.contains(" --network bridge "))
    );

    // Docker lets go: the node is removed and started in a namespace of its own, and only that one serves.
    fs::remove_file(workspace.dir.join("docker/fail-rm-automagik-tailnet")).expect("mended");
    says(
        &workspace,
        "the tailnet node is logged in and serves https://workspace1.tail0example.ts.net",
    );
    assert_eq!(
        fs::read_to_string(node.join("netmode"))
            .expect("the new node")
            .trim(),
        "bridge"
    );

    // The relay ends: the node is logged in, and is not said to serve while its one way to the gate is down.
    workspace.end("automagik-relay", 1);
    says(
        &workspace,
        "the relay to the gate stopped (exit code 1); it is started again after a pause",
    );
    says(
        &workspace,
        "the tailnet node is logged in as workspace1.tail0example.ts.net, and the relay to the gate is not running",
    );
    wait_for(&workspace, "the second relay", || {
        workspace.runs("automagik-relay").len() == 2
    });
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
    let mut workspace = Workspace::new(&settings_with_access(URL));
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
fn a_warning_beside_a_good_answer_from_docker_is_not_a_failure() {
    // Docker prints a warning on its standard error and answers all the same: the containers are seen as they
    // are, and all three start.
    let mut workspace = Workspace::new(&settings_with_access(URL));
    fs::write(workspace.dir.join("docker/warn-inspect"), "").expect("the warning");
    workspace.secret(TOKEN);
    workspace.start();
    says(&workspace, "the tunnel is started from");
    says(
        &workspace,
        "status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel is running",
    );
    assert!(!workspace.output().contains("Docker did not answer"));
    for name in ["automagik-server", "automagik-gate", "automagik-tunnel"] {
        assert_eq!(workspace.runs(name).len(), 1, "{name}");
    }
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
