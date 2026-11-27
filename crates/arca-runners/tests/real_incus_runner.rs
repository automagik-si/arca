//! The Incus launcher and the pool against a REAL Incus and the real runner image. Ignored, and a no-op unless
//! `ARCA_REAL_RUNNER_PROOF=1`: it makes virtual machines on the Incus it is pointed at, so it is only ever run by hand.
//! It makes one machine at a time and deletes each one, also when a check fails. It calls no GitHub: the runner is
//! handed a configuration of GitHub's shape that no runner can use ([`unusable_jit_config`]), and the proof goes up to
//! the runner's own refusal, its exit code, the logs collected and the machine destroyed.
//!
//! - `ARCA_PROOF_SOCKET`: the Incus socket (default `/var/lib/incus/unix.socket`).
//! - `ARCA_PROOF_PROJECT`: the project the machines are made in.
//! - `ARCA_PROOF_IMAGE`: the runner image as a pool's `image` names it: `<instance>/<snapshot>` of the stopped machine
//!   `images/github-runner/build.sh` kept (`arca-runner-image-<tag>/image`), or the image it published
//!   (`arca-runner-image-<tag>`). It is only read.
//! - `ARCA_PROOF_POOL`: the pool name the machines are marked with and named after (default `arca-proof`).
//! - `ARCA_PROOF_CPU`, `ARCA_PROOF_MEMORY_MIB`: each machine's size (default 2 and 4096).
//! - `ARCA_PROOF_NETWORK`: a bridge the machines' card is on, as a pool's `network` names it. Absent, the pool gets its
//!   own bridge `arcarun-<hash>`, made for the proof and deleted after it, as the pool's ACL `arca-runner-<pool>` is.
//! - `ARCA_PROOF_SERVICE_CIDR`: the node's service subnet, which the machines' rules close (as `kernel.serviceCidr`).
//! - `ARCA_PROOF_RESERVED`: ranges the pool's own bridge must keep clear of, comma separated (as `kernel.reservedCidrs`).
//!
//! The network proof (`a_real_job_machine_reaches_dns_and_the_internet_and_nothing_else`) makes, besides the job
//! machines, two throwaway machines with no rules of its own, copies of the image: the target, with a card on the
//! pool's bridge and a card on `ARCA_PROOF_SERVICE_BRIDGE` (default `arcak-svcbr`) and a listener on tcp 8080 that the
//! host reaches on both addresses (checked first); and the relay, with one card on the pool's bridge only, which must
//! reach the target's service-bridge address through the host (the path a job would take) before a job is refused it.
//! So every refusal below is the job machine's rules, and nothing else's; each must be a connect that timed out
//! (exit 124: Incus drops a rejected packet in the bridge's forward path). It makes two job
//! machines at once: set `ARCA_PROOF_MEMORY_MIB=2048` when the project's memory limit is 8 GiB. It also needs
//! `ARCA_PROOF_PRIVATE`, `<address>:<port>` of a private address outside both bridges that answers from the host
//! (checked first), and takes `ARCA_PROOF_DNS_NAME` (default `example.com`) and `ARCA_PROOF_INTERNET_URL` (default
//! `https://example.com`). The gateway is the pool bridge's own address; its port 8443 must answer from the host too.
//! The throwaway machine is driven with the `incus` command line, which must reach the same Incus.
//!
//! `cargo test -p arca-runners --test real_incus_runner -- --ignored --nocapture --test-threads=1` prints the measured
//! times.
//!
//! Both tests refuse to start when the pool already has machines in the project, so a live pool's name never loses its
//! machines to a proof. A failed check deletes the machine on its way out, but an interrupted run (Ctrl-C, a killed
//! process) can leave one behind: list it with `incus list --project <project> user.arcakernel.runner=<pool>` and
//! delete what that lists with `incus delete --force --project <project> <name>`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arca_incus::IncusRead as _;
use arca_incus::runner_vm::RunnerVms;
use arca_runners::access::RegistrationAccess;
use arca_runners::config::PoolConfig;
use arca_runners::fake::{FakeGitHub, unusable_jit_config};
use arca_runners::incus::{GUEST_JIT_PATH, IncusLauncher, NetworkPlan};
use arca_runners::launcher::{Launcher, MachineSpec, RunnerStart};
use arca_runners::record::Recorder;
use arca_runners::scaleset::{Http, ScaleSetClient};
use arca_runners::{Pool, Secret, SignInBoard, Timing};
use serde_json::{Value, json};

/// One machine at a time: the two tests never run side by side.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TOKEN: &str = "fake-installation-token";

struct Proof {
    socket: PathBuf,
    project: String,
    image: String,
    pool: String,
    cpu: u32,
    memory_mib: u64,
    plan: NetworkPlan,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn proof() -> Option<Proof> {
    if env("ARCA_REAL_RUNNER_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_RUNNER_PROOF is not 1; nothing was done");
        return None;
    }
    Some(Proof {
        socket: env("ARCA_PROOF_SOCKET")
            .unwrap_or_else(|| "/var/lib/incus/unix.socket".into())
            .into(),
        project: env("ARCA_PROOF_PROJECT").expect("ARCA_PROOF_PROJECT"),
        image: env("ARCA_PROOF_IMAGE").expect("ARCA_PROOF_IMAGE"),
        pool: env("ARCA_PROOF_POOL").unwrap_or_else(|| "arca-proof".into()),
        cpu: env("ARCA_PROOF_CPU").map_or(2, |cpu| cpu.parse().expect("ARCA_PROOF_CPU")),
        memory_mib: env("ARCA_PROOF_MEMORY_MIB")
            .map_or(4096, |mib| mib.parse().expect("ARCA_PROOF_MEMORY_MIB")),
        plan: NetworkPlan {
            bridge: env("ARCA_PROOF_NETWORK"),
            service_cidr: env("ARCA_PROOF_SERVICE_CIDR"),
            reserved: env("ARCA_PROOF_RESERVED")
                .map(|ranges| ranges.split(',').map(str::to_string).collect())
                .unwrap_or_default(),
            // The kernel passes the host's routing table; the proof's bridge is checked against none.
            routes: vec![],
        },
    })
}

impl Proof {
    fn launcher(&self) -> IncusLauncher {
        IncusLauncher::new(
            self.socket.clone(),
            &self.project,
            &self.pool,
            &self.image,
            vec![],
            None,
            &self.plan,
        )
        .expect("the launcher")
    }

    fn get(&self, path: &str) -> Result<Value, arca_incus::IncusError> {
        arca_incus::UnixIncus::new(self.socket.clone()).get(path)
    }

    /// The image's source instance and its snapshot as they are now: what the proof must leave unchanged.
    fn image_state(&self) -> Option<Value> {
        let (instance, snapshot) = self.image.split_once('/')?;
        let project = &self.project;
        let read = self
            .get(&format!("/1.0/instances/{instance}?project={project}"))
            .expect("the image's machine");
        let snap = self
            .get(&format!(
                "/1.0/instances/{instance}/snapshots/{snapshot}?project={project}"
            ))
            .expect("the image's snapshot");
        // The reader answers with the reply's metadata already unwrapped.
        assert!(read["status"].is_string() && snap["created_at"].is_string());
        Some(json!({
            "status": read["status"],
            "config": read["config"],
            "snapshot": snap["created_at"],
        }))
    }
}

/// Deletes, when dropped, the throwaway machines, every machine of the pool, then the pool's ACL and its own bridge, so
/// a failed check leaves nothing behind. It is made before the first launcher, which makes the bridge and the ACL.
struct Sweep {
    vms: RunnerVms,
    own_bridge: bool,
    project: String,
    throwaway: Vec<String>,
}

impl Sweep {
    /// Refuses to start when the pool already has machines in the project: a live pool's machines are never swept, and
    /// no sweep exists yet to drop.
    fn new(proof: &Proof) -> Self {
        let vms =
            RunnerVms::new(proof.socket.clone(), &proof.project, &proof.pool).expect("the writer");
        assert!(
            vms.list().expect("list").is_empty(),
            "the pool {} already has machines in {}",
            proof.pool,
            proof.project
        );
        Self {
            vms,
            own_bridge: proof.plan.bridge.is_none(),
            project: proof.project.clone(),
            throwaway: Vec::new(),
        }
    }
}

impl Drop for Sweep {
    fn drop(&mut self) {
        for name in &self.throwaway {
            if !incus_cli(&["delete", "--force", "--project", &self.project, name]).0 {
                eprintln!("could not delete the throwaway machine {name}");
            }
        }
        for name in self.vms.list().unwrap_or_default() {
            if let Err(error) = self.vms.destroy(&name) {
                eprintln!("could not delete {name}: {error}");
            }
        }
        if let Err(error) = self.vms.remove_acl() {
            eprintln!("could not delete the ACL {}: {error}", self.vms.acl_name());
        }
        if self.own_bridge
            && let Err(error) = self.vms.remove_bridge()
        {
            eprintln!(
                "could not delete the bridge {}: {error}",
                self.vms.bridge_name()
            );
        }
    }
}

/// Runs the `incus` command line within two minutes: whether it succeeded, and what it printed.
fn incus_cli(args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("timeout")
        .arg("120")
        .arg("incus")
        .args(args)
        .output()
        .expect("the incus command line");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

fn hex() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("random");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One file of a gzip'd tar, by the host's own tar.
fn untar(archive: &[u8], args: &[&str]) -> String {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("tar")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("tar");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(archive)
        .expect("tar's input");
    let out = child.wait_with_output().expect("tar ran");
    assert!(out.status.success(), "tar {args:?} failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What of a JIT configuration must never reach a log: the whole configuration, each of its three parts as the runner
/// reads them, and each decoded value of the RSA key (the runner's secret), so a runner that traced the decoded key
/// would be caught too.
fn secret_parts(jit: &str) -> Vec<String> {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let decode = |text: &str| -> Value {
        serde_json::from_slice(&engine.decode(text).expect("base64")).expect("json")
    };
    let mut parts = vec![jit.to_string()];
    let outer = decode(jit);
    for (name, part) in outer.as_object().expect("the three parts") {
        let part = part.as_str().expect("a part");
        parts.push(part.to_string());
        if name == ".credentials_rsaparams" {
            for value in decode(part).as_object().expect("the key").values() {
                let value = value.as_str().expect("a key value");
                // The public exponent is four characters that any text may hold.
                if value.len() >= 16 {
                    parts.push(value.to_string());
                }
            }
        }
    }
    parts
}

/// Fails when any secret part of the configuration is in `text`.
fn assert_no_secret(text: &str, jit: &str, place: &str) {
    for part in secret_parts(jit) {
        assert!(
            !text.contains(&part),
            "a part of the configuration is in {place}"
        );
    }
}

fn secs(duration: Duration) -> String {
    format!("{:.2}", duration.as_secs_f64())
}

#[test]
#[ignore = "makes virtual machines on a real Incus; run by hand with ARCA_REAL_RUNNER_PROOF=1"]
fn a_real_machine_takes_its_configuration_runs_the_runner_once_and_goes() {
    let Some(proof) = proof() else { return };
    let _one = ONE_AT_A_TIME.blocking_lock();
    let _sweep = Sweep::new(&proof);
    let launcher = proof.launcher();
    assert!(
        launcher.existing().expect("list").is_empty(),
        "the pool {} already has machines in {}",
        proof.pool,
        proof.project
    );
    let image_before = proof.image_state();
    let name = format!("{}-{}", proof.pool, hex());
    let spec = MachineSpec {
        name: name.clone(),
        cpu: proof.cpu,
        memory_mib: proof.memory_mib,
        disk_gib: None,
    };

    let clock = Instant::now();
    launcher.create(&spec).expect("the clone");
    let created = clock.elapsed();
    let made = proof
        .get(&format!("/1.0/instances/{name}?project={}", proof.project))
        .expect("the machine");
    assert_eq!(made["type"], "virtual-machine", "{made}");
    assert_eq!(
        made["config"]["user.arcakernel.runner"],
        proof.pool.as_str()
    );
    assert_eq!(made["config"]["limits.cpu"], proof.cpu.to_string());

    let clock = Instant::now();
    while !launcher.ready(&name).expect("the readiness read") {
        assert!(
            clock.elapsed() < Duration::from_secs(300),
            "no agent in 5 minutes"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let booted = clock.elapsed();

    let jit = unusable_jit_config(7, &name, &hex());
    let clock = Instant::now();
    launcher
        .start_runner(
            &name,
            &Secret::new(&jit),
            &RunnerStart {
                runner_dir: arca_runners::config::runner_folder().to_string(),
                work_folder: arca_runners::config::work_folder().to_string(),
            },
        )
        .expect("the hand-over");
    let handed = clock.elapsed();

    let clock = Instant::now();
    let code = loop {
        if let Some(code) = launcher
            .runner_exited(&name, Duration::from_secs(5))
            .expect("the runner's state")
        {
            break code;
        }
        assert!(
            clock.elapsed() < Duration::from_secs(300),
            "the runner did not exit in 5 minutes"
        );
    };
    let ran = clock.elapsed();
    assert_ne!(code, 0, "a configuration no runner can use is refused");

    // The configuration is gone from the machine, and is in none of its settings.
    let vms = RunnerVms::new(proof.socket.clone(), &proof.project, &proof.pool).expect("vms");
    assert_eq!(vms.pull_file(&name, GUEST_JIT_PATH).expect("a read"), None);
    let config = proof
        .get(&format!("/1.0/instances/{name}?project={}", proof.project))
        .expect("the machine");
    assert_no_secret(&config.to_string(), &jit, "the machine's settings");

    let logs = launcher
        .collect_logs(&name)
        .expect("the logs read")
        .expect("the guest packed its logs");
    let listing = untar(&logs, &["-tzf", "-"]);
    assert!(listing.contains("var/log/arca-job/runner.log"), "{listing}");
    assert!(
        listing.contains("actions-runner/_diag/Runner_"),
        "{listing}"
    );
    let log = untar(&logs, &["-xzOf", "-", "var/log/arca-job/runner.log"]);
    assert!(log.contains(&format!("runner exited {code}")), "{log}");
    // Every file collected (runner.log and the runner's own _diag), whole.
    let everything = untar(&logs, &["-xzOf", "-"]);
    assert!(everything.len() > log.len(), "the _diag files are read too");
    assert_no_secret(&everything, &jit, "the collected logs");

    let clock = Instant::now();
    launcher.destroy(&name).expect("the destroy");
    let destroyed = clock.elapsed();
    assert!(launcher.existing().expect("list").is_empty());
    assert!(matches!(
        proof.get(&format!("/1.0/instances/{name}?project={}", proof.project)),
        Err(arca_incus::IncusError::Status { code: 404, .. })
    ));
    assert_eq!(proof.image_state(), image_before, "the image is unchanged");

    eprintln!(
        "real runner proof: create {} s, boot to agent {} s, hand-over {} s, runner refused (exit {code}) after {} s, \
         logs {} KiB, destroy {} s\nrunner.log:\n{log}",
        secs(created),
        secs(booted),
        secs(handed),
        secs(ran),
        logs.len() / 1024,
        secs(destroyed),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "makes virtual machines on a real Incus; run by hand with ARCA_REAL_RUNNER_PROOF=1"]
async fn the_pool_records_a_refused_runner_and_destroys_its_machine() {
    let Some(proof) = proof() else { return };
    let _one = ONE_AT_A_TIME.lock().await;
    let _sweep = Sweep::new(&proof);
    let image_before = proof.image_state();
    let label = format!("{}-{}vcpu", proof.pool, proof.cpu);
    let config: PoolConfig = serde_json::from_value(json!({
        "name": proof.pool,
        "binding": "proof",
        "url": "https://github.com/proof",
        "runnerGroupId": 3,
        "budget": { "cpu": proof.cpu, "memory": format!("{}MiB", proof.memory_mib) },
        "project": proof.project,
        "image": proof.image,
        "classes": [
            { "labels": [label], "cpu": proof.cpu, "memory": format!("{}MiB", proof.memory_mib), "warm": 1, "max": 1 }
        ]
    }))
    .expect("a pool");
    config.check().expect("valid");
    let github = FakeGitHub::start(TOKEN).await.expect("fake GitHub");
    let http = Http::new().expect("http");
    let access = Arc::new(
        RegistrationAccess::new(
            http.clone(),
            github.api_url(),
            "https://github.com/proof",
            Secret::new(TOKEN),
        )
        .expect("access"),
    );
    let client = Arc::new(ScaleSetClient::new(http, access, &config.binding));
    let dir =
        std::env::temp_dir().join(format!("arca-real-runner-{}-{}", std::process::id(), hex()));
    let recorder = Recorder::open(dir.clone(), 7).expect("recorder");
    // One machine only: the class's wait after the failure outlasts the test.
    let timing = Timing {
        backoff_base: Duration::from_secs(3600),
        ..Timing::default()
    };
    let launcher = Arc::new(proof.launcher());
    let pool = Pool::with_timing(
        config,
        client,
        Arc::clone(&launcher),
        SignInBoard::new(),
        recorder,
        "proof-node",
        timing,
    );
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(Arc::clone(&pool).run(receiver));

    let clock = Instant::now();
    let jobs = dir.join("jobs.jsonl");
    let line: Value = loop {
        if let Some(line) = std::fs::read_to_string(&jobs)
            .ok()
            .and_then(|text| text.lines().next().map(str::to_string))
        {
            break serde_json::from_str(&line).expect("a job line");
        }
        assert!(
            clock.elapsed() < Duration::from_secs(600),
            "no record in 10 minutes"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let took = clock.elapsed();
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(60), task).await;

    let code = line["runnerExit"].as_i64().expect("an exit code");
    assert_ne!(code, 0, "{line}");
    assert_eq!(
        line["note"],
        format!("the runner exited {code} before it took a job")
    );
    assert_eq!(line["repository"], "");
    let logs = dir.join(line["logs"].as_str().expect("logs kept"));
    let mode = |path: &std::path::Path| {
        std::fs::metadata(path).expect("there").permissions().mode() & 0o777
    };
    assert_eq!(mode(&logs), 0o600);
    assert_eq!(mode(logs.parent().expect("its folder")), 0o700);
    let archive = std::fs::read(&logs).expect("the logs");
    let log = untar(&archive, &["-xzOf", "-", "var/log/arca-job/runner.log"]);
    assert!(log.contains(&format!("runner exited {code}")), "{log}");
    let name = line["runner"].as_str().expect("its name");
    let minted = github.minted();
    let (_, jit) = minted
        .iter()
        .find(|(runner, _)| runner == name)
        .expect("its configuration was minted");
    let everything = untar(&archive, &["-xzOf", "-"]);
    assert!(everything.len() > log.len(), "the _diag files are kept too");
    assert_no_secret(&everything, jit, "the logs kept on the host");
    assert!(
        launcher.existing().expect("list").is_empty(),
        "the machine is gone"
    );
    assert!(
        github
            .removed_runners()
            .iter()
            .any(|removed| removed == name)
    );
    assert_eq!(proof.image_state(), image_before, "the image is unchanged");
    eprintln!(
        "real pool proof: record after {} s (boot {} s, machine {} s), runner exit {code}, note: {}",
        secs(took),
        line["bootSeconds"],
        line["machineSeconds"],
        line["note"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tcp port that answers: an address and a port, `<address>:<port>`.
fn endpoint(name: &str) -> std::net::SocketAddr {
    env(name)
        .unwrap_or_else(|| panic!("{name} is needed for the network proof"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} is <address>:<port>"))
}

/// Whether a tcp connection from the host to `to` is accepted within five seconds.
fn host_reaches(to: std::net::SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&to, Duration::from_secs(5)).is_ok()
}

/// Runs `script` with `sh -c` in the job machine through the agent, and its exit code, within `limit`.
fn guest(vms: &RunnerVms, name: &str, script: &str, limit: Duration) -> i64 {
    use arca_incus::runner_vm::ExecState;
    let operation = vms
        .exec(name, &["sh", "-c", script], &[])
        .expect("the command starts");
    let clock = Instant::now();
    loop {
        match vms
            .exec_state(&operation, Duration::from_secs(5))
            .expect("the command's state")
        {
            ExecState::Exited(code) => return code,
            ExecState::Gone => return -1,
            ExecState::Running => {}
        }
        assert!(
            clock.elapsed() < limit,
            "{script:?} still runs after {limit:?}"
        );
    }
}

/// Runs `script` with `sh -c` in the throwaway machine, and whether it exited 0.
fn throwaway(project: &str, name: &str, script: &str) -> bool {
    throwaway_code(project, name, script) == 0
}

/// Runs `script` with `sh -c` in the throwaway machine, and its exit code, which `incus exec` passes through. A command
/// line that hangs is killed after two minutes (exit 137, never 124), and one that ends by a signal is -1.
fn throwaway_code(project: &str, name: &str, script: &str) -> i64 {
    let out = std::process::Command::new("timeout")
        .args([
            "--signal=KILL",
            "120",
            "incus",
            "exec",
            "--project",
            project,
            name,
            "--",
        ])
        .args(["sh", "-c", script])
        .output()
        .expect("the incus command line");
    out.status.code().map_or(-1, i64::from)
}

/// Whether the CIDR `subnet` holds `ip`.
fn holds(subnet: &str, ip: std::net::Ipv4Addr) -> bool {
    let (base, prefix) = subnet.split_once('/').expect("a CIDR");
    let base = u32::from(base.parse::<std::net::Ipv4Addr>().expect("an address"));
    let mask = u32::MAX
        .checked_shl(32 - prefix.parse::<u32>().expect("a prefix"))
        .unwrap_or(0);
    u32::from(ip) & mask == base & mask
}

/// The machine's IPv4 address inside `subnet`, as Incus reports it (from the agent, for a VM), once it has one.
fn address_in(proof: &Proof, name: &str, subnet: &str, deadline: Instant) -> std::net::Ipv4Addr {
    loop {
        let state = proof
            .get(&format!(
                "/1.0/instances/{name}/state?project={}",
                proof.project
            ))
            .expect("the machine's state");
        let found = state["network"]
            .as_object()
            .into_iter()
            .flat_map(|cards| cards.values())
            .flat_map(|card| card["addresses"].as_array().cloned().unwrap_or_default())
            .filter(|address| address["family"] == "inet")
            .filter_map(|address| address["address"].as_str()?.parse().ok())
            .find(|ip| holds(subnet, *ip));
        if let Some(ip) = found {
            return ip;
        }
        assert!(
            Instant::now() < deadline,
            "{name} has no address in {subnet} in time"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A connect from inside a machine: exit 0 when `to` accepts within eight seconds.
fn connect(to: std::net::Ipv4Addr, port: u16) -> String {
    format!("timeout 8 bash -c 'exec 3<>/dev/tcp/{to}/{port}'")
}

const LISTEN: &str = "setsid nohup python3 -m http.server 8080 >/dev/null 2>&1 </dev/null &";

#[test]
#[ignore = "makes virtual machines on a real Incus; run by hand with ARCA_REAL_RUNNER_PROOF=1"]
fn a_real_job_machine_reaches_dns_and_the_internet_and_nothing_else() {
    let Some(proof) = proof() else { return };
    let _one = ONE_AT_A_TIME.blocking_lock();
    let deadline = Instant::now() + Duration::from_secs(900);
    let private = endpoint("ARCA_PROOF_PRIVATE");
    let service_bridge =
        env("ARCA_PROOF_SERVICE_BRIDGE").unwrap_or_else(|| arca_netproject::BRIDGE.into());
    let service_cidr = proof
        .plan
        .service_cidr
        .clone()
        .expect("ARCA_PROOF_SERVICE_CIDR");
    let dns_name = env("ARCA_PROOF_DNS_NAME").unwrap_or_else(|| "example.com".into());
    let url = env("ARCA_PROOF_INTERNET_URL").unwrap_or_else(|| "https://example.com".into());
    let mut sweep = Sweep::new(&proof);
    let launcher = proof.launcher();
    assert!(launcher.existing().expect("list").is_empty());
    let bridge = proof
        .get(&format!(
            "/1.0/networks/{}?project={}",
            launcher.bridge(),
            proof.project
        ))
        .expect("the pool's bridge");
    let address = bridge["config"]["ipv4.address"]
        .as_str()
        .expect("its address")
        .to_string();
    let gateway: std::net::Ipv4Addr = address
        .split('/')
        .next()
        .unwrap()
        .parse()
        .expect("the gateway");
    let prefix: u32 = address
        .split('/')
        .nth(1)
        .unwrap()
        .parse()
        .expect("its prefix");
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    let subnet = format!(
        "{}/{prefix}",
        std::net::Ipv4Addr::from(u32::from(gateway) & mask)
    );
    let incus_api = std::net::SocketAddr::from((gateway, 8443));
    assert!(
        host_reaches(private),
        "the private address does not answer from the host"
    );
    assert!(
        host_reaches(incus_api),
        "the gateway's 8443 does not answer from the host"
    );

    // The throwaway machine: no rules of its own, on the pool's bridge and on the service bridge.
    let (instance, snapshot) = proof
        .image
        .split_once('/')
        .expect("the image is <instance>/<snapshot>");
    let project = proof.project.as_str();
    let copy = |name: &str, cards: &[(&str, &str)]| {
        let (made, out) = incus_cli(&[
            "copy",
            "--project",
            project,
            &format!("{instance}/{snapshot}"),
            name,
            "-c",
            "limits.cpu=1",
            "-c",
            "limits.memory=1GiB",
            "-c",
            "boot.autostart=false",
        ]);
        assert!(made, "the throwaway copy {name}: {out}");
        for (card, network) in cards {
            let (added, out) = incus_cli(&[
                "config",
                "device",
                "add",
                "--project",
                project,
                name,
                card,
                "nic",
                &format!("network={network}"),
            ]);
            assert!(added, "{name}'s {card}: {out}");
        }
        let (started, out) = incus_cli(&["start", "--project", project, name]);
        assert!(started, "{name}'s start: {out}");
    };
    let target = format!("{}-target-{}", proof.pool, hex());
    sweep.throwaway.push(target.clone());
    copy(
        &target,
        &[
            ("eth0", launcher.bridge()),
            ("eth1", service_bridge.as_str()),
        ],
    );
    // The relay: one card, on the pool's bridge, and no rules, like a job machine without its ACL.
    let relay = format!("{}-relay-{}", proof.pool, hex());
    sweep.throwaway.push(relay.clone());
    copy(&relay, &[("eth0", launcher.bridge())]);

    // Two job machines of the pool.
    let job = |n: u32| {
        let name = format!("{}-{n}-{}", proof.pool, hex());
        launcher
            .create(&MachineSpec {
                name: name.clone(),
                cpu: proof.cpu,
                memory_mib: proof.memory_mib,
                disk_gib: None,
            })
            .expect("the clone");
        name
    };
    let (first, second) = (job(0), job(1));
    let made = proof
        .get(&format!("/1.0/instances/{first}?project={}", proof.project))
        .expect("the machine");
    let cards: Vec<&String> = made["expanded_devices"]
        .as_object()
        .expect("devices")
        .iter()
        .filter(|(_, device)| device["type"] == "nic")
        .map(|(name, _)| name)
        .collect();
    assert_eq!(cards, vec!["eth0"], "Arca's card and no other");
    assert_eq!(
        made["expanded_devices"]["eth0"]["network"],
        launcher.bridge()
    );
    assert_eq!(
        made["expanded_devices"]["eth0"]["security.acls"],
        launcher.acl_name()
    );

    // The throwaways: every card up; the target listens, and the host reaches it on both addresses.
    for name in [&target, &relay] {
        while !throwaway(project, name, "true") {
            assert!(
                Instant::now() < deadline,
                "{name}'s agent did not answer in time"
            );
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(throwaway(
            project,
            name,
            "printf 'network:\n  version: 2\n  ethernets:\n    all:\n      match: {name: \"en*\"}\n      dhcp4: true\n' > /etc/netplan/99-proof.yaml && chmod 600 /etc/netplan/99-proof.yaml && netplan apply"
        ));
    }
    assert!(throwaway(project, &target, LISTEN));
    let target_here = address_in(&proof, &target, &subnet, deadline);
    let target_service = address_in(&proof, &target, &service_cidr, deadline);
    for to in [target_here, target_service] {
        let to = std::net::SocketAddr::from((to, 8080));
        let clock = Instant::now();
        while !host_reaches(to) {
            assert!(
                clock.elapsed() < Duration::from_secs(60),
                "the throwaway's {to} does not answer the host"
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    // The forwarded path's control: the relay, on the pool's bridge with no rules, reaches the target's service-bridge
    // address through the host. Without it, a host that drops forwarding between the bridges would pass probe 6.
    address_in(&proof, &relay, &subnet, deadline);
    let clock = Instant::now();
    let relay_code = loop {
        let code = throwaway_code(project, &relay, &connect(target_service, 8080));
        if code == 0 || clock.elapsed() > Duration::from_secs(60) {
            break code;
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    assert_eq!(
        relay_code, 0,
        "the relay on the pool's bridge does not reach the service bridge through the host"
    );

    let vms = RunnerVms::new(proof.socket.clone(), &proof.project, &proof.pool).expect("vms");
    for name in [&first, &second] {
        while !launcher.ready(name).expect("the readiness read") {
            assert!(Instant::now() < deadline, "no agent in time");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    // 1. DNS on the gateway answers, over udp and tcp, once the machine has its address.
    let dig = |tcp: &str| {
        format!(
            "timeout 10 dig {tcp} +short +time=3 +tries=2 @{gateway} {dns_name} A | grep -q '^[0-9]'"
        )
    };
    let clock = Instant::now();
    for name in [&first, &second] {
        while guest(&vms, name, &dig(""), Duration::from_secs(30)) != 0 {
            assert!(
                clock.elapsed() < Duration::from_secs(180),
                "no DNS answer from the gateway in 3 minutes"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    let dns_after = clock.elapsed();
    assert_eq!(
        guest(&vms, &first, &dig("+tcp"), Duration::from_secs(30)),
        0,
        "DNS over tcp"
    );
    // 2. The internet answers.
    let curl = format!("timeout 20 curl -sS -m 10 -o /dev/null {url}");
    assert_eq!(
        guest(&vms, &first, &curl, Duration::from_secs(30)),
        0,
        "the internet"
    );

    // The first job machine listens on 8080; it answers itself on its own address.
    let first_ip = address_in(&proof, &first, &subnet, deadline);
    assert_eq!(guest(&vms, &first, LISTEN, Duration::from_secs(30)), 0);
    let clock = Instant::now();
    while guest(
        &vms,
        &first,
        &connect(first_ip, 8080),
        Duration::from_secs(30),
    ) != 0
    {
        assert!(
            clock.elapsed() < Duration::from_secs(30),
            "the job's listener does not answer itself"
        );
        std::thread::sleep(Duration::from_secs(1));
    }

    let mut refused = Vec::new();
    // A refusal is a connect that timed out: 124. Any other code is the probe failing, not the rules.
    let mut refuse = |what: &str, code: i64| {
        assert_eq!(code, 124, "{what}: the connect did not time out");
        refused.push(format!("{what} (exit {code})"));
    };
    // 3. The throwaway on the same bridge, which answers the host: refused.
    refuse(
        "the throwaway on the same bridge",
        guest(
            &vms,
            &first,
            &connect(target_here, 8080),
            Duration::from_secs(30),
        ),
    );
    // 4. The throwaway into the job machine: refused.
    refuse(
        "the throwaway into the job machine",
        throwaway_code(project, &target, &connect(first_ip, 8080)),
    );
    // 5. The second job machine into the first: refused.
    refuse(
        "the second job machine into the first",
        guest(
            &vms,
            &second,
            &connect(first_ip, 8080),
            Duration::from_secs(30),
        ),
    );
    // 6. A service address on the service bridge, routed through the host, which the relay reached: refused.
    refuse(
        "the service bridge through the host (the relay reached it)",
        guest(
            &vms,
            &first,
            &connect(target_service, 8080),
            Duration::from_secs(30),
        ),
    );
    // And the gateway's 8443 and a private address outside both bridges, which answer the host.
    refuse(
        "the gateway's 8443",
        guest(
            &vms,
            &first,
            &connect(gateway, 8443),
            Duration::from_secs(30),
        ),
    );
    let private_ip = match private.ip() {
        std::net::IpAddr::V4(ip) => ip,
        std::net::IpAddr::V6(_) => panic!("ARCA_PROOF_PRIVATE is an IPv4 address"),
    };
    refuse(
        "the private address",
        guest(
            &vms,
            &first,
            &connect(private_ip, private.port()),
            Duration::from_secs(30),
        ),
    );
    assert!(Instant::now() < deadline, "the proof outlasted its bound");
    eprintln!(
        "real network proof: DNS on the gateway answered both job machines after {} s (udp and tcp on the first), {url} \
         answered; refused: {}",
        secs(dns_after),
        refused.join(", ")
    );
}
