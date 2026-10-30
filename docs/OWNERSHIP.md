# Ownership model

This document states which Incus objects the Arca kernel may write, how an object's owner is read, and how a removal
proceeds. Each rule is followed by its `Source:`, the code or test in this repository that enforces it. Where a rule rests on
Incus behaviour that has not yet been checked against a live host, it is listed under "Assumptions to confirm" and is not
treated as settled.

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
`UnixIncusWriter::read_owned` and `refusal` in `crates/arca-incus/src/write.rs`; test
`a_read_is_owned_only_for_this_entrys_own_kernel_object` in `crates/arca-incus/tests/write.rs`.

**Rule 6.** Ownership is checked at each write, not only at plan time. A PATCH, a tombstone and a DELETE each start from a fresh
read, and that read must class as the writer's entry's own. An `Owned` value that names another entry is refused.
Source: `UnixIncusWriter::{patch, tombstone, delete, check}` in `crates/arca-incus/src/write.rs`; test
`a_write_that_names_another_entrys_read_is_refused`.

**Rule 7.** A retained object stays owned and visible. Its entry may not create new objects, and nothing in it is written.
Source: `UnixIncusWriter::create` and `read_owned` classify with the writer's retained set; test
`a_retained_object_is_never_owned_by_its_entry`.

## Projects

**Rule 8.** The kernel writes only inside the project family its configuration names. The projects `bench` and `default`, and
every project the other system uses, are refused whatever the configuration says, and a configuration that lists one of them
fails to start. The writer does not hold that list, and this rule is not yet enforced anywhere in this repository. It is the
provider's check, to be made in its `plan` before any write is planned.
Source: `arca_kernel::plugin::Ctx::effect` refuses a project the manifest does not name (`ProjectNotAllowed`). The
configuration check belongs to the provider's `plan`, which is not yet in this repository.

**Rule 9.** A project is named by its own name. A project's path names it alone, and a project whose name differs from its
`project` field is refused before any request.
Source: `write::urls` in `crates/arca-incus/src/write.rs`; test
`an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`.

**Rule 10.** A non-empty project is not deleted. The DELETE is refused, and the project stays.
Source: the fake refuses it (`crates/arca-incus/src/fake.rs`, `delete`); test `a_non_empty_project_is_not_deleted`.

## Creates

**Rule 11.** A create carries this entry's mark and no other owner's key. The body's config must classify as the writer's entry's
own, with no retiring mark. A body whose config lacks the mark, names another entry, carries another system's key, or is
not text is refused before any request.
Source: `UnixIncusWriter::create` in `crates/arca-incus/src/write.rs`; test
`a_create_is_refused_unless_its_config_classes_as_the_entrys_own`.

**Rule 12.** After a create, the writer reads the object back. The read must class as the writer's entry's own, or the create
returns an error.
Source: `UnixIncusWriter::create` ends with `read_owned`; round-trip test `each_kind_round_trips_through_create_patch_tombstone_and_delete`.

## Writes

**Rule 13.** A PATCH carries the ETag of the read it was checked against, as `If-Match`. A PATCH with a stale ETag gets 412
and changes nothing, and the writer returns it as `IncusError::Status { code: 412 }`.
Source: `write::send` sets `If-Match`; `UnixIncusWriter::patch`; fake `patch` in `crates/arca-incus/src/fake.rs`; test
`a_stale_etag_is_412_and_changes_nothing`.

**Rule 14.** Every write that starts an operation is followed by a GET of that operation's `/wait`. A failed operation is an
`IncusError::Status`, carrying the operation's status code and its error text, capped at 512 bytes. The object stays as it was.
Source: `UnixIncusWriter::wait_for` and `IncusError::Status` in `crates/arca-incus/src/read.rs`; test
`a_failed_operation_is_a_status_error_and_the_object_stays`.

**Rule 15.** The writer sends only POST, PATCH and DELETE. It never sends PUT, and it never changes an object wholesale.
Source: the `Method` strings in `crates/arca-incus/src/write.rs`; round-trip test, which asserts the methods the fake receives.

**Rule 16.** A removed config key is sent as the empty string in a PATCH. The fake reads an empty value as unset.
Source: `write::config_patch` and fake `merge`; round-trip test. This encoding is listed under "Assumptions to confirm".

## Removal

**Rule 17.** Each journal record names its effect's class: `A` reverts at once, `B` is destructive and waits for an approved delete
plan, and `C` is an emission that is never reverted. A plugin's manifest declares the class of each Incus kind it may write.
The default classes this document proposes are: project `B`, instance `B`, volume `B`, bucket `B`; network `A`, ACL `A`,
profile `A`, zone `A`, forward `A`. No Incus kind is class `C`. A manifest may declare a stricter class, never a looser one.
Source: `arca_kernel::journal::{EffectClass, removal}` partitions by class; `arca_kernel::plugin::Manifest.effects`.

**Rule 18.** A destructive removal runs in five steps: read the object with its ETag and classify it; PATCH it with `If-Match`
to set `user.arcakernel.retiring` to the plan's digest; read it back and require the same uuid, the entry's mark and the
tombstone; read it again immediately before the delete and require the same; then DELETE. A DELETE sends no `If-Match`, so a
change that lands between the last read and the DELETE is not caught by Incus. That gap is the residual risk of this step. It
is covered by the one-writer rule (only the entry's own writer changes its objects) and by the tombstone, which another
writer would have to clear on purpose.
Source: `UnixIncusWriter::{tombstone, delete}` in `crates/arca-incus/src/write.rs`; tests
`a_delete_is_refused_when_the_tombstone_was_removed_after_it_was_set` and
`a_tombstone_with_a_stale_etag_is_412_and_sets_nothing`.

**Rule 19.** A false precondition skips the object and sends nothing more for it. A read-back that shows another uuid, no
tombstone, or another owner is refused. The caller records drift for that object, and the plan becomes partial.
Source: the refusals in `tombstone` and `delete`; test `a_tombstone_needs_a_uuid_and_a_delete_needs_its_tombstone`.

**Rule 20.** An object needs a uuid before it can be retired or deleted. The uuid is the `uuid` field of the object, or its
`volatile.uuid` config key. Without one, the writer refuses both.
Source: `write::uuid_of`, `UnixIncusWriter::tombstone`, `UnixIncusWriter::delete`.

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

**Rule 24.** The inverse is data from a closed vocabulary, built when the effect applies. A create's inverse is an
`IncusDelete` of the uuid the read-back showed. A patch's inverse is an `IncusRestoreConfig` built from the pre-image read at
plan time, where a key that was absent restores as absent.
Source: `arca_kernel::journal::Inverse`; test `a_patch_records_the_pre_image_as_its_inverse` in `crates/arca-kernel/tests/plugin.rs`.

**Rule 25.** Reads are open to every owner, and a read changes nothing. The watch report may read any class.
Source: `UnixIncusWriter::get_etag` does not classify; the GET-only client `arca_incus::UnixIncus` sends only GET.

**Rule 26.** Names are plain path segments. An object, project, pool, network or listen address is refused unless it is
letters, digits and `-_.:`, is not `.` or `..`, and is at most 253 bytes. Nothing is sent for a refused name.
Source: `write::segment`; test `an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`.

**Rule 27.** A volume and a bucket name their storage pool, and a forward names its network. Every other kind names no parent.
A target that breaks this is refused before any request.
Source: `write::urls`; test `an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`, which includes a volume
named without its pool and an instance that names a pool.

## Out of scope

nftables rules, Caddy routes, tunnels and DNS records carry no Incus mark, so this model does not cover them. Their removal is
a separate rule set for the providers that own them.

## Assumptions to confirm

These hold in the fake and are the basis of the rules above. None has yet been checked against a live Incus, and each check
must run before any live write.

- Incus checks `If-Match` only on PUT and PATCH, so a DELETE has no compare step, and the tombstone is the only one.
- A PATCH merges the `config` map, and an empty value unsets a key (Rule 16).
- An object's uuid is its `uuid` field or its `volatile.uuid` key (Rule 20).
- Incus accepts `config` on a POST for all nine kinds, and the object reads back with that config. The live create probe, one
  per kind, confirms it. The fake's rejection switch only tests how the writer handles a refusal; it says nothing about Incus.
