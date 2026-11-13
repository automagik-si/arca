//! The Automagik follower: a service made from Automagik's server image follows Automagik's releases.
//!
//! On a timer the node reads the record Automagik publishes for its newest server image
//! (`releases/download/server-lane/server-image.json` of `automagik-si/automagik`; until that lane exists, the
//! `server-image.json` of the newest `v<version>` release that carries one, found through the releases API). It never
//! reads `releases/latest`. When a followed service runs another digest of that image than the record's, and the
//! record's version is newer (semver; never older, never equal), the follower calls `service_update` for it.
//!
//! The follower looks at a service only when it follows ([`ServiceConfig::follows_automagik`]): a service made from the
//! `automagik-server` template does unless `service_update {name, follow: false}` said it stays. A node with no such
//! service asks the network for nothing.
//!
//! Each change is journalled in the update journal ([`SERVICE_UPDATES_FILE`]): one `follow` line before the update, with
//! the record and the provenance result, and one `follow end` line after it. A look that changes nothing is not
//! journalled. Provenance is `gh attestation verify` of the image's digest against `automagik-si/automagik`, or, where
//! `gh` cannot run, a look at GitHub's attestations API; its result is journalled and a failure of it never stops an
//! update. An update that failed (and was rolled back) is not tried again for the same digest until the record names
//! another one.
use super::*;
use crate::service::{AUTOMAGIK_IMAGE, FollowVersion, ServiceConfig};
use arca_kernel::desired::Entry;
use service_update::Journal;
use std::cmp::Ordering;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as Atomic};

/// Automagik's repository on GitHub, which every address here is under.
const REPOSITORY: &str = "automagik-si/automagik";
/// The release asset that names an image.
const ASSET: &str = "server-image.json";
/// The release the server image's newest record is published under.
pub const LANE_URL: &str =
    "https://github.com/automagik-si/automagik/releases/download/server-lane/server-image.json";
/// The releases of the repository, newest first, as GitHub's API lists them.
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/automagik-si/automagik/releases?per_page=10";
/// The images the shipped `automagik-server` template has pinned, and the version each is: a service made from the
/// template runs one of these until the follower records the version it moved it to. A test holds this table to the
/// template.
pub const PINNED: [(&str, &str); 1] = [(
    "sha256:49e997e36dc35b79b704fac45f2f14cc07cbe7cf3c943e0a5ceebb5a4c1d5dcc",
    "3.136.4",
)];
/// The most a fetched answer may hold.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// How long one fetch, and one attestation check, may take.
const FETCH_WAIT: Duration = Duration::from_secs(30);
const VERIFY_WAIT: Duration = Duration::from_secs(90);

/// Reads one https address and answers its body, or says why it could not.
pub type Fetch = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;
/// Checks the provenance of the image at a digest: the words of what was found, or why it could not be checked.
pub type Verify = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// What the follower does at its edges: the network and the attestation check. A node uses [`FollowEdges::default`]; a
/// test gives a fake source.
#[derive(Clone)]
pub struct FollowEdges {
    pub fetch: Fetch,
    pub verify: Verify,
}

impl Default for FollowEdges {
    fn default() -> Self {
        Self {
            fetch: Arc::new(|url| {
                run(
                    Command::new("curl").args([
                        "--fail",
                        "--silent",
                        "--show-error",
                        "--location",
                        "--proto",
                        "=https",
                        "--proto-redir",
                        "=https",
                        "--max-time",
                        "25",
                        "--max-filesize",
                        "4194304",
                        "--header",
                        "Accept: application/json",
                        "--",
                        url,
                    ]),
                    FETCH_WAIT,
                )
            }),
            verify: Arc::new(|digest| {
                run(
                    Command::new("gh").args([
                        "attestation",
                        "verify",
                        &format!("oci://{AUTOMAGIK_IMAGE}@{digest}"),
                        "--repo",
                        REPOSITORY,
                    ]),
                    VERIFY_WAIT,
                )
                .map(|_| format!("gh attestation verify passed for {digest}"))
            }),
        }
    }
}

/// Reads a pipe on a thread of its own, up to a little over the most a body may hold.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(MAX_BODY as u64 + 1).read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Runs `command` for at most `wait`, and answers what it printed, or the first words it said when it failed.
fn run(command: &mut Command, wait: Duration) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program} could not run: {error}"))?;
    let readers = (drain(child.stdout.take()), drain(child.stderr.take()));
    let begun = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if begun.elapsed() < wait => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} did not finish in {} seconds",
                    wait.as_secs()
                ));
            }
            Err(error) => return Err(format!("{program}: {error}")),
        }
    };
    let stdout = readers.0.join().unwrap_or_default();
    let stderr = readers.1.join().unwrap_or_default();
    if !status.success() {
        let said: String = String::from_utf8_lossy(&stderr)
            .chars()
            .filter(|c| !c.is_control() || *c == ' ')
            .take(300)
            .collect();
        return Err(format!("{program} failed ({status}): {}", said.trim()));
    }
    if stdout.len() > MAX_BODY {
        return Err(format!("{program}: the answer is larger than 4 MiB"));
    }
    String::from_utf8(stdout).map_err(|_| format!("{program}: the answer is not text"))
}

/// A semantic version: `major.minor.patch`, an optional `-pre-release`, and build metadata, which orders nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Number(u64),
    Word(String),
}

impl Version {
    /// `1.2.3`, `v1.2.3`, `1.2.3-rc.1`, `1.2.3+build`; `None` for anything else.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(core, _)| core);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut numbers = core.split('.');
        let mut core = [0u64; 3];
        for slot in &mut core {
            let part = numbers.next()?;
            if part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || part.len() > 9
                || (part.len() > 1 && part.starts_with('0'))
            {
                return None;
            }
            *slot = part.parse().ok()?;
        }
        if numbers.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|part| {
                    if part.is_empty()
                        || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        None
                    } else if part.bytes().all(|b| b.is_ascii_digit()) {
                        part.parse()
                            .ok()
                            .filter(|_| part.len() <= 9)
                            .map(Part::Number)
                    } else {
                        Some(Part::Word(part.to_string()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Version { core, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A release is newer than its own pre-releases.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a, b) {
                            (Part::Number(a), Part::Number(b)) => a.cmp(b),
                            (Part::Number(_), Part::Word(_)) => Ordering::Less,
                            (Part::Word(_), Part::Number(_)) => Ordering::Greater,
                            (Part::Word(a), Part::Word(b)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// What Automagik publishes for one server image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub digest: String,
    pub version_text: String,
    pub version: Version,
    pub reference: String,
    pub revision: String,
    pub tag: String,
}

impl Record {
    /// The record in `body`, or why it is not one: the image must be Automagik's server image, the digest a sha256 one,
    /// the version a semantic version.
    pub fn parse(body: &str) -> Result<Record, String> {
        let value: Value =
            serde_json::from_str(body).map_err(|_| "the record is not JSON".to_string())?;
        let text = |key: &str| -> Result<String, String> {
            value[key]
                .as_str()
                .filter(|text| {
                    !text.is_empty() && text.len() <= 300 && !text.chars().any(char::is_control)
                })
                .map(str::to_string)
                .ok_or_else(|| format!("the record has no usable \"{key}\""))
        };
        let image = text("image")?;
        if image != AUTOMAGIK_IMAGE {
            return Err(format!("the record is for another image ({image})"));
        }
        let digest = text("digest")?;
        if !service_update::digest_ok(&digest) {
            return Err("the record's digest is not sha256: and 64 lower-case hex digits".into());
        }
        let version_text = text("version")?;
        let version = Version::parse(&version_text).ok_or_else(|| {
            format!("the record's version {version_text} is not a semantic version")
        })?;
        Ok(Record {
            digest,
            version_text,
            version,
            reference: text("ref")?,
            revision: text("revision")?,
            tag: text("tag")?,
        })
    }
}

/// The result of checking an image's provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attestation {
    /// `gh attestation verify` passed.
    Verified(String),
    /// `gh` could not check, and GitHub's attestations API lists an attestation for the digest; its signature was not
    /// verified here.
    Found(String),
    /// Nothing could be shown.
    Failed(String),
}

impl Attestation {
    fn json(&self) -> Value {
        let (result, detail) = match self {
            Attestation::Verified(detail) => ("verified", detail),
            Attestation::Found(detail) => ("found, not verified", detail),
            Attestation::Failed(detail) => ("failed", detail),
        };
        json!({ "result": result, "detail": detail })
    }
}

/// One change the follower made, or tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Followed {
    pub service: String,
    pub digest: String,
    pub version: String,
    /// `Ok` when the service runs the new digest; the update's words otherwise.
    pub outcome: Result<(), String>,
}

/// The follower: [`Follower::tick`] is one look.
pub struct Follower {
    handler: ServiceHandler,
    edges: FollowEdges,
    /// The digest a service's update failed on, by the service's entry: not tried again for it.
    failed: Mutex<BTreeMap<String, String>>,
    /// What a look has already journalled, so a look that finds the same thing again says nothing.
    noted: Mutex<BTreeSet<String>>,
    /// The last thing said on the log about a look that could not be made, so it is said once.
    said: Mutex<Option<String>>,
    paused: AtomicBool,
    /// How long a moved service's first port has to answer; the update's own default when absent.
    health_seconds: Option<u64>,
    /// The digests whose version is known without asking anyone: the images the shipped template pinned.
    pins: Vec<(String, String)>,
}

impl Follower {
    pub fn new(
        kernel: Arc<Mutex<Kernel>>,
        clock: Clock,
        cfg: Arc<NodeConfig>,
        update: service_update::UpdateOptions,
        edges: FollowEdges,
    ) -> Follower {
        let writes = write_gate(&kernel);
        let exec = Arc::new(crate::service_exec::ServiceExec::new(cfg.clone()));
        Follower {
            handler: ServiceHandler {
                verb: "service_update",
                kernel,
                clock,
                cfg,
                writes,
                exec,
                functions: None,
                update,
            },
            edges,
            failed: Mutex::new(BTreeMap::new()),
            noted: Mutex::new(BTreeSet::new()),
            said: Mutex::new(None),
            paused: AtomicBool::new(false),
            health_seconds: None,
            pins: PINNED
                .iter()
                .map(|(digest, version)| (digest.to_string(), version.to_string()))
                .collect(),
        }
    }

    /// Replaces the digests whose version is known without asking anyone (a test's template pins its own).
    pub fn with_pins(mut self, pins: Vec<(String, String)>) -> Follower {
        self.pins = pins;
        self
    }

    /// How many seconds a moved service's first port has to answer (`service_update`'s `timeoutSeconds`).
    pub fn with_health_seconds(mut self, seconds: u64) -> Follower {
        self.health_seconds = Some(seconds);
        self
    }

    /// Stops the timer's looks (a test's, or a node going down).
    pub fn pause(&self) {
        self.paused.store(true, Atomic::SeqCst);
    }

    fn say(&self, why: Option<String>) {
        let mut said = self.said.lock().unwrap_or_else(PoisonError::into_inner);
        if *said != why {
            if let Some(why) = &why {
                eprintln!("arca: the Automagik follower: {why}");
            }
            *said = why;
        }
    }

    fn fetch_record(&self, url: &str) -> Result<Record, String> {
        let body = (self.edges.fetch)(url)?;
        if body.len() > MAX_BODY {
            return Err("the record is larger than 4 MiB".into());
        }
        Record::parse(&body)
    }

    /// The `v<version>` releases that carry the record, newest first.
    fn releases(&self) -> Result<Vec<(Version, String)>, String> {
        let body = (self.edges.fetch)(RELEASES_URL)
            .map_err(|why| format!("the releases of {REPOSITORY} were not listed: {why}"))?;
        let list: Value = serde_json::from_str(&body)
            .map_err(|_| format!("the releases of {REPOSITORY} are not JSON"))?;
        let mut found: Vec<(Version, String)> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|release| release["draft"] != true && release["prerelease"] != true)
            .filter(|release| {
                release["assets"]
                    .as_array()
                    .is_some_and(|assets| assets.iter().any(|asset| asset["name"] == ASSET))
            })
            .filter_map(|release| {
                let tag = release["tag_name"].as_str()?;
                tag.starts_with('v')
                    .then(|| Version::parse(tag).map(|version| (version, tag.to_string())))?
            })
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(found)
    }

    fn release_url(tag: &str) -> String {
        format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{ASSET}")
    }

    /// The newest published record: the lane's, or, until the lane exists, the newest release's.
    fn newest(&self) -> Result<Record, String> {
        let lane = match self.fetch_record(LANE_URL) {
            Ok(record) => return Ok(record),
            Err(why) => why,
        };
        let releases = self.releases().map_err(|why| format!("{lane}; {why}"))?;
        let (_, tag) = releases
            .first()
            .ok_or_else(|| format!("{lane}; no release of {REPOSITORY} carries {ASSET}"))?;
        self.fetch_record(&Self::release_url(tag))
    }

    /// The version the service's running image is: the one its record keeps for that digest, or the template's pin.
    /// Never a guess from a page of releases.
    fn version_of(&self, config: &ServiceConfig, running: &str) -> Option<Version> {
        config
            .followed_version(running)
            .map(str::to_string)
            .or_else(|| {
                self.pins
                    .iter()
                    .find(|(digest, _)| digest == running)
                    .map(|(_, version)| version.clone())
            })
            .and_then(|text| Version::parse(&text))
    }

    /// Journals `step` once for this `key` in this process: a look that finds the same thing again says nothing.
    fn note_once(&self, journal: &Journal, key: String, step: &str, detail: Value) {
        if self
            .noted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key)
        {
            journal.step(step, true, detail);
        }
    }

    fn attest(&self, digest: &str) -> Attestation {
        let why = match (self.edges.verify)(digest) {
            Ok(detail) => return Attestation::Verified(detail),
            Err(why) => why,
        };
        let url = format!("https://api.github.com/repos/{REPOSITORY}/attestations/{digest}");
        match (self.edges.fetch)(&url) {
            Ok(body)
                if serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["attestations"].as_array().map(|a| !a.is_empty()))
                    == Some(true) =>
            {
                Attestation::Found(format!(
                    "{why}; GitHub lists an attestation for the digest, whose signature was not verified here"
                ))
            }
            Ok(_) => {
                Attestation::Failed(format!("{why}; GitHub lists no attestation for the digest"))
            }
            Err(fetch) => Attestation::Failed(format!("{why}; {fetch}")),
        }
    }

    /// One look: every followed service whose digest differs from the newest record's, and whose version is older,
    /// is moved. Answers what it changed or tried; nothing for a look that changed nothing.
    pub fn tick(&self) -> Vec<Followed> {
        if self.paused.load(Atomic::SeqCst) {
            return Vec::new();
        }
        let entries = match lock(&self.handler.kernel) {
            Ok(kernel) => kernel.entries().to_vec(),
            Err(_) => return Vec::new(),
        };
        let followed: Vec<(Entry, ServiceConfig)> = service_entries(&entries)
            .into_iter()
            .filter(|(_, config)| config.follows_automagik())
            .collect();
        if followed.is_empty() {
            return Vec::new();
        }
        let newest = match self.newest() {
            Ok(record) => {
                self.say(None);
                record
            }
            Err(why) => {
                self.say(Some(why));
                return Vec::new();
            }
        };
        let mut done = Vec::new();
        let mut attested: Option<Attestation> = None;
        for (entry, config) in followed {
            if let Some(moved) = self.look_at(&entry, &config, &newest, &mut attested) {
                done.push(moved);
            }
        }
        done
    }

    /// One followed service against the newest record.
    fn look_at(
        &self,
        entry: &Entry,
        config: &ServiceConfig,
        newest: &Record,
        attested: &mut Option<Attestation>,
    ) -> Option<Followed> {
        let running = config.source["alias"]
            .as_str()
            .and_then(|alias| alias.rsplit_once('@'))
            .map(|(_, digest)| digest.to_string())?;
        let journal = Journal {
            path: self
                .handler
                .cfg
                .state_dir
                .join(service_update::SERVICE_UPDATES_FILE),
            id: format!("follow-{}-{}", entry.id.0, (self.handler.clock)()),
            service: config.name.clone(),
            clock: self.handler.clock.clone(),
        };
        if running == newest.digest {
            // The record names the image the service runs: its version is known from now on, restart or not.
            if config.followed_version(&running) != Some(newest.version_text.as_str()) {
                let version = newest.version_text.clone();
                let _ = self.handler.amend(&entry.id.0, &config.name, |record| {
                    record.follow_version = Some(FollowVersion {
                        digest: running.clone(),
                        version,
                    });
                });
            }
            return None;
        }
        if self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entry.id.0)
            == Some(&newest.digest)
        {
            return None;
        }
        let key = |kind: &str| format!("{kind}|{}|{running}|{}", entry.id.0, newest.digest);
        let Some(running_version) = self.version_of(config, &running) else {
            self.note_once(
                &journal,
                key("unknown"),
                "follow skipped",
                json!({
                    "why": "the version the service runs is not known (its image is not one the template pinned and the follower has not recorded it), so there is no order to move by",
                    "from": running,
                    "to": newest.digest,
                    "version": newest.version_text,
                }),
            );
            return None;
        };
        if newest.version <= running_version {
            return None;
        }
        let changer = self.handler.state_changer(config).ok()?;
        let seen = changer.read(&config.project, &config.instance).ok()?;
        if !seen.is_running() {
            self.note_once(
                &journal,
                key("stopped"),
                "follow skipped",
                json!({
                    "why": format!(
                        "the service is {}: the follower does not start a service its owner stopped. It moves the next time it runs and a newer release is out",
                        seen.status.to_lowercase()
                    ),
                    "to": newest.digest,
                    "version": newest.version_text,
                }),
            );
            return None;
        }
        // Incus may still be at an earlier rebuild (an update that did not settle): a second one would delete the root
        // volume the first is making. Nothing moves while an operation that may write names the instance, or while the
        // listing cannot be read.
        let busy = arca_incus::read::writing_on(
            &arca_incus::UnixIncus::new(self.handler.cfg.incus.socket.clone()),
            arca_kernel::ownership::IncusKind::Instance,
            &config.project,
            None,
            &config.instance,
        )
        .map_or(
            Some("Incus's operations could not be read".to_string()),
            |ops| {
                (!ops.is_empty()).then(|| {
                    format!(
                        "Incus is still working on its instance ({} operation{})",
                        ops.len(),
                        if ops.len() == 1 { "" } else { "s" }
                    )
                })
            },
        );
        if let Some(why) = busy {
            self.note_once(
                &journal,
                key("held"),
                "follow held",
                json!({
                    "why": format!("{why}: no move is started until it is idle"),
                    "to": newest.digest,
                    "version": newest.version_text,
                }),
            );
            return None;
        }
        let provenance = attested
            .get_or_insert_with(|| self.attest(&newest.digest))
            .clone();
        let old_image = seen.own_config.get("volatile.base_image").cloned();
        journal.step(
            "follow",
            true,
            json!({
                "from": running,
                "fromVersion": running_version_text(&running_version),
                "to": newest.digest,
                "version": newest.version_text,
                "ref": newest.reference,
                "revision": newest.revision,
                "tag": newest.tag,
                "attestation": provenance.json(),
            }),
        );
        let outcome = self
            .handler
            .call_update(
                &match self.health_seconds {
                    Some(seconds) => json!({
                        "name": config.name,
                        "digest": newest.digest,
                        "timeoutSeconds": seconds,
                    }),
                    None => json!({ "name": config.name, "digest": newest.digest }),
                },
                std::slice::from_ref(entry),
                false,
            )
            .map(|_| ())
            .map_err(|error| match error {
                CallError::Refused { code, message } => (code, message),
                CallError::Invalid(message) => ("invalid".to_string(), message),
                _ => ("failed".to_string(), "the update was not made".to_string()),
            });
        match &outcome {
            Ok(()) => {
                journal.step("follow end", true, json!({ "digest": newest.digest }));
                let version = newest.version_text.clone();
                let digest = newest.digest.clone();
                let _ = self.handler.amend(&entry.id.0, &config.name, |record| {
                    record.follow_version = Some(FollowVersion { digest, version });
                });
                // The image the service left is cached for ten days by Incus's own rule: at a release a day or more,
                // that is the disk. It goes now, when no instance is made from it.
                if let Some(fingerprint) = old_image {
                    match changer.delete_unused_image(&config.project, &fingerprint) {
                        Ok(true) => journal.step(
                            "follow image",
                            true,
                            json!({ "removed": fingerprint, "of": running }),
                        ),
                        Ok(false) => {}
                        Err(error) => journal.step(
                            "follow image",
                            false,
                            json!({ "why": error.to_string(), "image": fingerprint }),
                        ),
                    }
                }
            }
            Err((code, why)) => {
                journal.step("follow end", false, json!({ "why": why, "code": code }));
                self.failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(entry.id.0.clone(), newest.digest.clone());
                eprintln!(
                    "arca: the Automagik follower did not move service {}: {why}",
                    config.name
                );
            }
        }
        Some(Followed {
            service: config.name.clone(),
            digest: newest.digest.clone(),
            version: newest.version_text.clone(),
            outcome: outcome.map_err(|(_, why)| why),
        })
    }
}

fn running_version_text(version: &Version) -> String {
    let [major, minor, patch] = version.core;
    let mut text = format!("{major}.{minor}.{patch}");
    for (index, part) in version.pre.iter().enumerate() {
        text.push(if index == 0 { '-' } else { '.' });
        match part {
            Part::Number(number) => text.push_str(&number.to_string()),
            Part::Word(word) => text.push_str(word),
        }
    }
    text
}

/// Looks every `every` on a thread of its own, the first look a minute after the start, for as long as the process runs.
pub fn spawn_follower(follower: Arc<Follower>, every: Duration) -> io::Result<()> {
    thread::Builder::new()
        .name("arca-follower".into())
        .spawn(move || {
            thread::sleep(Duration::from_secs(60).min(every));
            loop {
                let _ = follower.tick();
                thread::sleep(every);
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("a version")
    }

    #[test]
    fn versions_order_as_semver_does_and_a_release_beats_its_own_pre_release() {
        assert!(v("3.136.5") > v("3.136.4"));
        assert!(v("3.137.0") > v("3.136.99"));
        assert!(v("v10.0.0") > v("9.9.9"));
        assert!(v("3.136.4") > v("3.136.4-rc.2"));
        assert!(v("3.136.4-rc.10") > v("3.136.4-rc.2"));
        assert!(v("3.136.4-rc.1") > v("3.136.4-beta.9"));
        assert_eq!(v("3.136.4+build5"), v("3.136.4"));
    }

    #[test]
    fn the_digest_the_shipped_template_pins_has_its_version_in_the_table() {
        let template: Value =
            serde_json::from_str(include_str!("../../../templates/automagik-server.json"))
                .expect("the template is JSON");
        let digest = template["digest"].as_str().expect("a digest");
        assert!(
            PINNED.iter().any(|(pinned, _)| *pinned == digest),
            "a new pin in templates/automagik-server.json needs its version in PINNED"
        );
    }

    #[test]
    fn a_text_that_is_not_a_semantic_version_is_refused() {
        for text in [
            "",
            "3.136",
            "3.136.4.1",
            "03.1.1",
            "a.b.c",
            "3.1.1-",
            "latest",
            "3.1.1-a..b",
        ] {
            assert!(Version::parse(text).is_none(), "{text}");
        }
    }
}
