//! The plan of one import: what the owner's card says, and exactly what Incus is asked to make.
//!
//! [`Plan::new`] takes a parsed source and the request's own arguments and refuses what K1 cannot move. Everything after
//! is a pure function of the two: the instance body, the volume bodies, the ACL bodies, the cloud-init seed, the copies
//! and the card rows. The same arguments always give the same plan, so the card the owner approved is the plan that runs.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

use serde_json::{Map, Value, json};

use crate::source::{
    Bus, Direction, DiskKind, EthContext, Firmware, Protocol, SgRule, SourceDisk, SourceNic,
    SourceVm, bridge_ok,
};

/// The key, in the config of every instance, volume and ACL an import makes, that marks it as an import's. Its value
/// is [`Plan::mark`]. A removal reads the object first and refuses one without it.
pub const IMPORT_KEY: &str = "user.arcakernel.import";
/// The prefix of every ACL an import makes.
pub const ACL_PREFIX: &str = "arca-import-";
/// The longest instance name an import takes, so that its ACL's name stays a valid Incus name.
pub const NAME_MAX: usize = 44;
/// The port the read-back asks the guest's address on.
pub const READ_BACK_PORT: u16 = 22;

/// The arguments of `instance_import` other than the source description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub name: String,
    pub project: String,
    pub pool: String,
    pub bridge: Option<String>,
    pub cpu: Option<u32>,
    pub memory_mib: Option<u64>,
    pub user_data: bool,
    pub start: bool,
}

/// An Incus instance name: a letter first, then letters, digits and `-`, not ending in `-`, at most [`NAME_MAX`].
pub fn instance_name_ok(name: &str) -> bool {
    name.len() <= NAME_MAX
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// An Incus project or storage pool name: letters, digits, `_` and `-`, at most 63 characters.
pub fn plain_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

impl Request {
    /// The request in `args`, which have met the verb's schema. A value the schema could not hold back (a name's
    /// grammar) is refused here, with its pointer.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
        let name = text("name").unwrap_or_default();
        if !instance_name_ok(&name) {
            return Err(format!(
                "args/name: expected an instance name: a letter, then letters, digits and -, at most {NAME_MAX} characters"
            ));
        }
        let project = text("project").unwrap_or_default();
        if !plain_name_ok(&project) {
            return Err("args/project: expected an Incus project name".to_string());
        }
        let pool = text("pool").unwrap_or_default();
        if !plain_name_ok(&pool) {
            return Err("args/pool: expected an Incus storage pool name".to_string());
        }
        let bridge = text("bridge");
        if bridge.as_deref().is_some_and(|bridge| !bridge_ok(bridge)) {
            return Err("args/bridge: expected a bridge name, at most 15 characters".to_string());
        }
        Ok(Self {
            name,
            project,
            pool,
            bridge,
            cpu: args
                .get("cpu")
                .and_then(Value::as_u64)
                .and_then(|cpu| u32::try_from(cpu).ok()),
            memory_mib: args.get("memoryMib").and_then(Value::as_u64),
            user_data: args
                .get("userData")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            start: args.get("start").and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

/// Where a copied disk lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The VM's root disk.
    Root,
    /// A custom block volume of this name.
    Volume(String),
}

/// One disk copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCopyStep {
    /// The Incus device the disk becomes.
    pub device: String,
    pub source: SourceDisk,
    pub target: Target,
}

/// One network ACL the import makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acl {
    pub name: String,
    /// The NIC devices that carry it.
    pub nics: Vec<String>,
    pub ingress: Vec<Value>,
    pub egress: Vec<Value>,
}

/// The cloud-init seed, as the three Incus keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub network_config: Option<String>,
    pub vendor_data: String,
    pub user_data: Option<String>,
}

/// The whole plan of one import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub source: SourceVm,
    pub request: Request,
    /// The value of [`IMPORT_KEY`] on everything the import makes: the first 16 hex digits of the description's sha256.
    pub mark: String,
    pub cpu: u32,
    pub memory_mib: u64,
    pub copies: Vec<DiskCopyStep>,
    pub acls: Vec<Acl>,
    /// Rules of the source's groups that no ACL carries, in the card's words.
    pub rules_left_out: Vec<String>,
    pub seed: Seed,
    /// The address and port the read-back asks, when the source has an address.
    pub read_back: Option<(Ipv4Addr, u16)>,
}

impl Plan {
    /// The plan for `source` as `request` asks. Refuses a source with no disk to copy, one whose NICs sit on more than
    /// one bridge, and a request with NICs and no bridge.
    pub fn new(source: SourceVm, request: Request) -> Result<Self, String> {
        if source.disks.is_empty() {
            return Err(
                "the source has no Ceph disk to copy: K1 moves only VMs whose disks are on Ceph"
                    .to_string(),
            );
        }
        if let Some(first) = source.nics.first() {
            if source.nics.iter().any(|nic| nic.bridge != first.bridge) {
                return Err(
                    "the source's NICs sit on more than one bridge; K1 moves VMs whose NICs share one bridge".to_string(),
                );
            }
            if request.bridge.is_none() {
                return Err(
                    "args/bridge: the source has a NIC, so the host bridge is required".to_string(),
                );
            }
        }
        let cpu = request.cpu.unwrap_or(source.vcpu);
        let memory_mib = request.memory_mib.unwrap_or(source.memory_mib);
        let mut copies = Vec::new();
        for (index, disk) in source.disks.iter().enumerate() {
            let (device, target) = if index == 0 {
                ("root".to_string(), Target::Root)
            } else {
                (
                    format!("disk{index}"),
                    Target::Volume(format!("{}-disk{index}", request.name)),
                )
            };
            copies.push(DiskCopyStep {
                device,
                source: disk.clone(),
                target,
            });
        }
        let (acls, rules_left_out) = acls(&source, &request.name);
        let seed = seed(&source, request.user_data);
        let read_back = source
            .nics
            .iter()
            .find_map(|nic| eth_of(&source, nic).and_then(|eth| eth.ip).or(nic.ip))
            .map(|ip| (ip, READ_BACK_PORT));
        Ok(Self {
            mark: source.digest[..16].to_string(),
            source,
            request,
            cpu,
            memory_mib,
            copies,
            acls,
            rules_left_out,
            seed,
            read_back,
        })
    }

    /// Refuses a disk that is not in `pool`, the one source pool this node reads.
    pub fn check_source_pool(&self, pool: &str) -> Result<(), String> {
        match self.copies.iter().find(|copy| copy.source.pool != pool) {
            Some(copy) => Err(format!(
                "disk {} of the source is in the Ceph pool {}, and this node reads only the source pool {pool}",
                copy.source.disk_id, copy.source.pool
            )),
            None => Ok(()),
        }
    }

    /// The custom volumes the import makes, by name with their size in MiB.
    pub fn volumes(&self) -> Vec<(String, u64)> {
        self.copies
            .iter()
            .filter_map(|copy| match &copy.target {
                Target::Volume(name) => Some((name.clone(), copy.source.size_mib)),
                Target::Root => None,
            })
            .collect()
    }

    /// The body of one custom volume's create.
    pub fn volume_body(&self, name: &str, size_mib: u64) -> Value {
        json!({
            "name": name,
            "type": "custom",
            "content_type": "block",
            "description": format!("Disk of {} imported from an outside hypervisor", self.request.name),
            "config": { "size": format!("{size_mib}MiB"), IMPORT_KEY: self.mark },
        })
    }

    /// The body of one ACL's create.
    pub fn acl_body(&self, acl: &Acl) -> Value {
        json!({
            "name": acl.name,
            "description": format!("Inbound and outbound rules of {}, carried from its source's security groups", self.request.name),
            "config": { IMPORT_KEY: self.mark },
            "ingress": acl.ingress,
            "egress": acl.egress,
        })
    }

    /// The instance's create body. `managed` says whether Incus manages the host bridge: a managed bridge's NIC names
    /// the network and carries the ACL; another's names the bridge as its parent and can carry none.
    pub fn instance_body(&self, managed: bool) -> Value {
        let mut config = Map::new();
        config.insert(IMPORT_KEY.into(), json!(self.mark));
        config.insert("limits.cpu".into(), json!(self.cpu.to_string()));
        config.insert(
            "limits.memory".into(),
            json!(format!("{}MiB", self.memory_mib)),
        );
        match self.source.firmware {
            Firmware::Bios => {
                config.insert("security.csm".into(), json!("true"));
                config.insert("security.secureboot".into(), json!("false"));
            }
            Firmware::Uefi { secure } => {
                config.insert("security.secureboot".into(), json!(secure.to_string()));
            }
        }
        if let Some(network) = &self.seed.network_config {
            config.insert("cloud-init.network-config".into(), json!(network));
        }
        config.insert(
            "cloud-init.vendor-data".into(),
            json!(self.seed.vendor_data),
        );
        if let Some(user) = &self.seed.user_data {
            config.insert("cloud-init.user-data".into(), json!(user));
        }
        let mut devices = Map::new();
        let count = self.copies.len();
        for (index, copy) in self.copies.iter().enumerate() {
            let mut device = json!({ "type": "disk", "pool": self.request.pool });
            match &copy.target {
                Target::Root => {
                    device["path"] = json!("/");
                    device["size"] = json!(format!("{}MiB", copy.source.size_mib));
                }
                Target::Volume(name) => device["source"] = json!(name),
            }
            // The boot disk first, the others in the source's order after it.
            device["boot.priority"] = json!((count - index).to_string());
            if copy.source.bus == Bus::VirtioBlk {
                device["io.bus"] = json!("virtio-blk");
            }
            devices.insert(copy.device.clone(), device);
        }
        let bridge = self.request.bridge.clone().unwrap_or_default();
        for (index, nic) in self.source.nics.iter().enumerate() {
            let name = format!("eth{index}");
            let mut device = if managed {
                json!({ "type": "nic", "network": bridge, "hwaddr": nic.mac })
            } else {
                json!({ "type": "nic", "nictype": "bridged", "parent": bridge, "hwaddr": nic.mac })
            };
            if managed && let Some(acl) = self.acls.iter().find(|acl| acl.nics.contains(&name)) {
                device["security.acls"] = json!(acl.name);
                device["security.acls.default.ingress.action"] = json!("drop");
                device["security.acls.default.egress.action"] = json!("drop");
            }
            devices.insert(name, device);
        }
        devices.insert(
            "cloud-init".into(),
            json!({ "type": "disk", "source": "cloud-init:config" }),
        );
        json!({
            "name": self.request.name,
            "type": "virtual-machine",
            "description": format!("Imported from an outside hypervisor: {} (id {})", clean(&self.source.name, 64), self.source.id),
            "source": { "type": "none" },
            "start": false,
            "profiles": [],
            "config": config,
            "devices": devices,
        })
    }

    /// The card's one-line summary.
    pub fn summary(&self) -> String {
        format!(
            "Move the VM {} (id {}) from the outside hypervisor into the instance {} in project {}",
            clean(&self.source.name, 64),
            self.source.id,
            self.request.name,
            self.request.project
        )
    }

    /// The card's rows, in order: what the owner approves, in words.
    pub fn card(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = Vec::new();
        let mut row = |label: &str, value: String| rows.push((label.to_string(), value));
        let source = &self.source;
        let state = source
            .state
            .map(|state| format!("; it was {state} when this description was read"))
            .unwrap_or_default();
        row(
            "Source",
            format!(
                "{} (id {}) in the outside hypervisor{state}. Description sha256 {}",
                clean(&source.name, 64),
                source.id,
                &source.digest[..16]
            ),
        );
        row("Instance", self.request.name.clone());
        row("Project", self.request.project.clone());
        row("Size", self.size_words());
        row("Boot", self.boot_words());
        for (index, copy) in self.copies.iter().enumerate() {
            row(
                &format!("Disk {}", index + 1),
                disk_words(copy, &self.request.pool),
            );
        }
        for skipped in &source.skipped_disks {
            row(
                "Disk not copied",
                format!("disk {}: {}", skipped.disk_id, skipped.why),
            );
        }
        let bridge = self.request.bridge.clone().unwrap_or_default();
        for (index, nic) in source.nics.iter().enumerate() {
            row(
                &format!("NIC {}", index + 1),
                nic_words(source, nic, &bridge),
            );
        }
        row("Inbound and outbound rules", self.rules_words());
        if !self.rules_left_out.is_empty() {
            row("Rules not carried", list_words(&self.rules_left_out, 6));
        }
        row("Cloud-init", self.seed_words());
        let mut not_carried = Vec::new();
        if let Some(max) = source.memory_max_mib {
            not_carried.push(format!("memory hot-plug up to {}", mib_words(max)));
        }
        if !source.context.dropped.is_empty() {
            not_carried.push(format!(
                "context keys {}",
                list_words(&source.context.dropped, 8)
            ));
        }
        not_carried
            .push("the source's snapshots (only each disk as it is now is copied)".to_string());
        row("Not carried", not_carried.join("; "));
        row(
            "Before you approve",
            "Power the source off in the outside hypervisor (power off or undeploy, never delete). Arca checks that no one holds its disks and stops before making anything if one does."
                .to_string(),
        );
        row("Downtime", self.downtime_words());
        row("Read-back", self.read_back_words());
        row(
            "Effect",
            format!(
                "Arca makes the VM {} stopped, copies every disk listed here from the source pool into the Incus pool {}, {}. If a step before the start fails, Arca removes what it made. The source is never written.",
                self.request.name,
                self.request.pool,
                if self.request.start {
                    "then starts it and reads it back"
                } else {
                    "and leaves it stopped"
                }
            ),
        );
        rows
    }

    fn size_words(&self) -> String {
        let now = format!(
            "{} CPU{}, {} memory",
            self.cpu,
            if self.cpu == 1 { "" } else { "s" },
            mib_words(self.memory_mib)
        );
        if self.cpu == self.source.vcpu && self.memory_mib == self.source.memory_mib {
            format!("{now}, as the source")
        } else {
            format!(
                "{now}; the source has {} CPU{} and {}",
                self.source.vcpu,
                if self.source.vcpu == 1 { "" } else { "s" },
                mib_words(self.source.memory_mib)
            )
        }
    }

    fn boot_words(&self) -> String {
        let firmware = match self.source.firmware {
            Firmware::Bios => "BIOS (legacy boot), as the source".to_string(),
            Firmware::Uefi { secure } => format!(
                "UEFI, secure boot {}, as the source. Its UEFI boot entries are not carried: the guest boots from its disk's fallback loader",
                if secure { "on" } else { "off" }
            ),
        };
        match &self.source.machine {
            Some(machine) if machine.contains("i440fx") || machine == "pc" => format!(
                "{firmware}. The source's machine type is {machine}; Incus has only q35, so the guest sees new PCI addresses and its NIC names may change (its network is matched by MAC)"
            ),
            _ => firmware,
        }
    }

    fn rules_words(&self) -> String {
        if self.acls.is_empty() {
            return "None: the source's NICs name no security group, so nothing is filtered"
                .to_string();
        }
        self.acls
            .iter()
            .map(|acl| {
                let outbound = if acl.egress.is_empty() {
                    "no outbound traffic (the source's groups allow none)".to_string()
                } else {
                    format!("outbound only by {} rule{}", acl.egress.len(), plural(acl.egress.len()))
                };
                let inbound = if acl.ingress.is_empty() {
                    "no inbound traffic".to_string()
                } else {
                    format!(
                        "inbound only {}",
                        list_words(&acl.ingress.iter().map(rule_words).collect::<Vec<_>>(), 6)
                    )
                };
                format!(
                    "The Incus network ACL {} on {}: {inbound}; {outbound}. Everything else is dropped, as the source's groups did. The bridge must be one Incus manages: on another, the import stops before making anything",
                    acl.name,
                    acl.nics.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(". ")
    }

    fn seed_words(&self) -> String {
        let context = &self.source.context;
        let mut parts = Vec::new();
        match &self.seed.network_config {
            Some(_) => parts.push(format!(
                "the network of {} NIC{} by MAC, from the source's context",
                self.source.nics.len(),
                plural(self.source.nics.len())
            )),
            None => parts.push(
                "no network config (the source's context gave none, so the guest keeps its own)"
                    .to_string(),
            ),
        }
        parts.push(format!(
            "{} SSH key{}",
            context.ssh_keys.len(),
            plural(context.ssh_keys.len())
        ));
        if let Some(hostname) = &context.hostname {
            parts.push(format!("the hostname {hostname}"));
        }
        parts.push("the guest keeps its SSH host keys".to_string());
        match (&context.user_data, &self.seed.user_data) {
            (Some(_), Some(data)) => parts.push(format!(
                "the source's user data ({} line{}), which runs again on the first boot because cloud-init sees a new instance",
                data.lines().count(),
                plural(data.lines().count())
            )),
            (Some(_), None) => parts.push("not the source's user data (userData is false)".to_string()),
            _ => {}
        }
        parts.join("; ")
    }

    fn downtime_words(&self) -> String {
        let total: u64 = self.copies.iter().map(|copy| copy.source.size_mib).sum();
        format!(
            "From the source's power-off until the read-back. The copy reads only the data each disk holds, at most {} in all",
            mib_words(total)
        )
    }

    fn read_back_words(&self) -> String {
        if !self.request.start {
            return "None: the VM is left stopped".to_string();
        }
        match self.read_back {
            Some((ip, port)) => format!(
                "After the start Arca asks whether the Incus agent answers (an imported guest often has none) and whether {ip} accepts a connection on port {port}, for up to five minutes. A failed read-back leaves the VM running so it can be looked at"
            ),
            None => "After the start Arca asks whether the Incus agent answers; the source has no address to ask".to_string(),
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// `text` with control characters as spaces, cut at `max` characters with an ellipsis.
pub fn clean(text: &str, max: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= max {
        return clean;
    }
    let mut cut: String = clean.chars().take(max).collect();
    cut.push('…');
    cut
}

/// 16384 MiB as `16 GiB`, 1536 as `1536 MiB`.
pub fn mib_words(mib: u64) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

fn list_words(items: &[String], max: usize) -> String {
    let shown: Vec<String> = items.iter().take(max).map(|item| clean(item, 80)).collect();
    match items.len().saturating_sub(max) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

fn disk_words(copy: &DiskCopyStep, pool: &str) -> String {
    let disk = &copy.source;
    let what = match &disk.kind {
        DiskKind::Persistent { image_name } => format!("the image {}", clean(image_name, 64)),
        DiskKind::Clone { base, image_name } => format!(
            "a copy of the base image {} ({}); Arca reads through to the base, so the new disk stands alone",
            clean(image_name, 64),
            base
        ),
        DiskKind::Volatile { format } => format!("a volatile {format} disk"),
    };
    let lands = match &copy.target {
        Target::Root => format!("the root disk, in the Incus pool {pool}"),
        Target::Volume(name) => format!(
            "the volume {name} in the Incus pool {pool}, as device {}",
            copy.device
        ),
    };
    let bus = match disk.bus {
        Bus::VirtioBlk => ", on the virtio-blk bus as the source",
        Bus::Scsi => "",
    };
    format!(
        "{}/{} ({}), {what}. Copied whole into {lands}{bus}",
        disk.pool,
        disk.image,
        mib_words(disk.size_mib)
    )
}

fn nic_words(source: &SourceVm, nic: &SourceNic, bridge: &str) -> String {
    let address = eth_of(source, nic)
        .and_then(|eth| eth.ip)
        .or(nic.ip)
        .map(|ip| format!(", address {ip}"))
        .unwrap_or_default();
    let was = if nic.bridge == bridge {
        String::new()
    } else {
        format!(" (it was on {} in the outside hypervisor)", nic.bridge)
    };
    format!("MAC {}{address}, on the host bridge {bridge}{was}", nic.mac)
}

/// The context that configures `nic`: the one whose `ETH<n>_MAC` is its MAC, else `ETH<NIC_ID>`.
fn eth_of<'a>(source: &'a SourceVm, nic: &SourceNic) -> Option<&'a EthContext> {
    let eth = &source.context.eth;
    eth.values()
        .find(|eth| eth.mac.as_deref() == Some(nic.mac.as_str()))
        .or_else(|| eth.get(&nic.nic_id))
}

/// The first and last address of `size` addresses from `first`, as Incus writes a range.
fn range_of(first: IpAddr, size: u64) -> Option<String> {
    let size = size.max(1);
    match first {
        IpAddr::V4(ip) => {
            let start = u32::from(ip);
            let last = start.checked_add(u32::try_from(size - 1).ok()?)?;
            Some(if size == 1 {
                ip.to_string()
            } else {
                format!("{ip}-{}", Ipv4Addr::from(last))
            })
        }
        IpAddr::V6(ip) => {
            let start = u128::from(ip);
            let last = start.checked_add(u128::from(size - 1))?;
            Some(if size == 1 {
                ip.to_string()
            } else {
                format!("{ip}-{}", std::net::Ipv6Addr::from(last))
            })
        }
    }
}

/// One source rule as an Incus ACL rule, or why it is not carried.
fn acl_rule(rule: &SgRule) -> Result<Value, String> {
    let group = if rule.group_name.is_empty() {
        format!("group {}", rule.group_id)
    } else {
        format!("group {} ({})", rule.group_id, clean(&rule.group_name, 40))
    };
    let mut out = json!({ "action": "allow", "state": "enabled" });
    match rule.protocol {
        Protocol::All => {}
        Protocol::Tcp => out["protocol"] = json!("tcp"),
        Protocol::Udp => out["protocol"] = json!("udp"),
        Protocol::Icmp => out["protocol"] = json!("icmp4"),
        Protocol::Icmpv6 => out["protocol"] = json!("icmp6"),
        Protocol::Other => {
            return Err(format!(
                "a rule of {group} for a protocol an Incus ACL cannot hold"
            ));
        }
    }
    if let (Some(range), Protocol::Tcp | Protocol::Udp) = (&rule.range, rule.protocol) {
        out["destination_port"] = json!(range.replace(':', "-"));
    }
    if let (Some(icmp), Protocol::Icmp | Protocol::Icmpv6) = (rule.icmp_type, rule.protocol) {
        out["icmp_type"] = json!(icmp.to_string());
    }
    match (rule.ip, rule.network_id) {
        (Some(ip), _) => {
            let range = range_of(ip, rule.size.unwrap_or(1))
                .ok_or_else(|| format!("a rule of {group} whose address range does not fit"))?;
            let side = match rule.direction {
                Direction::Inbound => "source",
                Direction::Outbound => "destination",
            };
            out[side] = json!(range);
        }
        (None, Some(network)) => {
            return Err(format!(
                "a rule of {group} that names only the outside hypervisor's network {network}"
            ));
        }
        (None, None) => {}
    }
    out["description"] = json!(format!("from the source's {group}"));
    Ok(out)
}

/// The ACLs: one for the NIC that names security groups, or one per such NIC when several do. Each holds the rules of
/// its NIC's groups that an Incus ACL can carry; the others are named as not carried.
fn acls(source: &SourceVm, name: &str) -> (Vec<Acl>, Vec<String>) {
    let with_groups: Vec<(usize, &SourceNic)> = source
        .nics
        .iter()
        .enumerate()
        .filter(|(_, nic)| !nic.security_groups.is_empty())
        .collect();
    let mut acls = Vec::new();
    let mut left_out: BTreeMap<String, ()> = BTreeMap::new();
    for (index, nic) in &with_groups {
        let acl_name = if with_groups.len() == 1 {
            format!("{ACL_PREFIX}{name}")
        } else {
            format!("{ACL_PREFIX}{name}-eth{index}")
        };
        let mut ingress = Vec::new();
        let mut egress = Vec::new();
        for rule in source
            .rules
            .iter()
            .filter(|rule| nic.security_groups.contains(&rule.group_id))
        {
            match acl_rule(rule) {
                Ok(carried) => match rule.direction {
                    Direction::Inbound => ingress.push(carried),
                    Direction::Outbound => egress.push(carried),
                },
                Err(why) => {
                    left_out.insert(why, ());
                }
            }
        }
        dedup(&mut ingress);
        dedup(&mut egress);
        acls.push(Acl {
            name: acl_name,
            nics: vec![format!("eth{index}")],
            ingress,
            egress,
        });
    }
    (acls, left_out.into_keys().collect())
}

fn dedup(rules: &mut Vec<Value>) {
    let mut seen = Vec::new();
    rules.retain(|rule| {
        if seen.contains(rule) {
            false
        } else {
            seen.push(rule.clone());
            true
        }
    });
}

/// One ACL rule in the card's words: `tcp 22 from 192.0.2.0-192.0.2.255`.
fn rule_words(rule: &Value) -> String {
    let text = |key: &str| rule.get(key).and_then(Value::as_str);
    let mut words = text("protocol").unwrap_or("any protocol").to_string();
    if let Some(ports) = text("destination_port") {
        words.push_str(&format!(" {ports}"));
    }
    if let Some(icmp) = text("icmp_type") {
        words.push_str(&format!(" type {icmp}"));
    }
    if let Some(from) = text("source") {
        words.push_str(&format!(" from {from}"));
    }
    if let Some(to) = text("destination") {
        words.push_str(&format!(" to {to}"));
    }
    words
}

/// The seed: the network config when the context gave the network, the vendor data always, the user data when asked.
fn seed(source: &SourceVm, user_data: bool) -> Seed {
    let context = &source.context;
    let network_config = context.network.then(|| {
        let mut ethernets = Map::new();
        let mut gateway_given = false;
        for (index, nic) in source.nics.iter().enumerate() {
            let eth = eth_of(source, nic).cloned().unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("match".into(), json!({ "macaddress": nic.mac }));
            let mut addresses = Vec::new();
            if let Some(ip) = eth.ip.or(nic.ip) {
                addresses.push(json!(format!("{ip}/{}", eth.prefix.unwrap_or(24))));
            }
            if let Some(ip6) = eth.ip6 {
                addresses.push(json!(format!("{ip6}/{}", eth.ip6_prefix.unwrap_or(64))));
            }
            if addresses.is_empty() {
                entry.insert("dhcp4".into(), json!(true));
            } else {
                entry.insert("addresses".into(), json!(addresses));
            }
            let mut routes = Vec::new();
            if let Some(gateway) = eth.gateway
                && !gateway_given
            {
                routes.push(json!({ "to": "0.0.0.0/0", "via": gateway.to_string() }));
                gateway_given = true;
            }
            if let Some(gateway) = eth.ip6_gateway {
                routes.push(json!({ "to": "::/0", "via": gateway.to_string() }));
            }
            if !routes.is_empty() {
                entry.insert("routes".into(), json!(routes));
            }
            if !eth.dns.is_empty() || !eth.search.is_empty() {
                let mut nameservers = Map::new();
                if !eth.dns.is_empty() {
                    nameservers.insert(
                        "addresses".into(),
                        json!(eth.dns.iter().map(ToString::to_string).collect::<Vec<_>>()),
                    );
                }
                if !eth.search.is_empty() {
                    nameservers.insert("search".into(), json!(eth.search));
                }
                entry.insert("nameservers".into(), Value::Object(nameservers));
            }
            if let Some(mtu) = eth.mtu {
                entry.insert("mtu".into(), json!(mtu));
            }
            ethernets.insert(format!("eth{index}"), Value::Object(entry));
        }
        yaml(&json!({ "version": 2, "ethernets": ethernets }))
    });
    let mut vendor = Map::new();
    if let Some(hostname) = &context.hostname {
        vendor.insert("hostname".into(), json!(hostname));
    }
    vendor.insert("ssh_deletekeys".into(), json!(false));
    if !context.ssh_keys.is_empty() {
        vendor.insert("ssh_authorized_keys".into(), json!(context.ssh_keys));
    }
    Seed {
        network_config,
        vendor_data: format!("#cloud-config\n{}", yaml(&Value::Object(vendor))),
        user_data: context.user_data.clone().filter(|_| user_data),
    }
}

/// `value` as block YAML. Every string is written as a double-quoted JSON string, which YAML reads as the same string,
/// so no value can become a key, a list or a tag.
pub fn yaml(value: &Value) -> String {
    let mut out = String::new();
    write_yaml(value, 0, &mut out);
    out
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string()),
        other => other.to_string(),
    }
}

fn write_yaml(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                let key = if !key.is_empty()
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    key.clone()
                } else {
                    scalar(&Value::String(key.clone()))
                };
                match item {
                    Value::Object(inner) if !inner.is_empty() => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        write_yaml(item, indent + 2, out);
                    }
                    Value::Array(inner) if !inner.is_empty() => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        write_yaml(item, indent + 2, out);
                    }
                    Value::Object(_) => out.push_str(&format!("{pad}{key}: {{}}\n")),
                    Value::Array(_) => out.push_str(&format!("{pad}{key}: []\n")),
                    _ => out.push_str(&format!("{pad}{key}: {}\n", scalar(item))),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::Object(inner) if !inner.is_empty() => {
                        let mut nested = String::new();
                        write_yaml(item, indent + 2, &mut nested);
                        // The first line of the mapping goes after the dash.
                        let body = nested.trim_start_matches(' ');
                        out.push_str(&format!("{pad}- {body}"));
                    }
                    Value::Array(_) | Value::Object(_) => {
                        out.push_str(&format!("{pad}- {}\n", item));
                    }
                    _ => out.push_str(&format!("{pad}- {}\n", scalar(item))),
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_its_first_and_last_address() {
        let ip: IpAddr = "192.0.2.0".parse().expect("an address");
        assert_eq!(range_of(ip, 256).as_deref(), Some("192.0.2.0-192.0.2.255"));
        assert_eq!(range_of(ip, 1).as_deref(), Some("192.0.2.0"));
        let top: IpAddr = "255.255.255.255".parse().expect("an address");
        assert_eq!(range_of(top, 2), None);
    }

    #[test]
    fn yaml_quotes_every_string() {
        let text = yaml(&json!({ "a": "x: y", "b": [{ "c": "- d", "e": 1 }], "f": [] }));
        assert_eq!(text, "a: \"x: y\"\nb:\n  - c: \"- d\"\n    e: 1\nf: []\n");
        assert_eq!(yaml(&json!({ "x: y": 1 })), "\"x: y\": 1\n");
    }
}
