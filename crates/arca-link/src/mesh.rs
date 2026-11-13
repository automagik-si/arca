//! Full snapshots converge membership without requiring every member to be online.
use crate::{
    PEERS_ALPN, Result, State,
    pairing::{read_line_bounded, write_line_bounded},
    path::Hints,
    runtime,
    state::Snapshot,
};
use iroh::{
    Endpoint,
    endpoint::{RecvStream, SendStream},
};
use std::{collections::BTreeSet, time::Duration};
use tokio::{task::JoinSet, time::timeout};

pub(crate) const LINE_LIMIT: usize = 64 * 1024;

pub(crate) async fn receive(
    state: &State,
    own_id: &str,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<bool> {
    let snapshot: Snapshot = runtime(serde_json::from_slice(
        &read_line_bounded(recv, LINE_LIMIT).await?,
    ))?;
    let changed = state.merge(snapshot, own_id)?;
    write_line_bounded(send, &state.snapshot()?, LINE_LIMIT).await?;
    runtime(send.finish())?;
    Ok(changed)
}

async fn exchange(
    endpoint: Endpoint,
    state: State,
    named: Hints,
    peer: crate::Peer,
) -> Result<bool> {
    let operation = async {
        let connection = runtime(
            endpoint
                .connect(named.dial(&peer.name, peer.addr()?), PEERS_ALPN)
                .await,
        )?;
        named.track(&connection);
        let result = async {
            let (mut send, mut recv) = runtime(connection.open_bi().await)?;
            write_line_bounded(&mut send, &state.snapshot()?, LINE_LIMIT).await?;
            runtime(send.finish())?;
            let snapshot: Snapshot = runtime(serde_json::from_slice(
                &read_line_bounded(&mut recv, LINE_LIMIT).await?,
            ))?;
            state.merge(snapshot, &endpoint.id().to_string())
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    };
    runtime(timeout(Duration::from_secs(10), operation).await)?
}

/// Try each member, including newly learned members, once. Offline peers are harmless.
pub(crate) async fn sync(endpoint: &Endpoint, state: &State, named: &Hints) -> Result<bool> {
    let mut attempted = BTreeSet::new();
    let mut changed = false;
    loop {
        let peers: Vec<_> = state
            .peers()?
            .into_iter()
            .filter(|p| p.id != endpoint.id().to_string() && attempted.insert(p.id.clone()))
            .collect();
        if peers.is_empty() {
            break;
        }
        let mut tasks = JoinSet::new();
        for peer in peers {
            if tasks.len() == 16 {
                changed |= matches!(tasks.join_next().await, Some(Ok(Ok(true))));
            }
            tasks.spawn(exchange(
                endpoint.clone(),
                state.clone(),
                named.clone(),
                peer,
            ));
        }
        while let Some(result) = tasks.join_next().await {
            changed |= matches!(result, Ok(Ok(true)));
        }
    }
    Ok(changed)
}
