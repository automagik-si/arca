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

const INSTANCES: &str = "/1.0/instances?all-projects=true&recursion=1";

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
        },
        load: Load {
            one: None,
            five: None,
            fifteen: None,
        },
        memory: Memory {
            total_bytes: None,
            available_bytes: None,
            swap_total_bytes: None,
            swap_free_bytes: None,
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
            fifteen: Some(0.40)
        }
    );
    // 16000000, 8000000, 2000000 and 1500000 kB, each times 1024.
    assert_eq!(
        second.memory,
        Memory {
            total_bytes: Some(16_384_000_000),
            available_bytes: Some(8_192_000_000),
            swap_total_bytes: Some(2_048_000_000),
            swap_free_bytes: Some(1_536_000_000),
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
            swap_total_bytes: None,
            swap_free_bytes: None,
        }
    );
    assert_eq!(
        sample.load,
        Load {
            one: None,
            five: None,
            fifteen: None
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
            cores: 0
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
            cores: 2
        }
    );
}

#[test]
fn the_fake_incus_records_only_get_and_one_request_per_pool_plus_the_list() {
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
        3,
        "the instance list and one resources read per pool"
    );
    assert!(requests.iter().all(|r| r.method == "GET"), "{requests:?}");
    let mut paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            INSTANCES,
            "/1.0/storage-pools/default/resources",
            "/1.0/storage-pools/missing/resources",
        ]
    );
    let missing = requests
        .iter()
        .find(|r| r.path.contains("missing"))
        .expect("the missing pool was asked about");
    assert_eq!(missing.status, 404);

    assert_eq!(sample.pools.len(), 2, "one pool entry per configured name");
    assert_eq!(sample.pools[1].name, "missing");
    assert_eq!(
        sample.pools[1].used_bytes, None,
        "a pool Incus does not answer for reads null"
    );
    assert_eq!(sample.pools[1].free_bytes, None);
}

#[test]
fn a_pool_name_that_is_not_plain_is_never_sent_to_incus() {
    let scratch = Scratch::new("names");
    write_first_tree(scratch.path());
    let fake = FakeIncus::start(scratch.path(), fake_state()).expect("start the fake Incus");
    let mut sampler = sampler_for(
        scratch.path(),
        fake.socket(),
        &["x/../default", "default?all=1", ".."],
        5_000,
        60,
    );
    let sample = sampler.sample(1_000).clone();

    let paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        vec![INSTANCES.to_string()],
        "only the instance list was sent"
    );
    assert_eq!(sample.pools.len(), 3);
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
