//! The listener of one size class: the port of `actions/scaleset`'s `listener.Run`.
//!
//! It finds or makes the class's scale set, opens a message session, hands the session's statistics to the pool, then
//! polls the queue: each message's offered jobs are acquired first (so GitHub assigns them as early as possible), the
//! message is handed to the pool, and only then acknowledged. An expired queue token refreshes the session. Any other
//! failure closes the session and starts over after a pause that grows with each failure; a credential failure is put on
//! the sign-in board, which Arca's alerts read.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::access::Access;
use crate::launcher::Launcher;
use crate::pool::{BACKOFF_MAX, Pool};
use crate::scaleset::{ApiError, Session};

/// The pause after GitHub answers 409 to a session: another session is open on the scale set (another node, or one this
/// node left when it stopped, which GitHub ends after a few minutes).
pub const CONFLICT_PAUSE: Duration = Duration::from_secs(30);

/// Listens for one class until `stop` turns true.
pub async fn listen<A: Access, L: Launcher>(
    pool: Arc<Pool<A, L>>,
    class: usize,
    mut stop: watch::Receiver<bool>,
) {
    let mut failures: u32 = 0;
    loop {
        if *stop.borrow() {
            return;
        }
        let pause = match session(&pool, class, &mut stop, &mut failures).await {
            Ok(()) => return,
            Err(error) => {
                let name = pool.config().classes[class].scale_set_name().to_string();
                if let Some(reason) = error.signin() {
                    pool.signin().failing(&pool.config().name, reason);
                }
                failures = failures.saturating_add(1);
                let pause = match &error {
                    ApiError::Status { status: 409, .. } => CONFLICT_PAUSE,
                    _ => BACKOFF_MAX.min(Duration::from_secs(
                        2u64.saturating_mul(1 << failures.min(8)),
                    )),
                };
                eprintln!(
                    "arca: runners: pool {}: scale set {name}: {error}; trying again in {}s",
                    pool.config().name,
                    pause.as_secs()
                );
                pause
            }
        };
        tokio::select! {
            () = stopped(&mut stop) => return,
            () = tokio::time::sleep(pause) => {}
        }
    }
}

/// Resolves once `stop` turns true (or its sender is gone). The borrow of the value ends inside, so the future
/// stays `Send` across the `select!` it sits in.
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// One session, from its open to its close. `Ok` only when `stop` ended it.
async fn session<A: Access, L: Launcher>(
    pool: &Arc<Pool<A, L>>,
    class: usize,
    stop: &mut watch::Receiver<bool>,
    failures: &mut u32,
) -> Result<(), ApiError> {
    let scale_set = pool.scale_set(class).await?;
    let client = Arc::clone(pool.client());
    let mut session: Session = client.create_session(scale_set, pool.owner()).await?;
    pool.signin().ok(&pool.config().name);
    *failures = 0;
    pool.statistics(class, session.statistics);
    pool.reconcile(class);
    let mut last: i64 = 0;
    loop {
        let capacity = pool.capacity(class);
        let polled = tokio::select! {
            () = stopped(stop) => {
                let _ = client.delete_session(scale_set, &session).await;
                return Ok(());
            }
            polled = client.get_message(&session, last, capacity) => polled,
        };
        match polled {
            Ok(None) => {
                pool.signin().ok(&pool.config().name);
                pool.reconcile(class);
            }
            Ok(Some(message)) => {
                if !message.available.is_empty() {
                    let ids: Vec<i64> = message
                        .available
                        .iter()
                        .map(|job| job.runner_request_id)
                        .collect();
                    match client.acquire_jobs(scale_set, &session, &ids).await {
                        Ok(_) => {}
                        Err(ApiError::QueueTokenExpired) => {
                            session = client.refresh_session(scale_set, &session).await?;
                            client.acquire_jobs(scale_set, &session, &ids).await?;
                        }
                        Err(error) => return close(&client, scale_set, &session, error).await,
                    }
                }
                pool.message(class, &message);
                match client.delete_message(&session, message.message_id).await {
                    Ok(()) => {}
                    Err(ApiError::QueueTokenExpired) => {
                        session = client.refresh_session(scale_set, &session).await?;
                        client.delete_message(&session, message.message_id).await?;
                    }
                    Err(error) => return close(&client, scale_set, &session, error).await,
                }
                last = message.message_id;
                pool.signin().ok(&pool.config().name);
            }
            Err(ApiError::QueueTokenExpired) => {
                session = client.refresh_session(scale_set, &session).await?;
            }
            Err(error) => return close(&client, scale_set, &session, error).await,
        }
    }
}

async fn close<A: Access>(
    client: &crate::scaleset::ScaleSetClient<A>,
    scale_set: u64,
    session: &Session,
    error: ApiError,
) -> Result<(), ApiError> {
    let _ = client.delete_session(scale_set, session).await;
    Err(error)
}
