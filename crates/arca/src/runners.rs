//! The runner pools as a kernel task: each pool of the node config's `runners` key runs on its own thread's runtime,
//! signs in with the kernel's GitHub signer (its binding names a signer connection), makes its machines with the Incus
//! launcher, keeps its records under `<stateDir>/runners/<pool>/`, and puts its sign-in state on a board the alerts
//! read. `docs/RUNNERS.md` has the design.

use std::path::Path;
use std::sync::Arc;

use arca_alerts::eval::{RunnerSignInUse, Timed};
use arca_runners::incus::IncusLauncher;
use arca_runners::record::Recorder;
use arca_runners::scaleset::{Http, ScaleSetClient};
use arca_runners::{
    Access, AccessError, ActionsConnection, JobRef, MintRequest, Minted, Pool, PoolConfig, Secret,
    SignInBoard, SignInReason,
};

use crate::config::NodeConfig;
use crate::github_app::{JobRef as SignerJobRef, MintRefused, MintRequest as SignerMintRequest};
use crate::github_signer::Signer;

/// The alert input of a tick: one row per pool, with why it cannot sign in. No board, no rows.
pub fn alert_input(board: Option<&SignInBoard>, now_ms: u64) -> Timed<Vec<RunnerSignInUse>> {
    let value = board
        .map(|board| {
            board
                .read()
                .into_iter()
                .map(|(pool, state)| RunnerSignInUse {
                    pool,
                    reason: state.failing.map(|reason| reason.as_str().to_string()),
                })
                .collect()
        })
        .unwrap_or_default();
    Timed { at: now_ms, value }
}

/// [`arca_runners::Access`] over the kernel's GitHub signer: the pool's binding is a signer binding, its Actions
/// service connection and every runner configuration come from the signer's App, and every mint is in the signer's
/// audit log. The pool never holds the App's key or an installation token.
#[derive(Clone)]
pub struct SignerAccess {
    signer: Signer,
}

impl SignerAccess {
    pub fn new(signer: Signer) -> Self {
        Self { signer }
    }
}

/// What the pool should do with a signer's refusal. Refusals that a person must act on in GitHub or in Arca are
/// sign-in failures, with the reason the alert shows.
pub fn access_error(refused: MintRefused) -> AccessError {
    // A refusal the signer decided from its own record (no GitHub answer behind it) has no status.
    let reason = |reason| AccessError::SignIn { reason, status: 0 };
    match refused {
        MintRefused::UnknownBinding => AccessError::NoCredential,
        MintRefused::NotInstalled | MintRefused::Revoked { .. } => {
            reason(SignInReason::AppNotInstalled)
        }
        MintRefused::Suspended { .. } => reason(SignInReason::Forbidden),
        MintRefused::GitHub {
            status: Some(401), ..
        } => AccessError::SignIn {
            reason: SignInReason::BadCredentials,
            status: 401,
        },
        MintRefused::GitHub {
            status: Some(403), ..
        } => AccessError::SignIn {
            reason: SignInReason::Forbidden,
            status: 403,
        },
        MintRefused::GitHub {
            status: Some(status),
            why,
            ..
        } => AccessError::GitHub {
            status,
            message: why,
        },
        MintRefused::GitHub {
            status: None, why, ..
        } => AccessError::Network(why),
        other => AccessError::Refused(other.message()),
    }
}

impl Access for SignerAccess {
    async fn actions_connection(&self, binding: &str) -> Result<ActionsConnection, AccessError> {
        let connection = self
            .signer
            .actions_connection(binding)
            .await
            .map_err(access_error)?;
        Ok(ActionsConnection {
            service_url: connection.service_url,
            admin_token: Secret::new(connection.admin_token.expose()),
            expires_at_ms: connection.expires_at,
        })
    }

    async fn mint(&self, request: MintRequest) -> Result<Minted, AccessError> {
        let minted = self
            .signer
            .mint(SignerMintRequest {
                binding: request.binding,
                runner_name: request.runner_name,
                labels: request.labels,
                work_folder: Some(request.work_folder),
                scale_set_id: Some(request.scale_set_id),
                job: None,
                caller: request.caller,
            })
            .await
            .map_err(access_error)?;
        Ok(Minted {
            mint_id: minted.mint_id,
            runner_id: minted.runner_id,
            runner_name: minted.runner_name,
            encoded_jit_config: Secret::new(minted.encoded_jit_config.expose()),
        })
    }

    async fn attach_job(&self, mint_id: u64, job: JobRef) {
        self.signer.attach_job(
            mint_id,
            &SignerJobRef {
                repo: job.repo,
                run_id: job.run_id,
                job_id: job.job_id.and_then(|id| id.parse().ok()),
                job_name: job.job_name,
                workflow_ref: job.workflow_ref,
            },
        );
    }

    async fn runner_group(&self, binding: &str) -> Result<Option<u64>, AccessError> {
        match self.signer.binding(binding) {
            Ok(info) => Ok(info.runner_group_id),
            Err(_) => Err(AccessError::NoCredential),
        }
    }

    fn forget_connection(&self, binding: &str) {
        self.signer.forget_actions_connection(binding);
    }
}

/// Starts the node's pools on a thread of their own, with the signer's App as their credential and their sign-in
/// state on `board`. A node with no pools starts nothing.
pub fn spawn(
    cfg: &NodeConfig,
    owner: &str,
    signer: Signer,
    board: SignInBoard,
) -> Result<(), String> {
    let Some(runners) = cfg
        .runners
        .as_ref()
        .filter(|runners| !runners.pools.is_empty())
    else {
        return Ok(());
    };
    let records = cfg.state_dir.join("runners");
    let socket = cfg.incus.socket.clone();
    let pools: Vec<PoolConfig> = runners.pools.clone();
    let owner = owner.to_string();
    let access = Arc::new(SignerAccess::new(signer));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("arca-runners")
        .enable_all()
        .build()
        .map_err(|error| format!("the runner pools' runtime could not start: {error}"))?;
    std::thread::Builder::new()
        .name("arca-runners".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                let (_keep, stop) = tokio::sync::watch::channel(false);
                let mut tasks = Vec::new();
                for pool in pools {
                    match start_pool(&pool, &socket, &access, &records, &board, &owner) {
                        Ok(pool) => tasks.push(tokio::spawn(pool.run(stop.clone()))),
                        Err(why) => {
                            eprintln!("arca: runners: pool {} does not start: {why}", pool.name)
                        }
                    }
                }
                for task in tasks {
                    let _ = task.await;
                }
            });
        })
        .map_err(|error| format!("the runner pools' thread could not start: {error}"))?;
    Ok(())
}

fn start_pool(
    pool: &PoolConfig,
    socket: &Path,
    access: &Arc<SignerAccess>,
    records: &Path,
    board: &SignInBoard,
    owner: &str,
) -> Result<Arc<Pool<SignerAccess, IncusLauncher>>, String> {
    let http = Http::new()?;
    let client = Arc::new(ScaleSetClient::new(http, Arc::clone(access), &pool.binding));
    let launcher = IncusLauncher::new(
        socket.to_path_buf(),
        &pool.project,
        &pool.name,
        &pool.image,
        pool.profiles.clone(),
        pool.storage_pool.clone(),
    )
    .map_err(|error| error.to_string())?;
    let recorder = Recorder::open(records.join(&pool.name), pool.log_days)
        .map_err(|error| format!("its folder could not be made: {error}"))?;
    eprintln!(
        "arca: runners: pool {}: {} class(es) in project {}, budget {} CPUs and {} MiB",
        pool.name,
        pool.classes.len(),
        pool.project,
        pool.budget.cpu,
        pool.budget.memory
    );
    Ok(Pool::new(
        pool.clone(),
        client,
        Arc::new(launcher),
        board.clone(),
        recorder,
        owner,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signer_refusal_a_person_must_act_on_is_a_signin_failure() {
        let reason = |refused| access_error(refused).signin();
        assert_eq!(
            reason(MintRefused::UnknownBinding),
            Some(SignInReason::NoCredential)
        );
        assert_eq!(
            reason(MintRefused::NotInstalled),
            Some(SignInReason::AppNotInstalled)
        );
        assert_eq!(
            reason(MintRefused::Revoked {
                why: "uninstalled".to_string()
            }),
            Some(SignInReason::AppNotInstalled)
        );
        assert_eq!(
            reason(MintRefused::Suspended {
                why: "suspended".to_string()
            }),
            Some(SignInReason::Forbidden)
        );
        let github = |status| MintRefused::GitHub {
            status,
            why: "no".to_string(),
            retry: false,
        };
        assert_eq!(
            reason(github(Some(401))),
            Some(SignInReason::BadCredentials)
        );
        assert_eq!(reason(github(Some(403))), Some(SignInReason::Forbidden));
        // GitHub's own trouble and a network failure keep the last sign-in state.
        assert_eq!(reason(github(Some(502))), None);
        assert_eq!(reason(github(None)), None);
        // A refusal of the request itself is the pool's settings, not its sign-in.
        assert_eq!(
            reason(MintRefused::ScaleSetOutsideGroup {
                scale_set: 1,
                scale_set_group: Some(2),
                group: Some(3),
            }),
            None
        );
        assert_eq!(
            reason(MintRefused::LabelsNotAllowed { allowed: vec![] }),
            None
        );
    }

    #[test]
    fn a_board_becomes_one_row_per_pool() {
        let board = SignInBoard::new();
        board.ok("a");
        board.failing("b", arca_runners::SignInReason::AppNotInstalled);
        let input = alert_input(Some(&board), 5);
        assert_eq!(input.at, 5);
        assert_eq!(input.value.len(), 2);
        assert_eq!(input.value[1].reason.as_deref(), Some("app-not-installed"));
        assert!(alert_input(None, 5).value.is_empty());
    }
}
