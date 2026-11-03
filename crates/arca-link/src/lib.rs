//! One node endpoint, one-time pairing and opaque streams to local unix sockets.

mod guard;
mod pairing;
mod server;
mod state;

pub use pairing::{PairLink, PairOutput, qr};
pub use server::{Link, Network, join_running};
pub use state::{Peer, State, Status};

use serde::Deserialize;
use std::{collections::BTreeMap, fmt, path::PathBuf};

pub const PAIR_ALPN: &[u8] = b"arca/pair/1";

/// The optional `link` section of the node config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub name: Option<String>,
    #[serde(default)]
    pub forward: BTreeMap<String, PathBuf>,
    pub pair_minutes: Option<u64>,
}

impl Config {
    pub fn check(&self) -> Result<()> {
        if self.forward.keys().any(|alpn| {
            alpn.is_empty() || alpn.len() > 255 || alpn == "arca/pair/1" || alpn == "arca/peers/1"
        }) {
            return Err(Error::Config(
                "link.forward contains an invalid or reserved ALPN",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum Error {
    InvalidLink,
    Config(&'static str),
    Runtime(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidLink => "invalid pairing link",
            Self::Config(message) | Self::Runtime(message) => message,
        })
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

fn runtime<T, E>(result: std::result::Result<T, E>) -> Result<T> {
    result.map_err(|_| Error::Runtime("link operation failed"))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
