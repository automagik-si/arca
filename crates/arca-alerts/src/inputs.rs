//! The tick's [`Inputs`], built from the sampler's newest sample.

use arca_incus::IncusRead;
use arca_metrics::Sampler;

use crate::eval::{Inputs, PoolUse, Timed};

/// The inputs of the sampler's newest sample, stamped with its time. Before the first sample every source is unknown.
pub fn from_sampler<R: IncusRead>(sampler: &Sampler<R>) -> Inputs {
    let Some(sample) = sampler.latest() else {
        return Inputs::default();
    };
    let pools = sample
        .pools
        .iter()
        .map(|pool| PoolUse {
            name: pool.name.clone(),
            used_bytes: pool.used_bytes,
            total_bytes: pool.total_bytes,
        })
        .collect();
    Inputs {
        at: sample.at,
        pools: Some(Timed {
            at: sample.at,
            value: pools,
        }),
        ..Inputs::default()
    }
}
