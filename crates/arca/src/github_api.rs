//! The GitHub side of the signer: the App's key and its short-lived JWT, and one HTTP client for GitHub's REST API, its
//! web host (only addresses are built for it, nothing is sent there) and the Actions service.
//!
//! Nothing here keeps state or writes a file. The key is read by `crate::github_signer` from its own private store and
//! handed in as text; this module turns it into a signing key and signs JWTs with it. A [`Secret`] never prints: its
//! `Debug` says `<secret>` and it has no `Display`, so a credential cannot reach a log or an answer by a format string.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::DecodePrivateKey as _;
use rsa::rand_core::{CryptoRng, RngCore};
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// GitHub's REST API on github.com. A binding never names another: GitHub Enterprise is not served by the signer yet.
pub const GITHUB_API: &str = "https://api.github.com";
/// GitHub's web host on github.com: where the person's browser creates and installs the App.
pub const GITHUB_WEB: &str = "https://github.com";
/// The REST API version every request names.
const API_VERSION: &str = "2022-11-28";
/// The Actions service's API version, as GitHub's own scale-set client sends it.
pub const ACTIONS_API_VERSION: &str = "6.0-preview";
/// The time one request may take, connect included.
const REQUEST_BUDGET: Duration = Duration::from_secs(30);
/// The most of an answer's body that is read. GitHub's answers to the signer's requests are a few KiB.
const ANSWER_MAX: usize = 4 * 1024 * 1024;

/// A credential: an App key, a JWT, an installation or admin token, a runner's JIT configuration or an intake token.
/// It never prints, and the signer never writes one into a log, a journal or an answer other than the one it was made for.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one place it is sent to.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<secret>")
    }
}

/// Where GitHub is. Production is [`Endpoints::github`]; a test points both at its fake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The REST API, with no trailing slash.
    pub api: String,
    /// The web host, with no trailing slash.
    pub web: String,
}

impl Endpoints {
    pub fn github() -> Self {
        Self {
            api: GITHUB_API.to_string(),
            web: GITHUB_WEB.to_string(),
        }
    }
}

/// The DER prefix of a SHA-256 DigestInfo (RFC 8017, section 9.2, note 1).
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// The App's private key, parsed. It signs JWTs and nothing else, and it is never written back out.
pub struct AppKey {
    key: RsaPrivateKey,
}

impl fmt::Debug for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppKey(<secret>)")
    }
}

impl AppKey {
    /// Reads a PEM key as GitHub hands it out (`RSA PRIVATE KEY`, PKCS#1) or as PKCS#8 (`PRIVATE KEY`). The refusal
    /// never quotes the text.
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let (label, der) = pem_body(pem).ok_or("the App key is not a PEM private key")?;
        let key = match label.as_str() {
            "RSA PRIVATE KEY" => RsaPrivateKey::from_pkcs1_der(&der).ok(),
            "PRIVATE KEY" => RsaPrivateKey::from_pkcs8_der(&der).ok(),
            _ => None,
        }
        .ok_or("the App key is not an RSA private key")?;
        Ok(Self { key })
    }

    /// An App JWT for `issuer` (the App's client ID or its numeric id), valid from a minute before `now_secs` (GitHub's
    /// advice for clock drift) for nine minutes after it, under GitHub's ten-minute ceiling.
    pub fn jwt(&self, issuer: &str, now_secs: u64) -> Result<Secret, String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iat": now_secs.saturating_sub(60),
            "exp": now_secs + 540,
            "iss": issuer,
        });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{payload}");
        let mut hashed = SHA256_DIGEST_INFO.to_vec();
        hashed.extend_from_slice(&Sha256::digest(input.as_bytes()));
        let signature = self
            .key
            .sign_with_rng(&mut OsRandom, Pkcs1v15Sign::new_unprefixed(), &hashed)
            .map_err(|_| "the App key did not sign".to_string())?;
        Ok(Secret(format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }
}

/// The size of a runner's own key, as the runner's `config.sh` makes it (`RSAFileKeyManager`).
pub const RUNNER_KEY_BITS: usize = 2048;

/// A registered runner's own key pair, made by the signer the way the runner's `config.sh` makes it. The public half is
/// sent to GitHub at registration; the private half is the runner's lasting credential and goes, with the rest of the
/// runner's files, only to the machine that runs it.
pub struct RunnerKey {
    key: RsaPrivateKey,
}

impl RunnerKey {
    /// A fresh key of `bits` bits. Production uses [`RUNNER_KEY_BITS`]; a test may use fewer, to stay fast.
    pub fn generate(bits: usize) -> Result<Self, String> {
        let key = RsaPrivateKey::new(&mut OsRandom, bits)
            .map_err(|_| "a runner key could not be made".to_string())?;
        Ok(Self { key })
    }

    /// The public key as .NET's `RSA.ToXmlString(false)` writes it: the form the runner-admin registration takes.
    pub fn public_xml(&self) -> String {
        use rsa::traits::PublicKeyParts as _;
        format!(
            "<RSAKeyValue><Modulus>{}</Modulus><Exponent>{}</Exponent></RSAKeyValue>",
            STANDARD.encode(self.key.n().to_bytes_be()),
            STANDARD.encode(self.key.e().to_bytes_be())
        )
    }

    /// The public key as the Actions service's agent record takes it: `{exponent, modulus}`, base64.
    pub fn public_json(&self) -> Value {
        use rsa::traits::PublicKeyParts as _;
        json!({
            "exponent": STANDARD.encode(self.key.e().to_bytes_be()),
            "modulus": STANDARD.encode(self.key.n().to_bytes_be()),
        })
    }

    /// The runner's `.credentials_rsaparams` file: .NET's `RSAParameters` as the runner serialises them, each number
    /// big-endian and padded to the length .NET's import requires (the modulus's for `d`, half of it for the five CRT
    /// numbers).
    pub fn rsaparams(&self) -> Result<Secret, String> {
        use rsa::traits::{PrivateKeyParts as _, PublicKeyParts as _};
        let mut key = self.key.clone();
        key.precompute()
            .map_err(|_| "a runner key could not be completed".to_string())?;
        let size = key.size();
        let half = size.div_ceil(2);
        let primes = key.primes();
        let (Some(p), Some(q)) = (primes.first(), primes.get(1)) else {
            return Err("a runner key has no two primes".to_string());
        };
        let dp = key.dp().ok_or("a runner key has no dp")?;
        let dq = key.dq().ok_or("a runner key has no dq")?;
        let inverse_q = key
            .crt_coefficient()
            .ok_or("a runner key has no inverse q")?;
        let padded = |bytes: Vec<u8>, len: usize| {
            let mut out = vec![0u8; len.saturating_sub(bytes.len())];
            out.extend_from_slice(&bytes);
            STANDARD.encode(out)
        };
        let value = json!({
            "d": padded(key.d().to_bytes_be(), size),
            "dp": padded(dp.to_bytes_be(), half),
            "dq": padded(dq.to_bytes_be(), half),
            "exponent": STANDARD.encode(key.e().to_bytes_be()),
            "inverseQ": padded(inverse_q.to_bytes_be(), half),
            "modulus": STANDARD.encode(key.n().to_bytes_be()),
            "p": padded(p.to_bytes_be(), half),
            "q": padded(q.to_bytes_be(), half),
        });
        Ok(Secret(value.to_string()))
    }
}

/// The label and DER bytes of the first PEM block in `text`.
fn pem_body(text: &str) -> Option<(String, Vec<u8>)> {
    let text = text.replace("\r\n", "\n");
    let begin = text.find("-----BEGIN ")?;
    let rest = &text[begin + "-----BEGIN ".len()..];
    let label_end = rest.find("-----")?;
    let label = rest[..label_end].to_string();
    let body_start = label_end + 5;
    let end_marker = format!("-----END {label}-----");
    let body_end = rest.find(&end_marker)?;
    let body: String = rest[body_start..body_end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = STANDARD.decode(body).ok()?;
    Some((label, der))
}

/// The operating system's random numbers, for RSA blinding.
struct OsRandom;

impl RngCore for OsRandom {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }
    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        getrandom::fill(dest).expect("the operating system gives random bytes");
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
        getrandom::fill(dest).map_err(|_| {
            rsa::rand_core::Error::from(
                NonZeroU32::new(rsa::rand_core::Error::CUSTOM_START).expect("a non-zero code"),
            )
        })
    }
}

impl CryptoRng for OsRandom {}

/// How a request authenticates.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    None,
    /// `Authorization: Bearer <value>`: an App JWT, an installation token or the Actions service's admin token.
    Bearer(&'a Secret),
    /// `Authorization: RemoteAuth <value>`: a runner registration token, for the runner-registration calls alone.
    RemoteAuth(&'a Secret),
    /// HTTP Basic with an OAuth App's client ID and secret, for deleting a grant.
    Basic(&'a str, &'a Secret),
}

/// One answer from GitHub: the status and the body.
#[derive(Clone, Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON, or `Null` when it is not JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// GitHub's own words for a refusal (its `message` field), cut to 300 characters, or the status alone. This text came
    /// from outside Arca: it is shown as data.
    pub fn why(&self) -> String {
        let message = self
            .json()
            .get("message")
            .and_then(Value::as_str)
            .map(|text| text.chars().take(300).collect::<String>());
        match message {
            Some(text) if !text.is_empty() => format!("GitHub answered {}: {text}", self.status),
            _ => format!("GitHub answered {}", self.status),
        }
    }
}

/// The one HTTP client the signer uses. It follows no redirect, so a credential is only ever sent to the address it was
/// made for.
#[derive(Clone)]
pub struct GitHubHttp {
    client: reqwest::Client,
    endpoints: Arc<Endpoints>,
}

impl GitHubHttp {
    /// A client for `endpoints`, with TLS through rustls and ring and the Mozilla roots of webpki-roots.
    pub fn new(endpoints: Endpoints) -> Result<Self, String> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS has no protocol version".to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_BUDGET)
            .user_agent(concat!("arca-github-signer/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "the HTTPS client could not be built".to_string())?;
        Ok(Self {
            client,
            endpoints: Arc::new(endpoints),
        })
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// `path` on the REST API.
    pub fn api(&self, path: &str) -> String {
        format!("{}{path}", self.endpoints.api)
    }

    /// `path` on the web host.
    pub fn web(&self, path: &str) -> String {
        format!("{}{path}", self.endpoints.web)
    }

    /// Sends one request. `Err` is a request that got no answer (no connection, the time ran out, a cut body), in words
    /// that hold no credential.
    pub async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        auth: Auth<'_>,
        body: Option<&Value>,
    ) -> Result<Answer, String> {
        // GitHub's OAuth endpoints on the web host answer JSON only to `application/json`; the REST API takes its own type.
        let accept = if url.starts_with(&format!("{}/", self.endpoints.web)) {
            "application/json"
        } else {
            "application/vnd.github+json"
        };
        let mut request = self
            .client
            .request(method, url)
            .header("accept", accept)
            .header("x-github-api-version", API_VERSION);
        request = match auth {
            Auth::None => request,
            Auth::Bearer(secret) => {
                request.header("authorization", format!("Bearer {}", secret.expose()))
            }
            Auth::RemoteAuth(secret) => {
                request.header("authorization", format!("RemoteAuth {}", secret.expose()))
            }
            Auth::Basic(user, secret) => request.header(
                "authorization",
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{user}:{}", secret.expose()))
                ),
            ),
        };
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                "GitHub did not answer in time".to_string()
            } else if error.is_connect() {
                "GitHub could not be reached".to_string()
            } else {
                "the request to GitHub failed".to_string()
            }
        })?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "GitHub's answer was cut off".to_string())?
        {
            if body.len() + chunk.len() > ANSWER_MAX {
                return Err("GitHub's answer was too long".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Answer { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints() {
        let secret = Secret::new("ghs_example");
        assert_eq!(format!("{secret:?}"), "<secret>");
        assert_eq!(format!("{:?}", Some(&secret)), "Some(<secret>)");
    }

    #[test]
    fn a_pem_body_is_read_with_either_line_ending() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\r\nAAEC\r\nAwQ=\r\n-----END RSA PRIVATE KEY-----\r\n";
        let (label, der) = pem_body(pem).expect("a block");
        assert_eq!(label, "RSA PRIVATE KEY");
        assert_eq!(der, vec![0, 1, 2, 3, 4]);
        assert!(pem_body("no key here").is_none());
        assert!(AppKey::from_pem(pem).is_err());
    }

    #[test]
    fn a_refusal_names_githubs_words_or_the_status() {
        let answer = Answer {
            status: 404,
            body: br#"{"message":"Not Found"}"#.to_vec(),
        };
        assert_eq!(answer.why(), "GitHub answered 404: Not Found");
        let answer = Answer {
            status: 502,
            body: b"<html>".to_vec(),
        };
        assert_eq!(answer.why(), "GitHub answered 502");
    }
}
