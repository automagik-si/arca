//! The GPU inventory, telemetry and attribution, end to end through the sampler. Every card is keyed by its PCI address:
//! two identical RTX 4060 Ti cards never merge into one row. The Incus is the fake and the `nvidia-smi` is a fake or a
//! missing program, so no real host is read.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arca_incus::{FakeIncus, FakeState, UnixIncus};
use arca_metrics::gpu::{
    GpuOwner, GpuTelemetry, InstanceKey, NOT_MEASURABLE, NVIDIA_MISSING, NvidiaProcess,
    NvidiaReading, NvidiaSmi, normalize_pci, parse_processes, parse_readings, read_cgroup,
    read_inventory,
};
use arca_metrics::{FakeNvidia, Nvidia, Sampler, SamplerConfig, mean};
use serde_json::{Value, json};

const FIRST: &str = "0000:01:00.0";
const SECOND: &str = "0000:02:00.0";
const DOCKER: &str =
    "docker-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.scope";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-gpu-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("the scratch directory");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    fs::write(path, text).expect("a fixture file");
}

fn card(pci: &str, uuid: &str) -> Value {
    json!({
        "pci_address": pci,
        "vendor": "NVIDIA Corporation",
        "product": "NVIDIA GeForce RTX 4060 Ti",
        "driver": "nvidia",
        "driver_version": "580.65.06",
        "numa_node": 0,
        "nvidia": {
            "uuid": uuid,
            "model": "NVIDIA GeForce RTX 4060 Ti",
            "memory": { "total": 17_179_869_184u64 },
        },
    })
}

fn resources(cards: Vec<Value>) -> Value {
    json!({
        "cpu": { "total": 16 },
        "memory": { "total": 34_359_738_368u64, "used": 8_589_934_592u64 },
        "gpu": { "total": cards.len(), "cards": cards },
    })
}

fn fake_state() -> FakeState {
    FakeState {
        server: json!({ "api_version": "1.0" }),
        projects: vec![json!({ "name": "default" }), json!({ "name": "svc" })],
        instances: vec![
            json!({ "name": "alpha", "project": "default", "type": "container", "status": "Running" }),
            json!({ "name": "beta", "project": "svc", "type": "container", "status": "Running" }),
        ],
        pools: Vec::new(),
        networks: Vec::new(),
    }
}

fn sampler(root: &Path, socket: &Path, local: bool, nvidia: FakeNvidia) -> Sampler<UnixIncus> {
    Sampler::new(
        SamplerConfig {
            proc_root: root.join("proc"),
            sys_root: root.join("sys"),
            pools: Vec::new(),
            interval_ms: 5_000,
            history_minutes: 60,
            incus_local: local,
            mode_retry: None,
        },
        UnixIncus::new(socket.to_path_buf()),
    )
    .with_nvidia(Arc::new(nvidia))
}

fn readings() -> Vec<NvidiaReading> {
    vec![
        NvidiaReading {
            pci: FIRST.to_string(),
            uuid: Some("GPU-11111111-1111-1111-1111-111111111111".to_string()),
            telemetry: GpuTelemetry {
                utilization_pct: Some(10.0),
                memory_used_bytes: Some(1 << 30),
                memory_total_bytes: Some(17_179_869_184),
                power_draw_w: Some(40.0),
                power_limit_w: Some(115.0),
                temperature_c: Some(50.0),
                sm_clock_mhz: Some(1800),
                event_reasons: Some("0x0".to_string()),
            },
        },
        NvidiaReading {
            pci: SECOND.to_string(),
            uuid: Some("GPU-22222222-2222-2222-2222-222222222222".to_string()),
            telemetry: GpuTelemetry {
                utilization_pct: Some(20.0),
                memory_used_bytes: Some(16 << 30),
                memory_total_bytes: Some(17_179_869_184),
                power_draw_w: Some(90.0),
                power_limit_w: Some(115.0),
                temperature_c: Some(70.0),
                sm_clock_mhz: Some(2400),
                event_reasons: Some("0x4".to_string()),
            },
        },
    ]
}

fn processes() -> Vec<NvidiaProcess> {
    vec![
        NvidiaProcess {
            pci: FIRST.to_string(),
            pid: 1000,
            vram_bytes: Some(1 << 30),
        },
        NvidiaProcess {
            pci: SECOND.to_string(),
            pid: 1001,
            vram_bytes: Some(2 << 30),
        },
        NvidiaProcess {
            pci: SECOND.to_string(),
            pid: 1002,
            vram_bytes: Some(3 << 30),
        },
        NvidiaProcess {
            pci: FIRST.to_string(),
            pid: 1003,
            vram_bytes: Some(4 << 30),
        },
        NvidiaProcess {
            pci: FIRST.to_string(),
            pid: 1004,
            vram_bytes: Some(5 << 30),
        },
        // A cgroup v1 line: the path is the third colon-separated field too.
        NvidiaProcess {
            pci: FIRST.to_string(),
            pid: 1005,
            vram_bytes: Some(6 << 30),
        },
    ]
}

/// The cgroup of each PID: an Incus instance in the default project, one in another project, a Docker container, a host
/// process and a process whose cgroup cannot be read. PID 1005 uses a v1 line.
fn write_cgroups(root: &Path) {
    write(root, "proc/1000/cgroup", "0::/lxc.payload.alpha\n");
    write(root, "proc/1001/cgroup", "0::/lxc.payload.svc_beta\n");
    write(
        root,
        "proc/1002/cgroup",
        &format!("0::/system.slice/{DOCKER}\n"),
    );
    write(
        root,
        "proc/1003/cgroup",
        "0::/user.slice/user-1000.slice/user@1000.service\n",
    );
    write(root, "proc/1005/cgroup", "12:devices:/lxc.payload.alpha\n");
}

#[test]
fn two_identical_cards_are_two_rows_keyed_by_pci() {
    let scratch = Scratch::new("two-cards");
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("the fake serves");
    fake.set_resources(resources(vec![
        card(FIRST, "GPU-11111111-1111-1111-1111-111111111111"),
        card(SECOND, "GPU-22222222-2222-2222-2222-222222222222"),
    ]));
    write_cgroups(scratch.path());
    let nvidia = FakeNvidia::with_readings(readings());
    nvidia.set_processes(processes());
    let mut sampler = sampler(scratch.path(), fake.socket(), true, nvidia.clone());

    let sample = sampler.sample(1_000).clone();
    assert_eq!(sample.gpu.reason, None, "the inventory was read");
    assert_eq!(sample.gpu.cards.len(), 2, "two cards, not one merged card");
    let first = &sample.gpu.cards[0];
    let second = &sample.gpu.cards[1];
    assert_eq!(first.pci, FIRST);
    assert_eq!(second.pci, SECOND);
    assert_eq!(
        first.product, second.product,
        "the cards are the same model; only the address tells them apart"
    );
    assert_ne!(first.uuid, second.uuid, "each card keeps its own UUID");
    assert_eq!(first.vram_total_bytes, Some(17_179_869_184));
    assert_eq!(second.vram_total_bytes, Some(17_179_869_184));
    assert_eq!(first.usage, arca_metrics::GpuUse::DriverBound);
    assert_eq!(
        first.telemetry.as_ref().and_then(|t| t.utilization_pct),
        Some(10.0)
    );
    assert_eq!(
        second.telemetry.as_ref().and_then(|t| t.utilization_pct),
        Some(20.0)
    );

    assert_eq!(first.processes.len(), 4, "PIDs 1000, 1003, 1004 and 1005");
    assert_eq!(second.processes.len(), 2, "PIDs 1001 and 1002");
    assert_eq!(
        first.processes[0].owner,
        GpuOwner::Instance {
            project: "default".to_string(),
            name: "alpha".to_string(),
            container: None,
        }
    );
    assert_eq!(
        first.processes[1].owner,
        GpuOwner::Host,
        "a host process is on the host"
    );
    assert_eq!(
        first.processes[2].owner,
        GpuOwner::Host,
        "a PID with no readable cgroup is on the host"
    );
    assert_eq!(
        first.processes[3].owner,
        GpuOwner::Instance {
            project: "default".to_string(),
            name: "alpha".to_string(),
            container: None,
        },
        "a cgroup v1 line resolves the same instance"
    );
    assert_eq!(
        second.processes[0].owner,
        GpuOwner::Instance {
            project: "svc".to_string(),
            name: "beta".to_string(),
            container: None,
        },
        "the project is part of the instance's cgroup name"
    );
    assert_eq!(
        second.processes[1].owner,
        GpuOwner::Container {
            name: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string()
        }
    );

    // A second sample: the mean keeps the two cards apart and averages each card's own readings.
    nvidia.set_readings(vec![
        NvidiaReading {
            pci: FIRST.to_string(),
            uuid: None,
            telemetry: GpuTelemetry {
                utilization_pct: Some(30.0),
                memory_used_bytes: Some(3 << 30),
                memory_total_bytes: Some(17_179_869_184),
                ..GpuTelemetry::default()
            },
        },
        NvidiaReading {
            pci: SECOND.to_string(),
            uuid: None,
            telemetry: GpuTelemetry {
                utilization_pct: Some(40.0),
                memory_used_bytes: Some(15 << 30),
                memory_total_bytes: Some(17_179_869_184),
                ..GpuTelemetry::default()
            },
        },
    ]);
    sampler.sample(6_000);
    let samples = sampler.history(1);
    let averaged = mean(&samples).expect("a mean");
    assert_eq!(averaged.gpu.cards.len(), 2);
    assert_eq!(averaged.gpu.cards[0].pci, FIRST);
    assert_eq!(averaged.gpu.cards[1].pci, SECOND);
    let first_mean = averaged.gpu.cards[0]
        .telemetry
        .as_ref()
        .and_then(|telemetry| telemetry.utilization_pct);
    let second_mean = averaged.gpu.cards[1]
        .telemetry
        .as_ref()
        .and_then(|telemetry| telemetry.utilization_pct);
    assert_eq!(first_mean, Some(20.0));
    assert_eq!(second_mean, Some(30.0));
}

#[test]
fn a_remote_host_reads_the_inventory_and_leaves_telemetry_absent_with_a_reason() {
    let scratch = Scratch::new("remote");
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("the fake serves");
    fake.set_resources(resources(vec![card(
        FIRST,
        "GPU-11111111-1111-1111-1111-111111111111",
    )]));
    let nvidia = FakeNvidia::with_readings(readings());
    let mut sampler = sampler(scratch.path(), fake.socket(), false, nvidia);

    let sample = sampler.sample(1_000).clone();
    assert_eq!(sample.gpu.cards.len(), 1);
    assert!(sample.gpu.cards[0].telemetry.is_none());
    assert_eq!(
        sample.gpu.cards[0].reason.as_deref(),
        Some(NOT_MEASURABLE),
        "a local nvidia-smi would describe the wrong machine"
    );
    assert_eq!(
        sample.gpu.cards[0].process_reason.as_deref(),
        Some(NOT_MEASURABLE)
    );
    assert!(
        sampler
            .readings()
            .instances
            .as_ref()
            .is_some_and(|instances| instances.len() == 2),
        "the inventory and instances are read from a remote host too"
    );
}

#[test]
fn a_missing_nvidia_smi_is_a_plain_reason_never_a_zero() {
    let scratch = Scratch::new("missing");
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("the fake serves");
    fake.set_resources(resources(vec![card(
        FIRST,
        "GPU-11111111-1111-1111-1111-111111111111",
    )]));
    let mut sampler = Sampler::new(
        SamplerConfig {
            proc_root: scratch.path().join("proc"),
            sys_root: scratch.path().join("sys"),
            pools: Vec::new(),
            interval_ms: 5_000,
            history_minutes: 60,
            incus_local: true,
            mode_retry: None,
        },
        UnixIncus::new(fake.socket().to_path_buf()),
    )
    .with_nvidia(Arc::new(NvidiaSmi::at(
        scratch.path().join("no-such-nvidia-smi"),
    )));

    let sample = sampler.sample(1_000).clone();
    let card = &sample.gpu.cards[0];
    assert!(card.telemetry.is_none(), "no zero reading is invented");
    assert_eq!(card.reason.as_deref(), Some(NVIDIA_MISSING));
    assert_eq!(card.process_reason.as_deref(), Some(NVIDIA_MISSING));
    assert!(card.processes.is_empty());
}

#[test]
fn a_card_that_vanishes_is_kept_as_gone_until_it_returns() {
    let scratch = Scratch::new("vanish");
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("the fake serves");
    fake.set_resources(resources(vec![
        card(FIRST, "GPU-11111111-1111-1111-1111-111111111111"),
        card(SECOND, "GPU-22222222-2222-2222-2222-222222222222"),
    ]));
    let nvidia = FakeNvidia::with_readings(readings());
    let mut sampler = sampler(scratch.path(), fake.socket(), true, nvidia);
    sampler.sample(1_000);
    assert!(sampler.readings().gpus_gone.is_empty());

    fake.set_resources(resources(vec![card(
        FIRST,
        "GPU-11111111-1111-1111-1111-111111111111",
    )]));
    let sample = sampler.sample(6_000).clone();
    assert_eq!(sample.gpu.cards.len(), 1);
    let gone: Vec<&str> = sampler
        .readings()
        .gpus_gone
        .iter()
        .map(|card| card.pci.as_str())
        .collect();
    assert_eq!(gone, vec![SECOND]);

    fake.set_resources(resources(vec![
        card(FIRST, "GPU-11111111-1111-1111-1111-111111111111"),
        card(SECOND, "GPU-22222222-2222-2222-2222-222222222222"),
    ]));
    sampler.sample(11_000);
    assert!(
        sampler.readings().gpus_gone.is_empty(),
        "the card returned, so the disappearance clears"
    );
}

#[test]
fn the_incus_gpu_cards_document_is_parsed_per_card() {
    let document = resources(vec![
        card(FIRST, "GPU-11111111-1111-1111-1111-111111111111"),
        card(SECOND, "GPU-22222222-2222-2222-2222-222222222222"),
    ]);
    let cards = arca_metrics::gpu::cards_of(&document)
        .expect("the cards parse")
        .cards;
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].pci, FIRST);
    assert_eq!(cards[1].pci, SECOND);
    assert_eq!(cards[0].driver.as_deref(), Some("nvidia"));
    assert_eq!(cards[0].driver_version.as_deref(), Some("580.65.06"));
    assert_eq!(cards[0].numa_node, 0);
    assert_eq!(cards[0].usage, arca_metrics::GpuUse::DriverBound);
}

#[test]
fn a_vfio_card_has_its_own_use_and_a_missing_gpu_list_is_empty_not_unknown() {
    let mut bound = card(SECOND, "GPU-22222222-2222-2222-2222-222222222222");
    bound["driver"] = json!("vfio-pci");
    bound["nvidia"] = json!({});
    let cards = arca_metrics::gpu::cards_of(&resources(vec![bound]))
        .expect("the cards parse")
        .cards;
    assert_eq!(cards[0].usage, arca_metrics::GpuUse::VfioPci);
    assert_eq!(cards[0].uuid, None);

    let no_gpu = json!({ "cpu": { "total": 4 }, "memory": { "total": 1u64, "used": 0u64 } });
    assert_eq!(
        arca_metrics::gpu::cards_of(&no_gpu),
        None,
        "the bare document has no cards list"
    );
    struct NoGpu(Value);
    impl arca_incus::IncusRead for NoGpu {
        fn get(&self, _path: &str) -> Result<Value, arca_incus::IncusError> {
            Ok(self.0.clone())
        }
    }
    assert_eq!(
        read_inventory(&NoGpu(no_gpu)),
        Some(arca_metrics::gpu::GpuInventory::default()),
        "a resources document without a GPU block is a known-empty inventory"
    );
    let malformed = json!({ "gpu": { "cards": "not a list" } });
    assert_eq!(
        arca_metrics::gpu::cards_of(&malformed),
        None,
        "a malformed list is unknown, never empty"
    );
}

#[test]
fn the_query_parser_normalizes_the_domain_and_keeps_unavailable_fields_none() {
    let text = "00000000:01:00.0, GPU-11111111-1111-1111-1111-111111111111, 10, 1024, 16376, 40.5, 115.0, 50, 1800, 0x0\n";
    let readings = parse_readings(text).expect("the CSV parses");
    assert_eq!(readings[0].pci, "0000:01:00.0");
    assert_eq!(readings[0].telemetry.memory_used_bytes, Some(1_073_741_824));
    let unavailable =
        "00000000:02:00.0, [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A]\n";
    let readings = parse_readings(unavailable).expect("the CSV parses");
    assert_eq!(readings[0].uuid, None);
    assert_eq!(readings[0].telemetry.utilization_pct, None);
    assert_eq!(readings[0].telemetry.memory_used_bytes, None);
    assert_eq!(
        parse_processes("00000000:01:00.0, 4242, 512\n").expect("the apps CSV parses")[0].pid,
        4242
    );
    assert_eq!(normalize_pci("00000000:0A:00.0"), "0000:0a:00.0");
}

#[test]
fn cgroups_resolve_incus_projects_and_docker_containers() {
    let scratch = Scratch::new("cgroups");
    write_cgroups(scratch.path());
    assert_eq!(
        read_cgroup(&scratch.path().join("proc"), 1000).as_deref(),
        Some("/lxc.payload.alpha")
    );
    let instances = vec![
        InstanceKey {
            project: "default".to_string(),
            name: "alpha".to_string(),
        },
        InstanceKey {
            project: "svc".to_string(),
            name: "beta".to_string(),
        },
    ];
    assert_eq!(
        arca_metrics::gpu::incus_instance("/lxc.payload.alpha", &instances),
        Some(("default".to_string(), "alpha".to_string()))
    );
    assert_eq!(
        arca_metrics::gpu::incus_instance("/lxc.payload.svc_beta", &instances),
        Some(("svc".to_string(), "beta".to_string()))
    );
    assert_eq!(
        arca_metrics::gpu::incus_instance("/lxc.payload.proj_alpha", &[]),
        Some(("proj".to_string(), "alpha".to_string())),
        "without the instance list the project/name split is still resolved"
    );
    assert_eq!(
        arca_metrics::gpu::incus_instance("/lxc.payload.solo", &[]),
        Some(("default".to_string(), "solo".to_string()))
    );
    assert_eq!(
        arca_metrics::gpu::docker_container(&("/system.slice/".to_string() + DOCKER)),
        Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string())
    );
}

#[test]
fn a_failed_resources_read_is_a_reason_not_an_empty_inventory() {
    struct Offline;
    impl arca_incus::IncusRead for Offline {
        fn get(&self, _path: &str) -> Result<Value, arca_incus::IncusError> {
            Err(arca_incus::IncusError::Status {
                code: 503,
                message: "unavailable".to_string(),
            })
        }
    }
    assert_eq!(read_inventory(&Offline), None);
}

#[test]
fn one_card_without_a_pci_address_is_skipped_with_a_reason() {
    let mut without = card(SECOND, "GPU-22222222-2222-2222-2222-222222222222");
    without
        .as_object_mut()
        .expect("a card object")
        .remove("pci_address");
    let inventory = arca_metrics::gpu::cards_of(&resources(vec![
        card(FIRST, "GPU-11111111-1111-1111-1111-111111111111"),
        without,
    ]))
    .expect("the list is read");
    assert_eq!(inventory.cards.len(), 1, "the valid card still lists");
    assert_eq!(inventory.cards[0].pci, FIRST);
    assert!(
        inventory
            .skipped
            .as_deref()
            .is_some_and(|reason| reason.contains("1 card without a readable PCI address")),
        "{:?}",
        inventory.skipped
    );
}

#[test]
fn a_hanging_nvidia_smi_is_killed_at_its_deadline() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let scratch = Scratch::new("hang");
    let script = scratch.path().join("hang.sh");
    fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("the script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("mode 0755");
    let reader = NvidiaSmi::at(script).with_timeout(Duration::from_millis(150));

    let started = Instant::now();
    let error = reader.readings().expect_err("the hang times out");
    assert_eq!(error, arca_metrics::gpu::NvidiaError::TimedOut);
    assert_eq!(error.to_string(), "the nvidia-smi read timed out");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the child was killed at the deadline"
    );
}

#[test]
fn an_oversize_nvidia_smi_answer_is_refused_at_the_cap() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let scratch = Scratch::new("cap");
    let script = scratch.path().join("large.sh");
    // One byte over the 1 MiB cap, without needing a big fixture file.
    fs::write(
        &script,
        "#!/bin/sh\nhead -c 1048577 /dev/zero | tr '\\0' x\n",
    )
    .expect("the script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("mode 0755");
    let reader = NvidiaSmi::at(script).with_timeout(Duration::from_secs(5));
    let error = reader.readings().expect_err("the answer is too large");
    assert_eq!(
        error,
        arca_metrics::gpu::NvidiaError::Failed("the answer is over the 1 MiB cap".to_string())
    );
}

#[test]
fn attribution_checks_the_first_lxc_segment_and_strict_docker_ids() {
    let scratch = Scratch::new("spoof");
    let docker = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let instances = vec![
        InstanceKey {
            project: "default".to_string(),
            name: "alpha".to_string(),
        },
        InstanceKey {
            project: "svc".to_string(),
            name: "beta".to_string(),
        },
    ];
    let cgroups = [
        (
            1010,
            format!("0::/lxc.payload.alpha/docker-{docker}.scope\n"),
        ),
        (
            1011,
            "0::/lxc.payload.alpha/docker-not-a-real-id.scope\n".to_string(),
        ),
        (
            1012,
            "0::/user.slice/lxc.payload.alpha\n".to_string(),
        ),
        (
            1013,
            "12:devices:/lxc.payload.svc_beta\n".to_string(),
        ),
        (
            1014,
            format!("0::/system.slice/docker-{docker}.scope\n"),
        ),
        (
            1015,
            "0::/lxc.payload.unknown/docker-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.scope\n".to_string(),
        ),
    ];
    for (pid, text) in &cgroups {
        write(scratch.path(), &format!("proc/{pid}/cgroup"), text);
    }
    let processes: Vec<NvidiaProcess> = cgroups
        .iter()
        .map(|(pid, _)| NvidiaProcess {
            pci: FIRST.to_string(),
            pid: *pid,
            vram_bytes: Some(1 << 20),
        })
        .collect();
    let owners = arca_metrics::gpu::attribute(&scratch.path().join("proc"), &processes, &instances);
    assert_eq!(
        owners[0].owner,
        GpuOwner::Instance {
            project: "default".to_string(),
            name: "alpha".to_string(),
            container: Some(docker.to_string()),
        },
        "Docker inside an Incus instance belongs to the instance, with the id as detail"
    );
    assert_eq!(
        owners[1].owner,
        GpuOwner::Instance {
            project: "default".to_string(),
            name: "alpha".to_string(),
            container: None,
        },
        "a faked docker-<text>.scope is ignored"
    );
    assert_eq!(
        owners[2].owner,
        GpuOwner::Host,
        "lxc.payload. is only recognized as the first segment"
    );
    assert_eq!(
        owners[3].owner,
        GpuOwner::Instance {
            project: "svc".to_string(),
            name: "beta".to_string(),
            container: None,
        },
        "the v1 devices line resolves the instance"
    );
    assert_eq!(
        owners[4].owner,
        GpuOwner::Container {
            name: docker.to_string()
        }
    );
    assert_eq!(
        owners[5].owner,
        GpuOwner::Instance {
            project: "default".to_string(),
            name: "unknown".to_string(),
            container: Some(docker.to_string()),
        },
        "a first-segment lxc.payload. is an instance; its Docker scope is extra detail"
    );
    assert_eq!(
        arca_metrics::gpu::docker_container(&format!("/system.slice/docker-{docker}.scope")),
        Some(docker.to_string())
    );
    assert_eq!(
        arca_metrics::gpu::docker_container("/system.slice/docker-abc.scope"),
        None
    );
    assert_eq!(
        arca_metrics::gpu::incus_instance("/user.slice/lxc.payload.alpha", &instances),
        None
    );
}
