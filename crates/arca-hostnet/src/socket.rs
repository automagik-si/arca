//! The root part's socket. Two verbs, `read` and `apply-declared-state`, neither with an argument: whatever else is sent is
//! refused, so no caller can make root do anything but read the host, or put back what the declared file says.
//!
//! A request is one line: the verb and a newline. A reply is one line of JSON: `{"ok":true,"status":{...}}`, or
//! `{"ok":false,"error":"..."}`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::root::Ops;

/// The verb that reads the host against the declared file.
pub const READ: &str = "read";
/// The verb that puts back the declared state.
pub const APPLY: &str = "apply-declared-state";
/// The most bytes of a request that are read. A request is a verb, which is shorter.
const MAX_REQUEST: u64 = 64;
/// How long a connection may hold the socket without sending its line.
const CLIENT_PATIENCE: Duration = Duration::from_secs(2);

/// The line a refused request is answered with. It does not echo what was sent.
pub const REFUSAL: &str =
    "the only verbs are read and apply-declared-state, and neither takes an argument";

/// Answers one request read from `request` onto `response`.
pub fn handle(request: impl Read, mut response: impl Write, ops: &dyn Ops) -> io::Result<()> {
    let mut line = Vec::new();
    BufReader::new(request.take(MAX_REQUEST + 1)).read_until(b'\n', &mut line)?;
    let line = line.strip_suffix(b"\n").unwrap_or(&line);
    let reply = match line {
        verb if verb == READ.as_bytes() => ok(&ops.read()),
        verb if verb == APPLY.as_bytes() => ok(&ops.apply()),
        _ => format!("{{\"ok\":false,\"error\":\"{REFUSAL}\"}}\n"),
    };
    response.write_all(reply.as_bytes())?;
    response.flush()
}

fn ok(status: &crate::status::Status) -> String {
    match serde_json::to_string(status) {
        Ok(status) => format!("{{\"ok\":true,\"status\":{status}}}\n"),
        Err(_) => "{\"ok\":false,\"error\":\"the status could not be written\"}\n".to_string(),
    }
}

/// Binds the socket at `path`, replacing a stale socket a crash left, and opens it to every user.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "something that is not a socket is already there",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Answers connections one at a time until `stop` is set.
pub fn serve(listener: &UnixListener, ops: Arc<dyn Ops + Send + Sync>, stop: &AtomicBool) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(CLIENT_PATIENCE));
                let _ = stream.set_write_timeout(Some(CLIENT_PATIENCE));
                let _ = handle(&stream, &stream, ops.as_ref());
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Status;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Counting {
        calls: Mutex<Vec<&'static str>>,
    }

    fn status(at: u64) -> Status {
        Status {
            at,
            declared: true,
            in_sync: true,
            drift: Vec::new(),
            applied: Vec::new(),
            errors: Vec::new(),
            version: "0.0.0".to_string(),
        }
    }

    impl Ops for Counting {
        fn read(&self) -> Status {
            self.calls.lock().expect("calls").push("read");
            status(1)
        }

        fn apply(&self) -> Status {
            self.calls.lock().expect("calls").push("apply");
            status(2)
        }
    }

    fn ask(request: &[u8], ops: &Counting) -> String {
        let mut reply = Vec::new();
        handle(request, &mut reply, ops).expect("answers");
        String::from_utf8(reply).expect("text")
    }

    #[test]
    fn the_two_verbs_are_answered_with_the_status_and_nothing_else_is() {
        let ops = Counting::default();
        let read = ask(b"read\n", &ops);
        assert!(read.starts_with("{\"ok\":true,\"status\":{"), "{read}");
        assert!(read.ends_with("}\n"));
        let apply = ask(b"apply-declared-state\n", &ops);
        assert!(apply.contains("\"at\":2"), "{apply}");
        // A client that closes its end without a newline is still understood.
        assert!(ask(b"read", &ops).contains("\"ok\":true"));
        assert_eq!(*ops.calls.lock().expect("calls"), ["read", "apply", "read"]);
    }

    #[test]
    fn anything_but_the_two_verbs_is_refused_and_runs_nothing() {
        let ops = Counting::default();
        let long = vec![b'a'; 4096];
        let refused: Vec<&[u8]> = vec![
            b"",
            b"\n",
            b"apply\n",
            b"READ\n",
            b" read\n",
            b"read \n",
            b"read\r\n",
            b"read --now\n",
            b"read extra\n",
            b"apply-declared-state --force\n",
            b"apply-declared-state now\n",
            b"apply-declared-state;reboot\n",
            b"{\"verb\":\"read\"}\n",
            b"iptables -F\n",
            b"read\0\n",
            &long,
        ];
        for request in refused {
            let reply = ask(request, &ops);
            assert_eq!(
                reply,
                format!("{{\"ok\":false,\"error\":\"{REFUSAL}\"}}\n"),
                "{request:?}"
            );
        }
        assert!(
            ops.calls.lock().expect("calls").is_empty(),
            "no refused request ran anything"
        );
    }

    #[test]
    fn a_real_socket_serves_the_verbs_to_any_client_and_refuses_the_rest() {
        let dir = std::env::temp_dir().join(format!("arca-hostnet-socket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("host.sock");
        // A stale socket file from a crash is replaced; a regular file is not.
        let stale = bind(&path).expect("binds");
        drop(stale);
        let listener = bind(&path).expect("binds over the stale socket");
        std::fs::write(dir.join("file"), "x").expect("file");
        assert!(bind(&dir.join("file")).is_err());
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o666
        );
        let ops = Arc::new(Counting::default());
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (ops, stop) = (Arc::clone(&ops), Arc::clone(&stop));
            thread::spawn(move || serve(&listener, ops, &stop))
        };
        let exchange = |text: &[u8]| {
            let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("connects");
            stream.write_all(text).expect("writes");
            let mut reply = String::new();
            stream.read_to_string(&mut reply).expect("reads");
            reply
        };
        assert!(exchange(b"apply-declared-state\n").contains("\"ok\":true"));
        assert!(exchange(b"reboot\n").contains("\"ok\":false"));
        assert_eq!(*ops.calls.lock().expect("calls"), ["apply"]);
        stop.store(true, Ordering::SeqCst);
        server.join().expect("stops");
        let _ = std::fs::remove_dir_all(dir);
    }
}
