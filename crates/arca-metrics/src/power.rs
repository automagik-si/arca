//! The power block of a host sample: the CPU packages' power from the powercap energy counters, the GPUs' own power, the
//! whole-chip sensors, and the sum of what was measured with the words for what it leaves out and what it does not add.
//!
//! A CPU package's power is a rate: the change of its `energy_uj` counter between two samples over the time between them.
//! The counter wraps at `max_energy_range_uj`. Only a top-level zone named `package-<N>` counts: a sub-domain such as
//! `core` or `uncore` is part of its package already, a `dram` zone is memory, and `psys` is the whole platform. A second
//! interface to the same package (`intel-rapl-mmio`) is read once, through the plain one.
//!
//! A DRM card's hwmon power (`power1_input`, or `power1_average`, in microwatts) is a reading, not a rate. For amdgpu it is
//! "power used by the SoC", and "on APUs this includes the CPU" (the kernel's amdgpu hwmon documentation), so an amdgpu
//! sensor is classified from what the driver shows beside it ([`classify_amdgpu`]): a whole-chip sensor goes in `chip`,
//! never in `gpu`; a discrete card's own sensor goes in `gpu`; a sensor the driver gives no certain sign for goes in `chip`
//! as not certain and is never added to the sum. An NVIDIA card's figure is its telemetry's `power.draw`.
//!
//! The sum never counts a figure twice: a whole-chip sensor holds the CPU packages, so when both have figures the sum holds
//! the chip and names the packages as measured but not added. Every source without a figure carries a reason in plain
//! words. The sum is never the machine's power at the wall.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gpu::{self, GpuSection, normalize_pci};
use crate::procfs::{is_card, read_text, sorted_names};
use crate::sample::REMOTE_HOST_REASON;

/// Why the CPU has no power figure: no powercap zone of a CPU package exists here.
pub const NO_CPU_COUNTER: &str = "no CPU power counter on this machine";

/// Why the GPUs have no power figure: no card with a power reading of its own exists here.
pub const NO_GPU_COUNTER: &str = "no GPU power counter on this machine";

/// Why the GPUs have no figure of their own: the integrated GPU is measured with the whole chip, in `chip`.
pub const GPU_IN_CHIP: &str =
    "the integrated GPU is measured with the whole chip, in the chip sensors";

/// Why the GPUs have no figure of their own: a GPU's sensor is listed in `chip`, not certain to be the card's alone.
pub const GPU_NOT_CLASSIFIED: &str = "the GPU's sensor is listed in the chip sensors and not counted as a GPU: this machine does not tell whether it includes the CPU";

/// Why `chip` has no figure: no whole-chip sensor exists here.
pub const NO_CHIP_SENSOR: &str = "no whole-chip power sensor on this machine";

/// Why a package has no figure yet: a rate needs a second reading.
pub const ONE_SAMPLE: &str = "only one sample so far; a rate needs two";

/// Why a package has no figure: its counter is lower than at the previous sample and the range it wraps at is unknown.
pub const COUNTER_WENT_BACK: &str =
    "the counter went backwards and the range it wraps at is not known";

/// Why a package has no figure: the sample time did not move forward.
pub const NO_TIME: &str = "no time passed since the previous sample";

/// Why a DRM card has no figure: its hwmon gives no power file.
pub const NO_CARD_COUNTER: &str = "this card's driver gives no power counter";

/// Why an NVIDIA card has no figure: its telemetry was read, without `power.draw`.
pub const NVIDIA_NO_POWER: &str = "nvidia-smi gave no power figure for this card";

/// What the measured sum never holds, in words.
pub const NOT_INCLUDED: [&str; 4] = ["memory", "disks", "fans", "the power supply's own loss"];

/// The words `notIncluded` carries when the CPU packages are not in the sum, alone or inside the chip.
pub const CPU_NOT_INCLUDED: &str = "the CPU packages";

/// Why an amdgpu sensor is the whole chip's: the driver shows the `in1` (vddnb) voltage only on an APU.
pub const BASIS_APU: &str = "the amdgpu hwmon shows in1 (vddnb), which the driver shows only on an APU, and on an APU this sensor includes the CPU";

/// Why an amdgpu sensor is the card's own: the driver hides fan controls on every APU.
pub const BASIS_FAN: &str =
    "the amdgpu hwmon shows fan controls, which the driver hides on every APU: a discrete card";

/// Why an amdgpu sensor is the card's own: the driver hides `pcie_bw` on every APU.
pub const BASIS_PCIE_BW: &str =
    "the device shows pcie_bw, which the driver hides on every APU: a discrete card";

/// Why an amdgpu sensor is not certain: the driver shows no sign either way.
pub const BASIS_NO_SIGN: &str = "the amdgpu hwmon shows neither an APU sign (in1, vddnb) nor a discrete sign (fan controls, pcie_bw), so it may include the CPU";

/// Why an amdgpu sensor is not certain: the driver shows signs both ways.
pub const BASIS_BOTH_SIGNS: &str = "the amdgpu hwmon shows both an APU sign (in1, vddnb) and a discrete sign (fan controls or pcie_bw), so it may include the CPU";

/// Why a sensor of another driver is the card's own: only amdgpu is known to report a whole chip here.
pub const BASIS_OTHER_DRIVER: &str = "not an amdgpu sensor: read as the card's own";

/// The reason for a counter this process may not read. Most kernels make the powercap counters readable by root only.
pub fn refused(path: &str) -> String {
    format!(
        "the counter {path} is there, and this process may not read it: most kernels let only root read it"
    )
}

/// The reason for a counter that is there but did not read as a number.
pub fn unreadable(path: &str) -> String {
    format!("the counter {path} could not be read")
}

/// The reason for an I/O error on the counter at `path`, as the file is named on a host.
pub fn read_error(path: &str, error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => refused(path),
        _ => unreadable(path),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Power {
    pub cpu: CpuPower,
    pub gpu: GpuPower,
    pub chip: ChipPower,
    /// The sum of what was measured, each part once: every GPU's own figure, and either the whole-chip sensors (when all
    /// of them have a figure) or else the CPU packages' sum. `None` when nothing was added. It is not the machine's power
    /// at the wall.
    pub measured_power_w: Option<f64>,
    /// What was measured and is not in the sum, because it overlaps with what is, in words.
    pub not_added: Vec<String>,
    /// What the sum does not hold, in words: always memory, disks, fans and the power supply's own loss, and each part of
    /// the machine here whose power is in no added figure. It never names a part the sum holds.
    pub not_included: Vec<String>,
}

impl Default for Power {
    fn default() -> Power {
        assemble(
            CpuPower {
                power_w: None,
                reason: Some(NO_CPU_COUNTER.to_string()),
                added: false,
                packages: Vec::new(),
            },
            gpu_total(Vec::new(), NO_GPU_COUNTER),
            chip_total(Vec::new()),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuPower {
    /// The sum of the packages, in watts. `None` unless every package has a figure: a part of the CPU is not the CPU.
    pub power_w: Option<f64>,
    /// Why `power_w` is `None`: the first package's reason, or that no counter exists.
    pub reason: Option<String>,
    /// Whether `power_w` is in `measured_power_w`. False when a whole-chip sensor holds the packages and is added instead.
    #[serde(default)]
    pub added: bool,
    pub packages: Vec<PackagePower>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackagePower {
    /// The powercap zone, such as `intel-rapl:0`.
    pub zone: String,
    /// The zone's own name, such as `package-0`.
    pub name: String,
    /// The package's power between the previous sample and this one, in watts.
    pub power_w: Option<f64>,
    pub reason: Option<String>,
}

/// The GPUs' own power: discrete cards. A whole-chip sensor is never here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuPower {
    /// The sum of the cards, in watts. `None` unless every card has a figure.
    pub power_w: Option<f64>,
    pub reason: Option<String>,
    pub cards: Vec<GpuCardPower>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCardPower {
    /// The card's PCI address, when it is known.
    pub pci: Option<String>,
    /// The DRM card, such as `card1`, for a card read through hwmon.
    pub card: Option<String>,
    /// Where the figure comes from: `nvidia-smi` or `hwmon`.
    pub source: String,
    /// For a hwmon card, why its sensor is read as the card's own, in words.
    #[serde(default)]
    pub basis: Option<String>,
    /// The card's power at the sample, in watts.
    pub power_w: Option<f64>,
    pub reason: Option<String>,
}

/// The sensors of a whole chip (the CPU, its integrated GPU and the rest of the chip), and the sensors that may be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChipPower {
    /// The sum of the whole-chip sensors (`wholeChip` true), in watts. `None` unless each has a figure.
    pub power_w: Option<f64>,
    pub reason: Option<String>,
    /// Whether `power_w` is in `measured_power_w`.
    pub added: bool,
    pub sensors: Vec<ChipSensor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChipSensor {
    pub pci: Option<String>,
    /// The DRM card the sensor hangs under, such as `card1`.
    pub card: Option<String>,
    /// The sensor's own label, such as `PPT`.
    pub label: Option<String>,
    /// True when the driver shows for certain that the sensor measures the whole chip; false when it does not tell, and
    /// the sensor is then never added.
    pub whole_chip: bool,
    /// What the classification stands on, in words.
    pub basis: String,
    pub power_w: Option<f64>,
    pub reason: Option<String>,
}

fn id_of(pci: &Option<String>, card: &Option<String>) -> String {
    pci.clone().or_else(|| card.clone()).unwrap_or_default()
}

impl GpuCardPower {
    fn key(&self) -> (&str, Option<&str>, Option<&str>) {
        (
            self.source.as_str(),
            self.pci.as_deref(),
            self.card.as_deref(),
        )
    }
}

/// One package counter as a sample read it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PackageCounter {
    zone: String,
    name: String,
    /// The counter in microjoules, or why it was not read.
    energy: Result<u64, String>,
    /// `max_energy_range_uj`, where the counter wraps.
    range: Option<u64>,
}

/// The package counters under `class/powercap`, in zone order. A zone with no `energy_uj` file is no counter. An empty
/// list means no counter exists here.
pub(crate) fn read_packages(sys_root: &Path) -> Vec<PackageCounter> {
    let powercap = sys_root.join("class").join("powercap");
    let mut found: Vec<(String, String, String)> = Vec::new();
    for zone in sorted_names(&powercap) {
        let Some((kind, index)) = zone.split_once(':') else {
            continue;
        };
        // `intel-rapl:0` is a top-level zone; `intel-rapl:0:0` is a sub-domain of it.
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(name) = read_text(&powercap.join(&zone).join("name")) else {
            continue;
        };
        let name = name.trim().to_string();
        if !name.starts_with("package-") || !powercap.join(&zone).join("energy_uj").exists() {
            continue;
        }
        found.push((kind.to_string(), zone, name));
    }
    // One zone per package: the plain interface before an `-mmio` one.
    found.sort_by(|a, b| {
        (&a.2, a.0.ends_with("-mmio"), &a.1).cmp(&(&b.2, b.0.ends_with("-mmio"), &b.1))
    });
    found.dedup_by(|later, earlier| later.2 == earlier.2);
    found.sort_by(|a, b| a.1.cmp(&b.1));
    found
        .into_iter()
        .map(|(_, zone, name)| {
            let base = powercap.join(&zone);
            let shown = format!("/sys/class/powercap/{zone}/energy_uj");
            let energy = match std::fs::read_to_string(base.join("energy_uj")) {
                Ok(text) => text.trim().parse::<u64>().map_err(|_| unreadable(&shown)),
                Err(error) => Err(read_error(&shown, &error)),
            };
            let range = read_text(&base.join("max_energy_range_uj"))
                .and_then(|text| text.trim().parse::<u64>().ok());
            PackageCounter {
                zone,
                name,
                energy,
                range,
            }
        })
        .collect()
}

/// The packages' power between `before` (its sample time and counters) and `now`.
pub(crate) fn cpu_power(
    before: Option<(u64, &[PackageCounter])>,
    now: &[PackageCounter],
    now_ms: u64,
) -> CpuPower {
    if now.is_empty() {
        return CpuPower {
            power_w: None,
            reason: Some(NO_CPU_COUNTER.to_string()),
            added: false,
            packages: Vec::new(),
        };
    }
    let packages: Vec<PackagePower> = now
        .iter()
        .map(|counter| {
            let earlier = before.and_then(|(at, list)| {
                list.iter()
                    .find(|old| old.zone == counter.zone)
                    .and_then(|old| old.energy.as_ref().ok().map(|energy| (at, *energy)))
            });
            let (power_w, reason) = match (&counter.energy, earlier) {
                (Err(why), _) => (None, Some(why.clone())),
                (Ok(_), None) => (None, Some(ONE_SAMPLE.to_string())),
                (Ok(energy), Some((at, old))) => {
                    match rate(old, *energy, counter.range, at, now_ms) {
                        Ok(watts) => (Some(watts), None),
                        Err(why) => (None, Some(why.to_string())),
                    }
                }
            };
            PackagePower {
                zone: counter.zone.clone(),
                name: counter.name.clone(),
                power_w,
                reason,
            }
        })
        .collect();
    let power_w = packages
        .iter()
        .map(|package| package.power_w)
        .sum::<Option<f64>>();
    let reason = match power_w {
        Some(_) => None,
        None => packages.iter().find_map(|package| package.reason.clone()),
    };
    CpuPower {
        power_w,
        reason,
        added: false,
        packages,
    }
}

/// Watts from two counter readings in microjoules, `before_ms` and `now_ms` apart. A counter lower than before wrapped at
/// `range`, unless `range` is unknown or below the earlier reading.
fn rate(
    before: u64,
    now: u64,
    range: Option<u64>,
    before_ms: u64,
    now_ms: u64,
) -> Result<f64, &'static str> {
    let elapsed_ms = now_ms
        .checked_sub(before_ms)
        .filter(|ms| *ms > 0)
        .ok_or(NO_TIME)?;
    let delta = if now >= before {
        now - before
    } else {
        match range {
            Some(range) if before <= range => range - before + now,
            _ => return Err(COUNTER_WENT_BACK),
        }
    };
    Ok(delta as f64 / 1e6 / (elapsed_ms as f64 / 1000.0))
}

/// What an amdgpu hwmon sensor measures, by what the driver shows beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorScope {
    /// The whole chip: the CPU, the integrated GPU and the rest of the chip.
    WholeChip,
    /// A discrete card's own.
    Card,
    /// Not certain: it may include the CPU.
    Unknown,
}

/// Classifies an amdgpu hwmon power sensor, with the words it stands on. The kernel's amdgpu hwmon documentation says
/// `power1_input`/`power1_average` is "power used by the SoC" and "on APUs this includes the CPU"; the label (`PPT`) is the
/// same on an APU and a discrete card, so it decides nothing. What decides is what the driver's visibility rules
/// (`hwmon_attributes_visible` and `default_attr_update` in `drivers/gpu/drm/amd/pm/amdgpu_pm.c`) show:
///
/// - `in1_input`/`in1_label` (vddnb) are hidden unless the device is an APU (and not a multi-die part): an APU.
/// - The fan attributes (`fan1_input`, `pwm1`, ...) are hidden on every APU: a discrete card.
/// - The device's `pcie_bw` is hidden on every APU ("PCIe Perf counters won't work on APU nodes"): a discrete card.
///
/// No sign, or signs both ways, is [`SensorScope::Unknown`]: a multi-die APU shows none of the three, and so does a
/// discrete card without fan control or PCIe counters.
pub fn classify_amdgpu(hwmon: &Path, device: &Path) -> (SensorScope, &'static str) {
    let apu = hwmon.join("in1_input").exists() || hwmon.join("in1_label").exists();
    let fan = ["fan1_input", "pwm1", "pwm1_enable", "fan1_target"]
        .iter()
        .any(|file| hwmon.join(file).exists());
    let pcie_bw = device.join("pcie_bw").exists();
    match (apu, fan || pcie_bw) {
        (true, false) => (SensorScope::WholeChip, BASIS_APU),
        (false, true) if fan => (SensorScope::Card, BASIS_FAN),
        (false, true) => (SensorScope::Card, BASIS_PCIE_BW),
        (true, true) => (SensorScope::Unknown, BASIS_BOTH_SIGNS),
        (false, false) => (SensorScope::Unknown, BASIS_NO_SIGN),
    }
}

/// The GPUs' own power and the chip sensors: each DRM card with a hwmon, classified, and each NVIDIA card of the GPU
/// section that hwmon does not already read.
pub(crate) fn devices(sys_root: &Path, section: &GpuSection) -> (GpuPower, ChipPower) {
    let (mut cards, sensors) = hwmon_sensors(sys_root);
    for card in &section.cards {
        let seen = |pci: &Option<String>| pci.as_deref() == Some(card.pci.as_str());
        if !card
            .driver
            .as_deref()
            .is_some_and(|driver| driver.eq_ignore_ascii_case("nvidia"))
            || cards.iter().any(|known| seen(&known.pci))
            || sensors.iter().any(|known| seen(&known.pci))
        {
            continue;
        }
        let (power_w, reason) = match &card.telemetry {
            Some(telemetry) => match telemetry.power_draw_w {
                Some(watts) => (Some(watts), None),
                None => (None, Some(NVIDIA_NO_POWER.to_string())),
            },
            None => (
                None,
                Some(
                    card.reason
                        .clone()
                        .unwrap_or_else(|| gpu::NO_READING_YET.to_string()),
                ),
            ),
        };
        cards.push(GpuCardPower {
            pci: Some(card.pci.clone()),
            card: None,
            source: "nvidia-smi".to_string(),
            basis: None,
            power_w,
            reason,
        });
    }
    cards.sort_by(|a, b| (&a.pci, &a.card).cmp(&(&b.pci, &b.card)));
    let chip = chip_total(sensors);
    let no_card = if chip.sensors.iter().any(|sensor| sensor.whole_chip) {
        GPU_IN_CHIP
    } else if chip.sensors.is_empty() {
        NO_GPU_COUNTER
    } else {
        GPU_NOT_CLASSIFIED
    };
    (gpu_total(cards, no_card), chip)
}

/// The GPU block of `cards`: their sum when every card has a figure, and the reason otherwise. With no card, the reason
/// is `no_card`: whether a GPU is measured in the chip sensors, certain or not, or there is no counter at all.
fn gpu_total(cards: Vec<GpuCardPower>, no_card: &str) -> GpuPower {
    if cards.is_empty() {
        return GpuPower {
            power_w: None,
            reason: Some(no_card.to_string()),
            cards,
        };
    }
    let power_w = cards.iter().map(|card| card.power_w).sum::<Option<f64>>();
    let reason = match power_w {
        Some(_) => None,
        None => cards.iter().find_map(|card| card.reason.clone()),
    };
    GpuPower {
        power_w,
        reason,
        cards,
    }
}

/// The chip block of `sensors`: the whole-chip sensors' sum when each has a figure. `added` is set by [`assemble`].
fn chip_total(sensors: Vec<ChipSensor>) -> ChipPower {
    let certain: Vec<&ChipSensor> = sensors.iter().filter(|sensor| sensor.whole_chip).collect();
    let (power_w, reason) = if certain.is_empty() {
        let reason = match sensors.first() {
            Some(sensor) => sensor.basis.to_string(),
            None => NO_CHIP_SENSOR.to_string(),
        };
        (None, Some(reason))
    } else {
        let power_w = certain
            .iter()
            .map(|sensor| sensor.power_w)
            .sum::<Option<f64>>();
        let reason = match power_w {
            Some(_) => None,
            None => certain.iter().find_map(|sensor| sensor.reason.clone()),
        };
        (power_w, reason)
    };
    ChipPower {
        power_w,
        reason,
        added: false,
        sensors,
    }
}

/// Each `card<N>` under `class/drm` whose device has a hwmon directory, with the power its first hwmon gives: a discrete
/// card's own sensor as a GPU card, any other as a chip sensor.
fn hwmon_sensors(sys_root: &Path) -> (Vec<GpuCardPower>, Vec<ChipSensor>) {
    let drm = sys_root.join("class").join("drm");
    let mut cards = Vec::new();
    let mut sensors = Vec::new();
    for name in sorted_names(&drm).into_iter().filter(|name| is_card(name)) {
        let device = drm.join(&name).join("device");
        let hwmon = device.join("hwmon");
        let monitors = sorted_names(&hwmon);
        if monitors.is_empty() {
            continue;
        }
        let pci = std::fs::canonicalize(&device)
            .ok()
            .and_then(|path| path.file_name()?.to_str().map(str::to_string))
            .filter(|base| looks_like_pci(base))
            .map(|base| normalize_pci(&base));
        let file = monitors.iter().find_map(|monitor| {
            ["power1_input", "power1_average"]
                .into_iter()
                .find(|file| hwmon.join(monitor).join(file).exists())
                .map(|file| (monitor.clone(), file))
        });
        // The hwmon that holds the power file, or the first one, says which driver it is and what it measures.
        let monitor = file
            .as_ref()
            .map(|(monitor, _)| monitor.clone())
            .unwrap_or_else(|| monitors[0].clone());
        let base = hwmon.join(&monitor);
        let (power_w, reason) = match &file {
            None => (None, Some(NO_CARD_COUNTER.to_string())),
            Some((monitor, file)) => {
                let shown = format!("/sys/class/drm/{name}/device/hwmon/{monitor}/{file}");
                match std::fs::read_to_string(base.join(file)) {
                    Ok(text) => match text.trim().parse::<u64>() {
                        Ok(microwatts) => (Some(microwatts as f64 / 1e6), None),
                        Err(_) => (None, Some(unreadable(&shown))),
                    },
                    Err(error) => (None, Some(read_error(&shown, &error))),
                }
            }
        };
        let driver = read_text(&base.join("name")).map(|text| text.trim().to_string());
        let (scope, basis) = if driver.as_deref() == Some("amdgpu") {
            classify_amdgpu(&base, &device)
        } else {
            (SensorScope::Card, BASIS_OTHER_DRIVER)
        };
        if scope == SensorScope::Card {
            cards.push(GpuCardPower {
                pci,
                card: Some(name),
                source: "hwmon".to_string(),
                basis: Some(basis.to_string()),
                power_w,
                reason,
            });
        } else {
            sensors.push(ChipSensor {
                pci,
                card: Some(name),
                label: read_text(&base.join("power1_label")).map(|text| text.trim().to_string()),
                whole_chip: scope == SensorScope::WholeChip,
                basis: basis.to_string(),
                power_w,
                reason,
            });
        }
    }
    (cards, sensors)
}

/// `0000:c6:00.0` or `c6:00.0`: hex groups split by colons, ending in a dot and the function.
fn looks_like_pci(text: &str) -> bool {
    let Some((head, function)) = text.rsplit_once('.') else {
        return false;
    };
    let groups: Vec<&str> = head.split(':').collect();
    (2..=3).contains(&groups.len())
        && groups
            .iter()
            .all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_hexdigit()))
        && !function.is_empty()
        && function.bytes().all(|b| b.is_ascii_digit())
}

/// The power block: which figures the sum holds, the sum, what was measured and not added, and what is not included.
///
/// The sum holds every GPU's own figure. For the CPU it holds the whole-chip sensors when each has a figure, since they
/// hold the CPU packages and the integrated GPU and the rest of the chip besides; only otherwise the CPU packages' sum. A
/// sensor that may or may not include the CPU is never added. So no figure is counted twice: at worst one is left out,
/// and `not_added` and `not_included` say so.
pub(crate) fn assemble(mut cpu: CpuPower, gpu: GpuPower, mut chip: ChipPower) -> Power {
    let certain: Vec<&ChipSensor> = chip.sensors.iter().filter(|s| s.whole_chip).collect();
    let chip_ids = certain
        .iter()
        .map(|sensor| id_of(&sensor.pci, &sensor.card))
        .collect::<Vec<_>>()
        .join(", ");
    chip.added = chip.power_w.is_some();
    cpu.added = !chip.added && cpu.power_w.is_some();
    let mut not_added = Vec::new();
    if chip.added && cpu.power_w.is_some() {
        not_added.push(format!(
            "the CPU packages: measured, not added, because the whole-chip sensor at {chip_ids} holds them"
        ));
    }
    if !chip.added {
        for sensor in certain.iter().filter(|sensor| sensor.power_w.is_some()) {
            not_added.push(format!(
                "the whole-chip sensor at {}: measured, not added, because not every whole-chip sensor has a figure",
                id_of(&sensor.pci, &sensor.card)
            ));
        }
    }
    for sensor in chip
        .sensors
        .iter()
        .filter(|s| !s.whole_chip && s.power_w.is_some())
    {
        not_added.push(format!(
            "the sensor at {}: measured, not added, because this machine does not tell whether it includes the CPU",
            id_of(&sensor.pci, &sensor.card)
        ));
    }
    let mut not_included: Vec<String> = NOT_INCLUDED.iter().map(|w| w.to_string()).collect();
    if !chip.added && !cpu.added {
        not_included.push(CPU_NOT_INCLUDED.to_string());
    }
    if !chip.added {
        for sensor in &certain {
            not_included.push(format!(
                "the integrated GPU and the rest of the chip at {}",
                id_of(&sensor.pci, &sensor.card)
            ));
        }
    }
    for sensor in chip.sensors.iter().filter(|s| !s.whole_chip) {
        not_included.push(format!(
            "the device at {}, whose sensor is not added",
            id_of(&sensor.pci, &sensor.card)
        ));
    }
    for card in gpu.cards.iter().filter(|card| card.power_w.is_none()) {
        not_included.push(format!("the GPU {}", id_of(&card.pci, &card.card)));
    }
    let added = gpu
        .cards
        .iter()
        .filter_map(|card| card.power_w)
        .chain(chip.added.then_some(chip.power_w).flatten())
        .chain(cpu.added.then_some(cpu.power_w).flatten());
    let measured_power_w = added.fold(None, |sum: Option<f64>, watts| {
        Some(sum.unwrap_or(0.0) + watts)
    });
    Power {
        cpu,
        gpu,
        chip,
        measured_power_w,
        not_added,
        not_included,
    }
}

/// The power block of a remote host: a local counter would describe the machine Arca runs on.
pub(crate) fn remote() -> Power {
    let reason = Some(REMOTE_HOST_REASON.to_string());
    assemble(
        CpuPower {
            power_w: None,
            reason: reason.clone(),
            added: false,
            packages: Vec::new(),
        },
        GpuPower {
            power_w: None,
            reason: reason.clone(),
            cards: Vec::new(),
        },
        ChipPower {
            power_w: None,
            reason,
            added: false,
            sensors: Vec::new(),
        },
    )
}

/// The mean power over a window. The packages, cards and chip sensors are the newest sample's, each averaged over the
/// samples that hold a figure for it, and each section's sum is averaged over the samples that hold it. A reason is the
/// newest sample's, kept only when the mean has no figure. The sum, `not_added` and `not_included` are then worked out
/// from those means by the same rule as one sample's, so they always agree with each other.
pub(crate) fn mean_power(samples: &[&Power]) -> Option<Power> {
    let last = *samples.last()?;
    let average = |values: Vec<f64>| -> Option<f64> {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let keep = |figure: Option<f64>, reason: &Option<String>| match figure {
        Some(_) => None,
        None => reason.clone(),
    };
    let packages = last
        .cpu
        .packages
        .iter()
        .map(|package| {
            let figure = average(
                samples
                    .iter()
                    .flat_map(|power| power.cpu.packages.iter())
                    .filter(|other| other.zone == package.zone)
                    .filter_map(|other| other.power_w)
                    .collect(),
            );
            PackagePower {
                power_w: figure,
                reason: keep(figure, &package.reason),
                ..package.clone()
            }
        })
        .collect();
    let cards = last
        .gpu
        .cards
        .iter()
        .map(|card| {
            let figure = average(
                samples
                    .iter()
                    .flat_map(|power| power.gpu.cards.iter())
                    .filter(|other| other.key() == card.key())
                    .filter_map(|other| other.power_w)
                    .collect(),
            );
            GpuCardPower {
                power_w: figure,
                reason: keep(figure, &card.reason),
                ..card.clone()
            }
        })
        .collect();
    let sensors = last
        .chip
        .sensors
        .iter()
        .map(|sensor| {
            let figure = average(
                samples
                    .iter()
                    .flat_map(|power| power.chip.sensors.iter())
                    .filter(|other| other.pci == sensor.pci && other.card == sensor.card)
                    .filter_map(|other| other.power_w)
                    .collect(),
            );
            ChipSensor {
                power_w: figure,
                reason: keep(figure, &sensor.reason),
                ..sensor.clone()
            }
        })
        .collect();
    let section = |pick: &dyn Fn(&Power) -> Option<f64>| {
        average(samples.iter().filter_map(|power| pick(power)).collect())
    };
    let cpu_w = section(&|power| power.cpu.power_w);
    let gpu_w = section(&|power| power.gpu.power_w);
    let chip_w = section(&|power| power.chip.power_w);
    Some(assemble(
        CpuPower {
            power_w: cpu_w,
            reason: keep(cpu_w, &last.cpu.reason),
            added: false,
            packages,
        },
        GpuPower {
            power_w: gpu_w,
            reason: keep(gpu_w, &last.gpu.reason),
            cards,
        },
        ChipPower {
            power_w: chip_w,
            reason: keep(chip_w, &last.chip.reason),
            added: false,
            sensors,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permission_error_names_the_file_and_root_any_other_error_says_it_could_not_be_read() {
        let path = "/sys/class/powercap/intel-rapl:0/energy_uj";
        let refused = read_error(path, &io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(refused.contains(path), "{refused}");
        assert!(refused.contains("may not read it"), "{refused}");
        assert!(refused.contains("root"), "{refused}");
        let other = read_error(path, &io::Error::from(io::ErrorKind::InvalidData));
        assert_eq!(other, unreadable(path));
    }

    #[test]
    fn a_rate_wraps_at_its_range_and_refuses_a_backward_counter_without_one() {
        assert_eq!(rate(1_000_000, 51_000_000, None, 0, 5_000), Ok(10.0));
        assert_eq!(rate(90, 40, Some(100), 0, 1_000), Ok(50.0 / 1e6));
        assert_eq!(rate(90, 40, None, 0, 1_000), Err(COUNTER_WENT_BACK));
        assert_eq!(rate(90, 40, Some(80), 0, 1_000), Err(COUNTER_WENT_BACK));
        assert_eq!(rate(1, 2, None, 5_000, 5_000), Err(NO_TIME));
        assert_eq!(rate(1, 2, None, 6_000, 5_000), Err(NO_TIME));
    }

    fn sensor(root: &Path, files: &[&str]) {
        std::fs::create_dir_all(root).expect("a dir");
        for file in files {
            std::fs::write(root.join(file), "1\n").expect("a file");
        }
    }

    #[test]
    fn an_amdgpu_sensor_is_told_by_the_signs_the_driver_shows_and_otherwise_is_not_certain() {
        let root = std::env::temp_dir().join(format!("arca-classify-{}", std::process::id()));
        // A tag, the hwmon's sign files, the device's sign files, and the expected scope and basis.
        type Case<'a> = (&'a str, &'a [&'a str], &'a [&'a str], SensorScope, &'a str);
        let cases: [Case; 6] = [
            (
                "apu",
                &["in1_input", "in1_label"],
                &[],
                SensorScope::WholeChip,
                BASIS_APU,
            ),
            ("fan", &["fan1_input"], &[], SensorScope::Card, BASIS_FAN),
            ("pwm", &["pwm1"], &[], SensorScope::Card, BASIS_FAN),
            ("pcie", &[], &["pcie_bw"], SensorScope::Card, BASIS_PCIE_BW),
            ("none", &[], &[], SensorScope::Unknown, BASIS_NO_SIGN),
            (
                "both",
                &["in1_input"],
                &["pcie_bw"],
                SensorScope::Unknown,
                BASIS_BOTH_SIGNS,
            ),
        ];
        for (tag, hwmon_files, device_files, scope, basis) in cases {
            let device = root.join(tag);
            let hwmon = device.join("hwmon").join("hwmon0");
            sensor(&hwmon, &["power1_input", "power1_label"]);
            sensor(&hwmon, hwmon_files);
            sensor(&device, device_files);
            assert_eq!(classify_amdgpu(&hwmon, &device), (scope, basis), "{tag}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pci_address_is_told_from_a_directory_name() {
        assert!(looks_like_pci("0000:c6:00.0"));
        assert!(looks_like_pci("c6:00.0"));
        assert!(!looks_like_pci("device"));
        assert!(!looks_like_pci("simple-framebuffer.0"));
        assert!(!looks_like_pci("0000:c6:00"));
    }
}
