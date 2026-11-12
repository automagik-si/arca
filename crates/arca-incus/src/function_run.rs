//! One run of a function: the writes the kernel sends to a function's own instance, outside the desired-state journal.
//!
//! A function is a container the kernel starts for one run and stops itself. This module is the only path that does so.
//! It forces a stop when a run passes its time limit; the instance verbs' own writer, [`StateChanger::change`], never
//! forces one, and nothing here is reachable through it. (A forced stop is also part of the kernel's delete of an
//! instance and of the folder and adoption writers, each on its own object.) Every call reads the instance first and refuses one that does not carry the function mark
//! ([`FUNCTION_KEY`]) in its own config, or that is not a container, before anything is written. A service's instance
//! never carries the mark, so no service is ever started, stopped or force-stopped from here.
//!
//! What a run writes to Incus: the instance's `state` (a start, and a stop when the run passes its time limit) and its
//! console log (cleared before the start, so what is read after the run is that run's output and nothing older). It
//! never creates, deletes, renames or reconfigures an instance.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::lifecycle::{InstanceState, StateChanger, checked_envelope, plain_name, refused};
use crate::read::{IncusError, exchange_capped};
use crate::write::ProjectFamily;

/// The key, in an instance's own config, that marks it as a function's: the kernel writes it when it creates one.
pub const FUNCTION_KEY: &str = "user.arca.function";
/// The most of a run's output that is read, in bytes. A longer log is an error, never a silent cut.
pub const OUTPUT_MAX: usize = 1024 * 1024;
/// The longest one start or stop operation is waited on.
const DEADLINE: Duration = Duration::from_secs(60);

/// Whether `state` is a function's instance: a container whose own config carries the mark.
pub fn is_function(state: &InstanceState) -> bool {
    state.kind == "container"
        && state.own_config.get(FUNCTION_KEY).map(String::as_str) == Some("true")
}

/// The writer for one function run.
#[derive(Debug, Clone)]
pub struct FunctionRun {
    changer: StateChanger,
    socket: PathBuf,
    timeout: Duration,
}

impl FunctionRun {
    /// A writer that may address only the projects of `family`.
    pub fn new(socket: PathBuf, family: ProjectFamily) -> Self {
        Self {
            changer: StateChanger::new(socket.clone(), family).with_deadline(DEADLINE),
            socket,
            timeout: crate::read::TIMEOUT,
        }
    }

    /// Reads one instance and refuses it unless it is a function's.
    pub fn read(&self, project: &str, name: &str) -> Result<InstanceState, IncusError> {
        let state = self.changer.read(project, name)?;
        if !is_function(&state) {
            return Err(refused(
                "the instance is not a function's: it does not carry the function mark",
            ));
        }
        Ok(state)
    }

    /// Reads the instance again and refuses one that was recreated since `seen`, or that is no longer a function's.
    fn fresh(&self, seen: &InstanceState) -> Result<InstanceState, IncusError> {
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid {
            return Err(refused("the instance was recreated since it was read"));
        }
        Ok(fresh)
    }

    /// The status of the instance now, as Incus spells it.
    pub fn status(&self, seen: &InstanceState) -> Result<String, IncusError> {
        Ok(self.fresh(seen)?.status)
    }

    /// Starts the instance. One that already runs is refused: a run never joins a start it did not make.
    pub fn start(&self, seen: &InstanceState) -> Result<(), IncusError> {
        let fresh = self.fresh(seen)?;
        if !fresh.is_stopped() {
            return Err(refused("the instance is not stopped"));
        }
        self.put_state(&fresh, &json!({ "action": "start" }))
    }

    /// Stops the instance: a graceful stop that Incus gives `grace` to finish, then a forced one. With no grace the stop
    /// is forced at once. An instance that is already stopped is left as it is. Returns whether the stop was forced.
    pub fn stop(&self, seen: &InstanceState, grace: Duration) -> Result<bool, IncusError> {
        let fresh = self.fresh(seen)?;
        if fresh.is_stopped() {
            return Ok(false);
        }
        if !grace.is_zero() {
            let body =
                json!({ "action": "stop", "timeout": grace.as_secs().max(1), "force": false });
            // A graceful stop that fails leaves the instance running: the forced stop below is what ends it.
            let _ = self.put_state(&fresh, &body);
            let after = self.fresh(seen)?;
            if after.is_stopped() {
                return Ok(false);
            }
        }
        let fresh = self.fresh(seen)?;
        if fresh.is_stopped() {
            return Ok(false);
        }
        let forced = self.put_state(&fresh, &json!({ "action": "stop", "force": true }));
        let after = self.fresh(seen)?;
        if after.is_stopped() {
            return Ok(true);
        }
        forced?;
        Err(refused("the instance still runs after a forced stop"))
    }

    /// Clears the instance's console log, so the next read holds only what is written from now on. An instance with no
    /// log yet has nothing to clear.
    pub fn clear_output(&self, seen: &InstanceState) -> Result<(), IncusError> {
        let fresh = self.fresh(seen)?;
        let path = console_url(&fresh, "")?;
        let request = format!(
            "DELETE {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        let framed = exchange_capped(&self.socket, self.timeout, request.as_bytes(), OUTPUT_MAX)?;
        match checked_envelope(framed) {
            Ok(_) | Err(IncusError::Status { code: 404, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The instance's console log: what its program wrote. An instance with no log has written nothing.
    pub fn output(&self, seen: &InstanceState) -> Result<String, IncusError> {
        let fresh = self.fresh(seen)?;
        let path = console_url(&fresh, "&type=log")?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: incus\r\nAccept: text/plain\r\nConnection: close\r\n\r\n"
        );
        let framed = exchange_capped(&self.socket, self.timeout, request.as_bytes(), OUTPUT_MAX)?;
        if framed.status == 404 {
            return Ok(String::new());
        }
        if !(200..300).contains(&framed.status) {
            return Err(IncusError::Status {
                code: framed.status,
                message: framed.reason,
            });
        }
        Ok(String::from_utf8_lossy(&framed.body).into_owned())
    }

    /// The output of a run that has just ended. Incus is still putting the instance away for a moment after its
    /// program exits, and a read of the log in that moment is answered with an error: the read is tried again until it
    /// is answered or `patience` has passed, and the last answer is returned.
    pub fn output_after_exit(
        &self,
        seen: &InstanceState,
        patience: Duration,
    ) -> Result<String, IncusError> {
        let from = std::time::Instant::now();
        loop {
            match self.output(seen) {
                Err(IncusError::Status { code, .. })
                    if code >= 500 && from.elapsed() < patience =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                answer => return answer,
            }
        }
    }

    fn put_state(&self, fresh: &InstanceState, body: &Value) -> Result<(), IncusError> {
        self.changer.put_state(&fresh.project, &fresh.name, body)
    }
}

fn console_url(state: &InstanceState, query: &str) -> Result<String, IncusError> {
    if !plain_name(&state.project) || !plain_name(&state.name) {
        return Err(refused("the project or instance name is not plain"));
    }
    Ok(format!(
        "/1.0/instances/{}/console?project={}{query}",
        state.name, state.project
    ))
}

#[cfg(test)]
#[path = "function_run_proof.rs"]
mod proof;
