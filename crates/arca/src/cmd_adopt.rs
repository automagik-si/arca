//! `arca adopt`: takes one allowlisted container from the default project into its own kernel project.
//!
//! - `plan --instance <i> --expect-ip <ipv4> --json --config <f>` runs the preflight with GET alone and prints the plan, its
//!   digest and every request the adoption would send.
//! - `apply --instance <i> --plan-digest <hex> [--dry-run] --config <f>` takes a fresh plan, whose digest must be the one given,
//!   then takes the state directory's lock and runs the steps through the adoption writer, recording each in the adoption
//!   record. `--dry-run` takes no lock and sends no write: it prints the plan and its requests.
//! - `status --instance <i> --json --config <f>` prints the record's plan and last step, and where the instance is now.
//!
//! A refusal before any write exits 2 ([`Failure::Refused`]): an instance the config does not list, a preflight check, or a stale
//! digest. A held lock or a failed step exits 1, and the record names the step.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;

use arca_incus::adopt::{
    ADOPT_ENTRY_PREFIX, AdoptStep, AdoptTarget, AdoptWriter, NIC, PreImage, SOURCE_PROJECT,
    adoption_project_body, devices_of, mac_ok,
};
use arca_incus::{IncusError, IncusRead, UnixIncus, running_on};
use arca_kernel::KernelError;
use arca_kernel::ownership::{IncusKind, OwnerClass, classify};
use serde_json::{Value, json};

use crate::adoptions::{AdoptionPlan, AdoptionRecord, Facts, Phase};
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

/// Runs one `adopt` action.
pub fn adopt(argv: &[String]) -> Result<Option<Value>, Failure> {
    let Some((action, rest)) = argv.split_first() else {
        return Err(usage_failure());
    };
    match action.as_str() {
        "plan" => plan(&parse_flags(
            rest,
            &["instance", "expect-ip", "config"],
            &["json"],
        )?),
        "apply" => apply(&parse_flags(
            rest,
            &["instance", "plan-digest", "config"],
            &["dry-run"],
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

fn apply(flags: &Flags) -> Result<Option<Value>, Failure> {
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
    if let Some(open) = record.incomplete().first() {
        return Err(refuse(format!(
            "the record holds an adoption of {open} that has not completed; it goes on from its record, not from a new plan"
        )));
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
    let mut ran = Vec::new();
    for step in &found.plan.steps {
        record.intend(*step)?;
        match writer.run(*step) {
            Ok(outcome) => {
                record.done(*step)?;
                ran.push(json!({ "step": step, "outcome": outcome }));
            }
            Err(error) => {
                let why = error.to_string();
                record.failed(*step, &why)?;
                return Err(Failure::Runtime(format!(
                    "the adoption stopped at {step:?}: {why}; the record names the step"
                )));
            }
        }
    }
    Ok(Some(json!({
        "instance": target.instance, "to": target.to, "digest": found.plan.digest,
        "snapshot": found.plan.snapshot, "steps": ran, "complete": true,
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
        "last": last.map(|(step, phase)| json!({ "step": step, "phase": phase })),
        "complete": last == Some((AdoptStep::Start, Phase::Done)),
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
