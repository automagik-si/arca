//! Moving a virtual machine from an outside hypervisor into Incus (`instance_import`). `DESIGN.md` beside this crate
//! states the design.
//!
//! [`source`] reads the outside hypervisor's description of one VM. [`plan`] turns it and the request into the owner's
//! card and the exact Incus bodies, the ACLs and the cloud-init seed. [`copy`] is the disk copy, by `rbd` or in memory.
//! [`run`] carries an approved plan out through those seams, and undoes it.

pub mod copy;
pub mod plan;
pub mod run;
pub mod source;

pub use copy::{DiskCopy, FakeCopy, ProcessRunner, RbdCopy, Runner, SourceImage, TargetImage};
pub use plan::{ACL_PREFIX, IMPORT_KEY, Plan, Request, Target};
pub use run::{Failure, Held, ImportIncus, Place, PoolInfo, Probe, incus_rbd_image, run, undo};
pub use source::{ParseError, SourceVm, parse, reduce, reduce_shown};

/// Parses `args.source` and builds the plan `args` ask for: what a proposal and an approved run both call, so the card
/// and the run come from the same function of the same arguments.
pub fn plan_of(args: &serde_json::Value) -> Result<Plan, String> {
    let request = Request::from_args(args)?;
    let xml = args
        .get("source")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "args/source: the source description is required".to_string())?;
    let source = parse(xml).map_err(|error| format!("args/source: {error}"))?;
    Plan::new(source, request)
}
