//! The host sampler, tested on a generated procfs and sysfs tree and a fake Incus. Nothing here reads the real host: every root
//! is a temporary directory this file writes, and every Incus request goes to a fake that records it. The expected values are
//! worked out by hand, in the comments beside them, with the same arithmetic the host sampler's formulas state.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_api::{conforms, verb};
use arca_incus::{FakeIncus, FakeState, UnixIncus};
use arca_metrics::{
    Cpu, HostSample, Load, Memory, Network, Pool, ProjectCount, Psi, Sampler, SamplerConfig, mean,
    node_metrics,
};
use serde_json::{Value, json};

const INSTANCES: &str = "/1.0/instances?all-projects=true&recursion=2";
const POOLS: &str = "/1.0/storage-pools?recursion=1";
/// The incus-osd documents the sampler reads at its first tick and every 5 min after.
const OSD_SECURITY: &str = "/os/1.0/system/security";
const OSD_STORAGE: &str = "/os/1.0/system/storage";
const OSD_UPDATE: &str = "/os/1.0/system/update";

/// A fresh temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-metrics-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
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

const MEMINFO: &str = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    8000000 kB\nBuffers:          500000 kB\nSwapTotal:       2000000 kB\nSwapFree:        1500000 kB\n";
const PSI_MEMORY: &str = "some avg10=1.25 avg60=0.80 avg300=0.40 total=123456\nfull avg10=0.50 avg60=0.20 avg300=0.10 total=65432\n";
const PSI_CPU: &str = "some avg10=2.75 avg60=1.00 avg300=0.50 total=987654\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";

/// The first `stat` reading: aggregate total 1000 + 500 + 6000 + 500 = 8000 jiffies over the first eight fields, idle
/// 6000 + 500 = 6500, so busy 1500. The ninth and tenth fields are zero here.
const STAT_FIRST: &str = "cpu  1000 0 500 6000 500 0 0 0 0 0\ncpu0 250 0 125 1500 125 0 0 0 0 0\ncpu1 250 0 125 1500 125 0 0 0 0 0\ncpu2 250 0 125 1500 125 0 0 0 0 0\ncpu3 250 0 125 1500 125 0 0 0 0 0\nintr 12345 0\nctxt 6789\n";

/// The second `stat` reading. The first eight fields total 1150 + 600 + 6600 + 650 = 9000 and idle is 6600 + 650 = 7250, so busy
/// is 1750. Against the first reading, the total rose by 1000 and busy by 250, a share of 100 * 250 / 1000 = 25 percent. The
/// ninth and tenth fields (777 and 888) are not counted.
const STAT_SECOND: &str = "cpu  1150 0 600 6600 650 0 0 0 777 888\ncpu0 287 0 150 1650 162 0 0 0 0 0\ncpu1 287 0 150 1650 162 0 0 0 0 0\ncpu2 288 0 150 1650 163 0 0 0 0 0\ncpu3 288 0 150 1650 163 0 0 0 0 0\nintr 23456 0\nctxt 9876\n";

/// The NIC table. `lo` and `br0` carry traffic too, but neither has a `device` link, so they never count.
fn net_dev(eth0_rx: u64, eth0_tx: u64) -> String {
    format!(
        "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 2000 10 0 0 0 0 0 0 2000 10 0 0 0 0 0 0\n  eth0: {eth0_rx} 100 0 0 0 0 0 0 {eth0_tx} 80 0 0 0 0 0 0\n  br0: 19000000 100 0 0 0 0 0 0 19000000 80 0 0 0 0 0 0\n"
    )
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a fixture file has a parent"))
        .expect("create a fixture directory");
    fs::write(&path, text).expect("write a fixture file");
}

/// The tree a sample starts from: every file the sampler reads, with the first counters.
fn write_first_tree(root: &Path) {
    write(root, "proc/stat", STAT_FIRST);
    write(root, "proc/loadavg", "0.52 0.48 0.40 2/345 6789\n");
    write(root, "proc/meminfo", MEMINFO);
    write(root, "proc/pressure/memory", PSI_MEMORY);
    write(root, "proc/pressure/cpu", PSI_CPU);
    write(root, "proc/net/dev", &net_dev(5_000_000, 3_000_000));
    write(root, "sys/class/hwmon/hwmon0/name", "acpitz\n");
    write(root, "sys/class/hwmon/hwmon0/temp1_input", "30000\n");
    write(root, "sys/class/hwmon/hwmon1/name", "k10temp\n");
    write(root, "sys/class/hwmon/hwmon1/temp1_input", "54875\n");
    write(root, "sys/class/drm/card0/device/gpu_busy_percent", "42\n");
    // eth0 is physical (it has a device link); lo and br0 are not.
    write(root, "sys/class/net/eth0/device/uevent", "");
    write(root, "sys/class/net/lo/uevent", "");
    write(root, "sys/class/net/br0/uevent", "");
}

/// The counters that move between two samples, five seconds apart.
fn write_second_counters(root: &Path) {
    write(root, "proc/stat", STAT_SECOND);
    // eth0 receives 500000 bytes and transmits 250000 more in five seconds: 100000 and 50000 bytes per second.
    write(root, "proc/net/dev", &net_dev(5_500_000, 3_250_000));
}

fn fake_state() -> FakeState {
    FakeState {
        server: json!({ "api_version": "1.0" }),
        projects: vec![json!({ "name": "apps" }), json!({ "name": "svc" })],
        instances: vec![
            json!({ "name": "a1", "project": "apps", "status": "Running" }),
            json!({ "name": "a2", "project": "apps", "status": "Stopped" }),
            json!({ "name": "b1", "project": "svc", "status": "Running" }),
        ],
        networks: Vec::new(),
        pools: vec![(
            "default".to_string(),
            json!({
                "space": { "used": 1_073_741_824u64, "total": 10_737_418_240u64 },
                "inodes": { "used": 1200, "total": 1_048_576 }
            }),
        )],
    }
}

fn sampler_for(
    root: &Path,
    socket: &Path,
    pools: &[&str],
    interval_ms: u64,
    history_minutes: u32,
) -> Sampler<UnixIncus> {
    let cfg = SamplerConfig {
        proc_root: root.join("proc"),
        sys_root: root.join("sys"),
        pools: pools.iter().map(|pool| pool.to_string()).collect(),
        interval_ms,
        history_minutes,
        incus_local: true,
    };
    Sampler::new(cfg, UnixIncus::new(socket.to_path_buf()))
}

fn has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_key(v, key)),
        Value::Array(items) => items.iter().any(|v| has_key(v, key)),
        _ => false,
    }
}

/// A sample with every reading absent, for the mean tests to fill in.
fn bare(at: u64) -> HostSample {
    HostSample {
        at,
        cpu: Cpu {
            used_pct: None,
            cores: 4,
            reason: None,
        },
        load: Load {
            one: None,
            five: None,
            fifteen: None,
            reason: None,
        },
        memory: Memory {
            total_bytes: None,
            available_bytes: None,
            used_bytes: None,
            swap_total_bytes: None,
            swap_free_bytes: None,
            reason: None,
        },
        psi: Psi {
            memory_some_avg10: None,
            memory_full_avg10: None,
            cpu_some_avg10: None,
        },
        cpu_temp_c: None,
        gpu_busy_pct: None,
        pools: Vec::new(),
        network: None,
        projects: Vec::new(),
    }
}

#[test]
fn every_field_matches_the_value_worked_out_by_hand() {
    let scratch = Scratch::new("fields");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);

    let first = sampler.sample(1_000).clone();
    assert_eq!(
        first.cpu.used_pct, None,
        "the first sample has no previous counters"
    );
    assert_eq!(
        first.network, None,
        "the first sample has no previous NIC counters"
    );
    assert_eq!(first.cpu.cores, 4, "four numbered CPU lines");

    write_second_counters(scratch.path());
    let second = sampler.sample(6_000).clone();

    assert_eq!(second.at, 6_000);
    assert_eq!(second.cpu.used_pct, Some(25.0));
    assert_eq!(second.cpu.cores, 4);
    assert_eq!(
        second.load,
        Load {
            one: Some(0.52),
            five: Some(0.48),
            fifteen: Some(0.40),
            reason: None
        }
    );
    // 16000000, 8000000, 2000000 and 1500000 kB, each times 1024.
    assert_eq!(
        second.memory,
        Memory {
            total_bytes: Some(16_384_000_000),
            available_bytes: Some(8_192_000_000),
            used_bytes: Some(8_192_000_000),
            swap_total_bytes: Some(2_048_000_000),
            swap_free_bytes: Some(1_536_000_000),
            reason: None,
        }
    );
    assert_eq!(
        second.psi,
        Psi {
            memory_some_avg10: Some(1.25),
            memory_full_avg10: Some(0.5),
            cpu_some_avg10: Some(2.75),
        }
    );
    // The k10temp hwmon, in millidegrees: 54875 / 1000. The acpitz hwmon does not count.
    assert_eq!(second.cpu_temp_c, Some(54.875));
    assert_eq!(second.gpu_busy_pct, Some(42.0));
    // Free is total minus used: 10737418240 - 1073741824 = 9663676416.
    assert_eq!(
        second.pools,
        vec![Pool {
            name: "default".to_string(),
            used_bytes: Some(1_073_741_824),
            total_bytes: Some(10_737_418_240),
            free_bytes: Some(9_663_676_416),
        }]
    );
    // Only eth0 counts: rx 500000 and tx 250000 more over five seconds, 100000 and 50000 bytes per second.
    assert_eq!(
        second.network,
        Some(Network {
            rx_bytes_per_sec: 100_000.0,
            tx_bytes_per_sec: 50_000.0,
        })
    );
    // apps holds two instances, one running; svc holds one, running.
    assert_eq!(
        second.projects,
        vec![
            ProjectCount {
                project: "apps".to_string(),
                total: 2,
                running: 1,
            },
            ProjectCount {
                project: "svc".to_string(),
                total: 1,
                running: 1,
            },
        ]
    );
}

#[test]
fn a_missing_file_reads_null_for_its_field_and_nothing_panics() {
    let scratch = Scratch::new("missing");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);
    sampler.sample(1_000);

    write_second_counters(scratch.path());
    for gone in [
        "proc/meminfo",
        "proc/loadavg",
        "proc/pressure/memory",
        "sys/class/hwmon/hwmon1/temp1_input",
    ] {
        fs::remove_file(scratch.path().join(gone)).expect("remove a fixture file");
    }
    let sample = sampler.sample(6_000).clone();

    assert_eq!(
        sample.memory,
        Memory {
            total_bytes: None,
            available_bytes: None,
            used_bytes: None,
            swap_total_bytes: None,
            swap_free_bytes: None,
            reason: None,
        }
    );
    assert_eq!(
        sample.load,
        Load {
            one: None,
            five: None,
            fifteen: None,
            reason: None
        }
    );
    assert_eq!(sample.psi.memory_some_avg10, None);
    assert_eq!(sample.psi.memory_full_avg10, None);
    assert_eq!(
        sample.psi.cpu_some_avg10,
        Some(2.75),
        "the CPU pressure file is still there"
    );
    assert_eq!(
        sample.cpu_temp_c, None,
        "a k10temp hwmon without temp1_input reads nothing"
    );
    assert_eq!(
        sample.cpu.used_pct,
        Some(25.0),
        "the CPU counters are still there"
    );
    assert!(sample.network.is_some());
}

#[test]
fn a_missing_cpu_file_gives_zero_cores_and_no_percentage() {
    let scratch = Scratch::new("no-stat");
    write_first_tree(scratch.path());
    fs::remove_file(scratch.path().join("proc/stat")).expect("remove the stat file");
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);

    let sample = sampler.sample(1_000).clone();
    assert_eq!(
        sample.cpu,
        Cpu {
            used_pct: None,
            cores: 0,
            reason: None
        }
    );
}

#[test]
fn an_unparseable_aggregate_line_reads_no_percentage_but_still_counts_cores() {
    let scratch = Scratch::new("bad-stat");
    write_first_tree(scratch.path());
    write(
        scratch.path(),
        "proc/stat",
        "cpu  a b c\ncpu0 1 2 3 4\ncpu1 1 2 3 4\n",
    );
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);

    // Two readings of the same unreadable aggregate: no percentage, and the two numbered CPUs still count.
    sampler.sample(1_000);
    let sample = sampler.sample(6_000).clone();
    assert_eq!(
        sample.cpu,
        Cpu {
            used_pct: None,
            cores: 2,
            reason: None
        }
    );
}

#[test]
fn the_fake_incus_records_only_get_the_pool_list_the_instance_list_and_one_read_per_admitted_pool()
{
    let scratch = Scratch::new("gets");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(
        scratch.path(),
        fake.socket(),
        &["default", "missing"],
        5_000,
        60,
    );
    let sample = sampler.sample(1_000).clone();

    let requests = fake.requests();
    assert_eq!(
        requests.len(),
        6,
        "the pool list, the instance list, one resources read for the admitted pool and the three incus-osd documents"
    );
    assert!(requests.iter().all(|r| r.method == "GET"), "{requests:?}");
    let mut paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            INSTANCES,
            "/1.0/storage-pools/default/resources",
            POOLS,
            OSD_SECURITY,
            OSD_STORAGE,
            OSD_UPDATE,
        ]
    );

    assert_eq!(
        sample.pools.len(),
        1,
        "a filter name Incus does not list has no row"
    );
    assert_eq!(sample.pools[0].name, "default");
}

#[test]
fn a_pool_name_that_is_not_plain_is_never_sent_to_incus() {
    let scratch = Scratch::new("names");
    write_first_tree(scratch.path());
    let mut state = fake_state();
    state.pools = ["x/../default", "default?all=1", ".."]
        .iter()
        .map(|name| {
            (
                name.to_string(),
                json!({ "space": { "used": 1, "total": 2 } }),
            )
        })
        .collect();
    let fake = FakeIncus::start(scratch.path(), state).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &[], 5_000, 60);
    let sample = sampler.sample(1_000).clone();

    let mut paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            INSTANCES.to_string(),
            POOLS.to_string(),
            OSD_SECURITY.to_string(),
            OSD_STORAGE.to_string(),
            OSD_UPDATE.to_string(),
        ],
        "the pool list and the instance list were sent, and no resources read"
    );
    assert_eq!(sample.pools.len(), 3, "every listed name keeps its row");
    assert!(
        sample
            .pools
            .iter()
            .all(|p| p.used_bytes.is_none() && p.free_bytes.is_none())
    );
}

#[test]
fn an_unreachable_incus_keeps_the_host_fields_and_empties_the_lists() {
    let scratch = Scratch::new("down");
    write_first_tree(scratch.path());
    let absent = scratch.path().join("absent.sock");
    let mut sampler = sampler_for(scratch.path(), &absent, &["default"], 5_000, 60);

    let sample = sampler.sample(1_000).clone();
    assert_eq!(sample.pools.len(), 1);
    assert_eq!(sample.pools[0].used_bytes, None);
    assert!(
        sample.projects.is_empty(),
        "no instance answer reads as no project rows"
    );
    assert_eq!(
        sample.gpu_busy_pct,
        Some(42.0),
        "host readings do not need Incus"
    );
}

#[test]
fn a_physical_nic_counter_that_moves_backwards_reads_a_zero_rate() {
    let scratch = Scratch::new("backwards");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);
    sampler.sample(1_000);

    // The receive counter fell (a reset); the transmit counter rose by 250000 over five seconds.
    write(scratch.path(), "proc/net/dev", &net_dev(100, 3_250_000));
    let sample = sampler.sample(6_000).clone();
    assert_eq!(
        sample.network,
        Some(Network {
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 50_000.0,
        })
    );
}

#[test]
fn a_host_with_no_physical_nic_reads_zero_rates_not_null() {
    let scratch = Scratch::new("no-nic");
    write_first_tree(scratch.path());
    fs::remove_dir_all(scratch.path().join("sys/class/net/eth0/device"))
        .expect("remove the eth0 device link");
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);
    sampler.sample(1_000);

    write_second_counters(scratch.path());
    let sample = sampler.sample(6_000).clone();
    assert_eq!(
        sample.network,
        Some(Network {
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
        }),
        "NIC counters were read and none is physical, so the sum is zero"
    );
}

#[test]
fn history_is_the_window_before_the_latest_sample() {
    let scratch = Scratch::new("window");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &[], 5_000, 60);
    for i in 0..=24u64 {
        sampler.sample(i * 5_000);
    }

    // The latest sample is at 120000 ms. One minute back is 60000 ms, inclusive: 13 samples from 60000 to 120000.
    let one_minute = sampler.history(1);
    assert_eq!(one_minute.len(), 13);
    assert_eq!(one_minute.first().map(|s| s.at), Some(60_000));
    assert_eq!(one_minute.last().map(|s| s.at), Some(120_000));
    assert_eq!(sampler.history(2).len(), 25);
    assert_eq!(sampler.latest().map(|s| s.at), Some(120_000));
}

#[test]
fn the_ring_keeps_sixty_minutes_of_five_second_samples() {
    let scratch = Scratch::new("ring");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &[], 5_000, 60);
    // 730 samples at 5 s: the ring keeps 60 * 60000 / 5000 = 720, the first 10 are gone.
    for i in 0..730u64 {
        sampler.sample(i * 5_000);
    }

    let window = sampler.history(60);
    assert_eq!(window.len(), 720);
    assert_eq!(window.first().map(|s| s.at), Some(50_000));
    assert_eq!(window.last().map(|s| s.at), Some(729 * 5_000));
}

#[test]
fn mean_averages_each_field_over_the_samples_that_hold_it() {
    let mut a = bare(1_000);
    a.memory.available_bytes = Some(100);
    a.network = Some(Network {
        rx_bytes_per_sec: 10.0,
        tx_bytes_per_sec: 20.0,
    });
    a.projects = vec![ProjectCount {
        project: "apps".to_string(),
        total: 4,
        running: 2,
    }];
    a.pools = vec![Pool {
        name: "default".to_string(),
        used_bytes: Some(1_000),
        total_bytes: Some(4_000),
        free_bytes: Some(3_000),
    }];

    let mut b = bare(2_000);
    b.cpu.used_pct = Some(20.0);
    b.memory.available_bytes = Some(200);
    b.pools = vec![Pool {
        name: "default".to_string(),
        used_bytes: Some(2_000),
        total_bytes: Some(4_000),
        free_bytes: Some(2_000),
    }];

    let mut c = bare(3_000);
    c.cpu.used_pct = Some(30.0);
    c.network = Some(Network {
        rx_bytes_per_sec: 30.0,
        tx_bytes_per_sec: 40.0,
    });
    c.projects = vec![ProjectCount {
        project: "apps".to_string(),
        total: 2,
        running: 0,
    }];
    c.pools = vec![Pool {
        name: "default".to_string(),
        used_bytes: None,
        total_bytes: Some(7_000),
        free_bytes: None,
    }];

    let window = mean(&[a, b, c]).expect("a window of three samples has a mean");
    assert_eq!(window.at, 3_000, "the time is the last sample's");
    // Only b and c hold a CPU share: (20 + 30) / 2.
    assert_eq!(window.cpu.used_pct, Some(25.0));
    // Only a and b hold available memory: (100 + 200) / 2.
    assert_eq!(window.memory.available_bytes, Some(150));
    // Only a and c hold a network sample: rx (10 + 30) / 2 and tx (20 + 40) / 2.
    assert_eq!(
        window.network,
        Some(Network {
            rx_bytes_per_sec: 20.0,
            tx_bytes_per_sec: 30.0,
        })
    );
    // apps is 4 in a, absent in b (zero), 2 in c: (4 + 0 + 2) / 3 = 2, and running (2 + 0 + 0) / 3 = 0.67, which rounds to 1.
    assert_eq!(
        window.projects,
        vec![ProjectCount {
            project: "apps".to_string(),
            total: 2,
            running: 1,
        }]
    );
    // default used: (1000 + 2000) / 2; total: (4000 + 4000 + 7000) / 3; free: (3000 + 2000) / 2. Each pool figure
    // averages the samples that hold it, so c's total counts and c's missing used and free do not.
    assert_eq!(
        window.pools,
        vec![Pool {
            name: "default".to_string(),
            used_bytes: Some(1_500),
            total_bytes: Some(5_000),
            free_bytes: Some(2_500),
        }]
    );
    assert_eq!(window.load.one, None, "no sample holds a load reading");
    assert_eq!(mean(&[]), None, "an empty window has no mean");
}

#[test]
fn node_metrics_conforms_to_the_frozen_result_schema_and_has_no_renter_gate() {
    let scratch = Scratch::new("conform");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);
    sampler.sample(1_000);
    write_second_counters(scratch.path());
    sampler.sample(6_000);

    let value = node_metrics(&sampler, Some(5), 6_000);
    let schema = (verb("node_metrics")
        .expect("node_metrics is in the verb table")
        .result)();
    conforms(&schema, &value).expect("the node_metrics result conforms to its schema");
    assert!(
        !has_key(&value, "renterGate"),
        "no renterGate key at any depth: {value}"
    );
    assert_eq!(value["minutes"], json!(5));
    assert_eq!(value["at"], json!(6_000));
    assert_eq!(value["latest"]["cpu"]["usedPct"], json!(25.0));
    assert_eq!(value["mean"]["cpu"]["usedPct"], json!(25.0));
}

#[test]
fn node_metrics_reads_one_minute_when_none_is_named_and_nulls_when_empty() {
    let scratch = Scratch::new("default-minutes");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(scratch.path(), fake.socket(), &["default"], 5_000, 60);

    let empty = node_metrics(&sampler, None, 0);
    assert_eq!(empty["minutes"], json!(1));
    assert_eq!(empty["latest"], Value::Null);
    assert_eq!(empty["mean"], Value::Null);

    sampler.sample(1_000);
    let filled = node_metrics(&sampler, None, 1_000);
    let schema = (verb("node_metrics")
        .expect("node_metrics is in the verb table")
        .result)();
    conforms(&schema, &filled).expect("a one-sample result conforms too");
    assert_eq!(filled["minutes"], json!(1));
}

/// The pools Incus lists, read from the sampler's Incus client. The listing is the only source of a pool row, so the allow-filter
/// and an outage are checked against the same listing a node has.
mod pools_from_incus {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use arca_incus::{IncusError, IncusRead};

    use super::*;

    /// Incus holds two pools: `default` (used 1073741824 of 10737418240) and `pool1` (used 500 of 1000).
    fn two_pools() -> FakeState {
        let mut state = fake_state();
        state.pools.push((
            "pool1".to_string(),
            json!({ "space": { "used": 500, "total": 1000 } }),
        ));
        state
    }

    fn default_row() -> Pool {
        Pool {
            name: "default".to_string(),
            used_bytes: Some(1_073_741_824),
            total_bytes: Some(10_737_418_240),
            free_bytes: Some(9_663_676_416),
        }
    }

    fn pool1_row() -> Pool {
        Pool {
            name: "pool1".to_string(),
            used_bytes: Some(500),
            total_bytes: Some(1000),
            free_bytes: Some(500),
        }
    }

    fn unread(name: &str) -> Pool {
        Pool {
            name: name.to_string(),
            used_bytes: None,
            total_bytes: None,
            free_bytes: None,
        }
    }

    fn sampler_over<R: IncusRead>(root: &Path, incus: R, pools: &[&str]) -> Sampler<R> {
        Sampler::new(
            SamplerConfig {
                proc_root: root.join("proc"),
                sys_root: root.join("sys"),
                pools: pools.iter().map(|pool| pool.to_string()).collect(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
            },
            incus,
        )
    }

    /// The fake's client, with two switches: every request fails while `down` is set, and the `resources` read of `refused`
    /// fails while the listing still names that pool.
    struct Switchable {
        inner: UnixIncus,
        down: Arc<AtomicBool>,
        refused: Option<&'static str>,
    }

    impl IncusRead for Switchable {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            let refused = self
                .refused
                .is_some_and(|pool| path == format!("/1.0/storage-pools/{pool}/resources"));
            if self.down.load(Ordering::SeqCst) || refused {
                return Err(IncusError::Decode(format!("refused for the test: {path}")));
            }
            self.inner.get(path)
        }
    }

    fn switchable(socket: &Path, down: bool, refused: Option<&'static str>) -> Switchable {
        Switchable {
            inner: UnixIncus::new(socket.to_path_buf()),
            down: Arc::new(AtomicBool::new(down)),
            refused,
        }
    }

    fn sorted_paths(fake: &FakeIncus) -> Vec<String> {
        let mut paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
        paths.sort_unstable();
        paths
    }

    #[test]
    fn lists_every_pool() {
        let scratch = Scratch::new("lists-every");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), two_pools()).expect("start the fake Incus");
        let mut sampler = sampler_over(
            scratch.path(),
            UnixIncus::new(fake.socket().to_path_buf()),
            &[],
        );

        let sample = sampler.sample(1_000).clone();

        assert_eq!(
            sampler.readings().pools,
            Some(vec![default_row(), pool1_row()]),
            "an empty filter admits every pool Incus lists"
        );
        assert_eq!(sample.pools, vec![default_row(), pool1_row()]);
        assert!(
            fake.requests().iter().all(|r| r.method == "GET"),
            "only GET requests"
        );
        assert_eq!(
            sorted_paths(&fake),
            vec![
                INSTANCES.to_string(),
                "/1.0/storage-pools/default/resources".to_string(),
                "/1.0/storage-pools/pool1/resources".to_string(),
                POOLS.to_string(),
                OSD_SECURITY.to_string(),
                OSD_STORAGE.to_string(),
                OSD_UPDATE.to_string(),
            ]
        );
    }

    #[test]
    fn filter() {
        let scratch = Scratch::new("filter");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), two_pools()).expect("start the fake Incus");
        let mut sampler = sampler_over(
            scratch.path(),
            UnixIncus::new(fake.socket().to_path_buf()),
            &["pool1"],
        );

        let sample = sampler.sample(1_000).clone();

        assert_eq!(
            sample.pools,
            vec![pool1_row()],
            "only the admitted pool has a row"
        );
        assert_eq!(sampler.readings().pools, Some(vec![pool1_row()]));
        assert_eq!(
            sorted_paths(&fake),
            vec![
                INSTANCES.to_string(),
                "/1.0/storage-pools/pool1/resources".to_string(),
                POOLS.to_string(),
                OSD_SECURITY.to_string(),
                OSD_STORAGE.to_string(),
                OSD_UPDATE.to_string(),
            ],
            "a pool the filter drops is not read"
        );
    }

    #[test]
    fn filtered_name_not_listed() {
        let scratch = Scratch::new("filtered-out");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), two_pools()).expect("start the fake Incus");
        let mut sampler = sampler_over(
            scratch.path(),
            UnixIncus::new(fake.socket().to_path_buf()),
            &["pool9"],
        );

        let sample = sampler.sample(1_000).clone();

        assert!(
            sample.pools.is_empty(),
            "pool9 is not listed, so it has no row"
        );
        assert_eq!(
            sampler.readings().pools,
            Some(Vec::new()),
            "a listing that worked with no admitted pool is known to be empty"
        );
        assert_eq!(
            sorted_paths(&fake),
            vec![
                INSTANCES.to_string(),
                POOLS.to_string(),
                OSD_SECURITY.to_string(),
                OSD_STORAGE.to_string(),
                OSD_UPDATE.to_string(),
            ],
            "no resources read for a name Incus does not list"
        );
    }

    #[test]
    fn incus_down() {
        let scratch = Scratch::new("incus-down");
        write_first_tree(scratch.path());
        let absent = scratch.path().join("absent.sock");
        let mut sampler = sampler_over(
            scratch.path(),
            UnixIncus::new(absent),
            &["default", "pool1"],
        );

        let sample = sampler.sample(1_000).clone();

        assert_eq!(
            sampler.readings().pools,
            None,
            "a failed listing is unknown"
        );
        assert_eq!(
            sampler.readings().projects,
            None,
            "a failed instance list is unknown"
        );
        assert!(
            sampler.readings().nic.is_some(),
            "the NIC counters come from the host, not from Incus"
        );
        assert!(
            sample.projects.is_empty(),
            "the sample's own list stays empty"
        );
        assert_eq!(
            sample.pools,
            vec![unread("default"), unread("pool1")],
            "before any listing, the filter's names carry null fields"
        );
        assert_eq!(sample.at, 1_000);
    }

    #[test]
    fn incus_down_after_a_listing_keeps_the_listed_names() {
        let scratch = Scratch::new("incus-down-after");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), two_pools()).expect("start the fake Incus");
        let down = Arc::new(AtomicBool::new(false));
        let incus = Switchable {
            inner: UnixIncus::new(fake.socket().to_path_buf()),
            down: Arc::clone(&down),
            refused: None,
        };
        let mut sampler = sampler_over(scratch.path(), incus, &[]);
        sampler.sample(1_000);
        down.store(true, Ordering::SeqCst);

        let sample = sampler.sample(6_000).clone();

        assert_eq!(sampler.readings().pools, None);
        assert_eq!(sampler.readings().projects, None);
        assert_eq!(
            sample.pools,
            vec![unread("default"), unread("pool1")],
            "the last listing's names stay, with null fields"
        );
        assert!(sample.projects.is_empty());
    }

    #[test]
    fn resources_fail() {
        let scratch = Scratch::new("resources-fail");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), two_pools()).expect("start the fake Incus");
        let mut sampler = sampler_over(
            scratch.path(),
            switchable(fake.socket(), false, Some("pool1")),
            &[],
        );

        let sample = sampler.sample(1_000).clone();

        assert_eq!(
            sampler.readings().pools,
            Some(vec![default_row(), unread("pool1")]),
            "the listing worked, so pool1 keeps its row with null fields"
        );
        assert_eq!(sample.pools, vec![default_row(), unread("pool1")]);
        assert_eq!(
            sample.projects.len(),
            2,
            "the instance list is still read on this tick"
        );
    }
}

mod instance_readings {
    use std::sync::{Arc, Mutex};

    use arca_incus::{IncusError, IncusRead};
    use arca_metrics::{InstanceKind, InstanceReading, ProjectCount};

    use super::*;

    const LISTED: &str = "/1.0/instances?all-projects=true&recursion=2";

    /// Incus holds one project with a running container `db1`, a running virtual machine `vm1` and a stopped container `web1`.
    /// `db1` was last used at 10:00:00 UTC.
    fn proj1_state() -> FakeState {
        FakeState {
            server: json!({ "api_version": "1.0" }),
            projects: vec![json!({ "name": "proj1" })],
            instances: vec![
                json!({
                    "name": "db1", "project": "proj1", "status": "Running", "type": "container",
                    "last_used_at": "2026-11-01T10:00:00Z"
                }),
                json!({ "name": "vm1", "project": "proj1", "status": "Running", "type": "virtual-machine" }),
                json!({ "name": "web1", "project": "proj1", "status": "Stopped", "type": "container" }),
            ],
            networks: Vec::new(),
            pools: Vec::new(),
        }
    }

    /// An instance row as the scripted listing serves it.
    fn row(name: &str, last_used: Option<&str>) -> Value {
        let mut row =
            json!({ "name": name, "project": "proj1", "status": "Running", "type": "container" });
        if let Some(last_used) = last_used {
            row["last_used_at"] = json!(last_used);
        }
        row
    }

    /// An Incus whose instance list the test sets: `None` makes the list fail, as an absent socket does. Every other path fails.
    #[derive(Clone, Default)]
    struct Listing(Arc<Mutex<Option<Vec<Value>>>>);

    impl Listing {
        fn answer(&self, rows: Option<Vec<Value>>) {
            *self.0.lock().expect("the listing is not poisoned") = rows;
        }
    }

    impl IncusRead for Listing {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path != LISTED {
                return Err(IncusError::Decode(format!("no answer for {path}")));
            }
            match self.0.lock().expect("the listing is not poisoned").clone() {
                Some(rows) => Ok(Value::Array(rows)),
                None => Err(IncusError::Decode("the listing is down".to_string())),
            }
        }
    }

    fn sampler_over<R: IncusRead>(root: &Path, incus: R) -> Sampler<R> {
        Sampler::new(
            SamplerConfig {
                proc_root: root.join("proc"),
                sys_root: root.join("sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
            },
            incus,
        )
    }

    /// The reading of the instance `name` at the latest sample. The list must have been read.
    fn reading_of<R: IncusRead>(sampler: &Sampler<R>, name: &str) -> InstanceReading {
        sampler
            .readings()
            .instances
            .as_ref()
            .expect("the instance list was read")
            .iter()
            .find(|instance| instance.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no reading for {name}"))
    }

    #[test]
    fn the_instance_list_is_one_get_per_tick_at_recursion_two() {
        let scratch = Scratch::new("one-get");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), proj1_state()).expect("start the fake Incus");
        let mut sampler = sampler_over(scratch.path(), UnixIncus::new(fake.socket().to_path_buf()));

        for at in [1_000, 6_000, 11_000] {
            sampler.sample(at);
        }

        let paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
        let listed = paths
            .iter()
            .filter(|path| path.starts_with("/1.0/instances"))
            .count();
        assert_eq!(listed, 3, "one instance list per tick: {paths:?}");
        assert!(
            paths
                .iter()
                .all(|path| path == LISTED || !path.starts_with("/1.0/instances"))
        );
    }

    #[test]
    fn an_instance_reading_carries_its_kind_running_state_and_usage() {
        let scratch = Scratch::new("kind-usage");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), proj1_state()).expect("start the fake Incus");
        fake.set_instance_usage("proj1", "db1", 2_500_000_000, 1_048_576);
        fake.set_instance_usage("proj1", "vm1", 7_000_000_000, 2_147_483_648);
        let mut sampler = sampler_over(scratch.path(), UnixIncus::new(fake.socket().to_path_buf()));

        let sample = sampler.sample(1_000).clone();

        let instances = sampler
            .readings()
            .instances
            .clone()
            .expect("the list was read");
        let plain = |project: &str, name: &str, kind, running, cpu, memory| InstanceReading {
            project: project.to_string(),
            name: name.to_string(),
            kind,
            running,
            starts_total: 0,
            starts_10m: 0,
            cpu_seconds: cpu,
            memory_bytes: memory,
        };
        assert_eq!(
            instances,
            vec![
                plain(
                    "proj1",
                    "db1",
                    InstanceKind::Container,
                    true,
                    Some(2.5),
                    Some(1_048_576)
                ),
                plain(
                    "proj1",
                    "vm1",
                    InstanceKind::Vm,
                    true,
                    Some(7.0),
                    Some(2_147_483_648)
                ),
                plain("proj1", "web1", InstanceKind::Container, false, None, None),
            ],
            "a stopped instance has no usage, so its series is left out, not written as zero"
        );
        assert_eq!(
            sample.projects,
            vec![ProjectCount {
                project: "proj1".to_string(),
                total: 3,
                running: 2,
            }],
            "the project counts come from the same list"
        );
        assert_eq!(sampler.readings().projects, Some(sample.projects.clone()));
    }

    #[test]
    fn starts_are_changes_of_last_used_inside_ten_minutes_and_the_first_tick_counts_none() {
        let scratch = Scratch::new("starts");
        write_first_tree(scratch.path());
        let fake = FakeIncus::start(scratch.path(), proj1_state()).expect("start the fake Incus");
        let mut sampler = sampler_over(scratch.path(), UnixIncus::new(fake.socket().to_path_buf()));

        sampler.sample(0);
        let first = reading_of(&sampler, "db1");
        assert_eq!(
            (first.starts_total, first.starts_10m),
            (0, 0),
            "the first tick counts nothing"
        );

        for (at, used) in [
            (5_000, "2026-11-01T10:00:05Z"),
            (10_000, "2026-11-01T10:00:10Z"),
            (15_000, "2026-11-01T10:00:15Z"),
        ] {
            fake.set_last_used("proj1", "db1", used);
            sampler.sample(at);
        }
        let burst = reading_of(&sampler, "db1");
        assert_eq!((burst.starts_total, burst.starts_10m), (3, 3));

        sampler.sample(300_000);
        let inside = reading_of(&sampler, "db1");
        assert_eq!(
            (inside.starts_total, inside.starts_10m),
            (3, 3),
            "still inside ten minutes"
        );

        // The last start was at 15 s. Ten minutes after it, the window no longer holds it.
        sampler.sample(615_000);
        let later = reading_of(&sampler, "db1");
        assert_eq!(
            (later.starts_total, later.starts_10m),
            (3, 0),
            "the total stays for the life of the process, the window empties"
        );
    }

    #[test]
    fn an_absent_socket_reads_no_instances_and_no_project_counts() {
        let scratch = Scratch::new("absent");
        write_first_tree(scratch.path());
        let absent = scratch.path().join("absent.sock");
        let mut sampler = sampler_over(scratch.path(), UnixIncus::new(absent));

        let sample = sampler.sample(1_000).clone();
        assert_eq!(
            sampler.readings().instances,
            None,
            "unknown, never no instances"
        );
        assert_eq!(sampler.readings().projects, None);
        assert!(sample.projects.is_empty());
    }

    #[test]
    fn a_failed_listing_keeps_each_count_and_the_next_one_counts_from_the_last_success() {
        let scratch = Scratch::new("outage");
        write_first_tree(scratch.path());
        let listing = Listing::default();
        let mut sampler = sampler_over(scratch.path(), listing.clone());

        listing.answer(Some(vec![row("db1", Some("A"))]));
        sampler.sample(0);
        listing.answer(Some(vec![row("db1", Some("B"))]));
        sampler.sample(5_000);
        assert_eq!(reading_of(&sampler, "db1").starts_total, 1);

        listing.answer(None);
        sampler.sample(10_000);
        assert_eq!(
            sampler.readings().instances,
            None,
            "a failed list is unknown"
        );

        listing.answer(Some(vec![row("db1", Some("C"))]));
        sampler.sample(15_000);
        let after = reading_of(&sampler, "db1");
        assert_eq!(
            (after.starts_total, after.starts_10m),
            (2, 2),
            "B to C counts, though the tick between them failed"
        );
    }

    #[test]
    fn an_instance_the_list_drops_is_forgotten_so_its_return_counts_no_start() {
        let scratch = Scratch::new("forgotten");
        write_first_tree(scratch.path());
        let listing = Listing::default();
        let mut sampler = sampler_over(scratch.path(), listing.clone());

        listing.answer(Some(vec![row("db1", Some("A"))]));
        sampler.sample(0);
        listing.answer(Some(vec![row("db1", Some("B"))]));
        sampler.sample(5_000);
        assert_eq!(reading_of(&sampler, "db1").starts_total, 1);

        listing.answer(Some(Vec::new()));
        sampler.sample(10_000);
        assert_eq!(
            sampler.readings().instances,
            Some(Vec::new()),
            "an empty list is known"
        );

        // The instance returns with no last use: it is seen for the first time, so the change from B to none counts nothing.
        listing.answer(Some(vec![row("db1", None)]));
        sampler.sample(15_000);
        let back = reading_of(&sampler, "db1");
        assert_eq!((back.starts_total, back.starts_10m), (0, 0));
    }

    #[test]
    fn a_row_without_a_project_or_a_name_is_no_instance() {
        let scratch = Scratch::new("malformed");
        write_first_tree(scratch.path());
        let listing = Listing::default();
        let mut sampler = sampler_over(scratch.path(), listing.clone());
        listing.answer(Some(vec![
            json!({ "name": "db1", "status": "Running", "type": "container" }),
            json!({ "project": "proj1", "status": "Running", "type": "container" }),
            row("web1", None),
        ]));

        let sample = sampler.sample(1_000).clone();
        let names: Vec<String> = sampler
            .readings()
            .instances
            .clone()
            .expect("the list was read")
            .into_iter()
            .map(|instance| instance.name)
            .collect();
        assert_eq!(names, vec!["web1".to_string()]);
        assert_eq!(
            sample.projects[0].total, 1,
            "the project count drops the same rows"
        );
    }
}

mod latency_probe {
    use std::net::{SocketAddr, TcpListener};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    use arca_metrics::ProbeTarget;
    use arca_metrics::probe::tcp_connect_seconds;

    use super::*;

    const ONE_SECOND: Duration = Duration::from_secs(1);

    /// A listener on a loopback port the kernel picks. Each connection it accepts is sent on the returned channel. The accepting
    /// thread ends with the test process.
    fn listener() -> (SocketAddr, Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().expect("the listener has an address");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
                let _ = tx.send(());
            }
        });
        (addr, rx)
    }

    /// A loopback port with nothing listening on it: the listener is bound, its address kept, and the listener closed.
    fn closed_port() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().expect("the listener has an address");
        drop(listener);
        addr
    }

    /// A sampler over a host with no files and an Incus with no socket, so only the probes have readings, with the given targets.
    fn probing(scratch: &Scratch, probes: Vec<(ProbeTarget, SocketAddr)>) -> Sampler<UnixIncus> {
        sampler_for(
            scratch.path(),
            &scratch.path().join("incus.sock"),
            &[],
            5_000,
            60,
        )
        .with_probes(probes)
    }

    /// The time of the newest probe round, or `None` when no round has run.
    fn round_at(sampler: &Sampler<UnixIncus>) -> Option<u64> {
        sampler.readings().latency.as_ref().map(|round| round.at)
    }

    #[test]
    fn a_listener_on_loopback_gives_the_connect_time_in_seconds() {
        let (addr, _accepted) = listener();
        let seconds = tcp_connect_seconds(addr, ONE_SECOND).expect("the connect completes");
        assert!(
            seconds.is_finite() && (0.0..1.0).contains(&seconds),
            "a loopback connect is quick: {seconds}"
        );
    }

    #[test]
    fn a_closed_port_gives_none_within_one_point_one_seconds() {
        let started = Instant::now();
        assert_eq!(tcp_connect_seconds(closed_port(), ONE_SECOND), None);
        assert!(
            started.elapsed() < Duration::from_millis(1_100),
            "the refused connect returns within the timeout's margin: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn probes_run_at_zero_and_thirty_seconds_and_not_at_five() {
        let scratch = Scratch::new("latency-due");
        let (addr, accepted) = listener();
        let mut sampler = probing(&scratch, vec![(ProbeTarget::Gateway, addr)]);

        sampler.sample(0);
        assert_eq!(round_at(&sampler), Some(0), "the first sample probes");
        accepted
            .recv_timeout(ONE_SECOND)
            .expect("the round at 0 s connects");

        sampler.sample(5_000);
        assert_eq!(round_at(&sampler), Some(0), "no round is due at 5 s");
        assert!(
            accepted.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing connects at 5 s"
        );

        sampler.sample(30_000);
        assert_eq!(round_at(&sampler), Some(30_000), "a round is due 30 s on");
        accepted
            .recv_timeout(ONE_SECOND)
            .expect("the round at 30 s connects");
    }

    #[test]
    fn a_clock_that_steps_back_still_runs_a_round() {
        let scratch = Scratch::new("latency-clock");
        let (addr, _accepted) = listener();
        let mut sampler = probing(&scratch, vec![(ProbeTarget::Gateway, addr)]);
        sampler.sample(60_000);
        sampler.sample(10_000);
        assert_eq!(round_at(&sampler), Some(10_000));
    }

    #[test]
    fn an_unnamed_target_has_no_entry_and_a_failed_one_reads_none() {
        let scratch = Scratch::new("latency-entries");
        let (addr, _accepted) = listener();
        let mut sampler = probing(
            &scratch,
            vec![
                (ProbeTarget::Gateway, addr),
                (ProbeTarget::Relay, closed_port()),
            ],
        );
        sampler.sample(0);
        let round = sampler.readings().latency.clone().expect("a round ran");
        assert_eq!(round.at, 0);
        assert_eq!(round.targets.len(), 2, "internet was not named");
        assert_eq!(round.targets[0].0, ProbeTarget::Gateway);
        assert!(round.targets[0].1.is_some(), "the listener answers");
        assert_eq!(round.targets[1], (ProbeTarget::Relay, None));
    }

    #[test]
    fn a_sampler_with_no_named_target_never_probes() {
        let scratch = Scratch::new("latency-none");
        let mut sampler = probing(&scratch, Vec::new());
        for at in [0, 30_000, 60_000] {
            sampler.sample(at);
        }
        assert_eq!(sampler.readings().latency, None);
    }
}

mod osd_readings {
    use std::sync::{Arc, Mutex};

    use arca_incus::{IncusError, IncusRead};
    use arca_metrics::OsdReading;

    use super::*;

    const SECURITY: &str = "/os/1.0/system/security";
    const UPDATE: &str = "/os/1.0/system/update";

    /// A fake secret. It is obviously fake, and no reading, debug text or error may carry it.
    const FAKE_KEY: &str = "FAKE-RECOVERY-KEY-NOT-REAL-0002";

    /// The check time of the fixtures: 10:00:00 UTC on 2026-11-01, in epoch milliseconds.
    const CHECKED: u64 = 1_793_527_200_000;
    const DAY: u64 = 86_400_000;

    /// The two documents the fake serves, or `None` for both answering 404 as a plain Incus does. Each read is recorded by path.
    #[derive(Clone, Default)]
    struct Docs {
        served: Arc<Mutex<Option<(Value, Value)>>>,
        reads: Arc<Mutex<Vec<String>>>,
    }

    impl Docs {
        fn serve(&self, security: Value, update: Value) {
            *self.served.lock().expect("the documents are not poisoned") = Some((security, update));
        }

        fn down(&self) {
            *self.served.lock().expect("the documents are not poisoned") = None;
        }

        /// The times the security document was read at, given the sample times.
        fn security_reads(&self) -> usize {
            self.reads
                .lock()
                .expect("the reads are not poisoned")
                .iter()
                .filter(|path| path.as_str() == SECURITY)
                .count()
        }
    }

    impl IncusRead for Docs {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            self.reads
                .lock()
                .expect("the reads are not poisoned")
                .push(path.to_string());
            let served = self.served.lock().expect("the documents are not poisoned");
            match (served.as_ref(), path) {
                (Some((security, _)), SECURITY) => Ok(security.clone()),
                (Some((_, update)), UPDATE) => Ok(update.clone()),
                _ => Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                }),
            }
        }
    }

    /// The security document with Secure Boot on, a TPM that reports ok, and the fake keys in the places incus-osd keeps them.
    fn security() -> Value {
        json!({
            "config": { "encryption_recovery_keys": [FAKE_KEY] },
            "state": {
                "secure_boot_enabled": true,
                "tpm_status": "ok",
                "drive_recovery_keys": { "drive1": FAKE_KEY },
                "pool_recovery_keys": { "pool1": FAKE_KEY },
                "tpm_public_key": "FAKE-PUBLIC-KEY-NOT-REAL",
            },
        })
    }

    /// The update document with a status and a check time.
    fn update(status: &str, last_check: &str) -> Value {
        json!({
            "config": { "auto_reboot": false },
            "state": { "status": status, "last_check": last_check, "needs_reboot": false },
        })
    }

    /// A sampler over roots that do not exist and no pools, so only the incus-osd documents have readings.
    fn sampler_over(docs: &Docs) -> Sampler<Docs> {
        Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-osd-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-osd-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
            },
            docs.clone(),
        )
    }

    /// The reading the sampler holds at its latest sample.
    fn reading(sampler: &Sampler<Docs>) -> Option<OsdReading> {
        sampler.readings().osd.clone()
    }

    #[test]
    fn a_plain_incus_that_answers_404_gives_no_osd_reading() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        sampler.sample(CHECKED);
        assert_eq!(reading(&sampler), None, "404 is unknown, never a reading");
    }

    #[test]
    fn never_checked_uses_proc_root_uptime_on_osd_cadence_even_after_sampler_restart() {
        let scratch = Scratch::new("never-checked-uptime");
        let docs = Docs::default();
        docs.serve(security(), json!({ "state": { "status": "" } }));
        let cfg = SamplerConfig {
            proc_root: scratch.path().join("proc"),
            sys_root: scratch.path().join("sys"),
            pools: Vec::new(),
            interval_ms: 5_000,
            history_minutes: 60,
            incus_local: true,
        };
        let mut sampler = Sampler::new(cfg.clone(), docs.clone());
        sampler.sample(CHECKED);
        assert_eq!(
            reading(&sampler).unwrap().update_failed,
            None,
            "missing uptime"
        );
        write(scratch.path(), "proc/uptime", "82800.00 0.00\n");
        sampler.sample(CHECKED + 300_000);
        assert_eq!(reading(&sampler).unwrap().update_failed, None, "23 hours");
        write(scratch.path(), "proc/uptime", "86400.00 0.00\n");
        sampler.sample(CHECKED + 305_000);
        assert_eq!(
            reading(&sampler).unwrap().update_failed,
            None,
            "between reads"
        );
        sampler.sample(CHECKED + 600_000);
        assert_eq!(
            reading(&sampler).unwrap().update_failed,
            Some(true),
            "24 hours"
        );

        let mut restarted = Sampler::new(cfg, docs.clone());
        restarted.sample(CHECKED + 600_000);
        assert_eq!(
            reading(&restarted).unwrap().update_failed,
            Some(true),
            "node uptime survives sampler restart"
        );

        write(scratch.path(), "proc/uptime", "not uptime\n");
        restarted.sample(CHECKED + 900_000);
        assert_eq!(
            reading(&restarted).unwrap().update_failed,
            None,
            "invalid uptime is unknown"
        );
        write(scratch.path(), "proc/uptime", "90000.00 0.00\n");
        docs.serve(
            security(),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        restarted.sample(CHECKED + 1_200_000);
        assert_eq!(reading(&restarted).unwrap().update_failed, Some(false));
        restarted.sample(CHECKED + 1_500_000);
        assert_eq!(reading(&restarted).unwrap().update_failed, Some(false));
    }

    #[test]
    fn the_documents_are_read_at_zero_and_300_seconds_and_not_between() {
        let docs = Docs::default();
        docs.serve(
            security(),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        let mut sampler = sampler_over(&docs);
        let mut read_at = Vec::new();
        let mut seen = 0;
        for at in (0..=600_000).step_by(5_000) {
            sampler.sample(at);
            let now = docs.security_reads();
            if now != seen {
                read_at.push(at);
                seen = now;
            }
        }
        assert_eq!(
            read_at,
            vec![0, 300_000, 600_000],
            "reads at 0 s, 300 s and 600 s only"
        );
        assert_eq!(
            docs.reads
                .lock()
                .expect("the reads are not poisoned")
                .iter()
                .filter(|p| p.as_str() == UPDATE)
                .count(),
            3,
            "the update document is read at the same times"
        );
    }

    #[test]
    fn a_failed_status_gives_update_failed_and_a_check_15_days_old_does_too() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        docs.serve(
            security(),
            update(
                "Failed to refresh system extensions",
                "2026-11-01T10:00:00Z",
            ),
        );
        sampler.sample(CHECKED);
        let failed = reading(&sampler).expect("both documents read");
        assert_eq!(failed.update_failed, Some(true), "a failed status");
        assert_eq!(
            failed.last_success_ms, None,
            "a failed status has no success time"
        );

        docs.serve(
            security(),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        sampler.sample(CHECKED + 15 * DAY);
        let stale = reading(&sampler).expect("both documents read");
        assert_eq!(
            stale.update_failed,
            Some(true),
            "a check 15 days old with no failed status"
        );
        assert_eq!(stale.last_check_ms, Some(CHECKED));
    }

    #[test]
    fn a_check_one_day_old_with_no_failed_status_is_not_a_failure() {
        let docs = Docs::default();
        docs.serve(
            security(),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        let mut sampler = sampler_over(&docs);
        sampler.sample(CHECKED + DAY);
        let fresh = reading(&sampler).expect("both documents read");
        assert_eq!(fresh.update_failed, Some(false));
        assert!(fresh.secure_boot);
        assert!(fresh.tpm_ok);
        assert_eq!(fresh.last_success_ms, Some(CHECKED));
    }

    #[test]
    fn a_read_that_fails_clears_the_reading_until_the_next_read_and_between_reads_it_holds() {
        let docs = Docs::default();
        docs.serve(
            security(),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        let mut sampler = sampler_over(&docs);
        sampler.sample(0);
        assert!(reading(&sampler).is_some());
        sampler.sample(60_000);
        assert!(
            reading(&sampler).is_some(),
            "between reads the reading holds"
        );
        docs.down();
        sampler.sample(300_000);
        assert_eq!(reading(&sampler), None, "the read at 300 s failed");
        docs.serve(
            security(),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        sampler.sample(330_000);
        assert_eq!(reading(&sampler), None, "no read until 600 s");
        sampler.sample(600_000);
        assert!(
            reading(&sampler).is_some(),
            "the next read at 600 s succeeds"
        );
    }

    #[test]
    fn a_missing_check_time_leaves_staleness_unknown_and_no_success_but_the_status_still_reads() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        docs.serve(
            security(),
            json!({ "state": { "status": "OS update check completed" } }),
        );
        sampler.sample(CHECKED + 30 * DAY);
        let reading =
            reading(&sampler).expect("both documents answer with the fields the status needs");
        assert_eq!(
            reading.update_failed, None,
            "no check time and no earlier success: the 14-day leg is unknown, never ok"
        );
        assert_eq!(reading.last_check_ms, None);
        assert_eq!(reading.last_success_ms, None);
    }

    /// The RFC 3339 check time `day` days after the fixtures' check time.
    fn checked_on(day: u64) -> String {
        format!("2026-11-{:02}T10:00:00Z", 1 + day)
    }

    /// Serves an update document with `status` checked on `day`, and samples at `at`.
    fn read_at(
        sampler: &mut Sampler<Docs>,
        docs: &Docs,
        status: &str,
        day: u64,
        at: u64,
    ) -> OsdReading {
        docs.serve(security(), update(status, &checked_on(day)));
        sampler.sample(at);
        reading(sampler).expect("both documents read")
    }

    #[test]
    fn a_failure_followed_by_a_skipping_status_stays_failed_even_across_a_failed_read() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        let failed = read_at(
            &mut sampler,
            &docs,
            "Failed to check for OS updates",
            0,
            CHECKED,
        );
        assert_eq!(failed.update_failed, Some(true));
        docs.down();
        sampler.sample(CHECKED + 300_000);
        assert_eq!(reading(&sampler), None, "the read failed");
        let skipped = read_at(
            &mut sampler,
            &docs,
            "Skipping update check outside of maintenance window(s), will check at the beginning of the next maintenance window in 2h0m0s",
            0,
            CHECKED + 600_000,
        );
        assert_eq!(
            skipped.update_failed,
            Some(true),
            "a skip says nothing new, so the failure holds"
        );
        assert_eq!(skipped.last_success_ms, None, "a skip is not a success");
    }

    #[test]
    fn a_checking_status_mid_check_does_not_clear_a_firing_alert() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        let failed = read_at(
            &mut sampler,
            &docs,
            "Failed to refresh system extensions",
            0,
            CHECKED,
        );
        assert_eq!(failed.update_failed, Some(true));
        let checking = read_at(
            &mut sampler,
            &docs,
            "Checking for application updates",
            1,
            CHECKED + DAY,
        );
        assert_eq!(
            checking.update_failed,
            Some(true),
            "a check in progress keeps the failure"
        );
        assert_eq!(checking.last_check_ms, Some(CHECKED + DAY));
        let done = read_at(
            &mut sampler,
            &docs,
            "Application update check completed",
            1,
            CHECKED + DAY + 300_000,
        );
        assert_eq!(
            done.update_failed,
            Some(false),
            "a completed check clears it"
        );
        assert_eq!(done.last_success_ms, Some(CHECKED + DAY));
    }

    #[test]
    fn a_checking_status_with_no_earlier_result_is_unknown_not_ok() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        let checking = read_at(&mut sampler, &docs, "Checking for OS updates", 0, CHECKED);
        assert_eq!(checking.update_failed, None);
        assert_eq!(checking.last_success_ms, None);
    }

    #[test]
    fn the_success_time_holds_the_last_success_across_a_failure() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        let ok = read_at(&mut sampler, &docs, "OS update check completed", 0, CHECKED);
        assert_eq!(ok.last_success_ms, Some(CHECKED));
        let failed = read_at(
            &mut sampler,
            &docs,
            "Failed to check for OS updates",
            1,
            CHECKED + DAY,
        );
        assert_eq!(failed.update_failed, Some(true));
        assert_eq!(failed.last_check_ms, Some(CHECKED + DAY));
        assert_eq!(
            failed.last_success_ms,
            Some(CHECKED),
            "the gauge keeps the last success"
        );
        let checking = read_at(
            &mut sampler,
            &docs,
            "Checking for OS updates",
            2,
            CHECKED + 2 * DAY,
        );
        assert_eq!(checking.last_success_ms, Some(CHECKED));
        let updated = read_at(
            &mut sampler,
            &docs,
            "IncusOS has been updated to version 202611030000",
            2,
            CHECKED + 2 * DAY + 300_000,
        );
        assert_eq!(updated.update_failed, Some(false));
        assert_eq!(updated.last_success_ms, Some(CHECKED + 2 * DAY));
    }

    #[test]
    fn the_14_day_leg_fires_while_checks_keep_failing_and_the_check_time_moves_forward() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        read_at(&mut sampler, &docs, "OS update check completed", 0, CHECKED);
        // An error's own text is not a known status, so it carries the earlier ok; only the age of the last success can
        // catch a node stuck like this.
        for day in 1..=13 {
            let carried = read_at(
                &mut sampler,
                &docs,
                "lookup of installed applications broke",
                day,
                CHECKED + day * DAY,
            );
            assert_eq!(
                carried.update_failed,
                Some(false),
                "day {day} is inside 14 days"
            );
            assert_eq!(carried.last_check_ms, Some(CHECKED + day * DAY));
        }
        for day in 14..=16 {
            let stale = read_at(
                &mut sampler,
                &docs,
                "lookup of installed applications broke",
                day,
                CHECKED + day * DAY,
            );
            assert_eq!(
                stale.update_failed,
                Some(true),
                "day {day}: no success for 14 days"
            );
            assert_eq!(stale.last_success_ms, Some(CHECKED));
            assert_eq!(
                stale.last_check_ms,
                Some(CHECKED + day * DAY),
                "the check time kept moving"
            );
        }
        // Failed statuses with a moving check time keep the same last success.
        let failed = read_at(
            &mut sampler,
            &docs,
            "Failed to check for OS updates",
            17,
            CHECKED + 17 * DAY,
        );
        assert_eq!(failed.update_failed, Some(true));
        assert_eq!(failed.last_success_ms, Some(CHECKED));
    }

    #[test]
    fn an_unparseable_check_time_does_not_turn_off_staleness() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        read_at(&mut sampler, &docs, "OS update check completed", 0, CHECKED);
        docs.serve(security(), update("Checking for OS updates", "not a time"));
        sampler.sample(CHECKED + 15 * DAY);
        let reading = reading(&sampler).expect("both documents read");
        assert_eq!(
            reading.last_check_ms, None,
            "an unparseable time is no time"
        );
        assert_eq!(
            reading.update_failed,
            Some(true),
            "staleness still runs from the last success"
        );
    }

    #[test]
    fn a_document_missing_its_security_half_is_no_reading_at_all() {
        let docs = Docs::default();
        let mut sampler = sampler_over(&docs);
        docs.serve(
            json!({ "state": {} }),
            update("OS update check completed", "2026-11-01T10:00:00Z"),
        );
        sampler.sample(CHECKED);
        assert_eq!(reading(&sampler), None);
    }

    #[test]
    fn no_fake_secret_reaches_the_readings_or_their_debug_form() {
        let docs = Docs::default();
        docs.serve(
            security(),
            update("Failed to check for OS updates", "2026-11-01T10:00:00Z"),
        );
        let mut sampler = sampler_over(&docs);
        sampler.sample(CHECKED);
        let debug = format!("{:?}", sampler.readings());
        assert!(!debug.contains(FAKE_KEY), "a fake key reached the readings");
        assert!(
            !debug.contains("FAKE-PUBLIC-KEY"),
            "a fake key reached the readings"
        );
        assert!(reading(&sampler).is_some());
    }
}

mod smart_readings {
    use std::sync::{Arc, Mutex};

    use arca_incus::{IncusError, IncusRead};
    use arca_metrics::DrivesReading;

    use super::*;

    const STORAGE: &str = "/os/1.0/system/storage";

    /// The storage document the fake serves, or nothing served, which answers 404 as a plain Incus does. Each read is counted.
    #[derive(Clone, Default)]
    struct Storage {
        served: Arc<Mutex<Option<Value>>>,
        reads: Arc<Mutex<usize>>,
    }

    impl Storage {
        fn serve(&self, document: Value) {
            *self.served.lock().expect("the document is not poisoned") = Some(document);
        }

        fn reads(&self) -> usize {
            *self.reads.lock().expect("the count is not poisoned")
        }
    }

    impl IncusRead for Storage {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path != STORAGE {
                return Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                });
            }
            *self.reads.lock().expect("the count is not poisoned") += 1;
            match self
                .served
                .lock()
                .expect("the document is not poisoned")
                .as_ref()
            {
                Some(document) => Ok(document.clone()),
                None => Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                }),
            }
        }
    }

    /// Two drives with SMART enabled, one passed and one failed, and a drive with no SMART block. The model and serial are fake,
    /// and no reading may carry them.
    fn storage_document() -> Value {
        json!({
            "config": { "scrub_schedule": "", "trim_schedule": "" },
            "state": {
                "drives": [
                    {
                        "id": "disk1",
                        "model_name": "FAKE-MODEL-NOT-REAL",
                        "serial_number": "FAKE-SERIAL-NOT-REAL-0001",
                        "smart": { "enabled": true, "passed": true },
                    },
                    {
                        "id": "disk2",
                        "model_name": "FAKE-MODEL-NOT-REAL",
                        "serial_number": "FAKE-SERIAL-NOT-REAL-0002",
                        "smart": { "enabled": true, "passed": false, "error": "" },
                    },
                    {
                        "id": "disk3",
                        "model_name": "FAKE-MODEL-NOT-REAL",
                        "serial_number": "FAKE-SERIAL-NOT-REAL-0003",
                    },
                ],
                "root_partition": {},
            },
        })
    }

    /// A sampler over roots that do not exist and no pools, so only the storage document has a reading.
    fn sampler_over(storage: &Storage) -> Sampler<Storage> {
        Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-smart-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-smart-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
            },
            storage.clone(),
        )
    }

    /// The drives reading the sampler holds at its latest sample.
    fn drives(sampler: &Sampler<Storage>) -> Option<DrivesReading> {
        sampler.readings().drives.clone()
    }

    #[test]
    fn smart_readings_one_reading_per_listed_drive_and_unknown_for_one_without_a_result() {
        let storage = Storage::default();
        storage.serve(storage_document());
        let mut sampler = sampler_over(&storage);
        sampler.sample(0);

        let read = drives(&sampler).expect("the storage document was read");
        assert_eq!(read.at, 0);
        assert_eq!(read.drives.len(), 3, "disk3 is listed with no SMART block");
        let passed: Vec<Option<bool>> = read.drives.iter().map(|drive| drive.passed).collect();
        assert_eq!(passed, vec![Some(true), Some(false), None]);
        for drive in &read.drives {
            assert!(drive.device.starts_with("drive-"), "{}", drive.device);
            assert_eq!(drive.device.len(), 18, "a prefix and twelve hex digits");
            assert!(!drive.device.contains("disk"), "the label is not the ID");
        }
        assert_ne!(read.drives[0].device, read.drives[1].device);
    }

    #[test]
    fn smart_readings_no_model_serial_or_id_text_reaches_the_readings_or_their_debug_form() {
        let storage = Storage::default();
        storage.serve(storage_document());
        let mut sampler = sampler_over(&storage);
        sampler.sample(0);

        let shown = format!("{:?}", sampler.readings().drives);
        for secret in ["FAKE-SERIAL", "FAKE-MODEL", "disk1", "disk2", "disk3"] {
            assert!(!shown.contains(secret), "{secret} reached the readings");
        }
    }

    #[test]
    fn smart_readings_the_read_happens_at_zero_and_600_seconds_and_not_between() {
        let storage = Storage::default();
        storage.serve(storage_document());
        let mut sampler = sampler_over(&storage);
        let mut read_at = Vec::new();
        let mut seen = 0;
        for at in (0..=900_000).step_by(5_000) {
            sampler.sample(at);
            if storage.reads() != seen {
                read_at.push(at);
                seen = storage.reads();
            }
            if at == 300_000 {
                assert_eq!(
                    drives(&sampler).map(|read| read.at),
                    Some(0),
                    "between reads the last reading holds"
                );
            }
        }
        assert_eq!(read_at, vec![0, 600_000], "reads at 0 s and 600 s only");
    }

    #[test]
    fn smart_readings_a_404_gives_none_and_so_does_an_enabled_drive_without_a_result() {
        let storage = Storage::default();
        let mut sampler = sampler_over(&storage);
        sampler.sample(0);
        assert_eq!(drives(&sampler), None, "404 is unknown, never a reading");

        let mut malformed = storage_document();
        malformed["state"]["drives"][1]["smart"]
            .as_object_mut()
            .expect("the SMART block is an object")
            .remove("passed");
        storage.serve(malformed);
        sampler.sample(600_000);
        assert_eq!(
            drives(&sampler),
            None,
            "a failing drive is never hidden as absent"
        );
    }
}

/// The host totals of the machine the node's Incus serves, and the local readings that must never stand in for them.
mod host_totals {
    use std::sync::{Arc, Mutex};

    use arca_incus::{IncusError, IncusRead};

    use super::*;

    /// The path the host totals are read from.
    const RESOURCES: &str = "/1.0/resources";
    const REMOTE_REASON: &str = "not available for a remote host";
    const FAILED_REASON: &str = "the Incus resources read failed";

    /// An Incus that answers one `/1.0/resources` result, and 404 for every other path, recording each path it is asked
    /// for.
    #[derive(Clone)]
    struct Resources {
        answer: Arc<Mutex<Result<Value, IncusError>>>,
        reads: Arc<Mutex<Vec<String>>>,
    }

    impl Resources {
        fn serving(document: Value) -> Resources {
            Resources {
                answer: Arc::new(Mutex::new(Ok(document))),
                reads: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing() -> Resources {
            Resources {
                answer: Arc::new(Mutex::new(Err(IncusError::Status {
                    code: 503,
                    message: "unavailable".to_string(),
                }))),
                reads: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// How many times the resources path itself was asked for.
        fn resources_reads(&self) -> usize {
            self.reads
                .lock()
                .expect("the reads are not poisoned")
                .iter()
                .filter(|path| path.as_str() == RESOURCES)
                .count()
        }
    }

    impl IncusRead for Resources {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            self.reads
                .lock()
                .expect("the reads are not poisoned")
                .push(path.to_string());
            if path != RESOURCES {
                return Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                });
            }
            self.answer
                .lock()
                .expect("the answer is not poisoned")
                .clone()
        }
    }

    /// The part of a `/1.0/resources` answer this sampler reads.
    fn resources(cpu_threads: u64, memory_total: u64, memory_used: u64) -> Value {
        json!({
            "cpu": { "total": cpu_threads },
            "memory": { "total": memory_total, "used": memory_used },
        })
    }

    fn sampler(root: &Path, incus: Resources, incus_local: bool) -> Sampler<Resources> {
        Sampler::new(
            SamplerConfig {
                proc_root: root.join("proc"),
                sys_root: root.join("sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local,
            },
            incus,
        )
    }

    /// A sample with no host reading at all, for the empty pressure comparison.
    fn no_psi() -> Psi {
        Psi {
            memory_some_avg10: None,
            memory_full_avg10: None,
            cpu_some_avg10: None,
        }
    }

    #[test]
    fn a_remote_host_reports_the_incus_totals_and_leaves_every_local_reading_null() {
        let scratch = Scratch::new("remote-totals");
        // The local tree says 4 threads and 16 GiB. A remote node must never report those.
        write_first_tree(scratch.path());
        let incus = Resources::serving(resources(16, 17_045_651_456, 3_417_186_304));
        let mut sampler = sampler(scratch.path(), incus.clone(), false);

        let sample = sampler.sample(1_000).clone();

        assert_eq!(sample.cpu.cores, 16, "cpu.total threads, never the local 4");
        assert_eq!(sample.cpu.used_pct, None);
        assert_eq!(sample.cpu.reason.as_deref(), Some(REMOTE_REASON));
        assert_eq!(
            sample.load,
            Load {
                one: None,
                five: None,
                fifteen: None,
                reason: Some(REMOTE_REASON.to_string()),
            }
        );
        assert_eq!(sample.memory.total_bytes, Some(17_045_651_456));
        assert_eq!(sample.memory.used_bytes, Some(3_417_186_304));
        assert_eq!(
            sample.memory.available_bytes,
            Some(17_045_651_456 - 3_417_186_304)
        );
        assert_eq!(sample.memory.reason, None);
        assert_eq!(sample.memory.swap_total_bytes, None);
        assert_eq!(sample.memory.swap_free_bytes, None);
        assert_eq!(sample.psi, no_psi());
        assert_eq!(sample.cpu_temp_c, None);
        assert_eq!(sample.gpu_busy_pct, None);
        assert_eq!(sample.network, None);
        assert_eq!(incus.resources_reads(), 1, "the totals came from Incus");

        let value = node_metrics(&sampler, Some(1), 1_000);
        let schema = (verb("node_metrics")
            .expect("node_metrics is in the verb table")
            .result)();
        conforms(&schema, &value).expect("a remote node_metrics result conforms to its schema");
        assert_eq!(value["latest"]["cpu"]["cores"], json!(16));
        assert_eq!(
            value["latest"]["memory"]["totalBytes"],
            json!(17_045_651_456u64)
        );
        assert_eq!(
            value["mean"]["memory"]["usedBytes"],
            json!(3_417_186_304u64)
        );
        assert_eq!(value["mean"]["load"]["reason"], json!(REMOTE_REASON));
    }

    #[test]
    fn a_local_host_keeps_the_proc_readings_and_never_asks_incus_for_totals() {
        let scratch = Scratch::new("local-proc");
        write_first_tree(scratch.path());
        // Incus would call this host a different machine. A local node must not read it.
        let incus = Resources::serving(resources(16, 17_045_651_456, 3_417_186_304));
        let mut sampler = sampler(scratch.path(), incus.clone(), true);

        let first = sampler.sample(1_000).clone();
        assert_eq!(first.cpu.cores, 4, "the local /proc count");
        assert_eq!(first.cpu.reason, None);
        assert_eq!(first.load.one, Some(0.52));
        assert_eq!(first.load.reason, None);
        assert_eq!(first.memory.total_bytes, Some(16_384_000_000));
        assert_eq!(first.memory.available_bytes, Some(8_192_000_000));
        assert_eq!(
            first.memory.used_bytes,
            Some(16_384_000_000 - 8_192_000_000)
        );
        assert_eq!(first.memory.reason, None);

        write_second_counters(scratch.path());
        let second = sampler.sample(6_000).clone();
        assert_eq!(second.cpu.used_pct, Some(25.0));
        assert_eq!(
            incus.resources_reads(),
            0,
            "a local host keeps its own /proc totals and asks Incus for none"
        );
    }

    #[test]
    fn a_failed_resources_read_is_null_with_a_reason_and_never_the_local_numbers() {
        let scratch = Scratch::new("remote-failed");
        // The local tree would answer 4 threads and 16 GiB. A failed resources read must not fall back to it.
        write_first_tree(scratch.path());
        let incus = Resources::failing();
        let mut sampler = sampler(scratch.path(), incus.clone(), false);

        let sample = sampler.sample(1_000).clone();

        assert_eq!(sample.cpu.cores, 0, "unknown, never the local 4");
        assert_eq!(sample.cpu.used_pct, None);
        assert_eq!(sample.cpu.reason.as_deref(), Some(FAILED_REASON));
        assert_eq!(sample.load.reason.as_deref(), Some(REMOTE_REASON));
        assert_eq!(sample.memory.total_bytes, None);
        assert_eq!(sample.memory.available_bytes, None);
        assert_eq!(sample.memory.used_bytes, None);
        assert_eq!(sample.memory.reason.as_deref(), Some(FAILED_REASON));
        assert_eq!(sample.psi, no_psi());
        assert_eq!(sample.cpu_temp_c, None);
        assert_eq!(sample.gpu_busy_pct, None);
        assert_eq!(sample.network, None);
        assert_eq!(incus.resources_reads(), 1);
    }

    #[test]
    fn a_malformed_resources_document_is_unknown_not_a_zero_or_partial_host() {
        let scratch = Scratch::new("remote-malformed");
        write_first_tree(scratch.path());
        let incus = Resources::serving(json!({
            "cpu": { "total": 16 },
            "memory": { "total": 17_045_651_456u64 },
        }));
        let mut sampler = sampler(scratch.path(), incus, false);

        let sample = sampler.sample(1_000).clone();
        assert_eq!(sample.cpu.cores, 0);
        assert_eq!(sample.cpu.reason.as_deref(), Some(FAILED_REASON));
        assert_eq!(sample.memory.total_bytes, None);
        assert_eq!(sample.memory.used_bytes, None);
        assert_eq!(sample.memory.reason.as_deref(), Some(FAILED_REASON));
    }
}
