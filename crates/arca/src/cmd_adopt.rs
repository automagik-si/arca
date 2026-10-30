//! `arca adopt`: takes one allowlisted container from the default project into its own kernel project, and back.
//!
//! - `plan --instance <i> --expect-ip <ipv4> --json --config <f>` runs the preflight with GET alone and prints the plan, its
//!   digest and every request the adoption would send.
//! - `apply --instance <i> --plan-digest <hex> [--dry-run] --config <f>` takes a fresh plan, whose digest must be the one given,
//!   then takes the state directory's lock and runs the steps through the adoption writer, recording each in the adoption
//!   record. When the record holds this instance's adoption unfinished, it resumes from the recorded plan instead, whose digest
//!   must be the one given, and runs no preflight. `--dry-run` takes no lock and sends no write: it prints the plan, or the steps
//!   left, and their requests.
//! - `release --instance <i> [--dry-run] --config <f>` returns the recorded adoption to the default project from wherever its
//!   record stands, keeping the pin and the snapshot. It refuses before any step when the instance has lost its pin.
//! - `verify --instance <i> --port <n> [--http-path <p>] [--wait <s>] --config <f>` checks the live MAC and IPv4 against the
//!   pre-image and that the service answers below 500, trying again for up to `--wait` seconds (60 by default).
//! - `status --instance <i> --json --config <f>` prints the record's plan and last step, and where the instance is now.
//!
//! A refusal before any write exits 2 ([`Failure::Refused`]): an instance the config does not list, a preflight check, a stale
//! digest, or a record that does not hold the adoption. A held lock, a failed step or a failed verify exits 1, and the record
//! names a failed step.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use arca_incus::adopt::{
    ADOPT_ENTRY_PREFIX, AdoptStep, AdoptTarget, AdoptWriter, NIC, PreImage, SOURCE_PROJECT,
    adoption_project_body, devices_of, mac_ok, pin_device,
};
use arca_incus::{IncusError, IncusRead, UnixIncus, running_on};
use arca_kernel::KernelError;
use arca_kernel::ownership::{IncusKind, OwnerClass, classify};
use serde_json::{Value, json};

use crate::adoptions::{AdoptionPlan, AdoptionRecord, Facts, Phase, release_steps, resume_steps};
use crate::config::{self, NodeConfig};
use crate::desired_store::lock_state_dir;
use crate::{Failure, Flags, parse_flags, usage_failure};

/// The kernel's config namespace: a key in it is a kernel mark or a tombstone.
const KERNEL_NAMESPACE: &str = "user.arcakernel.";
/// Container keys Incus counts as low level: a restricted project refuses each unless `restricted.containers.lowlevel` is
/// `allow`, as it refuses every `raw.*` key.
const LOW_LEVEL_KEYS: [&str; 5] = [
    "boot.host_shutdown_timeout",
    "linux.kernel_modules",
    "security.devlxd.images",
    "security.idmap.base",
    "security.idmap.size",
];
/// The instance key whose expiry Incus gives each new snapshot, the adoption's own among them.
const SNAPSHOT_EXPIRY: &str = "snapshots.expiry";

/// Devices by name, each with its keys.
type Devices = BTreeMap<String, BTreeMap<String, String>>;

/// Where a run of steps can be halted, as a kill of the process would halt it: after a step's intent line and before its
/// request, after its request and before its outcome line, or after its outcome line. The crash tests halt there; a
/// command run never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kill {
    AfterIntent,
    BeforeOutcome,
    AfterOutcome,
}

/// Asked at each [`Kill`] point of each step; `true` halts the run there, leaving the record and Incus as a kill would.
pub type Halt<'a> = &'a mut dyn FnMut(AdoptStep, Kill) -> bool;

/// Runs one `adopt` action.
pub fn adopt(argv: &[String]) -> Result<Option<Value>, Failure> {
    adopt_halting(argv, &mut |_, _| false)
}

/// Runs one `adopt` action, asking `halt` at each kill point of each step it runs.
pub fn adopt_halting(argv: &[String], halt: Halt<'_>) -> Result<Option<Value>, Failure> {
    let Some((action, rest)) = argv.split_first() else {
        return Err(usage_failure());
    };
    match action.as_str() {
        "plan" => plan(&parse_flags(
            rest,
            &["instance", "expect-ip", "config"],
            &["json"],
        )?),
        "apply" => apply(
            &parse_flags(rest, &["instance", "plan-digest", "config"], &["dry-run"])?,
            halt,
        ),
        "release" => release(
            &parse_flags(rest, &["instance", "config"], &["dry-run"])?,
            halt,
        ),
        "verify" => verify_command(&parse_flags(
            rest,
            &["instance", "port", "http-path", "wait", "config"],
            &[],
        )?),
        "status" => status(&parse_flags(rest, &["instance", "config"], &["json"])?),
        _ => Err(usage_failure()),
    }
}

/// What the preflight found: the plan and the facts beside its pre-image.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub plan: AdoptionPlan,
    pub facts: Facts,
}

fn refuse(why: impl Into<String>) -> Failure {
    Failure::Refused(why.into())
}

fn read_failure(error: IncusError) -> Failure {
    Failure::Runtime(format!("the preflight read failed: {error}"))
}

/// The config and the allowlisted target the flags name.
fn setup(flags: &Flags) -> Result<(NodeConfig, AdoptTarget), Failure> {
    let cfg = config::load(Path::new(flags.required("config")?))?;
    let instance = flags.required("instance")?;
    let target = cfg
        .kernel
        .adopt
        .iter()
        .find(|target| target.instance == instance)
        .cloned()
        .ok_or_else(|| refuse("the instance is not listed in kernel.adopt"))?;
    Ok((cfg, target))
}

fn plan(flags: &Flags) -> Result<Option<Value>, Failure> {
    if !flags.switch("json") {
        return Err(Failure::Usage(
            "adopt plan prints JSON: pass --json".to_string(),
        ));
    }
    let (cfg, target) = setup(flags)?;
    let expect: Ipv4Addr = flags
        .required("expect-ip")?
        .parse()
        .map_err(|_| Failure::Usage("--expect-ip is an IPv4 address".to_string()))?;
    let read = UnixIncus::new(cfg.incus.socket.clone());
    let found = preflight(&read, &cfg, &target, Some(expect))?;
    describe(&cfg, &target, &found, false)
}

fn apply(flags: &Flags, halt: Halt<'_>) -> Result<Option<Value>, Failure> {
    let (cfg, target) = setup(flags)?;
    let digest = flags.required("plan-digest")?;
    let dry_run = flags.switch("dry-run");
    // The lock is taken before anything is judged, so no server or second apply changes the state directory meanwhile.
    let _lock = if dry_run {
        None
    } else {
        Some(lock_state_dir(&cfg.state_dir).map_err(lock_failure)?)
    };
    let mut record = AdoptionRecord::open(&cfg.state_dir)?;
    let open = record.incomplete();
    if let Some(other) = open.iter().find(|open| **open != target.instance) {
        return Err(refuse(format!(
            "the record holds an adoption of {other} that has not completed; only one adoption may be open at a time"
        )));
    }
    // An open adoption goes on from its record, and a complete one is complete again with no write: a run killed after its
    // last outcome line cannot tell it finished.
    if !open.is_empty() || record.complete(&target.instance) {
        return resume(&cfg, &target, record, digest, dry_run, halt);
    }
    let read = UnixIncus::new(cfg.incus.socket.clone());
    let found = preflight(&read, &cfg, &target, None)?;
    if found.plan.digest != digest {
        return Err(refuse(
            "the plan digest is stale: it is not the digest of the plan the instance gives now",
        ));
    }
    if dry_run {
        return describe(&cfg, &target, &found, true);
    }
    let writer = writer(&cfg, &target, &found.plan)?;
    record.begin(&found.plan)?;
    record.facts(&found.facts)?;
    let ran = drive(
        &mut record,
        &writer,
        &target.instance,
        &found.plan.steps,
        "adoption",
        halt,
    )?;
    Ok(Some(json!({
        "instance": target.instance, "to": target.to, "digest": found.plan.digest,
        "snapshot": found.plan.snapshot, "steps": ran, "complete": true, "resumed": false,
    })))
}

/// Goes on with the open adoption of `target` from its recorded plan, never a fresh preflight: once the instance has moved,
/// a preflight would refuse it. The digest must be the recorded plan's, and a release that has begun is finished by
/// `release`, not undone here. Each step is checked by the writer against the recorded pre-image and the step table.
fn resume(
    cfg: &NodeConfig,
    target: &AdoptTarget,
    mut record: AdoptionRecord,
    digest: &str,
    dry_run: bool,
    halt: Halt<'_>,
) -> Result<Option<Value>, Failure> {
    let plan = recorded_plan(&record, target)?;
    if plan.digest != digest {
        return Err(refuse(
            "the plan digest is not the digest of the recorded plan this adoption resumes",
        ));
    }
    let last = record.last(&target.instance);
    if last.is_some_and(|(step, _)| AdoptStep::RELEASE.contains(&step)) {
        return Err(refuse(format!(
            "the adoption of {} is being released; arca adopt release finishes it",
            target.instance
        )));
    }
    let steps = resume_steps(&plan, last);
    let writer = writer(cfg, target, &plan)?;
    if dry_run {
        return Ok(Some(dry_run_of(&writer, &plan, &steps, last)));
    }
    record.select(&target.instance)?;
    let ran = drive(
        &mut record,
        &writer,
        &target.instance,
        &steps,
        "adoption",
        halt,
    )?;
    Ok(Some(json!({
        "instance": target.instance, "to": target.to, "digest": plan.digest,
        "snapshot": plan.snapshot, "steps": ran, "complete": true, "resumed": true,
    })))
}

/// Why a release closes an adopt step whose intent was the last line: the run that wrote it stopped before its outcome.
const INTERRUPTED: &str = "the run stopped before this step's outcome was recorded; the release took over and reads each outcome from Incus";

/// `release`: returns the recorded adoption of `target` to the default project, from wherever its record stands. It takes
/// the lock, except for `--dry-run`, which prints the steps left and their requests and sends nothing. The pin and the
/// snapshot are kept. An adoption already released in full is released again with no write.
fn release(flags: &Flags, halt: Halt<'_>) -> Result<Option<Value>, Failure> {
    let (cfg, target) = setup(flags)?;
    let dry_run = flags.switch("dry-run");
    let _lock = if dry_run {
        None
    } else {
        Some(lock_state_dir(&cfg.state_dir).map_err(lock_failure)?)
    };
    let mut record = AdoptionRecord::open(&cfg.state_dir)?;
    let plan = recorded_plan(&record, &target)?;
    if let Some(other) = record
        .incomplete()
        .into_iter()
        .find(|open| *open != target.instance)
    {
        return Err(refuse(format!(
            "the adoption of {other} has not completed; only one adoption may be open at a time"
        )));
    }
    let last = record.last(&target.instance);
    let steps = release_steps(last);
    let writer = writer(&cfg, &target, &plan)?;
    if steps.contains(&AdoptStep::MoveBack) {
        pin_kept(&cfg, &target, &writer)?;
    }
    if dry_run {
        return Ok(Some(dry_run_of(&writer, &plan, &steps, last)));
    }
    if !steps.is_empty() {
        record.select(&target.instance)?;
        // The record takes no intent while another is open, so an adopt step left at its intent is closed first.
        if let Some((pending, Phase::Intent)) = last
            && steps.first() != Some(&pending)
        {
            record.failed(pending, INTERRUPTED)?;
        }
    }
    let ran = drive(
        &mut record,
        &writer,
        &target.instance,
        &steps,
        "release",
        halt,
    )?;
    Ok(Some(json!({
        "instance": target.instance, "to": target.to, "digest": plan.digest,
        "steps": ran, "released": true,
    })))
}

/// Refuses, with GET alone and before any step, a release whose instance is in its adoption project without the pin. MoveBack
/// refuses an unpinned instance, so the release would otherwise stop and unmark it and only then stop. An instance not in the
/// adoption project is left to the steps, which read it again.
fn pin_kept(cfg: &NodeConfig, target: &AdoptTarget, writer: &AdoptWriter) -> Result<(), Failure> {
    let read = UnixIncus::new(cfg.incus.socket.clone());
    let failed =
        |error: IncusError| Failure::Runtime(format!("the release's pin read failed: {error}"));
    let body = match read.get(&format!(
        "/1.0/instances/{}?project={}",
        target.instance, target.to
    )) {
        Ok(body) => body,
        Err(IncusError::Status { code: 404, .. }) => return Ok(()),
        Err(error) => return Err(failed(error)),
    };
    let eth0 = devices_of(body.get("devices")).map_err(failed)?.remove(NIC);
    if eth0 != Some(pin_device(writer.pin())) {
        return Err(refuse(
            "the instance's eth0 in its adoption project is not the pin, so MoveBack would refuse it; the release stops before ReleaseStop and changes nothing",
        ));
    }
    Ok(())
}

/// The recorded plan of `target`, which must be its own: its digest verifies, its steps are the adoption's, and it moves the
/// instance between `default` and the adoption project the config names.
fn recorded_plan(record: &AdoptionRecord, target: &AdoptTarget) -> Result<AdoptionPlan, Failure> {
    let plan = record.plan(&target.instance).ok_or_else(|| {
        refuse(format!(
            "the record holds no adoption of {}",
            target.instance
        ))
    })?;
    if !plan.verify()
        || plan.steps != AdoptStep::ADOPT
        || plan.from != SOURCE_PROJECT
        || plan.to != target.to
    {
        return Err(refuse(
            "the recorded plan is not this adoption's: its digest, its steps or its projects do not match",
        ));
    }
    Ok(plan)
}

/// What a dry run of `steps` prints: the steps and every request they may send.
fn dry_run_of(
    writer: &AdoptWriter,
    plan: &AdoptionPlan,
    steps: &[AdoptStep],
    last: Option<(AdoptStep, Phase)>,
) -> Value {
    let requests: Vec<Value> = steps
        .iter()
        .flat_map(|step| writer.requests(*step))
        .map(|request| json!(request))
        .collect();
    json!({
        "dryRun": true, "instance": plan.instance, "to": plan.to, "digest": plan.digest,
        "last": last_json(last), "steps": steps, "requests": requests,
    })
}

fn last_json(last: Option<(AdoptStep, Phase)>) -> Value {
    last.map_or(
        Value::Null,
        |(step, phase)| json!({ "step": step, "phase": phase }),
    )
}

/// Runs `steps` through the writer, each with its intent line before its request and its outcome line after. A step whose
/// intent is the record's last line is run again without a second intent, and its outcome closes that intent: the writer reads
/// first, so a step that had landed sends nothing. A failed step is recorded and ends the run.
fn drive(
    record: &mut AdoptionRecord,
    writer: &AdoptWriter,
    instance: &str,
    steps: &[AdoptStep],
    what: &str,
    halt: Halt<'_>,
) -> Result<Vec<Value>, Failure> {
    let halted = |step: AdoptStep| Failure::Runtime(format!("the run was halted at {step:?}"));
    let mut ran = Vec::new();
    for &step in steps {
        if record.last(instance) != Some((step, Phase::Intent)) {
            record.intend(step)?;
        }
        if halt(step, Kill::AfterIntent) {
            return Err(halted(step));
        }
        match writer.run(step) {
            Ok(outcome) => {
                if halt(step, Kill::BeforeOutcome) {
                    return Err(halted(step));
                }
                record.done(step)?;
                if halt(step, Kill::AfterOutcome) {
                    return Err(halted(step));
                }
                ran.push(json!({ "step": step, "outcome": outcome }));
            }
            Err(error) => {
                let why = error.to_string();
                record.failed(step, &why)?;
                return Err(Failure::Runtime(format!(
                    "the {what} stopped at {step:?}: {why}; the record names the step"
                )));
            }
        }
    }
    Ok(ran)
}

/// How long the service has to accept the connection and answer the request.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `verify` keeps trying when no `--wait` is given, and the most `--wait` may name.
pub const VERIFY_WAIT: Duration = Duration::from_secs(60);
const MAX_VERIFY_WAIT_SECONDS: u64 = 3600;
/// The pause between two attempts of a waiting `verify`.
pub const VERIFY_INTERVAL: Duration = Duration::from_secs(2);

/// What `verify` proved: the instance's live MAC and IPv4 are the pre-image's, and the service there answered this status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub mac: String,
    pub ipv4: Ipv4Addr,
    pub http_status: u16,
}

/// The HTTP request `verify` makes: a TCP connect to the address and one GET of the path, answered within the timeout, as the
/// status code. [`http_probe`] is the one a command uses; a test passes its own.
pub type Probe<'a> = &'a dyn Fn(SocketAddrV4, &str, Duration) -> Result<u16, String>;

/// `verify` with the real probe.
pub fn verify(
    read: &dyn IncusRead,
    plan: &AdoptionPlan,
    project: &str,
    port: u16,
    path: &str,
) -> Result<Verified, Failure> {
    verify_with(read, plan, project, port, path, &http_probe)
}

/// Checks that the planned instance runs in `project` with the pre-image's MAC and IPv4 on `eth0`, then that the service at
/// `<ipv4>:<port>` answers a GET of `path` below 500 within [`VERIFY_TIMEOUT`]. It reads with GET alone and sends no write. A
/// port of 0 or a path that is not plain is a usage failure; every failed check is a runtime failure that names it.
pub fn verify_with(
    read: &dyn IncusRead,
    plan: &AdoptionPlan,
    project: &str,
    port: u16,
    path: &str,
    probe: Probe<'_>,
) -> Result<Verified, Failure> {
    if port == 0 {
        return Err(port_usage());
    }
    check_http_path(path)?;
    let failed = |why: String| Failure::Runtime(format!("verify failed: {why}"));
    let read_failed =
        |error: IncusError| Failure::Runtime(format!("the verify read failed: {error}"));
    let i = &plan.instance;
    let body = match read.get(&format!("/1.0/instances/{i}?project={project}")) {
        Ok(body) => body,
        Err(IncusError::Status { code: 404, .. }) => {
            return Err(failed(format!("the instance is not in project {project}")));
        }
        Err(error) => return Err(read_failed(error)),
    };
    if body["type"] != "container"
        || body["config"]["volatile.uuid"].as_str() != Some(plan.pre_image.uuid.as_str())
    {
        return Err(failed(
            "the instance is not the one the plan recorded".to_string(),
        ));
    }
    if body["status"] != "Running" {
        return Err(failed("the instance is not running".to_string()));
    }
    let state = read
        .get(&format!("/1.0/instances/{i}/state?project={project}"))
        .map_err(read_failed)?;
    let eth0 = &state["network"][NIC];
    if eth0["hwaddr"].as_str() != Some(plan.pre_image.mac.as_str()) {
        return Err(failed("eth0's MAC is not the pre-image's".to_string()));
    }
    let addresses: Vec<Ipv4Addr> = eth0["addresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|address| address["family"] == "inet" && address["scope"] == "global")
        .filter_map(|address| address["address"].as_str()?.parse().ok())
        .collect();
    if addresses != [plan.pre_image.ipv4] {
        return Err(failed("eth0's IPv4 is not the pre-image's".to_string()));
    }
    let status = probe(
        SocketAddrV4::new(plan.pre_image.ipv4, port),
        path,
        VERIFY_TIMEOUT,
    )
    .map_err(|why| failed(format!("the service did not answer on port {port}: {why}")))?;
    if status >= 500 {
        return Err(failed(format!("the service answered {status}")));
    }
    Ok(Verified {
        mac: plan.pre_image.mac.clone(),
        ipv4: plan.pre_image.ipv4,
        http_status: status,
    })
}

/// [`verify_with`] again every `interval` while it fails at run time and `wait` has not run out, so a lease still being
/// given or a service still starting is waited for. Once the time is out, the failure of the last attempt is the answer. With
/// no wait, it is one attempt. A usage failure is never retried.
#[allow(clippy::too_many_arguments)]
pub fn verify_until(
    read: &dyn IncusRead,
    plan: &AdoptionPlan,
    project: &str,
    port: u16,
    path: &str,
    wait: Duration,
    interval: Duration,
    probe: Probe<'_>,
) -> Result<Verified, Failure> {
    let deadline = Instant::now() + wait;
    loop {
        match verify_with(read, plan, project, port, path, probe) {
            Err(Failure::Runtime(_)) if Instant::now() < deadline => {
                thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
            }
            answer => return answer,
        }
    }
}

/// Connects to `addr`, sends `GET <path> HTTP/1.1` and returns the status code of the answer's status line, all within
/// `timeout`. A refused or timed-out connect, a closed connection and an answer that is not HTTP/1 are each an error.
pub fn http_probe(addr: SocketAddrV4, path: &str, timeout: Duration) -> Result<u16, String> {
    let deadline = Instant::now() + timeout;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| "no answer within the deadline".to_string())
    };
    let mut stream = TcpStream::connect_timeout(&SocketAddr::V4(addr), left()?)
        .map_err(|error| format!("the TCP connect failed ({})", error.kind()))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: arca-verify\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .set_write_timeout(Some(left()?))
        .and_then(|()| stream.write_all(request.as_bytes()))
        .map_err(|error| format!("the request was not sent ({})", error.kind()))?;
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    let end = loop {
        if let Some(end) = head.windows(2).position(|pair| pair == b"\r\n") {
            break end;
        }
        if head.len() > 4096 {
            return Err("the answer's status line is too long".to_string());
        }
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|error| format!("the answer cannot be read ({})", error.kind()))?;
        let read = stream
            .read(&mut chunk)
            .map_err(|error| format!("no answer ({})", error.kind()))?;
        if read == 0 {
            return Err("the connection closed before a status line".to_string());
        }
        head.extend_from_slice(&chunk[..read]);
    };
    let line = std::str::from_utf8(&head[..end]).unwrap_or("");
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || code.len() != 3
        || !code.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("the answer is not HTTP/1".to_string());
    }
    code.parse::<u16>()
        .ok()
        .filter(|code| (100..=599).contains(code))
        .ok_or_else(|| "the answer's status is not a status code".to_string())
}

fn port_usage() -> Failure {
    Failure::Usage("--port must be a port from 1 to 65535".to_string())
}

/// A path the request line may carry: `/` first, then visible ASCII only, at most 1024 bytes, so no space or line break
/// can reach the request.
fn check_http_path(path: &str) -> Result<(), Failure> {
    if path.starts_with('/')
        && path.len() <= 1024
        && path.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        Ok(())
    } else {
        Err(Failure::Usage(
            "--http-path must start with / and hold at most 1024 visible ASCII characters"
                .to_string(),
        ))
    }
}

/// `verify --instance <i> --port <n> [--http-path <p>] [--wait <s>] --config <f>`: runs [`verify_until`] where the record says
/// the instance is, in its adoption project after an adoption and in `default` after a release, for up to `--wait` seconds
/// (60 when not given, at most 3600). It takes no lock and sends no write. An adoption that is neither complete nor released
/// in full is refused.
fn verify_command(flags: &Flags) -> Result<Option<Value>, Failure> {
    let (cfg, target) = setup(flags)?;
    let port = flags
        .required("port")?
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(port_usage)?;
    let path = flags.value("http-path").unwrap_or("/");
    check_http_path(path)?;
    let wait = match flags.value("wait") {
        None => VERIFY_WAIT,
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= MAX_VERIFY_WAIT_SECONDS)
            .map(Duration::from_secs)
            .ok_or_else(|| {
                Failure::Usage(format!(
                    "--wait must be whole seconds from 0 to {MAX_VERIFY_WAIT_SECONDS}"
                ))
            })?,
    };
    let record = AdoptionRecord::open(&cfg.state_dir)?;
    let plan = recorded_plan(&record, &target)?;
    let project = if record.complete(&target.instance) {
        target.to.as_str()
    } else if record.released(&target.instance) {
        SOURCE_PROJECT
    } else {
        return Err(refuse(format!(
            "the adoption of {} is not complete; verify runs once apply or release has completed",
            target.instance
        )));
    };
    let read = UnixIncus::new(cfg.incus.socket.clone());
    let verified = verify_until(
        &read,
        &plan,
        project,
        port,
        path,
        wait,
        VERIFY_INTERVAL,
        &http_probe,
    )?;
    Ok(Some(json!({
        "instance": target.instance, "project": project, "mac": verified.mac,
        "ipv4": verified.ipv4.to_string(), "port": port, "httpPath": path,
        "httpStatus": verified.http_status, "verified": true,
    })))
}

fn status(flags: &Flags) -> Result<Option<Value>, Failure> {
    if !flags.switch("json") {
        return Err(Failure::Usage(
            "adopt status prints JSON: pass --json".to_string(),
        ));
    }
    let (cfg, target) = setup(flags)?;
    let record = AdoptionRecord::open(&cfg.state_dir)?;
    let plan = record.plan(&target.instance);
    let last = record.last(&target.instance);
    let read = UnixIncus::new(cfg.incus.socket.clone());
    let mut live = json!(null);
    for project in [SOURCE_PROJECT, target.to.as_str()] {
        match read.get(&format!(
            "/1.0/instances/{}?project={project}",
            target.instance
        )) {
            Ok(body) => {
                live = json!({
                    "project": project, "status": body["status"],
                    "entry": body["config"]["user.arcakernel.entry"],
                });
                break;
            }
            Err(IncusError::Status { code: 404, .. }) => {}
            Err(error) => return Err(read_failure(error)),
        }
    }
    Ok(Some(json!({
        "instance": target.instance, "to": target.to,
        "digest": plan.as_ref().map(|plan| plan.digest.clone()),
        "plan": plan,
        "last": last_json(last),
        "complete": record.complete(&target.instance),
        "released": record.released(&target.instance),
        "live": live,
    })))
}

/// The plan, its facts and every request its steps may send.
fn describe(
    cfg: &NodeConfig,
    target: &AdoptTarget,
    found: &Preflight,
    dry_run: bool,
) -> Result<Option<Value>, Failure> {
    let writer = writer(cfg, target, &found.plan)?;
    let requests: Vec<Value> = found
        .plan
        .steps
        .iter()
        .flat_map(|step| writer.requests(*step))
        .map(|request| json!(request))
        .collect();
    Ok(Some(json!({
        "dryRun": dry_run, "digest": found.plan.digest, "plan": found.plan,
        "facts": found.facts, "requests": requests,
    })))
}

fn writer(
    cfg: &NodeConfig,
    target: &AdoptTarget,
    plan: &AdoptionPlan,
) -> Result<AdoptWriter, Failure> {
    AdoptWriter::new(
        cfg.incus.socket.clone(),
        target.clone(),
        cfg.kernel.attach_networks.clone(),
        plan.pre_image.clone(),
    )
    .and_then(|writer| writer.with_snapshot(&plan.snapshot))
    .map_err(|error| refuse(error.to_string()))
}

fn lock_failure(error: KernelError) -> Failure {
    match error {
        KernelError::Locked { .. } => Failure::Runtime(
            "the state directory is held by another arca process; stop it before an adoption"
                .to_string(),
        ),
        other => Failure::Runtime(other.to_string()),
    }
}

/// The preflight, with GET alone. It refuses, giving the reason, an instance that is not a Foreign container in the default
/// project that the config lists and does not name as an identity phone; that has other than one NIC, `eth0`, on an attach
/// network, with no key the pin would drop; whose `volatile.eth0.hwaddr` is missing or is not the running MAC; whose IPv4 is not
/// `expect_ip` (when one is given) or is claimed by another lease or another device's `ipv4.address`; that names a profile
/// other than `default` or has no root of its own; that the move would refuse or change ([`moved_intact`]); that has a
/// backup; that an operation is running on; or whose adoption project exists but is not empty and marked for it.
pub fn preflight(
    read: &dyn IncusRead,
    cfg: &NodeConfig,
    target: &AdoptTarget,
    expect_ip: Option<Ipv4Addr>,
) -> Result<Preflight, Failure> {
    let i = target.instance.as_str();
    if cfg.identity_phones.iter().any(|phone| phone == i) {
        return Err(refuse("the instance is named in identityPhones"));
    }
    // The source is always the default project: an instance in `bench`, or in any other project, is not found there.
    let body = match read.get(&format!("/1.0/instances/{i}?project={SOURCE_PROJECT}")) {
        Ok(body) => body,
        Err(IncusError::Status { code: 404, .. }) => {
            return Err(refuse("the instance is not in project default"));
        }
        Err(error) => return Err(read_failure(error)),
    };
    if body["type"] != "container" {
        return Err(refuse("the instance is not a container"));
    }
    let own =
        text_map(&body["config"]).ok_or_else(|| refuse("the instance's config is not text"))?;
    match classify(&own, &BTreeMap::new()) {
        OwnerClass::Droidd => return Err(refuse("the instance is owned by droidd")),
        OwnerClass::Foreign if !own.keys().any(|key| key.starts_with(KERNEL_NAMESPACE)) => {}
        _ => return Err(refuse("the instance already carries a kernel mark")),
    }
    let profiles: Vec<String> = body["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_string))
        .collect();
    if profiles != ["default"] {
        return Err(refuse("the instance names a profile other than default"));
    }
    let expanded = devices_of(body.get("expanded_devices")).map_err(read_failure)?;
    let local = devices_of(body.get("devices")).map_err(read_failure)?;
    let nics: Vec<&String> = expanded
        .iter()
        .filter(|(_, device)| device.get("type").map(String::as_str) == Some("nic"))
        .map(|(name, _)| name)
        .collect();
    let nic = expanded
        .get(NIC)
        .filter(|_| nics == [NIC])
        .ok_or_else(|| refuse("the instance does not have exactly one NIC, and it named eth0"))?;
    let network = nic.get("network").cloned().unwrap_or_default();
    if !cfg.kernel.attach_networks.contains(&network) {
        return Err(refuse(
            "the instance's NIC is not on a network kernel.attachNetworks lists",
        ));
    }
    let pinless = nic
        .keys()
        .all(|key| matches!(key.as_str(), "type" | "name" | "network"));
    if !pinless || nic.get("name").is_some_and(|name| name != NIC) {
        return Err(refuse(
            "the instance's NIC sets a key the pin would replace",
        ));
    }
    let root = local
        .values()
        .find(|device| {
            device.get("type").map(String::as_str) == Some("disk")
                && device.get("path").map(String::as_str) == Some("/")
        })
        .ok_or_else(|| refuse("the instance's root is not its own device"))?;
    let expanded_config = text_map(&body["expanded_config"]).unwrap_or_else(|| own.clone());
    let project = adoption_project_body(target, &cfg.kernel.attach_networks);
    let restrictions = text_map(&project["config"]).unwrap_or_default();
    moved_intact(&restrictions, &own, &expanded_config, &local, &expanded)?;
    let backups = read
        .get(&format!(
            "/1.0/instances/{i}/backups?project={SOURCE_PROJECT}"
        ))
        .map_err(read_failure)?;
    if !backups.as_array().is_some_and(Vec::is_empty) {
        return Err(refuse("the instance has backups, which a move refuses"));
    }

    let mac = own
        .get(&format!("volatile.{NIC}.hwaddr"))
        .cloned()
        .unwrap_or_default();
    let state = read
        .get(&format!(
            "/1.0/instances/{i}/state?project={SOURCE_PROJECT}"
        ))
        .map_err(read_failure)?;
    let live = &state["network"][NIC];
    if !mac_ok(&mac) || live["hwaddr"].as_str() != Some(mac.as_str()) {
        return Err(refuse(
            "the instance's volatile.eth0.hwaddr is missing or is not its running MAC",
        ));
    }
    let addresses: Vec<Ipv4Addr> = live["addresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|address| address["family"] == "inet" && address["scope"] == "global")
        .filter_map(|address| address["address"].as_str()?.parse().ok())
        .collect();
    let ipv4 = match (addresses.as_slice(), expect_ip) {
        ([one], Some(expect)) if *one == expect => *one,
        ([one], None) => *one,
        _ => {
            return Err(refuse(
                "the instance's IPv4 is not the one --expect-ip names",
            ));
        }
    };
    claimed(read, target, &network, &mac, ipv4)?;
    if !running_on(read, IncusKind::Instance, SOURCE_PROJECT, None, i)
        .map_err(read_failure)?
        .is_empty()
    {
        return Err(refuse("an operation is running on the instance"));
    }
    project_free(read, target)?;
    let snapshots = read
        .get(&format!(
            "/1.0/instances/{i}/snapshots?project={SOURCE_PROJECT}"
        ))
        .map_err(read_failure)?
        .as_array()
        .map_or(0, Vec::len);

    let uuid = own.get("volatile.uuid").cloned().unwrap_or_default();
    if uuid.is_empty() {
        return Err(refuse("the instance has no volatile.uuid"));
    }
    let config: BTreeMap<String, String> = own
        .iter()
        .filter(|(key, _)| !key.starts_with("volatile."))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let pre = PreImage {
        uuid,
        created_at: body["created_at"].as_str().unwrap_or("").to_string(),
        mac,
        ipv4,
        profiles,
        config_hash: arca_watch::config_hash(&own),
        config,
        devices: expanded.clone(),
    };
    let facts = Facts {
        snapshots,
        pool: root.get("pool").cloned().unwrap_or_default(),
        root_size: root.get("size").cloned().unwrap_or_default(),
    };
    Ok(Preflight {
        plan: AdoptionPlan::new(target, pre)?,
        facts,
    })
}

/// Refuses what Incus would refuse or drop only at the move, after the stop. A stopped move copies the instance into the
/// adoption project under that project's restrictions, then deletes the source, and carries only its own devices and
/// config: a profile's device or key would be gone, and only the pinned `eth0` may come from a profile. A key or device the
/// restrictions refuse is refused here, the restrictions read from `restrictions`, the config of the project's create body,
/// so the two stay in step; a restriction the body leaves unset holds Incus's restricted default. `snapshots.expiry` is
/// refused because it would expire the adoption snapshot.
fn moved_intact(
    restrictions: &BTreeMap<String, String>,
    own: &BTreeMap<String, String>,
    expanded_config: &BTreeMap<String, String>,
    local: &Devices,
    expanded: &Devices,
) -> Result<(), Failure> {
    if own.contains_key(SNAPSHOT_EXPIRY) || expanded_config.contains_key(SNAPSHOT_EXPIRY) {
        return Err(refuse(
            "the instance sets snapshots.expiry, which could expire the adoption snapshot",
        ));
    }
    if let Some(name) = expanded
        .keys()
        .find(|name| name.as_str() != NIC && !local.contains_key(*name))
    {
        return Err(refuse(format!(
            "the instance's {name} device comes from a profile, and the move would drop it"
        )));
    }
    if let Some(key) = expanded_config.keys().find(|key| !own.contains_key(*key)) {
        return Err(refuse(format!(
            "the instance's {key} key comes from a profile, and the move would drop it"
        )));
    }
    if restrictions.get("restricted").map(String::as_str) != Some("true") {
        return Ok(());
    }
    let restriction = |key: &str| -> &str {
        restrictions.get(key).map_or(
            match key {
                "restricted.containers.privilege" => "unprivileged",
                "restricted.devices.nic" | "restricted.devices.disk" => "managed",
                _ => "block",
            },
            String::as_str,
        )
    };
    for (name, device) in expanded {
        let kind = device.get("type").map_or("", String::as_str);
        let field = |key: &str| device.get(key).map_or("", String::as_str);
        let allowed = match kind {
            "nic" => {
                !field("network").is_empty() && restriction("restricted.devices.nic") != "block"
            }
            // Incus always allows a pool-backed root. A host path needs `restricted.devices.disk` past its managed default,
            // and a custom volume stays in the default project, whose volumes the adoption project does not share.
            "disk" if field("path") == "/" && !field("pool").is_empty() => true,
            "disk" if field("pool").is_empty() => restriction("restricted.devices.disk") == "allow",
            "disk" => {
                return Err(refuse(format!(
                    "the instance's {name} device is a disk other than its root, on a volume the move cannot reach"
                )));
            }
            "none" => true,
            other => restriction(&format!("restricted.devices.{other}")) == "allow",
        };
        if !allowed {
            return Err(refuse(format!(
                "the adoption project's restrictions refuse the instance's {name} device ({kind})"
            )));
        }
    }
    for (key, value) in expanded_config {
        let on = !matches!(value.as_str(), "" | "false" | "0" | "off" | "no");
        let refused = match key.as_str() {
            "security.privileged" => {
                on && restriction("restricted.containers.privilege") != "allow"
            }
            "security.nesting" => on && restriction("restricted.containers.nesting") == "block",
            // Interception needs its own restriction lifted, and some of its keys count as low level: any is refused.
            intercept if intercept.starts_with("security.syscalls.intercept.") => {
                restriction("restricted.containers.interception") != "allow"
                    || restriction("restricted.containers.lowlevel") != "allow"
            }
            low if low.starts_with("raw.") || LOW_LEVEL_KEYS.contains(&low) => {
                restriction("restricted.containers.lowlevel") != "allow"
            }
            _ => false,
        };
        if refused {
            return Err(refuse(format!(
                "the adoption project's restrictions refuse the instance's {key} key"
            )));
        }
    }
    Ok(())
}

/// Refuses an IPv4 that another MAC's lease, or another instance's device, claims.
fn claimed(
    read: &dyn IncusRead,
    target: &AdoptTarget,
    network: &str,
    mac: &str,
    ipv4: Ipv4Addr,
) -> Result<(), Failure> {
    let ip = ipv4.to_string();
    let leases = read
        .get(&format!(
            "/1.0/networks/{network}/leases?project={SOURCE_PROJECT}"
        ))
        .map_err(read_failure)?;
    let leased = leases.as_array().into_iter().flatten().any(|lease| {
        lease["address"].as_str() == Some(ip.as_str()) && lease["hwaddr"].as_str() != Some(mac)
    });
    let listing = read
        .get("/1.0/instances?all-projects=true&recursion=1")
        .map_err(read_failure)?;
    let pinned = listing.as_array().into_iter().flatten().any(|row| {
        let me = row["name"] == target.instance.as_str() && row["project"] == SOURCE_PROJECT;
        // Both maps are read: a local device shows in each, and a listing may carry only one of them.
        !me && ["expanded_devices", "devices"].iter().any(|map| {
            row[*map]
                .as_object()
                .into_iter()
                .flatten()
                .any(|(_, device)| device["ipv4.address"].as_str() == Some(ip.as_str()))
        })
    });
    if leased || pinned {
        return Err(refuse(
            "the instance's IPv4 is claimed by another lease or device",
        ));
    }
    Ok(())
}

/// Refuses an adoption project that exists but is not marked for this adoption, or that holds an instance.
fn project_free(read: &dyn IncusRead, target: &AdoptTarget) -> Result<(), Failure> {
    let project = match read.get(&format!("/1.0/projects/{}", target.to)) {
        Ok(project) => project,
        Err(IncusError::Status { code: 404, .. }) => return Ok(()),
        Err(error) => return Err(read_failure(error)),
    };
    let entry = format!("{ADOPT_ENTRY_PREFIX}{}", target.instance);
    let config = text_map(&project["config"]).unwrap_or_default();
    let marked = matches!(classify(&config, &BTreeMap::new()), OwnerClass::Kernel { entry: owner } if owner.0 == entry);
    let listing = read
        .get("/1.0/instances?all-projects=true&recursion=1")
        .map_err(read_failure)?;
    let empty = !listing
        .as_array()
        .into_iter()
        .flatten()
        .any(|row| row["project"] == target.to.as_str());
    if !marked || !empty {
        return Err(refuse(
            "the adoption project exists, and it is not empty and marked for this adoption",
        ));
    }
    Ok(())
}

fn text_map(value: &Value) -> Option<BTreeMap<String, String>> {
    match value {
        Value::Null => Some(BTreeMap::new()),
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
            .collect(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn the_move_checks_follow_the_restrictions_the_project_body_sets() {
        let none = BTreeMap::new();
        let no_devices = Devices::new();
        let proxy: Devices = [("web".to_string(), map(&[("type", "proxy")]))].into();
        let nesting = map(&[("security.nesting", "true")]);
        let restricted = |extra: &[(&str, &str)]| {
            let mut config = map(&[("restricted", "true")]);
            config.extend(map(extra));
            config
        };
        assert!(moved_intact(&restricted(&[]), &none, &none, &proxy, &proxy).is_err());
        let allowed = restricted(&[("restricted.devices.proxy", "allow")]);
        assert!(moved_intact(&allowed, &none, &none, &proxy, &proxy).is_ok());
        assert!(
            moved_intact(&map(&[]), &nesting, &nesting, &proxy, &proxy).is_ok(),
            "an unrestricted project"
        );
        assert!(
            moved_intact(
                &restricted(&[]),
                &nesting,
                &nesting,
                &no_devices,
                &no_devices
            )
            .is_err()
        );
        let nest = restricted(&[("restricted.containers.nesting", "allow")]);
        assert!(moved_intact(&nest, &nesting, &nesting, &no_devices, &no_devices).is_ok());

        // The body the adoption sends leaves both at their restricted defaults.
        let target: AdoptTarget =
            serde_json::from_value(json!({ "instance": "svc1", "to": "arcak-adopt-svc1" }))
                .expect("a target");
        let sent = text_map(&adoption_project_body(&target, &["br1".to_string()])["config"])
            .expect("the config");
        assert!(moved_intact(&sent, &none, &none, &proxy, &proxy).is_err());
        assert!(moved_intact(&sent, &nesting, &nesting, &no_devices, &no_devices).is_err());
    }
}
