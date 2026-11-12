//! Tenants: the scope a verb's `tenant` argument gives it, the address blocks, the caps, and the tenant verbs. A tenant is
//! the entry `tenant-<id>`; everything in it is an entry whose record names the tenant's project. Rules 61 to 66 of the
//! ownership document.
use super::*;
use arca_kernel::desired::Entry;
use arca_kernel::ownership::{tenant_entry, tenant_id_ok, tenant_of_project, tenant_project};
use arca_netproject::tenant::{
    self as block, CONTAINER_ROOT_SIZE, PROFILE_CPU, PROFILE_MEMORY, TenantCaps, TenantConfig,
};
use std::net::Ipv4Addr;

/// The tenant a call acts in: its id, its project and the first address of its block.
#[derive(Clone)]
pub(super) struct TenantScope {
    pub id: String,
    pub project: String,
    pub block: block::Block,
    pub config: TenantConfig,
}

/// What every tenant verb, and every verb given a `tenant`, answers on a node without `kernel.tenants`.
pub(super) fn no_tenants() -> CallError {
    CallError::Refused {
        code: "no_tenants".into(),
        message: "this node has no tenants".into(),
    }
}

/// What a database verb answers for a call that names a tenant: the route that works inside one.
pub(super) fn database_route(verb: &str) -> CallError {
    service_refused(format!(
        "{verb}: inside a tenant, a database is the postgres service template, created with service_create and linked with service_connect; the postgres verbs act in the node's own project only"
    ))
}

fn tenant_not_found(id: &str) -> CallError {
    service_not_found(format!("tenant {id}: not found"))
}

/// Every tenant the desired state holds, with its record.
pub(super) fn tenants(entries: &[Entry]) -> Vec<(Entry, TenantConfig)> {
    entries
        .iter()
        .filter(|entry| entry.plugin.0 == "arca.tenant")
        .filter_map(|entry| {
            serde_json::from_value(entry.config.clone())
                .ok()
                .map(|config| (entry.clone(), config))
        })
        .collect()
}

/// The scope of tenant `id`, or "not found".
fn scope_of(cfg: &NodeConfig, id: &str, entries: &[Entry]) -> Result<TenantScope, CallError> {
    let service_cidr = cfg.kernel.service_cidr.as_deref().ok_or_else(no_tenants)?;
    tenants(entries)
        .into_iter()
        .find(|(entry, config)| entry.id.0 == tenant_entry(id) && config.id == id)
        .and_then(|(_, config)| {
            Some(TenantScope {
                id: id.to_string(),
                project: config.project.clone(),
                block: block::parse_block(&config.block, service_cidr)?,
                config,
            })
        })
        .ok_or_else(|| tenant_not_found(id))
}

/// The tenant a call's `tenant` argument names, or `None` when it names none. A node without `kernel.tenants` has no
/// tenants, whatever the argument says.
pub(super) fn tenant_arg(
    cfg: &NodeConfig,
    args: &Value,
    entries: &[Entry],
) -> Result<Option<TenantScope>, CallError> {
    let Some(value) = args.get("tenant") else {
        return Ok(None);
    };
    if cfg.kernel.tenants.is_none() {
        return Err(no_tenants());
    }
    let id = value
        .as_str()
        .filter(|id| tenant_id_ok(id))
        .ok_or_else(|| CallError::Invalid("tenant: expected a tenant id".into()))?;
    scope_of(cfg, id, entries).map(Some)
}

/// The service /24 and its gateway for a call in a tenant. A tenant sits on the node's managed bridge whether or not this
/// kernel also runs services of its own, so the node's own service project is not asked for.
pub(super) fn network(cfg: &NodeConfig) -> Result<(String, String), CallError> {
    let cidr = cfg.kernel.service_cidr.as_deref().ok_or_else(no_tenants)?;
    let gateway = crate::service::gateway(cidr)
        .map_err(|_| service_refused("the service CIDR is invalid"))?;
    Ok((cidr.to_string(), gateway.to_string()))
}

/// The project an entry's record names, for the kinds that live in a project: a service, a database and a consumer.
fn record_project(entry: &Entry) -> Option<&str> {
    matches!(
        entry.plugin.0.as_str(),
        "arca.service" | "arca.postgres" | "arca.consumer"
    )
    .then(|| entry.config["project"].as_str())
    .flatten()
}

/// The entries a call sees. In a tenant: the tenant's own records, and every entry that lives in no project. Outside one:
/// everything but the tenants' records. One tenant's records are never in another's view, nor in the node's own.
pub(super) fn scoped_entries(entries: &[Entry], scope: Option<&TenantScope>) -> Vec<Entry> {
    entries
        .iter()
        .filter(|entry| match (record_project(entry), scope) {
            (None, _) => true,
            (Some(project), Some(scope)) => project == scope.project,
            (Some(project), None) => tenant_of_project(project).is_none(),
        })
        .cloned()
        .collect()
}

/// The instance rows a call sees, by the same rule as [`scoped_entries`].
pub(super) fn scoped_rows(rows: &[Value], scope: Option<&TenantScope>) -> Vec<Value> {
    rows.iter()
        .filter(|row| {
            let project = row["project"].as_str().unwrap_or("");
            match scope {
                Some(scope) => project == scope.project,
                None => tenant_of_project(project).is_none(),
            }
        })
        .cloned()
        .collect()
}

/// Every block a tenant holds.
pub(super) fn held_blocks(cfg: &NodeConfig, entries: &[Entry]) -> Vec<block::Block> {
    let Some(cidr) = cfg.kernel.service_cidr.as_deref() else {
        return Vec::new();
    };
    tenants(entries)
        .iter()
        .filter_map(|(_, config)| block::parse_block(&config.block, cidr))
        .collect()
}

/// The addresses the node's own project may still take: `candidates` without every address inside a tenant's block.
pub(super) fn outside_blocks(candidates: Vec<Ipv4Addr>, blocks: &[block::Block]) -> Vec<Ipv4Addr> {
    candidates
        .into_iter()
        .filter(|ip| !blocks.iter().any(|held| held.holds(*ip)))
        .collect()
}

/// The free address a new instance of the tenant takes: the lowest of its block that nothing uses. A block with none
/// left is a `capacity` refusal of `scope` `node`, not `tenant`. The tenant's own caps are checked before an address is
/// looked for, and a block holds an address for every instance the caps allow, so a tenant that gets here is under its
/// instance cap: its block is full because something else holds addresses in it (a lease on the bridge that is not
/// one of its services). The tenant's plan is not what is full, and nothing the tenant removes makes room.
pub(super) fn free_address(
    scope: &TenantScope,
    used: &BTreeSet<Ipv4Addr>,
) -> Result<String, CallError> {
    let addresses = scope.block.addresses();
    let all = addresses.len() as u128;
    addresses
        .into_iter()
        .find(|ip| !used.contains(ip))
        .map(|ip| ip.to_string())
        .ok_or_else(|| {
            CallError::capacity(
                format!(
                    "tenant {}'s address block {} has no free address",
                    scope.id, scope.config.block
                ),
                Room::of(Whose::Node, Limit::Addresses, all, 1, all),
            )
        })
}

/// The address the node's front service holds, when the node names one and it exists (`kernel.tenants.front`).
pub(super) fn front_address(cfg: &NodeConfig, entries: &[Entry]) -> Option<String> {
    let front = cfg.kernel.tenants.as_ref()?.front.as_deref()?;
    service_entries(entries)
        .into_iter()
        .find(|(_, record)| {
            record.tenant.is_none()
                && record.name == front
                && record.project == arca_netproject::PROJECT
                && !record.on_host_bridge()
        })
        .map(|(_, record)| record.ipv4)
        // A kernel that holds no record of the front (a scratch run beside the node's own kernel) reads the address
        // the front's NIC reserves on the managed bridge.
        .or_else(|| {
            let row = UnixIncus::new(cfg.incus.socket.clone())
                .get(&format!(
                    "/1.0/instances/{front}?project={}",
                    arca_netproject::PROJECT
                ))
                .ok()?;
            row["devices"]
                .as_object()?
                .values()
                .find(|device| device["network"].as_str() == Some(arca_netproject::BRIDGE))
                .and_then(|device| device["ipv4.address"].as_str())
                .map(str::to_string)
        })
        .filter(|ip| ip.parse::<Ipv4Addr>().is_ok())
}

/// The journal of deploys, in the state directory: one line for each create that named a deploy, each confirm and each
/// rollback, with the token that made it. A deploy's 15 minutes are counted from its first line, so a restart does not
/// reset them.
pub const DEPLOYS_FILE: &str = "tenant-deploys.jsonl";
/// How long a deploy stays provisional when nothing confirms it.
pub const PROVISIONAL_MS: u64 = 15 * 60 * 1000;

/// One deploy, as its journal lines state it.
#[derive(Clone, Debug)]
pub(super) struct Deploy {
    pub token: String,
    pub created: u64,
    pub confirmed: bool,
    pub rolled_back: bool,
}

impl Deploy {
    /// Whether the deploy's objects are still provisional at `now`: not confirmed, not rolled back, and under 15
    /// minutes old.
    pub fn provisional(&self, now: u64) -> bool {
        !self.confirmed && !self.rolled_back && now.saturating_sub(self.created) < PROVISIONAL_MS
    }
}

fn deploy_id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Every deploy the journal holds, by tenant and deploy id. A journal that is not there holds none; one that cannot be
/// read refuses the call.
pub(super) fn deploys(cfg: &NodeConfig) -> Result<BTreeMap<(String, String), Deploy>, CallError> {
    let path = cfg.state_dir.join(DEPLOYS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err(internal()),
    };
    let mut found: BTreeMap<(String, String), Deploy> = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        // A torn last line is the only one that does not parse: it recorded nothing.
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(tenant), Some(deploy), Some(token), Some(at)) = (
            row["tenant"].as_str(),
            row["deploy"].as_str(),
            row["token_id"].as_str(),
            row["at_ms"].as_u64(),
        ) else {
            continue;
        };
        let key = (tenant.to_string(), deploy.to_string());
        match row["kind"].as_str() {
            Some("create") => {
                found.entry(key).or_insert(Deploy {
                    token: token.to_string(),
                    created: at,
                    confirmed: false,
                    rolled_back: false,
                });
            }
            Some("confirm") => {
                if let Some(deploy) = found.get_mut(&key) {
                    deploy.confirmed = true;
                }
            }
            Some("rollback") if row["ok"] == json!(true) => {
                if let Some(deploy) = found.get_mut(&key) {
                    deploy.rolled_back = true;
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

/// Appends one line to the deploy journal and makes it durable. Nothing is created, confirmed or rolled back unless its
/// line was written.
fn journal_deploy(cfg: &NodeConfig, row: Value) -> Result<(), CallError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let unavailable = || CallError::Refused {
        code: "DeployJournalUnavailable".into(),
        message: "the deploy journal could not be written, so nothing was changed".into(),
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(cfg.state_dir.join(DEPLOYS_FILE))
        .map_err(|_| unavailable())?;
    let mut bytes = serde_json::to_vec(&row).map_err(|_| internal())?;
    bytes.push(b'\n');
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| unavailable())
}

/// The deploy a create in a tenant names, checked and journaled: `None` when it names none. A deploy is opened by its
/// first create and takes more creates only from the same token, while it is provisional. Outside a tenant no create
/// names a deploy.
pub(super) fn open_deploy(
    cfg: &NodeConfig,
    scope: Option<&TenantScope>,
    args: &Value,
    caller: &Caller,
    entry: &str,
    now: u64,
) -> Result<Option<String>, CallError> {
    let Some(value) = args.get("deploy") else {
        return Ok(None);
    };
    let Some(scope) = scope else {
        return Err(CallError::Invalid(
            "deploy: only a create inside a tenant names a deploy".into(),
        ));
    };
    let id = value
        .as_str()
        .filter(|id| deploy_id_ok(id))
        .ok_or_else(|| {
            CallError::Invalid("deploy: expected 1 to 64 of a-z, 0-9 and \"-\"".into())
        })?;
    if let Some(open) = deploys(cfg)?.get(&(scope.id.clone(), id.to_string())) {
        if open.token != caller.token_id {
            return Err(service_refused(format!(
                "deploy {id} was opened by another token: only the token that opened a deploy adds to it"
            )));
        }
        if !open.provisional(now) {
            return Err(service_refused(format!(
                "deploy {id} is closed: it was confirmed, rolled back, or opened more than 15 minutes ago. Use a new deploy id"
            )));
        }
    }
    journal_deploy(
        cfg,
        json!({ "kind": "create", "tenant": scope.id, "deploy": id, "token_id": caller.token_id,
            "entry": entry, "at_ms": now }),
    )?;
    Ok(Some(id.to_string()))
}

/// The deploy an entry of a tenant was created in, when it names one.
fn entry_deploy(entry: &Entry) -> Option<&str> {
    entry.config["tenant"]["deploy"].as_str()
}

/// Brings the front rule of every tenant service to what the node says now: the address of the node's front service
/// for an ordinary service, and no rule for a provisional one or on a node that names no front. A front that was
/// recreated at another address, a deploy that was confirmed and one whose 15 minutes passed are each followed here, by
/// one desired-state change for the services that differ. Returns how many records changed.
pub fn sync_front(kernel: &Mutex<Kernel>, cfg: &NodeConfig, now: u64) -> Result<usize, CallError> {
    // The pass rewrites whole records: it holds the write gate, so it never writes over a change a handler is making.
    let gate = write_gate(kernel);
    let _write = gate.lock().map_err(|_| internal())?;
    sync_front_held(kernel, cfg, now)
}

/// [`sync_front`], for a caller that already holds the kernel's write gate.
pub(super) fn sync_front_held(
    kernel: &Mutex<Kernel>,
    cfg: &NodeConfig,
    now: u64,
) -> Result<usize, CallError> {
    if cfg.kernel.tenants.is_none() {
        return Ok(0);
    }
    let entries = lock(kernel)?.entries().to_vec();
    if !entries.iter().any(|entry| entry.plugin.0 == "arca.tenant") {
        return Ok(0);
    }
    let front = front_address(cfg, &entries);
    let open = deploys(cfg)?;
    let mut changes = Vec::new();
    for (entry, mut record) in service_entries(&entries) {
        let Some(tenant) = record.tenant.clone() else {
            continue;
        };
        let provisional = tenant.deploy.as_ref().is_some_and(|deploy| {
            open.get(&(tenant.id.clone(), deploy.clone()))
                .is_some_and(|deploy| deploy.provisional(now))
        });
        let wanted = if provisional { None } else { front.clone() };
        if tenant.front == wanted {
            continue;
        }
        record.tenant = Some(crate::service::TenantRef {
            front: wanted,
            ..tenant
        });
        // Only the ACL follows: no environment value is rendered again.
        record.environment_patch = Some(BTreeSet::new());
        changes.push(crate::desired_store::EntryChange::Put(service_entry(
            &entry.id.0,
            "arca.service",
            serde_json::to_value(&record).map_err(|_| internal())?,
        )));
    }
    let changed = changes.len();
    if changed != 0 {
        lock(kernel)?
            .propose(changes, now)
            .map_err(|_| internal())?;
    }
    Ok(changed)
}

/// Follows the front on a thread of its own, every 30 seconds, for as long as the process runs.
pub fn spawn_front_sync(
    kernel: Arc<Mutex<Kernel>>,
    cfg: Arc<NodeConfig>,
    clock: Clock,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("arca-front-sync".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(30));
                if sync_front(&kernel, &cfg, clock()).is_err() {
                    eprintln!("arca: the tenants' front rules were not brought up to date");
                }
            }
        })
        .map(|_| ())
}

/// `snapshot_list` on a node that may hold tenants: a call that names a tenant is answered only for a tenant the node
/// holds, as every other verb answers it, and then lists that tenant's snapshots.
pub struct SnapshotListInTenant {
    pub inner: Arc<dyn ToolHandler>,
    pub kernel: Arc<Mutex<Kernel>>,
    pub cfg: Arc<NodeConfig>,
}

impl ToolHandler for SnapshotListInTenant {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        if args.get("tenant").is_some() {
            let entries = lock(&self.kernel)?.entries().to_vec();
            tenant_arg(&self.cfg, &args, &entries)?;
        }
        self.inner.call(args, caller)
    }
}

/// What one instance counts against its tenant's caps.
#[derive(Clone, Copy, Default)]
struct Use {
    containers: u128,
    virtual_machines: u128,
    cpu: u128,
    memory: u128,
    disk: u128,
}

impl Use {
    fn add(&mut self, other: Use) {
        self.containers += other.containers;
        self.virtual_machines += other.virtual_machines;
        self.cpu = self.cpu.saturating_add(other.cpu);
        self.memory = self.memory.saturating_add(other.memory);
        self.disk = self.disk.saturating_add(other.disk);
    }
}

fn bytes(text: &str) -> u128 {
    // A size that cannot be read counts as over every cap.
    crate::service::size_bytes(text).unwrap_or(u128::MAX)
}

/// What a container counts: its template's limits, or the tenant profile's, and its root disk with its data volume.
fn container_use(limits: &Option<crate::service::Limits>, data: Option<&str>) -> Use {
    let cpu = limits
        .as_ref()
        .and_then(|limits| limits.cpu.as_deref())
        .map_or(u128::from(PROFILE_CPU), |cpu| {
            cpu.parse().unwrap_or(u128::MAX)
        });
    let memory = limits
        .as_ref()
        .and_then(|limits| limits.memory.as_deref())
        .map_or_else(|| bytes(PROFILE_MEMORY), bytes);
    Use {
        containers: 1,
        virtual_machines: 0,
        cpu,
        memory,
        disk: bytes(CONTAINER_ROOT_SIZE).saturating_add(data.map_or(0, bytes)),
    }
}

fn vm_use(limits: &Option<crate::service::Limits>, root_size: &str) -> Use {
    let limit = |pick: fn(&crate::service::Limits) -> Option<&str>| limits.as_ref().and_then(pick);
    Use {
        containers: 0,
        virtual_machines: 1,
        cpu: limit(|l| l.cpu.as_deref()).map_or(u128::MAX, |cpu| cpu.parse().unwrap_or(u128::MAX)),
        memory: limit(|l| l.memory.as_deref()).map_or(u128::MAX, bytes),
        disk: bytes(root_size),
    }
}

/// What the tenant's records use together: its services, whether or not they run.
fn used(scope: &TenantScope, entries: &[Entry]) -> Use {
    let mut total = Use::default();
    for (_, record) in service_entries(entries) {
        if record.project != scope.project {
            continue;
        }
        total.add(match &record.vm {
            Some(vm) => vm_use(&record.limits, &vm.root_size),
            None => container_use(
                &record.limits,
                record.data.as_ref().and_then(|data| data.size.as_deref()),
            ),
        });
    }
    total
}

/// Why one more instance would pass a cap of the tenant, or `None`. The refusal names the cap, in words and as a
/// `capacity` refusal's fields, with `scope` `tenant`: the cap is the tenant's own. It comes before any Incus request: Incus holds the same caps on the project and would
/// refuse too, in its own words.
fn caps_exceeded(
    scope: &TenantScope,
    entries: &[Entry],
    name: &str,
    asked: Use,
) -> Option<CallError> {
    let caps = &scope.config.caps;
    let held = used(scope, entries);
    let gib = |bytes: u128| format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64);
    let plain = |n: u128| n.to_string();
    type Check<'a> = (
        Limit,
        &'a str,
        &'a str,
        u128,
        u128,
        u128,
        &'a dyn Fn(u128) -> String,
    );
    let checks: [Check<'_>; 6] = [
        (
            Limit::Instances,
            "instances",
            "limits.instances",
            held.containers + held.virtual_machines,
            1,
            u128::from(caps.instances),
            &plain,
        ),
        (
            Limit::Containers,
            "containers",
            "limits.containers",
            held.containers,
            asked.containers,
            u128::from(caps.containers),
            &plain,
        ),
        (
            Limit::VirtualMachines,
            "virtual machines",
            "limits.virtual-machines",
            held.virtual_machines,
            asked.virtual_machines,
            u128::from(caps.virtual_machines),
            &plain,
        ),
        (
            Limit::Cpu,
            "CPUs",
            "limits.cpu",
            held.cpu,
            asked.cpu,
            u128::from(caps.cpu),
            &plain,
        ),
        (
            Limit::Memory,
            "memory",
            "limits.memory",
            held.memory,
            asked.memory,
            crate::service::size_bytes(&caps.memory).unwrap_or(0),
            &gib,
        ),
        (
            Limit::Disk,
            "disk",
            "limits.disk",
            held.disk,
            asked.disk,
            crate::service::size_bytes(&caps.disk).unwrap_or(0),
            &gib,
        ),
    ];
    for (limit, what, key, held, asked, cap, show) in checks {
        if asked != 0 && held.saturating_add(asked) > cap {
            return Some(CallError::capacity(
                format!(
                    "creating {name} would exceed tenant {}'s cap on {what} ({key}): {} in use, {} asked, {} allowed",
                    scope.id,
                    show(held),
                    if asked == u128::MAX {
                        "an unreadable amount".to_string()
                    } else {
                        show(asked)
                    },
                    show(cap)
                ),
                Room::of(Whose::Tenant, limit, held, asked, cap),
            ));
        }
    }
    None
}

/// Why creating the service `name` from `template` would pass a cap of the tenant, or `None`. A cap that would be
/// passed is a `capacity` refusal. A template whose data volume cannot be counted is a plain one: nothing ran out.
pub(super) fn service_caps_refusal(
    scope: &TenantScope,
    entries: &[Entry],
    name: &str,
    template: &crate::service::Template,
) -> Option<CallError> {
    let asked = if template.is_vm() {
        vm_use(
            &template.limits,
            template.root_size.as_deref().unwrap_or(""),
        )
    } else {
        if template
            .data
            .as_ref()
            .is_some_and(|data| data.size.is_none())
        {
            return Some(service_refused(format!(
                "template {}: its data volume states no size, so it cannot be counted against tenant {}'s cap on disk (limits.disk)",
                template.name, scope.id
            )));
        }
        container_use(
            &template.limits,
            template.data.as_ref().and_then(|data| data.size.as_deref()),
        )
    };
    caps_exceeded(scope, entries, name, asked)
}

/// The virtual machines every tenant may hold together: the sum of their caps. The node's own virtual-machine cap covers
/// them and the node's own machines (Rule 58).
pub(super) fn reserved_vms(entries: &[Entry]) -> u128 {
    tenants(entries)
        .iter()
        .map(|(_, config)| u128::from(config.caps.virtual_machines))
        .sum()
}

/// Whether every cap is at most the node's default for it.
fn within(caps: &TenantCaps, default: &TenantCaps) -> bool {
    caps.instances <= default.instances
        && caps.containers <= default.containers
        && caps.virtual_machines <= default.virtual_machines
        && caps.cpu <= default.cpu
        && bytes(&caps.memory) <= bytes(&default.memory)
        && bytes(&caps.disk) <= bytes(&default.disk)
}

/// Why one more tenant would pass `kernel.tenants.createRate`, or `None`. The creates are counted from the effect
/// journal (each tenant's project create, whether or not the tenant still exists), so a restart does not reset the count.
/// A journal that cannot be read refuses the create, as a plain refusal: no wait is known to end it. A rate that is
/// passed is a `rate` refusal, with the wait until the next create is allowed.
fn create_rate_refusal(cfg: &NodeConfig, now: u64) -> Option<CallError> {
    let rate = cfg.kernel.tenants.as_ref()?.create_rate;
    let window = rate.per_seconds.saturating_mul(1000);
    let path = cfg.state_dir.join(crate::kernel::JOURNAL_FILE);
    let records = match arca_kernel::journal::read_journal(&path) {
        Ok((records, _)) => records,
        Err(_) if !path.exists() => Vec::new(),
        Err(_) => {
            return Some(service_refused(
                "the journal could not be read, so the tenant create rate (kernel.tenants.createRate) cannot be checked",
            ));
        }
    };
    let mut created: BTreeMap<u64, u64> = BTreeMap::new();
    for record in records {
        if record
            .plugin
            .as_ref()
            .is_some_and(|plugin| plugin.0 == "arca.tenant")
            && record.target.kind == arca_kernel::ownership::IncusKind::Project
            && record.pre_image.is_none()
            && record.intent.is_none()
            && record.state == arca_kernel::journal::EffectState::Applied
            && now.saturating_sub(record.at_ms) < window
        {
            created.insert(record.id, record.at_ms);
        }
    }
    if (created.len() as u64) < u64::from(rate.count) {
        return None;
    }
    // The next create is allowed once enough of the oldest ones have left the span.
    let mut times: Vec<u64> = created.into_values().collect();
    times.sort_unstable();
    let leaving = times[times.len() - rate.count as usize];
    let allowed_at = leaving.saturating_add(window);
    Some(CallError::rate(
        format!(
            "this node creates at most {} tenants in {} seconds (kernel.tenants.createRate), and {} were created in that time; the next create is allowed at {}",
            rate.count,
            rate.per_seconds,
            times.len(),
            iso_utc(allowed_at)
        ),
        Some(seconds_until(allowed_at, now)),
    ))
}

fn caps_json(caps: &TenantCaps) -> Value {
    json!({
        "instances": caps.instances,
        "containers": caps.containers,
        "virtualMachines": caps.virtual_machines,
        "cpu": caps.cpu,
        "memory": caps.memory,
        "disk": caps.disk
    })
}

fn fresh_id(taken: &[String]) -> Result<String, CallError> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    for _ in 0..16 {
        let mut raw = [0u8; 8];
        getrandom::fill(&mut raw).map_err(|_| internal())?;
        let id: String = raw
            .iter()
            .map(|byte| ALPHABET[usize::from(byte & 31)] as char)
            .collect();
        if !taken.contains(&id) {
            return Ok(id);
        }
    }
    Err(internal())
}

/// Why the caps of a new tenant do not fit the node, or `None`: inside `capacity` with every other tenant's, and its
/// virtual machines inside the node's own virtual-machine cap with theirs and the node's own machines. Each is a
/// `capacity` refusal of `scope` `node`, whose amounts are caps set aside, not use: `have` is what the tenants the node holds were
/// promised (with the node's own machines, for virtual machines) and `want` is what this tenant asks to be promised.
/// A node that sets no virtual-machine caps at all is a plain refusal: nothing ran out.
fn admission_refusal(cfg: &NodeConfig, entries: &[Entry], caps: &TenantCaps) -> Option<CallError> {
    let tenants_cfg = cfg.kernel.tenants.as_ref()?;
    let held = tenants(entries);
    if held.len() as u64 >= u64::from(tenants_cfg.max_tenants) {
        return Some(CallError::capacity(
            format!(
                "this node holds {} tenants, and kernel.tenants.maxTenants is {}",
                held.len(),
                tenants_cfg.max_tenants
            ),
            Room::of(
                Whose::Node,
                Limit::Tenants,
                held.len() as u128,
                1,
                u128::from(tenants_cfg.max_tenants),
            ),
        ));
    }
    let capacity = &tenants_cfg.capacity;
    // What the tenants the node holds were promised, and what this one asks for.
    let promised = |pick: &dyn Fn(&TenantCaps) -> u128| -> (u128, u128) {
        let others = held
            .iter()
            .map(|(_, config)| pick(&config.caps))
            .fold(0, u128::saturating_add);
        (others, pick(caps))
    };
    let gib = |bytes: u128| format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64);
    let plain = |n: u128| n.to_string();
    type Check<'a> = (
        Limit,
        &'a str,
        (u128, u128),
        u128,
        &'a dyn Fn(u128) -> String,
    );
    let checks: [Check<'_>; 4] = [
        (
            Limit::Instances,
            "instances",
            promised(&|caps| u128::from(caps.instances)),
            u128::from(capacity.instances),
            &plain,
        ),
        (
            Limit::Cpu,
            "cpu",
            promised(&|caps| u128::from(caps.cpu)),
            u128::from(capacity.cpu),
            &plain,
        ),
        (
            Limit::Memory,
            "memory",
            promised(&|caps| bytes(&caps.memory)),
            crate::service::size_bytes(&capacity.memory).unwrap_or(0),
            &gib,
        ),
        (
            Limit::Disk,
            "disk",
            promised(&|caps| bytes(&caps.disk)),
            crate::service::size_bytes(&capacity.disk).unwrap_or(0),
            &gib,
        ),
    ];
    for (limit, key, (others, asked), cap, show) in checks {
        let wanted = others.saturating_add(asked);
        if wanted > cap {
            return Some(CallError::capacity(
                format!(
                    "the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.{key}): {} with this tenant, {} allowed",
                    show(wanted),
                    show(cap)
                ),
                Room::of(Whose::Node, limit, others, asked, cap),
            ));
        }
    }
    if caps.virtual_machines != 0 {
        let Some(node) = cfg.kernel.vm_caps() else {
            return Some(service_refused(
                "the caps allow virtual machines (virtualMachines), and this node sets no virtual-machine caps (kernel.vmLimits, or kernel.bridged with its limits)",
            ));
        };
        let own = service_entries(entries)
            .iter()
            .filter(|(_, record)| record.vm.is_some() && record.tenant.is_none())
            .count() as u128;
        let taken = reserved_vms(entries) + own;
        let wanted = taken + u128::from(caps.virtual_machines);
        if wanted > u128::from(node.virtual_machines) {
            return Some(CallError::capacity(
                format!(
                    "the tenants' virtual machines (virtualMachines) together with the node's own would exceed the node's virtual-machine cap: {wanted} with this tenant, {} allowed",
                    node.virtual_machines
                ),
                Room::of(
                    Whose::Node,
                    Limit::VirtualMachines,
                    taken,
                    u128::from(caps.virtual_machines),
                    u128::from(node.virtual_machines),
                ),
            ));
        }
    }
    None
}

/// The checks a proposed `tenant_create` passes before the owner is asked. The arguments are left exactly as proposed.
pub(super) fn check_create(
    cfg: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
) -> Result<(), CallError> {
    create_args(cfg, entries, payload).map(|_| ())
}

/// The checks a proposed `deploy_remove` passes before the owner is asked: the tenant and the deploy exist, the deploy
/// is no longer provisional, and something of it is left.
pub(super) fn check_deploy_remove(
    cfg: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
    now: u64,
) -> Result<(), CallError> {
    let scope = tenant_arg(cfg, payload, entries)?
        .ok_or_else(|| CallError::Invalid("tenant: expected a tenant id".into()))?;
    let id = payload["deploy"].as_str().unwrap_or_default();
    let open = deploys(cfg)?
        .remove(&(scope.id.clone(), id.to_string()))
        .ok_or_else(|| service_not_found(format!("deploy {id}: not found")))?;
    if open.rolled_back {
        return Err(service_refused(format!(
            "remove deploy {id}: it was rolled back"
        )));
    }
    if open.provisional(now) {
        return Err(service_refused(format!(
            "remove deploy {id}: it is still provisional; the token that opened it rolls it back with deploy_rollback, or confirms it first"
        )));
    }
    if !entries.iter().any(|entry| {
        record_project(entry) == Some(scope.project.as_str()) && entry_deploy(entry) == Some(id)
    }) {
        return Err(service_refused(format!(
            "remove deploy {id}: nothing of it is left in tenant {}",
            scope.id
        )));
    }
    Ok(())
}

/// The parts of a deploy as the owner's card names them: each service with its kind, in name order.
pub(super) fn deploy_parts(entries: &[Entry], payload: &Value) -> String {
    let (tenant, deploy) = (
        payload["tenant"].as_str().unwrap_or_default(),
        payload["deploy"].as_str().unwrap_or_default(),
    );
    let mut parts: Vec<String> = service_entries(entries)
        .into_iter()
        .filter(|(_, record)| {
            record
                .tenant
                .as_ref()
                .is_some_and(|found| found.id == tenant && found.deploy.as_deref() == Some(deploy))
        })
        .map(|(_, record)| {
            format!(
                "{} ({})",
                record.name,
                if record.vm.is_some() {
                    "virtual machine"
                } else {
                    "container"
                }
            )
        })
        .collect();
    parts.sort();
    parts.join(", ")
}

/// The check a proposed `tenant_remove` passes before the owner is asked: the tenant exists.
pub(super) fn check_remove(
    cfg: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
) -> Result<(), CallError> {
    if cfg.kernel.tenants.is_none() {
        return Err(no_tenants());
    }
    scope_of(cfg, id_arg(payload)?, entries).map(|_| ())
}

/// A `tenant_create`'s id, when it names one, and its caps, each checked against the node.
fn create_args(
    cfg: &NodeConfig,
    entries: &[Entry],
    args: &Value,
) -> Result<(Option<String>, TenantCaps), CallError> {
    let tenants_cfg = cfg.kernel.tenants.as_ref().ok_or_else(no_tenants)?;
    let id = match args.get("id") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|id| tenant_id_ok(id))
                .ok_or_else(|| {
                    CallError::Invalid(
                        "id: expected 8 lowercase base32 characters (a to z, 2 to 7)".into(),
                    )
                })?
                .to_string(),
        ),
    };
    if let Some(id) = &id
        && entries.iter().any(|entry| entry.id.0 == tenant_entry(id))
    {
        return Err(service_refused(format!("tenant {id} exists")));
    }
    let caps: TenantCaps = match args.get("caps") {
        None => tenants_cfg.default_caps.clone(),
        Some(value) => serde_json::from_value(value.clone()).map_err(|_| {
            CallError::Invalid(
                "caps: expected instances, containers, virtualMachines, cpu, memory and disk"
                    .into(),
            )
        })?,
    };
    if !block::caps_ok(&caps, crate::service::size_ok) {
        return Err(CallError::Invalid(
            "caps: instances of 1 to 16, containers and virtualMachines of at most that, cpu of at least 1, and memory and disk as sizes such as 8GiB".into(),
        ));
    }
    if let Some(refusal) = admission_refusal(cfg, entries, &caps) {
        return Err(refusal);
    }
    Ok((id, caps))
}

/// `tenant_create`, `tenant_remove`, `tenant_list`, `tenant_status` and `tenant_usage`.
pub(super) struct TenantHandler {
    pub verb: &'static str,
    pub kernel: Arc<Mutex<Kernel>>,
    pub clock: Clock,
    pub cfg: Arc<NodeConfig>,
    pub writes: Arc<Mutex<()>>,
}

pub fn tenant_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    let writes = write_gate(&kernel);
    [
        "tenant_create",
        "tenant_list",
        "tenant_status",
        "tenant_usage",
        "tenant_remove",
        "deploy_confirm",
        "deploy_rollback",
        "deploy_remove",
    ]
    .into_iter()
    .map(|verb| {
        (
            verb,
            Arc::new(TenantHandler {
                verb,
                kernel: kernel.clone(),
                clock: clock.clone(),
                cfg: cfg.clone(),
                writes: writes.clone(),
            }) as Arc<dyn ToolHandler>,
        )
    })
    .collect()
}

fn id_arg(args: &Value) -> Result<&str, CallError> {
    args["id"]
        .as_str()
        .filter(|id| tenant_id_ok(id))
        .ok_or_else(|| CallError::Invalid("id: expected a tenant id".into()))
}

/// The tenant's instances as Incus lists them, with their state and expanded config.
fn tenant_rows(read: &UnixIncus, scope: &TenantScope) -> Result<Vec<Value>, CallError> {
    service_listing(read, Some(&scope.project))
}

fn status_json(scope: &TenantScope, entries: &[Entry], instances: Option<usize>) -> Value {
    let held = used(scope, entries);
    let mut status = json!({
        "id": scope.id,
        "block": scope.config.block,
        "caps": caps_json(&scope.config.caps),
        "use": {
            "instances": (held.containers + held.virtual_machines) as u64,
            "containers": held.containers as u64,
            "virtualMachines": held.virtual_machines as u64,
            "cpu": held.cpu.min(u128::from(u64::MAX)) as u64,
            "memoryBytes": held.memory.min(u128::from(u64::MAX)) as u64,
            "diskBytes": held.disk.min(u128::from(u64::MAX)) as u64
        }
    });
    if let Some(instances) = instances {
        status["instances"] = json!(instances);
    }
    status
}

/// One instance as `tenant_usage` reports it: what it is allowed, never what it did or when.
fn usage_row(row: &Value) -> Option<Value> {
    let name = row["name"].as_str()?;
    let kind = if row["type"].as_str() == Some(crate::service::VM_KIND) {
        "vm"
    } else {
        "container"
    };
    let config = |key: &str| {
        row["expanded_config"][key]
            .as_str()
            .or_else(|| row["config"][key].as_str())
    };
    let root = ["expanded_devices", "devices"]
        .into_iter()
        .find_map(|field| {
            row[field]
                .as_object()?
                .values()
                .find(|device| device["type"] == "disk" && device["path"] == "/")
                .and_then(|device| device["size"].as_str())
        });
    Some(json!({
        "name": name,
        "kind": kind,
        "state": crate::service::state(row["status"].as_str()),
        "limitsCpu": config("limits.cpu"),
        "limitsMemory": config("limits.memory"),
        "rootDiskSize": root
    }))
}

impl ToolHandler for TenantHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let allowed: &[&str] = match self.verb {
            "tenant_create" => &["id", "caps", "approval"],
            "tenant_remove" => &["id", "approval"],
            "tenant_status" | "tenant_usage" => &["id"],
            "deploy_confirm" | "deploy_rollback" => &["tenant", "deploy"],
            "deploy_remove" => &["tenant", "deploy", "approval"],
            _ => &[],
        };
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(CallError::Invalid(arca_api::unknown_field(
                "", key, allowed,
            )));
        }
        // A node that was never set up for services or tenants, and whose desired state holds no tenant, has none: the
        // list is empty. A node that still holds tenant records, every other tenant verb, and a node set up for services
        // with no tenants policy still answer that the node has no tenants, which claims no list.
        if self.verb == "tenant_list"
            && self.cfg.kernel.tenants.is_none()
            && super::no_service_project_asked(&self.cfg)
            && tenants(lock(&self.kernel)?.entries()).is_empty()
        {
            return Ok(json!({ "tenants": [] }));
        }
        if self.cfg.kernel.tenants.is_none() {
            return Err(no_tenants());
        }
        let writing = matches!(self.verb, "tenant_create" | "tenant_remove");
        if writing {
            if caller.scope < Scope::Doctor {
                return Err(CallError::Forbidden {
                    verb: self.verb.into(),
                });
            }
            let approved = args
                .get("approval")
                .and_then(Value::as_str)
                .is_some_and(|grant| {
                    lock(&self.kernel)
                        .map(|mut kernel| kernel.consume_action_grant(self.verb, &args, grant))
                        .unwrap_or(false)
                });
            // A removal runs only with the one-shot grant of a proposal the owner approved. A create inside the node's
            // standing policy needs no approval, from a token whose tool allowlist names it; caps above the node's
            // defaults are the owner's to approve.
            let standing = self.verb == "tenant_create"
                && self.cfg.kernel.tenants.as_ref().is_some_and(|policy| {
                    args.get("caps").is_none_or(|caps| {
                        serde_json::from_value::<TenantCaps>(caps.clone())
                            .is_ok_and(|caps| within(&caps, &policy.default_caps))
                    })
                });
            if !approved {
                if !standing {
                    return Err(if self.verb == "tenant_create" {
                        CallError::Refused {
                            code: "NotApproved".into(),
                            message: "caps above this node's defaults (kernel.tenants.defaultCaps) need the owner's approval: propose the create with action_propose, and it runs once the owner approves it".into(),
                        }
                    } else {
                        import_not_approved()
                    });
                }
                if !caller
                    .tools
                    .as_ref()
                    .is_some_and(|tools| tools.iter().any(|tool| tool == self.verb))
                {
                    return Err(CallError::Refused {
                        code: "NotAllowed".into(),
                        message: "tenant_create without the owner's approval is for a token whose tool allowlist names tenant_create".into(),
                    });
                }
            }
        }
        let deploying = matches!(
            self.verb,
            "deploy_confirm" | "deploy_rollback" | "deploy_remove"
        );
        // The removal of a deploy runs only with the one-shot grant of a proposal the owner approved.
        if self.verb == "deploy_remove"
            && !args
                .get("approval")
                .and_then(Value::as_str)
                .is_some_and(|grant| {
                    lock(&self.kernel)
                        .map(|mut kernel| kernel.consume_action_grant(self.verb, &args, grant))
                        .unwrap_or(false)
                })
        {
            return Err(import_not_approved());
        }
        if deploying && caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        let _write = (writing || deploying)
            .then(|| self.writes.lock().map_err(|_| internal()))
            .transpose()?;
        let now = (self.clock)();
        let entries = lock(&self.kernel)?.entries().to_vec();
        let read = UnixIncus::new(self.cfg.incus.socket.clone());
        match self.verb {
            "tenant_list" => {
                let mut rows: Vec<Value> = tenants(&entries)
                    .iter()
                    .filter_map(|(_, config)| scope_of(&self.cfg, &config.id, &entries).ok())
                    .map(|scope| status_json(&scope, &entries, None))
                    .collect();
                rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
                Ok(json!({ "tenants": rows }))
            }
            "tenant_status" => {
                let scope = scope_of(&self.cfg, id_arg(&args)?, &entries)?;
                let instances = tenant_rows(&read, &scope)?.len();
                Ok(status_json(&scope, &entries, Some(instances)))
            }
            "tenant_usage" => {
                let scope = scope_of(&self.cfg, id_arg(&args)?, &entries)?;
                let open = deploys(&self.cfg)?;
                // Each instance's service, and the deploy it is still provisional in, from the tenant's own records.
                let records: BTreeMap<String, (String, Option<String>)> = entries
                    .iter()
                    .filter(|entry| record_project(entry) == Some(scope.project.as_str()))
                    .filter_map(|entry| {
                        let service = entry.config["name"].as_str()?;
                        let deploy = entry_deploy(entry).filter(|deploy| {
                            open.get(&(scope.id.clone(), deploy.to_string()))
                                .is_some_and(|deploy| deploy.provisional(now))
                        });
                        Some((
                            crate::service::instance_name(Some(&scope.id), service),
                            (service.to_string(), deploy.map(str::to_string)),
                        ))
                    })
                    .collect();
                let mut instances: Vec<Value> = tenant_rows(&read, &scope)?
                    .iter()
                    .filter_map(usage_row)
                    .map(|mut row| {
                        let record = row["name"].as_str().and_then(|name| records.get(name));
                        row["service"] = json!(record.map(|(service, _)| service));
                        if let Some(deploy) = record.and_then(|(_, deploy)| deploy.as_ref()) {
                            row["provisional"] = json!(true);
                            row["deploy"] = json!(deploy);
                        }
                        row
                    })
                    .collect();
                instances.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({ "id": scope.id, "instances": instances }))
            }
            "deploy_confirm" | "deploy_rollback" | "deploy_remove" => {
                self.deploy(&args, caller, &entries, now)
            }
            "tenant_create" => self.create(&args, &entries, &read, now),
            "tenant_remove" => self.remove(&args, &entries, &read, now),
            _ => Err(service_not_found("unknown tenant verb")),
        }
    }
}

impl TenantHandler {
    /// `deploy_confirm` and `deploy_rollback`: for the token that opened the deploy, while it is provisional.
    fn deploy(
        &self,
        args: &Value,
        caller: &Caller,
        entries: &[Entry],
        now: u64,
    ) -> Result<Value, CallError> {
        let scope = tenant_arg(&self.cfg, args, entries)?
            .ok_or_else(|| CallError::Invalid("tenant: expected a tenant id".into()))?;
        let id = args["deploy"]
            .as_str()
            .filter(|id| deploy_id_ok(id))
            .ok_or_else(|| {
                CallError::Invalid("deploy: expected 1 to 64 of a-z, 0-9 and \"-\"".into())
            })?;
        let open = deploys(&self.cfg)?
            .remove(&(scope.id.clone(), id.to_string()))
            .ok_or_else(|| service_not_found(format!("deploy {id}: not found")))?;
        let removing = self.verb == "deploy_remove";
        let word = match self.verb {
            "deploy_confirm" => "confirm",
            "deploy_remove" => "remove",
            _ => "roll back",
        };
        // A removal is the owner's, whatever token proposed it. A confirm and a rollback are the opening token's.
        if !removing && open.token != caller.token_id {
            return Err(service_refused(format!(
                "{word} deploy {id}: only the token that opened a deploy may {word} it"
            )));
        }
        if open.rolled_back {
            return Err(service_refused(format!(
                "{word} deploy {id}: it was rolled back"
            )));
        }
        if removing && open.provisional(now) {
            return Err(service_refused(format!(
                "remove deploy {id}: it is still provisional; the token that opened it rolls it back with deploy_rollback, or confirms it first"
            )));
        }
        if open.confirmed && !removing {
            if self.verb == "deploy_confirm" {
                return Ok(json!({ "confirmed": true, "deploy": id }));
            }
            return Err(service_refused(format!(
                "roll back deploy {id}: it was confirmed, so its services are ordinary now and a removal needs the owner's approval"
            )));
        }
        if !removing && !open.provisional(now) {
            return Err(service_refused(format!(
                "{word} deploy {id}: its 15 minutes passed at {}, so its services are ordinary now{}",
                iso_utc(open.created.saturating_add(PROVISIONAL_MS)),
                if self.verb == "deploy_rollback" {
                    " and a removal needs the owner's approval"
                } else {
                    ""
                }
            )));
        }
        let row = |kind: &str, extra: Value| {
            let mut row = json!({ "kind": kind, "tenant": scope.id, "deploy": id,
                "token_id": caller.token_id, "at_ms": now });
            if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
                row.extend(extra.clone());
            }
            row
        };
        if self.verb == "deploy_confirm" {
            journal_deploy(&self.cfg, row("confirm", json!({})))?;
            // Its services are ordinary from here: the front reaches them. This call holds the write gate.
            sync_front_held(&self.kernel, &self.cfg, now)?;
            return Ok(json!({ "confirmed": true, "deploy": id }));
        }
        // The rollback removes exactly the entries this deploy created, in the one order: each instance is deleted and
        // read back absent before its ACL, then its secrets.
        let own: Vec<Entry> = entries
            .iter()
            .filter(|entry| {
                record_project(entry) == Some(scope.project.as_str())
                    && entry_deploy(entry) == Some(id)
            })
            .cloned()
            .collect();
        if removing && own.is_empty() {
            return Err(service_refused(format!(
                "remove deploy {id}: nothing of it is left in tenant {}",
                scope.id
            )));
        }
        let mut removed = Vec::new();
        let mut left = Vec::new();
        for entry in &own {
            let name = entry.config["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let mut kernel = lock(&self.kernel)?;
            let done = if entry.plugin.0 == "arca.service" {
                // A service's removal also revokes its links among the tenant's services.
                let spent = kernel.spend_service_removal_authorized(&entry.id);
                kernel
                    .remove_service_spent(spent, now)
                    .ok()
                    .is_some_and(|removal| {
                        let changed: Vec<&str> = removal
                            .changed
                            .iter()
                            .map(|entry| entry.0.as_str())
                            .collect();
                        !service_wiring::failed_for(&removal.outcomes, &changed)
                            && removal
                                .run
                                .as_ref()
                                .is_ok_and(|run| run.as_ref().is_none_or(|run| !run.partial))
                    })
            } else {
                let proposed = kernel.propose(
                    vec![crate::desired_store::EntryChange::Remove(entry.id.clone())],
                    now,
                );
                let plan = kernel
                    .pending_plans()
                    .into_iter()
                    .find(|plan| plan.entry == entry.id);
                proposed.is_ok_and(|outcomes| {
                    !service_wiring::failed_for(&outcomes, &[entry.id.0.as_str()])
                }) && plan.is_none_or(|plan| {
                    kernel
                        .run_plan_authorized(&plan.digest, now)
                        .is_ok_and(|run| !run.partial)
                })
            };
            drop(kernel);
            if done {
                let _ =
                    std::fs::remove_dir_all(self.cfg.state_dir.join("secrets").join(&entry.id.0));
                removed.push(name);
            } else {
                left.push(name);
            }
        }
        journal_deploy(
            &self.cfg,
            row(
                if removing { "remove" } else { "rollback" },
                json!({ "ok": left.is_empty(), "removed": removed, "left": left }),
            ),
        )?;
        if removing {
            if !left.is_empty() {
                return Err(service_refused(format!(
                    "remove deploy {id}: partial removal (removed: {}; not removed: {}); the owner's approval is spent, so another try needs a new one",
                    if removed.is_empty() {
                        "nothing".to_string()
                    } else {
                        removed.join(", ")
                    },
                    left.join(", ")
                )));
            }
            return Ok(json!({ "removed": true, "deploy": id, "parts": removed }));
        }
        if !left.is_empty() {
            return Err(service_refused(format!(
                "roll back deploy {id}: partial rollback (removed: {}; not removed: {}); it may be tried again while the deploy is provisional",
                if removed.is_empty() {
                    "nothing".to_string()
                } else {
                    removed.join(", ")
                },
                left.join(", ")
            )));
        }
        Ok(json!({ "rolledBack": true, "deploy": id, "removed": removed }))
    }

    fn create(
        &self,
        args: &Value,
        entries: &[Entry],
        read: &UnixIncus,
        now: u64,
    ) -> Result<Value, CallError> {
        let (id, caps) = create_args(&self.cfg, entries, args)?;
        if let Some(refusal) = create_rate_refusal(&self.cfg, now) {
            return Err(refusal);
        }
        let taken: Vec<String> = tenants(entries)
            .into_iter()
            .map(|(_, config)| config.id)
            .collect();
        let id = match id {
            Some(id) => id,
            None => fresh_id(&taken)?,
        };
        let (cidr, _) = network(&self.cfg)?;
        let project = tenant_project(&id, self.cfg.kernel.scratch);
        // A project of that name that this tenant's entry did not make is left alone.
        match read.get(&format!("/1.0/projects/{project}")) {
            Err(arca_incus::IncusError::Status { code: 404, .. }) => {}
            Ok(_) => {
                return Err(service_refused(format!(
                    "the Incus project {project} already exists and Arca did not create it for this tenant; it is left alone"
                )));
            }
            Err(_) => return Err(internal()),
        }
        // The block: the highest one of the tenant's size that overlaps no tenant's and holds no address in use.
        let rows = service_listing(read, None)?;
        let leases = service_leases(read)?;
        let in_use = service_used(&rows, &leases, entries);
        let held = held_blocks(&self.cfg, entries);
        let prefix = block::block_prefix(&caps);
        let block = block::blocks(&cidr, prefix)
            .map_err(|_| internal())?
            .into_iter()
            .find(|candidate| {
                // The subnet's top block lacks the broadcast address: a tenant that may fill a whole block skips it.
                candidate.addresses().len() >= caps.instances as usize
                    && !held.iter().any(|other| other.overlaps(*candidate))
                    && !in_use.iter().any(|ip| candidate.holds(*ip))
            })
            .ok_or_else(|| {
                // No amount is told: what ran out is whole blocks, and the caps count addresses.
                CallError::capacity(
                    format!(
                        "the service subnet {cidr} has no free /{prefix} block for a tenant: every block is held by a tenant or holds an address in use"
                    ),
                    Room {
                        scope: Whose::Node,
                        limit: Limit::Addresses,
                        have: None,
                        want: None,
                        max: None,
                    },
                )
            })?;
        let pool = service_pool(&self.cfg)
            .map_err(|_| service_refused("the default profile does not name a storage pool"))?;
        // A node that runs services of its own makes the managed bridge first. A scratch run never does: it joins the
        // bridge the node already has.
        let mut changes = if self.cfg.kernel.scratch {
            Vec::new()
        } else {
            ensure_netproject(entries, &self.cfg).map_err(no_service_project)?
        };
        let entry_id = tenant_entry(&id);
        let config = TenantConfig {
            id: id.clone(),
            project: project.clone(),
            bridge: arca_netproject::BRIDGE.into(),
            cidr,
            block: block.text(),
            pool,
            caps,
            created: now,
        };
        changes.push(crate::desired_store::EntryChange::Put(service_entry(
            &entry_id,
            "arca.tenant",
            serde_json::to_value(&config).map_err(|_| internal())?,
        )));
        let outcomes = lock(&self.kernel)?
            .propose(changes, now)
            .map_err(|_| internal())?;
        if service_wiring::failed_for(&outcomes, &[entry_id.as_str()]) {
            let why = service_wiring::failure_reason(&outcomes, &[entry_id.as_str()])
                .unwrap_or_else(|| "the kernel recorded no reason".to_string());
            // A create that made no project leaves no record. One that made it keeps its record, so tenant_remove removes it.
            let made = read.get(&format!("/1.0/projects/{project}")).is_ok();
            if !made {
                let _ = lock(&self.kernel)?.propose(
                    vec![crate::desired_store::EntryChange::Remove(EntryId(entry_id))],
                    now,
                );
                return Err(service_refused(format!(
                    "create tenant {id}: {why}; nothing was left behind"
                )));
            }
            return Err(service_refused(format!(
                "create tenant {id}: {why}. Its project exists: tenant_remove removes it"
            )));
        }
        let entries = lock(&self.kernel)?.entries().to_vec();
        let scope = scope_of(&self.cfg, &id, &entries)?;
        Ok(status_json(&scope, &entries, Some(0)))
    }

    /// Removes a tenant in the one order Incus accepts: each instance is deleted and read back absent before its NIC
    /// ACL, then its secrets go, and only when nothing of the tenant is left do its profile and project go.
    fn remove(
        &self,
        args: &Value,
        entries: &[Entry],
        read: &UnixIncus,
        now: u64,
    ) -> Result<Value, CallError> {
        let scope = scope_of(&self.cfg, id_arg(args)?, entries)?;
        let inside: Vec<EntryId> = entries
            .iter()
            .filter(|entry| record_project(entry) == Some(scope.project.as_str()))
            .map(|entry| entry.id.clone())
            .collect();
        let partial = |what: String| {
            service_refused(format!(
                "remove tenant {}: partial removal ({what}); the owner's approval is spent, so another try needs a new one",
                scope.id
            ))
        };
        let left = |read: &UnixIncus| -> Result<Vec<String>, CallError> {
            Ok(tenant_rows(read, &scope)?
                .iter()
                .filter_map(|row| row["name"].as_str().map(str::to_string))
                .collect())
        };
        let entry = EntryId(tenant_entry(&scope.id));
        let mut kernel = lock(&self.kernel)?;
        let mut failed = false;
        if !inside.is_empty() {
            let outcomes = kernel
                .propose(
                    inside
                        .iter()
                        .cloned()
                        .map(crate::desired_store::EntryChange::Remove)
                        .collect(),
                    now,
                )
                .map_err(|_| internal())?;
            let ids: Vec<&str> = inside.iter().map(|id| id.0.as_str()).collect();
            failed = service_wiring::failed_for(&outcomes, &ids);
        }
        // Every plan that deletes something in the tenant's project runs under this one approval: the plans of the
        // entries removed just now, and those an earlier removal left waiting when it was cut short.
        for plan in kernel.pending_plans() {
            let ours = inside.contains(&plan.entry)
                || (plan.entry != entry
                    && plan
                        .objects
                        .iter()
                        .any(|object| object.project == scope.project));
            if !ours {
                continue;
            }
            match kernel.run_plan_authorized(&plan.digest, now) {
                Ok(run) if !run.partial => {}
                _ => failed = true,
            }
        }
        // What is said is what was read: the instances the project still holds, by name.
        let names = left(read)?;
        if failed || !names.is_empty() {
            return Err(partial(if names.is_empty() {
                "every instance is removed; a firewall rule set of the tenant remains, and its project stands".to_string()
            } else {
                format!("instances remain: {}; its project stands", names.join(", "))
            }));
        }
        for id in &inside {
            let _ = std::fs::remove_dir_all(self.cfg.state_dir.join("secrets").join(&id.0));
        }
        let outcomes = kernel
            .propose(
                vec![crate::desired_store::EntryChange::Remove(entry.clone())],
                now,
            )
            .map_err(|_| internal())?;
        let mut failed = service_wiring::failed_for(&outcomes, &[entry.0.as_str()]);
        if let Some(plan) = kernel
            .pending_plans()
            .into_iter()
            .find(|plan| plan.entry == entry)
        {
            match kernel.run_plan_authorized(&plan.digest, now) {
                Ok(run) if !run.partial => {}
                _ => failed = true,
            }
        }
        drop(kernel);
        let gone = matches!(
            read.get(&format!("/1.0/projects/{}", scope.project)),
            Err(arca_incus::IncusError::Status { code: 404, .. })
        );
        if failed || !gone {
            let names = left(read).unwrap_or_default();
            return Err(partial(if gone {
                "its project is removed".to_string()
            } else if names.is_empty() {
                "every instance is removed; its project remains".to_string()
            } else {
                format!(
                    "instances remain: {}; its project remains",
                    names.join(", ")
                )
            }));
        }
        Ok(json!({ "removed": true }))
    }
}
