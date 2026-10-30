//! A reconciler resumed after a restart from the converged set it kept on disk. An entry that converged with the same config
//! before the restart is not applied again, so a restart writes nothing the reconciler had already written.

mod common;

use arca_kernel::desired::{Applied, DesiredStateGate, EntryId};
use arca_kernel::journal::{FileJournal, Journal};
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PluginSet, Reconciler};
use common::*;

const ENTRY: &str = "toy-scratch";

#[test]
fn a_resumed_reconciler_does_not_apply_again_an_entry_that_converged_before_the_restart() {
    let scratch = Scratch::new("resume");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let doc = signed(&key, 1, None, vec![toy_entry("t1")]);

    let mut first = reconciler(&scratch, &fake, &key, &retained);
    assert_eq!(
        first.reconcile(&doc, NOW),
        vec![Outcome::Applied {
            entry: EntryId(ENTRY.to_string())
        }]
    );
    assert_eq!(first.converged(), vec![toy_entry("t1")]);
    let writes_before_restart = fake.writes().len();
    drop(first);

    // The restart: a new reconciler on the same journal, resumed with the converged set and the applied document.
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    let mut plugins = PluginSet::new();
    plugins
        .add::<arca_reconcile::toy::Toy>()
        .expect("the toy is valid");
    let applied = Applied {
        seq: 1,
        digest: doc.document.digest(),
    };
    let mut second = Reconciler::open_resuming(
        DesiredStateGate::new(key.public(), Some(applied)),
        journal,
        plugins,
        Box::new(provider(&scratch, &fake, &retained)),
        retained.clone(),
        vec![toy_entry("t1")],
        NOW,
    )
    .expect("the journal has no unresolved intent");

    assert_eq!(
        second.reconcile(&doc, NOW),
        vec![],
        "a converged entry is not applied again"
    );
    assert_eq!(
        fake.writes().len(),
        writes_before_restart,
        "the restart sends no write"
    );
}
