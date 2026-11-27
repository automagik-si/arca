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
//! - `ARCA_PROOF_NETWORK`: the bridge of the machines' card (default the node's service bridge, `arcak-svcbr`). The
//!   pool's ACL `arca-runner-<pool>` is made for the proof and deleted after it.
//!
//! The network proof (`a_real_job_machine_reaches_dns_and_the_internet_and_nothing_of_the_bridge`) also needs, each
//! as `<address>:<port>`:
//! - `ARCA_PROOF_SERVICE`: a service on the same bridge (whether the host reaches it is reported: a service's own ACL
//!   may refuse the host too);
//! - `ARCA_PROOF_PRIVATE`: a private address outside the bridge, on a tcp port that answers from the host (checked
//!   first).
//!
//! and takes `ARCA_PROOF_DNS_NAME` (default `example.com`) and `ARCA_PROOF_INTERNET_URL` (default
//! `https://example.com`). The gateway is the bridge's own address; its port 8443 must answer from the host too.
//!
//! `cargo test -p arca-runners --test real_incus_runner -- --ignored --nocapture --test-threads=1` prints the measured
//! times.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arca_incus::IncusRead as _;
use arca_incus::runner_vm::RunnerVms;
use arca_runners::access::RegistrationAccess;
use arca_runners::config::PoolConfig;
use arca_runners::fake::{FakeGitHub, unusable_jit_config};
use arca_runners::incus::{GUEST_JIT_PATH, IncusLauncher};
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
    network: String,
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
        network: env("ARCA_PROOF_NETWORK").unwrap_or_else(|| arca_netproject::BRIDGE.into()),
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
            &self.network,
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

/// Deletes every machine of the pool, then the pool's ACL, when dropped, so a failed check leaves nothing behind.
struct Sweep(IncusLauncher);

impl Drop for Sweep {
    fn drop(&mut self) {
        for name in self.0.existing().unwrap_or_default() {
            if let Err(error) = self.0.destroy(&name) {
                eprintln!("could not delete {name}: {error}");
            }
        }
        if let Err(error) = self.0.remove_acl() {
            eprintln!("could not delete the ACL {}: {error}", self.0.acl_name());
        }
    }
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

fn secs(duration: Duration) -> String {
    format!("{:.2}", duration.as_secs_f64())
}

#[test]
#[ignore = "makes virtual machines on a real Incus; run by hand with ARCA_REAL_RUNNER_PROOF=1"]
fn a_real_machine_takes_its_configuration_runs_the_runner_once_and_goes() {
    let Some(proof) = proof() else { return };
    let _one = ONE_AT_A_TIME.blocking_lock();
    let launcher = proof.launcher();
    assert!(
        launcher.existing().expect("list").is_empty(),
        "the pool {} already has machines in {}",
        proof.pool,
        proof.project
    );
    let _sweep = Sweep(proof.launcher());
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
    assert_eq!(made["config"]["user.arcakernel.runner"], proof.pool.as_str());
    assert_eq!(made["config"]["limits.cpu"], proof.cpu.to_string());

    let clock = Instant::now();
    while !launcher.ready(&name).expect("the readiness read") {
        assert!(clock.elapsed() < Duration::from_secs(300), "no agent in 5 minutes");
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
        assert!(clock.elapsed() < Duration::from_secs(300), "the runner did not exit in 5 minutes");
    };
    let ran = clock.elapsed();
    assert_ne!(code, 0, "a configuration no runner can use is refused");

    // The configuration is gone from the machine, and is in none of its settings.
    let vms = RunnerVms::new(proof.socket.clone(), &proof.project, &proof.pool).expect("vms");
    assert_eq!(vms.pull_file(&name, GUEST_JIT_PATH).expect("a read"), None);
    let config = proof
        .get(&format!("/1.0/instances/{name}?project={}", proof.project))
        .expect("the machine");
    assert!(!config.to_string().contains(&jit[..64]));

    let logs = launcher
        .collect_logs(&name)
        .expect("the logs read")
        .expect("the guest packed its logs");
    let listing = untar(&logs, &["-tzf", "-"]);
    assert!(listing.contains("var/log/arca-job/runner.log"), "{listing}");
    assert!(listing.contains("actions-runner/_diag/Runner_"), "{listing}");
    let log = untar(&logs, &["-xzOf", "-", "var/log/arca-job/runner.log"]);
    assert!(log.contains(&format!("runner exited {code}")), "{log}");
    assert!(!log.contains(&jit[..64]), "the configuration is not in the log");

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
    let _sweep = Sweep(proof.launcher());
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
        RegistrationAccess::new(http.clone(), github.api_url(), "https://github.com/proof", Secret::new(TOKEN))
            .expect("access"),
    );
    let client = Arc::new(ScaleSetClient::new(http, access, &config.binding));
    let dir = std::env::temp_dir().join(format!("arca-real-runner-{}-{}", std::process::id(), hex()));
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
        assert!(clock.elapsed() < Duration::from_secs(600), "no record in 10 minutes");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let took = clock.elapsed();
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(60), task).await;

    let code = line["runnerExit"].as_i64().expect("an exit code");
    assert_ne!(code, 0, "{line}");
    assert_eq!(line["note"], format!("the runner exited {code} before it took a job"));
    assert_eq!(line["repository"], "");
    let logs = dir.join(line["logs"].as_str().expect("logs kept"));
    let mode = |path: &std::path::Path| std::fs::metadata(path).expect("there").permissions().mode() & 0o777;
    assert_eq!(mode(&logs), 0o600);
    assert_eq!(mode(logs.parent().expect("its folder")), 0o700);
    let log = untar(&std::fs::read(&logs).expect("the logs"), &["-xzOf", "-", "var/log/arca-job/runner.log"]);
    assert!(log.contains(&format!("runner exited {code}")), "{log}");
    assert!(launcher.existing().expect("list").is_empty(), "the machine is gone");
    let name = line["runner"].as_str().expect("its name");
    assert!(github.removed_runners().iter().any(|removed| removed == name));
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

/// Runs `script` with `sh -c` in the machine through the agent, and its exit code, within `limit`.
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
        assert!(clock.elapsed() < limit, "{script:?} still runs after {limit:?}");
    }
}

#[test]
#[ignore = "makes virtual machines on a real Incus; run by hand with ARCA_REAL_RUNNER_PROOF=1"]
fn a_real_job_machine_reaches_dns_and_the_internet_and_nothing_of_the_bridge() {
    let Some(proof) = proof() else { return };
    let _one = ONE_AT_A_TIME.blocking_lock();
    let deadline = Instant::now() + Duration::from_secs(600);
    let service = endpoint("ARCA_PROOF_SERVICE");
    let private = endpoint("ARCA_PROOF_PRIVATE");
    let dns_name = env("ARCA_PROOF_DNS_NAME").unwrap_or_else(|| "example.com".into());
    let url = env("ARCA_PROOF_INTERNET_URL").unwrap_or_else(|| "https://example.com".into());
    let bridge = proof
        .get(&format!("/1.0/networks/{}?project={}", proof.network, proof.project))
        .expect("the bridge");
    let gateway: std::net::Ipv4Addr = bridge["config"]["ipv4.address"]
        .as_str()
        .and_then(|address| address.split('/').next())
        .and_then(|address| address.parse().ok())
        .expect("the bridge's gateway");
    let incus_api = std::net::SocketAddr::from((gateway, 8443));
    // What the machine must not reach answers from the host: a refusal below is the card's, not a closed port. A
    // service on the bridge may refuse the host by its own ACL; that is reported.
    for (what, to) in [("the private address", private), ("the gateway's 8443", incus_api)] {
        assert!(host_reaches(to), "{what} does not answer from the host either");
    }
    let service_from_host = host_reaches(service);

    let launcher = proof.launcher();
    assert!(launcher.existing().expect("list").is_empty());
    let _sweep = Sweep(proof.launcher());
    let name = format!("{}-{}", proof.pool, hex());
    launcher
        .create(&MachineSpec {
            name: name.clone(),
            cpu: proof.cpu,
            memory_mib: proof.memory_mib,
            disk_gib: None,
        })
        .expect("the clone");
    let made = proof
        .get(&format!("/1.0/instances/{name}?project={}", proof.project))
        .expect("the machine");
    assert_eq!(made["devices"]["eth0"]["network"], proof.network.as_str());
    assert_eq!(made["devices"]["eth0"]["security.acls"], launcher.acl_name());
    while !launcher.ready(&name).expect("the readiness read") {
        assert!(Instant::now() < deadline, "no agent in time");
        std::thread::sleep(Duration::from_millis(500));
    }
    let vms = RunnerVms::new(proof.socket.clone(), &proof.project, &proof.pool).expect("vms");

    // DNS on the gateway answers, over udp and tcp, once the machine has its address (DHCP takes a few seconds).
    let dig = |tcp: &str| {
        format!(
            "timeout 10 dig {tcp} +short +time=3 +tries=2 @{gateway} {dns_name} A | grep -q '^[0-9]'"
        )
    };
    let clock = Instant::now();
    while guest(&vms, &name, &dig(""), Duration::from_secs(30)) != 0 {
        assert!(clock.elapsed() < Duration::from_secs(120), "no DNS answer from the gateway in 2 minutes");
        std::thread::sleep(Duration::from_secs(2));
    }
    let dns_after = clock.elapsed();
    assert_eq!(guest(&vms, &name, &dig("+tcp"), Duration::from_secs(30)), 0, "DNS over tcp");
    // The internet answers.
    let curl = format!("timeout 20 curl -sS -m 10 -o /dev/null {url}");
    assert_eq!(guest(&vms, &name, &curl, Duration::from_secs(30)), 0, "the internet");
    // The service on the same bridge, the gateway's 8443 and the private address do not.
    let mut refused = Vec::new();
    for (what, to) in [("the service", service), ("the gateway's 8443", incus_api), ("the private address", private)] {
        let connect = format!("timeout 8 bash -c 'exec 3<>/dev/tcp/{}/{}'", to.ip(), to.port());
        let code = guest(&vms, &name, &connect, Duration::from_secs(30));
        assert_ne!(code, 0, "{what} answers from the job machine");
        refused.push(format!("{what} (exit {code})"));
    }
    let ping_private = format!("timeout 8 ping -c 2 -W 2 {}", private.ip());
    assert_ne!(guest(&vms, &name, &ping_private, Duration::from_secs(30)), 0, "the private address answers icmp");
    assert!(Instant::now() < deadline, "the proof outlasted its bound");
    eprintln!(
        "real network proof: DNS on the gateway answered after {} s (udp and tcp), {url} answered; refused: {}; the \
         service answers the host: {service_from_host}",
        secs(dns_after),
        refused.join(", ")
    );
}
