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
never in the family, so they are refused whatever the configuration says. The one exception is a network and a forward, which
are addressed in `default` and nowhere else, and only when the object carries the entry's mark (Rule 40). The other system's
projects come from the node's configuration, and `ProjectFamily::new` refuses a family that names one of them, so such a
configuration does not start. The Incus provider (`crates/arca-providers/src/incus.rs`) checks the same family in `plan` and
again in `effect`, so an effect made by hand for a project outside the family sends nothing.
Source: `write::ProjectFamily::{new, allows}` and `UnixIncusWriter::{allow_target, create, read_owned, check}` in
`crates/arca-incus/src/write.rs`; tests `a_target_outside_the_kernel_family_is_refused_before_any_request`,
`the_family_is_refused_at_construction_when_it_names_a_reserved_or_droidd_project`,
`networks_and_forwards_are_addressed_in_default_and_no_other_kind_is` and
`a_non_network_create_in_default_is_refused_before_any_request_at_the_writer` in `crates/arca-incus/tests/write.rs`.
`arca_kernel::plugin::Ctx::effect` refuses a project the manifest does not name (`ProjectNotAllowed`), a second check. The
provider's test `an_effect_rechecks_the_family_for_a_plan_the_caller_made_by_hand` is in
`crates/arca-providers/tests/incus_provider.rs`, and `a_non_network_create_in_default_is_still_refused_before_any_request` is
there too.

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
the same uuid. If it does not, the result is `IncusError::AppliedUnconfirmed`, which carries the full target the write named,
the uuid the read-back showed when it showed one, the class the read found when it found one, and the reason. That error means
the write landed. `IncusError::Refused` means that no write was sent.
Source: `UnixIncusWriter::confirm`; tests `a_create_that_lands_and_reads_back_foreign_is_applied_unconfirmed`,
`a_patch_that_reads_back_without_its_change_is_applied_unconfirmed`,
`a_tombstone_that_reads_back_without_the_tombstone_is_applied_unconfirmed`,
`a_tombstone_that_reads_back_on_another_uuid_is_applied_unconfirmed` and
`the_refused_and_unconfirmed_errors_say_whether_a_write_was_sent` and `a_landed_write_names_its_full_target_and_the_uuid_it_read_back`.

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
class. The default classes this document proposes are: project `B`, instance `B`, volume `B`, bucket `B`, network `B`; ACL `A`,
profile `A`, zone `A`, forward `A`. A network is `B` because it is a bridge the default project shares, and an instance attached
to it must be deleted first: a plan deletes the most recent object first, while a class-`A` revert runs before any plan and would
ask Incus to delete a bridge still in use. No Incus kind is class `C`. `Ctx::effect` refuses an effect whose planned class the
manifest does not declare for that kind. Whether a declared class is stricter than the default is not checked.
Source: `arca_kernel::journal::{EffectClass, removal}` partitions by class; `arca_kernel::plugin::Manifest.effects` and the
`NotDeclared` check in `Ctx::effect`.

**Rule 18.** A destructive removal runs in five steps: read the object with its ETag and classify it; PATCH it with `If-Match`
to set `user.arcakernel.retiring` to the plan's digest; read it back and require the entry's mark, the tombstone and, for an
instance or a volume, the same uuid; read it again immediately before the delete and require the same; then DELETE. A DELETE
sends no `If-Match`, so a change that lands between the last read and the DELETE is not caught by Incus. That gap is the
residual risk of this step. It is covered by the one-writer rule (only the entry's own writer changes its objects) and by the
tombstone, which another writer would have to clear on purpose.
The tombstone digest is the plan digest of an approved class-B plan (Rule 37). A class-A revert of a create has no
approval, so its tombstone is the create's own op digest, which the journal already records.
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
and does not revert it. The reconciler settles such an intent from a read-back when it opens (Rule 39); one that cannot be
settled keeps the reconciler from opening, so the plugin stops serving while its objects stay.
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
of the object the read-back showed. For a kind that carries a uuid (an instance or a volume) the inverse names the uuid. For any
other kind it names the object by kind, project, parent and name, with no uuid: a delete by name. The kernel accepts a create's
inverse only when the uuid it names is present exactly for the kinds that carry one (`required_inverse`). A create whose
read-back does not fit its kind is refused by the kernel, and its intent stays open. A patch's inverse is an `IncusRestoreConfig`
built from the pre-image read at plan time, where a key that was absent restores as absent, and a key Rule 32 redacts restores as absent. The pre-image
holds a secret reference where the value is a secret's content (Rule 32). The inverse names the object exactly as the op named it, so a by-name
patch gets a by-name restore that the kernel's check accepts. The uuid the read-back showed guards the write only and is not
carried into the inverse.
Source: `arca_kernel::journal::Inverse`; `required_inverse` in `crates/arca-kernel/src/plugin.rs`; test
`a_patch_records_the_pre_image_as_its_inverse` in `crates/arca-kernel/tests/plugin.rs`, the name-keyed inverse tests in
`crates/arca-kernel/tests/inverse_by_name.rs`, and `a_by_name_patch_of_an_instance_with_a_read_back_uuid_lands_and_its_inverse_names_the_op_object`
in `crates/arca-providers/tests/incus_provider.rs`.

**Rule 25.** Reads are open to every owner, and a read changes nothing. The watch report may read any class.
Source: `UnixIncusWriter::get_etag` does not classify; the GET-only client `arca_incus::UnixIncus` sends only GET.

**Rule 26.** Names are plain path segments. An object, project, pool, network or listen address is refused unless it is
letters, digits and `-_.:`, is not `.` or `..`, and is at most 253 bytes. Nothing is sent for a refused name.
Source: `write::segment`; test `an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`.

**Rule 27.** A volume and a bucket name their storage pool, and a forward names its network. Every other kind names no parent.
A target that breaks this is refused before any request. The parent is carried on `ObjectRef` as `parent`. The key is omitted
from the JSON when it is `None`, so the digest of an object that has no parent does not change.
Source: `write::urls`; `arca_kernel::ownership::ObjectRef`; test `an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`, which includes a volume
named without its pool and an instance that names a pool.

## Build

**Rule 28.** The writer is compiled only with the `write` feature of `arca-incus`, which is off by default. Only `arca-providers`
may enable it, so no other crate, the metrics sampler included, can send a write.
Source: `crates/arca-incus/Cargo.toml`, the `write` feature; `#[cfg(feature = "write")]` on the writer module in
`crates/arca-incus/src/lib.rs`; test `only_arca_providers_enables_the_write_feature` in `crates/arca-incus/tests/boundary.rs`.

## Provider

**Rule 29.** A write to an object that is not an instance or a project lands only in a project that enables the feature its kind
needs: `features.networks.zones` for a network zone; `features.profiles` for a profile; `features.storage.volumes` for a volume;
`features.storage.buckets` for a bucket. The provider reads the project's config and refuses the write when the key is not
`"true"`. Without the key, Incus sends the write to the default project. A network and a forward need no project feature, since
they are addressed in `default`, which holds them (Rule 40). An ACL needs none either: its kernel project keeps it, as the
kernel's ruling stands, which is listed under "Assumptions to confirm". The check runs for a create, a patch and a delete, and it
is made before any write is sent. Reading the project's config is a GET.
Source: `IncusProvider::check_features` in `crates/arca-providers/src/incus.rs`; tests
`a_non_instance_write_is_refused_before_any_write_when_its_project_lacks_the_feature` and
`a_zone_write_is_refused_before_any_write_in_a_project_without_the_zones_key`, in `crates/arca-providers/tests/incus_provider.rs`.

**Rule 30.** Every kernel project is created with `features.networks.zones`, `features.profiles`, `features.storage.volumes` and
`features.storage.buckets` set to `"true"`, with `restricted` set to `"true"`, and with the kernel's mark. `features.networks` is
not set: a kernel project has no networks of its own (Rule 40). A plugin may set a `restricted`, `restricted.*` or `features.*` key
on a project create only to the value the kernel pins it at, with one exception: `restricted.networks.access`, which names the
bridges the project may use and is checked as Rule 41 states. Any other value is refused, and so is a key the kernel does not pin,
whatever its value, so `restricted.containers.privilege` set to `allow`, `features.images` set to `false` and `features.networks`
set to anything are all refused. A project patch may not change any such key, whatever its value, and the refusal is made before
any request. A project is named by its own name.
Source: `IncusProvider::prepared_create` (`PROJECT_KEYS`, `RESTRICTED_KEY`, `NETWORK_ACCESS_KEY`, `guarded_project_key`) and
`check_patch_keys`; tests `a_project_create_carries_the_mark_the_features_restricted_and_the_op_config`,
`a_project_create_refuses_a_restricted_or_features_key_that_loosens_the_pinned_project`,
`a_project_patch_refuses_every_restricted_or_features_key_before_any_request`,
`a_project_create_that_sets_a_kernel_key_another_way_is_refused`, `a_project_create_carries_the_zones_key_pinned_to_true` and
`a_project_create_may_grant_only_bridges_the_entry_holds_in_default`.

**Rule 31.** A kernel network is a bridge in the default project (Rule 40). Its `ipv6.address` is `none`, its `ipv4.address` is an
IPv4 CIDR, and that range overlaps no reserved range. The test is overlap, not containment. The config keys a bridge may hold are an allowlist: the three
that the plans' bridges set, `ipv4.address`, `ipv4.nat` and `ipv6.address`. Any other key is refused. That includes a route
(`ipv4.routes` or `ipv6.routes`, whatever its range), `bridge.external_interfaces`, a `raw.*` key and the DNS keys, which no plan
sets. The kernel's own mark is not a setting, so it is allowed. A network patch meets the same rules for the config it leaves
behind, so a patch cannot move a network into a reserved range or add a refused key.
Source: `check_bridge_create`, `check_bridge_config` and `BRIDGE_KEYS` in `crates/arca-providers/src/incus.rs`; tests
`a_bridge_network_is_refused_without_ipv6_none_outside_ipv4_or_inside_a_reserved_range`,
`a_bridge_key_outside_the_allowlist_or_a_route_external_interface_or_raw_key_is_refused_at_plan` and
`a_bridge_patch_into_a_reserved_range_is_refused_at_plan_and_sends_no_patch`.

**Rule 32.** A config value that starts with `arca-secret:` names a secret file, and the value is never sent as the reference.
The name must match `^[a-z0-9][a-z0-9/-]{0,127}$`. The secrets directory must be a real directory, not a symlink, owned by the
provider's own user, with no group or other permission bits. Each directory a name passes through must meet the same rule. The
file must be a regular file, not a symlink, mode `0600`, owned by the provider's own user, and at most 64 KiB. A reference that
fails a rule is refused at plan. While the secrets directory fails the rule, nothing is read from it and no secret is known. The
plan and the effect each read the file, so a file that changed between them is read again at the effect. The request carries the
content.

The journal's pre-image is chosen key by key. For each changed key that the object holds a value for:

- A value equal to a current secret's content is journalled as that secret's reference, for a plain key and a secret key alike.
- A key the patch sets to a secret reference, whose value is no current secret, is journalled as `None`, a redacted marker. Its
  inverse removes the key. Undoing a rotated secret therefore removes the key; it does not restore the old value. This is the
  cost of the rule. A failed patch of such a key settles as `Unresolved`, since the marker cannot show the old value.
- Any other value, on a plain key (the new value is not a secret reference, and a removal counts as plain), is journalled as it
  is, so its inverse restores it. A value that starts with the reference prefix and matches no current secret is refused at plan
  and again at the effect, since an inverse that restored it would read it as a reference.

Residual, LOW: if a secret has been rotated away, and a plain patch or a removal lands on a key that still holds the old content,
that content is journalled raw, because no current secret matches it. The journal never holds it as a reference in that case.
Source: `IncusProvider::{read_secret, secrets_root_private, secrets_by_value, pre_image_of}`, `secret_reference` and `is_reference`
in `crates/arca-providers/src/incus.rs`; tests `a_secret_reference_is_sent_as_the_file_content_and_the_value_is_never_journalled`,
`a_patch_that_clears_a_key_holding_a_secret_journals_the_reference_as_its_pre_image`,
`a_patch_that_rotates_a_secret_journals_no_value_and_its_inverse_removes_the_key`,
`a_patch_that_sets_a_key_holding_a_current_secret_to_a_plain_value_journals_the_reference`,
`a_plain_patch_journals_a_removal_inverse_that_runs_as_a_patch`,
`a_plain_value_that_a_patch_replaces_is_journalled_as_it_is_and_its_inverse_restores_it`,
`a_landed_plain_patch_can_be_planned_again`,
`a_plain_patch_over_a_reference_shaped_value_that_no_secret_matches_is_refused_and_nothing_is_written`,
`a_secret_reference_that_is_not_a_valid_name_or_has_no_file_is_refused_at_plan`,
`a_secret_file_that_is_a_symlink_or_is_not_mode_0600_is_refused_at_plan` and
`a_secrets_directory_that_is_a_symlink_or_is_readable_by_others_is_refused_at_plan`, all in
`crates/arca-providers/tests/incus_provider.rs`.

**Rule 33.** A write that may have landed is settled by reading the object, and the outcome says which way it went. A refusal
before any write (`IncusError::Refused`) fails the effect. A create is recorded as failed only when Incus answered with a status
error and a GET of its name then answers 404. Any other failure of a create, namely a transport error, an undecodable answer, a
timeout, an oversized answer, or a wait whose answer is not the operation's status (including an HTTP error answer to the wait
request itself), is `EffectRefusal::Unresolved`, and the intent stays open. A patch answered 412 is `Stale` and changes nothing.
A patch that Incus answered with another status error is settled by a read-back: a read that shows the change has landed, a
read that shows the pre-image has not, and anything else is `Unresolved`. A patch whose answer could not be read (an I/O or a
decode error), a timeout and an unconfirmed write are `Unresolved` without a read, since a read taken while an operation still
runs proves nothing.
Source: `IncusProvider::{settle_create, settle_patch}` in `crates/arca-providers/src/incus.rs` and `UnixIncusWriter::wait_on` in
`crates/arca-incus/src/write.rs`; tests `a_create_whose_answer_is_an_error_and_whose_read_back_finds_nothing_is_a_failed_effect`,
`a_create_that_timed_out_stays_open_even_when_the_read_back_finds_nothing`,
`a_create_whose_wait_fails_without_an_answer_stays_unresolved_when_the_name_is_free`,
`a_create_whose_wait_answers_an_error_stays_unresolved_when_the_name_is_free`,
`a_create_whose_answer_is_garbled_stays_unresolved_when_the_name_is_free`,
`a_create_whose_read_back_finds_an_object_is_unresolved_not_failed`,
`a_patch_whose_answer_is_an_io_error_is_unresolved_and_is_not_read_back` and `a_patch_answered_412_is_stale_and_not_unresolved`
in `crates/arca-providers/tests/incus_provider.rs`, and
`a_wait_whose_own_answer_is_an_http_error_is_not_the_operations_status` in `crates/arca-incus/tests/write.rs`.

**Rule 34.** A retirement runs only through `IncusProvider::retire`, under a plan digest: an approved class-B plan (Rule 37), or
the create's op digest for a class-A revert (Rule 18). An instance or a volume is
named by its uuid, so a retirement that names none is refused before any request. The object is read, and it must be the entry's
own with the plan's uuid. The writer tombstones it under its ETag and reads it back. The object is read again, and it is deleted
only while it still shows the tombstone and the uuid. A change that a read shows, or a tombstone the writer refuses, is
`Retirement::Drift`, and no DELETE is sent. A 412 on the tombstone is `Stale`. A tombstone that landed but did not read back is
`Drift`, and the object stays tombstoned. The writer accepts a tombstone with the same plan again, so a later retirement under that
plan can continue. A DELETE the writer refuses is drift; any other failure of the DELETE is `Unresolved`. An object that answers 404
when it is first read is already gone: the retirement is `Deleted`, and no DELETE is sent.
Source: `IncusProvider::retire` in `crates/arca-providers/src/incus.rs`; tests
`retire_tombstones_then_deletes_an_owned_instance_in_order`,
`retire_refuses_an_instance_or_a_volume_named_without_its_uuid_before_any_request`,
`retire_skips_the_delete_as_drift_when_the_delete_read_back_shows_another_uuid`,
`retire_skips_the_delete_as_drift_when_the_tombstone_is_gone_at_delete` and `retire_stops_on_a_stale_etag_and_sends_no_delete`,
all in `crates/arca-providers/tests/incus_provider.rs`.

**Rule 35.** `effect` checks the plan again before it writes, since a plan may be made by hand. The plan's class must be the class
its kind gives. A create's plan has no pre-image. A patch's pre-image is derived again from the object's config as the effect reads
it, and must equal the plan's. A plan that names another class or another pre-image is refused with no write.
Source: `IncusProvider::effect` and `pre_image_of`; test `an_effect_rechecks_the_class_and_the_pre_image_of_a_plan_made_by_hand`.

**Rule 36.** The retained set is a required argument of `IncusProvider::new`, so a provider is never built without one. The
empty set is written out as `RetainedSet::empty`. `RetainedSet` is a shared handle: every clone sees the same entries, and the
provider reads the set at each plan, effect and retirement. An entry retained through any clone refuses that entry's writes from
the next plan on, including after the provider has been boxed. A set whose lock is poisoned refuses every write. It is never read
as empty.
Source: `RetainedSet` and `IncusProvider::{new, writer}` in `crates/arca-providers/src/incus.rs`; tests
`a_write_is_refused_once_the_retained_set_is_updated_through_a_handle_the_boxed_provider_shares` and
`a_retained_entrys_object_is_refused_for_a_patch_and_a_retirement_and_nothing_is_written`.

**Rule 37.** The reconciler is the only caller of `retire` for a class-B object. A class-B delete runs only under a plan the owner
approved: the approval is an `arca-kernel-approve` `delete` over the target `plan-<digest>` with the arguments `{"plan": digest}`,
checked by `ApprovalVerifier`. A pending plan runs only while it is at most `PLAN_TTL_MS` (15 minutes) old. A plan older than that,
or a plan whose live objects are not all named in it (a new step, or a new uuid), needs a new approval over a plan made now. A run
deletes only the live objects the plan names. A partial run keeps its plan, and a retry under the same approval is refused as
replayed. A declined plan's objects are retained: no run and no removal writes them again, and a later document that carries the
entry does not adopt them. An unresolved intent blocks every new effect, including every run. A suspended (disabled) entry keeps
its objects. A class-C emission is never reverted. A removal reads each class-B object before it asks for approval: one that reads absent
is settled without a plan, and a pending plan with no class-B object left is withdrawn.
Source: `arca_reconcile::Reconciler::{reconcile, run_plan, decline}`; tests `removal_leaves_zero_objects_with_the_entry_mark_and_an_empty_journal`,
`a_plan_whose_instance_is_gone_before_the_next_removal_is_withdrawn_and_nothing_is_pending`,
`the_container_delete_waits_for_an_approval_over_the_plan_digest`,
`an_object_changed_between_plan_and_run_survives_is_drift_and_a_retry_without_approval_is_refused`,
`a_plan_older_than_fifteen_minutes_needs_a_new_approval` and `a_declined_plan_retains_its_objects_and_cannot_run` in
`crates/arca-reconcile/tests/`, and the unit tests in `crates/arca-reconcile/src/tests.rs`.

**Rule 38.** A class-A patch's undo (`IncusRestoreConfig`) runs through the plugin's own manifest, as a `Ctx::effect` of the
restore, and then settles the restore and the record it undoes. A removal needs the plugin that applied the entry. Every intent a
plugin's apply makes names that plugin (`EffectRecord.plugin`), and `open` rebuilds the entry-to-plugin map from the live records,
so a removal after a restart finds the plugin. A journal whose entries carry no plugin name fails closed as "plugin not known".
Source: `arca_reconcile::Reconciler::{restore, plan_removal, rebuild_applied_by}`; tests
`a_class_b_plan_after_a_restart_with_the_plugin_missing_fails_closed_and_deletes_nothing` and
`a_kill_inside_a_removal_during_a_patch_restore_is_settled_and_the_removal_completes` in `crates/arca-reconcile/tests/restart.rs`,
and `a_live_class_b_record_without_an_applied_entry_fails_closed` in `crates/arca-reconcile/src/tests.rs`.

**Rule 39.** Before any effect runs, `open` settles each unresolved intent from a GET of its place. A create whose object the entry
owns is adopted, with an `IncusDelete` inverse; a create with nothing there is reverted; any other object is failed and reported
as drift. A patch's entry is held, so nothing is written for it until a document is accepted again. A patch whose object shows its
pre-image is failed; any other value settles the patch with its pre-image as the inverse, which a later removal restores. A delete
whose object is still the entry's is failed, so the next removal retries it; one whose object is gone is reverted, with the create
it undid. An object that is not the entry's, or whose uuid differs from the intent's, is failed and reported, and no delete or
patch is sent. A target outside the family, or in a project that does not enable its kind's feature, is not read. A read that cannot
be answered leaves its intent open, and `open` refuses.
Source: `arca_reconcile::Reconciler::{resolve, open}` in `crates/arca-reconcile/src/restart.rs` and
`arca_providers::incus::IncusProvider::read_back`; tests in `crates/arca-reconcile/tests/restart.rs` and
`a_read_back_in_a_project_without_the_feature_is_refused_and_sends_only_gets` in `crates/arca-providers/tests/incus_provider.rs`.

## Bridges in the default project

**Rule 40.** A network and a forward are addressed in project `default`, which Incus shares with a project that does not enable
networks, and no other kind is. A kernel project does not enable networks, so an instance in it attaches to a bridge in `default`.
Each write to either kind carries the entry's mark: a create names a free name and stamps the mark, and a patch, a tombstone or a
delete reads the object first and refuses it unless the entry owns it. An unmarked bridge therefore costs one read and no write.
A forward is made under its network, since Incus keeps forwards in their network's project, so a forward create also reads its
network and refuses one the entry does not own. Zones and ACLs are not addressed in `default`: a zone stays in its kernel project
under `features.networks.zones`, since a bridge sets no DNS key that would attach one.
Source: `write::{network_home, NETWORK_HOME}`, `ProjectFamily::allows`, `UnixIncusWriter::network_is_own` in
`crates/arca-incus/src/write.rs`, and `IncusProvider::{check_family, prepared_create}` in `crates/arca-providers/src/incus.rs`;
tests `a_marked_bridge_in_default_is_created_patched_and_removed`,
`an_unmarked_bridge_in_default_is_refused_for_patch_and_delete_and_no_write_is_sent`,
`a_non_network_create_in_default_is_still_refused_before_any_request`, `a_forward_is_made_only_on_a_network_the_entry_holds_in_default`
in `crates/arca-providers/tests/incus_provider.rs`, and `a_forward_is_refused_before_any_write_when_its_network_is_not_the_entrys`
in `crates/arca-incus/tests/write.rs`.

**Rule 41.** A project grants its instances the bridges it may use through `restricted.networks.access`, a comma-separated list of
plain names. The grant is set at the project's create only, since a project patch may not change it (Rule 30). Each name must be a
bridge in `default` that carries the entry's mark, read before the create is sent; one name that is not marked refuses the whole
list, and so does an empty or a non-plain name. `restricted` stays `"true"`. A restricted project refuses an instance whose NIC
names a bridge its grant does not list.
Source: `IncusProvider::check_network_access` in `crates/arca-providers/src/incus.rs`; tests
`a_project_create_may_grant_only_bridges_the_entry_holds_in_default` in `crates/arca-providers/tests/incus_provider.rs`, and
`a_nic_on_a_network_the_restricted_project_does_not_list_is_refused_by_incus` in `crates/arca-incus/tests/write.rs`, which runs
against the fake's restriction (`FakeIncus` answers 403 to such a NIC).

## Out of scope

nftables rules, Caddy routes, tunnels and DNS records carry no Incus mark, so this model does not cover them. Their removal is
a separate rule set for the providers that own them.

The retained set is a shared handle that a reconciler updates through a clone of it (Rule 36). A provider built with no set is not
possible.

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
- Incus deletes a project, network, ACL, profile, zone or forward by its name in its project, and a volume or a bucket by its name,
  its pool and its project (Rules 24, 27).
- Incus routes a write sent to a project without its feature key to the default project, so the provider requires the key to be
  `"true"` first (Rule 29).
- Incus serves a network, and a forward, that a project without `features.networks` names from the default project, so a kernel
  project's instance reaches a bridge in `default` (Rule 40). A forward is kept in its network's project (Rule 40).
- Incus keeps an ACL in its own project, and does not route it to `default`, when the project does not enable `features.networks`.
  This is a ruling, and no check here confirms it. If Incus routes ACLs by that key, an ACL create lands in `default`
  carrying the entry's mark, and the read-back in the kernel project does not find it. Confirm on a live host before any ACL write.
- Incus gives a zone its own project under `features.networks.zones`, whether or not `features.networks` is set (Rule 29).
- Incus 7.3 has `restricted.networks.access`, and a restricted project refuses an instance NIC on a network the grant does not
  list (Rule 41). The grant is the only key this rule adds to a kernel project.
- Incus refuses to delete a bridge that an instance still attaches to (Rule 17).
- The probe's image alias resolves in the probe's project. A kernel project does not enable `features.images` (Rule 30), so it
  reads the image store that Incus gives it, and the host imports the alias there before a live probe. The kernel imports nothing.
- Incus accepts `restricted` and the four `features.*` keys on a project create (Rule 30).
- Incus accepts `ipv6.address` set to `none` on a bridge create (Rule 31).
- Incus accepts `ipv4.nat` on a bridge create, and reports a network's config with the keys that were set on it and no others.
  A key Incus adds on read would refuse a patch of that network, which fails closed (Rule 31).
- Incus accepts `features.networks.zones` on a project create and reports it in the project's config (Rule 30).
- Incus answers a GET of an object that is gone with 404, which is how Rule 39 reads an absent object.
