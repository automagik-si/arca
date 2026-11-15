//! The power block of a host sample: the CPU packages' power from the powercap energy counters, the GPUs' power, and the
//! sum of what was measured with the words for what it leaves out.
//!
//! A CPU package's power is a rate: the change of its `energy_uj` counter between two samples over the time between them.
//! The counter wraps at `max_energy_range_uj`. Only a top-level zone named `package-<N>` counts: a sub-domain such as
//! `core` or `uncore` is part of its package already, a `dram` zone is memory, and `psys` is the whole platform. A second
//! interface to the same package (`intel-rapl-mmio`) is read once, through the plain one.
//!
//! A GPU's power is a reading, not a rate: an NVIDIA card's `power.draw` from the telemetry the GPU section already holds,
//! and for any other DRM card its hwmon `power1_input` (or `power1_average`), in microwatts.
//!
//! Every source without a figure carries a reason in plain words. The sum is never the machine's power at the wall.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gpu::{self, GpuSection, normalize_pci};
use crate::procfs::{is_card, read_text, sorted_names};
use crate::sample::REMOTE_HOST_REASON;

/// Why the CPU has no power figure: no powercap zone of a CPU package exists here.
pub const NO_CPU_COUNTER: &str = "no CPU power counter on this machine";

/// Why the GPUs have no power figure: no card with a power reading exists here.
pub const NO_GPU_COUNTER: &str = "no GPU power counter on this machine";

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

/// The words `notIncluded` carries when the CPU packages have no summed figure.
pub const CPU_NOT_INCLUDED: &str = "the CPU packages";

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
    /// The sum of every figure measured: the CPU packages' sum when it is known, and each GPU that has a figure. `None`
    /// when nothing was measured. It is not the machine's power at the wall.
    pub measured_power_w: Option<f64>,
    /// What `measured_power_w` does not hold, in words: always memory, disks, fans and the power supply's own loss, and
    /// each source here that has no figure.
    pub not_included: Vec<String>,
}

impl Default for Power {
    fn default() -> Power {
        assemble(
            CpuPower {
                power_w: None,
                reason: Some(NO_CPU_COUNTER.to_string()),
                packages: Vec::new(),
            },
            GpuPower {
                power_w: None,
                reason: Some(NO_GPU_COUNTER.to_string()),
                cards: Vec::new(),
            },
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
    /// The card's power at the sample, in watts.
    pub power_w: Option<f64>,
    pub reason: Option<String>,
}

impl GpuCardPower {
    /// The words that name this card in `notIncluded`.
    fn label(&self) -> String {
        let id = self
            .pci
            .clone()
            .or_else(|| self.card.clone())
            .unwrap_or_default();
        format!("the GPU {id}")
    }

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

/// The GPUs' power: each DRM card with a hwmon, and each NVIDIA card of the GPU section that hwmon does not already read.
pub(crate) fn gpu_power(sys_root: &Path, section: &GpuSection) -> GpuPower {
    let mut cards = hwmon_cards(sys_root);
    for card in &section.cards {
        if !card
            .driver
            .as_deref()
            .is_some_and(|driver| driver.eq_ignore_ascii_case("nvidia"))
            || cards
                .iter()
                .any(|known| known.pci.as_deref() == Some(card.pci.as_str()))
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
            power_w,
            reason,
        });
    }
    cards.sort_by(|a, b| (&a.pci, &a.card).cmp(&(&b.pci, &b.card)));
    gpu_total(cards)
}

/// The GPU block of `cards`: their sum when every card has a figure, and the reason otherwise.
fn gpu_total(cards: Vec<GpuCardPower>) -> GpuPower {
    if cards.is_empty() {
        return GpuPower {
            power_w: None,
            reason: Some(NO_GPU_COUNTER.to_string()),
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

/// Each `card<N>` under `class/drm` whose device has a hwmon directory, with the power its first hwmon gives.
fn hwmon_cards(sys_root: &Path) -> Vec<GpuCardPower> {
    let drm = sys_root.join("class").join("drm");
    sorted_names(&drm)
        .into_iter()
        .filter(|name| is_card(name))
        .filter_map(|name| {
            let device = drm.join(&name).join("device");
            let hwmon = device.join("hwmon");
            let monitors = sorted_names(&hwmon);
            if monitors.is_empty() {
                return None;
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
            let (power_w, reason) = match file {
                None => (None, Some(NO_CARD_COUNTER.to_string())),
                Some((monitor, file)) => {
                    let shown = format!("/sys/class/drm/{name}/device/hwmon/{monitor}/{file}");
                    match std::fs::read_to_string(hwmon.join(&monitor).join(file)) {
                        Ok(text) => match text.trim().parse::<u64>() {
                            Ok(microwatts) => (Some(microwatts as f64 / 1e6), None),
                            Err(_) => (None, Some(unreadable(&shown))),
                        },
                        Err(error) => (None, Some(read_error(&shown, &error))),
                    }
                }
            };
            Some(GpuCardPower {
                pci,
                card: Some(name),
                source: "hwmon".to_string(),
                power_w,
                reason,
            })
        })
        .collect()
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

/// The power block of a CPU and a GPU reading: the measured sum and what it leaves out.
pub(crate) fn assemble(cpu: CpuPower, gpu: GpuPower) -> Power {
    let figures = cpu
        .power_w
        .into_iter()
        .chain(gpu.cards.iter().filter_map(|card| card.power_w));
    let measured_power_w = figures.fold(None, |sum: Option<f64>, watts| {
        Some(sum.unwrap_or(0.0) + watts)
    });
    let not_included = not_included(&cpu, &gpu);
    Power {
        cpu,
        gpu,
        measured_power_w,
        not_included,
    }
}

fn not_included(cpu: &CpuPower, gpu: &GpuPower) -> Vec<String> {
    let mut words: Vec<String> = NOT_INCLUDED.iter().map(|word| word.to_string()).collect();
    if cpu.power_w.is_none() {
        words.push(CPU_NOT_INCLUDED.to_string());
    }
    words.extend(
        gpu.cards
            .iter()
            .filter(|card| card.power_w.is_none())
            .map(GpuCardPower::label),
    );
    words
}

/// The power block of a remote host: a local counter would describe the machine Arca runs on.
pub(crate) fn remote() -> Power {
    assemble(
        CpuPower {
            power_w: None,
            reason: Some(REMOTE_HOST_REASON.to_string()),
            packages: Vec::new(),
        },
        GpuPower {
            power_w: None,
            reason: Some(REMOTE_HOST_REASON.to_string()),
            cards: Vec::new(),
        },
    )
}

/// The mean power over a window. The packages and cards are the newest sample's, each averaged over the samples that hold a
/// figure for it; a sum is averaged over the samples that hold it. A reason is the newest sample's, and is dropped when the
/// mean has a figure. `measured_power_w` is averaged over the samples that hold it, so it can differ from the sum of the
/// averaged parts when a source was missing in part of the window. `not_included` is worked out from the mean's figures.
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
                zone: package.zone.clone(),
                name: package.name.clone(),
                power_w: figure,
                reason: keep(figure, &package.reason),
            }
        })
        .collect();
    let cpu_w = average(
        samples
            .iter()
            .filter_map(|power| power.cpu.power_w)
            .collect(),
    );
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
    let gpu_w = average(
        samples
            .iter()
            .filter_map(|power| power.gpu.power_w)
            .collect(),
    );
    let cpu = CpuPower {
        power_w: cpu_w,
        reason: keep(cpu_w, &last.cpu.reason),
        packages,
    };
    let gpu = GpuPower {
        power_w: gpu_w,
        reason: keep(gpu_w, &last.gpu.reason),
        cards,
    };
    let not_included = not_included(&cpu, &gpu);
    Some(Power {
        cpu,
        gpu,
        measured_power_w: average(
            samples
                .iter()
                .filter_map(|power| power.measured_power_w)
                .collect(),
        ),
        not_included,
    })
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

    #[test]
    fn a_pci_address_is_told_from_a_directory_name() {
        assert!(looks_like_pci("0000:c6:00.0"));
        assert!(looks_like_pci("c6:00.0"));
        assert!(!looks_like_pci("device"));
        assert!(!looks_like_pci("simple-framebuffer.0"));
        assert!(!looks_like_pci("0000:c6:00"));
    }
}
