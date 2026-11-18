//! The server's own stream (`GET /mcp`, Streamable HTTP `2025-11-25`, "Listening for Messages from the Server").
//!
//! A caller whose terminal may be asked ([`crate::elicit`]) and whose client declared elicitation at `initialize` opens it.
//! Every plan that starts waiting and that may be decided on the stream
//! ([`crate::elicit::TerminalApprover::approvable_on_stream`]: never one that deletes or erases, for any caller) is put to
//! it as one `elicitation/create`, once per run of the node. The stored plan is checked again when the answer comes. A yes
//! approves and runs it, an explicit no in the owner's session declines it, and a cancel leaves it waiting on its card.
//! After a yes or a no the owner is told what became of it, in a message on the same stream. An answer given after the
//! session's passkey lapsed runs nothing: the owner is sent the step-up page, and the plan is asked again after the tap.
//!
//! Every event carries an id of its own, `<stream>.<n>`, unique across the session's streams. A client that lost the
//! connection opens `GET /mcp` again with `Last-Event-ID` within [`RESUME_FOR`]: the questions still open are sent again,
//! with new ids, and the stream goes on. A stream with no connection for longer ends; its open questions are left on their
//! cards. The stream ends at once when the caller may no longer be asked (its grant revoked), and every question open on it
//! goes with it.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::auth::Caller;
use crate::elicit::{Answer, Asking, Modes, PendingPlan, Terminal};
use crate::http::Server;

/// How often the stream looks for plans that started waiting.
const POLL: Duration = Duration::from_secs(1);
/// How often an idle connection is sent a comment, so a proxy in between keeps it open.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long a question on the stream waits for its answer: as long as a plan can wait. No call is held open by it.
const ANSWER: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a stream with no connection waits for its client to resume it.
pub(crate) const RESUME_FOR: Duration = Duration::from_secs(5 * 60);
/// The most events a stream keeps to send again on a resume. Its questions are far fewer: at most the waiting plans.
const KEPT_EVENTS: usize = 64;
/// The most streams remembered at once; past it the oldest ends.
const MAX_STREAMS: usize = 64;

/// One server stream: what was sent on it, and the connections that read it.
pub(crate) struct Stream {
    id: String,
    token: String,
    opened: Instant,
    state: Mutex<State>,
    /// The last event's number, and whether the stream ended: each change wakes the connections.
    pulse: watch::Sender<(u64, bool)>,
}

struct State {
    next: u64,
    /// The events kept to send again: number, the request id it asks (if it is a question), message.
    kept: VecDeque<(u64, Option<String>, Value)>,
    connections: usize,
    alone_since: Option<Instant>,
    ended: bool,
}

impl Stream {
    fn new(token: &str) -> Option<Arc<Stream>> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).ok()?;
        Some(Arc::new(Stream {
            id: URL_SAFE_NO_PAD.encode(raw),
            token: token.to_string(),
            opened: Instant::now(),
            state: Mutex::new(State {
                next: 0,
                kept: VecDeque::new(),
                connections: 0,
                alone_since: Some(Instant::now()),
                ended: false,
            }),
            pulse: watch::channel((0, false)).0,
        }))
    }

    /// Sends `message` on the stream; `asks` is the request id when it is a question. `false` once the stream ended.
    fn send(&self, message: Value, asks: Option<&str>) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.ended {
            return false;
        }
        state.next += 1;
        let number = state.next;
        state
            .kept
            .push_back((number, asks.map(str::to_string), message));
        while state.kept.len() > KEPT_EVENTS {
            state.kept.pop_front();
        }
        self.pulse.send_replace((number, false));
        true
    }

    /// The question `request` was answered or dropped: it is not sent again on a resume.
    fn settled(&self, request: &str) {
        if let Ok(mut state) = self.state.lock() {
            state
                .kept
                .retain(|(_, asks, _)| asks.as_deref() != Some(request));
        }
    }

    fn end(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.ended = true;
            state.kept.clear();
        }
        self.pulse.send_modify(|(_, ended)| *ended = true);
    }

    fn ended(&self) -> bool {
        self.pulse.borrow().1
    }

    /// Waits until the stream ended.
    async fn until_ended(&self) {
        let mut pulse = self.pulse.subscribe();
        let _ = pulse.wait_for(|(_, ended)| *ended).await;
    }

    /// Whether the stream has had no connection for longer than [`RESUME_FOR`].
    fn abandoned(&self) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.connections == 0
                && state
                    .alone_since
                    .is_some_and(|since| since.elapsed() >= RESUME_FOR)
        })
    }

    fn connected(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.connections > 0)
    }

    /// A new connection: its priming event's number, after which it reads. A resume from `after` sends the questions kept
    /// since then again, numbered anew after the priming event, so every id stays unique and a later resume from the
    /// priming event loses none of them.
    fn connect(&self, after: Option<u64>) -> Option<u64> {
        let mut state = self.state.lock().ok()?;
        if state.ended {
            return None;
        }
        state.connections += 1;
        state.alone_since = None;
        state.next += 1;
        let prime = state.next;
        if let Some(after) = after {
            let again: Vec<(Option<String>, Value)> = state
                .kept
                .iter()
                .filter(|(number, asks, _)| *number > after && asks.is_some())
                .map(|(_, asks, message)| (asks.clone(), message.clone()))
                .collect();
            state.kept.retain(|(_, asks, _)| asks.is_none());
            for (asks, message) in again {
                state.next += 1;
                let number = state.next;
                state.kept.push_back((number, asks, message));
            }
        } else {
            // A fresh connection reads only what comes after it.
        }
        let last = state.next;
        drop(state);
        self.pulse.send_replace((last, false));
        Some(prime)
    }

    fn disconnect(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.connections = state.connections.saturating_sub(1);
            if state.connections == 0 {
                state.alone_since = Some(Instant::now());
            }
        }
    }

    /// The kept events numbered after `after`, in order.
    fn after(&self, after: u64) -> Vec<(u64, Value)> {
        self.state.lock().map_or_else(
            |_| Vec::new(),
            |state| {
                state
                    .kept
                    .iter()
                    .filter(|(number, _, _)| *number > after)
                    .map(|(number, _, message)| (*number, message.clone()))
                    .collect()
            },
        )
    }

    fn event_id(&self, number: u64) -> String {
        format!("{}.{number}", self.id)
    }
}

/// A connection's hold on its stream; dropping it tells the stream the connection went.
struct Connection(Arc<Stream>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}

impl Terminal {
    /// The stream `id` names, when it is `token`'s and has not ended.
    fn stream_of(&self, id: &str, token: &str) -> Option<Arc<Stream>> {
        let streams = self.streams.lock().ok()?;
        streams
            .get(id)
            .filter(|stream| stream.token == token && !stream.ended())
            .cloned()
    }

    fn keep_stream(&self, stream: &Arc<Stream>) {
        let Ok(mut streams) = self.streams.lock() else {
            return;
        };
        streams.retain(|_, kept| !kept.ended());
        if streams.len() >= MAX_STREAMS
            && let Some(oldest) = streams
                .values()
                .min_by_key(|kept| kept.opened)
                .map(|kept| kept.id.clone())
            && let Some(gone) = streams.remove(&oldest)
        {
            gone.end();
        }
        streams.insert(stream.id.clone(), Arc::clone(stream));
    }

    fn forget_stream(&self, stream: &Stream) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.remove(&stream.id);
        }
    }
}

/// Opens the server's stream for `caller`, or resumes the one `last_event` (`Last-Event-ID`) names, when its terminal may
/// be asked and its client declared elicitation at `initialize`. `None` otherwise. What comes out is the SSE body: a
/// priming event, then one event per message, and a comment now and then while it is quiet.
pub(crate) fn open(
    server: &Arc<Server>,
    caller: &Caller,
    last_event: Option<&str>,
) -> Option<mpsc::Receiver<Bytes>> {
    let terminal = server.terminal.as_ref()?;
    if terminal.approver.may_ask(caller) == Asking::No {
        return None;
    }
    let modes = terminal.modes(&caller.token_id);
    if !(modes.form || modes.url) {
        return None;
    }
    let resumed = last_event
        .and_then(|id| id.rsplit_once('.'))
        .and_then(|(stream, number)| {
            let stream = terminal.stream_of(stream, &caller.token_id)?;
            Some((stream, number.parse::<u64>().ok()?))
        });
    let (stream, after) = match resumed {
        Some((stream, after)) => (stream, Some(after)),
        None => {
            let stream = Stream::new(&caller.token_id)?;
            terminal.keep_stream(&stream);
            tokio::spawn(push(
                Arc::clone(server),
                caller.clone(),
                modes,
                Arc::clone(&stream),
            ));
            (stream, None)
        }
    };
    let prime = stream.connect(after)?;
    let connection = Connection(Arc::clone(&stream));
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    tokio::spawn(read(connection, prime, tx));
    Some(rx)
}

/// Writes the stream to one connection, from its priming event on, until the stream ends or the client goes.
async fn read(connection: Connection, prime: u64, tx: mpsc::Sender<Bytes>) {
    let stream = Arc::clone(&connection.0);
    let mut pulse = stream.pulse.subscribe();
    if tx
        .send(Bytes::from(format!(
            "id: {}\ndata:\n\n",
            stream.event_id(prime)
        )))
        .await
        .is_err()
    {
        return;
    }
    let mut cursor = prime;
    loop {
        for (number, message) in stream.after(cursor) {
            let event = format!("id: {}\ndata: {message}\n\n", stream.event_id(number));
            if tx.send(Bytes::from(event)).await.is_err() {
                return;
            }
            cursor = number;
        }
        if stream.ended() {
            return;
        }
        tokio::select! {
            changed = tokio::time::timeout(KEEPALIVE, pulse.changed()) => match changed {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return,
                Err(_) => {
                    if tx.send(Bytes::from_static(b": keepalive\n\n")).await.is_err() {
                        return;
                    }
                }
            },
            () = tx.closed() => return,
        }
    }
}

/// Puts the waiting plans to `stream`, while `caller` may be asked and the stream has a connection or may still be resumed.
async fn push(server: Arc<Server>, caller: Caller, modes: Modes, stream: Arc<Stream>) {
    let Some(terminal) = server.terminal.as_ref() else {
        stream.end();
        return;
    };
    // The plans the owner was sent to the step-up page for on this stream since the passkey last counted, and that page's
    // open request. A later lapse is another step-up: each plan it finds waiting brings the page again.
    let mut stepped: HashSet<(String, u64)> = HashSet::new();
    let mut confirming: Option<String> = None;
    loop {
        let asking = terminal.approver.may_ask(&caller);
        if asking == Asking::No || stream.ended() || stream.abandoned() {
            break;
        }
        if asking == Asking::Yes
            && let Some(elicitation) = confirming.take()
        {
            let complete = json!({
                "jsonrpc": "2.0",
                "method": "notifications/elicitation/complete",
                "params": { "elicitationId": elicitation }
            });
            if !stream.send(complete, None) {
                break;
            }
        }
        if asking == Asking::Yes {
            stepped.clear();
        }
        // While nobody reads the stream, no plan is claimed for it: another stream may ask it.
        let plans = if stream.connected() {
            let approver = Arc::clone(&terminal.approver);
            let owned = caller.clone();
            tokio::task::spawn_blocking(move || {
                approver
                    .waiting_now(&owned)
                    .into_iter()
                    .filter(|plan| approver.approvable_on_stream(&plan.id, plan.created_at))
                    .collect::<Vec<PendingPlan>>()
            })
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|plan| !terminal.was_asked(plan))
            .collect()
        } else {
            Vec::new()
        };
        match &asking {
            Asking::Yes if modes.form => {
                for plan in plans {
                    if !ask(&server, &caller, modes, &stream, plan) {
                        break;
                    }
                }
            }
            Asking::StepUp { url } if modes.url => {
                let fresh: Vec<(String, u64)> = plans
                    .iter()
                    .map(|plan| (plan.id.clone(), plan.created_at))
                    .filter(|key| !stepped.contains(key))
                    .collect();
                if !fresh.is_empty() {
                    stepped.extend(fresh);
                    let Some(request_id) = step_up(&server, &caller, &stream, url) else {
                        break;
                    };
                    confirming = Some(request_id);
                }
            }
            _ => {}
        }
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            () = stream.until_ended() => break,
        }
    }
    stream.end();
    terminal.forget_stream(&stream);
}

/// Sends the step-up page on `stream` (URL mode) and answers its request id; the tap itself is seen as the session
/// becoming one that may be asked.
fn step_up(
    server: &Arc<Server>,
    caller: &Caller,
    stream: &Arc<Stream>,
    url: &str,
) -> Option<String> {
    let terminal = server.terminal.as_ref()?;
    let (request_id, answer) = terminal.expect(&caller.token_id)?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "elicitation/create",
        "params": crate::elicit::url_params(crate::elicit::STEP_UP_MESSAGE, url, &request_id)
    });
    if !stream.send(request, Some(&request_id)) {
        terminal.forget(&request_id);
        return None;
    }
    let (server, stream, id) = (Arc::clone(server), Arc::clone(stream), request_id.clone());
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::timeout(ANSWER, answer) => {}
            () = stream.until_ended() => {}
        }
        stream.settled(&id);
        if let Some(terminal) = server.terminal.as_ref() {
            terminal.forget(&id);
        }
    });
    Some(request_id)
}

/// Puts `plan` to `stream` when no stream or call asked it yet, and decides it when the answer comes. `false` when the
/// stream ended.
fn ask(
    server: &Arc<Server>,
    caller: &Caller,
    modes: Modes,
    stream: &Arc<Stream>,
    plan: PendingPlan,
) -> bool {
    let Some(terminal) = server.terminal.as_ref() else {
        return false;
    };
    if !terminal.claim_plan(&plan, now_seconds()) {
        return true;
    }
    let Some((request_id, answer)) = terminal.expect(&caller.token_id) else {
        terminal.release(&plan, true);
        return false;
    };
    let request = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "elicitation/create",
        "params": crate::elicit::request_params(&plan.message, caller.is_issued())
    });
    if !stream.send(request, Some(&request_id)) {
        terminal.forget(&request_id);
        terminal.release(&plan, true);
        return false;
    }
    let (server, caller, stream) = (Arc::clone(server), caller.clone(), Arc::clone(stream));
    tokio::spawn(async move {
        let Some(terminal) = server.terminal.as_ref() else {
            return;
        };
        let answered = tokio::select! {
            answered = tokio::time::timeout(ANSWER, answer) => answered.ok().and_then(Result::ok),
            () = stream.until_ended() => None,
        };
        terminal.forget(&request_id);
        stream.settled(&request_id);
        let said = answered.map_or(Answer::Left, |response| {
            crate::elicit::answer(&response["result"])
        });
        if said == Answer::Left {
            // A cancel, no answer, or the stream gone: the plan waits on its card, and no stream asks it again.
            terminal.release(&plan, false);
            return;
        }
        match terminal.approver.may_ask(&caller) {
            Asking::Yes => {}
            Asking::StepUp { url } => {
                // The passkey lapsed while the question was open: nothing runs. The stream sends the step-up page for it
                // (or, to a client without URL mode, says where it is), and asks it again after the tap.
                terminal.release(&plan, true);
                if !modes.url {
                    tell(
                        &server,
                        &caller,
                        &stream,
                        &format!(
                            "Plan {} was not run: confirm it is you with your passkey at {url}, then this session asks you again.",
                            plan.id
                        ),
                    )
                    .await;
                }
                return;
            }
            Asking::No => {
                terminal.release(&plan, false);
                return;
            }
        }
        // The stored plan decides, not the one that was put: one that deletes or erases, or one no longer waiting, is not
        // decided here.
        let approver = Arc::clone(&terminal.approver);
        let (id, created_at) = (plan.id.clone(), plan.created_at);
        let approvable =
            tokio::task::spawn_blocking(move || approver.approvable_on_stream(&id, created_at))
                .await
                .unwrap_or(false);
        let words = if approvable {
            let decision =
                crate::jsonrpc::decide(terminal, &caller, &plan.id, plan.created_at, said).await;
            outcome(&plan.id, &decision)
        } else {
            format!(
                "Plan {} was not run here: it is no longer waiting, or it deletes or erases and is approved on its card with your passkey.",
                plan.id
            )
        };
        terminal.release(&plan, false);
        tell(&server, &caller, &stream, &words).await;
    });
    true
}

/// The owner's words for what became of a plan asked about on the stream.
fn outcome(plan: &str, decision: &Value) -> String {
    match decision["decision"].as_str() {
        Some("approved") => format!("Approved: plan {plan} was run."),
        Some("declined") => format!("Declined: plan {plan} will not run."),
        Some("refused") => format!(
            "Plan {plan} was not run: {}",
            decision["message"]
                .as_str()
                .unwrap_or("the plan was not run")
        ),
        _ => format!("Plan {plan} is still waiting on its card."),
    }
}

/// Tells the owner `words` on `stream`: a form request with nothing to fill in, which the client shows as a message.
async fn tell(server: &Arc<Server>, caller: &Caller, stream: &Arc<Stream>, words: &str) {
    let Some(terminal) = server.terminal.as_ref() else {
        return;
    };
    let Some((request_id, answer)) = terminal.expect(&caller.token_id) else {
        return;
    };
    let message = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "elicitation/create",
        "params": told_params(words)
    });
    if stream.send(message, Some(&request_id)) {
        tokio::select! {
            _ = tokio::time::timeout(ANSWER, answer) => {}
            () = stream.until_ended() => {}
        }
        stream.settled(&request_id);
    }
    terminal.forget(&request_id);
}

/// The `elicitation/create` params of a message to read: form mode with an empty schema.
fn told_params(words: &str) -> Value {
    json!({
        "mode": "form",
        "message": words,
        "requestedSchema": { "type": "object", "properties": {} }
    })
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resume_numbers_the_open_questions_anew_after_its_priming_event() {
        let stream = Stream::new("t").expect("a stream");
        let first = stream.connect(None).expect("connected");
        assert!(stream.send(json!({ "q": 1 }), Some("r1")));
        assert!(stream.send(json!({ "n": 1 }), None));
        stream.disconnect();
        let prime = stream.connect(Some(first)).expect("resumed");
        let again = stream.after(prime);
        assert_eq!(again.len(), 1, "only the open question: {again:?}");
        assert!(again[0].0 > prime);
        assert_eq!(again[0].1, json!({ "q": 1 }));
        stream.settled("r1");
        assert!(stream.after(prime).is_empty());
    }
}
