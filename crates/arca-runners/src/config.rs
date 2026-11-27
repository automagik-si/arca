//! A runner pool's settings: where its runners register on GitHub, the machines it may hold at once, and its size
//! classes, each chosen by the labels a job's `runs-on` names.

use serde::Deserialize;

/// The guest's default folders, one per line: the work folder, then the runner's own folder. They live in a text file
/// beside the guest script so that each one is, whole, a line of the product's sources, which is what the portable
/// build's check for build-machine paths accepts (two Rust literals would sit side by side in the binary as one string).
const GUEST_DEFAULTS: &str = include_str!("../guest/defaults");

fn guest_default(line: usize) -> &'static str {
    GUEST_DEFAULTS.lines().nth(line).unwrap_or_default()
}

/// The default work folder, `/home/runner/work`: the workspace is `<work folder>/<repo>/<repo>`, as on GitHub's own
/// runners.
pub fn work_folder() -> &'static str {
    guest_default(0)
}

/// The default runner folder in the image, `/home/runner/actions-runner`.
pub fn runner_folder() -> &'static str {
    guest_default(1)
}

/// One pool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PoolConfig {
    /// The pool's name: a plain name, unique on the node. Machines carry it in their mark.
    pub name: String,
    /// The signer's binding: the GitHub organisation (or installation) whose credential the pool uses.
    pub binding: String,
    /// The organisation's address, such as `https://github.com/acme`. Only a pool that signs in with a token of its
    /// own needs it; the signer knows its binding's organisation.
    #[serde(default)]
    pub url: Option<String>,
    /// The runner group the scale sets live in. It decides which repositories may send jobs here. Absent, the group
    /// the signer made for the binding's repositories.
    #[serde(default)]
    pub runner_group_id: Option<u64>,
    /// What the pool may hold at once, across all its classes.
    pub budget: Budget,
    /// The size classes. Each is one scale set on GitHub.
    pub classes: Vec<ClassConfig>,
    /// The Incus project the machines are made in.
    pub project: String,
    /// The runner image: `arca-runner-image-<tag>/image`, the stopped machine `images/github-runner/build.sh` kept and
    /// its snapshot, or `arca-runner-image-<tag>`, the image it published. Any image alias or fingerprint, or
    /// `<instance>/<snapshot>` of a stopped instance in the pool's project, works the same way.
    pub image: String,
    /// The profiles of every machine, in order; empty takes the project's `default`. A profile's `eth0` is replaced by
    /// the card Arca gives every machine on [`Self::network`].
    #[serde(default)]
    pub profiles: Vec<String>,
    /// The bridge the machines' card is on; absent, the node's service bridge. The card is Arca's, not a profile's: MAC,
    /// IPv4 and IPv6 filtering, nothing inbound, and the pool's ACL, which rejects the bridge's whole subnet, the
    /// private ranges, the tailnet range and the host's addresses and lets the rest of the internet out.
    #[serde(default)]
    pub network: Option<String>,
    /// The storage pool of the machines' root disks, needed for a class with `diskGiB`.
    #[serde(default)]
    pub storage_pool: Option<String>,
    /// Where the runner program is installed in the image.
    #[serde(default = "default_runner_dir")]
    pub runner_dir: String,
    /// The runner's work folder. The workspace is `<workFolder>/<repo>/<repo>`.
    #[serde(default = "default_work_folder")]
    pub work_folder: String,
    /// How long, in days, a job's logs are kept on the host.
    #[serde(default = "default_log_days")]
    pub log_days: u32,
}

/// The most a pool holds at once. A machine counts its full size from its create to its delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    pub cpu: u32,
    #[serde(deserialize_with = "mib")]
    pub memory: u64,
}

/// One size class: the labels its jobs ask for and the machine each of them gets.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClassConfig {
    /// The labels, chosen by the person. A job whose `runs-on` names these lands in this class. The first one names
    /// the scale set.
    pub labels: Vec<String>,
    pub cpu: u32,
    /// Memory, such as `16GiB` or `512MiB`; held as MiB.
    #[serde(deserialize_with = "mib")]
    pub memory: u64,
    /// The root disk's size in GiB; absent keeps the image's.
    #[serde(default, rename = "diskGiB")]
    pub disk_gib: Option<u64>,
    /// Booted machines with an idle registered runner kept ready, so a job starts without a boot.
    #[serde(default)]
    pub warm: u32,
    /// The most machines of this class at once.
    pub max: u32,
}

impl ClassConfig {
    /// The scale set's name on GitHub: the first label.
    pub fn scale_set_name(&self) -> &str {
        self.labels.first().map_or("", String::as_str)
    }
}

impl PoolConfig {
    /// The bridge the machines' card is on: [`Self::network`], or the node's service bridge.
    pub fn bridge(&self) -> &str {
        self.network.as_deref().unwrap_or(arca_netproject::BRIDGE)
    }

    /// Every rule a pool's settings must keep, with the first one broken named.
    pub fn check(&self) -> Result<(), String> {
        if !plain(&self.name) || !plain(&self.project) {
            return Err("a pool's name and project are plain names".to_string());
        }
        if self
            .url
            .as_ref()
            .is_some_and(|url| !url.starts_with("https://"))
        {
            return Err("a pool's url is an https address".to_string());
        }
        if self.classes.is_empty() {
            return Err("a pool has at least one class".to_string());
        }
        if self.budget.cpu == 0 || self.budget.memory == 0 {
            return Err("a pool's budget has CPUs and memory".to_string());
        }
        let mut names = std::collections::BTreeSet::new();
        for class in &self.classes {
            if class.labels.is_empty() || class.labels.iter().any(|label| !label_ok(label)) {
                return Err(
                    "a class names at least one label, each of letters, digits, '-', '_' or '.'"
                        .to_string(),
                );
            }
            if !names.insert(class.scale_set_name().to_ascii_lowercase()) {
                return Err("two classes share a first label".to_string());
            }
            if class.cpu == 0 || class.memory == 0 {
                return Err("a class has CPUs and memory".to_string());
            }
            if class.cpu > self.budget.cpu || class.memory > self.budget.memory {
                return Err("a class is larger than the pool's budget".to_string());
            }
            if class.disk_gib.is_some() && self.storage_pool.is_none() {
                return Err("a class with diskGiB needs the pool's storagePool".to_string());
            }
            if class.warm > class.max {
                return Err("a class keeps more warm machines than its max".to_string());
            }
        }
        if !self.work_folder.starts_with('/') || !self.runner_dir.starts_with('/') {
            return Err("runnerDir and workFolder are absolute paths".to_string());
        }
        Ok(())
    }
}

fn default_runner_dir() -> String {
    runner_folder().to_string()
}

fn default_work_folder() -> String {
    work_folder().to_string()
}

fn default_log_days() -> u32 {
    7
}

fn plain(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn label_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 100
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// A size such as `16GiB`, `512MiB` or `2TiB`, as MiB.
pub fn parse_mib(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let number: u64 = number.parse().ok()?;
    let factor = match unit {
        "MiB" => 1,
        "GiB" => 1024,
        "TiB" => 1024 * 1024,
        _ => return None,
    };
    number.checked_mul(factor).filter(|mib| *mib > 0)
}

fn mib<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_mib(&text)
        .ok_or_else(|| serde::de::Error::custom("a size is a whole number of MiB, GiB or TiB"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn example() -> serde_json::Value {
        json!({
            "name": "acme",
            "binding": "acme-org",
            "url": "https://github.com/acme",
            "runnerGroupId": 3,
            "budget": { "cpu": 16, "memory": "64GiB" },
            "project": "arcak-runners",
            "image": "arca-runner-image-ubuntu24-20261111-370/image",
            "classes": [
                { "labels": ["acme-4vcpu-ubuntu-2404"], "cpu": 4, "memory": "16GiB", "warm": 1, "max": 4 },
                { "labels": ["acme-2vcpu-ubuntu-2404"], "cpu": 2, "memory": "8GiB", "max": 4 }
            ]
        })
    }

    #[test]
    fn a_pool_reads_and_checks() {
        let pool: PoolConfig = serde_json::from_value(example()).expect("a pool");
        pool.check().expect("valid");
        assert_eq!(pool.classes[0].memory, 16 * 1024);
        assert_eq!(pool.classes[0].scale_set_name(), "acme-4vcpu-ubuntu-2404");
        assert_eq!(pool.work_folder, "/home/runner/work");
        assert_eq!(pool.log_days, 7);
        assert_eq!(pool.network, None);
        assert_eq!(pool.bridge(), "arcak-svcbr", "the node's service bridge");
        let mut value = example();
        value["network"] = json!("ci-br0");
        let pool: PoolConfig = serde_json::from_value(value).expect("a pool");
        assert_eq!(pool.bridge(), "ci-br0");
    }

    #[test]
    fn a_class_larger_than_the_budget_is_refused() {
        let mut value = example();
        value["classes"][0]["cpu"] = json!(32);
        let pool: PoolConfig = serde_json::from_value(value).expect("a pool");
        assert!(pool.check().is_err());
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_mib("512MiB"), Some(512));
        assert_eq!(parse_mib("2GiB"), Some(2048));
        assert_eq!(parse_mib("2GB"), None);
        assert_eq!(parse_mib("GiB"), None);
    }
}
