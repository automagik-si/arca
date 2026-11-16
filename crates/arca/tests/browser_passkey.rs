//! A real browser passkey ceremony on the approval page: `arca serve` in its own process on a scratch state directory, over a
//! fake Incus, its page served at `https://localhost:<port>` through a TLS front with a throwaway certificate, and a headless
//! Chromium whose WebAuthn virtual authenticators (CDP `WebAuthn.addVirtualAuthenticator`) play the owner's devices.
//!
//! The page's own JavaScript runs every ceremony: the first passkey is registered with the setup code (a stub
//! `notify-send` on the server's `PATH` stands in for the desktop), a second is added on the `passkey_add` path and
//! confirmed by the first, the first approves a delete plan, and the second approves an owner approval request, whose
//! requester then spends the approval through the real `service_remove`. The test reads back the spend's audit outcome
//! (`via=passkey`, the key's fingerprint, its sign count and its synced flag), the kept sign counts and the spent nonces.
//!
//! Synthetic-key tests sign their own assertions; this one checks what a browser really sends: the credential the page
//! registers, the one it selects, the origin and rp it reports, and the flags and counters of a real authenticator.
//!
//! It needs Chromium and `openssl`, so it is ignored by default. Run it with:
//!
//! ```text
//! cargo test -p arca --test browser_passkey -- --ignored --nocapture
//! ```
//!
//! `ARCA_TEST_CHROME` names another Chromium binary (default `/usr/bin/google-chrome`). Every secret stays in the scratch
//! directory, which is removed when the test ends; nothing is printed but the outcome. Chromium runs with `HOME` and its
//! profile in that directory and with its background networking and component updates off.
//!
//! The replay: once the requester has spent the approval, `nats` is created again under the same name and the approval is
//! sent again, so the call reaches the approval check rather than a not_found. Both devices are device-bound passkeys that
//! count, and Arca checks the count before the nonce, so the replay is refused by the sign count (`owner_approval`,
//! `SignCountRegressed`), with no nonce spent and the kept count unchanged. The nonce refusal (`Replayed`) belongs to a
//! synced passkey that reports count 0. Chromium's virtual authenticator cannot be one: with backup eligibility and state
//! set it still counts (Chrome 155: 2, then 3). That path is proved in process, with a synced software passkey at count 0,
//! by `g7_a_spent_approval_re_encoded_as_its_high_s_twin_verifies_and_is_refused_as_a_replay_by_its_nonce`.
//!
//! The server is not a scratch run (`kernel.scratch` stays false): a scratch run takes only `arcak-scratch-*` projects, and
//! the service and postgres verbs this test drives work only in the `arcak-svc` project, so serve would refuse the config.
//! The state directory, the token directory, the fake Incus and the trust are all the test's own.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_approval::load_state_passkeys;
use arca_incus::{FakeIncus, FakeState, IncusRead as _};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WAIT: Duration = Duration::from_secs(30);
const CHROME_ENV: &str = "ARCA_TEST_CHROME";
const DEFAULT_CHROME: &str = "/usr/bin/google-chrome";
const APPROVED_REQUEST: &str = "Approved. Your assistant can go ahead now.";

/// A fresh directory under the default temporary directory, removed when the test ends. The default keeps the unix socket
/// path short.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-pk-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        Scratch(dir)
    }

    fn at(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    /// Removes the directory, retrying while a browser helper that outlived the browser still writes its profile.
    fn drop(&mut self) {
        for _ in 0..50 {
            if fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
}

/// The binary, with no home directory and no variable to take a config or a token from: a call that names neither must
/// never read the files of the user who runs the tests.
fn arca() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arca"));
    command
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE")
        // No screen, unless a test says there is one: what `init` and `status` say next depends on it, and must not
        // depend on where the tests run.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    command
}

fn run(command: &mut Command, what: &str) {
    let out = command.output().unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(
        out.status.success(),
        "{what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A throwaway P-256 certificate for `localhost`, made by `openssl`: the DER certificate, the PKCS#8 key, and the base64
/// SHA-256 of its public key, which Chromium is told to accept (`--ignore-certificate-errors-spki-list`). With that flag the
/// page is a valid secure context, so WebAuthn runs as it does behind the owner's real proxy.
fn certificate(dir: &Path) -> (Vec<u8>, Vec<u8>, String) {
    private_dir(dir);
    run(
        Command::new("openssl")
            .current_dir(dir)
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-nodes",
                "-keyout",
                "key.pem",
                "-out",
                "cert.pem",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
            ])
            .stderr(Stdio::piped()),
        "openssl makes the certificate",
    );
    run(
        Command::new("openssl").current_dir(dir).args([
            "x509", "-in", "cert.pem", "-outform", "DER", "-out", "cert.der",
        ]),
        "openssl writes the DER certificate",
    );
    run(
        Command::new("openssl").current_dir(dir).args([
            "pkcs8", "-topk8", "-nocrypt", "-in", "key.pem", "-outform", "DER", "-out", "key.der",
        ]),
        "openssl writes the PKCS#8 key",
    );
    run(
        Command::new("sh").current_dir(dir).arg("-c").arg(
            "openssl x509 -in cert.pem -pubkey -noout | openssl pkey -pubin -outform DER -out spki.der",
        ),
        "openssl writes the public key",
    );
    let spki = fs::read(dir.join("spki.der")).expect("the public key");
    (
        fs::read(dir.join("cert.der")).expect("the certificate"),
        fs::read(dir.join("key.der")).expect("the key"),
        STANDARD.encode(Sha256::digest(&spki)),
    )
}

/// The TLS front, as the owner's reverse proxy is: TLS from the browser, plain HTTP to the server's unix socket, byte for byte.
fn tls_front(listener: std::net::TcpListener, cert: Vec<u8>, key: Vec<u8>, upstream: PathBuf) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .expect("the certificate loads");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    listener.set_nonblocking(true).expect("nonblocking");
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("the listener");
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    continue;
                };
                let (acceptor, upstream) = (acceptor.clone(), upstream.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut unix) = tokio::net::UnixStream::connect(&upstream).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut unix).await;
                });
            }
        });
    });
}

/// What a test's node is started through: a shell that closes every file it was handed but its three standard ones,
/// then becomes the program. A test process may hold files that are not its own to pass on, such as the descriptor of a
/// lock its runner took; a node that inherited one would keep it for as long as it lived.
const CLOSE_THEN_EXEC: &str = r#"for fd in /proc/$$/fd/*; do fd=${fd##*/}; case $fd in 0|1|2) ;; *) eval "exec $fd>&-" 2>/dev/null ;; esac; done; exec "$0" "$@""#;

/// `arca serve --config <config>` as a command to spawn, with no file of the test's process but its standard three.
fn serve_command(config: &Path) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(CLOSE_THEN_EXEC)
        .arg(env!("CARGO_BIN_EXE_arca"))
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE")
        .args(["serve", "--config"])
        .arg(config);
    command
}

/// A server process with its stderr in a file, killed if the test ends first.
struct Serve {
    child: Option<Child>,
    log: PathBuf,
}

impl Serve {
    fn start(config: &Path, bin: &Path, log: PathBuf) -> Serve {
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let child = serve_command(config)
            .env("PATH", path)
            .stdout(Stdio::null())
            .stderr(File::create(&log).expect("the server log"))
            .spawn()
            .expect("start arca serve");
        Serve {
            child: Some(child),
            log,
        }
    }

    fn wait_for(&mut self, socket: &Path) {
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(socket).is_err() {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("running")
                .try_wait()
                .expect("poll")
            {
                panic!(
                    "the server exited before listening ({status}): {}",
                    self.log_text()
                );
            }
            assert!(Instant::now() < deadline, "the server listens in time");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn terminate(&mut self) -> Option<i32> {
        let mut child = self.child.take()?;
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = child.try_wait().expect("poll") {
                return status.code();
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One `tools/call` over the unix socket. Returns the JSON-RPC reply.
fn mcp(socket: &Path, secret: &str, id: u64, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
    .to_string();
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("a deadline");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nAuthorization: Bearer {secret}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    let text = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| panic!("{name} answers JSON-RPC"))
}

/// The structured result of a call that must succeed.
fn result(reply: &Value, what: &str) -> Value {
    assert!(reply.get("error").is_none(), "{what} succeeds: {reply}");
    assert_ne!(reply["result"]["isError"], true, "{what} succeeds: {reply}");
    reply["result"]["structuredContent"].clone()
}

/// A Chromium driven over `--remote-debugging-pipe`: CDP messages are JSON, each ended by a NUL byte, read from fd 3 and
/// written to fd 4. `sh` puts the child's stdin and stdout there, so no WebSocket client and no unsafe code is needed.
struct Chrome {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Value>,
    events: VecDeque<Value>,
    next: u64,
    session: String,
}

impl Chrome {
    /// Launches `binary` with its profile and `HOME` in the scratch directory, and with no background traffic of its own.
    fn launch(binary: &str, home: &Path, spki: &str, log: &Path) -> Chrome {
        private_dir(home);
        let profile = home.join("profile");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(r#"exec "$0" "$@" 3<&0 4>&1 </dev/null >/dev/null"#)
            .arg(binary)
            .args([
                "--headless=new",
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--password-store=basic",
            ])
            .env("HOME", home)
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--ignore-certificate-errors-spki-list={spki}"))
            .arg("about:blank")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(log).expect("the browser log"))
            .spawn()
            .unwrap_or_else(|e| panic!("start {binary}: {e}"));
        let input = child.stdin.take().expect("the CDP input");
        let output = child.stdout.take().expect("the CDP output");
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(output);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(0, &mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if buffer.last() == Some(&0) {
                    buffer.pop();
                }
                if let Ok(message) = serde_json::from_slice::<Value>(&buffer)
                    && sender.send(message).is_err()
                {
                    return;
                }
            }
        });
        let mut chrome = Chrome {
            child,
            input,
            messages,
            events: VecDeque::new(),
            next: 0,
            session: String::new(),
        };
        let target = chrome.send(None, "Target.createTarget", json!({ "url": "about:blank" }));
        let attached = chrome.send(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target["targetId"], "flatten": true }),
        );
        chrome.session = attached["sessionId"]
            .as_str()
            .expect("a session")
            .to_string();
        chrome
    }

    /// Sends one command, to the page's session or the browser, and waits for its answer. Events read meanwhile are kept.
    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> Value {
        self.next += 1;
        let mut message = json!({ "id": self.next, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        self.input.write_all(&bytes).expect("write to the browser");
        self.input.flush().expect("flush");
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(left) {
                Ok(reply) if reply["id"] == json!(self.next) => {
                    assert!(reply.get("error").is_none(), "{method}: {reply}");
                    return reply["result"].clone();
                }
                Ok(event) => self.events.push_back(event),
                Err(RecvTimeoutError::Timeout) => panic!("{method} got no answer in time"),
                Err(RecvTimeoutError::Disconnected) => panic!("the browser exited during {method}"),
            }
        }
    }

    fn page(&mut self, method: &str, params: Value) -> Value {
        let session = self.session.clone();
        self.send(Some(&session), method, params)
    }

    /// Waits for the next event named `method`.
    fn event(&mut self, method: &str) -> Value {
        if let Some(at) = self.events.iter().position(|e| e["method"] == method) {
            return self.events.remove(at).expect("the event")["params"].clone();
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(left) {
                Ok(event) if event["method"] == method => return event["params"].clone(),
                Ok(other) => self.events.push_back(other),
                Err(_) => panic!("no {method} event in time"),
            }
        }
    }

    /// Evaluates `js` in the page as a user gesture, awaiting a promise, and returns its value.
    fn eval(&mut self, js: &str) -> Value {
        let answer = self.page(
            "Runtime.evaluate",
            json!({ "expression": js, "awaitPromise": true, "returnByValue": true, "userGesture": true }),
        );
        if let Some(thrown) = answer.get("exceptionDetails") {
            panic!("the page threw on {js}: {thrown}");
        }
        answer["result"]["value"].clone()
    }

    /// Clicks the button `selector` once it is on the page, enabled, and not held off: the page holds taps for a moment
    /// after its cards moved (`inert`), and a person's tap does nothing there.
    fn click(&mut self, selector: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let found = self.eval(&format!(
                "(() => {{ const b = document.querySelector({selector:?}); if (!b || b.disabled || b.closest('[inert]')) return false; b.click(); return true; }})()"
            ));
            if found == json!(true) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{selector} is on the page and enabled"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// A person's tap at `(x, y)` in the page's viewport: a real mouse press and release, which an `inert` part of the page
    /// does not take.
    fn tap_at(&mut self, x: f64, y: f64) {
        for kind in ["mousePressed", "mouseReleased"] {
            self.page(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            );
        }
    }

    /// Waits until the page's status line reads `want` (its start) in the ok tone. A bad or warning tone fails at once.
    fn status_ok(&mut self, want: &str, serve: &Serve) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let seen = self.eval(
                "(() => { const s = document.getElementById('status'); return [s.textContent, s.dataset.tone || ''] })()",
            );
            let (text, tone) = (
                seen[0].as_str().unwrap_or_default().to_string(),
                seen[1].as_str().unwrap_or_default().to_string(),
            );
            if tone == "ok" && text.starts_with(want) {
                return text;
            }
            if tone == "bad" || tone == "warn" {
                panic!(
                    "the page says {text:?} ({tone}) while waiting for {want:?}; server log:\n{}",
                    serve.log_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the page reads {want:?} in time, it reads {text:?}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// A virtual device holding passkeys: `internal` is the laptop's own, `cable` a phone's over the hybrid transport. Chrome
    /// keeps at most one `internal` device per browser.
    fn authenticator(&mut self, transport: &str) -> String {
        let added = self.page(
            "WebAuthn.addVirtualAuthenticator",
            json!({ "options": {
                "protocol": "ctap2", "transport": transport, "hasResidentKey": true,
                "hasUserVerification": true, "isUserVerified": true, "automaticPresenceSimulation": true
            }}),
        );
        added["authenticatorId"]
            .as_str()
            .expect("an id")
            .to_string()
    }

    /// Whether `authenticator` answers a request at once (`true`) or never confirms presence (`false`), so only the others
    /// answer it.
    fn presence(&mut self, authenticator: &str, on: bool) {
        self.page(
            "WebAuthn.setAutomaticPresenceSimulation",
            json!({ "authenticatorId": authenticator, "enabled": on }),
        );
    }
}

impl Drop for Chrome {
    /// Closes the browser and waits for it, so its profile is no longer written when the scratch directory goes.
    fn drop(&mut self) {
        let mut bytes = json!({ "id": 0, "method": "Browser.close" })
            .to_string()
            .into_bytes();
        bytes.push(0);
        let _ = self.input.write_all(&bytes);
        let _ = self.input.flush();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The passkeys the server keeps in its state directory, by fingerprint, in file order.
fn passkeys(state: &Path) -> Vec<arca_approval::PasskeyLine> {
    load_state_passkeys(&state.join(arca_approval::STATE_PASSKEYS_FILE)).expect("the passkeys file")
}

/// The last sign count the server kept for `fingerprint`.
fn kept_count(state: &Path, fingerprint: &str) -> Option<u64> {
    let text =
        fs::read_to_string(state.join(arca_approval::PASSKEY_COUNTS_FILE)).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["fingerprint"] == fingerprint)
        .filter_map(|record| record["count"].as_u64())
        .next_back()
}

fn lines(path: &Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

fn instance_exists(fake: &FakeIncus, name: &str) -> bool {
    arca_incus::UnixIncus::new(fake.socket().to_path_buf())
        .get(&format!(
            "/1.0/instances/{name}?project={}",
            arca_netproject::PROJECT
        ))
        .is_ok()
}

#[test]
#[ignore = "drives a real Chromium; run with: cargo test -p arca --test browser_passkey -- --ignored"]
fn g8_a_real_browser_registers_two_passkeys_and_approves_a_request_and_a_delete_plan_on_the_page() {
    let chrome_binary = std::env::var(CHROME_ENV).unwrap_or_else(|_| DEFAULT_CHROME.to_string());
    let scratch = Scratch::new("browser");
    let state = scratch.at("state");
    private_dir(&state);
    let hash_dir = scratch.at("tokens");
    private_dir(&hash_dir);

    // The Doctor's token, minted by the binary.
    let secret_file = scratch.at("doctor.secret");
    run(
        arca()
            .args([
                "token",
                "mint",
                "--scope",
                "doctor",
                "--id",
                "doctor-1",
                "--hash-dir",
            ])
            .arg(&hash_dir)
            .arg("--out")
            .arg(&secret_file),
        "token mint",
    );
    let doctor = fs::read_to_string(&secret_file)
        .expect("the secret")
        .trim_end()
        .to_string();

    // The fake Incus, a service template, and a stub desktop that keeps the setup code in a private file.
    let fake = FakeIncus::start(
        &scratch.0,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("the fake Incus");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
                "devices": { "root": { "type": "disk", "pool": "default", "path": "/" } } }),
    );
    let templates = state.join("templates");
    private_dir(&templates);
    fs::write(
        templates.join("nats.json"),
        json!({
            "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "ports": [4222], "env": {}, "publishable": false
        })
        .to_string(),
    )
    .expect("the template");
    let bin = scratch.at("bin");
    private_dir(&bin);
    let code_file = scratch.at("setup-code");
    let notify = bin.join("notify-send");
    fs::write(
        &notify,
        format!(
            "#!/bin/sh\numask 077\nprintf '%s' \"$3\" > '{0}.tmp' && mv '{0}.tmp' '{0}'\n",
            code_file.display()
        ),
    )
    .expect("the stub desktop");
    fs::set_permissions(&notify, fs::Permissions::from_mode(0o700)).expect("chmod");

    // The page origin is https://localhost:<port>; the port is bound before the config names it.
    let (cert, key, spki) = certificate(&scratch.at("tls"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let origin = format!(
        "https://localhost:{}",
        listener.local_addr().expect("addr").port()
    );
    let socket = scratch.at("arca.sock");
    tls_front(listener, cert, key, socket.clone());
    let config = scratch.at("node.json");
    fs::write(
        &config,
        json!({
            "stateDir": state.display().to_string(),
            "incus": { "socket": fake.socket().display().to_string(), "pools": [] },
            "listen": { "unix": socket.display().to_string(), "tailnet": null },
            "tokens": { "hashDir": hash_dir.display().to_string() },
            "approvalOrigin": origin,
            "approvalHost": "node1",
            "kernel": {
                "projects": [arca_netproject::PROJECT], "scratch": false, "droiddProjects": ["droid"],
                "reservedCidrs": [], "serviceCidr": "192.0.2.0/24",
                // A database of the old kind, whose delete waits as a plan: its image is the config's to name.
                "postgresImage": format!("example/db@sha256:{}", "b".repeat(64))
            }
        })
        .to_string(),
    )
    .expect("the config");
    let mut serve = Serve::start(&config, &bin, scratch.at("serve.log"));
    serve.wait_for(&socket);

    // What waits for the owner: a delete plan (postgres_delete) and a request to remove the service `nats`.
    result(
        &mcp(
            &socket,
            &doctor,
            1,
            "service_create",
            json!({ "name": "nats", "template": "nats" }),
        ),
        "service_create",
    );
    assert!(instance_exists(&fake, "nats"), "nats runs");
    result(
        &mcp(
            &socket,
            &doctor,
            2,
            "postgres_create",
            json!({ "name": "db1", "image": format!("example/db@sha256:{}", "b".repeat(64)) }),
        ),
        "postgres_create",
    );
    let deleted = result(
        &mcp(
            &socket,
            &doctor,
            3,
            "postgres_delete",
            json!({ "name": "db1" }),
        ),
        "postgres_delete",
    );
    assert!(
        deleted["plan"]["statement"].is_string(),
        "a delete plan waits: {deleted}"
    );
    let asked = result(
        &mcp(
            &socket,
            &doctor,
            4,
            "owner_approval_request",
            json!({ "kind": "arca", "action": "delete", "target": "nats", "args": { "name": "nats" } }),
        ),
        "owner_approval_request",
    );
    let request = asked["id"].as_str().expect("a request id").to_string();

    // The browser, with the owner's first device, opens the page.
    let mut chrome = Chrome::launch(
        &chrome_binary,
        &scratch.at("chrome"),
        &spki,
        &scratch.at("chrome.log"),
    );
    chrome.page("WebAuthn.enable", json!({ "enableUI": false }));
    chrome.page(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    );
    let first = chrome.authenticator("internal");
    chrome.page(
        "Page.navigate",
        json!({ "url": format!("{origin}/approve") }),
    );
    let deadline = Instant::now() + WAIT;
    while chrome.eval(
        "document.readyState === 'complete' && document.getElementById('trusted').textContent === '0' && !!document.querySelector('#plans article.request')",
    ) != json!(true)
    {
        assert!(Instant::now() < deadline, "the page loads with no passkey and one request");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        chrome.eval("location.origin"),
        json!(origin),
        "the page runs at the configured origin"
    );

    // 1. The first passkey: a setup code from the machine's screen, then the page's registration.
    chrome.click("#get-code");
    let deadline = Instant::now() + WAIT;
    while !code_file.exists() {
        assert!(
            Instant::now() < deadline,
            "the setup code reaches the screen"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let code = fs::read_to_string(&code_file).expect("the code");
    assert!(
        code.len() == 8 && code.bytes().all(|b| b.is_ascii_digit()),
        "an 8-digit code"
    );
    chrome.eval(&format!(
        "document.getElementById('setup-code').value = {code:?}; document.getElementById('passkey-label').value = 'Laptop'; true"
    ));
    chrome.click("#register");
    chrome.status_ok("Passkey trusted.", &serve);
    let kept = passkeys(&state);
    assert_eq!(kept.len(), 1, "one passkey is trusted");
    assert_eq!(kept[0].rp_id, "localhost");
    assert_eq!(kept[0].origin, origin);
    let laptop = kept[0].fingerprint.clone();

    // 2. A second device (a phone), added on the passkey_add path: the new device alone answers the registration (the first holds an
    //    excluded credential), and the first alone confirms the change. The registration POST is held while the devices
    //    swap, so which one answers is decided, not raced.
    let second = chrome.authenticator("cable");
    chrome.presence(&first, false);
    chrome.page(
        "Fetch.enable",
        json!({ "patterns": [{ "urlPattern": "*/approve/register*", "requestStage": "Request" }] }),
    );
    chrome.eval("document.getElementById('passkey-label').value = 'Phone'; true");
    chrome.click("#register");
    let paused = chrome.event("Fetch.requestPaused");
    chrome.presence(&first, true);
    chrome.presence(&second, false);
    chrome.page(
        "Fetch.continueRequest",
        json!({ "requestId": paused["requestId"] }),
    );
    chrome.status_ok("Passkey trusted.", &serve);
    chrome.page("Fetch.disable", json!({}));
    let kept = passkeys(&state);
    assert_eq!(
        kept.len(),
        2,
        "the passkey_add change trusts the second passkey"
    );
    let phone = kept
        .iter()
        .map(|line| line.fingerprint.clone())
        .find(|fingerprint| fingerprint != &laptop)
        .expect("a second fingerprint");
    assert!(
        kept_count(&state, &laptop).is_some(),
        "the change was signed by the first passkey, which kept its count"
    );

    // 3. The delete plan, approved by the first device on its card.
    let nonces = state.join("approval-nonces.jsonl");
    let spent_before = lines(&nonces);
    let deletes = |fake: &FakeIncus| {
        fake.requests()
            .iter()
            .filter(|r| r.method == "DELETE")
            .count()
    };
    let deletes_before = deletes(&fake);
    let laptop_before = kept_count(&state, &laptop).expect("a count");
    chrome.click("#plans button.approve");
    let said = chrome.status_ok("Approved.", &serve);
    assert!(said.contains("deleted"), "{said}");
    assert!(
        deletes(&fake) > deletes_before,
        "the plan's delete reached Incus"
    );
    assert_eq!(
        lines(&nonces),
        spent_before + 1,
        "the plan's approval spent one nonce"
    );
    assert!(
        kept_count(&state, &laptop).expect("a count") > laptop_before,
        "the first passkey's count rose"
    );
    assert_eq!(
        chrome.eval("fetch('/approve/plans').then(r => r.json()).then(b => b.plans.length)"),
        json!(0),
        "no plan waits"
    );

    // 4. The owner approval request, approved by the second device with one tap.
    chrome.presence(&first, false);
    chrome.presence(&second, true);
    chrome.click("#plans article.request button.approve");
    chrome.status_ok(APPROVED_REQUEST, &serve);
    let audit =
        fs::read_to_string(state.join(arca::approval_requests::AUDIT_FILE)).expect("the audit");
    let approved = audit
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|row| row["outcome"].as_str().map(str::to_string))
        .find(|outcome| outcome.starts_with(&format!("approved:{request}:")))
        .expect("the approval is audited");
    assert!(
        approved.starts_with(&format!("approved:{request}:arca:delete:nats:params=")),
        "{approved}"
    );
    assert!(approved.contains(":requester=doctor-1:"), "{approved}");
    let via = format!(":key=passkey:{phone}:via=passkey:count=");
    let count: u64 = approved
        .split_once(&via)
        .and_then(|(_, rest)| rest.split(':').next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| {
            panic!("the outcome names the second passkey, via passkey, and a count: {approved}")
        });
    assert!(count > 0, "a device-bound passkey counts: {approved}");
    assert!(
        approved.ends_with(":synced=0"),
        "a device-bound passkey is not synced: {approved}"
    );

    // 5. The requester reads the approval and spends it through the real service_remove, once.
    let status = result(
        &mcp(
            &socket,
            &doctor,
            5,
            "owner_approval_status",
            json!({ "id": request }),
        ),
        "owner_approval_status",
    );
    assert_eq!(status["state"], "approved", "{status}");
    let approval = status["approval"]
        .as_str()
        .expect("the requester gets the approval")
        .to_string();
    let spent_before = lines(&nonces);
    result(
        &mcp(
            &socket,
            &doctor,
            6,
            "service_remove",
            json!({ "name": "nats", "approval": approval }),
        ),
        "service_remove with the page's approval",
    );
    assert!(!instance_exists(&fake, "nats"), "nats is removed");
    assert_eq!(lines(&nonces), spent_before + 1, "one nonce spent");
    assert_eq!(
        kept_count(&state, &phone),
        Some(count),
        "the spend kept the count the audit names"
    );
    // The replay: `nats` is made again under the same name, so the call reaches the approval check (a missing service would
    // answer not_found before it). The approval names `delete nats {name: nats}`, which this call matches exactly.
    result(
        &mcp(
            &socket,
            &doctor,
            7,
            "service_create",
            json!({ "name": "nats", "template": "nats" }),
        ),
        "service_create again",
    );
    assert!(instance_exists(&fake, "nats"), "nats runs again");
    let replay = mcp(
        &socket,
        &doctor,
        8,
        "service_remove",
        json!({ "name": "nats", "approval": approval }),
    );
    assert_eq!(
        replay["error"]["data"]["refusal"], "owner_approval",
        "the replay is refused as an owner approval, not as a missing service: {replay}"
    );
    // A device-bound passkey counts, and the count is checked before the nonce: its replay is refused by the sign count.
    assert!(
        replay["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("sign count is not above the last one kept")),
        "a counting passkey's replay is refused by its sign count: {replay}"
    );
    assert_eq!(
        lines(&nonces),
        spent_before + 1,
        "the replay spends no nonce"
    );
    assert_eq!(
        kept_count(&state, &phone),
        Some(count),
        "the replay leaves the kept count as it was"
    );
    assert!(instance_exists(&fake, "nats"), "the replay removes nothing");

    // No approval string, code or credential reaches the server's log.
    let log = serve.log_text();
    assert!(!log.contains(&approval), "the approval is not logged");
    assert!(!log.contains(&code), "the setup code is not logged");
    drop(chrome);
    assert_eq!(serve.terminate(), Some(0), "the server stops cleanly");
    println!(
        "browser passkey ceremony: 2 passkeys registered (setup code, passkey_add), 1 delete plan and 1 request approved, \
         request spend audited with via=passkey count={count} synced=0, service_remove spent it once, \
         the replay on a re-created nats refused by the sign count with no nonce spent"
    );
}

/// The page of a node on its own machine: `http://localhost:<port>` on the loopback listener, with no certificate and no
/// TLS front. A real Chromium saves the first passkey there with the setup code `arca setup-code` prints on a machine
/// with no screen, approves a change an assistant proposed, and is refused both at the same page under the IP address.
#[test]
#[ignore = "drives a real Chromium; run with: cargo test -p arca --test browser_passkey -- --ignored"]
fn g9_a_real_browser_at_http_localhost_saves_the_first_passkey_with_the_terminals_code_and_approves_a_change()
 {
    first_passkey_and_first_approved_change("browser-local", false, false);
}

/// The same walk with a device that makes only RSA keys (RS256, COSE -257), as some Windows Hello and TPM authenticators
/// do: the page offers ES256 and RS256, the device takes RS256, and the page's own script saves that passkey and approves
/// the change with it.
#[test]
#[ignore = "drives a real Chromium; run with: cargo test -p arca --test browser_passkey -- --ignored"]
fn a_real_browser_with_an_rs256_only_device_saves_its_passkey_and_approves_a_change() {
    first_passkey_and_first_approved_change("browser-rs256", false, true);
}

/// The whole way from the installer's node to the first approved change: the config is the installer's, which only
/// looks, and `arca init --enable-changes` is what lets it change things and names its approval page. Then the same
/// walk as above: the terminal's code, the first passkey, one proposal approved.
#[test]
#[ignore = "drives a real Chromium; run with: cargo test -p arca --test browser_passkey -- --ignored"]
fn g10_from_the_installers_node_arca_init_then_the_first_passkey_and_the_first_approved_change() {
    first_passkey_and_first_approved_change("browser-init", true, false);
}

/// The 2026-11-15 sitting, in a real browser: two cards approved one after the other in one page load both reach the server
/// and both run, each tap reaches `navigator.credentials.get` before the click handler returns (nothing is awaited first,
/// so Safari keeps the tap's gesture), and a card proposed after the page loaded shows up within one refresh, without a
/// reload.
#[test]
#[ignore = "drives a real Chromium; run with: cargo test -p arca --test browser_passkey -- --ignored"]
fn two_approvals_in_one_page_load_both_reach_the_server_and_a_later_card_shows_up_by_itself() {
    let chrome_binary = std::env::var(CHROME_ENV).unwrap_or_else(|_| DEFAULT_CHROME.to_string());
    let scratch = Scratch::new("browser-two");
    let state = scratch.at("state");
    private_dir(&state);
    let hash_dir = scratch.at("tokens");
    private_dir(&hash_dir);
    let secret_file = scratch.at("viewer.secret");
    run(
        arca()
            .args([
                "token",
                "mint",
                "--scope",
                "viewer",
                "--id",
                "assistant-1",
                "--hash-dir",
            ])
            .arg(&hash_dir)
            .arg("--out")
            .arg(&secret_file),
        "token mint",
    );
    let viewer = fs::read_to_string(&secret_file)
        .expect("the secret")
        .trim_end()
        .to_string();
    let fake = FakeIncus::start(
        &scratch.0,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("the fake Incus");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
                "devices": { "root": { "type": "disk", "pool": "default", "path": "/" } } }),
    );
    let templates = state.join("templates");
    private_dir(&templates);
    fs::write(
        templates.join("nats.json"),
        json!({
            "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "ports": [4222], "env": {}, "publishable": false
        })
        .to_string(),
    )
    .expect("the template");
    let bin = scratch.at("bin");
    private_dir(&bin);
    let notify = bin.join("notify-send");
    fs::write(&notify, "#!/bin/sh\nexit 1\n").expect("the stub desktop");
    fs::set_permissions(&notify, fs::Permissions::from_mode(0o700)).expect("chmod");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let origin = format!("http://localhost:{port}");
    let socket = scratch.at("arca.sock");
    let config = scratch.at("node.json");
    fs::write(
        &config,
        json!({
            "stateDir": state.display().to_string(),
            "incus": { "socket": fake.socket().display().to_string(), "pools": [] },
            "listen": { "unix": socket.display().to_string(), "loopback": format!("127.0.0.1:{port}") },
            "tokens": { "hashDir": hash_dir.display().to_string() },
            "approvalOrigin": origin,
            "approvalHost": "node1",
            "kernel": {
                "projects": [arca_netproject::PROJECT], "scratch": false, "droiddProjects": ["droid"],
                "reservedCidrs": [], "serviceCidr": "192.0.2.0/24"
            }
        })
        .to_string(),
    )
    .expect("the config");
    let mut serve = Serve::start(&config, &bin, scratch.at("serve.log"));
    serve.wait_for(&socket);
    let propose = |id: u64, name: &str| {
        result(
            &mcp(
                &socket,
                &viewer,
                id,
                "action_propose",
                json!({ "action": "service_create", "args": { "name": name, "template": "nats" } }),
            ),
            "action_propose",
        );
    };
    propose(1, "nats");
    propose(2, "natsb");

    let mut chrome = Chrome::launch(
        &chrome_binary,
        &scratch.at("chrome"),
        "AAAA",
        &scratch.at("chrome.log"),
    );
    chrome.page("WebAuthn.enable", json!({ "enableUI": false }));
    chrome.page(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    );
    chrome.authenticator("internal");
    chrome.page(
        "Page.navigate",
        json!({ "url": format!("{origin}/approve") }),
    );
    let wait_for = |chrome: &mut Chrome, js: &str, what: &str, within: Duration| {
        let deadline = Instant::now() + within;
        while chrome.eval(js) != json!(true) {
            assert!(
                Instant::now() < deadline,
                "{what}; server log:\n{}",
                serve.log_text()
            );
            thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for(
        &mut chrome,
        "document.readyState === 'complete' && document.querySelectorAll('#plans article.action').length === 2",
        "the page loads with both cards",
        WAIT,
    );

    // The first passkey, with the terminal's setup code.
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&config)
        .output()
        .expect("run arca setup-code");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    let code = printed["code"].as_str().expect("a code").to_string();
    chrome.eval(&format!(
        "document.getElementById('setup-code').value = {code:?}; document.getElementById('passkey-label').value = 'This computer'; true"
    ));
    chrome.click("#register");
    chrome.status_ok("Passkey trusted.", &serve);
    wait_for(
        &mut chrome,
        "[...document.querySelectorAll('#plans button.approve')].every((b) => b.textContent === 'Approve')",
        "with a passkey trusted, the cards' buttons approve",
        WAIT,
    );

    // From here the page is never loaded again: this mark would be gone. Every passkey request records whether it came
    // inside the click, before the handler returned.
    chrome.eval(
        "(() => { window.__sameLoad = true; window.__inTap = false; window.__getsInTap = []; \
         const get = navigator.credentials.get.bind(navigator.credentials); \
         navigator.credentials.get = (options) => { window.__getsInTap.push(window.__inTap); return get(options); }; \
         return true; })()",
    );
    let tap = |chrome: &mut Chrome, name: &str| {
        let deadline = Instant::now() + WAIT;
        while chrome.eval(&format!(
            "(() => {{ const card = [...document.querySelectorAll('#plans article.action')] \
               .find((c) => c.querySelector('h3').textContent.includes('service {name} from')); \
             const button = card && card.querySelector('button.approve'); \
             if (!button || button.disabled || button.closest('[inert]')) return false; \
             window.__inTap = true; button.click(); window.__inTap = false; return true; }})()"
        )) != json!(true)
        {
            assert!(
                Instant::now() < deadline,
                "the card for {name} is on the page"
            );
            thread::sleep(Duration::from_millis(100));
        }
    };

    tap(&mut chrome, "nats");
    chrome.status_ok("Approved. Done: create the service nats ", &serve);
    tap(&mut chrome, "natsb");
    chrome.status_ok("Approved. Done: create the service natsb ", &serve);
    assert!(instance_exists(&fake, "nats"), "the first approval ran");
    assert!(
        instance_exists(&fake, "natsb"),
        "the second approval in the same load ran"
    );
    assert_eq!(
        chrome.eval("[window.__sameLoad === true, JSON.stringify(window.__getsInTap)]"),
        json!([true, "[true,true]"]),
        "one page load, and each tap reached the passkey before its click handler returned"
    );

    // A card proposed after the page loaded shows up by itself within one refresh (60 s), with no reload. The list read
    // "Nothing to approve" before it, and the card moved what is below the list: taps are held.
    wait_for(
        &mut chrome,
        "!!document.querySelector('#plans .notice') && !document.getElementById('plans').inert",
        "the list says nothing waits, and taps are let through",
        WAIT,
    );
    chrome.eval(
        "(() => { window.__held = []; for (const id of ['plans', 'passkeys']) { const list = document.getElementById(id); \
         new MutationObserver(() => { if (list.inert && !window.__held.includes(id)) window.__held.push(id); }) \
           .observe(list, { attributes: true, attributeFilter: ['inert'] }); } return true; })()",
    );
    propose(3, "natsc");
    wait_for(
        &mut chrome,
        "window.__sameLoad === true && [...document.querySelectorAll('#plans article.action h3')].some((h) => h.textContent.includes('service natsc from'))",
        "the later card shows up within one refresh",
        Duration::from_secs(75),
    );
    assert_eq!(
        chrome.eval("[...window.__held].sort()"),
        json!(["passkeys", "plans"]),
        "the first card after Nothing to approve held taps"
    );

    drop(chrome);
    assert_eq!(serve.terminate(), Some(0), "serve exits 0 on SIGTERM");
}

/// Review 1 of PR 205, in a real browser with two real kernels: node1's page, and node2 linked to it. node2 trusts node1's
/// passkeys for node1's origin, and node1 holds a relay token for node2.
/// - While a tap runs, the page's refresh reads nothing.
/// - A refresh whose list puts a new card above a drawn one keeps the drawn one where it was. The new card goes after it,
///   taps are held off for a moment, and a real tap at the old place approves the card that was read there.
/// - A card refused `Expired` says so on the card, is renewed in place, and the next tap approves it.
/// - Decline on a linked card declines it on its own machine.
/// - A card that stops waiting while the page is open leaves a placeholder of its height, and nothing below it moves.
/// - Review 2: a request an assistant made stands in the same list. A plan proposed after it goes after it, so a real tap
///   at the request's old place after the hold approves the request, never the plan. The keyboard focus comes back after
///   the hold, and the passkeys list below is held with the cards.
/// - Review 3: a card refused `Expired` while the card above it stopped waiting. The reading after the refusal leaves a
///   placeholder where the card above stood, so the card the page asks to tap again stays where it is, and a real tap at
///   the same spot never approves the card below.
#[test]
#[ignore = "drives a real Chromium; run with: cargo test -p arca --test browser_passkey -- --ignored"]
fn a_refresh_never_moves_a_card_under_the_tap_an_expired_card_asks_once_more_and_a_linked_decline_reaches_its_host()
 {
    let chrome_binary = std::env::var(CHROME_ENV).unwrap_or_else(|_| DEFAULT_CHROME.to_string());
    let scratch = Scratch::new("browser-keep");
    let bin = scratch.at("bin");
    private_dir(&bin);
    let notify = bin.join("notify-send");
    fs::write(&notify, "#!/bin/sh\nexit 1\n").expect("the stub desktop");
    fs::set_permissions(&notify, fs::Permissions::from_mode(0o700)).expect("chmod");
    let kernel_config = json!({
        "projects": [arca_netproject::PROJECT], "scratch": false, "droiddProjects": ["droid"],
        "reservedCidrs": [], "serviceCidr": "192.0.2.0/24"
    });
    // One machine: its state, its tokens and an assistant's viewer token, a fake Incus and the nats template.
    let machine = |name: &str| {
        let state = scratch.at(&format!("{name}-state"));
        private_dir(&state);
        let hash_dir = scratch.at(&format!("{name}-tokens"));
        private_dir(&hash_dir);
        let secret_file = scratch.at(&format!("{name}-viewer.secret"));
        run(
            arca()
                .args([
                    "token",
                    "mint",
                    "--scope",
                    "viewer",
                    "--id",
                    "assistant-1",
                    "--hash-dir",
                ])
                .arg(&hash_dir)
                .arg("--out")
                .arg(&secret_file),
            "token mint",
        );
        let viewer = fs::read_to_string(&secret_file)
            .expect("the secret")
            .trim_end()
            .to_string();
        let incus_dir = scratch.at(&format!("{name}-incus"));
        private_dir(&incus_dir);
        let fake = FakeIncus::start(
            &incus_dir,
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![],
            },
        )
        .expect("the fake Incus");
        fake.add_profile(
            "default",
            json!({ "name": "default", "config": {},
                    "devices": { "root": { "type": "disk", "pool": "default", "path": "/" } } }),
        );
        let templates = state.join("templates");
        private_dir(&templates);
        fs::write(
            templates.join("nats.json"),
            json!({
                "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
                "digest": format!("sha256:{}", "a".repeat(64)),
                "ports": [4222], "env": {}, "publishable": false
            })
            .to_string(),
        )
        .expect("the template");
        (state, hash_dir, viewer, fake)
    };
    let (state1, tokens1, viewer1, fake1) = machine("node1");
    // The assistant's doctor token on node1, for an owner approval request.
    let doctor_file = scratch.at("node1-doctor.secret");
    run(
        arca()
            .args([
                "token",
                "mint",
                "--scope",
                "doctor",
                "--id",
                "doctor-1",
                "--hash-dir",
            ])
            .arg(&tokens1)
            .arg("--out")
            .arg(&doctor_file),
        "token mint",
    );
    let doctor1 = fs::read_to_string(&doctor_file)
        .expect("the doctor secret")
        .trim_end()
        .to_string();
    let (state2, tokens2, viewer2, fake2) = machine("node2");
    let relay_file = scratch.at("node2-relay.secret");
    run(
        arca()
            .args([
                "token",
                "mint",
                "--scope",
                "doctor",
                "--id",
                "relay",
                "--tools",
                "plan_list,action_approve,plan_decline,approval_submit",
                "--hash-dir",
            ])
            .arg(&tokens2)
            .arg("--out")
            .arg(&relay_file),
        "token mint",
    );
    let relay = fs::read_to_string(&relay_file)
        .expect("the relay secret")
        .trim_end()
        .to_string();

    // node1's page is at https://localhost:<port>, the origin node2 trusts node1's passkeys for.
    let (cert, key, spki) = certificate(&scratch.at("tls"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let origin = format!(
        "https://localhost:{}",
        listener.local_addr().expect("addr").port()
    );
    let socket1 = scratch.at("node1.sock");
    let socket2 = scratch.at("node2.sock");
    tls_front(listener, cert, key, socket1.clone());
    let config2 = scratch.at("node2.json");
    fs::write(
        &config2,
        json!({
            "stateDir": state2.display().to_string(),
            "incus": { "socket": fake2.socket().display().to_string(), "pools": [] },
            "listen": { "unix": socket2.display().to_string(), "tailnet": null },
            "tokens": { "hashDir": tokens2.display().to_string() },
            "approvalHost": "node2",
            "kernel": kernel_config,
            "approval": {
                "trustOwnerKeysFrom": [state1.join(arca_approval::STATE_PASSKEYS_FILE).display().to_string()],
                "origins": [origin]
            }
        })
        .to_string(),
    )
    .expect("node2's config");
    let config1 = scratch.at("node1.json");
    fs::write(
        &config1,
        json!({
            "stateDir": state1.display().to_string(),
            "incus": { "socket": fake1.socket().display().to_string(), "pools": [] },
            "listen": { "unix": socket1.display().to_string(), "tailnet": null },
            "tokens": { "hashDir": tokens1.display().to_string() },
            "approvalOrigin": origin,
            "approvalHost": "node1",
            "kernel": kernel_config,
            "approval": {
                "linkedHosts": [{
                    "name": "node2",
                    "socket": socket2.display().to_string(),
                    "tokenFile": relay_file.display().to_string()
                }]
            }
        })
        .to_string(),
    )
    .expect("node1's config");
    let mut serve2 = Serve::start(&config2, &bin, scratch.at("node2.log"));
    serve2.wait_for(&socket2);
    let mut serve = Serve::start(&config1, &bin, scratch.at("node1.log"));
    serve.wait_for(&socket1);
    let propose = |socket: &Path, viewer: &str, id: u64, name: &str| -> String {
        let proposed = result(
            &mcp(
                socket,
                viewer,
                id,
                "action_propose",
                json!({ "action": "service_create", "args": { "name": name, "template": "nats" } }),
            ),
            "action_propose",
        );
        proposed["id"].as_str().expect("an id").to_string()
    };
    propose(&socket1, &viewer1, 1, "nats");
    propose(&socket2, &viewer2, 1, "natsb");
    let natsc = propose(&socket2, &viewer2, 2, "natsc");

    let mut chrome = Chrome::launch(
        &chrome_binary,
        &scratch.at("chrome"),
        &spki,
        &scratch.at("chrome.log"),
    );
    chrome.page("WebAuthn.enable", json!({ "enableUI": false }));
    chrome.page(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    );
    chrome.authenticator("internal");
    chrome.page(
        "Page.navigate",
        json!({ "url": format!("{origin}/approve") }),
    );
    let wait_for = |chrome: &mut Chrome, js: &str, what: &str| {
        let deadline = Instant::now() + WAIT;
        while chrome.eval(js) != json!(true) {
            assert!(
                Instant::now() < deadline,
                "{what}; node1's log:\n{}\nnode2's log:\n{}",
                serve.log_text(),
                serve2.log_text()
            );
            thread::sleep(Duration::from_millis(100));
        }
    };
    // The card for the service `name`, in page script.
    let card = |name: &str| {
        format!(
            "[...document.querySelectorAll('#plans article.action')].find((c) => c.querySelector('h3').textContent.includes('service {name} from'))"
        )
    };
    wait_for(
        &mut chrome,
        "document.readyState === 'complete' && document.querySelectorAll('#plans article.action').length === 3",
        "the page loads with node1's card and node2's two",
    );

    // The first passkey, with the terminal's setup code.
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&config1)
        .output()
        .expect("run arca setup-code");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    let code = printed["code"].as_str().expect("a code").to_string();
    chrome.eval(&format!(
        "document.getElementById('setup-code').value = {code:?}; document.getElementById('passkey-label').value = 'This computer'; true"
    ));
    chrome.click("#register");
    chrome.status_ok("Passkey trusted.", &serve);
    wait_for(
        &mut chrome,
        "[...document.querySelectorAll('#plans button.approve')].every((b) => b.textContent === 'Approve') && !document.getElementById('plans').inert",
        "with a passkey trusted, the cards' buttons approve, and taps are let through",
    );

    // From here the page is never loaded again. Every list the page reads, and every passkey request, is counted; a
    // passkey request can be held until the test lets it go.
    chrome.eval(
        "(() => { window.__sameLoad = true; window.__lists = 0; window.__gets = 0; window.__getHook = null; \
         const fetched = window.fetch.bind(window); \
         window.fetch = (url, options) => { if (String(url) === '/approve/plans') window.__lists++; return fetched(url, options); }; \
         const get = navigator.credentials.get.bind(navigator.credentials); \
         navigator.credentials.get = (options) => { window.__gets++; return window.__getHook ? window.__getHook(options) : get(options); }; \
         return true; })()",
    );
    let click = |chrome: &mut Chrome, name: &str, button: &str| {
        let deadline = Instant::now() + WAIT;
        while chrome.eval(&format!(
            "(() => {{ const card = {}; const b = card && card.querySelector({button:?}); \
             if (!b || b.disabled || b.closest('[inert]')) return false; status(''); b.click(); return true; }})()",
            card(name)
        )) != json!(true)
        {
            assert!(Instant::now() < deadline, "{button} on the card for {name}");
            thread::sleep(Duration::from_millis(100));
        }
    };

    // 1. The page's refresh reads the list while nothing is being approved, and reads nothing while a tap runs.
    assert_eq!(chrome.eval("document.visibilityState"), json!("visible"));
    chrome.eval("window.__lists = 0; refreshPlans(); true");
    wait_for(
        &mut chrome,
        "window.__lists === 1",
        "the refresh reads the list",
    );
    chrome.eval(
        "window.__getHook = () => new Promise((resolve, reject) => { window.__cancel = () => reject(new DOMException('cancelled', 'NotAllowedError')); }); true",
    );
    click(&mut chrome, "nats", "button.approve");
    wait_for(
        &mut chrome,
        "typeof window.__cancel === 'function'",
        "the tap asks for the passkey",
    );
    chrome.eval(
        "window.__lists = 0; refreshPlans(); document.dispatchEvent(new Event('visibilitychange')); true",
    );
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        chrome.eval("window.__lists"),
        json!(0),
        "no list is read while a tap runs"
    );
    chrome.eval("window.__getHook = null; window.__cancel(); window.__cancel = undefined; true");
    wait_for(
        &mut chrome,
        &format!("!{}.querySelector('button.approve').disabled", card("nats")),
        "the cancelled tap gives the card its button back",
    );

    // 2. A card proposed on node1 lists above node2's: a refresh keeps node2's card where it was and puts the new card
    //    after it, holds taps off for a moment, and a real tap at the old place approves the card that was read there.
    let place = |chrome: &mut Chrome, name: &str| -> (f64, f64) {
        let at = chrome.eval(&format!(
            "(() => {{ const r = {}.querySelector('button.approve').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
            card(name)
        ));
        (at[0].as_f64().expect("x"), at[1].as_f64().expect("y"))
    };
    chrome.eval(&format!(
        "status(''); {}.scrollIntoView({{ block: 'center' }}); true",
        card("natsb")
    ));
    let before = place(&mut chrome, "natsb");
    chrome.eval(&format!("{}.dataset.read = 'yes'; true", card("natsb")));
    propose(&socket1, &viewer1, 2, "natsd");
    assert_eq!(
        chrome.eval(
            "fetch('/approve/plans').then((r) => r.json()).then((b) => { \
               const at = (name) => b.plans.findIndex((p) => String(p.summary).includes('service ' + name + ' from')); \
               return at('natsd') < at('natsb'); })"
        ),
        json!(true),
        "the new card lists above the card being read"
    );
    chrome.eval(
        "(() => { window.__held = false; const plans = document.getElementById('plans'); \
         new MutationObserver(() => { if (plans.inert) window.__held = true; }).observe(plans, { attributes: true, attributeFilter: ['inert'] }); \
         refreshPlans(); return true; })()",
    );
    wait_for(
        &mut chrome,
        &format!("!!{}", card("natsd")),
        "the refresh draws the new card",
    );
    assert_eq!(
        chrome.eval(&format!(
            "[window.__held, {}.dataset.read, [...document.querySelectorAll('#plans article')].at(-1) === {}]",
            card("natsb"),
            card("natsd")
        )),
        json!([true, "yes", true]),
        "taps were held off, the card being read is the same card, and the new card went after the others"
    );
    let after = place(&mut chrome, "natsb");
    assert!(
        (after.0 - before.0).abs() < 0.5 && (after.1 - before.1).abs() < 0.5,
        "the card being read did not move: {before:?} then {after:?}"
    );
    wait_for(
        &mut chrome,
        "!document.getElementById('plans').inert",
        "taps are let through again",
    );
    let gets = chrome.eval("window.__gets");
    chrome.eval("status(''); true");
    chrome.tap_at(before.0, before.1);
    chrome.status_ok("Approved. Done: create the service natsb ", &serve);
    assert_eq!(
        chrome.eval("window.__gets"),
        json!(gets.as_u64().expect("a count") + 1),
        "one tap, one passkey request"
    );
    assert!(
        instance_exists(&fake2, "natsb"),
        "node2 ran the card that was read"
    );
    assert!(
        !instance_exists(&fake1, "natsd"),
        "the new card was not approved"
    );

    // 3. node1's card, signed over a statement past its ten minutes, is refused Expired; the page says so on the card,
    //    renews it in place, and the next tap approves it.
    chrome.eval(&format!(
        "(() => {{ const listed = window.fetch; window.__age = true; \
         window.fetch = async (url, options) => {{ \
           const answer = await listed(url, options); \
           if (String(url) !== '/approve/plans' || !window.__age) return answer; \
           window.__age = false; \
           const body = await answer.json(); \
           for (const plan of body.plans) if (String(plan.summary).includes('service nats from')) {{ \
             const statement = JSON.parse(plan.statement); \
             const older = (stamp) => new Date(Date.parse(stamp) - 11 * 60000).toISOString(); \
             statement.issuedAt = older(statement.issuedAt); statement.expiresAt = older(statement.expiresAt); \
             plan.statement = JSON.stringify(statement); \
           }} \
           return new Response(JSON.stringify(body), {{ status: 200, headers: {{ 'Content-Type': 'application/json' }} }}); \
         }}; \
         {}.dataset.read = 'yes'; return loadPlans().then(() => true); }})()",
        card("nats")
    ));
    click(&mut chrome, "nats", "button.approve");
    wait_for(
        &mut chrome,
        &format!(
            "(() => {{ const c = {}; const note = c && c.querySelector('.card-note'); return !!note && c.dataset.read === 'yes' && note.textContent.includes('Tap Approve again') && !c.querySelector('button.approve').disabled; }})()",
            card("nats")
        ),
        "the Expired card says so on itself, in its own place",
    );
    assert!(
        !instance_exists(&fake1, "nats"),
        "nothing ran on the stale statement"
    );
    click(&mut chrome, "nats", "button.approve");
    chrome.status_ok("Approved. Done: create the service nats ", &serve);
    assert!(instance_exists(&fake1, "nats"), "the renewed card ran");

    // 4. Decline on node2's card declines it on node2.
    click(&mut chrome, "natsc", "button.btn-secondary");
    chrome.status_ok("Declined. Nothing changed.", &serve);
    let listed = result(
        &mcp(&socket2, &relay, 3, "plan_list", json!({})),
        "plan_list",
    );
    assert!(
        !listed.to_string().contains(&natsc),
        "node2 no longer lists the declined card: {listed}"
    );
    assert!(!instance_exists(&fake2, "natsc"), "nothing ran");

    // 5. A card declined on node2 while the page is open leaves a placeholder of its height, and nothing below it moves.
    let natse = propose(&socket2, &viewer2, 4, "natse");
    chrome.eval("refreshPlans(); true");
    wait_for(
        &mut chrome,
        &format!("!!{}", card("natse")),
        "node2's new card shows up",
    );
    wait_for(
        &mut chrome,
        "!document.getElementById('plans').inert",
        "taps are let through again",
    );
    let measure = format!(
        "(() => {{ const r = {}.getBoundingClientRect(); const d = document.querySelector('section.setup').getBoundingClientRect(); return [r.top, r.height, d.top]; }})()",
        card("natse")
    );
    let measured = chrome.eval(&measure);
    result(
        &mcp(
            &socket2,
            &relay,
            5,
            "plan_decline",
            json!({ "plan": natse }),
        ),
        "plan_decline",
    );
    chrome.eval("refreshPlans(); true");
    wait_for(
        &mut chrome,
        &format!(
            "!{} && !!document.querySelector('#plans .card.gone')",
            card("natse")
        ),
        "the declined card leaves a placeholder",
    );
    let gone = chrome.eval(
        "(() => { const g = document.querySelector('#plans .card.gone').getBoundingClientRect(); const d = document.querySelector('section.setup').getBoundingClientRect(); return [g.top, g.height, d.top]; })()",
    );
    let close = |a: &Value, b: &Value| {
        (a.as_f64().expect("a number") - b.as_f64().expect("a number")).abs() < 0.5
    };
    assert!(
        close(&gone[0], &measured[0])
            && close(&gone[1], &measured[1])
            && close(&gone[2], &measured[2]),
        "the placeholder stands where the card stood, at its height, and what comes after it stays: {measured} then {gone}"
    );

    // 6. A request an assistant made goes in the same list as the plans. A plan proposed after it, which the server lists
    //    above every request, goes after it: the request does not move, and a real tap at its old place after the hold
    //    approves the request, never the new plan. Its Approve had the keyboard focus, and gets it back after the hold;
    //    the passkeys list below is held with the cards.
    let asked = result(
        &mcp(
            &socket1,
            &doctor1,
            10,
            "owner_approval_request",
            json!({ "kind": "arca", "action": "delete", "target": "natsb", "args": { "name": "natsb" } }),
        ),
        "owner_approval_request",
    );
    let request = asked["id"].as_str().expect("a request id").to_string();
    chrome.eval("refreshPlans(); true");
    let request_button = "document.querySelector('#plans article.request button.approve')";
    wait_for(
        &mut chrome,
        &format!("!!{request_button}"),
        "the request shows up among the cards",
    );
    wait_for(
        &mut chrome,
        "!document.getElementById('plans').inert",
        "taps are let through again",
    );
    let spot_of = format!(
        "(() => {{ const r = {request_button}.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
    );
    chrome.eval(&format!(
        "{request_button}.scrollIntoView({{ block: 'center' }}); {request_button}.focus(); true"
    ));
    let spot = chrome.eval(&spot_of);
    propose(&socket1, &viewer1, 11, "natsf");
    assert_eq!(
        chrome.eval(
            "fetch('/approve/plans').then((r) => r.json()).then((b) => b.plans.some((p) => String(p.summary).includes('service natsf from')) && b.approvalRequests.length > 0)"
        ),
        json!(true),
        "the server lists the new plan with the plans, above every request"
    );
    chrome.eval(
        "(() => { window.__heldLists = []; for (const id of ['plans', 'passkeys']) { const list = document.getElementById(id); \
         new MutationObserver(() => { if (list.inert && !window.__heldLists.includes(id)) window.__heldLists.push(id); }) \
           .observe(list, { attributes: true, attributeFilter: ['inert'] }); } \
         refreshPlans(); return true; })()",
    );
    wait_for(
        &mut chrome,
        &format!("!!{}", card("natsf")),
        "the new plan shows up",
    );
    assert_eq!(
        chrome.eval(&format!(
            "[[...window.__heldLists].sort(), [...document.querySelectorAll('#plans article')].at(-1) === {}]",
            card("natsf")
        )),
        json!([["passkeys", "plans"], true]),
        "the cards and the passkeys list were held, and the new plan went after the request"
    );
    let moved_to = chrome.eval(&spot_of);
    assert!(
        close(&moved_to[0], &spot[0]) && close(&moved_to[1], &spot[1]),
        "the request did not move: {spot} then {moved_to}"
    );
    wait_for(
        &mut chrome,
        &format!(
            "!document.getElementById('plans').inert && document.activeElement === {request_button}"
        ),
        "after the hold, the request's Approve has the keyboard focus again",
    );
    chrome.eval("status(''); true");
    chrome.tap_at(spot[0].as_f64().expect("x"), spot[1].as_f64().expect("y"));
    chrome.status_ok(APPROVED_REQUEST, &serve);
    let audit =
        fs::read_to_string(state1.join(arca::approval_requests::AUDIT_FILE)).expect("the audit");
    assert!(
        audit.contains(&format!("approved:{request}:")),
        "the request was approved: {audit}"
    );
    assert!(
        !instance_exists(&fake1, "natsf"),
        "the new plan was not approved"
    );
    wait_for(
        &mut chrome,
        &format!("!!{}", card("natsf")),
        "the new plan still waits",
    );

    // 7. Three cards: natsh, natsi and natsj. natsi's next listing carries a statement past its ten minutes, and natsh is
    //    declined on node1 while it is still drawn. A real tap on natsi is refused Expired; the reading after it leaves
    //    a placeholder where natsh stood, so natsi stays where it was. The page has no scroll anchoring here, as in
    //    Safari. A real tap at the same spot approves natsi or nothing, never natsj.
    let natsh = propose(&socket1, &viewer1, 12, "natsh");
    propose(&socket1, &viewer1, 13, "natsi");
    propose(&socket1, &viewer1, 14, "natsj");
    // natsf's Approve has the keyboard focus, and the owner moves it to the passkey name field during the hold: it stays
    // there after the hold.
    wait_for(
        &mut chrome,
        "!document.getElementById('plans').inert",
        "taps are let through after the request's approval",
    );
    let focused = chrome.eval(&format!(
        "(() => {{ const b = {}.querySelector('button.approve'); b.focus(); const plans = document.getElementById('plans'); \
           const moved = new MutationObserver(() => {{ if (plans.inert) {{ document.getElementById('passkey-label').focus(); moved.disconnect(); }} }}); \
           moved.observe(plans, {{ attributes: true, attributeFilter: ['inert'] }}); return document.activeElement === b; }})()",
        card("natsf")
    ));
    assert_eq!(
        focused,
        json!(true),
        "natsf's Approve has the keyboard focus"
    );
    chrome.eval(
        "document.documentElement.style.overflowAnchor = 'none'; document.body.style.overflowAnchor = 'none'; refreshPlans(); true",
    );
    wait_for(
        &mut chrome,
        &format!(
            "!!{} && !document.getElementById('plans').inert",
            card("natsj")
        ),
        "the three cards show up, and taps are let through",
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        chrome.eval("document.activeElement === document.getElementById('passkey-label')"),
        json!(true),
        "the focus the owner moved during the hold stays where they put it"
    );
    chrome.eval("document.activeElement.blur(); true");
    chrome.eval(
        "(() => { const listed = window.fetch; window.__ageName = 'natsi'; \
         window.fetch = async (url, options) => { \
           const answer = await listed(url, options); \
           if (String(url) !== '/approve/plans' || !window.__ageName) return answer; \
           const name = window.__ageName; window.__ageName = null; \
           const body = await answer.json(); \
           for (const plan of body.plans) if (String(plan.summary).includes('service ' + name + ' from')) { \
             const statement = JSON.parse(plan.statement); \
             const older = (stamp) => new Date(Date.parse(stamp) - 11 * 60000).toISOString(); \
             statement.issuedAt = older(statement.issuedAt); statement.expiresAt = older(statement.expiresAt); \
             plan.statement = JSON.stringify(statement); \
           } \
           return new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } }); \
         }; \
         return loadPlans(true).then(() => true); })()",
    );
    assert_eq!(chrome.eval("window.__ageName === null"), json!(true));
    result(
        &mcp(
            &socket1,
            &doctor1,
            15,
            "plan_decline",
            json!({ "plan": natsh }),
        ),
        "plan_decline",
    );
    let approve_of = |name: &str| {
        format!(
            "(() => {{ const r = {}.querySelector('button.approve').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
            card(name)
        )
    };
    // The status line is cleared first: a change of its height moves every card.
    chrome.eval(&format!(
        "status(''); {}.scrollIntoView({{ block: 'center' }}); true",
        card("natsi")
    ));
    let height = chrome
        .eval(&format!("{}.getBoundingClientRect().height", card("natsh")))
        .as_f64()
        .expect("natsh's height");
    let first = chrome.eval(&approve_of("natsi"));
    let (x, y) = (first[0].as_f64().expect("x"), first[1].as_f64().expect("y"));
    chrome.tap_at(x, y);
    wait_for(
        &mut chrome,
        &format!(
            "(() => {{ const c = {}; const note = c && c.querySelector('.card-note'); \
               return !!note && note.textContent.includes('Tap Approve again') && !c.querySelector('button.approve').disabled \
                 && !!c.previousElementSibling && c.previousElementSibling.matches('.card.gone') \
                 && !document.getElementById('plans').inert; }})()",
            card("natsi")
        ),
        "the Expired card asks for one more tap, natsh's place is a placeholder, and taps are let through",
    );
    let again = chrome.eval(&approve_of("natsi"));
    let moved = (again[1].as_f64().expect("y") - y).abs();
    assert!(
        moved < height / 2.0,
        "natsi stayed where the owner tapped it ({moved} px against a card of {height} px)"
    );
    assert!(
        !instance_exists(&fake1, "natsi"),
        "nothing ran on the stale statement"
    );
    // The owner taps again where they tapped before, with the page as it is (its status line asks for the tap).
    let gets = chrome.eval("window.__gets");
    chrome.tap_at(x, y);
    thread::sleep(Duration::from_millis(1500));
    if chrome.eval("window.__gets") == gets {
        // The same spot hit no button (the status line grew): the owner taps the button where it now is.
        chrome.tap_at(again[0].as_f64().expect("x"), again[1].as_f64().expect("y"));
    }
    chrome.status_ok("Approved. Done: create the service natsi ", &serve);
    assert!(instance_exists(&fake1, "natsi"), "the renewed card ran");
    assert!(
        !instance_exists(&fake1, "natsj"),
        "the card below was never approved"
    );

    assert_eq!(
        chrome.eval("window.__sameLoad === true"),
        json!(true),
        "one page load"
    );
    let log = serve.log_text();
    for line in [
        "on node2: approved",
        "on node1: refused Expired",
        "on node1: approved",
        "on node2: declined",
    ] {
        assert!(log.contains(line), "node1's log has {line:?}:\n{log}");
    }
    drop(chrome);
    assert_eq!(serve.terminate(), Some(0), "node1 exits 0 on SIGTERM");
    assert_eq!(serve2.terminate(), Some(0), "node2 exits 0 on SIGTERM");
}

/// What a device that makes only RSA keys leaves of a page's `pubKeyCredParams`: RS256 (-257). It runs in the loaded page
/// before the passkey is saved, and records the algorithms the page offered in `window.__arcaOfferedAlgs`.
const RS256_ONLY_DEVICE: &str = r#"(() => {
  const create = navigator.credentials.create.bind(navigator.credentials);
  navigator.credentials.create = (options) => {
    const offered = options.publicKey.pubKeyCredParams;
    window.__arcaOfferedAlgs = offered.map((param) => param.alg);
    const publicKey = { ...options.publicKey, pubKeyCredParams: offered.filter((param) => param.alg === -257) };
    return create({ ...options, publicKey });
  };
  return true;
})();"#;

/// The walk the tests above make. With `by_init`, the node's config starts as the installer writes it and `arca init
/// --enable-changes --yes` makes it one that may change things; without, the config is written whole by the test. With
/// `rs256_only`, the device makes only RSA keys: a script run in the loaded page keeps only RS256 (-257) of the
/// algorithms the page offers when it saves the passkey, and records what the page offered.
fn first_passkey_and_first_approved_change(tag: &str, by_init: bool, rs256_only: bool) {
    let chrome_binary = std::env::var(CHROME_ENV).unwrap_or_else(|_| DEFAULT_CHROME.to_string());
    let scratch = Scratch::new(tag);
    let state = scratch.at("state");
    private_dir(&state);
    let hash_dir = scratch.at("tokens");
    private_dir(&hash_dir);

    // The assistant's token: a viewer, which may look and propose and nothing more.
    let secret_file = scratch.at("viewer.secret");
    run(
        arca()
            .args([
                "token",
                "mint",
                "--scope",
                "viewer",
                "--id",
                "assistant-1",
                "--hash-dir",
            ])
            .arg(&hash_dir)
            .arg("--out")
            .arg(&secret_file),
        "token mint",
    );
    let viewer = fs::read_to_string(&secret_file)
        .expect("the secret")
        .trim_end()
        .to_string();

    let fake = FakeIncus::start(
        &scratch.0,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("the fake Incus");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
                "devices": { "root": { "type": "disk", "pool": "default", "path": "/" } } }),
    );
    let templates = state.join("templates");
    private_dir(&templates);
    fs::write(
        templates.join("nats.json"),
        json!({
            "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "ports": [4222], "env": {}, "publishable": false
        })
        .to_string(),
    )
    .expect("the template");
    // A machine with no screen: its `notify-send` fails, so the page's own request for a code cannot be shown.
    let bin = scratch.at("bin");
    private_dir(&bin);
    let notify = bin.join("notify-send");
    fs::write(&notify, "#!/bin/sh\nexit 1\n").expect("the stub desktop");
    fs::set_permissions(&notify, fs::Permissions::from_mode(0o700)).expect("chmod");

    // The door's port is free when it is read; the node binds it a moment later.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let origin = format!("http://localhost:{port}");
    let socket = scratch.at("arca.sock");
    let config = scratch.at("node.json");
    if by_init {
        // The installer's config, which names no kernel project, no service network and no approval page.
        fs::write(
            &config,
            json!({
                "stateDir": state.display().to_string(),
                "incus": { "socket": fake.socket().display().to_string() },
                "listen": { "unix": socket.display().to_string(), "loopback": format!("127.0.0.1:{port}") },
                "tokens": { "hashDir": hash_dir.display().to_string() },
                "kernel": { "droiddProjects": [] }
            })
            .to_string(),
        )
        .expect("the installer's config");
        let out = arca()
            .args(["init", "--enable-changes", "--yes", "--config"])
            .arg(&config)
            .output()
            .expect("run arca init");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let said = String::from_utf8_lossy(&out.stdout).to_string();
        // Init's report is short: the page it wrote is named before the question, and the report ends in the one
        // pointer to the rest.
        assert!(
            said.contains(&format!("approvalOrigin: {origin}, the approval page"))
                && said.contains("A database: ask your assistant for the postgres-autopg service.")
                && said.contains("More: `arca status --config "),
            "{said}"
        );
        let written: Value =
            serde_json::from_str(&fs::read_to_string(&config).expect("the config")).expect("JSON");
        assert_eq!(written["approvalOrigin"], json!(origin));
        assert_eq!(
            written["kernel"]["projects"],
            json!([arca_netproject::PROJECT])
        );
    } else {
        fs::write(
            &config,
            json!({
                "stateDir": state.display().to_string(),
                "incus": { "socket": fake.socket().display().to_string(), "pools": [] },
                "listen": { "unix": socket.display().to_string(), "loopback": format!("127.0.0.1:{port}") },
                "tokens": { "hashDir": hash_dir.display().to_string() },
                "approvalOrigin": origin,
                "approvalHost": "node1",
                "kernel": {
                    "projects": [arca_netproject::PROJECT], "scratch": false, "droiddProjects": ["droid"],
                    "reservedCidrs": [], "serviceCidr": "192.0.2.0/24"
                }
            })
            .to_string(),
        )
        .expect("the config");
    }
    let mut serve = Serve::start(&config, &bin, scratch.at("serve.log"));
    serve.wait_for(&socket);

    // The assistant proposes a change. Nothing runs until the owner approves it.
    result(
        &mcp(
            &socket,
            &viewer,
            1,
            "action_propose",
            json!({ "action": "service_create", "args": { "name": "nats", "template": "nats" } }),
        ),
        "action_propose",
    );
    assert!(!instance_exists(&fake, "nats"), "a proposal runs nothing");

    // The browser, with a device that checks its user, opens the page by the name localhost, over plain http.
    let mut chrome = Chrome::launch(
        &chrome_binary,
        &scratch.at("chrome"),
        "AAAA",
        &scratch.at("chrome.log"),
    );
    chrome.page("WebAuthn.enable", json!({ "enableUI": false }));
    chrome.page(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    );
    chrome.authenticator("internal");
    chrome.page(
        "Page.navigate",
        json!({ "url": format!("{origin}/approve") }),
    );
    let deadline = Instant::now() + WAIT;
    while chrome.eval(
        "document.readyState === 'complete' && document.getElementById('trusted').textContent === '0' && !!document.querySelector('#plans button.approve')",
    ) != json!(true)
    {
        assert!(
            Instant::now() < deadline,
            "the page loads with no passkey and one waiting card; server log:\n{}",
            serve.log_text()
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(chrome.eval("location.origin"), json!(origin));
    assert_eq!(
        chrome.eval("window.isSecureContext"),
        json!(true),
        "the browser treats http://localhost as a secure context"
    );
    assert_eq!(
        chrome.eval("!document.getElementById('local-help').hidden && document.getElementById('local-help').textContent.includes('ssh -o ExitOnForwardFailure=yes -L localhost:' + location.port + ':127.0.0.1:' + location.port)"),
        json!(true),
        "the page says what this browser needs, and the other way in"
    );

    // 0. No passkey is trusted yet, so no approval can work: the setup comes first on the page, and the waiting card's
    //    button says what comes first and takes the owner to the setup instead of asking for a passkey there is none of.
    assert_eq!(
        chrome.eval(
            "document.querySelector('section.setup').getBoundingClientRect().top < document.getElementById('plans').getBoundingClientRect().top"
        ),
        json!(true),
        "the passkey setup is above the waiting card"
    );
    assert_eq!(
        chrome.eval("document.querySelector('#plans button.approve').textContent"),
        json!("Set up your passkey first"),
        "the card's button says the passkey comes first"
    );
    chrome.click("#plans button.approve");
    assert_eq!(
        chrome.eval(
            "[document.activeElement && document.activeElement.id, document.getElementById('status').dataset.tone || '']"
        ),
        json!(["get-code", ""]),
        "the button takes the owner to the setup's first step, and nothing failed"
    );
    assert!(!instance_exists(&fake, "nats"), "nothing ran");

    // 0b. Issue 117: the card speaks to the owner. It leads with one sentence of what the assistant wants, says what the
    //     service takes of the node and how to undo it, and keeps the verb name, the image digest, the template's
    //     content and the fingerprint in a Details disclosure that starts closed.
    let card = chrome.eval(
        "(() => { const card = document.querySelector('#plans article.action'); const more = card.querySelector('details'); \
         const shown = card.cloneNode(true); shown.querySelector('details').remove(); \
         return { lead: card.querySelector('h3').textContent, shown: shown.textContent, open: more.open, \
                  folded: more.textContent, lede: document.querySelector('.lede').textContent }; })()",
    );
    assert_eq!(
        card["lead"],
        json!("Your assistant wants to create the service nats from template nats."),
        "{card}"
    );
    let shown = card["shown"].as_str().expect("the card's text");
    for word in ["service_create", "sha256", "Fingerprint", &"a".repeat(64)] {
        assert!(
            !shown.contains(word),
            "the card shows {word} up front: {shown}"
        );
    }
    assert!(
        shown.contains("Uses")
            && shown.contains("Up to 2 CPUs and 2GiB of memory (Arca's default for a service)")
            && shown.contains("To undo")
            && shown.contains("Ask your assistant to remove it."),
        "{shown}"
    );
    assert_eq!(card["open"], json!(false), "Details starts closed");
    let folded = card["folded"].as_str().expect("the details' text");
    for word in [
        "service_create",
        "sha256:",
        "Template content",
        "Fingerprint",
    ] {
        assert!(folded.contains(word), "Details holds {word}: {folded}");
    }
    assert!(
        card["lede"]
            .as_str()
            .is_some_and(|lede| lede.starts_with("Arca asks before it changes anything.")),
        "{card}"
    );

    // 1. The machine has no screen: the page says so, and names the command for the terminal.
    chrome.click("#get-code");
    let deadline = Instant::now() + WAIT;
    loop {
        let said = chrome.eval("document.getElementById('status').textContent");
        if said
            .as_str()
            .is_some_and(|text| text.contains("no screen") && text.contains("arca setup-code"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the page names the terminal's command; it says {said}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&config)
        .output()
        .expect("run arca setup-code");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    let code = printed["code"].as_str().expect("a code").to_string();
    assert_eq!(printed["page"], json!(format!("{origin}/approve")));

    // 2. The first passkey, saved by the page's own script at http://localhost.
    if rs256_only {
        chrome.eval(RS256_ONLY_DEVICE);
    }
    chrome.eval(&format!(
        "document.getElementById('setup-code').value = {code:?}; document.getElementById('passkey-label').value = 'This computer'; true"
    ));
    chrome.click("#register");
    chrome.status_ok("Passkey trusted.", &serve);
    let kept = passkeys(&state);
    assert_eq!(kept.len(), 1, "one passkey is trusted");
    assert_eq!(kept[0].rp_id, "localhost");
    assert_eq!(kept[0].origin, origin);
    if rs256_only {
        assert_eq!(
            chrome.eval("JSON.stringify(window.__arcaOfferedAlgs)"),
            json!("[-7,-257]"),
            "the page offers ES256, then RS256"
        );
        assert_eq!(
            kept[0].alg(),
            arca_approval::PasskeyAlg::Rs256,
            "the device's RSA key is trusted as RS256"
        );
    } else {
        assert_eq!(kept[0].alg(), arca_approval::PasskeyAlg::Es256);
    }

    // The page draws the cards again once the passkey is trusted, and the button then approves.
    let deadline = Instant::now() + WAIT;
    loop {
        let said = chrome.eval("document.querySelector('#plans button.approve').textContent");
        if said == json!("Approve") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "with a passkey trusted, the card's button approves; it says {said}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // 3. The first approved change: the owner approves the assistant's proposal, and it runs once.
    chrome.click("#plans button.approve");
    let said = chrome.status_ok("Approved.", &serve);
    assert_eq!(
        said, "Approved. Done: create the service nats from template nats.",
        "the page says what was done in the card's words, not the verb's name"
    );
    assert!(instance_exists(&fake, "nats"), "the approved change ran");

    // 3b. An MCP client given only the URL: it registers, the owner approves it on the consent page with the same
    //     passkey, the browser is sent back to the client with a code, and the code's token calls the MCP door.
    connect_an_mcp_client(&mut chrome, &socket, &origin, &serve);

    // 4. The same page under the IP address is another origin: its POSTs are refused, and the browser itself refuses a
    //    passkey there, because an IP address is no relying party.
    chrome.page(
        "Page.navigate",
        json!({ "url": format!("http://127.0.0.1:{port}/approve") }),
    );
    let deadline = Instant::now() + WAIT;
    while chrome.eval("document.readyState === 'complete' && location.hostname === '127.0.0.1'")
        != json!(true)
    {
        assert!(
            Instant::now() < deadline,
            "the page loads at the IP address"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        chrome.eval(
            "fetch('/approve/setup-code/verify', { method: 'POST', body: '{}' }).then(r => r.status)"
        ),
        json!(403),
        "a POST from the IP origin is not the page's origin"
    );
    assert_eq!(
        chrome.eval(
            "navigator.credentials.get({ publicKey: { challenge: new Uint8Array(32), rpId: location.hostname, userVerification: 'required' } }).then(() => 'a passkey answered', e => e.name)"
        ),
        json!("SecurityError"),
        "the browser takes no IP address as a relying party"
    );

    drop(chrome);
    assert_eq!(serve.terminate(), Some(0), "serve exits 0 on SIGTERM");
}

/// One request on the node's unix socket: its status and body.
fn http(
    socket: &Path,
    path: &str,
    content_type: &str,
    body: &str,
    bearer: Option<&str>,
) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("a deadline");
    let auth = bearer
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: arca\r\nContent-Type: {content_type}\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    let status = reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status");
    let text = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    (status, text.to_string())
}

fn percent(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The MCP authorization flow with a real browser and a real authenticator: registration, the consent page, the
/// passkey, the redirect to the client's loopback port, the code exchange and one MCP call with the issued token.
fn connect_an_mcp_client(chrome: &mut Chrome, socket: &Path, origin: &str, serve: &Serve) {
    let callback = std::net::TcpListener::bind("127.0.0.1:0").expect("the client's callback port");
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        callback.local_addr().expect("addr").port()
    );
    let (status, body) = http(
        socket,
        "/oauth/register",
        "application/json",
        &json!({ "client_name": "Claude Code", "redirect_uris": [redirect] }).to_string(),
        None,
    );
    assert_eq!(status, 201, "the client registers: {body}");
    let client = serde_json::from_str::<Value>(&body).expect("JSON")["client_id"]
        .as_str()
        .expect("a client id")
        .to_string();
    let verifier = "a-browser-pkce-verifier-of-fifty-one-characters-012";
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let (sent, back) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = callback.accept() {
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..read])
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = sent.send(line);
        }
    });
    let url = format!(
        "{origin}/oauth/authorize?response_type=code&client_id={client}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state=browser-state&scope=viewer&resource={}",
        percent(&redirect),
        percent(&format!("{origin}/mcp"))
    );
    chrome.page("Page.navigate", json!({ "url": url }));
    let deadline = Instant::now() + WAIT;
    while chrome.eval("document.readyState === 'complete' && !!document.getElementById('approve')")
        != json!(true)
    {
        assert!(
            Instant::now() < deadline,
            "the consent page loads; server log:\n{}",
            serve.log_text()
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        chrome.eval(
            "[document.getElementById('client-name').textContent, document.getElementById('redirect-host').textContent, document.getElementById('scope').textContent, !!document.getElementById('name-shared')]"
        ),
        json!(["Claude Code", "127.0.0.1", "viewer", false]),
        "the consent page names the app, where its answer goes and what it asks"
    );
    chrome.click("#approve");
    let line = back.recv_timeout(WAIT).unwrap_or_else(|_| {
        panic!(
            "the browser is sent back to the app; the page says {}; server log:\n{}",
            chrome.eval(
                "document.getElementById('status') && document.getElementById('status').textContent"
            ),
            serve.log_text()
        )
    });
    let query = line
        .split_whitespace()
        .nth(1)
        .and_then(|target| target.split_once('?'))
        .map(|(_, query)| query.to_string())
        .expect("the callback has a query");
    let field = |key: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .map(str::to_string)
    };
    assert_eq!(field("state").as_deref(), Some("browser-state"));
    let code = field("code").expect("a code");
    let form = format!(
        "grant_type=authorization_code&code={code}&client_id={client}&redirect_uri={}&code_verifier={verifier}&resource={}",
        percent(&redirect),
        percent(&format!("{origin}/mcp"))
    );
    let (status, body) = http(
        socket,
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &form,
        None,
    );
    assert_eq!(status, 200, "the code becomes a token: {body}");
    let access = serde_json::from_str::<Value>(&body).expect("JSON")["access_token"]
        .as_str()
        .expect("an access token")
        .to_string();
    let (status, body) = http(
        socket,
        "/mcp",
        "application/json",
        &json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }).to_string(),
        Some(&access),
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"arca_status\""),
        "the issued viewer token lists the viewer's tools"
    );
}
