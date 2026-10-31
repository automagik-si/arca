//! The adoption writer: the one path that writes outside the kernel's project family. It takes one existing container from the
//! default project into an adoption project of its own, and back, through a closed table of steps. It is compiled only with
//! the `write` feature.
//!
//! Each step names the project, the owner class and the status it expects the instance in ([`AdoptWriter::expect`]). Before
//! each request the writer reads the instance there and refuses unless it is a container whose `volatile.uuid` is the one the
//! writer expects (the recorded pre-image's, or the one a confirmed copy gave it), in that class and that status; Mark also
//! takes the instance running. A step whose outcome already reads true sends nothing. Every request is one of the table's
//! ([`AdoptWriter::requests`]): no write to a network, a profile or an ACL; no project write but the one create and the one
//! delete of the emptied adoption project; and no other DELETE. Every request passes `exchange`, whose guard refuses a
//! project PATCH.
//!
//! The NIC is pinned on the device itself before the move and never unpinned: `devices.eth0` names the network, the pre-image
//! MAC and the pre-image IPv4, and keeps the NIC's ACL keys ([`pin_patch`], [`NIC_KEPT_KEYS`]). No step removes or renames
//! `eth0`, so Incus's `deviceVolatileReset` never clears a key the instance's MAC depends on.
//!
//! The adoption project allows exactly what the instance already uses ([`Allowances`]): the plan derives each `restricted.*`
//! key from the instance's config and devices ([`needed_allowances`]), the owner approves them with the plan's digest, and the
//! project is created with those keys and no other. A root disk that a profile gives is made the instance's own before the
//! move (LocalizeRoot), since the adoption project has its own profiles, and a release takes it back off (RestoreRoot) once
//! the instance is in `default` again and its profiles give the same root.
//!
//! Incus 7.5 runs a stopped instance's project move as a copy, which gets a new `volatile.uuid`. A move is confirmed under a
//! new uuid only by the copy rule ([`AdoptWriter::uuid`]), and the writer then follows the new uuid.
//!
//! A plan that found the instance stopped ([`PreImage::stopped`]) neither stops nor starts it: its steps leave out Stop and
//! Start, and its release ReleaseStop and ReleaseStart ([`AdoptStep::STOP_START`]). Those steps have no request in its table,
//! so no stop or start can be sent, and every step it runs expects the instance stopped ([`AdoptWriter::expects`]).

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use arca_kernel::desired::EntryId;
use arca_kernel::ownership::{IncusKind, KERNEL_ENTRY_KEY, ObjectRef, OwnerClass, classify};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::read::{
    IncusError, IncusRead, TIMEOUT, Unconfirmed, UnixIncus, checked_envelope, operation_names,
    running_operations,
};
use crate::write::{ProjectFamily, UnixIncusWriter};

/// The prefix of an adopted instance's kernel entry: the entry is `adopt-<instance>`.
pub const ADOPT_ENTRY_PREFIX: &str = "adopt-";
/// The prefix every adoption project's name carries.
pub const ADOPT_PROJECT_PREFIX: &str = "arcak-adopt-";
/// The prefix of the adoption snapshot's name: `arca-adopt-<the plan digest's first 12 hex characters>`.
pub const SNAPSHOT_PREFIX: &str = "arca-adopt-";
/// The project an adopted instance comes from and is released to.
pub const SOURCE_PROJECT: &str = "default";
/// The one NIC an adopted instance has, and the device the pin sets.
pub const NIC: &str = "eth0";
/// The seconds a graceful stop is given before the forced one.
pub const STOP_TIMEOUT_SECONDS: u64 = 60;
/// The kernel's own config namespace. A Foreign instance carries no key in it.
const KERNEL_NAMESPACE: &str = "user.arcakernel.";
/// The grant: the networks a restricted project's NICs may name.
pub const NETWORKS_ACCESS: &str = "restricted.networks.access";
/// The keys `eth0` may set beside [`NIC_KEPT_KEYS`]: the three that name it, and the two the pin sets, which must already agree
/// with the pin when the instance sets them.
pub const PIN_KEYS: [&str; 5] = ["type", "name", "network", "hwaddr", "ipv4.address"];
/// The `eth0` keys the pin keeps as they are: the NIC's ACLs and their default actions. A bridged NIC looks its ACLs up in the
/// project that serves its network, which is `default` for a project whose `features.networks` is off, as an adoption
/// project's is; and no `restricted.*` key governs an ACL. So they need no allowance.
pub const NIC_KEPT_KEYS: [&str; 5] = [
    "security.acls",
    "security.acls.default.egress.action",
    "security.acls.default.egress.logged",
    "security.acls.default.ingress.action",
    "security.acls.default.ingress.logged",
];
/// The value every allowance but the disk paths and the grant is set to.
const ALLOW: &str = "allow";
const NESTING: &str = "restricted.containers.nesting";
const INTERCEPTION: &str = "restricted.containers.interception";
const LOW_LEVEL: &str = "restricted.containers.lowlevel";
const DISK: &str = "restricted.devices.disk";
const DISK_PATHS: &str = "restricted.devices.disk.paths";
/// The device types a restricted project refuses unless `restricted.devices.<type>` is `allow` (Incus 7.5,
/// `checkRestrictions` in `internal/server/project/permissions.go`).
const ALLOW_DEVICE_TYPES: [&str; 8] = [
    "unix-char",
    "unix-block",
    "unix-hotplug",
    "infiniband",
    "gpu",
    "usb",
    "pci",
    "proxy",
];
/// The interception keys `restricted.containers.interception=allow` admits (Incus's `allowableIntercept`). Every other
/// `security.syscalls.intercept*` key counts as low level.
const INTERCEPT_KEYS: [&str; 7] = [
    "security.syscalls.intercept.bpf",
    "security.syscalls.intercept.bpf.devices",
    "security.syscalls.intercept.mknod",
    "security.syscalls.intercept.mount",
    "security.syscalls.intercept.mount.fuse",
    "security.syscalls.intercept.setxattr",
    "security.syscalls.intercept.sysinfo",
];
/// The container keys Incus counts as low level (`isContainerLowLevelOptionForbidden`), which a restricted project refuses
/// unless `restricted.containers.lowlevel` is `allow`, whatever their value. `security.devlxd.images` is the older name of
/// `security.guestapi.images`.
const LOW_LEVEL_KEYS: [&str; 17] = [
    "boot.host_shutdown_action",
    "boot.host_shutdown_timeout",
    "linux.kernel_modules",
    "limits.memory.swap",
    "limits.memory.oom_priority",
    "raw.apparmor",
    "raw.idmap",
    "raw.lxc",
    "raw.seccomp",
    "security.devlxd.images",
    "security.guestapi.images",
    "security.idmap.base",
    "security.idmap.size",
    "security.selinux.domain",
    "security.selinux.label_rootfs",
    "security.selinux.level",
    "security.selinux.type",
];
/// The prefixes of low-level keys: every `raw.*` key, and `security.bpffs*`.
const LOW_LEVEL_PREFIXES: [&str; 2] = ["raw.", "security.bpffs"];

/// A device: its keys and their values.
pub type Device = BTreeMap<String, String>;
/// Devices by name.
pub type Devices = BTreeMap<String, Device>;

/// What an adoption's plan allows and changes past the pin. The owner approves both with the plan's digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Allowances {
    /// The `restricted.*` keys the adoption project sets past the kernel's own, each to the value that allows what the
    /// instance already uses ([`needed_allowances`]). The grant is here only when it names a network `kernel.attachNetworks`
    /// does not list.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project: BTreeMap<String, String>,
    /// The devices a profile gives that the adoption makes the instance's own before the move, by name: its root disk, when
    /// a profile gives it. A release takes each back off.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub localize: Devices,
}

impl Allowances {
    /// Whether the plan allows and localizes nothing.
    pub fn is_empty(&self) -> bool {
        self.project.is_empty() && self.localize.is_empty()
    }
}

/// The allowances an instance needs: each `restricted.*` key with the value that allows it, and, for each key, the config
/// keys and the devices (`devices.<name>`) that need it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Needed {
    pub project: BTreeMap<String, String>,
    pub why: BTreeMap<String, Vec<String>>,
}

impl Needed {
    fn add(&mut self, key: &str, value: &str, because: &str) {
        self.project.insert(key.to_string(), value.to_string());
        self.why
            .entry(key.to_string())
            .or_default()
            .push(because.to_string());
    }
}

/// The `restricted.*` keys a restricted project needs set to take a container with this config and these devices, as Incus
/// 7.5 checks them on a moved-in instance (`checkRestrictions`). `config` and `devices` are what the instance will hold after
/// the move: its own config, and its devices with `eth0` pinned and its root its own. `volatile.*` keys are left to Incus,
/// which drops them on the copy. The network grant is not here: it always names eth0's network.
///
/// | The instance has | The allowance |
/// |---|---|
/// | `security.nesting` true | `restricted.containers.nesting=allow` |
/// | an interception key Incus admits, not false | `restricted.containers.interception=allow` |
/// | another `security.syscalls.intercept*` key, a `raw.*` key or another low-level key | `restricted.containers.lowlevel=allow` |
/// | a host-path disk | `restricted.devices.disk=allow` and `restricted.devices.disk.paths=<its sources>` |
/// | a `unix-char`, `unix-block`, `unix-hotplug`, `infiniband`, `gpu`, `usb`, `pci` or `proxy` device | `restricted.devices.<type>=allow` |
/// | eth0 on a managed network, its ACLs, a pool-backed root, a `none` device | nothing |
///
/// It refuses, naming the reason, what no allowance the adoption grants would admit: `security.privileged` true; a NIC other
/// than eth0, whose MAC the move would reset; eth0 without a network; a disk on a pool other than the root, which stays in
/// `default`; a host path that a comma-separated list cannot hold exactly, or that sets `shift`, which Incus refuses once the
/// paths are listed; and any other device type.
pub fn needed_allowances(
    config: &BTreeMap<String, String>,
    devices: &Devices,
) -> Result<Needed, String> {
    let mut needed = Needed::default();
    for (key, value) in config {
        if key.starts_with("volatile.") {
            continue;
        }
        let restriction = match key.as_str() {
            "security.privileged" if is_true(value) => {
                return Err(
                    "the instance sets security.privileged: the adoption project's restrictions refuse a privileged container, and the adoption does not grant restricted.containers.privilege"
                        .to_string(),
                );
            }
            "security.nesting" => is_true(value).then_some(NESTING),
            intercept if INTERCEPT_KEYS.contains(&intercept) => {
                (!is_false_or_empty(value)).then_some(INTERCEPTION)
            }
            low if low.starts_with("security.syscalls.intercept")
                || LOW_LEVEL_KEYS.contains(&low)
                || LOW_LEVEL_PREFIXES
                    .iter()
                    .any(|prefix| low.starts_with(prefix)) =>
            {
                Some(LOW_LEVEL)
            }
            _ => None,
        };
        if let Some(restriction) = restriction {
            needed.add(restriction, ALLOW, key);
        }
    }
    let mut paths = BTreeSet::new();
    for (name, device) in devices {
        let field = |key: &str| device.get(key).map_or("", String::as_str);
        let because = format!("devices.{name}");
        match field("type") {
            "nic" if name != NIC => {
                return Err(format!(
                    "the instance's {name} device is a NIC other than eth0: only eth0 is pinned, and the move would give {name} a new MAC"
                ));
            }
            "nic" if field("network").is_empty() => {
                return Err(
                    "the instance's eth0 names no managed network, which a restricted project refuses"
                        .to_string(),
                );
            }
            "nic" => {}
            "disk" if field("path") == "/" && !field("pool").is_empty() => {}
            "disk" if field("pool").is_empty() && !field("source").is_empty() => {
                let source = field("source");
                if !listable(source) {
                    return Err(format!(
                        "the instance's {name} disk has the host path {source:?}, which restricted.devices.disk.paths cannot list exactly"
                    ));
                }
                // Incus refuses `shift` on a host-path disk while `restricted.devices.disk.paths` is set (`disk.go`, Incus
                // 7.5), and the adoption always sets it: the move would fail only after the stop.
                if is_true(field("shift")) {
                    return Err(format!(
                        "the instance's {name} disk sets shift on a host path, which Incus refuses in a project that lists restricted.devices.disk.paths"
                    ));
                }
                needed.add(DISK, ALLOW, &because);
                needed.add(DISK_PATHS, "", &because);
                paths.insert(source.to_string());
            }
            "disk" if !field("pool").is_empty() => {
                return Err(format!(
                    "the instance's {name} device is a disk other than its root, on a volume the move cannot reach"
                ));
            }
            "disk" => {
                return Err(format!(
                    "the instance's {name} disk names neither a pool nor a host path"
                ));
            }
            kind if ALLOW_DEVICE_TYPES.contains(&kind) => {
                needed.add(&format!("restricted.devices.{kind}"), ALLOW, &because);
            }
            "none" => {}
            kind => {
                return Err(format!(
                    "the instance's {name} device is of type {kind:?}, which no restricted key the adoption maps allows"
                ));
            }
        }
    }
    if !paths.is_empty() {
        let list: Vec<String> = paths.into_iter().collect();
        needed
            .project
            .insert(DISK_PATHS.to_string(), list.join(","));
    }
    Ok(needed)
}

/// Incus's `IsTrue`: `true`, `1`, `yes` or `on`, in any case.
fn is_true(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// Incus's `IsFalseOrEmpty`: empty, or `false`, `0`, `no` or `off`, in any case.
fn is_false_or_empty(value: &str) -> bool {
    value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        )
}

/// A host path `restricted.devices.disk.paths` can list exactly: absolute and already clean, as Incus cleans it, with no comma,
/// which separates the list, and no space at either end, which Incus trims.
fn listable(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    path.starts_with('/')
        && path.len() <= 4096
        && path.trim() == path
        && path
            .bytes()
            .all(|b| (0x20..=0x7e).contains(&b) && b != b',')
        && path[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// One allowlisted adoption: the instance in the default project and the adoption project it moves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptTarget {
    pub instance: String,
    pub to: String,
}

impl AdoptTarget {
    /// Checks the names. The instance is `^[a-z][a-z0-9-]{0,56}$`, so its entry `adopt-<instance>` is a valid entry id. The
    /// project is `arcak-adopt-` and then letters, digits or hyphens, at most 63 bytes in all.
    pub fn check(&self) -> Result<(), &'static str> {
        let bytes = self.instance.as_bytes();
        let plain = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-';
        if !(1..=57).contains(&bytes.len())
            || !bytes[0].is_ascii_lowercase()
            || !bytes.iter().all(plain)
        {
            return Err("an adopted instance's name is not plain");
        }
        let suffix = self.to.strip_prefix(ADOPT_PROJECT_PREFIX).unwrap_or("");
        if suffix.is_empty() || self.to.len() > 63 || !suffix.as_bytes().iter().all(plain) {
            return Err("an adoption project must be arcak-adopt- and a plain name");
        }
        Ok(())
    }

    /// The kernel entry the adopted instance is marked with: `adopt-<instance>`.
    pub fn entry(&self) -> Option<EntryId> {
        EntryId::parse(&format!("{ADOPT_ENTRY_PREFIX}{}", self.instance))
    }
}

/// The instance as the plan found it in the default project. `config` is its own config without any `volatile.*` key: its
/// uuid and MAC are kept in their own fields. `devices` are its expanded devices, so the NIC its profile gives is recorded.
/// `config_hash` is the watch's hash of its own config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreImage {
    pub uuid: String,
    pub created_at: String,
    pub mac: String,
    pub ipv4: Ipv4Addr,
    pub profiles: Vec<String>,
    pub config: BTreeMap<String, String>,
    pub devices: BTreeMap<String, BTreeMap<String, String>>,
    pub config_hash: String,
    /// Whether the plan found the instance stopped. Its MAC is then the one Incus starts it with, and its IPv4 the one the
    /// owner named; the adoption neither stops nor starts it, and its release leaves it stopped. Left out when false, so a
    /// running instance's plan keeps the bytes and the digest it had before.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stopped: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PreImage {
    /// The pin of this pre-image: its MAC and IPv4, on the network its `eth0` names, with the ACL keys its `eth0` sets.
    pub fn pin(&self) -> Option<Pin> {
        let eth0 = self.devices.get(NIC)?;
        let network = eth0.get("network")?.clone();
        let kept = eth0
            .iter()
            .filter(|(key, _)| NIC_KEPT_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Some(Pin {
            mac: self.mac.clone(),
            ipv4: self.ipv4,
            network,
            kept,
        })
    }
}

/// The NIC pin: the MAC and the IPv4 set on `eth0` itself, on its network, with the NIC's own ACL keys kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub mac: String,
    pub ipv4: Ipv4Addr,
    pub network: String,
    /// The [`NIC_KEPT_KEYS`] the instance's `eth0` sets, as it sets them.
    pub kept: BTreeMap<String, String>,
}

/// The pinned `eth0` device, key by key.
pub fn pin_device(pin: &Pin) -> BTreeMap<String, String> {
    let mut device = pin.kept.clone();
    device.extend([
        ("type".to_string(), "nic".to_string()),
        ("name".to_string(), NIC.to_string()),
        ("network".to_string(), pin.network.clone()),
        ("hwaddr".to_string(), pin.mac.clone()),
        ("ipv4.address".to_string(), pin.ipv4.to_string()),
    ]);
    device
}

/// The PATCH body that pins the NIC: `devices.eth0` alone, which Incus replaces whole and which overrides the profile's.
pub fn pin_patch(pin: &Pin) -> Value {
    json!({ "devices": { NIC: pin_device(pin) } })
}

/// The create body of the adoption project. It is marked `adopt-<instance>` from the moment it exists. It is restricted, its
/// grant names `network`, eth0's, and nothing else, and it allows snapshots and backups. Past those it sets exactly the
/// plan's `allowances`, and no other `restricted.*` key. It reads the default project's networks and ACLs, and has its own
/// profiles, volumes and images.
pub fn adoption_project_body(
    target: &AdoptTarget,
    network: &str,
    allowances: &BTreeMap<String, String>,
) -> Value {
    let mut config = json!({
        KERNEL_ENTRY_KEY: format!("{ADOPT_ENTRY_PREFIX}{}", target.instance),
        "restricted": "true",
        NETWORKS_ACCESS: network,
        "restricted.snapshots": "allow",
        "restricted.backups": "allow",
        "features.networks": "false",
        "features.profiles": "true",
        "features.storage.volumes": "true",
        "features.images": "true",
    });
    if let Value::Object(config) = &mut config {
        for (key, value) in allowances {
            config.insert(key.clone(), json!(value));
        }
    }
    json!({
        "name": target.to,
        "description": format!("Arca adoption of {}", target.instance),
        "config": config,
    })
}

/// The `restricted` keys of a project config, and its mark: the part of an adoption project the plan settles. Incus adds
/// default `features.*` keys of its own on a create, so those are not compared.
fn settled(config: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    config
        .iter()
        .filter(|(key, _)| key.starts_with("restricted") || key.as_str() == KERNEL_ENTRY_KEY)
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

/// The steps, adoption then release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AdoptStep {
    CreateProject,
    Stop,
    Snapshot,
    PinNic,
    LocalizeRoot,
    Move,
    Mark,
    Start,
    ReleaseStop,
    Unmark,
    MoveBack,
    RestoreRoot,
    ReleaseStart,
    DeleteProject,
}

impl AdoptStep {
    /// Every adoption step, in order. A plan that localizes nothing leaves LocalizeRoot out ([`AdoptStep::adopt_steps`]).
    pub const ADOPT: [AdoptStep; 8] = [
        AdoptStep::CreateProject,
        AdoptStep::Stop,
        AdoptStep::Snapshot,
        AdoptStep::PinNic,
        AdoptStep::LocalizeRoot,
        AdoptStep::Move,
        AdoptStep::Mark,
        AdoptStep::Start,
    ];
    /// Every release step, in order. A plan that localizes nothing leaves RestoreRoot out ([`AdoptStep::release_steps`]).
    pub const RELEASE: [AdoptStep; 6] = [
        AdoptStep::ReleaseStop,
        AdoptStep::Unmark,
        AdoptStep::MoveBack,
        AdoptStep::RestoreRoot,
        AdoptStep::ReleaseStart,
        AdoptStep::DeleteProject,
    ];
    /// The steps that stop or start the instance. A plan that found it stopped has none of them.
    pub const STOP_START: [AdoptStep; 4] = [
        AdoptStep::Stop,
        AdoptStep::Start,
        AdoptStep::ReleaseStop,
        AdoptStep::ReleaseStart,
    ];

    /// The adoption steps of a plan that found the instance running: LocalizeRoot only when the plan localizes a device.
    pub fn adopt_steps(localize: bool) -> Vec<AdoptStep> {
        Self::adopt_steps_of(localize, false)
    }

    /// The release steps of a plan that found the instance running: RestoreRoot only when the plan localizes a device.
    pub fn release_steps(localize: bool) -> Vec<AdoptStep> {
        Self::release_steps_of(localize, false)
    }

    /// The adoption steps of a plan: LocalizeRoot only when it localizes a device, and Stop and Start only when it found the
    /// instance running.
    pub fn adopt_steps_of(localize: bool, stopped: bool) -> Vec<AdoptStep> {
        Self::ADOPT
            .into_iter()
            .filter(|step| step.in_plan(localize, stopped))
            .collect()
    }

    /// The release steps of a plan: RestoreRoot only when it localizes a device, and ReleaseStop and ReleaseStart only when
    /// it found the instance running.
    pub fn release_steps_of(localize: bool, stopped: bool) -> Vec<AdoptStep> {
        Self::RELEASE
            .into_iter()
            .filter(|step| step.in_plan(localize, stopped))
            .collect()
    }

    fn in_plan(self, localize: bool, stopped: bool) -> bool {
        let root = matches!(self, AdoptStep::LocalizeRoot | AdoptStep::RestoreRoot);
        (localize || !root) && !(stopped && Self::STOP_START.contains(&self))
    }
}

/// What a step did: it sent its request, or its outcome already held and it sent nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepOutcome {
    Sent,
    AlreadyDone,
}

/// The project a step expects the instance in: the default project or the adoption project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Default,
    To,
}

/// The owner class a step expects: no owner's mark, or this adoption's entry mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectClass {
    Foreign,
    Adopted,
}

/// Where, in which class and in which status a step expects the instance before it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expect {
    pub project: Side,
    pub class: ExpectClass,
    pub status: &'static str,
}

/// One request of the step table: the step, the method, the path and the body; whether it carries the ETag of the read before
/// it; for the forced stop, the condition it is sent under; and, for RestoreRoot's full PUT, how its body is taken from the
/// read before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptRequest {
    pub step: AdoptStep,
    pub method: &'static str,
    pub path: String,
    pub if_match: bool,
    pub body: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_if: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_read: Option<&'static str>,
}

/// How RestoreRoot's PUT body is made. A PATCH cannot remove a device, so the step sends the whole instance back as it read it,
/// under that read's ETag, less the device it localized.
const FROM_READ: &str = "the instance's writable fields as the read before the PUT gives them, less each device this body sets to null";
/// The fields of an instance a PUT replaces (`api.InstancePut`).
const WRITABLE: [&str; 7] = [
    "architecture",
    "config",
    "devices",
    "ephemeral",
    "profiles",
    "stateful",
    "description",
];

/// The instance as one read showed it.
struct Seen {
    etag: String,
    class: OwnerClass,
    status: String,
    kernel_keys: bool,
    devices: Devices,
    expanded: Devices,
    /// The whole read, for RestoreRoot's PUT.
    body: Value,
}

/// The adoption writer for one allowlisted target and its recorded pre-image.
#[derive(Debug)]
pub struct AdoptWriter {
    writer: UnixIncusWriter,
    read: UnixIncus,
    target: AdoptTarget,
    pre: PreImage,
    allowances: Allowances,
    pin: Pin,
    entry: EntryId,
    snapshot: Option<String>,
    /// The uuid the instance has now: the pre-image's until a move is confirmed as a copy under a new one.
    uuid: Mutex<String>,
}

impl Clone for AdoptWriter {
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            read: self.read.clone(),
            target: self.target.clone(),
            pre: self.pre.clone(),
            allowances: self.allowances.clone(),
            pin: self.pin.clone(),
            entry: self.entry.clone(),
            snapshot: self.snapshot.clone(),
            uuid: Mutex::new(self.uuid()),
        }
    }
}

impl AdoptWriter {
    /// A writer for `target` over the Incus socket at `socket`, for a plan that allows and localizes nothing (see
    /// [`AdoptWriter::new_allowing`]). Nothing is read or sent here.
    pub fn new(
        socket: PathBuf,
        target: AdoptTarget,
        attach: Vec<String>,
        pre: PreImage,
    ) -> Result<Self, IncusError> {
        Self::new_allowing(socket, target, attach, pre, Allowances::default())
    }

    /// A writer for `target` and a plan of `pre` and `allowances`, over the Incus socket at `socket`. `attach` lists the
    /// networks the adoption project grants without an allowance. The pre-image's `eth0` must be one the pin can carry: on a
    /// plain network, with no key but [`PIN_KEYS`] and [`NIC_KEPT_KEYS`], and a `hwaddr` or `ipv4.address` it sets must be the
    /// pre-image's. The allowances must be exactly the ones the pre-image needs ([`needed_allowances`]), and the grant one
    /// only when it names eth0's network and `attach` does not list it; a localized device must be a pool-backed root that
    /// the pre-image holds as recorded. Nothing is read or sent here.
    pub fn new_allowing(
        socket: PathBuf,
        target: AdoptTarget,
        attach: Vec<String>,
        pre: PreImage,
        allowances: Allowances,
    ) -> Result<Self, IncusError> {
        target.check().map_err(refused)?;
        let entry = target
            .entry()
            .ok_or_else(|| refused("the adoption entry is not a valid entry id"))?;
        if !attach.iter().all(|name| plain_name(name)) {
            return Err(refused("an attach network is not a plain name"));
        }
        let pin = pre
            .pin()
            .ok_or_else(|| refused("the pre-image names no network on eth0"))?;
        if !plain_name(&pin.network) {
            return Err(refused("the NIC's network is not a plain name"));
        }
        if !mac_ok(&pin.mac) || pre.uuid.is_empty() {
            return Err(refused("the pre-image has no plain MAC or uuid"));
        }
        pin_carries(&pre, &pin)?;
        check_allowances(&attach, &pre, &pin, &allowances)?;
        let uuid = Mutex::new(pre.uuid.clone());
        let family = ProjectFamily::new([target.to.as_str()], [])
            .map_err(|_| refused("the adoption project cannot be named"))?;
        let writer = UnixIncusWriter::new(socket.clone(), entry.clone(), BTreeMap::new(), family);
        Ok(Self {
            writer,
            read: UnixIncus::new(socket),
            target,
            pre,
            allowances,
            pin,
            entry,
            snapshot: None,
            uuid,
        })
    }

    /// The adoption steps this writer's plan runs, in order.
    pub fn adopt_steps(&self) -> Vec<AdoptStep> {
        AdoptStep::adopt_steps_of(!self.allowances.localize.is_empty(), self.pre.stopped)
    }

    /// The release steps this writer's plan runs, in order.
    pub fn release_steps(&self) -> Vec<AdoptStep> {
        AdoptStep::release_steps_of(!self.allowances.localize.is_empty(), self.pre.stopped)
    }

    /// Whether this writer's plan found the instance stopped, so that it neither stops nor starts it.
    pub fn stopped(&self) -> bool {
        self.pre.stopped
    }

    /// The plan's allowances.
    pub fn allowances(&self) -> &Allowances {
        &self.allowances
    }

    /// The same writer with the adoption snapshot's name, `arca-adopt-` and 12 lowercase hex characters. The Snapshot step
    /// refuses without it.
    pub fn with_snapshot(mut self, name: &str) -> Result<Self, IncusError> {
        let hex = name.strip_prefix(SNAPSHOT_PREFIX).unwrap_or("");
        if hex.len() != 12
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refused(
                "the snapshot name is not arca-adopt- and 12 hex characters",
            ));
        }
        self.snapshot = Some(name.to_string());
        Ok(self)
    }

    /// The same writer with another overall deadline for an operation.
    pub fn with_operation_deadline(mut self, deadline: Duration) -> Self {
        self.writer = self.writer.with_operation_deadline(deadline);
        self
    }

    /// The uuid the writer expects the instance to have: the pre-image's, or the one a confirmed copy gave it.
    ///
    /// A move that reads back under a new uuid in its new project is confirmed only by the copy rule: the new uuid is a plain
    /// uuid other than the old one; the old uuid is in no project of the instance listing, and the name is gone from the project
    /// it left; exactly one instance of the name is in its new project; that one carries no owner's mark, its own `eth0` is the
    /// recorded pin and it holds the adoption snapshot; and the adoption project carries this adoption's mark. Every later step
    /// expects the new uuid.
    pub fn uuid(&self) -> String {
        self.uuid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The same writer expecting the instance under `uuid`, the one a recorded copy gave it. The uuid is lowercase hex in
    /// groups of 8, 4, 4, 4 and 12, joined by hyphens.
    pub fn with_uuid(self, uuid: &str) -> Result<Self, IncusError> {
        if !uuid_ok(uuid) {
            return Err(refused("the recorded uuid is not a plain uuid"));
        }
        self.follow(uuid);
        Ok(self)
    }

    fn follow(&self, uuid: &str) {
        *self
            .uuid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = uuid.to_string();
    }

    /// The pin this writer sets.
    pub fn pin(&self) -> &Pin {
        &self.pin
    }

    /// Where, in which class and in which status `step` of a plan that found the instance running expects it (Decision table
    /// in `docs/OWNERSHIP.md`). The status is the one the step's request is sent in; Mark also takes the instance running (see
    /// `before`), in this table only. [`AdoptWriter::expects`] is this writer's own table.
    pub fn expect(step: AdoptStep) -> Expect {
        use AdoptStep as S;
        use ExpectClass::{Adopted, Foreign};
        use Side::{Default, To};
        let (project, class, status) = match step {
            S::CreateProject | S::Stop => (Default, Foreign, "Running"),
            S::Snapshot | S::PinNic | S::LocalizeRoot | S::Move => (Default, Foreign, "Stopped"),
            // Or Running: one started by hand after a move whose outcome was not recorded.
            S::Mark => (To, Foreign, "Stopped"),
            S::Start | S::Unmark => (To, Adopted, "Stopped"),
            S::ReleaseStop => (To, Adopted, "Running"),
            S::MoveBack => (To, Foreign, "Stopped"),
            S::RestoreRoot | S::ReleaseStart => (Default, Foreign, "Stopped"),
            S::DeleteProject => (Default, Foreign, "Running"),
        };
        Expect {
            project,
            class,
            status,
        }
    }

    /// Where, in which class and in which status `step` of this writer's plan expects the instance: [`AdoptWriter::expect`],
    /// but a plan that found the instance stopped expects it stopped at CreateProject and DeleteProject too, since none of its
    /// steps stops or starts it.
    pub fn expects(&self, step: AdoptStep) -> Expect {
        let mut expect = Self::expect(step);
        if self.pre.stopped {
            expect.status = "Stopped";
        }
        expect
    }

    /// Every request `step` may send, in the order it sends them. A step sends the forced stop only under its condition. A plan
    /// that found the instance stopped has no request for a step that stops or starts it.
    pub fn requests(&self, step: AdoptStep) -> Vec<AdoptRequest> {
        if self.pre.stopped && AdoptStep::STOP_START.contains(&step) {
            return Vec::new();
        }
        let i = &self.target.instance;
        let project = self.side(self.expects(step).project);
        let instance = format!("/1.0/instances/{i}?project={project}");
        let state = format!("/1.0/instances/{i}/state?project={project}");
        let request = |method, path: &str, if_match, body| AdoptRequest {
            step,
            method,
            path: path.to_string(),
            if_match,
            body,
            only_if: None,
            from_read: None,
        };
        let mark = |value: String| json!({ "config": { KERNEL_ENTRY_KEY: value } });
        let localize = &self.allowances.localize;
        match step {
            AdoptStep::CreateProject => {
                vec![request("POST", "/1.0/projects", false, self.project_body())]
            }
            AdoptStep::LocalizeRoot if localize.is_empty() => Vec::new(),
            AdoptStep::LocalizeRoot => vec![request(
                "PATCH",
                &instance,
                true,
                json!({ "devices": localize }),
            )],
            AdoptStep::RestoreRoot if localize.is_empty() => Vec::new(),
            AdoptStep::RestoreRoot => {
                let removed: serde_json::Map<String, Value> = localize
                    .keys()
                    .map(|name| (name.clone(), Value::Null))
                    .collect();
                vec![AdoptRequest {
                    from_read: Some(FROM_READ),
                    ..request("PUT", &instance, true, json!({ "devices": removed }))
                }]
            }
            AdoptStep::DeleteProject => vec![request(
                "DELETE",
                &format!("/1.0/projects/{}", self.target.to),
                false,
                Value::Null,
            )],
            AdoptStep::Stop | AdoptStep::ReleaseStop => vec![
                request(
                    "PUT",
                    &state,
                    false,
                    json!({ "action": "stop", "timeout": STOP_TIMEOUT_SECONDS, "force": false }),
                ),
                AdoptRequest {
                    only_if: Some("the instance still runs after the graceful stop"),
                    ..request(
                        "PUT",
                        &state,
                        false,
                        json!({ "action": "stop", "force": true }),
                    )
                },
            ],
            AdoptStep::Snapshot => {
                let name = self.snapshot.clone().unwrap_or_default();
                let path = format!("/1.0/instances/{i}/snapshots?project={project}");
                vec![request(
                    "POST",
                    &path,
                    false,
                    json!({ "name": name, "stateful": false }),
                )]
            }
            AdoptStep::PinNic => vec![request("PATCH", &instance, true, pin_patch(&self.pin))],
            AdoptStep::Move => {
                let body = json!({ "name": i, "project": self.target.to });
                vec![request("POST", &instance, false, body)]
            }
            AdoptStep::MoveBack => {
                let body = json!({ "name": i, "project": SOURCE_PROJECT });
                vec![request("POST", &instance, false, body)]
            }
            AdoptStep::Mark => vec![request(
                "PATCH",
                &instance,
                true,
                mark(self.entry.0.clone()),
            )],
            AdoptStep::Unmark => vec![request("PATCH", &instance, true, mark(String::new()))],
            AdoptStep::Start | AdoptStep::ReleaseStart => {
                vec![request("PUT", &state, false, json!({ "action": "start" }))]
            }
        }
    }

    /// Runs one step. No step runs while an operation names the instance, in either project, or the adoption project. The
    /// instance is read first; a step whose outcome already holds returns [`StepOutcome::AlreadyDone`] and sends nothing.
    /// Otherwise the step's expected project, class and status are checked, then its request is sent and its operation waited
    /// on, and the instance is read back. [`IncusError::Refused`] means nothing was sent by this step.
    pub fn run(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        if self.pre.stopped && AdoptStep::STOP_START.contains(&step) {
            return Err(refused(
                "the plan found the instance stopped, and it neither stops nor starts it",
            ));
        }
        self.idle()?;
        let localizes = !self.allowances.localize.is_empty();
        match step {
            // A plan that localizes nothing has nothing to localize or restore.
            AdoptStep::LocalizeRoot | AdoptStep::RestoreRoot if !localizes => {
                Ok(StepOutcome::AlreadyDone)
            }
            AdoptStep::CreateProject => self.create_project(),
            AdoptStep::Move | AdoptStep::MoveBack => self.move_step(step),
            AdoptStep::DeleteProject => self.delete_project(),
            _ => self.instance_step(step),
        }
    }

    /// The adoption project's create body: eth0's network granted, and the plan's allowances.
    fn project_body(&self) -> Value {
        adoption_project_body(&self.target, &self.pin.network, &self.allowances.project)
    }

    /// Refuses, with GET alone, a RestoreRoot that would change the instance's disks or keep one it changed: the profiles the
    /// pre-image names, read in `default`, must give each localized device as the plan recorded it, so that taking the
    /// instance's own copy off leaves its expanded devices as they are; and the instance, wherever it is, must hold each one
    /// as the plan made it, or not at all. A release runs it before ReleaseStop, so it never stops half way.
    pub fn restore_ready(&self) -> Result<(), IncusError> {
        self.profiles_give(&self.pre.profiles)?;
        // By name, whatever its uuid: a move whose copy is not yet recorded is the steps' to judge, and this only refuses.
        for side in [Side::To, Side::Default] {
            let Some((_, seen)) = self.read_any(self.side(side))? else {
                continue;
            };
            for (name, device) in &self.allowances.localize {
                if seen.devices.get(name).is_some_and(|own| own != device) {
                    return Err(refused(&format!(
                        "the instance's own {name} device changed since the adoption localized it, so RestoreRoot would keep it"
                    )));
                }
            }
            return Ok(());
        }
        Ok(())
    }

    fn profiles_give(&self, profiles: &[String]) -> Result<(), IncusError> {
        let mut given = Devices::new();
        for profile in profiles {
            if !plain_name(profile) {
                return Err(refused("a profile the instance names is not a plain name"));
            }
            let path = format!("/1.0/profiles/{profile}?project={SOURCE_PROJECT}");
            match self.read.get(&path) {
                Ok(body) => given.extend(devices_of(body.get("devices"))?),
                Err(IncusError::Status { code: 404, .. }) => {}
                Err(error) => return Err(error),
            }
        }
        for (name, device) in &self.allowances.localize {
            if given.get(name) != Some(device) {
                return Err(refused(&format!(
                    "the default project's profiles no longer give the {name} device the plan recorded, so taking the instance's own {name} off would change it"
                )));
            }
        }
        Ok(())
    }

    /// Whether every device the plan localizes is the instance's own, as recorded.
    fn localized(&self, seen: &Seen) -> bool {
        self.allowances
            .localize
            .iter()
            .all(|(name, device)| seen.devices.get(name) == Some(device))
    }

    fn side(&self, side: Side) -> &str {
        match side {
            Side::Default => SOURCE_PROJECT,
            Side::To => &self.target.to,
        }
    }

    fn instance_ref(&self, project: &str) -> ObjectRef {
        ObjectRef {
            kind: IncusKind::Instance,
            project: project.to_string(),
            parent: None,
            name: self.target.instance.clone(),
            uuid: Some(self.uuid()),
        }
    }

    fn project_ref(&self) -> ObjectRef {
        ObjectRef {
            kind: IncusKind::Project,
            project: self.target.to.clone(),
            parent: None,
            name: self.target.to.clone(),
            uuid: None,
        }
    }

    /// Refuses while an operation names the instance in either project, or the adoption project. A listing that cannot be read
    /// is an error, and the step is not attempted.
    fn idle(&self) -> Result<(), IncusError> {
        let (i, to) = (&self.target.instance, &self.target.to);
        let busy = running_operations(&self.read)?.iter().any(|op| {
            operation_names(op, IncusKind::Instance, SOURCE_PROJECT, None, i)
                || operation_names(op, IncusKind::Instance, to, None, i)
                || operation_names(op, IncusKind::Project, to, None, to)
        });
        if busy {
            return Err(refused(
                "an operation is still running on the instance or its project",
            ));
        }
        Ok(())
    }

    /// Reads the instance in `project`: `None` when it is not there. A non-container, or one whose uuid is not the one the
    /// writer expects, is refused.
    fn read_in(&self, project: &str) -> Result<Option<Seen>, IncusError> {
        match self.read_any(project)? {
            Some((uuid, _)) if uuid != self.uuid() => {
                Err(refused("the instance is not the one the plan recorded"))
            }
            found => Ok(found.map(|(_, seen)| seen)),
        }
    }

    /// Reads the instance of the target's name in `project`, whatever its uuid: the uuid and the read, or `None` when it is not
    /// there. A non-container is refused.
    fn read_any(&self, project: &str) -> Result<Option<(String, Seen)>, IncusError> {
        let (metadata, etag) = match self.writer.get_etag(&self.instance_ref(project)) {
            Ok(found) => found,
            Err(IncusError::Status { code: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.get("type").and_then(Value::as_str) != Some("container") {
            return Err(refused("the instance is not a container"));
        }
        let config = text_map(metadata.get("config"))
            .ok_or_else(|| decode("the instance's config is not a map of text"))?;
        let uuid = config.get("volatile.uuid").cloned().unwrap_or_default();
        let status = metadata
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let devices = devices_of(metadata.get("devices"))?;
        let expanded = devices_of(metadata.get("expanded_devices"))?;
        Ok(Some((
            uuid,
            Seen {
                etag,
                class: classify(&config, &BTreeMap::new()),
                status,
                kernel_keys: config.keys().any(|key| key.starts_with(KERNEL_NAMESPACE)),
                devices,
                expanded,
                body: metadata,
            },
        )))
    }

    /// Whether a read shows the class a step expects. Foreign is no owner's mark and no key in the kernel's namespace.
    fn class_is(&self, seen: &Seen, class: ExpectClass) -> bool {
        match class {
            ExpectClass::Foreign => seen.class == OwnerClass::Foreign && !seen.kernel_keys,
            ExpectClass::Adopted => {
                seen.class
                    == OwnerClass::Kernel {
                        entry: self.entry.clone(),
                    }
            }
        }
    }

    fn pinned(&self, seen: &Seen) -> bool {
        seen.devices.get(NIC) == Some(&pin_device(&self.pin))
    }

    /// Refuses a read that is not in the class and the status `step` expects. A running plan's Mark also takes a running
    /// instance: one started by hand in its adoption project after a move whose outcome was not recorded is marked as it runs,
    /// and Start then finds its outcome already holds. A stopped plan's Mark does not, since Mark completes its adoption: one
    /// started since its move is stopped by hand before Mark, or released.
    fn before(&self, step: AdoptStep, seen: &Seen) -> Result<(), IncusError> {
        let expect = self.expects(step);
        if !self.class_is(seen, expect.class) {
            return Err(refused(
                "the instance is not in the owner class the step expects",
            ));
        }
        if self.pre.stopped && step == AdoptStep::Mark && seen.status != expect.status {
            return Err(refused(
                "the plan found the instance stopped and it is not stopped now, so Mark would complete the adoption with it running: stop it by hand first",
            ));
        }
        let running_mark = step == AdoptStep::Mark && seen.status == "Running";
        if seen.status != expect.status && !running_mark {
            return Err(refused(
                "the instance is not in the status the step expects",
            ));
        }
        Ok(())
    }

    /// The instance read in the step's own project, which it must be in.
    fn located(&self, step: AdoptStep) -> Result<Seen, IncusError> {
        self.read_in(self.side(self.expects(step).project))?
            .ok_or_else(|| refused("the instance is not in the project the step expects"))
    }

    /// The adoption project's config, or `None` when it does not exist.
    fn project_config(&self) -> Result<Option<BTreeMap<String, String>>, IncusError> {
        match self.writer.get_etag(&self.project_ref()) {
            Ok((metadata, _)) => text_map(metadata.get("config"))
                .map(Some)
                .ok_or_else(|| decode("the project's config is not a map of text")),
            Err(IncusError::Status { code: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether a project config carries this entry's mark.
    fn marked_ours(&self, config: &BTreeMap<String, String>) -> bool {
        classify(config, &BTreeMap::new())
            == OwnerClass::Kernel {
                entry: self.entry.clone(),
            }
    }

    /// Whether the adoption project is this adoption's: it carries this entry's mark.
    fn project_is_ours(&self) -> Result<Option<bool>, IncusError> {
        Ok(self
            .project_config()?
            .map(|config| self.marked_ours(&config)))
    }

    /// Whether a project config sets exactly the `restricted` keys the plan's body sets, and its mark.
    fn settled_as_planned(&self, config: &BTreeMap<String, String>) -> bool {
        let body = text_map(self.project_body().get("config")).unwrap_or_default();
        settled(config) == settled(&body)
    }

    /// The instance listing's rows in the adoption project.
    fn rows_in_project(&self) -> Result<Vec<Value>, IncusError> {
        let listing = self
            .read
            .get("/1.0/instances?all-projects=true&recursion=1")?;
        let rows = listing
            .as_array()
            .ok_or_else(|| decode("the instance listing is not a list"))?;
        Ok(rows
            .iter()
            .filter(|row| {
                row.get("project").and_then(Value::as_str) == Some(self.target.to.as_str())
            })
            .cloned()
            .collect())
    }

    /// Whether the adoption project holds no instance but this one.
    fn holds_only_ours(&self) -> Result<bool, IncusError> {
        Ok(self.rows_in_project()?.iter().all(|row| {
            row.get("name").and_then(Value::as_str) == Some(self.target.instance.as_str())
        }))
    }

    /// CreateProject. A project that exists, is this adoption's, holds no other instance and sets the plan's restrictions is
    /// the step done; one that sets other restrictions is refused, since no step writes a project's config.
    fn create_project(&self) -> Result<StepOutcome, IncusError> {
        match self.project_config()? {
            Some(config) if self.marked_ours(&config) && self.holds_only_ours()? => {
                if !self.settled_as_planned(&config) {
                    return Err(refused(
                        "the adoption project exists with restrictions other than the plan's",
                    ));
                }
                return Ok(StepOutcome::AlreadyDone);
            }
            Some(_) => {
                return Err(refused(
                    "the adoption project exists and is not this adoption's",
                ));
            }
            None => {}
        }
        let seen = self.located(AdoptStep::CreateProject)?;
        self.before(AdoptStep::CreateProject, &seen)?;
        self.send_all(AdoptStep::CreateProject, None, None)?;
        match self.project_config() {
            Ok(Some(config)) if self.marked_ours(&config) && self.settled_as_planned(&config) => {
                Ok(StepOutcome::Sent)
            }
            _ => Err(self.unconfirmed(
                &self.project_ref(),
                "the adoption project does not read back marked, with the plan's restrictions",
            )),
        }
    }

    /// DeleteProject, the release's last step: deletes the adoption project once the instance is back in `default`, running
    /// (stopped, for a plan that found it stopped), and the project holds no instance. A project already gone is the step done; one that is not this adoption's,
    /// or that still holds an instance, is refused.
    fn delete_project(&self) -> Result<StepOutcome, IncusError> {
        match self.project_is_ours()? {
            None => return Ok(StepOutcome::AlreadyDone),
            Some(false) => {
                return Err(refused(
                    "the adoption project is not this adoption's, so it is not deleted",
                ));
            }
            Some(true) => {}
        }
        let seen = self.located(AdoptStep::DeleteProject)?;
        self.before(AdoptStep::DeleteProject, &seen)?;
        if !self.rows_in_project()?.is_empty() {
            return Err(refused(
                "the adoption project still holds an instance, so it is not deleted",
            ));
        }
        self.send_all(AdoptStep::DeleteProject, None, None)?;
        match self.project_config() {
            Ok(None) => Ok(StepOutcome::Sent),
            _ => Err(self.unconfirmed(
                &self.project_ref(),
                "the adoption project still reads back after its delete",
            )),
        }
    }

    /// Move and MoveBack. The move is done when the instance reads in its new project under the expected uuid, or under a new
    /// one the copy rule confirms. A MoveBack whose writer still expects the pre-image's uuid also takes, in the adoption
    /// project, a copy the forward move left unrecorded, when the copy rule confirms it there.
    fn move_step(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        let (from, to) = if step == AdoptStep::Move {
            (SOURCE_PROJECT, self.target.to.as_str())
        } else {
            (self.target.to.as_str(), SOURCE_PROJECT)
        };
        let current = self.uuid();
        let (uuid, seen) = match (self.read_any(from)?, self.read_any(to)?) {
            (None, Some((uuid, seen))) => {
                if uuid != current {
                    self.copied(from, to, &uuid, &seen)?;
                    self.follow(&uuid);
                }
                return Ok(StepOutcome::AlreadyDone);
            }
            (Some(_), Some(_)) => return Err(refused("the instance shows in both projects")),
            (None, None) => return Err(refused("the instance is in neither project")),
            (Some(found), None) => found,
        };
        if uuid != current {
            // Only a MoveBack whose writer still expects the pre-image's uuid may take a copy: the forward move's copy was
            // never recorded.
            if step != AdoptStep::MoveBack || current != self.pre.uuid {
                return Err(refused("the instance is not the one the plan recorded"));
            }
            self.copied(SOURCE_PROJECT, from, &uuid, &seen)?;
        }
        self.before(step, &seen)?;
        if !self.pinned(&seen) {
            return Err(refused("the instance's NIC is not pinned"));
        }
        if step == AdoptStep::Move && !self.localized(&seen) {
            return Err(refused(
                "the instance's root is not its own device yet, and the adoption project does not have the profile that gives it",
            ));
        }
        if self.project_is_ours()? != Some(true) {
            return Err(refused("the adoption project is not this adoption's"));
        }
        self.follow(&uuid);
        self.send_all(step, None, None)?;
        let back = match self.read_any(to) {
            Ok(Some((moved, _))) if moved == uuid => Ok(()),
            Ok(Some((moved, seen))) => self
                .copied(from, to, &moved, &seen)
                .map(|()| self.follow(&moved)),
            Ok(None) => Err(refused(
                "the instance does not read back in its new project",
            )),
            Err(error) => Err(error),
        };
        match back {
            Ok(()) => Ok(StepOutcome::Sent),
            Err(error) => Err(self.unconfirmed(
                &self.instance_ref(to),
                &format!(
                    "the instance does not read back in its new project: {}",
                    reason_of(&error)
                ),
            )),
        }
    }

    /// The copy rule: refuses unless the instance read in `to` under the new uuid `uuid` is the copy of the one that left `from`.
    fn copied(&self, from: &str, to: &str, uuid: &str, seen: &Seen) -> Result<(), IncusError> {
        let (instance, old) = (self.target.instance.as_str(), self.uuid());
        if !uuid_ok(uuid) || uuid == old {
            return Err(refused(&format!(
                "the instance in {to} does not carry a plain uuid other than the old one, so it is not a copy"
            )));
        }
        if !self.class_is(seen, ExpectClass::Foreign) {
            return Err(refused(&format!(
                "the instance under a new uuid in {to} carries an owner's mark"
            )));
        }
        let listing = self
            .read
            .get("/1.0/instances?all-projects=true&recursion=1")?;
        let rows = listing
            .as_array()
            .ok_or_else(|| decode("the instance listing is not a list"))?;
        let text =
            |row: &Value, key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
        let old_uuid = |row: &Value| {
            row.get("config")
                .and_then(|config| config.get("volatile.uuid"))
                .and_then(Value::as_str)
                == Some(old.as_str())
        };
        if let Some(row) = rows.iter().find(|row| old_uuid(row)) {
            let project = text(row, "project").unwrap_or_default();
            return Err(refused(&format!(
                "the old uuid is still in project {project}, so the instance under a new uuid is not its copy"
            )));
        }
        if rows.iter().any(|row| {
            text(row, "project").as_deref() == Some(from)
                && text(row, "name").as_deref() == Some(instance)
        }) {
            return Err(refused(&format!(
                "the name is still in {from}, so the instance under a new uuid is not its copy"
            )));
        }
        let named_in_to = rows
            .iter()
            .filter(|row| {
                text(row, "project").as_deref() == Some(to)
                    && text(row, "name").as_deref() == Some(instance)
            })
            .count();
        if named_in_to > 1 {
            return Err(refused(&format!(
                "more than one instance of the name is in {to}"
            )));
        }
        if named_in_to == 0 {
            return Err(refused(&format!(
                "the listing shows no instance of the name in {to}"
            )));
        }
        if !self.pinned(seen) {
            return Err(refused(&format!(
                "the instance under a new uuid in {to} does not carry the recorded pin"
            )));
        }
        if !self.snapshot_in(to)? {
            return Err(refused(&format!(
                "the instance under a new uuid in {to} holds no adoption snapshot"
            )));
        }
        if self.project_is_ours()? != Some(true) {
            return Err(refused(
                "the adoption project is not this adoption's, so the instance under a new uuid is not taken as its copy",
            ));
        }
        Ok(())
    }

    /// Whether a step's outcome already holds on a read of the instance in the step's own project.
    fn done(&self, step: AdoptStep, seen: &Seen) -> Result<bool, IncusError> {
        use ExpectClass::{Adopted, Foreign};
        Ok(match step {
            AdoptStep::Stop => self.class_is(seen, Foreign) && seen.status == "Stopped",
            AdoptStep::ReleaseStop => self.class_is(seen, Adopted) && seen.status == "Stopped",
            AdoptStep::Snapshot => {
                self.class_is(seen, Foreign) && self.snapshot_in(SOURCE_PROJECT)?
            }
            AdoptStep::PinNic => self.class_is(seen, Foreign) && self.pinned(seen),
            AdoptStep::LocalizeRoot => self.class_is(seen, Foreign) && self.localized(seen),
            AdoptStep::RestoreRoot => {
                self.class_is(seen, Foreign)
                    && self
                        .allowances
                        .localize
                        .keys()
                        .all(|name| !seen.devices.contains_key(name))
            }
            AdoptStep::Mark => self.class_is(seen, Adopted),
            AdoptStep::Unmark => self.class_is(seen, Foreign),
            AdoptStep::Start => self.class_is(seen, Adopted) && seen.status == "Running",
            AdoptStep::ReleaseStart => self.class_is(seen, Foreign) && seen.status == "Running",
            AdoptStep::CreateProject
            | AdoptStep::Move
            | AdoptStep::MoveBack
            | AdoptStep::DeleteProject => false,
        })
    }

    /// Refuses a LocalizeRoot whose device changed since the plan, or that the instance already has a device of its own by
    /// that name that is not the plan's.
    fn localizable(&self, seen: &Seen) -> Result<(), IncusError> {
        for (name, device) in &self.allowances.localize {
            if seen.expanded.get(name) != Some(device) {
                return Err(refused(&format!(
                    "the instance's {name} device changed since the plan"
                )));
            }
            if seen.devices.contains_key(name) {
                return Err(refused(&format!(
                    "the instance has a {name} device of its own that is not the plan's"
                )));
            }
        }
        Ok(())
    }

    /// Refuses a RestoreRoot unless each device the plan localized is the instance's own exactly as the plan made it, so a
    /// device changed since is kept, and the profiles the instance names give it as recorded.
    fn restorable(&self, seen: &Seen) -> Result<(), IncusError> {
        for (name, device) in &self.allowances.localize {
            if seen.devices.get(name) != Some(device) {
                return Err(refused(&format!(
                    "the instance's own {name} device is not the one the adoption made its own, so it is kept"
                )));
            }
        }
        let profiles: Vec<String> = seen
            .body
            .get("profiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect();
        self.profiles_give(&profiles)
    }

    /// Whether the instance in `project` holds the adoption snapshot.
    fn snapshot_in(&self, project: &str) -> Result<bool, IncusError> {
        let name = self
            .snapshot
            .as_deref()
            .ok_or_else(|| refused("the writer was given no snapshot name"))?;
        let path = format!(
            "/1.0/instances/{}/snapshots/{name}?project={project}",
            self.target.instance
        );
        match self.read.get(&path) {
            Ok(_) => Ok(true),
            Err(IncusError::Status { code: 404, .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Stop, Snapshot, PinNic, Mark, Start and their release twins: each acts on the instance in one project.
    fn instance_step(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        let seen = self.located(step)?;
        if self.done(step, &seen)? {
            return Ok(StepOutcome::AlreadyDone);
        }
        self.before(step, &seen)?;
        if step == AdoptStep::PinNic && seen.expanded.get(NIC) != self.pre.devices.get(NIC) {
            return Err(refused("the instance's NIC changed since the plan"));
        }
        if step == AdoptStep::LocalizeRoot {
            self.localizable(&seen)?;
        }
        if step == AdoptStep::RestoreRoot {
            self.restorable(&seen)?;
        }
        match step {
            AdoptStep::Stop | AdoptStep::ReleaseStop => return self.stop(step),
            // A config or device change is sent under the ETag of the read its checks were made on.
            AdoptStep::PinNic | AdoptStep::LocalizeRoot | AdoptStep::Mark | AdoptStep::Unmark => {
                self.send_all(step, Some(&seen.etag), None)?
            }
            // The full PUT is that same read, less the device, under its ETag.
            AdoptStep::RestoreRoot => self.send_all(step, Some(&seen.etag), Some(&seen.body))?,
            _ => self.send_all(step, None, None)?,
        }
        let project = self.side(self.expects(step).project).to_string();
        let back = self.read_in(&project);
        let confirmed = match &back {
            Ok(Some(seen)) => self.done(step, seen).unwrap_or(false),
            _ => false,
        };
        if confirmed {
            Ok(StepOutcome::Sent)
        } else {
            Err(self.unconfirmed(
                &self.instance_ref(&project),
                "the read-back does not show the step's outcome",
            ))
        }
    }

    /// The stop: a graceful stop with its timeout, then a forced stop when the instance still runs. A graceful stop whose
    /// operation fails is not an error by itself; a forced stop that still leaves the instance running is.
    fn stop(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        let [graceful, forced] = <[AdoptRequest; 2]>::try_from(self.requests(step))
            .map_err(|_| refused("the stop is not two requests"))?;
        match self.send(&graceful, None, None) {
            Ok(()) | Err(IncusError::Status { .. }) => {}
            Err(error) => return Err(error),
        }
        let still = self.located(step)?;
        if still.status == "Running" {
            match self.send(&forced, None, None) {
                Ok(()) | Err(IncusError::Status { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let project = self.side(self.expects(step).project).to_string();
        match self.read_in(&project) {
            Ok(Some(seen)) if self.done(step, &seen)? => Ok(StepOutcome::Sent),
            _ => Err(self.unconfirmed(
                &self.instance_ref(&project),
                "the forced stop did not stop the instance",
            )),
        }
    }

    /// Sends every request of `step` that carries no condition. `read` is the read a body taken from a read is made of.
    fn send_all(
        &self,
        step: AdoptStep,
        etag: Option<&str>,
        read: Option<&Value>,
    ) -> Result<(), IncusError> {
        for request in self.requests(step).iter().filter(|r| r.only_if.is_none()) {
            self.send(request, etag, read)?;
        }
        Ok(())
    }

    /// Sends one request and waits on its operation. A request that is not one of the table's is refused before any byte is
    /// written, and so is one whose ETag does not match its `if_match`, and one whose body is taken from a read without the
    /// read. A body taken from a read is that read's writable fields, less each device the table's body sets to null, and
    /// nothing else; a null body is no body.
    fn send(
        &self,
        request: &AdoptRequest,
        etag: Option<&str>,
        read: Option<&Value>,
    ) -> Result<(), IncusError> {
        let listed = AdoptStep::ADOPT
            .iter()
            .chain(AdoptStep::RELEASE.iter())
            .any(|step| self.requests(*step).contains(request));
        if !listed {
            return Err(refused("the request is not in the adoption step table"));
        }
        if request.if_match != etag.is_some() {
            return Err(refused("the request's ETag does not match the table"));
        }
        if request.step == AdoptStep::Snapshot && self.snapshot.is_none() {
            return Err(refused("the writer was given no snapshot name"));
        }
        let body = match (request.from_read, read) {
            (None, _) if request.body.is_null() => None,
            (None, _) => Some(request.body.clone()),
            (Some(_), Some(read)) => Some(from_read(read, &request.body)?),
            (Some(_), None) => {
                return Err(refused(
                    "the request's body is taken from a read, and none was given",
                ));
            }
        };
        let framed = self.writer.send_checked(
            request.method,
            &request.path,
            etag,
            body.as_ref(),
            TIMEOUT,
        )?;
        let envelope = checked_envelope(&framed.into_raw())?;
        if envelope.get("type").and_then(Value::as_str) == Some("sync") {
            return Ok(());
        }
        match envelope.get("operation").and_then(Value::as_str) {
            None | Some("") => Ok(()),
            Some(url) => {
                let id = url
                    .strip_prefix("/1.0/operations/")
                    .filter(|id| {
                        !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    })
                    .ok_or_else(|| decode("the answer names no operation"))?;
                self.writer.wait_on(id)
            }
        }
    }

    fn unconfirmed(&self, target: &ObjectRef, reason: &str) -> IncusError {
        IncusError::AppliedUnconfirmed(Box::new(Unconfirmed {
            target: target.clone(),
            uuid: target.uuid.clone(),
            owner: None,
            reason: reason.to_string(),
        }))
    }
}

/// Refuses a pre-image whose `eth0` the pin cannot carry: a key other than [`PIN_KEYS`] and [`NIC_KEPT_KEYS`], a type other
/// than `nic`, a name other than `eth0`, or a `hwaddr` or `ipv4.address` other than the pin's.
fn pin_carries(pre: &PreImage, pin: &Pin) -> Result<(), IncusError> {
    let eth0 = pre.devices.get(NIC).cloned().unwrap_or_default();
    let field = |key: &str| eth0.get(key).map(String::as_str);
    let known = eth0
        .keys()
        .all(|key| PIN_KEYS.contains(&key.as_str()) || NIC_KEPT_KEYS.contains(&key.as_str()));
    let ipv4 = pin.ipv4.to_string();
    if !known
        || field("type") != Some("nic")
        || field("name").is_some_and(|name| name != NIC)
        || field("hwaddr").is_some_and(|mac| mac != pin.mac)
        || field("ipv4.address").is_some_and(|address| address != ipv4)
    {
        return Err(refused(
            "the pre-image's eth0 sets a key the pin does not carry, or one the pin would change",
        ));
    }
    Ok(())
}

/// Refuses allowances that are not exactly the ones the pre-image needs: its [`needed_allowances`], and the grant when it
/// names eth0's network, which it must when `attach` does not list that network. A localized device must be a pool-backed
/// root disk that the pre-image's devices hold as it is recorded, and there is at most one.
fn check_allowances(
    attach: &[String],
    pre: &PreImage,
    pin: &Pin,
    allowances: &Allowances,
) -> Result<(), IncusError> {
    let mut expected = needed_allowances(&pre.config, &pre.devices)
        .map_err(|why| refused(&why))?
        .project;
    match allowances.project.get(NETWORKS_ACCESS) {
        Some(network) if *network == pin.network => {
            expected.insert(NETWORKS_ACCESS.to_string(), network.clone());
        }
        Some(_) => {
            return Err(refused(
                "the plan grants a network other than the one eth0 names",
            ));
        }
        None if !attach.contains(&pin.network) => {
            return Err(refused(
                "the NIC's network is not an attach network, and the plan does not allow it",
            ));
        }
        None => {}
    }
    if allowances.project != expected {
        return Err(refused(
            "the plan's allowances are not exactly the ones its pre-image needs",
        ));
    }
    if allowances.localize.len() > 1 {
        return Err(refused("the plan localizes more than the root disk"));
    }
    for (name, device) in &allowances.localize {
        let field = |key: &str| device.get(key).map_or("", String::as_str);
        if field("type") != "disk"
            || field("path") != "/"
            || field("pool").is_empty()
            || pre.devices.get(name) != Some(device)
        {
            return Err(refused(
                "the plan localizes a device that is not the pre-image's pool-backed root disk",
            ));
        }
    }
    Ok(())
}

/// RestoreRoot's PUT body: the read's writable fields, less each device `template` sets to null. Nothing else is added or
/// changed, so the PUT gives back the instance as it was read but for those devices.
fn from_read(read: &Value, template: &Value) -> Result<Value, IncusError> {
    let removed: Vec<&String> = template
        .get("devices")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, device)| device.is_null())
        .map(|(name, _)| name)
        .collect();
    let mut body = serde_json::Map::new();
    for field in WRITABLE {
        if let Some(value) = read.get(field) {
            body.insert(field.to_string(), value.clone());
        }
    }
    let devices = body
        .entry("devices")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| decode("the instance's devices are not a map"))?;
    for name in removed {
        devices.remove(name);
    }
    Ok(Value::Object(body))
}

/// A network name that is safe in a grant: letters, digits, hyphens and underscores.
fn plain_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A uuid as Incus writes it: lowercase hex in groups of 8, 4, 4, 4 and 12, joined by hyphens.
pub fn uuid_ok(uuid: &str) -> bool {
    let groups: Vec<&str> = uuid.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, len)| {
            group.len() == len
                && group
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// The reason an error gives, for a message that carries it on.
fn reason_of(error: &IncusError) -> String {
    match error {
        IncusError::Refused(reason) | IncusError::Decode(reason) => reason.clone(),
        other => other.to_string(),
    }
}

/// A MAC as Incus writes it: six lowercase hex pairs joined by colons.
pub fn mac_ok(mac: &str) -> bool {
    mac.len() == 17
        && mac.split(':').count() == 6
        && mac.split(':').all(|pair| {
            pair.len() == 2
                && pair
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

fn text_map(value: Option<&Value>) -> Option<BTreeMap<String, String>> {
    match value {
        None | Some(Value::Null) => Some(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
            .collect(),
        Some(_) => None,
    }
}

/// A device map: each device a map of text.
pub fn devices_of(
    value: Option<&Value>,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, IncusError> {
    let Some(Value::Object(devices)) = value else {
        return Ok(BTreeMap::new());
    };
    devices
        .iter()
        .map(|(name, device)| {
            text_map(Some(device))
                .map(|device| (name.clone(), device))
                .ok_or_else(|| decode("a device is not a map of text"))
        })
        .collect()
}

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeIncus, FakeState};

    fn writer(socket: PathBuf) -> AdoptWriter {
        writer_of(socket, false)
    }

    /// The writer of a plan that found the instance running, or, when `stopped`, stopped.
    fn writer_of(socket: PathBuf, stopped: bool) -> AdoptWriter {
        let device = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let pre = PreImage {
            uuid: "00000000-0000-4000-8000-0000000000a1".to_string(),
            created_at: String::new(),
            mac: "10:66:6a:00:00:01".to_string(),
            ipv4: Ipv4Addr::new(192, 0, 2, 131),
            profiles: vec!["default".to_string()],
            config: BTreeMap::new(),
            devices: BTreeMap::from([(
                NIC.to_string(),
                device(&[("type", "nic"), ("name", NIC), ("network", "br1")]),
            )]),
            config_hash: String::new(),
            stopped,
        };
        let target = AdoptTarget {
            instance: "svc1".to_string(),
            to: "arcak-adopt-svc1".to_string(),
        };
        AdoptWriter::new(socket, target, vec!["br1".to_string()], pre)
            .and_then(|writer| writer.with_snapshot("arca-adopt-0123456789ab"))
            .expect("the writer")
    }

    #[test]
    fn a_request_outside_the_step_table_is_refused_before_any_byte_is_sent() {
        let dir = std::env::temp_dir().join(format!("arca-adopt-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the directory");
        let state = FakeState {
            server: json!({}),
            projects: vec![json!({ "name": "default", "config": {} })],
            instances: Vec::new(),
            pools: Vec::new(),
            networks: Vec::new(),
        };
        let fake = FakeIncus::start(&dir, state).expect("the fake");
        let writer = writer(fake.socket().to_path_buf());
        let request = |method, path: &str, body| AdoptRequest {
            step: AdoptStep::Mark,
            method,
            path: path.to_string(),
            if_match: false,
            body,
            only_if: None,
            from_read: None,
        };
        let outside = [
            request("DELETE", "/1.0/instances/svc1?project=default", Value::Null),
            request("DELETE", "/1.0/projects/arcak-adopt-svc1", Value::Null),
            request(
                "PATCH",
                "/1.0/projects/arcak-adopt-svc1",
                json!({ "config": {} }),
            ),
            request(
                "PUT",
                "/1.0/projects/arcak-adopt-svc1",
                json!({ "config": {} }),
            ),
            request(
                "PATCH",
                "/1.0/networks/br1",
                json!({ "config": { "ipv4.address": "none" } }),
            ),
            request(
                "PUT",
                "/1.0/profiles/default?project=default",
                json!({ "devices": {} }),
            ),
            request(
                "POST",
                "/1.0/instances?project=default",
                json!({ "name": "svc2" }),
            ),
            request(
                "POST",
                "/1.0/instances/svc1?project=default",
                json!({ "name": "svc1", "project": "bench" }),
            ),
            request(
                "PATCH",
                "/1.0/instances/svc1?project=default",
                json!({ "config": { "security.privileged": "true" } }),
            ),
            request(
                "PATCH",
                "/1.0/instances/svc1?project=default",
                json!({ "devices": { "eth0": {} } }),
            ),
            request(
                "PUT",
                "/1.0/instances/svc1/state?project=default",
                json!({ "action": "restart" }),
            ),
            request(
                "POST",
                "/1.0/network-acls?project=default",
                json!({ "name": "a1" }),
            ),
        ];
        for request in &outside {
            let sent = writer.send(request, None, None);
            assert!(
                matches!(sent, Err(IncusError::Refused(_))),
                "{request:?}: {sent:?}"
            );
        }
        // A table request is refused too when its ETag does not match the table: Mark is sent under If-Match only.
        let mark = writer.requests(AdoptStep::Mark).remove(0);
        assert!(matches!(
            writer.send(&mark, None, None),
            Err(IncusError::Refused(_))
        ));
        let start = writer.requests(AdoptStep::Start).remove(0);
        assert!(matches!(
            writer.send(&start, Some("\"x\""), None),
            Err(IncusError::Refused(_))
        ));
        // The one DELETE in the table is the adoption project's, under DeleteProject: the same DELETE under another step,
        // and a DELETE of the instance under DeleteProject, are not.
        let delete = writer.requests(AdoptStep::DeleteProject).remove(0);
        assert_eq!(
            (delete.method, delete.path.as_str()),
            ("DELETE", "/1.0/projects/arcak-adopt-svc1")
        );
        for step in AdoptStep::ADOPT.iter().chain(AdoptStep::RELEASE.iter()) {
            for listed in writer.requests(*step) {
                assert!(
                    listed.method != "DELETE" || listed == delete,
                    "{step:?}: {listed:?}"
                );
            }
        }
        let instance = AdoptRequest {
            path: "/1.0/instances/svc1?project=default".to_string(),
            ..delete.clone()
        };
        let under_mark = AdoptRequest {
            step: AdoptStep::Mark,
            ..delete
        };
        for request in [instance, under_mark] {
            assert!(
                matches!(
                    writer.send(&request, None, None),
                    Err(IncusError::Refused(_))
                ),
                "{request:?}"
            );
        }
        // A writer whose plan localizes nothing has no LocalizeRoot or RestoreRoot request, so neither PUT nor PATCH of a
        // root is in its table.
        assert!(writer.requests(AdoptStep::LocalizeRoot).is_empty());
        assert!(writer.requests(AdoptStep::RestoreRoot).is_empty());
        assert!(
            fake.requests().is_empty(),
            "no byte reached the fake: {:?}",
            fake.requests()
        );
        drop(fake);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_plan_refuses_a_running_plans_stop_and_start_requests_before_any_byte_is_sent() {
        let dir = std::env::temp_dir().join(format!("arca-adopt-stopped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the directory");
        let state = FakeState {
            server: json!({}),
            projects: vec![json!({ "name": "default", "config": {} })],
            instances: Vec::new(),
            pools: Vec::new(),
            networks: Vec::new(),
        };
        let fake = FakeIncus::start(&dir, state).expect("the fake");
        let running = writer(fake.socket().to_path_buf());
        let stopped = writer_of(fake.socket().to_path_buf(), true);
        let mut checked = 0;
        for step in [
            AdoptStep::Stop,
            AdoptStep::Start,
            AdoptStep::ReleaseStop,
            AdoptStep::ReleaseStart,
        ] {
            assert!(stopped.requests(step).is_empty(), "{step:?}");
            for request in running.requests(step) {
                let sent = stopped.send(&request, None, None);
                assert!(
                    matches!(sent, Err(IncusError::Refused(_))),
                    "{request:?}: {sent:?}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 6, "two stops and a start, adopting and releasing");
        assert!(
            fake.requests().is_empty(),
            "no byte reached the fake: {:?}",
            fake.requests()
        );
        drop(fake);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
