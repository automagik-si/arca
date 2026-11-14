//! The runner pools as a kernel task: each pool of the node config's `runners` key runs on its own thread's runtime,
//! with the Incus launcher, its records under `<stateDir>/runners/<pool>/`, and its sign-in state on a board the alerts
//! read. `docs/RUNNERS.md` has the design.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arca_alerts::eval::{RunnerSignInUse, Timed};
use arca_runners::access::RegistrationAccess;
use arca_runners::incus::IncusLauncher;
use arca_runners::record::Recorder;
use arca_runners::scaleset::{Http, ScaleSetClient};
use arca_runners::{Pool, PoolConfig, Secret, SignInBoard};

use crate::config::NodeConfig;

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

/// GitHub's REST address for an organisation's address: `api.github.com` for github.com, `<host>/api/v3` for a GitHub
/// Enterprise Server.
pub fn api_url_for(org_url: &str) -> String {
    let host = org_url
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default();
    if host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com") {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    }
}

/// Reads a binding's token from its file. A missing or empty file is no credential.
fn read_token(dir: &Path, binding: &str) -> Option<Secret> {
    if binding.is_empty() || binding.contains('/') || binding.starts_with('.') {
        return None;
    }
    std::fs::read_to_string(dir.join(binding))
        .ok()
        .map(|text| Secret::new(text.trim()))
        .filter(|token| !token.is_empty())
}

/// Starts the node's pools, if it has any, on a thread of their own. Returns the board their sign-in state is on.
pub fn spawn(cfg: &NodeConfig, owner: &str) -> Result<Option<SignInBoard>, String> {
    let Some(runners) = cfg
        .runners
        .as_ref()
        .filter(|runners| !runners.pools.is_empty())
    else {
        return Ok(None);
    };
    let board = SignInBoard::new();
    let credentials: PathBuf = runners
        .credentials_dir
        .clone()
        .unwrap_or_else(|| cfg.state_dir.join("runner-credentials"));
    let records = cfg.state_dir.join("runners");
    let socket = cfg.incus.socket.clone();
    let pools: Vec<PoolConfig> = runners.pools.clone();
    let owner = owner.to_string();
    let shared = board.clone();
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
                    match start_pool(&pool, &socket, &credentials, &records, &shared, &owner) {
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
    Ok(Some(board))
}

fn start_pool(
    pool: &PoolConfig,
    socket: &Path,
    credentials: &Path,
    records: &Path,
    board: &SignInBoard,
    owner: &str,
) -> Result<Arc<Pool<RegistrationAccess, IncusLauncher>>, String> {
    let http = Http::new()?;
    let dir = credentials.to_path_buf();
    let binding = pool.binding.clone();
    let access = RegistrationAccess::with_source(
        http.clone(),
        &api_url_for(&pool.url),
        &pool.url,
        move || read_token(&dir, &binding),
    )?;
    let client = Arc::new(ScaleSetClient::new(http, Arc::new(access), &pool.binding));
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
    fn the_api_address_follows_the_host() {
        assert_eq!(
            api_url_for("https://github.com/acme"),
            "https://api.github.com"
        );
        assert_eq!(
            api_url_for("https://ghe.example.com/acme"),
            "https://ghe.example.com/api/v3"
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
