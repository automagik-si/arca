//! `setup-code [--config <file>]`: the setup code for the first passkey, on this terminal. It is for a machine with no screen
//! for the approval page's own "Get a setup code" to show the code on.
//!
//! The command asks the node that `serve` runs, over its unix socket, at `POST /local/setup-code`. First it makes an
//! empty file with a random name in the node's state directory, which only the node's user can, and sends that name: the
//! node answers only a caller whose file it finds there (see `approve_page::local_setup_code`). So the code reaches a
//! terminal of the node's own user and nobody else. The command prints the code, how long it lasts and the page it is typed
//! on, as JSON on stdout, and one sentence on stderr.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::approve_page::{LOCAL_PROOF_LEN, LOCAL_PROOF_PREFIX, LOCAL_SETUP_CODE_PATH};
use crate::{Failure, Flags};

/// The longest the node may take to answer.
const TIMEOUT: Duration = Duration::from_secs(10);

pub fn setup_code(flags: &Flags) -> Result<Option<Value>, Failure> {
    let (_, cfg) = crate::load_config_to(flags, crate::Does::Acts)?;
    let Some(origin) = cfg.approval_origin.clone() else {
        return Err(Failure::Config(
            "this node has no approval page, so there is nowhere to type a setup code: approvalOrigin is not set in its config"
                .to_string(),
        ));
    };
    let proof = fresh_proof()?;
    let file = cfg.state_dir.join(format!("{LOCAL_PROOF_PREFIX}{proof}"));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .map_err(|_| {
            Failure::Runtime(format!(
                "the node's state directory {} cannot be written: run this as the user who runs the node",
                cfg.state_dir.display()
            ))
        })?;
    let answered = ask(&cfg.listen.unix, &proof);
    // The node removes the file when it takes it. Whatever happened, none is left behind.
    let _ = fs::remove_file(&file);
    let (status, body) = answered?;
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let said = body.get("message").and_then(Value::as_str);
    match status {
        200 => {}
        404 => {
            return Err(Failure::Runtime(
                "the node that runs does not offer a setup code on its socket: it is an older program, or it started before approvalOrigin was set; start it again"
                    .to_string(),
            ));
        }
        409 => {
            return Err(Failure::Refused(
                "a passkey is already trusted on this node, so there is no setup code: more passkeys are added on the approval page, from one that is already trusted"
                    .to_string(),
            ));
        }
        _ => {
            return Err(Failure::Refused(match said {
                Some(said) => format!("the node gave no setup code: {said}"),
                None => "the node gave no setup code".to_string(),
            }));
        }
    }
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::Runtime("the node's answer has no code".to_string()))?;
    let seconds = body
        .get("expiresInSeconds")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let page = format!("{origin}/approve");
    eprintln!(
        "arca: type this code on the approval page ({page}) within {} minute(s). It works once, for the first passkey only.",
        seconds.div_ceil(60).max(1)
    );
    // The page of this machine alone, on a machine with no screen: how to bring it to the person's own computer, in
    // two sentences, and what it cannot do.
    if let Some(port) = arca_approval::localhost_origin_port(&origin) {
        eprintln!(
            "arca: no browser on this machine? On your own computer run: ssh -o ExitOnForwardFailure=yes -L {port}:127.0.0.1:{port} <this machine>"
        );
        eprintln!("arca: then open {page} in its browser: the page is this node's.");
        eprintln!(
            "arca: a phone cannot open this page. Approving from a phone needs the node reachable under a name, on a tailnet or a domain; the guide's section \"The approval page\" says how."
        );
    }
    Ok(Some(
        json!({ "code": code, "expiresInSeconds": seconds, "page": page }),
    ))
}

/// 16 random bytes from the operating system, in lowercase hexadecimal.
fn fresh_proof() -> Result<String, Failure> {
    let mut bytes = [0u8; LOCAL_PROOF_LEN / 2];
    getrandom::fill(&mut bytes).map_err(|_| {
        Failure::Runtime("the operating system's random source is not available".to_string())
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One POST to the node's unix socket. The reply's status and body.
fn ask(socket: &Path, proof: &str) -> Result<(u16, Vec<u8>), Failure> {
    let unreachable = || {
        Failure::Runtime(
            "the node's socket does not answer: the setup code comes from a node that is running"
                .to_string(),
        )
    };
    let mut stream = UnixStream::connect(socket).map_err(|_| unreachable())?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|_| unreachable())?;
    let payload = json!({ "proof": proof }).to_string();
    let request = format!(
        "POST {LOCAL_SETUP_CODE_PATH} HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| unreachable())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).map_err(|_| unreachable())?;
    crate::cmd_call::split_reply(&reply)
}
