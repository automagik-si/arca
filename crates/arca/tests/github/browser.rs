//! A headless Chrome for the GitHub page's browser test, driven over `--remote-debugging-pipe`: CDP messages are JSON, each
//! ended by a NUL byte, read from fd 3 and written to fd 4 (`sh` puts the child's stdin and stdout there), as the
//! approval page's browser passkey test drives it. Every wait is bounded by [`WAIT`].
//!
//! The browser runs with `HOME` and its profile in the test's scratch directory, and with its background networking,
//! component updates and sync off: it asks nothing of the internet.

use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

/// The longest any one browser step may take.
pub const WAIT: Duration = Duration::from_secs(30);
/// Names another Chrome binary.
pub const CHROME_ENV: &str = "ARCA_TEST_CHROME";
/// The Chrome binary when `ARCA_TEST_CHROME` names none.
pub const DEFAULT_CHROME: &str = "/usr/bin/google-chrome";

/// The Chrome binary to drive: the file `ARCA_TEST_CHROME` names, or `/usr/bin/google-chrome`. `None` when the variable
/// is not set and there is no file at the default; a variable that names no file fails the test, so a typo never turns
/// the browser test into a skip.
pub fn binary() -> Option<PathBuf> {
    match std::env::var_os(CHROME_ENV) {
        Some(named) => {
            let path = PathBuf::from(named);
            assert!(
                path.is_file(),
                "{CHROME_ENV} names {}, and there is no file there",
                path.display()
            );
            Some(path)
        }
        None => {
            let path = PathBuf::from(DEFAULT_CHROME);
            path.is_file().then_some(path)
        }
    }
}

/// Says that `test` was skipped, and `why`, where a passing test's output is still seen: straight on stderr, which the
/// test harness does not capture (it captures only the `print!` macros), and, under GitHub Actions, as a warning on the
/// run's summary.
pub fn skipped(test: &str, why: &str) {
    let line = format!("SKIPPED {test}: {why}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    if std::env::var_os("GITHUB_ACTIONS").is_some_and(|value| value == "true") {
        let warning = format!("::warning title=Browser test skipped::{test}: {why}\n");
        let _ = std::io::stdout().write_all(warning.as_bytes());
    }
}

pub struct Chrome {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Value>,
    next: u64,
    session: String,
}

impl Chrome {
    /// Launches `binary` with its profile and `HOME` under `home`, on one blank page.
    pub fn launch(binary: &Path, home: &Path, log: &Path) -> Chrome {
        fs::create_dir_all(home).expect("the browser's home");
        fs::set_permissions(home, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
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
            .arg(format!(
                "--user-data-dir={}",
                home.join("profile").display()
            ))
            .arg("about:blank")
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(log).expect("the browser log"))
            .spawn()
            .unwrap_or_else(|e| panic!("start {}: {e}", binary.display()));
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
            next: 0,
            session: String::new(),
        };
        let target = chrome
            .send(None, "Target.createTarget", json!({ "url": "about:blank" }))
            .expect("a page");
        let attached = chrome
            .send(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target["targetId"], "flatten": true }),
            )
            .expect("the page's session");
        chrome.session = attached["sessionId"]
            .as_str()
            .expect("a session")
            .to_string();
        chrome
    }

    /// Sends one command, to the page's session or the browser, and waits for its answer; `Err` is the browser's error.
    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<Value, Value> {
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
                    return match reply.get("error") {
                        Some(error) => Err(error.clone()),
                        None => Ok(reply["result"].clone()),
                    };
                }
                // An event: nothing here waits for one.
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("{method} got no answer in time"),
                Err(RecvTimeoutError::Disconnected) => panic!("the browser exited during {method}"),
            }
        }
    }

    /// One command to the page, which must succeed.
    pub fn page(&mut self, method: &str, params: Value) -> Value {
        let session = self.session.clone();
        self.send(Some(&session), method, params)
            .unwrap_or_else(|error| panic!("{method}: {error}"))
    }

    /// Evaluates `js` in the page as a user gesture, awaiting a promise: its value, or `None` while the page is between
    /// documents or the expression threw.
    pub fn eval(&mut self, js: &str) -> Option<Value> {
        let session = self.session.clone();
        let answer = self
            .send(
                Some(&session),
                "Runtime.evaluate",
                json!({ "expression": js, "awaitPromise": true, "returnByValue": true, "userGesture": true }),
            )
            .ok()?;
        if answer.get("exceptionDetails").is_some() {
            return None;
        }
        Some(answer["result"]["value"].clone())
    }

    /// Opens `url` in the page.
    pub fn open(&mut self, url: &str) {
        self.page("Page.navigate", json!({ "url": url }));
    }

    /// Waits until the loaded document's `body` text holds `needle`: the address and the text. On a timeout it fails
    /// with what the page shows instead.
    pub fn until_text(&mut self, needle: &str) -> (String, String) {
        let deadline = Instant::now() + WAIT;
        let mut seen = (String::new(), String::new());
        loop {
            if let Some(value) = self.eval(
                "document.readyState === 'complete' ? [location.href, document.body ? document.body.innerText : ''] : null",
            ) && let (Some(href), Some(text)) = (value[0].as_str(), value[1].as_str())
            {
                seen = (href.to_string(), text.to_string());
                if seen.1.contains(needle) {
                    return seen;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the page shows {needle:?} in time; it is at {} and shows:\n{}",
                seen.0,
                seen.1
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Presses the first enabled button `selector` names once it is on the page, as a person's click does.
    pub fn press(&mut self, selector: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let pressed = self.eval(&format!(
                "(() => {{ const b = document.querySelector({selector:?}); if (document.readyState !== 'complete' || !b || b.disabled) return false; b.click(); return true; }})()"
            ));
            if pressed == Some(json!(true)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{selector} is on the page and enabled"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// A laptop's own authenticator holding one passkey: `private_pkcs8` (DER) for `credential_id`, for `rp_id`. It
    /// verifies the user and confirms presence by itself.
    pub fn passkey(&mut self, rp_id: &str, credential_id: &[u8], private_pkcs8: &[u8]) {
        self.page("WebAuthn.enable", json!({}));
        let added = self.page(
            "WebAuthn.addVirtualAuthenticator",
            json!({ "options": {
                "protocol": "ctap2", "transport": "internal", "hasResidentKey": true,
                "hasUserVerification": true, "isUserVerified": true, "automaticPresenceSimulation": true
            }}),
        );
        let authenticator = added["authenticatorId"]
            .as_str()
            .expect("an id")
            .to_string();
        self.page(
            "WebAuthn.addCredential",
            json!({
                "authenticatorId": authenticator,
                "credential": {
                    "credentialId": STANDARD.encode(credential_id),
                    "isResidentCredential": false,
                    "rpId": rp_id,
                    "privateKey": STANDARD.encode(private_pkcs8),
                    "signCount": 0,
                },
            }),
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

/// The PKCS#8 DER of a P-256 private key, as the virtual authenticator takes it: the `ecPrivateKey` of RFC 5915 with its
/// public point, in a PKCS#8 `PrivateKeyInfo` for `id-ecPublicKey` on `prime256v1`.
pub fn p256_pkcs8(signing: &p256::ecdsa::SigningKey) -> Vec<u8> {
    let scalar = signing.to_bytes();
    let point = signing.verifying_key().to_encoded_point(false);
    let mut der = vec![
        0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d,
        0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x6d, 0x30,
        0x6b, 0x02, 0x01, 0x01, 0x04, 0x20,
    ];
    der.extend_from_slice(&scalar);
    der.extend_from_slice(&[0xa1, 0x44, 0x03, 0x42, 0x00]);
    der.extend_from_slice(point.as_bytes());
    assert_eq!(der.len(), 138, "a P-256 PKCS#8 key with its public point");
    der
}
