use crate::{Error, PAIR_ALPN, Peer, Result, State, now_ms, runtime, state::Pending};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{Endpoint, EndpointId};
use iroh_tickets::endpoint::EndpointTicket;
use qrcodegen::{QrCode, QrCodeEcc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Serialize)]
pub struct PairOutput {
    pub link: String,
    pub expires: u64,
}

/// Parsed without retaining the input in errors or debug output.
pub struct PairLink {
    pub name: String,
    pub(crate) code: String,
    pub(crate) ticket: EndpointTicket,
}

impl PairLink {
    pub fn parse(text: &str) -> Result<Self> {
        if text
            .bytes()
            .any(|b| b.is_ascii_control() || b == b' ' || b == b'#')
        {
            return Err(Error::InvalidLink);
        }
        let query = text
            .strip_prefix("arca://pair?")
            .ok_or(Error::InvalidLink)?;
        let mut fields = BTreeMap::new();
        for part in query.split('&') {
            let (key, value) = part.split_once('=').ok_or(Error::InvalidLink)?;
            if !["v", "n", "c", "t"].contains(&key) || fields.insert(key, decode(value)?).is_some()
            {
                return Err(Error::InvalidLink);
            }
        }
        if fields.len() != 4 || fields.get("v").map(String::as_str) != Some("1") {
            return Err(Error::InvalidLink);
        }
        let code = fields.remove("c").ok_or(Error::InvalidLink)?;
        if URL_SAFE_NO_PAD
            .decode(&code)
            .map_err(|_| Error::InvalidLink)?
            .len()
            != 16
        {
            return Err(Error::InvalidLink);
        }
        let ticket = fields
            .remove("t")
            .ok_or(Error::InvalidLink)?
            .parse()
            .map_err(|_| Error::InvalidLink)?;
        Ok(Self {
            name: fields.remove("n").ok_or(Error::InvalidLink)?,
            code,
            ticket,
        })
    }
}

fn encode(text: &str) -> String {
    let mut result = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(result, "%{byte:02X}");
        }
    }
    result
}

fn decode(text: &str) -> Result<String> {
    let mut result = Vec::new();
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = bytes
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or(Error::InvalidLink)?;
            let lo = bytes
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or(Error::InvalidLink)?;
            result.push((hi * 16 + lo) as u8);
        } else {
            result.push(byte);
        }
    }
    String::from_utf8(result).map_err(|_| Error::InvalidLink)
}

impl State {
    pub fn pair(&self, minutes: u64) -> Result<PairOutput> {
        self.pair_at(minutes, now_ms())
    }

    /// Allows callers to supply a clock, including expiry tests without sleeping.
    pub fn pair_at(&self, minutes: u64, now: u64) -> Result<PairOutput> {
        let _lock = self.lock()?;
        let status = self.status()?;
        let mut random = [0; 16];
        runtime(getrandom::fill(&mut random))?;
        let code = URL_SAFE_NO_PAD.encode(random);
        let expires = now.saturating_add(minutes.clamp(1, 60) * 60_000);
        self.write(
            "pairing.json",
            &Pending {
                code: code.clone(),
                expires,
                name: status.name.clone(),
                failures: 0,
                tries: BTreeMap::new(),
            },
        )?;
        Ok(PairOutput {
            link: format!(
                "arca://pair?v=1&n={}&c={code}&t={}",
                encode(&status.name),
                encode(&status.ticket)
            ),
            expires,
        })
    }

    pub(crate) fn redeem(&self, id: EndpointId, request: Request) -> Result<Response> {
        let _lock = self.lock()?;
        let Some(mut pending): Option<Pending> = self.read("pairing.json")? else {
            return Ok(Response::refused("no pending code"));
        };
        let id = id.to_string();
        if pending.expires <= now_ms() {
            return Ok(Response::refused("code expired"));
        }
        if pending.failures >= 1000 || pending.tries.get(&id).copied().unwrap_or(0) >= 3 {
            return Ok(Response::refused("code exhausted"));
        }
        let expected = Sha256::digest(pending.code.as_bytes());
        let supplied = Sha256::digest(request.code.as_bytes());
        if !bool::from(expected.as_slice().ct_eq(supplied.as_slice())) {
            pending.failures += 1;
            *pending.tries.entry(id).or_default() += 1;
            self.write("pairing.json", &pending)?;
            return Ok(Response::refused("wrong code"));
        }
        self.delete("pairing.json")?;
        let device: String = request
            .device
            .chars()
            .filter(|c| *c > '\u{001f}')
            .take(60)
            .collect();
        let device = if device.is_empty() {
            "device".to_string()
        } else {
            device
        };
        self.add_locked(id, device)?;
        Ok(Response {
            ok: true,
            name: Some(pending.name),
            error: None,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub code: String,
    pub device: String,
}
#[derive(Deserialize, Serialize)]
pub(crate) struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Response {
    pub(crate) fn refused(why: &str) -> Self {
        Self {
            ok: false,
            name: None,
            error: Some(why.to_string()),
        }
    }
}

pub(crate) async fn read_line<R: AsyncRead + Unpin>(recv: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    for _ in 0..4096 {
        let byte = runtime(recv.read_u8().await)?;
        if byte == b'\n' {
            return Ok(line);
        }
        line.push(byte);
    }
    Err(Error::Runtime("link JSON line exceeds 4096 bytes"))
}

pub(crate) async fn write_line<W: AsyncWrite + Unpin, T: Serialize>(
    send: &mut W,
    value: &T,
) -> Result<()> {
    let mut bytes = runtime(serde_json::to_vec(value))?;
    bytes.push(b'\n');
    if bytes.len() > 4096 {
        return Err(Error::Runtime("link JSON line exceeds 4096 bytes"));
    }
    runtime(send.write_all(&bytes).await)
}

pub(crate) async fn join(
    endpoint: &Endpoint,
    state: &State,
    name: &str,
    link: &PairLink,
) -> Result<Peer> {
    let operation = async {
        let connection = runtime(
            endpoint
                .connect(link.ticket.endpoint_addr().clone(), PAIR_ALPN)
                .await,
        )?;
        let result = async {
            let (mut send, mut recv) = runtime(connection.open_bi().await)?;
            write_line(
                &mut send,
                &Request {
                    code: link.code.clone(),
                    device: name.to_string(),
                },
            )
            .await?;
            runtime(send.finish())?;
            let response: Response = runtime(serde_json::from_slice(&read_line(&mut recv).await?))?;
            if !response.ok {
                return Err(Error::Runtime("pairing refused"));
            }
            let host_name = response
                .name
                .ok_or(Error::Runtime("invalid pairing reply"))?;
            state.add(connection.remote_id().to_string(), host_name)
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    };
    runtime(tokio::time::timeout(std::time::Duration::from_secs(10), operation).await)?
}

/// A quiet zone of four modules; two rows per terminal character.
pub fn qr(text: &str) -> Result<String> {
    let code = runtime(QrCode::encode_text(text, QrCodeEcc::Low))?;
    let size = code.size();
    let mut output = String::new();
    for y in (-4..size + 4).step_by(2) {
        output.push_str("\x1b[30;47m");
        for x in -4..size + 4 {
            output.push(match (code.get_module(x, y), code.get_module(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        output.push_str("\x1b[0m\n");
    }
    Ok(output)
}
