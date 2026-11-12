//! `store check [--config <file>]`: proves this build's SQLite opens a store as the server does. It opens a scratch store in a
//! fresh directory inside the state directory, through the same open path as `arca.db`, prints the pragmas it read back,
//! and removes the scratch directory. It never opens the node's own `arca.db`.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;

use arca_store::Store;
use serde_json::{Value, json};

use crate::handlers::now_ms;
use crate::{Failure, Flags};

pub fn check(flags: &Flags) -> Result<Option<Value>, Failure> {
    let cfg = crate::load_config_to(flags, crate::Does::Acts)?.1;
    let scratch = cfg.state_dir.join(format!(
        ".arca-store-check-{}-{}",
        std::process::id(),
        now_ms()
    ));
    DirBuilder::new()
        .mode(0o700)
        .create(&scratch)
        .map_err(|e| Failure::Runtime(format!("the scratch directory cannot be made: {e}")))?;
    // The store is closed before its directory goes, so its WAL and shared memory files go with it.
    let pragmas = Store::open(&scratch, now_ms()).map(|opened| opened.pragmas);
    let removed = fs::remove_dir_all(&scratch);
    let pragmas = pragmas.map_err(|e| Failure::Runtime(e.to_string()))?;
    removed.map_err(|e| Failure::Runtime(format!("the scratch store cannot be removed: {e}")))?;
    Ok(Some(json!({
        "sqlite_version": pragmas.sqlite_version,
        "journal_mode": pragmas.journal_mode,
        "synchronous": pragmas.synchronous,
        "auto_vacuum": pragmas.auto_vacuum,
    })))
}
