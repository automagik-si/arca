//! Callers that give up and ask again (issue 303). A caller that times out drops its connection, but the handler it
//! started keeps running on its blocking thread: nothing can stop a handler half way. Before the door shared runs, every
//! retry started one more handler next to the ones still running, so a slow read piled up handlers (and their Incus
//! reads and parses) for as long as callers kept asking. These tests count handler runs, not time.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use arca_api::{Scope, Verb, VerbClass, verbs};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, serve_unix};
use common::{Scratch, bearer, connect_unix, mint_into, post, post_bytes, rpc, wait_for_socket};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

/// The API table plus one write verb, so a test can show that writes are never shared.
fn table() -> &'static [Verb] {
    static TABLE: OnceLock<&'static [Verb]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut all = verbs().to_vec();
        all.push(Verb {
            name: "write_probe",
            summary: "A write verb for the pile-up tests.",
            class: VerbClass::L,
            min_scope: Scope::Doctor,
            writes: true,
            destructive: false,
            untrusted: false,
            output_key: None,
            summary_text: arca_api::verbs::summary_done,
            cut: arca_api::verbs::no_cut,
            params: || json!({ "type": "object", "properties": { "name": { "type": "string" } } }),
            result: || json!({ "type": "object" }),
            target: arca_api::limits::Target::Node,
        });
        Box::leak(all.into_boxed_slice())
    })
}

/// A gate the slow handlers wait at until the test opens it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    turned: Condvar,
}

impl Gate {
    fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut open = self.open.lock().expect("the gate");
        while !*open && Instant::now() < deadline {
            open = self
                .turned
                .wait_timeout(open, Duration::from_millis(100))
                .expect("the gate")
                .0;
        }
    }

    fn open(&self) {
        *self.open.lock().expect("the gate") = true;
        self.turned.notify_all();
    }
}

/// A handler as slow as the test wants: it counts each run it begins and each run still going, then waits at the gate.
struct Slow {
    answer: Value,
    started: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
    gate: Arc<Gate>,
}

impl ToolHandler for Slow {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.running.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(self.answer.clone())
    }
}

fn status_result() -> Value {
    json!({
        "version": "0.1.0",
        "node": "a".repeat(64),
        "mode": "watch-only",
        "uptimeS": 12,
        "incusReachable": false,
        "appliedSeq": null,
        "metricsSource": "local",
        "metricsSourceReason": "the test names the local mode"
    })
}

struct Fixture {
    _rt: Runtime,
    _scratch: Scratch,
    socket: std::path::PathBuf,
    viewer: String,
    other: String,
    doctor: String,
    started: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
    gate: Arc<Gate>,
}

impl Fixture {
    fn start(tag: &str) -> Fixture {
        let scratch = Scratch::new(tag);
        let viewer = mint_into(scratch.path(), "v-viewer", Scope::Viewer);
        let other = mint_into(scratch.path(), "v-other", Scope::Viewer);
        let doctor = mint_into(scratch.path(), "d-doctor", Scope::Doctor);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let (started, running, writes) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        let gate = Arc::new(Gate::default());
        let server = Server::new(table(), tokens)
            .log_to(Arc::new(|_| {}))
            .handle(
                "arca_status",
                Arc::new(Slow {
                    answer: status_result(),
                    started: started.clone(),
                    running: running.clone(),
                    gate: gate.clone(),
                }),
            )
            .handle(
                "write_probe",
                Arc::new(Slow {
                    answer: json!({}),
                    started: writes.clone(),
                    running: Arc::new(AtomicUsize::new(0)),
                    gate: gate.clone(),
                }),
            );
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Fixture {
            _rt: rt,
            _scratch: scratch,
            socket,
            viewer,
            other,
            doctor,
            started,
            running,
            writes,
            gate,
        }
    }

    /// One call by `token` that gives up after `patience`, as the cloud's meter and a polling page do: it sends the
    /// request, reads until its patience runs out and drops the connection. The server is not told anything else.
    fn give_up(&self, token: &str, name: &str, arguments: Value, patience: Duration) {
        let body = rpc(
            json!(1),
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .write_all(&post_bytes(&[bearer(token)], &body))
            .expect("send the call");
        stream
            .set_read_timeout(Some(patience))
            .expect("a read timeout");
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
    }

    /// Waits, at most a few seconds, until `count` reads `want`. The handlers run on threads of their own.
    fn settle(count: &AtomicUsize, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while count.load(Ordering::SeqCst) != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

const CALLERS: usize = 4;
const RETRIES: usize = 5;
const PATIENCE: Duration = Duration::from_millis(150);

#[test]
fn callers_that_give_up_and_retry_a_slow_read_start_one_handler_not_one_per_retry() {
    let fx = Arc::new(Fixture::start("pileup-retry"));
    let callers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let fx = Arc::clone(&fx);
            std::thread::spawn(move || {
                for _ in 0..RETRIES {
                    fx.give_up(&fx.viewer, "arca_status", json!({}), PATIENCE);
                }
            })
        })
        .collect();
    for caller in callers {
        caller.join().expect("a caller");
    }
    // Every one of the twenty calls was answered by the one run that is still going, or is waiting on it.
    let started = fx.started.load(Ordering::SeqCst);
    let running = fx.running.load(Ordering::SeqCst);
    fx.gate.open();
    Fixture::settle(&fx.running, 0);
    assert_eq!(
        (started, running),
        (1, 1),
        "{CALLERS} callers retrying {RETRIES} times each started {started} handlers, {running} of them still running"
    );
}

#[test]
fn a_caller_that_waits_gets_the_answer_of_the_run_it_joined_and_a_later_call_reads_again() {
    let fx = Arc::new(Fixture::start("pileup-join"));
    fx.give_up(&fx.viewer, "arca_status", json!({}), PATIENCE);
    Fixture::settle(&fx.started, 1);
    let waiter = {
        let fx = Arc::clone(&fx);
        std::thread::spawn(move || {
            let mut stream = connect_unix(&fx.socket);
            post(
                &mut stream,
                &[bearer(&fx.viewer)],
                &rpc(
                    json!(7),
                    "tools/call",
                    json!({ "name": "arca_status", "arguments": {} }),
                ),
            )
        })
    };
    std::thread::sleep(PATIENCE);
    fx.gate.open();
    let reply = waiter.join().expect("the waiter");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json()["result"]["structuredContent"]["version"],
        "0.1.0"
    );
    assert_eq!(
        fx.started.load(Ordering::SeqCst),
        1,
        "the waiter joined the run"
    );
    // The run ended: the next call is a read of its own, not an answer kept from before.
    let mut stream = connect_unix(&fx.socket);
    let again = post(
        &mut stream,
        &[bearer(&fx.viewer)],
        &rpc(
            json!(8),
            "tools/call",
            json!({ "name": "arca_status", "arguments": {} }),
        ),
    );
    assert_eq!(again.status, 200);
    assert_eq!(fx.started.load(Ordering::SeqCst), 2);
}

#[test]
fn reads_by_other_tokens_and_every_write_run_on_their_own() {
    let fx = Fixture::start("pileup-apart");
    fx.give_up(&fx.viewer, "arca_status", json!({}), PATIENCE);
    fx.give_up(&fx.other, "arca_status", json!({}), PATIENCE);
    fx.give_up(&fx.doctor, "write_probe", json!({ "name": "a" }), PATIENCE);
    fx.give_up(&fx.doctor, "write_probe", json!({ "name": "a" }), PATIENCE);
    Fixture::settle(&fx.writes, 2);
    let (reads, writes) = (
        fx.started.load(Ordering::SeqCst),
        fx.writes.load(Ordering::SeqCst),
    );
    fx.gate.open();
    Fixture::settle(&fx.running, 0);
    assert_eq!(writes, 2, "a write is never shared: each call is its own");
    assert_eq!(reads, 2, "another token's read is a run of its own");
}
