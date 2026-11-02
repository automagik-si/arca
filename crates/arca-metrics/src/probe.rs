//! Latency probes: the TCP connect time to a target. A connect that completes gives its time in seconds. A connect that is refused,
//! times out or cannot be made gives `None`, which is no reading, never zero.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// The targets the latency gauge names. The address for each comes from `alerts.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeTarget {
    Gateway,
    Internet,
    Relay,
}

impl ProbeTarget {
    /// The label value the gauge writes for this target.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Internet => "internet",
            Self::Relay => "relay",
        }
    }
}

/// The seconds a TCP connect to `addr` takes, or `None` when the connect is refused, does not complete within `timeout`, or
/// cannot be started.
pub fn tcp_connect_seconds(addr: SocketAddr, timeout: Duration) -> Option<f64> {
    let started = Instant::now();
    let stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    let seconds = started.elapsed().as_secs_f64();
    drop(stream);
    Some(seconds)
}
