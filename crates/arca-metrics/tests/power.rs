//! The CPU thread counts, the power block and each instance's CPU in threads, read through the sampler from a generated
//! procfs and sysfs tree and a scripted Incus. Nothing here reads the real host. The expected values are worked out by hand
//! in the comments beside them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_incus::{IncusError, IncusRead};
use arca_metrics::gpu::{GpuTelemetry, NvidiaReading};
use arca_metrics::{
    FakeNvidia, HostSample, INSTANCE_CPU_MAX, InstanceThreads, MonotonicClock, NO_CPU_COUNTER,
    NO_GPU_COUNTER, NOT_INCLUDED, REMOTE_HOST_REASON, Sampler, SamplerConfig, mean, power, usage,
};
use serde_json::{Value, json};

const LISTED: &str = "/1.0/instances?all-projects=true&recursion=1";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-power-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("the scratch directory");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        restore_permissions(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Makes every file under `dir` readable by its owner again, so a test that took read permission away still cleans up.
fn restore_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => restore_permissions(&entry.path()),
            Ok(kind) if kind.is_file() => {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o644));
            }
            _ => {}
        }
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    fs::write(path, text).expect("a fixture file");
}

/// Four numbered CPUs. The aggregate line totals 8000 jiffies, 1500 of them busy.
const STAT_FIRST: &str = "cpu  1000 0 500 6000 500 0 0 0 0 0\ncpu0 250 0 125 1500 125 0 0 0 0 0\ncpu1 250 0 125 1500 125 0 0 0 0 0\ncpu2 250 0 125 1500 125 0 0 0 0 0\ncpu3 250 0 125 1500 125 0 0 0 0 0\n";

/// Against the first reading the total rose by 1000 and busy by 250: 25 percent of four threads is one busy thread.
const STAT_SECOND: &str = "cpu  1150 0 600 6600 650 0 0 0 0 0\ncpu0 287 0 150 1650 162 0 0 0 0 0\ncpu1 287 0 150 1650 162 0 0 0 0 0\ncpu2 288 0 150 1650 163 0 0 0 0 0\ncpu3 288 0 150 1650 163 0 0 0 0 0\n";

/// A powercap zone with its name, its counter and, when given, its range.
fn zone(root: &Path, dir: &str, name: &str, energy_uj: u64, range_uj: Option<u64>) {
    write(
        root,
        &format!("sys/class/powercap/{dir}/name"),
        &format!("{name}\n"),
    );
    set_energy(root, dir, energy_uj);
    if let Some(range) = range_uj {
        write(
            root,
            &format!("sys/class/powercap/{dir}/max_energy_range_uj"),
            &format!("{range}\n"),
        );
    }
}

fn set_energy(root: &Path, dir: &str, energy_uj: u64) {
    write(
        root,
        &format!("sys/class/powercap/{dir}/energy_uj"),
        &format!("{energy_uj}\n"),
    );
}

/// The four CPUs as two physical cores of one package: cpu0 and cpu2 share a core, cpu1 and cpu3 the other.
fn topology(root: &Path) {
    for (cpu, siblings) in [(0, "0,2"), (1, "1,3"), (2, "0,2"), (3, "1,3")] {
        let base = format!("sys/devices/system/cpu/cpu{cpu}/topology");
        write(
            root,
            &format!("{base}/core_cpus_list"),
            &format!("{siblings}\n"),
        );
        write(root, &format!("{base}/physical_package_id"), "0\n");
    }
}

/// An Incus whose answers the test sets: the resources document, the instance rows and each instance's CPU time. Every
/// other path fails, as a plain Incus without that document does.
#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<Script>>);

#[derive(Default)]
struct Script {
    resources: Option<Value>,
    rows: Option<Vec<Value>>,
    cpu_ns: BTreeMap<String, u64>,
}

impl Scripted {
    fn resources(&self, document: Value) {
        self.0.lock().expect("the script").resources = Some(document);
    }

    fn rows(&self, rows: Vec<Value>) {
        self.0.lock().expect("the script").rows = Some(rows);
    }

    fn cpu(&self, name: &str, seconds: f64) {
        self.0
            .lock()
            .expect("the script")
            .cpu_ns
            .insert(name.to_string(), (seconds * 1e9).round() as u64);
    }
}

impl IncusRead for Scripted {
    fn get_within(&self, path: &str, _timeout: std::time::Duration) -> Result<Value, IncusError> {
        self.get(path)
    }

    fn get(&self, path: &str) -> Result<Value, IncusError> {
        let script = self.0.lock().expect("the script");
        let refuse = || Err(IncusError::Decode(format!("no answer for {path}")));
        if path == "/1.0/resources" {
            return script.resources.clone().map_or_else(refuse, Ok);
        }
        if path == LISTED {
            return script
                .rows
                .clone()
                .map_or_else(refuse, |rows| Ok(Value::Array(rows)));
        }
        if let Some(rest) = path.strip_prefix("/1.0/instances/")
            && let Some((name, _)) = rest.split_once("/state?")
        {
            return match script.cpu_ns.get(name) {
                Some(ns) => Ok(json!({ "cpu": { "usage": ns }, "memory": { "usage": 1024 } })),
                None => refuse(),
            };
        }
        refuse()
    }
}

/// A running container row in project `apps`, with its creation and last start times.
fn running(name: &str, created: &str, last_used: &str) -> Value {
    json!({
        "name": name, "project": "apps", "status": "Running", "type": "container",
        "created_at": created, "last_used_at": last_used,
    })
}

fn stopped(name: &str, created: &str, last_used: &str) -> Value {
    let mut row = running(name, created, last_used);
    row["status"] = json!("Stopped");
    row
}

/// A clock that never moves, so every state read answers at its sample's own time and a figure is exact.
struct Frozen;

impl MonotonicClock for Frozen {
    fn now_ms(&self) -> u64 {
        0
    }
}

fn sampler<R: IncusRead>(root: &Path, incus: R, local: bool) -> Sampler<R> {
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
        incus,
    )
    .with_nvidia(Arc::new(FakeNvidia::missing()))
    .with_clock(Arc::new(Frozen))
}

fn tree(tag: &str) -> Scratch {
    let scratch = Scratch::new(tag);
    write(scratch.path(), "proc/stat", STAT_FIRST);
    scratch
}

fn not_included(sample: &HostSample) -> Vec<&str> {
    sample
        .power
        .not_included
        .iter()
        .map(String::as_str)
        .collect()
}

mod cpu_counts {
    use super::*;

    #[test]
    fn threads_physical_cores_and_busy_threads_are_separate_and_cores_keeps_counting_threads() {
        let scratch = tree("threads");
        topology(scratch.path());
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);

        let first = sampler.sample(1_000).clone();
        assert_eq!(first.cpu.threads, Some(4), "four numbered CPU lines");
        assert_eq!(
            first.cpu.physical_cores,
            Some(2),
            "two distinct sibling lists"
        );
        assert_eq!(first.cpu.busy_threads, None, "no rate on the first sample");
        assert_eq!(first.cpu.cores, 4, "cores still counts the logical CPUs");

        write(scratch.path(), "proc/stat", STAT_SECOND);
        let second = sampler.sample(6_000).clone();
        assert_eq!(second.cpu.used_pct, Some(25.0));
        // 25 percent of four threads.
        assert_eq!(second.cpu.busy_threads, Some(1.0));
        assert_eq!(second.cpu.threads, Some(4));
        assert_eq!(second.cpu.physical_cores, Some(2));
    }

    #[test]
    fn a_host_without_topology_files_has_threads_and_no_physical_core_count() {
        let scratch = tree("no-topology");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        let sample = sampler.sample(1_000).clone();
        assert_eq!(sample.cpu.threads, Some(4));
        assert_eq!(sample.cpu.physical_cores, None);
    }

    #[test]
    fn a_missing_stat_file_has_no_thread_count() {
        let scratch = Scratch::new("no-stat");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        let sample = sampler.sample(1_000).clone();
        assert_eq!(sample.cpu.threads, None);
        assert_eq!(sample.cpu.busy_threads, None);
        assert_eq!(sample.cpu.cores, 0);
    }

    #[test]
    fn a_remote_host_counts_physical_cores_from_the_incus_sockets() {
        let scratch = Scratch::new("remote-cores");
        let incus = Scripted::default();
        incus.resources(json!({
            "cpu": {
                "total": 8,
                "sockets": [
                    { "cores": [ { "threads": [{}, {}] }, { "threads": [{}, {}] } ] },
                    { "cores": [ { "threads": [{}, {}] }, { "threads": [{}, {}] } ] },
                ],
            },
            "memory": { "total": 1_073_741_824u64, "used": 1024u64 },
        }));
        let mut sampler = sampler(scratch.path(), incus, false);
        let sample = sampler.sample(1_000).clone();
        assert_eq!(sample.cpu.cores, 8);
        assert_eq!(sample.cpu.threads, Some(8));
        assert_eq!(
            sample.cpu.physical_cores,
            Some(4),
            "two sockets of two cores"
        );
        assert_eq!(sample.cpu.busy_threads, None);
    }
}

mod cpu_power {
    use super::*;

    #[test]
    fn a_host_with_no_counter_says_so_and_the_sum_leaves_the_cpu_out() {
        let scratch = tree("no-counter");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        let sample = sampler.sample(6_000).clone();

        assert_eq!(sample.power.cpu.power_w, None);
        assert_eq!(sample.power.cpu.reason.as_deref(), Some(NO_CPU_COUNTER));
        assert!(sample.power.cpu.packages.is_empty());
        assert_eq!(sample.power.gpu.reason.as_deref(), Some(NO_GPU_COUNTER));
        assert_eq!(sample.power.measured_power_w, None, "nothing was measured");
        let words = not_included(&sample);
        for part in NOT_INCLUDED {
            assert!(words.contains(&part), "{part} is named: {words:?}");
        }
        assert!(words.contains(&"the CPU packages"), "{words:?}");
    }

    #[test]
    fn the_first_sample_has_no_rate_and_says_it_is_the_only_one() {
        let scratch = tree("first");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            1_000_000,
            Some(262_143_328_850),
        );
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        let first = sampler.sample(1_000).clone();

        assert_eq!(first.power.cpu.packages.len(), 1);
        let package = &first.power.cpu.packages[0];
        assert_eq!(package.zone, "intel-rapl:0");
        assert_eq!(package.name, "package-0");
        assert_eq!(package.power_w, None);
        assert_eq!(package.reason.as_deref(), Some(power::ONE_SAMPLE));
        assert_eq!(first.power.cpu.power_w, None);
        assert_eq!(first.power.cpu.reason.as_deref(), Some(power::ONE_SAMPLE));
    }

    #[test]
    fn two_packages_give_one_figure_each_and_their_sum() {
        let scratch = tree("two-packages");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            1_000_000,
            Some(262_143_328_850),
        );
        zone(
            scratch.path(),
            "intel-rapl:1",
            "package-1",
            1_000_000,
            Some(262_143_328_850),
        );
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        // 50 J and 25 J over five seconds: 10 W and 5 W.
        set_energy(scratch.path(), "intel-rapl:0", 51_000_000);
        set_energy(scratch.path(), "intel-rapl:1", 26_000_000);
        let sample = sampler.sample(6_000).clone();

        let figures: Vec<(&str, Option<f64>)> = sample
            .power
            .cpu
            .packages
            .iter()
            .map(|package| (package.name.as_str(), package.power_w))
            .collect();
        assert_eq!(
            figures,
            vec![("package-0", Some(10.0)), ("package-1", Some(5.0))]
        );
        assert_eq!(sample.power.cpu.power_w, Some(15.0));
        assert_eq!(sample.power.cpu.reason, None);
        assert_eq!(sample.power.measured_power_w, Some(15.0));
        let words = not_included(&sample);
        assert!(!words.contains(&"the CPU packages"), "{words:?}");
        assert!(words.contains(&"memory"), "{words:?}");
    }

    #[test]
    fn a_sub_domain_another_interface_and_the_memory_zone_are_not_added_to_the_package() {
        let scratch = tree("sub-domain");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            1_000_000,
            Some(262_143_328_850),
        );
        // The core sub-domain moves too, and is part of package-0 already.
        zone(
            scratch.path(),
            "intel-rapl:0:0",
            "core",
            1_000_000,
            Some(262_143_328_850),
        );
        // The MMIO interface reads the same package a second way.
        zone(
            scratch.path(),
            "intel-rapl-mmio:0",
            "package-0",
            1_000_000,
            Some(262_143_328_850),
        );
        // A memory zone is memory, which the sum never holds.
        zone(
            scratch.path(),
            "intel-rapl:1",
            "dram",
            1_000_000,
            Some(262_143_328_850),
        );
        // The control type directory has no counter of its own.
        fs::create_dir_all(scratch.path().join("sys/class/powercap/intel-rapl")).expect("a dir");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        set_energy(scratch.path(), "intel-rapl:0", 51_000_000);
        set_energy(scratch.path(), "intel-rapl:0:0", 41_000_000);
        set_energy(scratch.path(), "intel-rapl-mmio:0", 51_000_000);
        set_energy(scratch.path(), "intel-rapl:1", 11_000_000);
        let sample = sampler.sample(6_000).clone();

        let zones: Vec<&str> = sample
            .power
            .cpu
            .packages
            .iter()
            .map(|package| package.zone.as_str())
            .collect();
        assert_eq!(zones, vec!["intel-rapl:0"]);
        assert_eq!(sample.power.cpu.power_w, Some(10.0));
        assert_eq!(sample.power.measured_power_w, Some(10.0));
    }

    #[test]
    fn a_counter_that_wraps_reads_through_its_range() {
        let scratch = tree("wrap");
        let range = 262_143_328_850u64;
        // 10 J before the wrap and 40 J after it: 50 J over five seconds is 10 W.
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            range - 10_000_000,
            Some(range),
        );
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        set_energy(scratch.path(), "intel-rapl:0", 40_000_000);
        let sample = sampler.sample(6_000).clone();
        assert_eq!(sample.power.cpu.packages[0].power_w, Some(10.0));
        assert_eq!(sample.power.cpu.power_w, Some(10.0));
    }

    #[test]
    fn a_counter_that_goes_back_with_no_known_range_has_no_figure() {
        let scratch = tree("back");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            90_000_000,
            None,
        );
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        set_energy(scratch.path(), "intel-rapl:0", 40_000_000);
        let sample = sampler.sample(6_000).clone();
        let package = &sample.power.cpu.packages[0];
        assert_eq!(package.power_w, None);
        assert_eq!(package.reason.as_deref(), Some(power::COUNTER_WENT_BACK));
        assert_eq!(sample.power.cpu.power_w, None);
    }

    #[test]
    fn a_counter_that_cannot_be_read_names_the_file_and_says_only_root_may() {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = tree("refused");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            1_000_000,
            Some(262_143_328_850),
        );
        let counter = scratch
            .path()
            .join("sys/class/powercap/intel-rapl:0/energy_uj");
        fs::set_permissions(&counter, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read(&counter).is_ok() {
            // Root reads a file whatever its mode; the unit test of the error kinds covers this case there.
            eprintln!("running as root: the refused counter cannot be made here");
            return;
        }
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        let sample = sampler.sample(6_000).clone();

        let package = &sample.power.cpu.packages[0];
        assert_eq!(package.power_w, None);
        let reason = package.reason.clone().expect("a reason");
        assert!(
            reason.contains("/sys/class/powercap/intel-rapl:0/energy_uj"),
            "the file is named: {reason}"
        );
        assert!(reason.contains("may not read it"), "{reason}");
        assert!(reason.contains("root"), "{reason}");
        assert_eq!(sample.power.cpu.power_w, None);
        assert_eq!(sample.power.cpu.reason.as_deref(), Some(reason.as_str()));
        assert_eq!(sample.power.measured_power_w, None);
        assert!(not_included(&sample).contains(&"the CPU packages"));
    }

    #[test]
    fn a_remote_host_reports_no_local_power() {
        let scratch = Scratch::new("remote-power");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            1_000_000,
            Some(262_143_328_850),
        );
        let incus = Scripted::default();
        incus.resources(json!({
            "cpu": { "total": 4 },
            "memory": { "total": 1_073_741_824u64, "used": 1024u64 },
        }));
        let mut sampler = sampler(scratch.path(), incus, false);
        sampler.sample(1_000);
        let sample = sampler.sample(6_000).clone();
        assert_eq!(sample.power.cpu.reason.as_deref(), Some(REMOTE_HOST_REASON));
        assert!(sample.power.cpu.packages.is_empty());
        assert_eq!(sample.power.gpu.reason.as_deref(), Some(REMOTE_HOST_REASON));
        assert_eq!(sample.power.measured_power_w, None);
    }
}

mod gpu_power {
    use super::*;

    /// card1 is an amdgpu card at 0000:c6:00.0 whose hwmon gives power1_input in microwatts.
    fn amdgpu(root: &Path, microwatts: &str) {
        let device = "sys/devices/pci0000:00/0000:c6:00.0";
        write(root, &format!("{device}/hwmon/hwmon9/name"), "amdgpu\n");
        write(
            root,
            &format!("{device}/hwmon/hwmon9/power1_input"),
            microwatts,
        );
        fs::create_dir_all(root.join("sys/class/drm/card1")).expect("a dir");
        std::os::unix::fs::symlink(root.join(device), root.join("sys/class/drm/card1/device"))
            .expect("the device link");
        // A connector is not a card.
        fs::create_dir_all(root.join("sys/class/drm/card1-DP-1")).expect("a dir");
    }

    #[test]
    fn an_amdgpu_hwmon_power_is_read_in_watts_from_the_first_sample() {
        let scratch = tree("amdgpu");
        amdgpu(scratch.path(), "52035000\n");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        let sample = sampler.sample(1_000).clone();

        assert_eq!(sample.power.gpu.cards.len(), 1);
        let card = &sample.power.gpu.cards[0];
        assert_eq!(card.pci.as_deref(), Some("0000:c6:00.0"));
        assert_eq!(card.card.as_deref(), Some("card1"));
        assert_eq!(card.source, "hwmon");
        assert_eq!(card.power_w, Some(52.035));
        assert_eq!(card.reason, None);
        assert_eq!(sample.power.gpu.power_w, Some(52.035));
        assert_eq!(sample.power.gpu.reason, None);
        // The CPU has no counter, so the measured sum is the card alone.
        assert_eq!(sample.power.measured_power_w, Some(52.035));
        assert!(not_included(&sample).contains(&"the CPU packages"));
    }

    #[test]
    fn a_card_power_file_that_does_not_parse_has_no_figure_and_says_why() {
        let scratch = tree("amdgpu-bad");
        amdgpu(scratch.path(), "garbage\n");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        let sample = sampler.sample(1_000).clone();
        let card = &sample.power.gpu.cards[0];
        assert_eq!(card.power_w, None);
        let reason = card.reason.clone().expect("a reason");
        assert!(reason.contains("power1_input"), "{reason}");
        assert_eq!(sample.power.gpu.power_w, None);
        assert_eq!(sample.power.measured_power_w, None);
        assert!(
            not_included(&sample).contains(&"the GPU 0000:c6:00.0"),
            "{:?}",
            not_included(&sample)
        );
    }

    #[test]
    fn an_nvidia_card_power_comes_from_nvidia_smi() {
        let scratch = tree("nvidia");
        let incus = Scripted::default();
        incus.resources(json!({
            "cpu": { "total": 4 },
            "memory": { "total": 1_073_741_824u64, "used": 1024u64 },
            "gpu": { "total": 1, "cards": [{
                "pci_address": "0000:01:00.0",
                "vendor": "NVIDIA Corporation",
                "product": "NVIDIA GeForce RTX 4060 Ti",
                "driver": "nvidia",
                "driver_version": "580.65.06",
                "numa_node": 0,
            }]},
        }));
        let nvidia = FakeNvidia::with_readings(vec![NvidiaReading {
            pci: "0000:01:00.0".to_string(),
            uuid: None,
            telemetry: GpuTelemetry {
                power_draw_w: Some(42.5),
                ..GpuTelemetry::default()
            },
        }]);
        let mut sampler = sampler(scratch.path(), incus, true).with_nvidia(Arc::new(nvidia));
        let sample = sampler.sample(1_000).clone();

        assert_eq!(sample.power.gpu.cards.len(), 1);
        let card = &sample.power.gpu.cards[0];
        assert_eq!(card.pci.as_deref(), Some("0000:01:00.0"));
        assert_eq!(card.card, None);
        assert_eq!(card.source, "nvidia-smi");
        assert_eq!(card.power_w, Some(42.5));
        assert_eq!(sample.power.gpu.power_w, Some(42.5));
        assert_eq!(sample.power.measured_power_w, Some(42.5));
    }

    #[test]
    fn an_nvidia_card_without_telemetry_carries_the_card_reason() {
        let scratch = tree("nvidia-missing");
        let incus = Scripted::default();
        incus.resources(json!({
            "cpu": { "total": 4 },
            "memory": { "total": 1_073_741_824u64, "used": 1024u64 },
            "gpu": { "total": 1, "cards": [{
                "pci_address": "0000:01:00.0",
                "vendor": "NVIDIA Corporation",
                "product": "NVIDIA GeForce RTX 4060 Ti",
                "driver": "nvidia",
                "numa_node": 0,
            }]},
        }));
        let mut sampler = sampler(scratch.path(), incus, true);
        let sample = sampler.sample(1_000).clone();
        let card = &sample.power.gpu.cards[0];
        assert_eq!(card.power_w, None);
        assert_eq!(
            card.reason.as_deref(),
            Some(arca_metrics::NVIDIA_MISSING),
            "the telemetry's own reason"
        );
    }
}

mod power_mean {
    use super::*;

    #[test]
    fn the_mean_averages_each_package_over_the_samples_that_hold_it() {
        let scratch = tree("mean");
        zone(
            scratch.path(),
            "intel-rapl:0",
            "package-0",
            0,
            Some(262_143_328_850),
        );
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        sampler.sample(1_000);
        // 50 J in five seconds is 10 W, then 100 J is 20 W.
        set_energy(scratch.path(), "intel-rapl:0", 50_000_000);
        sampler.sample(6_000);
        set_energy(scratch.path(), "intel-rapl:0", 150_000_000);
        sampler.sample(11_000);

        let averaged = mean(&sampler.history(1)).expect("a mean");
        assert_eq!(averaged.power.cpu.packages[0].power_w, Some(15.0));
        assert_eq!(averaged.power.cpu.packages[0].reason, None);
        assert_eq!(averaged.power.cpu.power_w, Some(15.0));
        assert_eq!(
            averaged.power.cpu.reason, None,
            "the newest known reading has none"
        );
        assert_eq!(averaged.power.measured_power_w, Some(15.0));
    }
}

mod instance_cpu {
    use super::*;

    const DAY: &str = "2026-11-01T00:00:00Z";
    const TEN: &str = "2026-11-01T10:00:00Z";
    const ELEVEN: &str = "2026-11-01T11:00:00Z";

    fn figures(sample: &HostSample) -> Vec<(String, Option<f64>, Option<String>)> {
        sample
            .instance_cpu
            .instances
            .iter()
            .map(|entry: &InstanceThreads| {
                (entry.name.clone(), entry.busy_threads, entry.reason.clone())
            })
            .collect()
    }

    #[test]
    fn an_instance_cpu_is_threads_in_use_between_two_samples_largest_first() {
        let scratch = tree("threads-in-use");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN), running("b1", DAY, TEN)]);
        incus.cpu("a1", 2.0);
        incus.cpu("b1", 10.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);

        let first = sampler.sample(1_000).clone();
        let first_reading = Some(usage::FIRST_READING.to_string());
        assert_eq!(
            figures(&first),
            vec![
                ("a1".to_string(), None, first_reading.clone()),
                ("b1".to_string(), None, first_reading),
            ]
        );
        assert!(
            first
                .instance_cpu
                .instances
                .iter()
                .all(|e| e.project == "apps")
        );

        // a1 used 2.5 s more in five seconds, half a thread; b1 used 5 s more, one thread.
        incus.cpu("a1", 4.5);
        incus.cpu("b1", 15.0);
        let second = sampler.sample(6_000).clone();
        assert_eq!(
            figures(&second),
            vec![
                ("b1".to_string(), Some(1.0), None),
                ("a1".to_string(), Some(0.5), None),
            ]
        );
        assert_eq!(second.instance_cpu.omitted, 0);
        assert_eq!(second.instance_cpu.reason, None);
    }

    #[test]
    fn an_instance_that_restarts_between_two_samples_has_no_figure_never_a_negative_one() {
        let scratch = tree("restart");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN)]);
        incus.cpu("a1", 100.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);

        // It restarted: a new last start time and a counter from zero.
        incus.rows(vec![running("a1", DAY, ELEVEN)]);
        incus.cpu("a1", 1.0);
        let restarted = sampler.sample(6_000).clone();
        assert_eq!(
            figures(&restarted),
            vec![("a1".to_string(), None, Some(usage::RESTARTED.to_string()))]
        );

        // The next sample measures from the restarted counter: 2.5 s over five seconds.
        incus.cpu("a1", 3.5);
        let after = sampler.sample(11_000).clone();
        assert_eq!(figures(&after), vec![("a1".to_string(), Some(0.5), None)]);
    }

    #[test]
    fn an_instance_replaced_between_two_samples_has_no_figure_never_a_huge_one() {
        let scratch = tree("replaced");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN)]);
        incus.cpu("a1", 1.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);

        // Deleted and made again under the same name, with a counter far ahead.
        incus.rows(vec![running("a1", "2026-11-01T10:30:00Z", TEN)]);
        incus.cpu("a1", 9_000.0);
        let replaced = sampler.sample(6_000).clone();
        assert_eq!(
            figures(&replaced),
            vec![("a1".to_string(), None, Some(usage::REPLACED.to_string()))]
        );
    }

    #[test]
    fn a_counter_that_goes_back_with_the_same_start_has_no_figure() {
        let scratch = tree("cpu-back");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN)]);
        incus.cpu("a1", 50.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);
        incus.cpu("a1", 40.0);
        let sample = sampler.sample(6_000).clone();
        assert_eq!(
            figures(&sample),
            vec![(
                "a1".to_string(),
                None,
                Some(usage::CPU_WENT_BACK.to_string())
            )]
        );
    }

    #[test]
    fn a_figure_above_the_host_threads_is_dropped() {
        let scratch = tree("too-high");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN)]);
        incus.cpu("a1", 1.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);
        // 100 s in five seconds is 20 threads on a host of four.
        incus.cpu("a1", 101.0);
        let sample = sampler.sample(6_000).clone();
        assert_eq!(
            figures(&sample),
            vec![("a1".to_string(), None, Some(usage::ABOVE_HOST.to_string()))]
        );
    }

    #[test]
    fn an_instance_that_stops_leaves_the_list_and_its_return_is_a_first_reading() {
        let scratch = tree("stops");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN), running("b1", DAY, TEN)]);
        incus.cpu("a1", 1.0);
        incus.cpu("b1", 1.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);

        incus.rows(vec![running("a1", DAY, TEN), stopped("b1", DAY, TEN)]);
        incus.cpu("a1", 6.0);
        let stopped_sample = sampler.sample(6_000).clone();
        assert_eq!(
            figures(&stopped_sample),
            vec![("a1".to_string(), Some(1.0), None)],
            "a stopped instance has no row"
        );

        // It starts again with the same last start time and a counter far ahead: still no figure, since it was not
        // seen running at the previous sample.
        incus.rows(vec![running("a1", DAY, TEN), running("b1", DAY, TEN)]);
        incus.cpu("a1", 11.0);
        incus.cpu("b1", 500.0);
        let back = sampler.sample(11_000).clone();
        assert_eq!(
            figures(&back),
            vec![
                ("a1".to_string(), Some(1.0), None),
                (
                    "b1".to_string(),
                    None,
                    Some(usage::FIRST_READING.to_string())
                ),
            ]
        );
    }

    #[test]
    fn an_instance_whose_state_was_not_read_has_no_figure_and_says_so() {
        let scratch = tree("not-read");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN), running("b1", DAY, TEN)]);
        incus.cpu("a1", 1.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        let sample = sampler.sample(1_000).clone();
        let b1 = sample
            .instance_cpu
            .instances
            .iter()
            .find(|entry| entry.name == "b1")
            .expect("b1 is listed");
        assert_eq!(b1.busy_threads, None);
        assert_eq!(b1.reason.as_deref(), Some(usage::NOT_READ_NOW));
    }

    #[test]
    fn a_failed_instance_list_says_so_and_lists_nothing() {
        let scratch = tree("list-down");
        let mut sampler = sampler(scratch.path(), Scripted::default(), true);
        let sample = sampler.sample(1_000).clone();
        assert!(sample.instance_cpu.instances.is_empty());
        assert_eq!(
            sample.instance_cpu.reason.as_deref(),
            Some(usage::LIST_FAILED)
        );
    }

    #[test]
    fn the_list_is_capped_and_the_rest_is_counted_with_its_threads() {
        let scratch = Scratch::new("cap");
        // Sixty-four threads, so no figure here is above the host.
        let mut stat = String::from("cpu  1000 0 500 6000 500 0 0 0 0 0\n");
        for cpu in 0..64 {
            stat.push_str(&format!("cpu{cpu} 1 0 1 1 1 0 0 0 0 0\n"));
        }
        write(scratch.path(), "proc/stat", &stat);
        let incus = Scripted::default();
        let count = INSTANCE_CPU_MAX + 10;
        let names: Vec<String> = (0..count).map(|i| format!("i{i:03}")).collect();
        incus.rows(names.iter().map(|name| running(name, DAY, TEN)).collect());
        for name in &names {
            incus.cpu(name, 0.0);
        }
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);
        // Instance i uses i * 0.05 s in five seconds: i / 100 threads.
        for (i, name) in names.iter().enumerate() {
            incus.cpu(name, i as f64 * 0.05);
        }
        let sample = sampler.sample(6_000).clone();

        let listed = &sample.instance_cpu.instances;
        assert_eq!(listed.len(), INSTANCE_CPU_MAX);
        assert_eq!(
            listed[0].name,
            format!("i{:03}", count - 1),
            "the largest first"
        );
        assert_eq!(sample.instance_cpu.omitted, 10);
        // The ten smallest, i000 to i009: (0 + 1 + ... + 9) / 100 = 0.45 threads.
        let omitted = sample.instance_cpu.omitted_busy_threads;
        assert!((omitted - 0.45).abs() < 1e-9, "{omitted}");
    }

    #[test]
    fn the_mean_averages_each_instance_of_the_newest_sample_over_the_window() {
        let scratch = tree("instance-mean");
        let incus = Scripted::default();
        incus.rows(vec![running("a1", DAY, TEN)]);
        incus.cpu("a1", 0.0);
        let mut sampler = sampler(scratch.path(), incus.clone(), true);
        sampler.sample(1_000);
        // Half a thread, then one and a half.
        incus.cpu("a1", 2.5);
        sampler.sample(6_000);
        incus.cpu("a1", 10.0);
        sampler.sample(11_000);
        let averaged = mean(&sampler.history(1)).expect("a mean");
        assert_eq!(
            figures(&averaged),
            vec![("a1".to_string(), Some(1.0), None)]
        );
    }
}
