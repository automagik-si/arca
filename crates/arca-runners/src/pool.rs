//! The controller of one pool.
//!
//! Per size class it keeps `min(max, warm + assigned)` runners alive, where `assigned` is GitHub's count of the class's
//! jobs that are assigned and not finished, and as long as the pool's budget has room. Each runner is one machine:
//! made, handed one just-in-time configuration, watched until its runner exits, its logs read, destroyed. A machine is
//! never reused. A machine that never took a job is destroyed too, and its runner removed from GitHub.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::access::{Access, JobRef, MintRequest};
use crate::config::PoolConfig;
use crate::launcher::{LaunchError, Launcher, MachineSpec, RunnerStart};
use crate::record::{JobRecord, Recorder};
use crate::scaleset::{ApiError, JobMessage, Message, ScaleSetClient, Statistics};
use crate::signin::SignInBoard;
use crate::time::{now_ms, parse_rfc3339};

/// How long a machine may take from its create until its agent answers.
pub const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a machine's runner may live: GitHub's own longest job is 6 hours; a runner past this is ended.
pub const RUNNER_TIMEOUT: Duration = Duration::from_secs(7 * 3600);
/// How long an idle runner above the wanted count is kept before its machine goes.
pub const IDLE_GRACE: Duration = Duration::from_secs(300);
/// The longest pause before a class tries again after failures.
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Where a machine is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Being made, booting, or being handed its runner.
    Booting,
    /// Its runner is registered and waits for a job.
    Idle,
    /// Its runner runs a job.
    Busy,
    /// Its runner exited or it is being ended: its logs are read and it is destroyed.
    Ending,
}

#[derive(Debug)]
struct Machine {
    class: usize,
    phase: Phase,
    created_ms: u64,
    runner_start_ms: Option<u64>,
    idle_since_ms: u64,
    started: Option<JobMessage>,
    completed: Option<JobMessage>,
    mint_id: Option<u64>,
    runner_id: Option<u64>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Default, Clone, Copy)]
struct ClassState {
    scale_set_id: Option<u64>,
    assigned: i64,
    failures: u32,
    retry_at_ms: u64,
}

#[derive(Debug, Default)]
struct State {
    classes: Vec<ClassState>,
    machines: BTreeMap<String, Machine>,
    cpu_used: u32,
    memory_used: u64,
    stopping: bool,
}

/// One class as the pool sees it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassStatus {
    pub scale_set: String,
    pub scale_set_id: Option<u64>,
    pub assigned: i64,
    pub booting: u32,
    pub idle: u32,
    pub busy: u32,
    pub ending: u32,
}

/// The pool now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    pub classes: Vec<ClassStatus>,
    pub cpu_used: u32,
    pub memory_used_mib: u64,
}

/// One pool's controller.
pub struct Pool<A: Access, L: Launcher> {
    config: PoolConfig,
    client: Arc<ScaleSetClient<A>>,
    launcher: Arc<L>,
    signin: SignInBoard,
    recorder: Recorder,
    owner: String,
    state: Mutex<State>,
}

impl<A: Access, L: Launcher> Pool<A, L> {
    /// A controller for `config`. `owner` names this node in GitHub's sessions.
    pub fn new(
        config: PoolConfig,
        client: Arc<ScaleSetClient<A>>,
        launcher: Arc<L>,
        signin: SignInBoard,
        recorder: Recorder,
        owner: &str,
    ) -> Arc<Self> {
        let classes = vec![ClassState::default(); config.classes.len()];
        Arc::new(Self {
            config,
            client,
            launcher,
            signin,
            recorder,
            owner: owner.to_string(),
            state: Mutex::new(State {
                classes,
                ..State::default()
            }),
        })
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    pub fn client(&self) -> &Arc<ScaleSetClient<A>> {
        &self.client
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn signin(&self) -> &SignInBoard {
        &self.signin
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn say(&self, text: &str) {
        eprintln!("arca: runners: pool {}: {text}", self.config.name);
    }

    /// Runs the pool until `stop` turns true: removes what an earlier run left, starts one listener per class, and
    /// sweeps old logs every hour. On stop, the listeners close their sessions and the machines without a job go.
    pub async fn run(self: Arc<Self>, stop: watch::Receiver<bool>) {
        self.clean_leftovers().await;
        self.recorder.sweep();
        let mut listeners = Vec::new();
        for class in 0..self.config.classes.len() {
            listeners.push(tokio::spawn(crate::listener::listen(
                Arc::clone(&self),
                class,
                stop.clone(),
            )));
        }
        let mut stop_wait = stop.clone();
        let mut sweep = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tokio::select! {
                () = crate::listener::stopped(&mut stop_wait) => break,
                _ = sweep.tick() => { self.recorder.sweep(); }
            }
        }
        for listener in listeners {
            let _ = listener.await;
        }
        self.stop_machines().await;
    }

    /// Destroys every machine of the pool that exists before the pool starts: a kernel that stopped leaves none behind
    /// for long. Its runner, if GitHub still lists it, is removed too.
    async fn clean_leftovers(&self) {
        let launcher = Arc::clone(&self.launcher);
        let names = match blocking(move || launcher.existing()).await {
            Ok(names) => names,
            Err(error) => {
                self.say(&format!(
                    "the machines left by an earlier run could not be listed: {error}"
                ));
                return;
            }
        };
        for name in names {
            self.say(&format!("removing machine {name}, left by an earlier run"));
            let launcher = Arc::clone(&self.launcher);
            let gone = name.clone();
            if let Err(error) = blocking(move || launcher.destroy(&gone)).await {
                self.say(&format!("machine {name} could not be removed: {error}"));
            }
            if let Ok(Some(runner)) = self.client.get_runner_by_name(&name).await {
                let _ = self.client.remove_runner(runner.id).await;
            }
        }
    }

    /// Ends the machines whose runner has no job, and waits for every machine to be gone or busy.
    async fn stop_machines(&self) {
        {
            let mut state = self.lock();
            state.stopping = true;
            for machine in state.machines.values() {
                if machine.phase != Phase::Busy {
                    machine.cancel.store(true, Ordering::SeqCst);
                }
            }
        }
        for _ in 0..600 {
            let left = self
                .lock()
                .machines
                .values()
                .filter(|machine| machine.phase != Phase::Busy)
                .count();
            if left == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The scale set of a class: found by name in the runner group, made when there is none, and given the class's
    /// labels when GitHub's differ.
    pub(crate) async fn scale_set(&self, class: usize) -> Result<u64, ApiError> {
        let config = &self.config.classes[class];
        let group = self.config.runner_group_id;
        let name = config.scale_set_name();
        let id = match self.client.get_scale_set(group, name).await? {
            Some(found) => {
                let mut theirs: Vec<&str> = found
                    .labels
                    .iter()
                    .map(|label| label.name.as_str())
                    .collect();
                let mut ours: Vec<&str> = config.labels.iter().map(String::as_str).collect();
                theirs.sort_unstable();
                ours.sort_unstable();
                if theirs != ours {
                    self.say(&format!(
                        "scale set {name}: setting its labels to {}",
                        config.labels.join(",")
                    ));
                    self.client
                        .update_scale_set(found.id, group, name, &config.labels)
                        .await?;
                }
                found.id
            }
            None => {
                self.say(&format!(
                    "scale set {name}: creating it in runner group {group}"
                ));
                self.client
                    .create_scale_set(group, name, &config.labels)
                    .await?
                    .id
            }
        };
        self.lock().classes[class].scale_set_id = Some(id);
        Ok(id)
    }

    /// How many runners GitHub may count on for a class: its machines now, plus what the free budget holds, up to
    /// its max.
    pub fn capacity(&self, class: usize) -> u32 {
        let state = self.lock();
        let config = &self.config.classes[class];
        let live = live_of(&state, class);
        let free_cpu = self.config.budget.cpu.saturating_sub(state.cpu_used) / config.cpu.max(1);
        let free_memory =
            self.config.budget.memory.saturating_sub(state.memory_used) / config.memory.max(1);
        let more = u32::try_from(u64::from(free_cpu).min(free_memory)).unwrap_or(u32::MAX);
        live.saturating_add(more).min(config.max)
    }

    /// Takes the statistics GitHub sent with a session or a message.
    pub fn statistics(&self, class: usize, statistics: Option<Statistics>) {
        if let Some(statistics) = statistics {
            self.lock().classes[class].assigned = statistics.total_assigned_jobs.max(0);
        }
    }

    /// Takes one message of a class's queue: jobs that started and finished, and the statistics.
    pub fn message(self: &Arc<Self>, class: usize, message: &Message) {
        let mut attach = Vec::new();
        {
            let mut state = self.lock();
            for job in &message.started {
                if let Some(machine) = state.machines.get_mut(&job.runner_name) {
                    machine.phase = Phase::Busy;
                    machine.started = Some(job.clone());
                    if let Some(mint) = machine.mint_id {
                        attach.push((mint, job.clone()));
                    }
                }
            }
            for job in &message.completed {
                if let Some(machine) = state.machines.get_mut(&job.runner_name) {
                    machine.completed = Some(job.clone());
                }
            }
        }
        for job in &message.completed {
            let queued = queue_seconds(job)
                .map_or_else(String::new, |seconds| format!(", queued {seconds:.1}s"));
            self.say(&format!(
                "job {} of {} on {}: {}{queued}",
                job.job_display_name, job.repository_name, job.runner_name, job.result
            ));
        }
        for (mint, job) in attach {
            let access = Arc::clone(self.client.access());
            tokio::spawn(async move {
                access
                    .attach_job(
                        mint,
                        JobRef {
                            repo: format!("{}/{}", job.owner_name, job.repository_name),
                            run_id: u64::try_from(job.workflow_run_id).ok(),
                            job_id: Some(job.job_id.clone()),
                            job_name: Some(job.job_display_name.clone()),
                        },
                    )
                    .await;
            });
        }
        self.statistics(class, message.statistics);
        self.reconcile(class);
    }

    /// Starts the machines a class wants and has room for, and ends one idle machine above what it wants.
    pub fn reconcile(self: &Arc<Self>, class: usize) {
        let config = &self.config.classes[class];
        let mut spawn = Vec::new();
        {
            let mut state = self.lock();
            if state.stopping {
                return;
            }
            let now = now_ms();
            let assigned = u32::try_from(state.classes[class].assigned).unwrap_or(u32::MAX);
            let wanted = config.warm.saturating_add(assigned).min(config.max);
            let mut live = live_of(&state, class);
            while live < wanted
                && now >= state.classes[class].retry_at_ms
                && state.cpu_used + config.cpu <= self.config.budget.cpu
                && state.memory_used + config.memory <= self.config.budget.memory
            {
                let name = machine_name(&self.config.name, class);
                let cancel = Arc::new(AtomicBool::new(false));
                state.cpu_used += config.cpu;
                state.memory_used += config.memory;
                state.machines.insert(
                    name.clone(),
                    Machine {
                        class,
                        phase: Phase::Booting,
                        created_ms: now,
                        runner_start_ms: None,
                        idle_since_ms: now,
                        started: None,
                        completed: None,
                        mint_id: None,
                        runner_id: None,
                        cancel: Arc::clone(&cancel),
                    },
                );
                spawn.push((name, cancel));
                live += 1;
            }
            if live > wanted {
                let grace = u64::try_from(IDLE_GRACE.as_millis()).unwrap_or(u64::MAX);
                if let Some(extra) = state.machines.values().find(|machine| {
                    machine.class == class
                        && machine.phase == Phase::Idle
                        && now.saturating_sub(machine.idle_since_ms) > grace
                }) {
                    extra.cancel.store(true, Ordering::SeqCst);
                }
            }
        }
        for (name, cancel) in spawn {
            tokio::spawn(Arc::clone(self).machine(class, name, cancel));
        }
    }

    /// One machine's whole life, and what follows it: the record, the class's backoff, a reconcile.
    async fn machine(self: Arc<Self>, class: usize, name: String, cancel: Arc<AtomicBool>) {
        let outcome = self.machine_life(class, &name, &cancel).await;
        let exit = match &outcome {
            Ok(Some(code)) => Some(*code),
            _ => None,
        };
        let launcher = Arc::clone(&self.launcher);
        let pulled = name.clone();
        let logs = match blocking(move || launcher.collect_logs(&pulled)).await {
            Ok(Some(bytes)) => match self.recorder.logs(&name, &bytes) {
                Ok(path) => Some(path),
                Err(error) => {
                    self.say(&format!(
                        "machine {name}: its logs could not be kept: {error}"
                    ));
                    None
                }
            },
            Ok(None) | Err(_) => None,
        };
        let launcher = Arc::clone(&self.launcher);
        let doomed = name.clone();
        if let Err(error) = blocking(move || launcher.destroy(&doomed)).await {
            self.say(&format!("machine {name} could not be destroyed: {error}"));
        }
        // GitHub's JobCompleted can trail the runner's exit by a poll: wait a little for it, so the record has the result.
        for _ in 0..150 {
            let waiting =
                self.lock().machines.get(&name).is_some_and(|machine| {
                    machine.started.is_some() && machine.completed.is_none()
                });
            if !waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (machine, ran) = {
            let state = self.lock();
            let machine = state.machines.get(&name);
            (
                machine.map(|machine| {
                    (
                        machine.runner_id,
                        machine.started.clone(),
                        machine.completed.clone(),
                        machine.created_ms,
                        machine.runner_start_ms,
                    )
                }),
                machine.is_some_and(|machine| machine.started.is_some()),
            )
        };
        let Some((runner_id, started, completed, created_ms, runner_start_ms)) = machine else {
            return;
        };
        if !ran && let Some(runner) = runner_id {
            // A runner that never took a job stays registered on GitHub until removed.
            if let Err(error) = self.client.remove_runner(runner).await {
                self.say(&format!(
                    "runner {name} could not be removed from GitHub: {error}"
                ));
            }
        }
        let note = match &outcome {
            Ok(_) => None,
            Err(why) => Some(why.clone()),
        };
        if let Some(why) = &note {
            self.say(&format!("machine {name}: {why}"));
        }
        let job = completed.or(started).unwrap_or_default();
        let record = JobRecord {
            runner: name.clone(),
            class: self.config.classes[class].scale_set_name().to_string(),
            repository: if job.owner_name.is_empty() {
                job.repository_name.clone()
            } else {
                format!("{}/{}", job.owner_name, job.repository_name)
            },
            workflow_ref: job.job_workflow_ref.clone(),
            run_id: job.workflow_run_id,
            job_id: job.job_id.clone(),
            job_name: job.job_display_name.clone(),
            event: job.event_name.clone(),
            result: job.result.clone(),
            queued_at: job.queue_time.clone(),
            assigned_at: job.runner_assign_time.clone(),
            finished_at: job.finish_time.clone(),
            queue_seconds: queue_seconds(&job),
            boot_seconds: runner_start_ms.map(|start| seconds(start.saturating_sub(created_ms))),
            machine_seconds: seconds(now_ms().saturating_sub(created_ms)),
            runner_exit: exit,
            logs,
            note: note.clone(),
        };
        if let Err(error) = self.recorder.job(&record) {
            self.say(&format!("the job record could not be written: {error}"));
        }
        {
            let mut state = self.lock();
            let config = &self.config.classes[class];
            state.machines.remove(&name);
            state.cpu_used = state.cpu_used.saturating_sub(config.cpu);
            state.memory_used = state.memory_used.saturating_sub(config.memory);
            let class_state = &mut state.classes[class];
            // A failure before the runner started is the machine's or the credential's: the class waits before the
            // next try, longer each time, so a broken image or credential does not spin.
            if note.is_some() && runner_start_ms.is_none() && !cancel.load(Ordering::SeqCst) {
                class_state.failures = class_state.failures.saturating_add(1);
                let pause = BACKOFF_MAX.min(Duration::from_secs(
                    5u64.saturating_mul(1 << class_state.failures.min(6)),
                ));
                class_state.retry_at_ms = now_ms() + u64::try_from(pause.as_millis()).unwrap_or(0);
            } else if runner_start_ms.is_some() {
                class_state.failures = 0;
                class_state.retry_at_ms = 0;
            }
        }
        self.reconcile(class);
    }

    /// Makes the machine, hands it its runner, and waits for the runner to exit. `Ok(Some(code))` when it exited,
    /// `Ok(None)` when it was ended without exiting, `Err` with what failed.
    async fn machine_life(
        &self,
        class: usize,
        name: &str,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<i64>, String> {
        let config = &self.config.classes[class];
        let spec = MachineSpec {
            name: name.to_string(),
            cpu: config.cpu,
            memory_mib: config.memory,
            disk_gib: config.disk_gib,
        };
        let launcher = Arc::clone(&self.launcher);
        blocking(move || launcher.create(&spec))
            .await
            .map_err(|error| format!("the machine could not be made: {error}"))?;
        let started = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let launcher = Arc::clone(&self.launcher);
            let probed = name.to_string();
            if blocking(move || launcher.ready(&probed))
                .await
                .map_err(|error| format!("the machine could not be read: {error}"))?
            {
                break;
            }
            if started.elapsed() > READY_TIMEOUT {
                return Err("the machine did not boot in time".to_string());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let scale_set_id = self.lock().classes[class]
            .scale_set_id
            .ok_or("the class has no scale set yet")?;
        let minted = self
            .client
            .access()
            .mint(MintRequest {
                binding: self.config.binding.clone(),
                scale_set_id,
                runner_name: name.to_string(),
                labels: config.labels.clone(),
                work_folder: self.config.work_folder.clone(),
                caller: format!("runner-pool:{}", self.config.name),
            })
            .await
            .map_err(|error| {
                if let Some(reason) = error.signin() {
                    self.signin.failing(&self.config.name, reason);
                }
                format!("no runner configuration: {error}")
            })?;
        {
            let mut state = self.lock();
            if let Some(machine) = state.machines.get_mut(name) {
                machine.mint_id = Some(minted.mint_id);
                machine.runner_id = Some(minted.runner_id);
            }
        }
        let launcher = Arc::clone(&self.launcher);
        let target = name.to_string();
        let start = RunnerStart {
            runner_dir: self.config.runner_dir.clone(),
        };
        let jit = minted.encoded_jit_config;
        blocking(move || launcher.start_runner(&target, &jit, &start))
            .await
            .map_err(|error| format!("the runner could not be started: {error}"))?;
        {
            let mut state = self.lock();
            if let Some(machine) = state.machines.get_mut(name) {
                machine.runner_start_ms = Some(now_ms());
                machine.idle_since_ms = now_ms();
                if machine.phase == Phase::Booting {
                    machine.phase = Phase::Idle;
                }
            }
        }
        let started = std::time::Instant::now();
        loop {
            let busy = self
                .lock()
                .machines
                .get(name)
                .is_some_and(|machine| machine.phase == Phase::Busy);
            if !busy && cancel.load(Ordering::SeqCst) {
                return Ok(None);
            }
            if started.elapsed() > RUNNER_TIMEOUT {
                return Err("the runner ran past its time limit".to_string());
            }
            let launcher = Arc::clone(&self.launcher);
            let watched = name.to_string();
            if let Some(code) =
                blocking(move || launcher.runner_exited(&watched, Duration::from_secs(5)))
                    .await
                    .map_err(|error| format!("the runner could not be watched: {error}"))?
            {
                if let Some(machine) = self.lock().machines.get_mut(name) {
                    machine.phase = Phase::Ending;
                }
                return Ok(Some(code));
            }
        }
    }

    /// The pool now.
    pub fn status(&self) -> PoolStatus {
        let state = self.lock();
        let classes = self
            .config
            .classes
            .iter()
            .enumerate()
            .map(|(index, class)| {
                let count = |phase: Phase| {
                    u32::try_from(
                        state
                            .machines
                            .values()
                            .filter(|machine| machine.class == index && machine.phase == phase)
                            .count(),
                    )
                    .unwrap_or(u32::MAX)
                };
                ClassStatus {
                    scale_set: class.scale_set_name().to_string(),
                    scale_set_id: state.classes[index].scale_set_id,
                    assigned: state.classes[index].assigned,
                    booting: count(Phase::Booting),
                    idle: count(Phase::Idle),
                    busy: count(Phase::Busy),
                    ending: count(Phase::Ending),
                }
            })
            .collect();
        PoolStatus {
            classes,
            cpu_used: state.cpu_used,
            memory_used_mib: state.memory_used,
        }
    }
}

/// The class's machines that count toward what it wants: all but those ending.
fn live_of(state: &State, class: usize) -> u32 {
    u32::try_from(
        state
            .machines
            .values()
            .filter(|machine| machine.class == class && machine.phase != Phase::Ending)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// A machine's name, which is its runner's name on GitHub too: the pool, the class's index, and 8 random hex digits.
fn machine_name(pool: &str, class: usize) -> String {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{pool}-{class}-{hex}")
}

/// From GitHub's queue time to the runner's assignment, in seconds.
pub fn queue_seconds(job: &JobMessage) -> Option<f64> {
    let queued = parse_rfc3339(&job.queue_time)?;
    let assigned = parse_rfc3339(&job.runner_assign_time)?;
    Some(seconds(assigned.checked_sub(queued)?))
}

fn seconds(ms: u64) -> f64 {
    // Milliseconds of a job fit an f64 exactly.
    #[allow(clippy::cast_precision_loss)]
    let seconds = ms as f64 / 1000.0;
    seconds
}

/// Runs a launcher call on a blocking thread.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, LaunchError> + Send + 'static,
) -> Result<T, LaunchError> {
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or_else(|_| Err(LaunchError("the launcher call panicked".to_string())))
}
