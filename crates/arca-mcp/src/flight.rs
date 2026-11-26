//! One run for identical reads in flight (issue 303).
//!
//! A tool's handler runs on a blocking thread, and nothing can stop it half way: a caller that gives up drops its
//! connection, and the handler it started runs on to its end. A caller that then asks again used to start another handler
//! next to it. While a read was slow (a full Incus listing that takes seconds), callers that time out and retry (the
//! cloud's meter, a page that polls, an agent) piled up handlers without bound, each doing the same Incus read and the
//! same parse: the node spent its CPUs on reads nobody was waiting for any more, and every read slowed down further.
//!
//! So a read-only call that is the same as one still running (same verb, same caller, same arguments) joins that run and
//! gets its answer, instead of starting one of its own. The run holds no answer once it has ended: the next call reads
//! again. Writes, and every verb that is not advertised read-only, are never shared.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::sync::watch;

use crate::tools::CallError;

/// How a run ended: the handler's answer, or why there is none.
#[derive(Clone)]
pub(crate) enum Ran {
    Answered(Result<Value, CallError>),
    /// The handler panicked.
    Panicked,
    /// The run was stopped before it answered (the runtime is shutting down).
    Stopped,
}

/// The read-only runs in flight, by key.
#[derive(Default)]
pub(crate) struct Flights {
    runs: Mutex<HashMap<String, watch::Receiver<Option<Ran>>>>,
}

impl Flights {
    /// The key of a call: its verb, its caller (the token with its scope, tool list and limits, as one text) and its
    /// arguments. Arguments are a JSON object, whose keys serde_json keeps sorted, so the same arguments give the same
    /// text.
    pub(crate) fn key(verb: &str, caller: &str, arguments: &Value) -> String {
        format!("{verb}\n{caller}\n{arguments}")
    }

    /// How many runs are going now.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Runs `call` on a blocking thread, or joins the run of `key` that is still going, and waits for its answer. The run
    /// takes itself out of the table when it ends, whether anyone still waits or not.
    pub(crate) async fn run(
        self: &Arc<Self>,
        key: String,
        call: impl FnOnce() -> Result<Value, CallError> + Send + 'static,
    ) -> Ran {
        let mut answer = self.begin(key, call);
        match answer.wait_for(Option::is_some).await {
            Ok(ran) => ran.clone().unwrap_or(Ran::Stopped),
            // The run's sender went without an answer: its thread never ran (the runtime is shutting down).
            Err(_) => Ran::Stopped,
        }
    }

    /// Joins the run of `key` that is going, or starts `call` as that run: where its answer will come.
    fn begin(
        self: &Arc<Self>,
        key: String,
        call: impl FnOnce() -> Result<Value, CallError> + Send + 'static,
    ) -> watch::Receiver<Option<Ran>> {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = runs.get(&key) {
            return running.clone();
        }
        let (tx, rx) = watch::channel(None);
        runs.insert(key.clone(), rx.clone());
        let flights = Arc::clone(self);
        // The handle is not kept: the run goes on whether or not its first caller still waits.
        drop(tokio::task::spawn_blocking(move || {
            let ran = match catch_unwind(AssertUnwindSafe(call)) {
                Ok(result) => Ran::Answered(result),
                Err(_) => Ran::Panicked,
            };
            // Out of the table before the answer is sent: a call that comes after the answer reads again.
            flights
                .runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
            let _ = tx.send(Some(ran));
        }));
        rx
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn identical_calls_in_flight_share_one_run_and_the_table_empties_after_it() {
        let flights = Arc::new(Flights::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let key = Flights::key("arca_status", "v-viewer", &json!({}));
        let answers: Vec<_> = (0..8)
            .map(|_| {
                let (calls, gate) = (Arc::clone(&calls), Arc::clone(&gate));
                flights.begin(key.clone(), move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let _ = gate
                        .lock()
                        .expect("the gate")
                        .recv_timeout(Duration::from_secs(10));
                    Ok(json!({ "ok": true }))
                })
            })
            .collect();
        assert_eq!(flights.in_flight(), 1, "eight identical calls, one run");
        open.send(()).expect("open the gate");
        for mut answer in answers {
            let ran = answer
                .wait_for(Option::is_some)
                .await
                .expect("an answer")
                .clone();
            assert!(
                matches!(ran, Some(Ran::Answered(Ok(ref value))) if *value == json!({ "ok": true }))
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(flights.in_flight(), 0, "an ended run keeps no answer");
        // A different token or other arguments is a run of its own.
        let other = Flights::key("arca_status", "v-other", &json!({}));
        assert_ne!(other, key);
        assert_ne!(
            Flights::key("arca_status", "v-viewer", &json!({ "minutes": 5 })),
            key
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_run_answers_panicked_and_leaves_the_table() {
        let flights = Arc::new(Flights::default());
        let ran = flights
            .run(Flights::key("arca_status", "t", &json!({})), || {
                panic!("a handler bug")
            })
            .await;
        assert!(matches!(ran, Ran::Panicked));
        assert_eq!(flights.in_flight(), 0);
    }
}
