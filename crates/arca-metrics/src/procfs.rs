//! The host readings from procfs and sysfs. Each reader takes the root of a tree, so a test can point it at a generated one.
//! A file that is missing or does not parse gives `None` for its field, and nothing here panics.
//!
//! The rules are the ones the droidd host sampler applies: CPU busy is the first eight fields of the aggregate
//! `cpu` line less idle and iowait; a NIC is physical when `class/net/<name>/device` exists; the temperature is the `k10temp`
//! hwmon's `temp1_input` in millidegrees; the iGPU figure is the first `card<N>` in name order that has
//! `device/gpu_busy_percent`.

use std::fs;
use std::path::Path;

use crate::sample::{Load, Memory, Psi};

/// The aggregate CPU counters, in jiffies, over the first eight fields of the `cpu` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTotals {
    /// Every counted jiffy that is neither idle nor iowait.
    pub busy: u64,
    /// Every counted jiffy.
    pub total: u64,
}

/// What `/proc/stat` gives: the aggregate counters (`None` when that line does not parse) and the numbered CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuReading {
    pub totals: Option<CpuTotals>,
    pub cores: u64,
}

/// The byte counters of the physical NICs, summed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NicTotals {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub fn read_cpu(proc_root: &Path) -> CpuReading {
    parse_stat(&read_text(&proc_root.join("stat")).unwrap_or_default())
}

/// The local host name, from `sys/kernel/hostname`. `None` when it is missing or empty.
pub fn read_hostname(proc_root: &Path) -> Option<String> {
    let text = read_text(&proc_root.join("sys/kernel/hostname"))?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Node uptime in epoch-independent milliseconds, from the first field of `uptime`. Missing or invalid data is unknown.
pub fn read_uptime_ms(proc_root: &Path) -> Option<u64> {
    let text = read_text(&proc_root.join("uptime"))?;
    parse_uptime_ms(text.split_whitespace().next()?)
}

fn parse_uptime_ms(seconds: &str) -> Option<u64> {
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, "0"));
    if whole.is_empty()
        || fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let millis = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(3)
        .fold(0u64, |value, digit| value * 10 + u64::from(digit - b'0'));
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)?
        .checked_add(millis)
}

pub fn read_load(proc_root: &Path) -> Load {
    let text = read_text(&proc_root.join("loadavg")).unwrap_or_default();
    let mut values = text.split_whitespace().map(parse_finite);
    Load {
        one: values.next().flatten(),
        five: values.next().flatten(),
        fifteen: values.next().flatten(),
        reason: None,
    }
}

pub fn read_memory(proc_root: &Path) -> Memory {
    let text = read_text(&proc_root.join("meminfo")).unwrap_or_default();
    let total_bytes = meminfo_bytes(&text, "MemTotal");
    let available_bytes = meminfo_bytes(&text, "MemAvailable");
    Memory {
        total_bytes,
        available_bytes,
        used_bytes: total_bytes
            .zip(available_bytes)
            .map(|(total, available)| total.saturating_sub(available)),
        swap_total_bytes: meminfo_bytes(&text, "SwapTotal"),
        swap_free_bytes: meminfo_bytes(&text, "SwapFree"),
        reason: None,
    }
}

pub fn read_psi(proc_root: &Path) -> Psi {
    let memory = read_text(&proc_root.join("pressure").join("memory")).unwrap_or_default();
    let cpu = read_text(&proc_root.join("pressure").join("cpu")).unwrap_or_default();
    Psi {
        memory_some_avg10: avg10(&memory, "some"),
        memory_full_avg10: avg10(&memory, "full"),
        cpu_some_avg10: avg10(&cpu, "some"),
    }
}

/// The `k10temp` hwmon's `temp1_input`, in degrees. The hwmon directories are read in name order.
pub fn read_cpu_temp_c(sys_root: &Path) -> Option<f64> {
    let hwmon = sys_root.join("class").join("hwmon");
    sorted_names(&hwmon).into_iter().find_map(|name| {
        let base = hwmon.join(&name);
        if read_text(&base.join("name"))?.trim() != "k10temp" {
            return None;
        }
        let millidegrees = read_text(&base.join("temp1_input"))?;
        parse_finite(millidegrees.trim()).map(|value| value / 1000.0)
    })
}

/// The first `card<N>` in name order whose `device/gpu_busy_percent` reads.
pub fn read_gpu_busy_pct(sys_root: &Path) -> Option<f64> {
    let drm = sys_root.join("class").join("drm");
    sorted_names(&drm)
        .into_iter()
        .filter(|name| is_card(name))
        .find_map(|card| {
            let busy = drm.join(card).join("device").join("gpu_busy_percent");
            read_text(&busy).and_then(|text| parse_finite(text.trim()))
        })
}

/// The receive and transmit byte counters of the physical NICs, summed. Only a NIC whose `class/net/<name>/device` exists
/// counts, so bridges, veths and the tunnels do not count the same traffic twice. A counter line that does not parse is
/// skipped. `None` when `/proc/net/dev` cannot be read.
pub fn read_nic_totals(proc_root: &Path, sys_root: &Path) -> Option<NicTotals> {
    let text = read_text(&proc_root.join("net").join("dev"))?;
    let mut totals = NicTotals {
        rx_bytes: 0,
        tx_bytes: 0,
    };
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !is_plain_name(name) || !is_physical(sys_root, name) {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let rx = fields.first().and_then(|field| field.parse::<u64>().ok());
        let tx = fields.get(8).and_then(|field| field.parse::<u64>().ok());
        if let (Some(rx), Some(tx)) = (rx, tx) {
            totals.rx_bytes = totals.rx_bytes.saturating_add(rx);
            totals.tx_bytes = totals.tx_bytes.saturating_add(tx);
        }
    }
    Some(totals)
}

fn parse_stat(text: &str) -> CpuReading {
    let cores = text.lines().filter(|line| is_numbered_cpu(line)).count();
    let totals = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("cpu"))
        .and_then(parse_aggregate);
    CpuReading {
        totals,
        cores: u64::try_from(cores).unwrap_or(u64::MAX),
    }
}

/// `cpu<digits>` followed by whitespace. The aggregate `cpu` line has no digits after the name.
fn is_numbered_cpu(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("cpu") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(char::is_whitespace)
}

fn parse_aggregate(line: &str) -> Option<CpuTotals> {
    let fields = line
        .split_whitespace()
        .skip(1)
        .map(|field| field.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()?;
    if fields.len() < 4 {
        return None;
    }
    let counted = &fields[..fields.len().min(8)];
    let total = counted
        .iter()
        .fold(0u64, |sum, field| sum.saturating_add(*field));
    let idle = fields[3].saturating_add(fields.get(4).copied().unwrap_or(0));
    Some(CpuTotals {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// The value of one `meminfo` key in bytes: a `kB` value is scaled by 1024, any other value is taken as bytes.
fn meminfo_bytes(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        let mut parts = rest.split_whitespace();
        let value = parts.next()?.parse::<u64>().ok()?;
        let scale = if parts.next() == Some("kB") { 1024 } else { 1 };
        value.checked_mul(scale)
    })
}

/// The `avg10` of the pressure line of one kind (`some` or `full`).
fn avg10(text: &str, kind: &str) -> Option<f64> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(kind))?;
    line.split_whitespace()
        .find_map(|token| token.strip_prefix("avg10="))
        .and_then(parse_finite)
}

fn is_card(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// A NIC name that cannot climb out of `class/net`.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

fn is_physical(sys_root: &Path, name: &str) -> bool {
    sys_root
        .join("class")
        .join("net")
        .join(name)
        .join("device")
        .exists()
}

fn parse_finite(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// The entry names of a directory, sorted. A directory that cannot be read has no entries.
fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_seconds_and_decimals_give_milliseconds() {
        for (text, expected) in [
            ("0", 0),
            ("0.01", 10),
            ("1.2", 1200),
            ("123.4567", 123456),
            ("86399.999", 86_399_999),
            ("86400.00", 86_400_000),
        ] {
            assert_eq!(parse_uptime_ms(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn invalid_or_overflowing_uptime_is_unknown() {
        for text in [
            "",
            ".1",
            "1.",
            "-1.0",
            "NaN",
            "inf",
            "1e5",
            "1.2.3",
            "garbage",
            "18446744073709551616",
            "18446744073709552",
            "18446744073709551.616",
        ] {
            assert_eq!(parse_uptime_ms(text), None, "{text}");
        }
    }
}
