# Ownership model

This document states which Incus objects the Arca kernel may write, how an object's owner is read, and how a removal
proceeds. Each rule is followed by its `Source:`, the code or test in this repository that enforces it. Where a rule rests on
Incus behaviour that has not yet been checked against a live host, the rule says so, and the behaviour is listed under
"Assumptions to confirm". Such a rule holds against the fake Incus in this repository, and a live probe must confirm it before
any live write.

## Marks

**Rule 1.** Ownership is read from an object's own `config`, never from its `expanded_config`. A profile cannot change who
owns an object.
Source: `arca_kernel::ownership::classify` takes the own config; `crates/arca-kernel/tests/ownership.rs`, test
`a_mark_that_only_a_profile_carries_does_not_class_the_object`.

**Rule 2.** The kernel's marks sit in their own namespace. `user.arcakernel.entry` names the entry that owns the object.
`user.arcakernel.retiring` holds the digest of an approved delete plan and only ever sits beside an entry mark. A retiring
mark without an entry names no owner, so the object is foreign.
Source: `arca_kernel::ownership::{KERNEL_ENTRY_KEY, KERNEL_RETIRING_KEY}`; test
`a_retiring_tombstone_without_an_entry_is_foreign` in `crates/arca-kernel/tests/ownership.rs`.

**Rule 3.** The other system's marks are read-only to the kernel. Any key under `user.droid.`, `user.tactu.` or `user.arca.`
means that system owns the object. The match ignores ASCII case, and a bare namespace key counts. The kernel never writes such
an object, and it never adds such a key to one of its own, because the writer classifies the object's config after the
change and refuses any result that is not its entry's own.
Source: `arca_kernel::ownership::DROIDD_MARK_PREFIXES`; `UnixIncusWriter::patch` in `crates/arca-incus/src/write.rs`; tests
`droidd_namespace_matching_ignores_ascii_case_so_a_near_miss_only_refuses` (kernel) and
`a_patch_that_would_leave_the_object_owned_by_another_owner_is_refused` (`crates/arca-incus/tests/write.rs`).

## Classes

**Rule 4.** Every object has exactly one class. `Droidd` has a mark from the other system and no kernel mark. `Foreign` has no
mark. `Kernel { entry }` has an entry mark for an entry whose plan was not declined. `Retained { entry, plugin, plan }` has an
entry mark for an entry whose plan was declined. `Contested` has a mark from the other system and a kernel mark together.
Source: `arca_kernel::ownership::{OwnerClass, classify}`; `crates/arca-kernel/tests/ownership.rs`.

**Rule 5.** Every class may be read, and only the writer's own entry may write an object of its own class. Foreign, droidd,
contested and retained objects are written by nobody in this kernel.
Source: `arca_kernel::ownership::kernel_may_write` is true only for `Kernel { entry }` with the same entry;
`UnixIncusWriter::read_owned` and `refusal_reason` in `crates/arca-incus/src/write.rs`; test
`a_read_is_owned_only_for_this_entrys_own_kernel_object` in `crates/arca-incus/tests/write.rs`.

**Rule 6.** Ownership is checked at each write. A PATCH and a tombstone check the `Owned` value's config against the writer's
entry, and they send that value's ETag as `If-Match`, so Incus refuses the write with 412 when the object has changed since the
read. A DELETE alone starts from a fresh read, which must class as the writer's entry's own and show the tombstone. An `Owned`
value that names another entry is refused.
Source: `UnixIncusWriter::{patch, tombstone, delete, check}` in `crates/arca-incus/src/write.rs`; tests
`a_write_that_names_another_entrys_read_is_refused` and `a_stale_etag_is_412_and_changes_nothing`.

**Rule 7.** A retained object stays owned and visible. Its entry may not create new objects, and nothing in it is written.
Source: `UnixIncusWriter::create` and `read_owned` classify with the writer's retained set; test
`a_retained_object_is_never_owned_by_its_entry`.

## Projects

**Rule 8.** The kernel writes only inside the project family its configuration names. `ProjectFamily` holds those names and
nothing else. A target outside the family is refused before any request is sent, a read included. `bench` and `default` are
refused whatever the configuration says. The other system's projects come from the node's configuration, and
`ProjectFamily::new` refuses a family that names one of them, so such a configuration does not start. The provider's `effect`,
when it is added, must check the same family again before it applies anything.
Source: `write::ProjectFamily::{new, allows}` and `UnixIncusWriter::{allow_project, create, read_owned, check}` in
`crates/arca-incus/src/write.rs`; tests `a_target_outside_the_kernel_family_is_refused_before_any_request` and
`the_family_is_refused_at_construction_when_it_names_a_reserved_or_droidd_project` in `crates/arca-incus/tests/write.rs`.
`arca_kernel::plugin::Ctx::effect` refuses a project the manifest does not name (`ProjectNotAllowed`), a second check.

**Rule 9.** A project is named by its own name. A project's path names it alone, and a project whose name differs from its
`project` field is refused before any request.
Source: `write::urls` in `crates/arca-incus/src/write.rs`; test
`an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`.

**Rule 10.** A non-empty project is not deleted: Incus refuses the DELETE, and the project stays. The writer sends the DELETE
and returns Incus's answer as `IncusError::Status`. It does not count the project's objects first.
Source: `UnixIncusWriter::delete` returns the status error from `mutate`. The refusal is Incus's, and it is listed under
"Assumptions to confirm". Test `a_non_empty_project_is_not_deleted` models it on the fake (`crates/arca-incus/src/fake.rs`,
`delete`).

## Creates

**Rule 11.** A create carries this entry's mark and no other owner's key, and it names an image source only. The body's config
must classify as the writer's entry's own, with no retiring mark. A `source`, when the body has one, must be an image named by
alias, fingerprint or properties. A copy, a migration, a refresh, a server, a remote or a protocol is refused. The name must be
free: the object's GET must answer 404 before the POST is sent. Every refusal happens before any write.
Source: `UnixIncusWriter::create` and `write::check_source` in `crates/arca-incus/src/write.rs`; tests
`a_create_is_refused_unless_its_config_classes_as_the_entrys_own`, `a_create_takes_only_an_image_source_and_sends_nothing_else`
and `a_create_whose_name_is_taken_is_refused_and_nothing_is_written`.

**Rule 12.** A write that Incus accepted is confirmed by a read-back. After a create, a PATCH or a tombstone, the writer reads
the object again. The read must class as the writer's entry's own, and it must show the write: the change, or the tombstone on
the same uuid. If it does not, the result is `IncusError::AppliedUnconfirmed`, which carries the object, the class the read
found when it found one, and the reason. That error means the write landed. `IncusError::Refused` means that no write was sent.
Source: `UnixIncusWriter::confirm`; tests `a_create_that_lands_and_reads_back_foreign_is_applied_unconfirmed`,
`a_patch_that_reads_back_without_its_change_is_applied_unconfirmed`,
`a_tombstone_that_reads_back_without_the_tombstone_is_applied_unconfirmed`,
`a_tombstone_that_reads_back_on_another_uuid_is_applied_unconfirmed` and
`the_refused_and_unconfirmed_errors_say_whether_a_write_was_sent`.

## Writes

**Rule 13.** A PATCH carries the ETag of the read it was checked against, as `If-Match`. A PATCH with a stale ETag gets 412
and changes nothing, and the writer returns it as `IncusError::Status { code: 412 }`.
Source: `write::send_within` sets `If-Match`; `UnixIncusWriter::patch`; fake `patch` in `crates/arca-incus/src/fake.rs`; test
`a_stale_etag_is_412_and_changes_nothing`.

**Rule 14.** A write that Incus answers with an operation is settled by waiting on it. The writer GETs
`/1.0/operations/<id>/wait?timeout=5` until the operation's `status_code` is 200 with the status `Success`. It waits again while
the code is 1xx, which means still running. A 4xx or 5xx is the operation's failure, returned as `IncusError::Status` with its
`err` text, capped at 512 bytes. Any other code is a failure too. When the writer's overall deadline passes first, the result is
`IncusError::Timeout`. A write whose answer is sync, or names no operation, is done at once. A failed operation leaves the object
as it was. Against the fake the object stays, and a live probe must confirm it.
Source: `UnixIncusWriter::{wait_for, wait_on}` and `IncusError::Status` in `crates/arca-incus/src/read.rs`; tests
`the_operation_wait_follows_running_and_accepts_only_status_200_success`, `a_sync_answer_is_done_at_once_and_an_async_one_is_waited_on`,
`an_answer_with_an_empty_or_absent_operation_is_done_without_a_wait` and `a_failed_operation_is_a_status_error_and_the_object_stays`.

**Rule 15.** The writer sends only POST, PATCH and DELETE. It never sends PUT, and it never changes an object wholesale: a POST
creates, a PATCH sets or removes config keys, and a DELETE removes.
Source: the methods passed to `mutate` in `crates/arca-incus/src/write.rs` (`POST` in `create`, `PATCH` in `patch` and
`tombstone`, `DELETE` in `delete`); `UnixIncus` in `crates/arca-incus/src/read.rs` sends only GET; round-trip test, which asserts
the methods the fake receives.

**Rule 16.** A removed config key is sent as the empty string in a PATCH. The fake reads an empty value as unset.
Source: `write::config_patch` and fake `merge`; round-trip test. This encoding is listed under "Assumptions to confirm".

## Removal

**Rule 17.** Each journal record names its effect's class: `A` reverts at once, `B` is destructive and waits for an approved
delete plan, and `C` is an emission that is never reverted. A plugin's manifest declares each Incus kind it may write with its
class. The default classes this document proposes are: project `B`, instance `B`, volume `B`, bucket `B`; network `A`, ACL `A`,
profile `A`, zone `A`, forward `A`. No Incus kind is class `C`. `Ctx::effect` refuses an effect whose planned class the manifest
does not declare for that kind. Whether a declared class is stricter than the default is not checked.
Source: `arca_kernel::journal::{EffectClass, removal}` partitions by class; `arca_kernel::plugin::Manifest.effects` and the
`NotDeclared` check in `Ctx::effect`.

**Rule 18.** A destructive removal runs in five steps: read the object with its ETag and classify it; PATCH it with `If-Match`
to set `user.arcakernel.retiring` to the plan's digest; read it back and require the entry's mark, the tombstone and, for an
instance or a volume, the same uuid; read it again immediately before the delete and require the same; then DELETE. A DELETE
sends no `If-Match`, so a change that lands between the last read and the DELETE is not caught by Incus. That gap is the
residual risk of this step. It is covered by the one-writer rule (only the entry's own writer changes its objects) and by the
tombstone, which another writer would have to clear on purpose.
Source: `UnixIncusWriter::{tombstone, delete}` in `crates/arca-incus/src/write.rs`; tests
`a_delete_is_refused_when_the_tombstone_was_removed_after_it_was_set` and
`a_tombstone_with_a_stale_etag_is_412_and_sets_nothing`.

**Rule 19.** A false precondition is refused before any write, and nothing more is sent for that object. A read-back after a
write that shows the wrong uuid, no tombstone, another owner, or not the change is `AppliedUnconfirmed` (Rule 12). The caller
records drift for that object, and the plan becomes partial.
Source: the refusals in `tombstone` and `delete`, and the read-back in `confirm`; test
`a_tombstone_needs_a_uuid_where_incus_gives_one_and_a_delete_needs_its_tombstone`.

**Rule 20.** Only the kinds Incus gives a uuid carry one: an instance and a volume, through their `volatile.uuid` config key. An
instance or a volume without that key cannot be retired or deleted. Every other kind is identified by its name and ETag. Its
tombstone is set under its ETag, and its delete follows a fresh read that shows the tombstone. A target that names a uuid reads
only the object with that uuid, and a mismatch is refused before any write.
Source: `write::{has_uuid, uuid_of}`, `UnixIncusWriter::{classed, tombstone, delete}`; tests
`a_tombstone_needs_a_uuid_where_incus_gives_one_and_a_delete_needs_its_tombstone`,
`a_target_that_names_a_uuid_reads_only_the_object_with_that_uuid` and `each_kind_round_trips_through_create_patch_tombstone_and_delete`.

**Rule 21.** A delete reaches an object only through a tombstone that the writer has read back. A plugin cannot ask the
kernel to delete an object through its context.
Source: `arca_kernel::plugin::Ctx::effect` refuses `IncusDelete` (`NotDeclared`); test
`a_plugin_cannot_delete_through_ctx_and_no_provider_call_runs` in `crates/arca-kernel/tests/plugin.rs`.

**Rule 22.** A lost outcome suspends and never retires. An intent with no recorded outcome is unresolved. Removal reports it
and does not revert it, and the plugin stops serving while its objects stay.
Source: `arca_kernel::journal::{Journal::unresolved, removal}` report unresolved intents; test
`an_intent_with_no_outcome_is_unresolved_and_live_for_its_entry_only` in `crates/arca-kernel/tests/journal.rs`.

## Effects

**Rule 23.** Every effect runs in four steps. The provider's `plan` is read-only and returns the class, the op digest, the
pre-image of the keys a patch changes, and the ETag. The journal records the intent. The provider's `effect` applies the op
and returns the object and its inverse. The journal records the outcome with that inverse. A refusal at plan time writes no
record.
Source: `arca_kernel::plugin::Ctx::effect` and `Provider`; tests
`an_effect_is_planned_then_intended_then_applied_then_recorded_with_its_inverse`, `the_intent_is_on_the_journal_before_the_provider_applies`
and `a_plan_refusal_writes_no_record` in `crates/arca-kernel/tests/plugin.rs`.

**Rule 24.** The inverse is data from a closed vocabulary, built when the effect applies. A create's inverse is an `IncusDelete`
of the object the read-back showed. The kernel takes that inverse only for an object with a uuid (`required_inverse`), so a
create of a kind without one records no inverse until the kernel's contract is changed (see "Assumptions to confirm"). A patch's
inverse is an `IncusRestoreConfig` built from the pre-image read at plan time, where a key that was absent restores as absent.
Source: `arca_kernel::journal::Inverse`; `required_inverse` in `crates/arca-kernel/src/plugin.rs`; test
`a_patch_records_the_pre_image_as_its_inverse` in `crates/arca-kernel/tests/plugin.rs`.

**Rule 25.** Reads are open to every owner, and a read changes nothing. The watch report may read any class.
Source: `UnixIncusWriter::get_etag` does not classify; the GET-only client `arca_incus::UnixIncus` sends only GET.

**Rule 26.** Names are plain path segments. An object, project, pool, network or listen address is refused unless it is
letters, digits and `-_.:`, is not `.` or `..`, and is at most 253 bytes. Nothing is sent for a refused name.
Source: `write::segment`; test `an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`.

**Rule 27.** A volume and a bucket name their storage pool, and a forward names its network. Every other kind names no parent.
A target that breaks this is refused before any request.
Source: `write::urls`; test `an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`, which includes a volume
named without its pool and an instance that names a pool.

## Build

**Rule 28.** The writer is compiled only with the `write` feature of `arca-incus`, which is off by default. Only `arca-providers`
may enable it, so no other crate, the metrics sampler included, can send a write.
Source: `crates/arca-incus/Cargo.toml`, the `write` feature; `#[cfg(feature = "write")]` on the writer module in
`crates/arca-incus/src/lib.rs`; test `only_arca_providers_enables_the_write_feature` in `crates/arca-incus/tests/boundary.rs`.

## Out of scope

nftables rules, Caddy routes, tunnels and DNS records carry no Incus mark, so this model does not cover them. Their removal is
a separate rule set for the providers that own them.

## Assumptions to confirm

These hold in the fake and are the basis of the rules above. None has yet been checked against a live Incus, and each check
must run before any live write.

- Incus checks `If-Match` only on PUT and PATCH, so a DELETE has no compare step, and the tombstone is the only one (Rule 18).
- A PATCH merges the `config` map, and an empty value unsets a key (Rule 16).
- An instance's and a volume's uuid is their `volatile.uuid` key, and no other kind has a uuid (Rule 20).
- Incus answers sync to the project, network, profile, ACL, zone, forward and bucket writes, and to an instance's or a volume's
  PATCH. It answers an instance's or a volume's create and delete with an operation (Rule 14).
- The wait's answer carries the operation's `id`, `status_code` and `status`, and 200 with `Success` is the only success (Rule 14).
- A failed operation leaves the object as it was (Rule 14).
- Incus refuses the DELETE of a project that still holds an object (Rule 10).
- A name is free when its GET answers 404 (Rule 11).
- Incus accepts `config` on a POST for all nine kinds, and the object reads back with that config. The live create probe, one
  per kind, confirms it. The fake's rejection switch only tests how the writer handles a refusal; it says nothing about Incus.
- A create's inverse needs the object's uuid in the kernel's `required_inverse`, so projects, networks and the other kinds without
  one have no create inverse until the kernel accepts a name-keyed delete (Rule 24).
