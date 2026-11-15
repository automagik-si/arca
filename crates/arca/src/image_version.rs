//! The version an image digest is, as its registry's tags name it, for the `Version` row of a `service_create` or
//! `service_update` proposal's card. Only Omni's API image is looked up ([`OMNI_IMAGE`]): this module is the one place
//! that says which repository is asked and what the card calls it.
//!
//! The lookup is read-only and anonymous, on `ghcr.io` alone: a pull token from `ghcr.io/token`, the repository's tags
//! (paged, at most [`MAX_PAGES`] pages), and a HEAD of the manifest of each of the [`NEWEST`] newest version tags. It
//! follows no redirect and no token realm, reads at most [`ANSWER_MAX`] bytes of an answer, and the whole round has one
//! deadline ([`DEADLINE_MS`]). A tag is considered only when it has a version's shape ([`version_tag_ok`]); no other
//! byte of a registry answer reaches a card or a log.
//!
//! What the registry answered is kept as one tag-to-digest map per repository, at most [`MAX_AGE_MS`] old, with one
//! refresh in flight at a time; every proposal, for any digest, is answered from that map. A digest the map does not
//! name refreshes it once, and only when it is older than [`MISS_REFRESH_MS`]; otherwise its version is unknown. The
//! lookup never refuses anything: a miss or a failure is only "version unknown" on the card.
//!
//! The version is a label: it is no argument of the proposal, so the owner's approval binds the digest and the args
//! digest is the same with or without the lookup. A tag can move, so the card says the name is the tag the registry
//! gave the digest when the proposal was made.
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::action_proposal::ActionDetail;

/// Omni's API image, the one image whose versions are looked up.
pub const OMNI_IMAGE: &str = "ghcr.io/automagik-si/omni-api";
/// The registry host every request goes to, and no other.
const HOST: &str = "ghcr.io";
/// How many of the newest version tags have their digest asked.
pub const NEWEST: usize = 50;
/// The most pages of the tags list read in one round.
pub const MAX_PAGES: usize = 5;
/// How many tags one page asks for.
const PAGE_SIZE: usize = 1000;
/// The whole round, every request in it, ends this long after it starts.
pub const DEADLINE_MS: u64 = 5_000;
/// A map older than this is asked again before it answers.
pub const MAX_AGE_MS: u64 = 10 * 60_000;
/// A digest the map does not name refreshes it only when the map is older than this.
pub const MISS_REFRESH_MS: u64 = 60_000;
/// The most bytes of one registry answer read.
pub const ANSWER_MAX: usize = 1024 * 1024;
/// The card row's label.
pub const ROW_LABEL: &str = "Version";

/// The name the card gives an image this module looks up, by the image (`<registry>/<repository>`). The one table of
/// which images are looked up.
fn label(image: &str) -> Option<&'static str> {
    (image == OMNI_IMAGE).then_some("Omni")
}

/// Whether `tag` has a version's shape: `v`, then one to four groups of digits joined by dots, at most 32 characters
/// (`v2.261024.2`). Only such a tag is considered, and only such a tag reaches a card.
pub fn version_tag_ok(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let groups: Vec<&str> = rest.split('.').collect();
    tag.len() <= 32
        && (1..=4).contains(&groups.len())
        && groups.iter().all(|group| {
            (1..=9).contains(&group.len()) && group.bytes().all(|b| b.is_ascii_digit())
        })
}

/// The numbers of a version tag, for ordering: newest is the largest, compared group by group.
fn version_key(tag: &str) -> Vec<u64> {
    tag[1..]
        .split('.')
        .map(|group| group.parse().unwrap_or(0))
        .collect()
}

/// Whether `tag` may be sent back to the registry as the `last` of the next page: a tag in the registry's own grammar.
fn tag_grammar_ok(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The value of the card's `Version` row for `image` at `digest`, or `None` for an image this module does not look up.
/// With a version: `Omni v2.261024.2 (sha256:...)` and where the name comes from. Without one: the digest and
/// "version unknown".
pub fn version_words(image: &str, digest: &str, version: Option<&str>) -> Option<String> {
    label(image).map(|label| words(label, digest, version))
}

fn words(label: &str, digest: &str, version: Option<&str>) -> String {
    match version.filter(|tag| version_tag_ok(tag)) {
        Some(tag) => format!(
            "{label} {tag} ({digest}). The name is the tag {HOST} gave this digest when this was proposed."
        ),
        None => format!("{digest} (version unknown)"),
    }
}

/// Whether `value` is a `Version` row this module writes for `digest`: kept when a proposal is read back from its
/// journal, refused otherwise.
pub fn row_fits(value: &str, digest: &str) -> bool {
    if value == words("", digest, None) {
        return true;
    }
    ["Omni"].iter().any(|label| {
        value
            .strip_prefix(&format!("{label} "))
            .and_then(|rest| rest.split_once(' '))
            .is_some_and(|(tag, _)| value == words(label, digest, Some(tag)))
    })
}

/// A registry request that gave no usable answer. It carries nothing of what the registry said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unanswered;

/// One page of a repository's tags.
pub struct Page {
    pub tags: Vec<String>,
    /// Whether the registry says another page follows.
    pub more: bool,
}

/// A registry's read side, for the lookup. A node uses [`Ghcr`]; a test gives a fake one. An error carries nothing of
/// the registry's answer.
pub trait TagRegistry: Send + Sync {
    /// Opens one round on `repository` (the path on the host, `automagik-si/omni-api`), with `wait` left for it.
    fn open<'a>(
        &'a self,
        repository: &str,
        wait: Duration,
    ) -> Result<Box<dyn RegistryRound + 'a>, Unanswered>;
}

/// One round of requests on one repository.
pub trait RegistryRound {
    /// The page of tags after `last`, in the registry's order.
    fn tags(&mut self, last: Option<&str>, wait: Duration) -> Result<Page, Unanswered>;
    /// The digest the manifest of `tag` has, as the registry names it (an index's own digest for a multi-arch image).
    fn digest(&mut self, tag: &str, wait: Duration) -> Result<String, Unanswered>;
}

/// A monotonic clock in milliseconds.
pub type Millis = Arc<dyn Fn() -> u64 + Send + Sync>;

/// One round: the digest of each of the newest version tags, as `digest -> tag` (the newest tag of a digest wins).
/// Every request is given what is left of the one deadline, and none starts after it. A failure of the token or of the
/// tags gives an empty map; a failed HEAD leaves only its tag out.
pub fn round(
    registry: &dyn TagRegistry,
    repository: &str,
    clock: &dyn Fn() -> u64,
) -> BTreeMap<String, String> {
    let deadline = clock().saturating_add(DEADLINE_MS);
    let left = || {
        let left = deadline.saturating_sub(clock());
        (left > 0).then(|| Duration::from_millis(left))
    };
    let mut found = BTreeMap::new();
    let Some(wait) = left() else {
        return found;
    };
    let Ok(mut session) = registry.open(repository, wait) else {
        return found;
    };
    let mut versions: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let Some(wait) = left() else {
            return found;
        };
        let Ok(page) = session.tags(last.as_deref(), wait) else {
            return found;
        };
        versions.extend(page.tags.iter().filter(|tag| version_tag_ok(tag)).cloned());
        last = page.tags.last().filter(|tag| tag_grammar_ok(tag)).cloned();
        if !page.more || last.is_none() {
            break;
        }
    }
    versions.sort_by_key(|tag| std::cmp::Reverse(version_key(tag)));
    versions.dedup();
    for tag in versions.into_iter().take(NEWEST) {
        let Some(wait) = left() else {
            break;
        };
        if let Ok(digest) = session.digest(&tag, wait)
            && crate::handlers::service_digest_ok(&digest)
        {
            found.entry(digest).or_insert(tag);
        }
    }
    found
}

/// What the registry answered for one repository, and when.
struct Known {
    at: u64,
    digests: BTreeMap<String, String>,
}

/// What the map says of one digest now.
enum Seen {
    /// A map at most [`MAX_AGE_MS`] old names it.
    Named(String),
    /// A map at most [`MAX_AGE_MS`] old, this old, does not name it.
    Missing(u64),
    /// No map, or one too old to answer.
    Stale,
}

/// The lookup a node's proposals share: the map per repository and the one refresh in flight.
pub struct VersionLookup {
    registry: Arc<dyn TagRegistry>,
    clock: Millis,
    known: Mutex<HashMap<String, Known>>,
    refreshing: Mutex<()>,
}

impl VersionLookup {
    pub fn new(registry: Arc<dyn TagRegistry>, clock: Millis) -> Self {
        Self {
            registry,
            clock,
            known: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(()),
        }
    }

    /// The node's own: ghcr.io over HTTPS, on a monotonic clock.
    pub fn ghcr() -> Self {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        Self::new(
            Arc::new(Ghcr),
            Arc::new(move || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)),
        )
    }

    fn seen(&self, image: &str, digest: &str) -> Seen {
        let now = (self.clock)();
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        match known.get(image) {
            Some(map) if now.saturating_sub(map.at) < MAX_AGE_MS => match map.digests.get(digest) {
                Some(tag) => Seen::Named(tag.clone()),
                None => Seen::Missing(now.saturating_sub(map.at)),
            },
            _ => Seen::Stale,
        }
    }

    /// The version tag the registry names `digest` of `image` by: `None` when this module does not look `image` up,
    /// `Some(None)` when the version is unknown. At most one round, and only as the module's header says.
    pub fn version(&self, image: &str, digest: &str) -> Option<Option<String>> {
        label(image)?;
        let repository = image.strip_prefix(&format!("{HOST}/"))?;
        let answer = |seen: Seen| match seen {
            Seen::Named(tag) => Some(Some(tag)),
            Seen::Missing(age) if age < MISS_REFRESH_MS => Some(None),
            _ => None,
        };
        if let Some(known) = answer(self.seen(image, digest)) {
            return Some(known);
        }
        let _one = self
            .refreshing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Another proposal may have refreshed the map while this one waited.
        if let Some(known) = answer(self.seen(image, digest)) {
            return Some(known);
        }
        // The round runs on a thread of its own, so it never blocks on a runtime the caller is inside.
        let registry = &*self.registry;
        let clock = &*self.clock;
        let digests = std::thread::scope(|scope| {
            scope
                .spawn(|| round(registry, repository, clock))
                .join()
                .unwrap_or_default()
        });
        let tag = digests.get(digest).cloned();
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                image.to_string(),
                Known {
                    at: (self.clock)(),
                    digests,
                },
            );
        Some(tag)
    }

    /// The card's `Version` row for `image` at `digest`, or `None` for an image this module does not look up.
    pub fn row(&self, image: &str, digest: &str) -> Option<ActionDetail> {
        let version = self.version(image, digest)?;
        Some(ActionDetail {
            label: ROW_LABEL.to_string(),
            value: version_words(image, digest, version.as_deref())?,
        })
    }
}

/// ghcr.io, anonymously, over HTTPS.
pub struct Ghcr;

impl TagRegistry for Ghcr {
    fn open<'a>(
        &'a self,
        repository: &str,
        wait: Duration,
    ) -> Result<Box<dyn RegistryRound + 'a>, Unanswered> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Unanswered)?;
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| Unanswered)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("arca/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Unanswered)?;
        let mut round = GhcrRound {
            runtime,
            client,
            repository: repository.to_string(),
            token: String::new(),
        };
        let url = format!("https://{HOST}/token?scope=repository:{repository}:pull&service={HOST}");
        let (status, _, body) = round.send(reqwest::Method::GET, &url, None, wait)?;
        if status != 200 {
            return Err(Unanswered);
        }
        let token = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|_| Unanswered)?
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 8192
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
            })
            .ok_or(Unanswered)?
            .to_string();
        round.token = token;
        Ok(Box::new(round))
    }
}

struct GhcrRound {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    repository: String,
    token: String,
}

/// What one request answered: its status, the headers the lookup reads, and at most [`ANSWER_MAX`] bytes of body.
type Answered = (u16, reqwest::header::HeaderMap, Vec<u8>);

impl GhcrRound {
    fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        accept: Option<&str>,
        wait: Duration,
    ) -> Result<Answered, Unanswered> {
        let mut request = self.client.request(method, url).timeout(wait);
        if !self.token.is_empty() {
            request = request.header("authorization", format!("Bearer {}", self.token));
        }
        if let Some(accept) = accept {
            request = request.header("accept", accept);
        }
        self.runtime.block_on(async {
            tokio::time::timeout(wait, async {
                let mut response = request.send().await.map_err(|_| Unanswered)?;
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| Unanswered)? {
                    if body.len() + chunk.len() > ANSWER_MAX {
                        return Err(Unanswered);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok((status, headers, body))
            })
            .await
            .map_err(|_| Unanswered)?
        })
    }
}

impl RegistryRound for GhcrRound {
    fn tags(&mut self, last: Option<&str>, wait: Duration) -> Result<Page, Unanswered> {
        let mut url = format!(
            "https://{HOST}/v2/{}/tags/list?n={PAGE_SIZE}",
            self.repository
        );
        if let Some(last) = last.filter(|tag| tag_grammar_ok(tag)) {
            url.push_str(&format!("&last={last}"));
        }
        let (status, headers, body) =
            self.send(reqwest::Method::GET, &url, Some("application/json"), wait)?;
        if status != 200 {
            return Err(Unanswered);
        }
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| Unanswered)?;
        let tags = value
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let more = headers.get_all("link").iter().any(|link| {
            link.to_str()
                .is_ok_and(|link| link.contains("rel=\"next\""))
        });
        Ok(Page { tags, more })
    }

    fn digest(&mut self, tag: &str, wait: Duration) -> Result<String, Unanswered> {
        if !version_tag_ok(tag) {
            return Err(Unanswered);
        }
        let url = format!("https://{HOST}/v2/{}/manifests/{tag}", self.repository);
        let accept = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
        let (status, headers, _) = self.send(reqwest::Method::HEAD, &url, Some(accept), wait)?;
        if status != 200 {
            return Err(Unanswered);
        }
        headers
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .filter(|digest| crate::handlers::service_digest_ok(digest))
            .map(str::to_string)
            .ok_or(Unanswered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn digest_of(n: usize) -> String {
        format!("sha256:{n:064x}")
    }

    /// A registry of `tags` (in the registry's alphabetical order, `page` to a page), each tag's digest the one
    /// `digests` names. It counts every request and moves the shared clock `step` milliseconds at each HEAD.
    struct Fake {
        tags: Vec<String>,
        digests: HashMap<String, String>,
        page: usize,
        fail_open: bool,
        clock: Arc<AtomicU64>,
        step: u64,
        opens: AtomicUsize,
        pages: AtomicUsize,
        heads: Arc<AtomicUsize>,
    }

    impl Fake {
        fn new(clock: &Arc<AtomicU64>, pairs: &[(&str, String)]) -> Self {
            let mut tags: Vec<String> = pairs.iter().map(|(tag, _)| tag.to_string()).collect();
            tags.sort();
            Self {
                tags,
                digests: pairs
                    .iter()
                    .map(|(tag, digest)| (tag.to_string(), digest.clone()))
                    .collect(),
                page: 1000,
                fail_open: false,
                clock: Arc::clone(clock),
                step: 0,
                opens: AtomicUsize::new(0),
                pages: AtomicUsize::new(0),
                heads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn requests(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
                + self.pages.load(Ordering::SeqCst)
                + self.heads.load(Ordering::SeqCst)
        }
    }

    struct FakeRound<'a>(&'a Fake);

    impl TagRegistry for Arc<Fake> {
        fn open<'a>(
            &'a self,
            _repository: &str,
            _wait: Duration,
        ) -> Result<Box<dyn RegistryRound + 'a>, Unanswered> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.fail_open {
                return Err(Unanswered);
            }
            Ok(Box::new(FakeRound(self)))
        }
    }

    impl RegistryRound for FakeRound<'_> {
        fn tags(&mut self, last: Option<&str>, _wait: Duration) -> Result<Page, Unanswered> {
            let fake = self.0;
            fake.pages.fetch_add(1, Ordering::SeqCst);
            let start = last.map_or(0, |last| {
                fake.tags
                    .iter()
                    .position(|tag| tag == last)
                    .map_or(fake.tags.len(), |at| at + 1)
            });
            let end = (start + fake.page).min(fake.tags.len());
            Ok(Page {
                tags: fake.tags[start..end].to_vec(),
                more: end < fake.tags.len(),
            })
        }

        fn digest(&mut self, tag: &str, wait: Duration) -> Result<String, Unanswered> {
            let fake = self.0;
            assert!(
                wait > Duration::ZERO,
                "no request starts after the deadline"
            );
            fake.heads.fetch_add(1, Ordering::SeqCst);
            fake.clock.fetch_add(fake.step, Ordering::SeqCst);
            fake.digests.get(tag).cloned().ok_or(Unanswered)
        }
    }

    fn lookup(fake: &Arc<Fake>, clock: &Arc<AtomicU64>) -> VersionLookup {
        let clock = Arc::clone(clock);
        VersionLookup::new(
            Arc::new(Arc::clone(fake)),
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
    }

    #[test]
    fn only_a_version_shaped_tag_counts() {
        for good in ["v2.261024.2", "v1", "v10.0.0.1"] {
            assert!(version_tag_ok(good), "{good}");
        }
        for bad in [
            "latest",
            "2.261024.2",
            "v",
            "v2..1",
            "v2.1-rc1",
            "V2.1",
            "v1.2.3.4.5",
            "v2.261024.2\n",
            "v1234567890.1",
            "v2.<b>",
        ] {
            assert!(!version_tag_ok(bad), "{bad}");
        }
    }

    #[test]
    fn the_card_names_the_version_on_a_match_and_says_unknown_otherwise() {
        assert_eq!(
            version_words(OMNI_IMAGE, DIGEST_A, Some("v2.261024.2")).expect("omni"),
            format!(
                "Omni v2.261024.2 ({DIGEST_A}). The name is the tag ghcr.io gave this digest when this was proposed."
            )
        );
        assert_eq!(
            version_words(OMNI_IMAGE, DIGEST_A, None).expect("omni"),
            format!("{DIGEST_A} (version unknown)")
        );
        // A tag of any other shape is never shown.
        assert_eq!(
            version_words(OMNI_IMAGE, DIGEST_A, Some("evil<script>")).expect("omni"),
            format!("{DIGEST_A} (version unknown)")
        );
        assert_eq!(version_words("registry.example/app", DIGEST_A, None), None);
        for value in [
            version_words(OMNI_IMAGE, DIGEST_A, Some("v2.261024.2")).expect("omni"),
            version_words(OMNI_IMAGE, DIGEST_A, None).expect("omni"),
        ] {
            assert!(row_fits(&value, DIGEST_A), "{value}");
            assert!(!row_fits(&value, &digest_of(7)), "{value}");
        }
        assert!(!row_fits("Omni latest (x)", DIGEST_A));
    }

    #[test]
    fn the_newest_fifty_by_number_are_asked_across_pages() {
        let clock = Arc::new(AtomicU64::new(0));
        // 120 versions, alphabetical order puts v2.99 before v2.100: newest is decided by the numbers.
        let pairs: Vec<(String, String)> = (1..=120)
            .map(|n| (format!("v2.{n}"), digest_of(n)))
            .collect();
        let mut with_noise: Vec<(&str, String)> = pairs
            .iter()
            .map(|(tag, digest)| (tag.as_str(), digest.clone()))
            .collect();
        with_noise.push(("latest", digest_of(120)));
        with_noise.push(("sha-abc", digest_of(999)));
        let mut fake = Fake::new(&clock, &with_noise);
        fake.page = 40;
        let fake = Arc::new(fake);
        let found = round(&Arc::clone(&fake), "automagik-si/omni-api", &|| {
            clock.load(Ordering::SeqCst)
        });
        assert_eq!(fake.heads.load(Ordering::SeqCst), NEWEST, "the cap");
        assert_eq!(
            fake.pages.load(Ordering::SeqCst),
            4,
            "every page of 122 tags, 40 a page"
        );
        assert_eq!(
            found.get(&digest_of(120)).map(String::as_str),
            Some("v2.120")
        );
        assert_eq!(found.get(&digest_of(71)).map(String::as_str), Some("v2.71"));
        assert_eq!(
            found.get(&digest_of(70)),
            None,
            "the 51st newest is not asked"
        );
        assert_eq!(
            found.get(&digest_of(999)),
            None,
            "a tag of another shape is never asked"
        );
    }

    #[test]
    fn the_pages_read_are_bounded() {
        let clock = Arc::new(AtomicU64::new(0));
        let pairs: Vec<(String, String)> = (1..=100)
            .map(|n| (format!("v1.{n}"), digest_of(n)))
            .collect();
        let pairs: Vec<(&str, String)> = pairs
            .iter()
            .map(|(tag, digest)| (tag.as_str(), digest.clone()))
            .collect();
        let mut fake = Fake::new(&clock, &pairs);
        fake.page = 10;
        let fake = Arc::new(fake);
        round(&Arc::clone(&fake), "automagik-si/omni-api", &|| {
            clock.load(Ordering::SeqCst)
        });
        assert_eq!(fake.pages.load(Ordering::SeqCst), MAX_PAGES);
    }

    #[test]
    fn the_round_stops_at_its_one_deadline() {
        let clock = Arc::new(AtomicU64::new(0));
        let pairs: Vec<(String, String)> = (1..=50)
            .map(|n| (format!("v3.{n}"), digest_of(n)))
            .collect();
        let pairs: Vec<(&str, String)> = pairs
            .iter()
            .map(|(tag, digest)| (tag.as_str(), digest.clone()))
            .collect();
        let mut fake = Fake::new(&clock, &pairs);
        // Each HEAD takes a second of the five: the sixth never starts.
        fake.step = 1_000;
        let fake = Arc::new(fake);
        let found = round(&Arc::clone(&fake), "automagik-si/omni-api", &|| {
            clock.load(Ordering::SeqCst)
        });
        assert_eq!(fake.heads.load(Ordering::SeqCst), 5);
        assert_eq!(found.len(), 5);
        assert!(
            found.contains_key(&digest_of(50)),
            "the newest are asked first"
        );
    }

    #[test]
    fn n_proposals_for_n_digests_inside_the_interval_make_one_round() {
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let pairs: Vec<(String, String)> =
            (1..=5).map(|n| (format!("v2.{n}"), digest_of(n))).collect();
        let pairs: Vec<(&str, String)> = pairs
            .iter()
            .map(|(tag, digest)| (tag.as_str(), digest.clone()))
            .collect();
        let fake = Arc::new(Fake::new(&clock, &pairs));
        let versions = lookup(&fake, &clock);
        for n in 1..=5 {
            assert_eq!(
                versions.version(OMNI_IMAGE, &digest_of(n)),
                Some(Some(format!("v2.{n}")))
            );
            clock.fetch_add(30_000, Ordering::SeqCst);
        }
        assert_eq!(fake.opens.load(Ordering::SeqCst), 1, "one round");
        let after_one = fake.requests();
        assert_eq!(after_one, 1 + 1 + 5);
        // A second proposal for the same digest asks nothing.
        assert_eq!(
            versions.version(OMNI_IMAGE, &digest_of(3)),
            Some(Some("v2.3".into()))
        );
        assert_eq!(fake.requests(), after_one);
    }

    #[test]
    fn a_miss_on_a_fresh_map_asks_nothing_and_on_an_older_one_refreshes_once() {
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let fake = Arc::new(Fake::new(&clock, &[("v2.1", digest_of(1))]));
        let versions = lookup(&fake, &clock);
        assert_eq!(
            versions.version(OMNI_IMAGE, &digest_of(1)),
            Some(Some("v2.1".into()))
        );
        let after_one = fake.requests();
        // Fresh: a miss is "unknown" and asks nothing.
        clock.fetch_add(MISS_REFRESH_MS - 1, Ordering::SeqCst);
        assert_eq!(versions.version(OMNI_IMAGE, &digest_of(9)), Some(None));
        assert_eq!(fake.requests(), after_one);
        // Older than a minute: one refresh, then the refreshed map answers.
        clock.fetch_add(2, Ordering::SeqCst);
        assert_eq!(versions.version(OMNI_IMAGE, &digest_of(9)), Some(None));
        assert_eq!(fake.opens.load(Ordering::SeqCst), 2);
        let after_two = fake.requests();
        assert_eq!(versions.version(OMNI_IMAGE, &digest_of(9)), Some(None));
        assert_eq!(fake.requests(), after_two);
        // Past ten minutes even a named digest is asked again.
        clock.fetch_add(MAX_AGE_MS, Ordering::SeqCst);
        assert_eq!(
            versions.version(OMNI_IMAGE, &digest_of(1)),
            Some(Some("v2.1".into()))
        );
        assert_eq!(fake.opens.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_registry_failure_is_unknown_and_is_not_asked_again_inside_a_minute() {
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let mut fake = Fake::new(&clock, &[("v2.1", digest_of(1))]);
        fake.fail_open = true;
        let fake = Arc::new(fake);
        let versions = lookup(&fake, &clock);
        assert_eq!(versions.version(OMNI_IMAGE, &digest_of(1)), Some(None));
        assert_eq!(versions.version(OMNI_IMAGE, &digest_of(1)), Some(None));
        assert_eq!(fake.opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_image_that_is_not_looked_up_asks_nothing() {
        let clock = Arc::new(AtomicU64::new(0));
        let fake = Arc::new(Fake::new(&clock, &[("v2.1", digest_of(1))]));
        let versions = lookup(&fake, &clock);
        assert_eq!(
            versions.version("registry.example/app", &digest_of(1)),
            None
        );
        assert_eq!(
            versions.version("ghcr.io/automagik-si/other", &digest_of(1)),
            None
        );
        assert_eq!(fake.requests(), 0);
    }
}
