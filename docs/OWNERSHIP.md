# Ownership model

This document states which Incus objects the Automagik Arca kernel may write, how an object's owner is read, and how a
removal proceeds. Each rule is followed by its `Source:`, the code or test in this repository that enforces it. Where a rule rests on
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
change and refuses any result that is not its entry's own. The one exception is the service import's single key (Rule 52).
Source: `arca_kernel::ownership::DROIDD_MARK_PREFIXES`; `UnixIncusWriter::patch` in `crates/arca-incus/src/write.rs`; tests
`droidd_namespace_matching_ignores_ascii_case_so_a_near_miss_only_refuses` (kernel) and
`a_patch_that_would_leave_the_object_owned_by_another_owner_is_refused` (`crates/arca-incus/tests/write.rs`).

## Classes

**Rule 4.** Every object has exactly one class. `Droidd` has a mark from the other system and no kernel mark. `Foreign` has no
mark. `Kernel { entry }` has an entry mark for an entry whose plan was not declined. `Retained { entry, plugin, plan }` has an
entry mark for an entry whose plan was declined. `Contested` has a mark from the other system and a kernel mark together.
Source: `arca_kernel::ownership::{OwnerClass, classify}`; `crates/arca-kernel/tests/ownership.rs`.

**Rule 5.** Every class may be read, and only the writer's own entry may write an object of its own class. Foreign, droidd,
contested and retained objects are written by nobody in this kernel, with one exception: the service import sets or clears
its one key on an instance the configuration allowlists (Rule 52), and nothing else.
Source: `arca_kernel::ownership::kernel_may_write` is true only for `Kernel { entry }` with the same entry;
`UnixIncusWriter::read_owned` and `refusal_reason` in `crates/arca-incus/src/write.rs`; test
`a_read_is_owned_only_for_this_entrys_own_kernel_object` in `crates/arca-incus/tests/write.rs`.

**Rule 6.** Ownership is checked at each write. A config change and a tombstone check the `Owned` value's config against the
writer's entry, and they send that value's ETag as `If-Match` (a PATCH, or for a project a PUT of its whole config, Rule 43), so
Incus refuses the write with 412 when the object has changed since the read. A DELETE alone starts from a fresh read, which must
class as the writer's entry's own and show the tombstone. An `Owned` value that names another entry is refused.
Source: `UnixIncusWriter::{patch, tombstone, delete, check}` in `crates/arca-incus/src/write.rs`; tests
`a_write_that_names_another_entrys_read_is_refused` and `a_stale_etag_is_412_and_changes_nothing`.

**Rule 7.** A retained object stays owned and visible. Its entry may not create new objects, and nothing in it is written.
Source: `UnixIncusWriter::create` and `read_owned` classify with the writer's retained set; test
`a_retained_object_is_never_owned_by_its_entry`.

## Projects

**Rule 8.** The kernel writes only inside the project family its configuration names. `ProjectFamily` holds those names and
nothing else. A target outside the family is refused before any request is sent, a read included. `bench` and `default` are
never in the family, so they are refused whatever the configuration says. The one exception is a network, a forward and an ACL,
which are addressed in `default` and nowhere else, and only when the object carries the entry's mark (Rule 40). The other system's
projects come from the node's configuration, and `ProjectFamily::new` refuses a family that names one of them, so such a
configuration does not start. The Incus provider (`crates/arca-providers/src/incus.rs`) checks the same family in `plan` and
again in `effect`, so an effect made by hand for a project outside the family sends nothing.
Source: `write::ProjectFamily::{new, allows}` and `UnixIncusWriter::{allow_target, create, read_owned, check}` in
`crates/arca-incus/src/write.rs`; tests `a_target_outside_the_kernel_family_is_refused_before_any_request`,
`the_family_is_refused_at_construction_when_it_names_a_reserved_or_droidd_project`,
`networks_forwards_and_acls_are_addressed_in_default_and_no_other_kind_is` and
`a_non_network_create_in_default_is_refused_before_any_request_at_the_writer` in `crates/arca-incus/tests/write.rs`.
`arca_kernel::plugin::Ctx::effect` refuses a project the manifest does not name (`ProjectNotAllowed`), a second check. The
provider's test `an_effect_rechecks_the_family_for_a_plan_the_caller_made_by_hand` is in
`crates/arca-providers/tests/incus_provider.rs`, and `a_non_network_create_in_default_is_still_refused_before_any_request` is
there too.
The one other exception is the adoption writer (Rule 45): for one container that `kernel.adopt` lists, it writes that instance
in `default`, creates and deletes its adoption project, and moves the instance between the two, through a closed table of
steps. It is a
separate writer with its own family of one project; `ProjectFamily` and every other path are unchanged, so `default` stays out
of every family.

**Rule 9.** A project is named by its own name. A project's path names it alone, and a project whose name differs from its
`project` field is refused before any request.
Source: `write::urls` in `crates/arca-incus/src/write.rs`; test
`an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write`.

**Rule 10.** A non-empty project is not deleted: Incus refuses the DELETE, and the project stays. The writer sends the DELETE
and returns Incus's answer as `IncusError::Status`. It does not count the project's objects first. The images in a kernel project's
own store are the one exception: they are the project's by containment, and the project's retirement deletes them before the
project (Rule 42).
Source: `UnixIncusWriter::delete` returns the status error from `mutate`. The refusal is Incus's, and it is listed under
"Assumptions to confirm". Test `a_non_empty_project_is_not_deleted` models it on the fake (`crates/arca-incus/src/fake.rs`,
`delete`).

## Creates

**Rule 11.** A create carries this entry's mark and no other owner's key, and it names an image source only. The body's config
must classify as the writer's entry's own, with no retiring mark. A `source`, when the body has one, must be an image named by
alias, fingerprint or properties. A copy, a migration, a refresh and a remote are refused, and so is a server, with one exception:
the image server `https://images.linuxcontainers.org` named with the protocol `simplestreams`, exactly and together, and with no
secret or certificate. Incus then pulls the image into the store the instance's project reads, which for a kernel project is its
own (Rules 30, 42), so a probe makes its instance with no copy by hand. Any other server, any other protocol, and either one without
the other are refused. The name must be free: the object's GET must answer 404 before the POST is sent. Every refusal happens
before any write. An instance create starts the instance by default; a body that sets `start: false` creates it stopped.
Source: `UnixIncusWriter::create`, `write::check_source` and `write::{IMAGE_SERVER, IMAGE_PROTOCOL}` in
`crates/arca-incus/src/write.rs`; tests `a_create_is_refused_unless_its_config_classes_as_the_entrys_own`,
`a_create_takes_only_an_image_source_and_sends_nothing_else`, `a_create_may_name_the_one_image_server_and_no_other`,
`a_create_whose_name_is_taken_is_refused_and_nothing_is_written`,
`instance_create_starts_by_default_and_respects_explicit_false` and
`a_delete_stops_a_running_instance_before_the_deleting_request` in `crates/arca-incus/tests/write.rs`.

**Rule 12.** A write that Incus accepted is confirmed by a read-back. After a create, a config change or a tombstone, the writer
reads the object again. The read must class as the writer's entry's own, and it must show the write: the change, or the tombstone
on the same uuid, and after a project's PUT every key the PUT sent, with its value (Rule 43). If it does not, the result is
`IncusError::AppliedUnconfirmed`, which carries the full target the write named, the uuid the read-back showed when it showed
one, the class the read found when it found one, and the reason. That error means the write landed. `IncusError::Refused` means
that no write was sent.
Source: `UnixIncusWriter::confirm`; tests `a_create_that_lands_and_reads_back_foreign_is_applied_unconfirmed`,
`a_patch_that_reads_back_without_its_change_is_applied_unconfirmed`,
`a_tombstone_that_reads_back_without_the_tombstone_is_applied_unconfirmed`,
`a_tombstone_that_reads_back_on_another_uuid_is_applied_unconfirmed`,
`a_project_put_that_reads_back_without_a_key_it_sent_is_applied_unconfirmed`,
`the_refused_and_unconfirmed_errors_say_whether_a_write_was_sent` and `a_landed_write_names_its_full_target_and_the_uuid_it_read_back`.

## Writes

**Rule 13.** A PATCH, and a project's PUT, carries the ETag of the read it was checked against, as `If-Match`. A write with a
stale ETag gets 412 and changes nothing, and the writer returns it as `IncusError::Status { code: 412 }`.
Source: `write::send_within` sets `If-Match`; `UnixIncusWriter::{patch, send_config}`; fake `patch` and `put` in
`crates/arca-incus/src/fake.rs`; tests `a_stale_etag_is_412_and_changes_nothing` and
`a_project_put_under_a_stale_etag_is_412_and_changes_nothing`.

**Rule 14.** A write that Incus answers with an operation is settled by waiting on it. The writer GETs
`/1.0/operations/<id>/wait?timeout=5` until the operation's `status_code` is 200 with the status `Success`. It waits again while
the code is 1xx, which means still running. A 4xx or 5xx is the operation's failure, returned as `IncusError::Status` with its
`err` text, capped at 512 bytes. Any other code is a failure too. When the writer's overall deadline passes first, the result is
`IncusError::Timeout`. A write whose answer is sync, or names no operation, is done at once. A failed operation leaves the object
as it was. Against the fake the object stays, and a live probe must confirm it.
Source: `UnixIncusWriter::{wait_for, wait_on}` and `IncusError::Status` in `crates/arca-incus/src/read.rs`; tests
`the_operation_wait_follows_running_and_accepts_only_status_200_success`, `a_sync_answer_is_done_at_once_and_an_async_one_is_waited_on`,
`an_answer_with_an_empty_or_absent_operation_is_done_without_a_wait` and `a_failed_operation_is_a_status_error_and_the_object_stays`.

**Rule 15.** The writer changes objects only with POST, PATCH, PUT and DELETE; every other request it sends is a GET, for a
read-back or the instance state read before a stop. A POST creates, a PATCH sets or removes config keys of any kind
but a project, or replaces one ACL ingress or egress array (Rule 47), and a DELETE removes. A PUT sends a project's config change
or tombstone as its whole config with the change applied, under the ETag of the read the change was checked against (Rule 43).
A PUT also force-stops a running instance through its state endpoint before DELETE (Rule 18). No other object is written
wholesale, and no PATCH is sent to a project.
Source: the methods passed to `mutate` in `crates/arca-incus/src/write.rs` (`POST` in `create`, `PATCH` and `PUT` in
`send_config`, which `patch` and `tombstone` call, `PUT` in `delete`, `DELETE` in `delete` and `clear_images`, and `GET` in
`delete`'s state read), the `send("GET", ...)` read-backs in `write.rs`, and `UnixIncus` in
`crates/arca-incus/src/read.rs`, which sends only GET; the round-trip test `each_kind_round_trips_through_create_patch_tombstone_and_delete`,
which allows a PUT only for a project or an instance's stop and asserts that a PATCH never reaches a project.

**Rule 16.** A removed config key is sent as the empty string in a PATCH. The fake reads an empty value as unset. A project's PUT
leaves a removed key out of the whole config it sends.
Source: `write::config_patch` and fake `merge`; `UnixIncusWriter::send_config`; round-trip test. This encoding is listed under
"Assumptions to confirm".

## Removal

**Rule 17.** Each journal record names its effect's class: `A` reverts at once, `B` is destructive and waits for an approved
delete plan, and `C` is an emission that is never reverted. A plugin's manifest declares each Incus kind it may write with its
class. The default classes this document proposes are: project `B`, instance `B`, volume `B`, bucket `B`, network `B`; ACL `A`,
profile `A`, zone `A`, forward `A`. A network is `B` because it is a bridge the default project shares, and an instance attached
to it must be deleted first: a plan deletes the most recent object first, while a class-`A` revert runs before any plan and would
ask Incus to delete a bridge still in use. An ACL stays `A` although it is addressed in `default` (Rule 40); an ACL that a
NIC of the entry's own instance names is retired after that instance, not at once (Rule 66). No Incus kind is class `C`. `Ctx::effect` refuses an effect whose planned class the
manifest does not declare for that kind. Whether a declared class is stricter than the default is not checked.
Source: `arca_kernel::journal::{EffectClass, removal}` partitions by class; `arca_kernel::plugin::Manifest.effects` and the
`NotDeclared` check in `Ctx::effect`.

**Rule 18.** A destructive removal runs in five steps: read the object with its ETag and classify it; set
`user.arcakernel.retiring` to the plan's digest under `If-Match`, with a PATCH, or for a project a PUT of its whole config
(Rule 43); read it back and require the entry's mark, the tombstone and, for an instance, the same uuid; read it again
immediately before the delete and require the same; then DELETE. Before the DELETE, a running instance is stopped with
`force: true`, and the stop's operation is waited on, because Incus refuses to delete a running instance. A failed stop prevents
the DELETE and leaves the instance for a later reconciliation to retry. A stopped instance receives no stop request, and no
other kind is stopped. A DELETE sends no `If-Match`, so a change that lands between
the last read and the DELETE is not caught by Incus. That gap is the residual risk of this step. It is covered by the one-writer
rule (only the entry's own writer changes its objects) and by the tombstone, which another writer would have to clear on purpose.
The tombstone digest is the plan digest of an approved class-B plan (Rule 37). A class-A revert of a create has no
approval, so its tombstone is the create's own op digest, which the journal already records.
Source: `UnixIncusWriter::{tombstone, delete}` in `crates/arca-incus/src/write.rs`; tests
`a_delete_is_refused_when_the_tombstone_was_removed_after_it_was_set`,
`a_tombstone_with_a_stale_etag_is_412_and_sets_nothing`,
`a_delete_stops_a_running_instance_before_the_deleting_request` and
`a_delete_of_a_stopped_instance_sends_no_stop_request` in `crates/arca-incus/tests/write.rs`.

**Rule 19.** A false precondition is refused before any write, and nothing more is sent for that object. A read-back after a
write that shows the wrong uuid, no tombstone, another owner, or not the change is `AppliedUnconfirmed` (Rule 12). The caller
records drift for that object, and the plan becomes partial.
Source: the refusals in `tombstone` and `delete`, and the read-back in `confirm`; test
`a_tombstone_needs_a_uuid_where_incus_gives_one_and_a_delete_needs_its_tombstone`.

**Rule 20.** Only the kind Incus gives a uuid carries one: an instance, through its `volatile.uuid` config key. An instance
without that key cannot be retired or deleted. Every other kind is identified by its kind, project, parent and name, with its
ETag, and that includes a custom volume: Incus 7.3 gives a volume no `volatile.uuid` and no uuid field, so a volume's create, its
inverse and its retirement name it by its pool and name. Such an object's tombstone is set under its ETag, and its delete follows
a fresh read that shows the tombstone. A target that names a uuid reads only the object with that uuid, and a mismatch is refused
before any write. A journal record that an earlier kernel left open for a volume, because it refused a create answered without a
uuid, is settled when the reconciler opens: the volume reads back owned and without a uuid, so it is adopted with a delete by name.
Source: `arca_kernel::ownership::IncusKind::has_uuid`, `write::uuid_of`, `UnixIncusWriter::{classed, tombstone, delete}`; tests
`only_an_instance_has_a_uuid` (`crates/arca-kernel/tests/ownership.rs`),
`a_volume_answered_without_a_uuid_takes_a_delete_by_name_inverse` (`crates/arca-kernel/tests/inverse_by_name.rs`),
`a_tombstone_needs_a_uuid_where_incus_gives_one_and_a_delete_needs_its_tombstone`,
`a_target_that_names_a_uuid_reads_only_the_object_with_that_uuid` and `each_kind_round_trips_through_create_patch_tombstone_and_delete`
(`crates/arca-incus/tests/write.rs`), `a_volume_without_a_uuid_is_made_and_retired_by_its_pool_and_name`
(`crates/arca-providers/tests/incus_provider.rs`), and `the_journal_an_earlier_probe_left_opens_with_its_volume_adopted_by_name`
(`crates/arca-reconcile/tests/probe.rs`, over the journal fixture `crates/arca-reconcile/tests/fixtures/journal-volume-without-uuid.jsonl`).

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
of the object the read-back showed. For the kind that carries a uuid (an instance) the inverse names the uuid. For any
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
`"true"`. Without the key, Incus sends the write to the default project. A network, a forward and an ACL need no project feature,
since they are addressed in `default`, which holds them (Rule 40). The check runs for a create, a patch and a delete, and it is
made before any write is sent. Reading the project's config is a GET.
Source: `IncusProvider::check_features` in `crates/arca-providers/src/incus.rs`; tests
`a_non_instance_write_is_refused_before_any_write_when_its_project_lacks_the_feature` and
`a_zone_write_is_refused_before_any_write_in_a_project_without_the_zones_key`, in `crates/arca-providers/tests/incus_provider.rs`.

**Rule 30.** Every kernel project is created with `features.images`, `features.networks.zones`, `features.profiles`,
`features.storage.volumes` and `features.storage.buckets` set to `"true"`, with `restricted` set to `"true"`, and with the kernel's
mark. `features.networks` is not set: a kernel project has no networks of its own (Rule 40). `features.images` gives the project its
own image store, so an image an instance pulls lands inside the project and never in the default project's store, and the
project's retirement deletes it (Rule 42). Incus 7.3 sets it to `"true"` by default; the kernel sets it so that nothing depends on
that default. A plugin may set a `restricted`, `restricted.*` or `features.*` key
on a project create only to the value the kernel pins it at, with one exception: `restricted.networks.access`, which names the
bridges the project may use and is checked as Rule 41 states. Any other value is refused, and so is a key the kernel does not pin,
whatever its value, so `restricted.containers.privilege` set to `allow`, `features.images` set to `false` and `features.networks`
set to anything are all refused. A project patch may not change any such key, whatever its value, and the refusal is made before
any request; the one exception is the widening of the service project by the entry that owns it (Rule 55). A project is named by its own name. Whatever the provider allows, no request can PATCH a project: `exchange`, the one
function every request of the read client and the writer passes through, refuses a PATCH whose path is `/1.0/projects` or under
it, percent-decoded, with `IncusError::Refused("a project is never sent a PATCH")`, before it connects, so no byte reaches the
socket (Rule 43).
Source: `IncusProvider::prepared_create` (`PROJECT_KEYS`, `RESTRICTED_KEY`, `NETWORK_ACCESS_KEY`, `guarded_project_key`) and
`check_patch_keys`; the guard `read::guard_request`, which `read::exchange` runs first, in `crates/arca-incus/src/read.rs`, with
its unit tests `a_patch_to_a_project_path_is_refused_by_exchange_before_any_byte_is_sent` (the encoded, trailing-slash, query,
dot-segment and letter-case variants), `a_request_whose_line_cannot_be_read_is_refused_before_any_byte_is_sent` and
`the_guard_passes_every_other_method_and_every_patch_outside_the_projects_path`, and
`no_public_writer_method_sends_a_patch_to_a_project` in `crates/arca-incus/tests/write.rs`; tests
`a_project_create_carries_the_mark_the_features_restricted_and_the_op_config`,
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
The name must match `^[a-z0-9][a-z0-9/-]{0,127}$`. A name with more than one path segment belongs to the entry named by its
first segment when that segment parses as an `EntryId` (`^[a-z][a-z0-9-]{0,62}$`); every other name is shared. Only that entry
may use an owned secret; every entry may use a shared secret. A reference naming another entry's secret is refused at plan,
before any filesystem access for the reference. The secrets directory must be a real directory, not a symlink, owned by the
provider's own user, with no group or other permission bits. Each directory a name passes through must meet the same rule. The
file must be a regular file, not a symlink, mode `0600`, owned by the provider's own user, and at most 64 KiB. A reference that
fails a rule is refused at plan. While the secrets directory fails the rule, nothing is read from it and no secret is known. The
plan and the effect each read the file, so a file that changed between them is read again at the effect. The request carries the
content.

The journal's pre-image is chosen key by key. For each changed key that the object holds a value for:

- A value equal to a current shared or own secret's content is journalled as that secret's reference, for a plain key and a
  secret key alike.
- A key the patch sets to a secret reference, whose value is no current secret, is journalled as `None`, a redacted marker. Its
  inverse removes the key. Undoing a rotated secret therefore removes the key; it does not restore the old value. This is the
  cost of the rule. A failed patch of such a key settles as `Unresolved`, since the marker cannot show the old value.
- Any other value, on a plain key (the new value is not a secret reference, and a removal counts as plain), is journalled as it
  is, so its inverse restores it. A value that starts with the reference prefix and matches no current secret is refused at plan
  and again at the effect, since an inverse that restored it would read it as a reference.

Residual, LOW: if a secret has been rotated away, and a plain patch or a removal lands on a key that still holds the old content,
that content is journalled raw, because no current secret matches it. The journal never holds it as a reference in that case.
A value equal only to another entry's secret likewise matches no usable current secret and is journalled raw on a plain patch
or removal.
Source: `IncusProvider::{read_secret, read_secret_file, secrets_root_private, secrets_by_value, pre_image_of}`, `secret_owner`,
`may_use_secret`, `secret_reference` and `is_reference`
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
the create's op digest for a class-A revert (Rule 18). An instance is named by its
uuid, so a retirement that names none is refused before any request; a volume has none and is named by its pool and name. The object is read, and it must be the entry's
own with the plan's uuid. The writer tombstones it under its ETag and reads it back. The object is read again, and it is deleted
only while it still shows the tombstone and the uuid. A change that a read shows, or a tombstone the writer refuses, is
`Retirement::Drift`, and no DELETE is sent. A 412 on the tombstone is `Stale`. A tombstone Incus answers with another 4xx is read
back: an object that shows no tombstone of the plan, or is gone, was not tombstoned, so the retirement is `Drift`, a settled failure
that leaves no intent open, and a later removal tries again; one that shows the tombstone, or cannot be read, is `Unresolved`, and so
is a tombstone answered with a 5xx. A tombstone that landed but did not read back is `Drift`, and the object stays tombstoned. The
writer accepts a tombstone with the same plan again, so a later retirement under that plan can continue. A DELETE the writer
refuses is drift; any other failure of the DELETE is `Unresolved`. An object that answers 404 when it is first read is already
gone: the retirement is `Deleted`, and no DELETE is sent.
Source: `IncusProvider::retire` in `crates/arca-providers/src/incus.rs`; tests
`retire_tombstones_then_deletes_an_owned_instance_in_order`,
`retire_refuses_an_instance_named_without_its_uuid_before_any_request`,
`a_volume_without_a_uuid_is_made_and_retired_by_its_pool_and_name`,
`retire_skips_the_delete_as_drift_when_the_delete_read_back_shows_another_uuid`,
`retire_skips_the_delete_as_drift_when_the_tombstone_is_gone_at_delete`, `retire_stops_on_a_stale_etag_and_sends_no_delete`,
`a_tombstone_refused_with_a_4xx_that_reads_back_without_it_is_drift_and_no_delete_is_sent`,
`a_project_tombstone_incus_refuses_with_400_is_drift_and_not_an_open_outcome` and
`a_tombstone_that_reads_back_after_a_4xx_or_is_answered_5xx_stays_unresolved`, all in
`crates/arca-providers/tests/incus_provider.rs`.

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
patch is sent. A target outside the family, or in a project that does not enable its kind's feature, is not read. A target that a
running operation names is read only once the operation ends (Rule 44). A read that cannot be answered leaves its intent open, and
`open` refuses.
Source: `arca_reconcile::Reconciler::{resolve, open}` in `crates/arca-reconcile/src/restart.rs` and
`arca_providers::incus::IncusProvider::read_back`; tests in `crates/arca-reconcile/tests/restart.rs` and
`a_read_back_in_a_project_without_the_feature_is_refused_and_sends_only_gets` in `crates/arca-providers/tests/incus_provider.rs`.

## Bridges in the default project

**Rule 40.** A network, a forward and an ACL are addressed in project `default`, which Incus shares with a project that does not
enable networks, and no other kind is. A kernel project does not enable networks, so an instance in it attaches to a bridge in
`default`, and Incus 7.3 keeps the project's ACLs in `default` too: an ACL created with `?project=<kernel project>` lands in
`default`, and a read sent to the kernel project is answered from there. Each write to any of the three kinds carries the entry's
mark: a create names a free name and stamps the mark, and a patch, a tombstone or a delete reads the object first and refuses it
unless the entry owns it. An unmarked bridge or ACL therefore costs one read and no write. A forward is made under its network,
since Incus keeps forwards in their network's project, so a forward create also reads its network and refuses one the entry does
not own. A zone is not addressed in `default`: it stays in its kernel project under `features.networks.zones`, since a bridge sets
no DNS key that would attach one. A journal record an earlier kernel wrote can name an ACL in its kernel project; the provider
reads and retires that ACL in `default` when the project's own config does not set `features.networks` to `"true"` (or the
project is gone), and keeps the record's address, which the family refuses, when it does. The read-back still names the object
as the record names it, so the journal accepts the outcome.
Source: `write::{network_home, NETWORK_HOME}`, `ProjectFamily::allows`, `UnixIncusWriter::network_is_own` in
`crates/arca-incus/src/write.rs`, and `IncusProvider::{check_family, prepared_create}` in `crates/arca-providers/src/incus.rs`;
tests `a_marked_bridge_in_default_is_created_patched_and_removed`,
`an_unmarked_bridge_in_default_is_refused_for_patch_and_delete_and_no_write_is_sent`,
`a_non_network_create_in_default_is_still_refused_before_any_request`, `a_forward_is_made_only_on_a_network_the_entry_holds_in_default`,
`an_acl_is_made_marked_in_default_and_refused_in_a_kernel_project_before_any_request` and
`an_acl_an_earlier_kernel_named_in_its_kernel_project_is_read_and_retired_in_default` in `crates/arca-providers/tests/incus_provider.rs`
(`IncusProvider::served_at`), and `a_forward_is_refused_before_any_write_when_its_network_is_not_the_entrys`,
`an_acl_is_created_marked_in_default_and_refused_in_a_kernel_project_before_any_request` and
`an_acl_sent_to_a_project_without_the_networks_key_lands_in_default` in `crates/arca-incus/tests/write.rs`.

**Rule 41.** A project grants its instances the bridges it may use through `restricted.networks.access`, a comma-separated list of
plain names. The grant is set at the project's create only, since a project patch may not change it (Rule 30); the service
project's owner may drop it when its node gains host bridges, and never rewrite it (Rule 55). Each name must be a
bridge in `default` that carries the entry's mark, read before the create is sent; one name that is not marked refuses the whole
list, and so does an empty or a non-plain name. `restricted` stays `"true"`. A restricted project refuses an instance whose NIC
names a bridge its grant does not list. The one exception is an adoption project's grant (Rule 45), which names `eth0`'s
bridge alone: an unmarked bridge `kernel.attachNetworks` lists, or one the owner approved in the plan's allowances, and nothing
writes it. Every other path still refuses an unmarked bridge.
Source: `IncusProvider::check_network_access` in `crates/arca-providers/src/incus.rs`; tests
`a_project_create_may_grant_only_bridges_the_entry_holds_in_default` in `crates/arca-providers/tests/incus_provider.rs`, and
`a_nic_on_a_network_the_restricted_project_does_not_list_is_refused_by_incus` in `crates/arca-incus/tests/write.rs`, which runs
against the fake's restriction (`FakeIncus` answers 403 to such a NIC). The adoption grant is `adoption_project_body` in
`crates/arca-incus/src/adopt.rs`, tested by `the_adoption_project_grants_only_eth0s_network_and_allows_snapshots_and_backups`
in `crates/arca-incus/tests/adopt.rs`.

## Images in a kernel project

**Rule 42.** An image carries no mark. An image in a kernel project's own store is the project's by containment: only a write into
the project puts one there (an instance the kernel makes from the image server, or a copy by hand), and it goes with the project.
A project's DELETE is preceded, in the writer, by the DELETE of each image in its own store: the project must be the entry's own and
carry its tombstone (Rule 18), its own config must set `features.images` to `"true"`, the listing is
`/1.0/images?project=<project>&recursion=1`, each image must name a plain fingerprint, and an image whose `project` names another
project refuses the whole step before any image is deleted. A project whose config does not set `features.images` reads the
default project's store, none of whose images is the project's, so nothing is listed or deleted for it. The project is then read
again and deleted as Rule 18 states. A failed image DELETE leaves the project tombstoned, and a retirement under the same plan
continues it.
Source: `UnixIncusWriter::{delete, clear_images}` in `crates/arca-incus/src/write.rs`, and `PROJECT_KEYS` in
`crates/arca-providers/src/incus.rs`; tests `a_retiring_project_deletes_the_images_in_its_own_store_and_then_itself`,
`a_project_that_reads_the_default_image_store_deletes_no_image` and
`an_instance_from_the_image_server_pulls_its_image_into_the_store_its_project_reads` in `crates/arca-incus/tests/write.rs`, which run
against the fake's image stores, and `an_uninterrupted_probe_creates_and_removes_every_object` and
`the_journal_an_earlier_probe_left_is_removed_to_nothing_and_a_fresh_probe_then_runs` in `crates/arca-reconcile/tests/probe.rs`.

## Project config writes

**Rule 43.** A project's config is written with a PUT, never with a PATCH. Incus 7.3's project PATCH replaces the whole config
with the keys it names: `projectPatch` in `cmd/incusd/api_project.go` copies the project's old keys into the raw request map, not
into the request it applies, so `projectChange` sees only the keys sent. The same code is still in Incus 7.6.0 and on Incus's main
branch, so the rule holds after the upgrade to 7.6. On a project in use, a PATCH of the tombstone alone reads
as turning every feature off, and Incus refuses it with 400, which a live host showed. On a project used only by its default profile
the same PATCH would land and erase the kernel's mark, `restricted` and every feature, which would leave a project the kernel can
never manage again. So every
project config write, whether a plugin's patch, its restore or the tombstone, is the read the classifier checked (a GET of the
whole project) followed by a PUT of the project's whole config with the change applied and of its description as read, under
`If-Match` set to that GET's ETag. Incus checks a PUT's `If-Match` against the project's description and config, the pair its GET's
ETag covers, so a project that changed since the read gets 412 and changes nothing (Rule 13). The read-back must hold every key the
PUT sent (Rule 12). The writer sends a PUT for nothing else, and it sends no PATCH to a project; the guard in `exchange` refuses one
from any client before a byte is sent (Rule 30). A profile's, an ACL's, a volume's
and a network's PATCH merge on Incus 7.3, so they stay PATCHes. The fake writes a project's config as Incus 7.3 does: a PATCH
replaces it, a PUT replaces it and the description, and a write that would turn off a feature of a project that holds an object is
refused with 400, so any kernel path that sent a project PATCH would fail the tests. A journal an earlier kernel left with a
project's delete open, because its tombstone PATCH was refused, opens with that delete settled as failed (the project is still
there and still the entry's, Rule 39), and the next removal deletes the project, the images in its own store (Rule 42) and the
bridge it granted.
Source: `UnixIncusWriter::{patch, tombstone, send_config}` and `write::holds_sent` in `crates/arca-incus/src/write.rs`; fake
`write_project` in `crates/arca-incus/src/fake.rs`; tests `a_project_patch_on_the_fake_replaces_the_whole_config_as_incus_7_3_does`,
`a_project_write_that_drops_a_feature_of_a_project_in_use_is_refused_by_the_fake`,
`a_project_put_on_the_fake_replaces_the_config_and_the_description_under_its_etag`,
`a_project_patch_and_tombstone_are_each_a_put_of_the_whole_config_under_the_reads_etag`,
`a_project_put_under_a_stale_etag_is_412_and_changes_nothing`,
`a_project_put_that_reads_back_without_a_key_it_sent_is_applied_unconfirmed` and
`each_kind_round_trips_through_create_patch_tombstone_and_delete` in `crates/arca-incus/tests/write.rs`;
`a_project_patch_and_its_restore_are_puts_that_keep_every_other_key` and
`retire_of_a_project_in_use_tombstones_it_with_a_put_then_deletes_its_image_and_itself` in
`crates/arca-providers/tests/incus_provider.rs`; and in `crates/arca-reconcile/tests/probe.rs`, where the full probe runs and the
removals of an earlier journal assert that no project PATCH was sent, and
`the_journal_a_refused_project_tombstone_left_opens_and_its_removal_deletes_the_project_its_image_and_the_bridge` runs over the
journal fixture `crates/arca-reconcile/tests/fixtures/journal-project-tombstone-open.jsonl`.

## Running operations

**Rule 44.** An intent is never settled while an operation that Incus still runs, and that may still create, change or delete
its target, names the target. Before a read-back reads the object, it lists `/1.0/operations?all-projects=true&recursion=1` and
keeps each operation whose `status_code` is 1xx: pending (105), running (103) or cancelling (104), and any other code that has
not ended. Two read-backs do this: the one that settles an intent after a restart (Rule 39), and the one a removal makes of each
class-B object before it asks the owner, which settles an object already gone instead of asking about it (`settle_gone`). It
then leaves out each operation that cannot write the target. One is an operation whose `description` is exactly
`Executing command` (Incus's `CommandExec`, of class `websocket`, or `task` when no websocket is asked for) or `Showing console`
(`ConsoleShow`, class `websocket`), as Incus's `internal/server/db/operationtype/operation_type.go` names them: such a session
only attaches to a running instance, and an interactive one stays at 103 for as long as it is open. The other is an operation of
`class` `token`, for which Incus refuses a run hook. Every other operation is kept, a `websocket` one included, since Incus runs
a push migration's create (`Creating instance`) as a websocket operation, and so is one with no class or description, or a
description in any other spelling. An operation names the target when one of its `resources` URLs is the target's path, or a
path under it such as a snapshot, percent-decoded, in the target's project: a URL without a `project` query names `default`, as
Incus renders a resource in `default` without one. A network, a forward and an ACL are compared in `default` (Rule 40), so their
URL's project is not compared, and a project is named by its path alone. The read-back waits on each kept operation that names
the target with `/1.0/operations/<id>/wait?timeout=<s>`, at most 5 s per wait, and lists the operations again after a pause of
100 ms, whether the wait said the operation ended or still runs, so a listing that still shows an operation its wait says has
ended is not spun on. The whole wait is bounded by the provider's operation deadline (`OPERATION_DEADLINE`, 600 s, unless the
provider is given another). Then it reads the object. An operation still running at the deadline, an operations listing that
cannot be read and a wait that cannot be answered are each `EffectRefusal::Unresolved`: the object is not read, the intent stays
open, and `open` refuses with `KernelError::Unresolved`. So a create whose operation still runs is not read as absent and
reverted, which would leave the object it then makes marked and unknown to the journal, and a create that shows its object while
it runs and then fails is not journalled as made. An operation on any other object holds nothing. Only GETs are sent.
`running_on` is the same check, sessions included, for any other caller that must not act on an object while an operation names
it, such as the adoption writer, whose stop would end an open session; `writing_on` is `running_on` without the sessions and the
token operations, and is the read-backs' check.
Source: `read::{running_operations, operation_names, running_on, writing_on, wait_operation, OPERATIONS_PATH, RunningOp,
RunningOp::may_write, SESSION_DESCRIPTIONS, TOKEN_CLASS}` in `crates/arca-incus/src/read.rs`; `IncusProvider::{read_back,
settle_operations}` in `crates/arca-providers/src/incus.rs`; `Reconciler::settle_gone` in `crates/arca-reconcile/src/lib.rs`; the
fake's held operations (`FakeIncus::{hold_operation, hold_operation_as, hold_create, release_operation, fail_operation,
answer_wait_ended, set_operations_unreadable}`,
with a create whose object shows only when its operation is released, `Appears::OnCompletion`, or while it runs,
`Appears::WhileRunning`) in `crates/arca-incus/src/fake.rs`; tests in `crates/arca-incus/tests/operations.rs`, among them
`only_an_operation_that_can_still_create_change_or_delete_its_object_may_write_it` and
`writing_on_leaves_out_the_sessions_running_on_an_object`;
`a_read_back_while_a_create_still_runs_on_its_target_is_unresolved_and_reads_no_object`,
`a_read_back_waits_for_an_operation_that_ends_before_the_deadline_and_then_reads_the_object`,
`a_read_back_whose_operation_fails_before_the_deadline_reads_the_object_as_absent`,
`a_read_back_whose_operations_listing_fails_is_unresolved_and_reads_no_object`,
`a_read_back_with_no_deadline_left_is_unresolved_at_once_while_an_operation_runs`,
`a_read_back_reads_its_object_at_once_while_only_an_exec_or_a_console_session_runs_on_it`,
`a_read_back_still_waits_for_a_websocket_create_on_its_object` and
`a_read_back_pauses_between_listings_when_a_wait_says_an_operation_ended_that_is_still_listed` in
`crates/arca-providers/tests/incus_provider.rs`; and the kill-restart cases
`a_restart_while_a_create_still_runs_keeps_its_intent_open_and_the_open_refuses`,
`a_create_that_completes_while_the_restart_waits_is_adopted_by_that_open`,
`a_create_whose_operation_fails_while_the_restart_waits_is_reverted_and_leaves_nothing`,
`a_running_operation_on_another_object_holds_no_intent`,
`an_exec_session_on_a_removed_entrys_instance_does_not_hold_the_removal` and
`a_failed_operations_read_holds_every_intent_and_the_open_refuses_with_no_write` in `crates/arca-reconcile/tests/restart.rs`.

## Adoption

**Rule 45.** An adoption takes one existing container that `kernel.adopt` lists from `default` into its adoption project
`arcak-adopt-<name>`, and a release takes it back. `AdoptWriter` is the only path that does it. It sends only the requests of
the table below, for that one instance, and before each request it reads the instance and refuses unless it is a container
whose `volatile.uuid` is the one the record holds (the pre-image's, until a move's copy gives it a new one), in the project,
owner class and status the step names. A step whose
outcome already reads true sends nothing. No step writes a network, a profile or an ACL, or sends a project anything but the
one create and the one delete; DeleteProject is the only DELETE and RestoreRoot the only instance PUT, and every request
passes the `exchange` guard (Rule 30). No step runs while an operation names the instance, in either project, or the
adoption project (Rule 44). Foreign means no owner's mark and no key in the kernel's namespace; Adopted means the mark
`adopt-<instance>`.

| Step | Request | Before | Done when |
|------|---------|--------|-----------|
| CreateProject | `POST /1.0/projects`, marked, restricted, granting `eth0`'s network, allowing snapshots, backups and exactly the plan's allowances | `default`, Foreign, Running | the project exists, is marked, sets exactly the plan's `restricted*` keys, and holds no other instance |
| Stop | `PUT .../state` stop, 60 s graceful, then forced if it still runs | `default`, Foreign, Running | Stopped |
| Snapshot | `POST .../snapshots`, `arca-adopt-<digest[0..12]>`, stateless | `default`, Foreign, Stopped | the snapshot exists |
| PinNic | `PATCH` `devices.eth0` only, under `If-Match` | `default`, Foreign, Stopped | the local `eth0` is the pin |
| LocalizeRoot (when the plan localizes) | `PATCH` the localized root only, as the profile gave it, under `If-Match` | `default`, Foreign, Stopped, the expanded root as planned, no local one | the local root is the plan's |
| Move | `POST` `{"name": i, "project": to}` | `default`, Foreign, Stopped, pinned, root localized | in `to` and not in `default`, under the recorded uuid or a copy's |
| Mark | `PATCH` `config.user.arcakernel.entry` only, under `If-Match` | `to`, Foreign, Stopped or Running | Adopted |
| Start | `PUT .../state` start | `to`, Adopted, Stopped | Running |
| ReleaseStop | as Stop, in `to` | `to`, Adopted, Running | Stopped |
| Unmark | `PATCH` the entry key to `""`, under `If-Match` | `to`, Adopted, Stopped | Foreign |
| MoveBack | `POST` `{"name": i, "project": "default"}` | `to`, Foreign, Stopped, pinned | in `default` and not in `to`, under the recorded uuid or a copy's |
| RestoreRoot (when the plan localizes) | `PUT` the instance as read, less the localized root, under `If-Match` | `default`, Foreign, Stopped, the local root as localized, the profiles giving it as recorded | no local device of the root's name |
| ReleaseStart | as Start, in `default` | `default`, Foreign, Stopped | Running |
| DeleteProject | `DELETE /1.0/projects/<to>` | `default`, Foreign, Running; the project marked for this adoption and holding no instance | the project is gone |

A plan that found the instance stopped says so in its pre-image (`stopped`, left out of a running instance's plan, so that
plan keeps the bytes and the digest it had before). It has no Stop, Start, ReleaseStop or ReleaseStart: those steps have no
request in its table, so a stop or a start is refused before any byte is sent, and the writer refuses to run them. Every step
it runs expects the instance Stopped, CreateProject and DeleteProject included (`AdoptWriter::expects`), so a plan of a stopped
instance refuses one started since, and a plan of a running one refuses one stopped since. Mark is its last adoption step, and
the adoption is complete once Mark is done, so its Mark refuses, with no write, an instance started by hand after its Move,
which the running table's Mark takes: stop it by hand, then Mark, or release it. Its pin is the MAC Incus starts the instance with, `eth0`'s own `hwaddr` or else
its `volatile.eth0.hwaddr`, and the IPv4 the owner names with `--expect-ip`, which `eth0`'s `ipv4.address` and any static lease
the network lists for that MAC must agree with; a dynamic lease, or none, is no claim. A release of it refuses, before any
step, an instance that is not stopped where the release finds it, and leaves the instance stopped in `default`. `verify` probes
nothing for it: it checks the move, the mark and the pin, and that the instance is stopped.

The plan's allowances are what the container already uses, mapped to Incus 7.5's `restricted.*` keys by
`adopt::needed_allowances` (the table is in [`OPERATING.md`](OPERATING.md), "Adopting a container"): the project is created
with `restricted=true`, the base keys and exactly those, and the writer refuses allowances that are not exactly the
pre-image's. They are in the plan, so the digest the owner approves covers them. What no allowance admits is refused, among
it `security.privileged`, which the adoption never grants. The writer refuses a CreateProject whose project already exists
with other restrictions, so no step loosens one. A root the default profile gives is localized before Move and taken back off
after MoveBack only when it is still exactly the one LocalizeRoot set.

The pin is `devices.eth0` with the network, the pre-image MAC as `hwaddr` and the pre-image IPv4 as `ipv4.address`. It is set
before Move and never removed. Because it sets `hwaddr`, Incus's `deviceVolatileReset` drops `volatile.eth0.hwaddr` at PinNic,
and from then on the MAC is the device's own, the same in `default`, in `to` and after a release; no step removes or renames
`eth0`. An adopted instance is never a desired-state entry, and its steps are not journal effects: the adoption lives in its
mark and in `<stateDir>/adoptions.jsonl` (JSON lines, synced, mode 0600, a link refused), which holds the plan, its digest, the
pre-image, and an intent line before each request and an outcome line after it. The record takes no intent while another is
open, and only one adoption may be open. A resume works from the recorded plan, never a fresh preflight: the plan's digest
must be the one given, and the step whose intent is the last line runs again and closes it, so Foreign-in-`to`, the window
between Move and Mark, is accepted only by the recorded uuid. A release computes its steps from the last recorded step
(RestoreRoot, ReleaseStart and DeleteProject before Move; MoveBack first after Move; Unmark first after Mark; all of them
after Start; RestoreRoot only for a plan that localizes), closes an adopt step left at its intent as failed, and keeps the pin
and the snapshot. A release whose steps include MoveBack first reads the instance in `to` and refuses, before any write, one
whose local `eth0` is not the pin, or, with RestoreRoot to come, one whose root or whose profiles' root is not the one the
plan localized, since those steps would refuse only after ReleaseStop and Unmark. A release is complete once DeleteProject is
done. The watch counts each adoption not released in full as a live entry, so an adopted instance and its project are
kernel-owned with no drift, and one that is gone is `EntryWithoutObject`. `serve` refuses a config that lists an adoption.

Incus 7.5.1 runs a stopped instance's project move as a copy: the instance gets a new `volatile.uuid`, a new `created_at` and
`volatile.apply_template=copy`. A Move or a MoveBack is confirmed under a new uuid only by the copy rule: the new uuid is a
plain uuid other than the old one; the old uuid is in no project of the instance listing; the name is gone from the project it
left; exactly one instance of the name is in its new project; that one is Foreign, its local `eth0` is the recorded pin and it
holds the snapshot `arca-adopt-<digest[0..12]>`; and the adoption project carries this adoption's mark. Otherwise the step
refuses, and after its request it fails as landed but unconfirmed. The record writes the new uuid in a `uuid` line before the
step's outcome, and every later step, `verify` and a later run expect the uuid the record holds last. A Move whose copy was not
recorded is confirmed by the same rule when the run is resumed, and nothing is sent again. A MoveBack whose writer still expects
the pre-image's uuid takes such a copy in `to` by the same rule. Mark also takes a Running instance, which is one started by
hand in `to` after a Move whose outcome was not recorded; Start then finds its outcome already holds.
Source: `adopt::{AdoptWriter, Allowances, needed_allowances, adoption_project_body, pin_patch, pin_device}` in
`crates/arca-incus/src/adopt.rs`;
`adoptions::{AdoptionPlan, AdoptionRecord, resume_steps, release_steps}` in `crates/arca/src/adoptions.rs`;
`cmd_adopt::{preflight, verify, verify_with, verify_stopped, http_probe}` in `crates/arca/src/cmd_adopt.rs`; `cmd_serve::watch_handler` in
`crates/arca/src/cmd_serve.rs`; the fake's stop and start, snapshots, backups, moves (copies under a new uuid), restricted
project checks on a moved-in instance, ACL lookup, lease table and `deviceVolatileReset` in
`crates/arca-incus/src/fake.rs`. Tests: in `crates/arca-incus/tests/adopt.rs`,
`each_allowance_the_table_derives_is_one_the_fakes_incus_model_needs_and_none_is_extra`,
`what_no_allowance_admits_is_refused_with_its_reason`,
`a_writer_is_refused_for_allowances_that_are_not_exactly_the_pre_images_or_a_nic_the_pin_cannot_carry`,
`an_instance_whose_nic_sets_its_ip_and_acls_and_whose_root_a_profile_gives_is_adopted_and_released_with_no_allowance`,
`a_nic_acl_needs_no_allowance_but_must_be_in_default_where_the_adoption_project_reads_it`,
`a_host_path_disk_and_nesting_get_exactly_their_allowances_and_the_adoption_runs`,
`a_changed_root_is_neither_localized_nor_taken_off_and_the_move_waits_for_the_localized_root`,
`restore_ready_refuses_while_adopted_when_the_instances_own_root_changed_since_it_was_localized`,
`the_adoption_project_is_deleted_only_when_empty_and_this_adoptions_and_created_only_as_planned`,
`the_expected_class_and_project_of_each_step_follow_the_table`,
`an_adoption_keeps_the_mac_and_ip_and_each_step_sends_only_its_own_request_once`,
`a_release_keeps_the_pin_the_snapshot_the_mac_and_the_ip_and_each_step_runs_once`,
`a_move_without_the_pin_clears_the_mac_the_instance_depends_on`,
`a_move_that_copies_the_instance_under_a_new_uuid_is_confirmed_and_the_new_uuid_is_followed`,
`a_copy_under_a_new_uuid_is_refused_when_the_snapshot_is_missing_or_the_pin_differs`,
`a_copy_under_a_new_uuid_is_refused_while_the_old_uuid_is_in_default_or_two_of_the_name_are_in_to`,
`a_copy_under_a_new_uuid_is_refused_while_the_old_uuid_is_in_any_project`,
`a_copy_whose_new_uuid_is_not_plain_is_refused_and_not_followed`,
`a_copy_under_a_new_uuid_is_refused_while_the_adoption_project_is_not_this_adoptions`,
`every_step_out_of_its_expected_project_class_or_status_is_refused_with_no_write` and
`a_running_operation_on_the_instance_holds_every_step_with_no_write`; in `crates/arca/tests/adopt_apply.rs`, the preflight
refusals, `each_thing_the_instance_already_uses_is_planned_as_exactly_its_allowance_and_the_adoption_runs`,
`a_nic_on_a_network_attach_networks_does_not_list_is_planned_as_its_grant`,
`an_instance_whose_nic_sets_its_ip_and_acls_and_whose_root_a_profile_gives_is_adopted_verified_and_released`,
`an_eth0_hwaddr_that_agrees_with_the_running_mac_is_kept_and_the_adoption_runs`,
`plan_then_apply_adopts_the_instance_and_keeps_its_mac_ip_lease_and_config`,
`a_stopped_instance_whose_mac_or_ipv4_cannot_be_settled_is_refused_with_no_write`,
`a_stopped_instance_is_adopted_verified_and_released_stopped_with_its_mac_and_the_named_ipv4_pinned`,
`a_stopped_adoption_verify_fails_on_a_lost_pin_or_a_running_instance`,
`a_stopped_adoption_killed_before_its_move_and_started_by_hand_is_not_released_until_stopped` and
`apply_takes_an_expect_ip_for_a_running_instance_only_as_its_running_ipv4_and_the_digest_is_the_same`; in
`crates/arca/tests/adopt_release.rs`,
`every_kill_point_of_an_apply_resumes_to_a_complete_adoption_with_no_step_sent_twice`,
`every_kill_point_of_an_apply_releases_to_the_pre_image_location_class_mac_and_ipv4`,
`the_move_mark_window_stopped_unmarked_in_to_is_resumed_and_released`,
`foreign_in_to_is_accepted_only_for_the_uuid_the_record_holds`,
`a_resume_of_a_move_incus_ran_as_a_copy_records_the_new_uuid_marks_it_and_sends_no_second_move`,
`a_resume_of_an_unconfirmed_copy_whose_pin_differs_is_refused_with_no_write`,
`a_full_adopt_then_release_keeps_the_mac_with_volatile_reset_modelled`,
`a_release_whose_pin_was_removed_is_refused_before_any_write`,
`a_rooted_release_whose_profile_root_or_own_root_changed_is_refused_before_any_write`,
`a_held_operation_blocks_the_next_step_of_a_resume_and_of_a_release`,
`verify_fails_on_a_changed_mac_a_changed_ipv4_a_closed_port_or_a_5xx`,
`verify_waits_for_a_slow_start_and_reports_the_last_failure_when_time_runs_out`,
`verify_through_a_url_never_shows_its_query_which_may_carry_a_secret`,
`the_steps_left_of_a_stopped_plan_follow_the_recorded_last_step`,
`every_kill_point_of_a_stopped_apply_resumes_to_a_stopped_adoption_with_no_step_sent_twice`,
`every_kill_point_of_a_stopped_apply_releases_to_default_still_stopped` and
`after_adoption_the_watch_shows_the_instance_kernel_owned_with_no_drift`; in `crates/arca-incus/tests/adopt.rs`,
`a_stopped_instance_is_adopted_and_released_with_no_stop_or_start_and_stays_stopped`,
`a_stopped_plan_has_no_stop_or_start_request_and_expects_the_instance_stopped_at_every_step`,
`a_stopped_plan_refuses_an_instance_started_since_and_a_running_plan_refuses_a_stopped_one` and
`a_stopped_adoption_started_by_hand_is_released_only_once_it_is_stopped_again`; and in `crates/arca/src/adoptions.rs`,
`a_running_plan_keeps_its_digest_and_a_recorded_plan_stays_valid`.

## ACL arrays and scoped observations

**Rule 46.** Config keys accept uppercase ASCII letters only after a leading lowercase `environment.` prefix.
`environment.DATABASE_URL` is accepted; `Environment.X`, `foo.BAR`, non-ASCII letters and path separators are refused.
Every other key keeps the lowercase ASCII, digit and `_ . -` rule. Both the provider and the writer enforce it.
Source: orchestrator ruling / Group 22a; `plain_config_key` in `crates/arca-providers/src/incus.rs` and `plain_key` in
`crates/arca-incus/src/write.rs`; tests `environment_uppercase_suffix_is_accepted_and_other_uppercase_keys_are_refused`
in `crates/arca-providers/tests/incus_provider.rs` and `writer_accepts_uppercase_only_after_lowercase_environment_prefix`
in `crates/arca-incus/tests/write.rs`.

**Rule 47.** `IncusPatchAclField { object, field, rules }` replaces exactly one top-level ACL array, `ingress` or `egress`,
with an array of rule objects. It is class A, addresses only an ACL in `default`, and requires the object's own config to
classify as the requesting entry's own. Planning reads the ownership, previous array and ETag together without writing;
the effect rechecks them and PATCHes under `If-Match`, sending both arrays and the description from that read, with only
the named array changed, and `config: {}`. Incus merges config but replaces the other writable fields even when omitted;
the config map must be present for the merge. Config patches and tombstones also preserve both arrays and the description.
A stale ETag is 412 and changes nothing.
No other kind or field is admitted, and a retiring, foreign, droidd, contested or retained ACL is refused.
Source: orchestrator ruling / Group 22a; `EffectOp::IncusPatchAclField`, `IncusProvider::{plan, effect}` and
`UnixIncusWriter::patch_acl_field`; tests `acl_array_patch_journals_typed_pre_image_and_inverse_restores_both_fields`,
`acl_array_patch_refuses_foreign_droidd_non_acl_and_non_object_rules` and `acl_array_patch_answered_412_is_stale` in
`crates/arca-providers/tests/incus_provider.rs`, `acl_array_patch_under_a_stale_etag_is_412_and_changes_nothing` in
`crates/arca-incus/tests/write.rs` and `ctx_accepts_only_acl_arrays_with_the_planned_inverse_and_class_a` in
`crates/arca-kernel/tests/plugin.rs`.

**Rule 48.** An ACL array patch follows Rule 23's four steps. Its intent durably records `acl_patch` with `field`, `previous`
and `rules`; its inverse is `IncusRestoreAclField { object, field, rules }`, whose rules are the previous array. Its read-back
must still classify as the entry's own and show the sent array; undo restores the previous array through the same journalled
op. After a restart, an owned ACL showing the previous array settles as not landed, and one showing the sent array settles
with the restore inverse. A different array, another owner or an unreadable object keeps the intent unresolved. Existing
records omit `acl_patch` and keep deserializing. Writer diagnostics omit config and rule values.
Source: orchestrator ruling / Group 22a; `AclPatch`, `Inverse::IncusRestoreAclField`, `JournalSink::intend_acl`,
`Provider::read_back_acl`, `IncusProvider::read_back_acl` and `Reconciler::{settle_acl_patch, restore}`; tests
`acl_array_intents_replay_before_and_after_patch_and_removal_restores_the_array` in
`crates/arca-reconcile/tests/restart.rs`, `acl_array_inverse_is_closed_and_old_records_keep_their_serialized_shape`
in `crates/arca-kernel/tests/journal.rs`, `acl_array_patch_journals_typed_pre_image_and_inverse_restores_both_fields`
and `acl_array_patch_read_back_requires_the_sent_array_and_the_entrys_ownership` in
`crates/arca-providers/tests/incus_provider.rs` and `writer_accepts_uppercase_only_after_lowercase_environment_prefix`
in `crates/arca-incus/tests/write.rs`.

**Rule 49.** `Observe::IncusGet` admits only an instance object or its `state` path in a kernel-family project named by the
requesting manifest, or an ACL in `default` whose own config classifies as the requesting entry's own. The path must name
one plain object and exactly one literal `project` query; unknown endpoints, encodings, extra query parameters and projects
outside the family are refused. Observations send GETs only. Returned values equal to current shared or own secret contents
become their `arca-secret:` references, including instance config and expanded config. Otherwise, a value equal to another
entry's secret becomes `<redacted: another entry's secret>`. Shared and own matches take precedence. Both secret lists are
read before the GET, and redaction is applied after it.
Source: orchestrator ruling / Group 22a; `IncusProvider::{observe, secrets_by_value, foreign_secrets_by_value, read_secret_file}`,
`may_use_secret`, `FOREIGN_SECRET_MARKER` and `redact_secret_values`; tests
`scoped_observe_allows_instance_state_and_own_acl_and_redacts_secret_values` and
`scoped_observe_marks_another_entrys_secret_and_keeps_own_and_shared_references` in
`crates/arca-providers/tests/incus_provider.rs`.
Group 22 additionally admits an own project object inside the kernel family, with matching project name/query, and an own
network object in `default`. Both require the manifest's project and the requesting entry's own mark; this lets a partial
netproject apply retry without adopting a foreign object. No other route is admitted. Source: `IncusProvider::observe`;
tests: `own_project_and_network_observe_are_scoped`, `own_project_and_network_observe_refuses_foreign_and_other_routes`.


**Rule 50.** Serve may execute only the service module's closed command enum, outside the effect journal and unavailable to
plugins or Provider. It refuses adopted, OCI, foreign, retained, retiring, stopped and UUID-mismatched instances. Identity
comes from the newest live class-B instance effect, and a post-exec UUID check discards output from a changed instance.
Every step has an in-instance SIGKILL timeout, a client deadline, a 64 KiB recording cap, and a synced private exec audit with
no argv or output. Client deadlines mark that UUID hung; only key cleanup and daemon stop remain permitted. Recordings are
deleted on every outcome. After acquiring the state lock, startup sweeps at most 256 validated unfinished audit operations,
newest first, by their exact recording names. A sweep failure does not stop serve.

Tailnet keys travel only in the root:0600 files request at the fixed key path. No environment, argv, result, error, audit,
journal or public metadata contains them. Publication is serialized by an async mutex; its lock precedes the writes lock
and kernel lock. Journal reads take no such lock and admit four callers. Removal spends approval under the kernel lock,
releases it for a total 60-second logout budget, then proposes and runs the removal under one kernel lock. Logout failure
never prevents deletion. Config, publication records and ACL changes remain normal journal effects. Adopted service writes
are refused explicitly; their ownership classifier is unchanged.
Source: `ExecCommand` and `ServiceFile` in `crates/arca-incus/src/exec.rs`; `ServiceExec::{run, check, sweep}` in
`crates/arca/src/service_exec.rs`; the service wiring and removal tests in `crates/arca/tests/services/wiring.rs`.

**Rule 51.** A GPU assignment writes only when the node is in kernel mode, and only to an instance in the kernel's project
family (Rules 5 and 8). `serve` builds no device writer in watch-only mode, and the GPU handler refuses there. The
instance's own config must classify as a kernel object: a foreign, droidd, contested or retained instance is refused even
inside the family. When the proposal is made, the kernel resolves the instance's project, type and `volatile.uuid` and puts
them in the proposal's arguments, so the owner's digest binds them; execution re-reads the instance under that project and
refuses one that moved, changed type or was recreated with another uuid. The writer itself holds the same `ProjectFamily` and refuses a project
outside it before any request, a read included. It changes only the instance's own `devices` map: it adds exactly
`gpu-<normalized pci>` with `{"type":"gpu","gputype":"physical","pci":"<normalized pci>"}`, under the read's `If-Match`, and
detaches only that exact device; any other GPU device is refused as not Arca's, and a second device for the same card is
never a false failed removal. The PCI spelling is normalized on both sides. The project, type and uuid checks use the same
identity rule Rule 50 uses for exec, and the project rule is Rule 45's: a separate, narrow path, never a widening of
`ProjectFamily`.
Source: `GpuBackend`, `IncusGpu`, `GpuRefusal` and `GpuActionHandler` in `crates/arca/src/gpu.rs` and
`crates/arca/src/handlers.rs`; `DeviceAssigner::{attach_gpu, detach_gpu}` and `normalize_pci` in
`crates/arca-incus/src/assign.rs`; tests `two_identical_gpus_stay_apart_in_proposals_and_only_an_approval_runs_one`,
`a_gpu_proposal_binds_identity_and_execution_refuses_a_recreated_instance`,
`an_instance_outside_the_kernel_family_is_refused`, `a_watch_only_node_has_no_gpu_writer`,
`a_stale_etag_is_412_and_changes_nothing`, `a_gpu_device_that_arca_did_not_add_is_refused_and_left_alone`,
`two_devices_for_one_card_do_not_leave_a_false_unconfirmed_removal`, `the_writer_itself_refuses_a_project_outside_its_family`
and `the_backend_refuses_a_foreign_or_retained_instance_and_reads_the_mode_each_call`.

## Importing a service another manager created

**Rule 52.** The service import is the one write the kernel makes to an instance another manager created, in that manager's
project, and it is one key. `ImportWriter` is the only path that sends it. It holds no project family and no classifier; it
holds one allowance, `import_allowance`, and can build one request: a config `PATCH` of `user.arcakernel.entry`, under the
`If-Match` of the read the caller checked. The allowance holds only when all of this is true:

- the entry is `adopt-<instance>`, that instance's import entry, and no other kind of entry;
- the object is an instance, named with no parent, and its `{project, instance}` is listed in `kernel.importServices`;
- the change names exactly one key, `user.arcakernel.entry`, and either sets it to that entry's id (the import) or removes it
  (the release).

A second key in the same change, another key alone, another value for the mark, another instance, a project or a name the
allowlist does not hold, another entry and an empty ETag are refused before any request, a read included. A changed ETag is
Incus's 412 and changes nothing. The writer has no device, profile, state, ACL, PUT, POST or DELETE request to send. The
configuration refuses an allowlist entry whose project is not one of `kernel.droiddProjects`, so the allowlist can only name
another manager's project, and it is empty by default. Every other writer is as it was: `ProjectFamily` still refuses that
project, so the provider answers `ProjectNotAllowed` for any effect on the instance, for the import's own entry too, before
the mark and with it; and with the mark the instance classes `Contested` (Rule 4), which no other writer writes.
Source: `import_allowance`, `ImportWriter::{read, patch}` in `crates/arca-incus/src/import.rs`; `check_import_services` in
`crates/arca/src/config.rs`; tests `the_import_writer_sends_one_key_under_if_match_and_refuses_every_other_write`
(`crates/arca/tests/service_import.rs`) and `the_import_allowlist_is_empty_by_default_and_refused_unless_every_rule_holds`
(`crates/arca/src/config.rs`).

**Rule 53.** An import and a release run only from an action proposal the owner approved (`service_import`,
`service_release`), and each re-reads the instance before it writes. The import refuses, with no write, unless the name is
listed, the instance is a container in the listed project, it carries the other manager's service keys (`user.arca.template`,
`user.arca.address`, `user.arca.ports`) and no key in the kernel's namespace, its address is inside the listed cidr and is the
one its `eth0` pins, its keys read as a service record, Arca has no service of that name, and it has a uuid. A proposal whose
plan is not ready is refused when it is proposed. The release clears the key only when the instance still carries
`adopt-<name>`. It refuses, with no write, when the allowlist no longer lists the instance, when the instance's uuid is not
the one the import recorded, when it carries another key in the kernel's namespace, or when its mark names another entry. An
instance that is gone, or whose mark is already cleared, takes no write. The record `service-<name>` is removed only after
the key is cleared, since a marked instance gets its record again on the next call. Each write is recorded in
`<stateDir>/adoptions.jsonl`, the intent before the request and the outcome after it, with the instance's uuid and the
pre-image digest; no config value is recorded. A restart reads the record back and writes nothing.
Source: `plan`, `import`, `release` and `image` in `crates/arca/src/service_import.rs`; the `service_import` and
`service_release` arms of `ServiceHandler::call` and `adoption_subnet` in `crates/arca/src/handlers.rs`; `ImportEvent` in
`crates/arca/src/adoptions.rs`; tests `an_approved_import_writes_one_key_and_makes_the_adopted_record`,
`an_approval_for_another_target_and_a_replay_are_refused_and_write_nothing`,
`an_import_that_fails_a_check_is_refused_before_any_write`, `a_release_restores_the_exact_pre_image_and_removes_the_record`,
`a_release_refuses_in_plain_words_when_the_inverse_is_not_safe`,
`a_restart_reads_the_imported_record_back_and_writes_nothing` and
`every_other_writing_verb_still_refuses_an_imported_service` in `crates/arca/tests/service_import.rs`.
## Bridged and mixed nodes

A node's config says which networks its services may use. `kernel.serviceCidr` gives it the managed service network, which
Arca creates. `kernel.bridged` gives it host bridges that already exist, which Arca never writes. A node with only the first is a
managed node, a node with only the second is a bridged node, and a node with both is a mixed node. Containers, databases and
consumers live on the managed network only. A virtual machine lives on either.

**Rule 54.** A node has one service project, `arcak-svc`, whatever its networks. The entry that first needed the project
owns it and carries its mark: `net-arcak-svc` (plugin `arca.netproject`) when the first service used the managed network,
`bridged-arcak-svc` (plugin `arca.bridgedproject`) when it was a virtual machine on a host bridge. The owner never changes. When
the node later gains the other network, the project is widened in place by its owner; no second project is made, and nothing is
moved. A second project was ruled out for three reasons. Every service path names the one project: the family in
`kernel.projects`, the ACL names, the exec identity (Rule 50), the address plan and the caps all read it, so a second project
would need the operator to edit `kernel.projects` and would split one address plan and one set of caps over two projects. A
virtual machine that already runs would have to stay where it is, so which project a service is in would depend on the node's
history. And the widened project is no looser than the two projects taken together.
Source: `project_changes` and `project_owner` in `crates/arca/src/handlers.rs`; tests
`a_bridged_node_that_gains_the_service_network_becomes_mixed_in_place_and_its_vm_is_untouched` and
`a_managed_node_that_gains_virtual_machines_widens_its_own_project` in `crates/arca/tests/services/mixed.rs`, which
`crates/arca/tests/services_bridged.rs` includes. The first is the in-place upgrade of a bridged node that already runs a
virtual machine: serve's own start check passes on the new config, the machine's record and instance read back unchanged,
and the project is written once more, by a PUT.

**Rule 55.** The project is widened only by its owner, only through the journaled project config write (Rule 43), and only by
the steps below. Each step is a field of the owner's entry, so the desired state records it, and an entry without the field
is exactly the entry an earlier kernel wrote.

| Owner | Field | Written when | The write |
| --- | --- | --- | --- |
| `bridged-arcak-svc` | `managed: true` | the first service on the managed network | unsets `limits.containers`, `limits.cpu`, `limits.memory` and `limits.disk`; sets `restricted.devices.proxy` to `allow` |
| `net-arcak-svc` | `virtualMachines: <cap>` | the first virtual machine, and any later virtual-machine create that finds the configured cap changed | sets `limits.virtual-machines` to the cap |
| `bridged-arcak-svc` | `virtualMachines`, `cpu`, `memory`, `disk` | a virtual-machine create that finds the configured caps changed | sets `limits.virtual-machines`, and while `managed` is unset `limits.cpu`, `limits.memory` and `limits.disk`, to the caps |
| `net-arcak-svc` | `hostBridges: true` | the first virtual machine on a host bridge | sets `restricted.devices.nic` to `allow`; unsets `restricted.networks.access` |

The caps Incus holds follow the node's configured caps, for both owners alike, and nothing is set by hand. A
virtual-machine create that has passed Arca's own cap check (Rule 58) writes the configured caps into the owner's entry when
they differ, up or down, in the same document as the new service and ahead of it, so the project is written before the
instance is sent. Because that check has passed, a lowered cap is never below what the node holds; a cap lowered further
refuses the create in words before any write and leaves the entry and the project as they were. The owner's
entry carries the configured caps whenever it is written at all, the widening of Rule 55's table included: the bridged
plugin writes the count on every apply, so an entry left with a stale count (one an earlier release wrote, its project
raised by hand since) would otherwise set the project below what it holds, which Incus refuses. Before any such write,
a configured count below the number of virtual machines the node holds refuses the create in words, naming both numbers
and the config key. A value set by hand on the project is replaced differently by the two owners: the network entry
re-applies on every pass and writes its entry's value back (its `0`, held until the first virtual machine, is never
written); the bridged entry is applied only when it changes, so a hand-set value stays until its next write. A pass over
an unchanged desired state writes nothing.

On a project the bridged entry owns, `net-arcak-svc` is written with `sharedProject: true`: it creates the managed bridge and the
service profile, and it never writes the project. The bridged project never had a bridge grant, so the managed bridge
needs none there. `limits.cpu`, `limits.memory` and `limits.disk` go because Incus counts every instance of a project against
them, containers included (Rule 58). `restricted.networks.access` goes because Incus checks a host bridge named as a NIC's
`parent` against that list once NICs are allowed, and the list may name only bridges the entry made (Rule 41); a project with
host-bridge NICs has no grant on a bridged node either. Which bridge a NIC joins is then Arca's own check, as it is on a bridged
node: the managed bridge, or a name in `kernel.bridged.bridges`. The provider admits these guarded keys for these two entries in
this one project and for no other: `restricted.devices.proxy=allow` on a create or a patch by either entry,
`restricted.devices.nic=allow` on a create by the bridged entry or a patch by the network entry, and the removal of
`restricted.networks.access` by the network entry. Every other guarded key is refused as Rule 30 states.
Source: `BridgedProject::apply` and `NetProject::apply` in `crates/arca-netproject/src/lib.rs`; `service_proxy_allowance`,
`host_nic_allowance`, `bridge_grant_removal` and `check_patch_keys` in `crates/arca-providers/src/incus.rs`; Incus's
`NetworkAllowed` in `internal/server/project/project.go` for the grant; tests
`the_service_project_owners_may_widen_it_and_no_other_entry_or_key_may` and
`the_host_bridge_nic_allowance_is_scoped_to_the_bridged_project_entry_and_to_a_create` in
`crates/arca-providers/tests/incus_provider.rs`,
`the_bridged_project_is_created_alone_with_no_network_or_profile_and_a_retry_skips_it` and
`the_network_entry_widens_its_own_project_for_virtual_machines_and_host_bridges` in
`crates/arca-netproject/tests/netproject.rs`, the two tests of Rule 54, and for the caps
`the_cap_incus_holds_follows_the_configured_cap_up_and_down`, `the_bridged_owners_caps_follow_the_config_too`,
`a_widening_writes_the_configured_caps_over_a_stale_entry_and_the_first_container_is_created`,
`a_widening_with_a_count_below_the_machines_held_is_refused_in_words_before_any_write` and
`a_bridged_nodes_lowered_caps_are_refused_below_use_and_followed_above_it` in `crates/arca/tests/services/mixed.rs`. The
fake refuses a project write whose `limits.virtual-machines` is below the virtual machines the project holds, as Incus does.

**Rule 56.** A virtual machine on the managed network is a service like a container. `service_create` with no `bridge`, on a node
with `kernel.serviceCidr`, gives it the next free address of the same plan (`.10` to `.239`, shared with containers and
databases), a NIC on the managed bridge with that address reserved and MAC and IPv4 filtering on, and its own NIC ACL with the
same rules: every ingress rejected except a link's, egress to the internet only. It takes no profile (the service profile holds
container keys) and names its root disk on the node's pool: `kernel.bridged.pool` when the node is bridged too, the default
profile's pool otherwise. A `bridge` names a host bridge and is allowed only from `kernel.bridged.bridges`; such a virtual
machine is as it is on a bridged node, with no address, ACL or link. What is sound for a virtual machine on the managed network
is kept, and the rest is refused in words:

| Verb | On the managed network | Why |
| --- | --- | --- |
| `service_connect` to it | allowed when its template declares `ports` | A link is a rule in the provider's NIC ACL plus values for the consumer; both hold for a virtual machine. |
| `service_connect` from it | refused | The consumer's values are container environment, which a virtual machine's programs never read. |
| `service_publish` | refused | A dash's tailnet node runs inside an Arca-built container image; no virtual-machine image carries it. |
| `service_secret_set` | allowed for a secret its template declares | The value is stored and written into the guest at the next `instance_start`, never into its config (Rule 68). |
| `mac` on `service_create` | refused | The address is reserved by Arca's plan; a fixed MAC is for a host bridge whose own network hands out addresses. |
| a template with `ports` or `provides`, on a host bridge | refused | Ports are opened by the managed network's ACL, which a host bridge does not have. |

Source: `Service::apply`, `managed_vm_body` and `validate_vm_template` in `crates/arca/src/service.rs`; the `service_create` arm
of `ServiceHandler::call` and `vm_network` in `crates/arca/src/handlers.rs`; `call_wiring` and `link` in
`crates/arca/src/service_wiring.rs`; tests `a_managed_node_that_gains_virtual_machines_widens_its_own_project` (the
machine's NIC, address and ACL) and `a_container_links_to_a_virtual_machine_and_the_other_verbs_are_refused_in_words`.

**Rule 57.** Holding both kinds is never refused. What is refused is a config that would leave a half-state, at `arca serve`'s
start before anything is reconciled, and again before a virtual-machine create writes:

- `kernel.bridged` without `kernel.serviceCidr`, on a node that holds the managed network, a container, a database, a consumer
  or a virtual machine on the managed network: the managed network would be dropped under them.
- no `kernel.bridged`, on a node that holds a virtual machine on a host bridge: its bridge would no longer be an allowed one.
- no `kernel.bridged`, on a node that holds the bridged project entry before it was widened (`managed` unset): the project
  is still a bridged-only one. The refusal says what can be done: put `kernel.bridged` back, and add `kernel.serviceCidr`
  beside it to make the node mixed; it cannot become a managed-only node.
- no caps (Rule 58), on a node that holds any virtual machine.

A mixed node may list `kernel.importServices` (Rules 52 and 53): an import needs the node's own service network, which a
mixed node has, and an imported service is a record on the managed side, so a config that drops `kernel.serviceCidr` is
refused while one is held. A bridged node still may not: it has no service network. Test
`a_mixed_node_may_list_imports_and_a_bridged_node_may_not` in `crates/arca/src/config.rs`.

A bridged node with no `kernel.serviceCidr` behaves exactly as before: it refuses a container template, and a virtual machine
with no `bridge` joins its only bridge.
Source: `mode_refusal` in `crates/arca/src/handlers.rs` and `check_bridged_start` in `crates/arca/src/cmd_serve.rs`; tests
`the_mode_refusals_protect_against_a_half_state_and_allow_both_kinds` and, in `crates/arca/tests/services_bridged.rs`,
`a_managed_node_switched_to_bridged_is_refused_at_start_and_at_create_and_nothing_is_left_behind` and
`a_bridged_node_switched_to_managed_is_refused_at_start`, which pass unchanged.

**Rule 58.** The virtual-machine caps (count, CPUs, memory, root disk) are one set per node and cover every virtual machine Arca
holds, on either network: `kernel.bridged.limits` when the node sets `kernel.bridged`, and `kernel.vmLimits` on a managed node
that does not. A node with neither creates no virtual machine. `service_create` adds up the virtual-machine records it holds and
refuses a create that would pass a cap, before any write. A proposed create is refused the same way when the proposal is made,
by the same function, so no card is recorded for it; the approved create asks again. Incus enforces them too only where it can do so for virtual machines
alone: on a bridged node the project carries all four as `limits.*` and `limits.containers` is `0`; on a mixed or managed node
the project carries `limits.virtual-machines` only, because Incus's `limits.cpu`, `limits.memory` and `limits.disk` count the
containers of the project as well. So on those nodes the CPU, memory and disk caps are Arca's check alone, and a virtual machine
made by hand in the project is outside them. Containers have no node cap: each keeps the service profile's own limits (or its
template's), and the address plan bounds containers, databases and managed virtual machines together at 230.
Source: `KernelConfig::vm_caps`, `check_bridged` and `check_vm_limits` in `crates/arca/src/config.rs`; `caps_refusal` and
`create_room_refusal` in `crates/arca/src/handlers.rs`; tests
`the_caps_count_virtual_machines_on_both_networks_and_no_container`,
`vm_limits_are_the_caps_of_a_managed_node_without_host_bridges` and
`a_proposal_over_a_cap_of_the_node_is_refused_when_it_is_made_and_no_card_waits`.

**Rule 59.** Every virtual machine Arca creates carries the device `agent` (`{"type":"disk","source":"agent:config"}`), the NIC
name `enp5s0` on its `eth0` device and `agent.nic_config` set to `true`. Incus 7.6 offers the guest its config share over 9p
only, and a QEMU built without virtio-9p gives the guest nothing, so the agent never starts; the `agent:config` disk carries the
same share as a drive. A restricted project always admits that disk (`checkRestrictions` in Incus's
`internal/server/project/permissions.go`). The guest's interface name follows the PCI slot QEMU gives the NIC, which depends on
the devices of the host's QEMU, and the public images configure `enp5s0` only; with `agent.nic_config` the agent renames the
interface whose MAC is the device's to the device's `name` when it starts (`osReconfigureNetworkInterfaces` in Incus's
`cmd/incus-agent/os_linux.go`), whatever its slot. Nothing is changed on a virtual machine after its create, so one made before
this rule keeps the devices it has.
Source: `vm_config`, `vm_devices`, `vm_body` and `managed_vm_body` in `crates/arca/src/service.rs`; tests
`every_virtual_machine_carries_the_agent_disk_and_a_nic_name_the_image_configures` and
`a_vm_is_created_with_its_type_devices_and_limits_and_no_network_is_created_or_read`. The fake admits the drive in a
restricted project as Incus does and refuses any other disk with no pool. That the agent renames the NIC and that the guest
then takes its address is read from Incus's source and is not yet confirmed on a live host ("Assumptions to confirm").

**Rule 60.** An instance that was created and then failed to start is a settled effect, not an open intent. Incus creates
an instance and then starts it, in one operation, and a failed start fails the operation and leaves the instance stopped
(a live host showed it; the earlier assumption that a failed operation leaves nothing was wrong for this case). The
provider settles it only when all of this holds: the create asked for a start, Incus answered a status error, the instance
now reads as stopped, no operation Incus still runs names it (Rule 44), and it reads as the entry's own with a uuid and no
tombstone. The effect is then journalled as applied, with the instance's delete as its inverse, and the plugin's apply
fails with Incus's reason, so the entry is reported failed and a removal deletes the instance through the usual plan.
Incus's reason is carried the same way when the create did not land at all. In both, every secret file's content is
replaced in the text before it is returned or journalled, since a create's body carries resolved secrets.
A create that asked for no start is not this case and needs no rule of its own. `service_create` with `start: false`,
a container template whose secret is not set yet and a virtual machine whose template declares secrets (Rule 68) all
record `start: false`; the create's body carries it, the instance is created stopped and the effect applies as any
create does. A container's secrets and links are then config patches of a stopped instance, and the first start is
`instance_start`, which is not a plugin effect. The three are not one create, though. An explicit `start: false` also
records `autostart: false` and its body carries no `boot.autostart`, whatever the template sets: Incus restores an
instance's last state when the key is unset, so a host restart never boots the service before its first
`instance_start`. Nothing writes the key later, so no instance is changed after its create for this. A create that the
template alone stops keeps the template's `autostart` in its record and `boot.autostart` in its body, as before
(tests in `crates/arca/tests/tenants/create_stopped.rs`).
Every other failed create is settled as before: failed when the name answers 404, and otherwise left open. Before this
rule the intent stayed open, which blocks every later effect until a restart.
Source: `IncusProvider::settle_create` in `crates/arca-providers/src/incus.rs`; `EffectRefusal::Landed` and `Ctx::effect`
in `crates/arca-kernel/src/plugin.rs`; `ServiceHandler::settle_create` in `crates/arca/src/handlers.rs`; tests
`a_failed_start_keeps_the_created_instance_record_and_secrets` in `crates/arca/tests/services.rs`,
`an_approved_create_that_fails_at_start_reports_its_message_and_leaves_a_defined_state` and
`after_a_create_that_did_not_start_instance_start_retries_it_and_service_remove_removes_it` in
`crates/arca/tests/services/mixed.rs` and, for the cases that stay open,
`a_create_whose_read_back_finds_an_object_is_unresolved_not_failed` in `crates/arca-providers/tests/incus_provider.rs`.

**Rule 68.** A virtual-machine template may carry a first-boot script, settings and secrets, and may let a create replace
some of its settings. The script and the settings are part of the machine's create and of nothing later; the secrets never
enter Incus's instance config.

- `firstBoot` is `{ "file": "<name>" }`, a plain file beside the template, read as an entrypoint's file is. Its text becomes
  the instance key `cloud-init.user-data`, so it begins `#!` (cloud-init runs it once, as root, at the first boot) or
  `#cloud-config`. `settings` is one JSON object of at most 16KiB. It becomes `cloud-init.vendor-data`: a `#cloud-config`
  with one `write_files` entry that writes the object to `/etc/arca/settings.json`, its content base64, so no setting can
  change the YAML around it. Both are recorded in the service's record (`vm.userData`, `vm.vendorData`), because the create
  body is built from the record; a record without them is exactly the one an earlier kernel wrote, and its create is
  unchanged. Neither may hold a secret, and nothing is changed on a virtual machine after its create (Rule 59), so a changed
  script or setting applies to a machine created after the change.
- `createSettings` lists the top-level keys of `settings` that one `service_create` may replace with its own `settings`
  argument. The value in the template is the default and gives the shape. `create_settings` refuses, before any write and
  again when a create is proposed, and names the place down to a list's element (`setting_refusal`): a key that is not
  listed; at any level a value of another JSON type than the default, an element of a list unlike the default's first,
  or a key the default's object does not have; a number that does not read back the same from the canonical JSON an
  approval's digest is made over (`2.0`, a whole number of 2^53 or more), which before this check spent the owner's
  approval on an internal error, and a negative number where the default is not negative; a control character or a
  character that reorders or hides text (the bidirectional controls, zero-width characters) in any string or key, or
  more than eight levels; where the default is an `https://` address, anything but `https://<host>[/<path>]` with a
  plain path (`https_address_ok`: no `@`, `?`, `#`, `%`, space or backslash) on the default's own host and port, since
  such a setting decides where the guest sends a credential; where the default is a size, anything but a size above
  zero of at most six digits, and under a key named `memory` more than the template's `limits.memory`; and a merged
  object over the settings size. The machine's `cloud-init.vendor-data` is built from the merged object; the record
  keeps the replaced values as `vm.settings`, and a record of a create that gave none has no such key and is the record
  of before. `service_status` answers `settings`, read back from the record's vendor data (`Vm::guest_settings`), so it
  is what the machine got whatever the template says later; the owner's card has one row for each replaced key with its
  value in words, and for a list one row for each element (`setting_rows`). The card cuts nothing a proposal carries:
  `ActionKind::prepare` refuses a setting whose row would pass 160 characters (`SETTING_ROW_MAX`), so the owner
  approves exactly what is shown. Settings never hold a
  secret. Tests in `crates/arca/tests/services/create_settings.rs`.
- `update` is `{ "pin": "<key>" }`, on a virtual-machine template only: the key of `settings` that holds the version
  its service runs. `validate_vm_template` refuses a key that `settings` does not have, one that `createSettings`
  lists (a create never chooses the version), and a value that is not an image by digest (`image_by_digest`).
  `service_status` answers `update`: `pin`, the version the service was created with, read from its own record
  (`created_pin`: a container's OCI source, or what the record's vendor data holds under that key), `templatePin`, the
  one the template installed in `<stateDir>/templates` names now (`template_pin`), and `state`, `same` or `differs`, or
  `unknown` with a `problem` when the installed template cannot say. `service_list` carries `update` only on a row that
  differs. It is a read of the node's own records and files and writes nothing: no registry or network is asked, no
  instance, record or journal changes, and it says only that the two differ, never which is newer. An adopted
  instance and a service from a system image carry no `update`. Tests in
  `crates/arca/tests/services/update_available.rs`.
- `secrets` names the template's secrets. `service_secret_set` stores a value in the secret store, the private files under
  `<stateDir>/secrets` that a container's secrets use, and puts a reference in the record's `secretRefs`. A virtual
  machine's value may have line breaks (a private key in PEM form), since it is a file and not an environment value, and
  it is stored as given, with the line break at its end when it has one (the limit of 4096 characters does not count
  that one); a carriage return before a line feed is dropped, and a carriage return alone is refused. A container's and
  a function's value is one line, and a line break at its end is refused (`service::secret_value`). A virtual machine whose template
  declares secrets is created stopped. That fails closed and is not a guarantee that it never boots without them: a
  machine with `boot.autostart` that Incus starts by itself before the first `instance_start` boots with no secret file,
  and what runs in it must wait for the file.
- The channel is Incus's file request, which Incus hands to the guest agent: after `instance_start` or `instance_restart`
  has left the machine running, Arca reads each stored value and sends `POST /1.0/instances/<name>/files?path=...` with the
  value as the request body. First `/etc/arca-secrets`, a directory of root's with mode 0700; then one file per secret in
  it, named by the secret, of root's with mode 0600. The directory comes first because Incus makes a new file before it
  sets its mode, and only root enters the directory. The directory is read before it is made (`GET` of the same path:
  its type, mode and owner are in the answer's headers), so one an earlier start made is left alone and nothing depends
  on how Incus answers a request to make a directory that exists. Anything else at the path, a file, a link, or a
  directory with another mode or owner, refuses the start's secrets in words and nothing is written under it. A start of
  a machine that already runs writes the files again and restarts nothing, which is how a new value arrives without an
  approval.
- Each stored value is read by the provider's own rule for a secret file and by nothing looser
  (`arca_providers::incus::read_entry_secret`): the reference names this entry's secret or a shared one, the secrets
  directory and every directory on the way are real, private and this process's own, the file is a regular file of this
  process's with mode 0600 and at most 64KiB, and the file that was checked is the file that is read.
- Why it is safe: the value is in two places at rest, Arca's secret store and the guest's own disk, and passes through
  Incus as a request body, which Incus neither stores nor logs (its lifecycle event names the path). It is not in the
  instance config, the desired state, the effect journal, the exec audit or any answer. The instance action journal gets
  one line, `{"kind":"guest_secrets","outcome":"written","secrets":[<names>]}`, or `"failed"` with no reason. A refusal uses
  fixed words and names a secret, never a value. What this does not hide: root inside the guest reads the files, and a
  snapshot or a copy of the machine's disk carries them. The guest's disk was chosen over memory (`/run`) so that a
  machine that Incus starts by itself after a host restart, or that restarts from inside, still has its secrets without
  Arca being asked.
- The write is not a plugin effect, like an instance action (it is made beside one) and the exec seam (Rule 50). It is made
  only for a record that is a virtual machine Arca created, through a writer bound to the record's entry, on the instance
  the effect journal holds for that entry (its project and its uuid), and not on one with a tombstone. An adopted service
  and a container get nothing.
- A virtual machine in a tenant (Rule 61) is served the same way, in its tenant's project. This is the one place where a
  file request leaves the node's own project, so where it goes is never a caller's word. The project and the instance a
  call names only find the record: a record counts when its own project and instance are exactly those and its entry id
  is its own. The request is then addressed to the instance the effect journal holds for that entry, which must be in
  the project the record names; that project must be the record's own tenant's, and the tenant's entry must exist and
  name it. The writer is the exec seam's (`writer_for`): bound to the entry, over the node's family and that one
  tenant's project, so it reads no other tenant's instance as owned. Only the two guest-secret destinations
  (`/etc/arca-secrets` and a file in it) follow the owned instance into a tenant's project (`guest_target`). A command
  and every other file stay in the node's own project and are refused for a tenant's instance (`target`). A recording
  is read and deleted by a path that names the node's own project and no other, so it cannot reach a tenant's
  instance either, and none exists there since no command runs there. Rule 50's seam is as it was. The instance verbs already refuse a call that names a tenant together with another
  project, and a tenant's project without its tenant (Rule 65), before any instance is read.
- The guest agent is waited for, up to 120 seconds: a machine that has just started boots before it takes a file. When
  it never answers, the verb is refused with `InstanceSecretsNotWritten` and the machine stays running. The answer says
  what to do next once, and only what is true: to run `instance_start` again when the guest was not ready or a request
  did not get through (a read of the instance that timed out among them), and that running it again changes nothing
  when something has to be put right first (a secret that cannot be read, a directory in the guest that is not
  private, a record that does not fit its instance, an instance that is no longer the one Arca created, a kernel lock
  that is poisoned until Arca is restarted). The place checks are one function, `misplaced` in
  `crates/arca/src/guest_secrets.rs`, with its own tests there: a tenant entry that is missing or names another
  project, a record in another project or with another instance than its own, and a journalled instance that is
  missing, has no uuid, or is in another project.

Source: `first_boot`, `settings_vendor_data`, `validate_vm_template` and `vm_config` in `crates/arca/src/service.rs`;
`vm_service_config` in `crates/arca/src/handlers.rs`; `call_wiring` in `crates/arca/src/service_wiring.rs`;
`crates/arca/src/guest_secrets.rs`; `InstanceLifecycle::run` and `with_after_start` in `crates/arca/src/lifecycle.rs`;
`ServiceFile::GuestSecretsDir`, `ServiceFile::GuestSecret`, `guest_target`, `guest_secrets_dir` and `write_service_file` in
`crates/arca-incus/src/exec.rs`; `writer_for` in `crates/arca/src/service_exec.rs`; `Kernel::service_object` in
`crates/arca/src/kernel.rs`;
Incus's `fileSFTPPost` in `cmd/incusd/instance_file.go` for the order of create and mode. Tests, in
`crates/arca/tests/services/vm_secrets.rs`:
`a_vm_with_secrets_is_created_stopped_with_its_script_and_incus_never_receives_a_secret_but_as_a_guest_file` (every request
the fake received is read: a secret is in the body of its own file request and nowhere else),
`a_secret_set_on_a_running_vm_applies_at_its_next_start_which_never_restarts_it`,
`a_guest_agent_that_is_not_up_yet_is_waited_for_and_one_that_never_answers_is_said_in_words`,
`the_guest_directory_is_read_before_it_is_made_and_one_that_is_not_private_is_refused`,
`a_stored_secret_is_read_by_the_providers_rule_and_a_key_with_crlf_lines_is_stored_with_line_feeds`,
`a_vm_template_takes_a_first_boot_script_settings_and_secrets_and_no_other_kind_does` and
`the_create_requests_of_an_oci_a_system_and_a_plain_vm_template_are_the_golden_ones`, whose golden file was written by the
kernel before this rule. For a tenant, in `crates/arca/tests/tenants/isolation.rs`:
`a_start_writes_a_tenants_machine_its_own_secrets_in_its_own_project_and_nowhere_else`,
`a_call_in_a_tenant_cannot_send_secrets_to_another_tenants_or_the_nodes_project`,
`the_guest_file_seam_follows_the_owned_instance_and_a_command_never_does` and
`a_secret_that_was_not_written_says_whether_asking_again_can_help`. The fake refuses a file for a virtual machine that is stopped or whose agent is not up, as Incus
does. That cloud-init on a public cloud image runs the two keys in a machine Arca creates, and that the agent takes the
files, are read from Incus's and cloud-init's behaviour and not yet confirmed on a live host ("Assumptions to confirm").

## Tenants

A node whose config sets `kernel.tenants` is a node with tenants. A tenant is one desired-state entry, `tenant-<id>` (plugin
`arca.tenant`), where `<id>` is 8 random lowercase base32 characters and never a name. The node's own project, `arcak-svc`,
stays the node owner's. A node without the key runs none of the code below: every tenant verb, and every verb given a
`tenant`, answers "this node has no tenants".

**Rule 61.** A tenant's project is in the project family exactly while its entry exists, and only under the tenant prefix.
The project is `arcak-t-<id>`, and `arcak-scratch-t-<id>` in a scratch run. The kernel keeps the set of tenant projects: the
project of each `tenant-<id>` entry in the desired state, and of each one the journal still holds an effect for, so a tenant
being removed stays in the family until nothing of it is left. The set is filled before the journal's intents are settled at
start, before a new document is reconciled, and again after every reconcile and every plan run. The provider admits a project
outside its configured family only when the name is a tenant prefix of this run followed by a tenant id, the set holds it,
and it is not a droidd project; the writer's family is built from the same set for each write and refuses any name that is
not a tenant project's. A plugin's manifest names tenant projects by the two prefixes followed by `*`
(`manifest_names_project`); that admits nothing by itself. A service's `user.arca.*` keys are classed as its entry's own
in its tenant's project only (`classify_object`), as they are in `arcak-svc`.
Source: `TenantProjects` and `IncusProvider::{allows, writer, with_tenants}` in `crates/arca-providers/src/incus.rs`;
`ProjectFamily::with_tenants` in `crates/arca-incus/src/write.rs`; `Kernel::{open_with_tenants, refresh_tenants}` in
`crates/arca/src/kernel.rs`; `tenant_of_project`, `manifest_names_project` and `classify_object` in
`crates/arca-kernel/src/ownership.rs`. Tests `removing_a_tenant_takes_everything_of_it_in_order_and_leaves_the_others_as_they_were`
(a removed tenant's project is out of the family) in `crates/arca/tests/tenants/isolation.rs` and
`a_scratch_run_names_its_tenant_projects_under_the_scratch_prefix_and_joins_the_bridge_it_finds` in
`crates/arca/tests/tenants/admission.rs`.

**Rule 62.** A tenant owns its project and that project's `arca-service` profile, and nothing else. It makes no bridge and
no network: its project is granted the node's managed bridge, `restricted.networks.access=arcak-svcbr`, and no other. The
bridge is the network entry's, so this one grant is checked differently from Rule 41: the bridge must exist in `default` and
carry the mark `net-arcak-svc`. The project is restricted like every kernel project (Rule 30) and is created with these
keys, each at its tighter value, which the provider admits for a tenant's entry in its own project and for nothing else:
`restricted.devices.proxy=block`, `restricted.devices.gpu=block`, `restricted.devices.disk=managed`,
`restricted.devices.nic=managed`, `restricted.containers.nesting=block`, `restricted.containers.privilege=unprivileged`,
`restricted.containers.lowlevel=block` and `restricted.virtual-machines.lowlevel=block`. A scratch run never makes the
bridge: it joins the one the node has, or the create is refused.
Source: `Tenant::apply` and `RESTRICTIONS` in `crates/arca-netproject/src/tenant.rs`; `tenant_restriction`, `tenant_owns`
and `prepared_create` in `crates/arca-providers/src/incus.rs`. Test
`a_tenant_is_one_restricted_project_on_the_managed_bridge_with_its_caps_and_profile` in
`crates/arca/tests/tenants/admission.rs`.

**Rule 63.** A tenant's caps are its project's own `limits.instances`, `limits.containers`, `limits.virtual-machines`,
`limits.cpu`, `limits.memory` and `limits.disk`. They are set at the project's create and, when the entry's caps change,
by one journaled write of the project's whole config under the read's ETag (Rule 43). Incus enforces them. The kernel checks
them first, from its own records, so a refusal names the cap: a container counts its template's limits or the tenant
profile's (2 CPUs, 2GiB), a 16GiB root disk and its data volume's size; a virtual machine counts its limits and its root
disk. `kernel.tenants.capacity` bounds the sum of every tenant's caps, and
`kernel.tenants.maxTenants` their number. The node's virtual-machine caps (Rule 58) cover every tenant: the sum of the
tenants' virtual-machine caps and the node's own machines stays within the node's count, and a tenant's machine is checked
against the node's CPU, memory and disk caps with every other machine. Rules 56 and 59 apply to a tenant's machine unchanged,
with one difference: it joins the managed network only. A host bridge in a tenant is refused.
Source: `TenantCaps::limits` in `crates/arca-netproject/src/tenant.rs`; `caps_exceeded`, `admission_refusal` and
`reserved_vms` in `crates/arca/src/tenants.rs`; `caps_refusal` in `crates/arca/src/handlers.rs`. Tests
`a_tenants_caps_are_checked_before_incus_and_the_refusal_names_the_cap` and
`a_tenants_machine_sits_on_the_managed_network_and_the_nodes_machine_cap_covers_every_tenant` in
`crates/arca/tests/tenants/isolation.rs`, and `the_policys_limits_refuse_in_plain_words_that_name_the_limit` in
`crates/arca/tests/tenants/admission.rs`.

**Rule 64.** Tenants share the managed bridge, and each is kept to its own block of the service /24. A tenant holds a /29
(8 addresses), or a /28 when its `instances` cap is above 8; the entry records the block with its size. Blocks are taken
from the top of the /24 down, never inside the lowest /28 (the gateway's), never over another tenant's block, and never
over an address in use. A block holds an address for every instance its tenant may hold: the subnet's top block lacks
the broadcast address, so a tenant that may fill a whole block is not given it. Two limits of the shared bridge are
known and are not closed here. The bridge sets no DHCP range, so an instance of the node's own project that reserves no
address (a database or a consumer of the node) may be leased an address inside a tenant's block; a block is never laid
over a lease that exists, and such an instance keeps its own ACL, but the block then holds one address fewer than its
tenant may use. And the bridge's DNS answers forward and reverse lookups for every instance on it, so a tenant can
learn the instance names of other tenants (`<service>-<id>`) and of the node's own services, though it can reach none
of them. A DHCP range kept clear of the tenant blocks, and a per-tenant DNS view or instance names that carry no service
name, are later tasks. Every instance of a tenant takes a fixed address of the block, and the node's own address plan
skips every tenant's block. On the shared bridge the isolation is the NIC's: `security.mac_filtering` and
`security.ipv4_filtering` are on, so an instance cannot take another's address; a tenant's NIC also sets
`security.ipv6_filtering`, and since the bridge has no IPv6 address the NIC is given none, so Incus drops every IPv6
frame the instance sends and no tenant reaches another over the bridge's link-local addresses. That filter is the
second of two layers: the NIC's ACL rejects all IPv6 by itself, as a control run showed ("Assumptions to confirm"); `security.port_isolation` stays `false`,
since it would cut links inside a tenant; and the NIC's ACL rejects by default both ways. A tenant NIC's egress rejects the
service /24 outside its own block (the fewest CIDR ranges that cover it), and names DHCP (udp 67) and DNS (udp and tcp 53) on
the gateway as allowed; everything else is the egress of every service: the other private ranges and the host's addresses
rejected, the internet allowed. Ingress is a link's port (Rule 65) and, on a node that sets `kernel.tenants.front`, one
more rule: the address of that one service of `arcak-svc`, tcp, on every port the tenant's service declares. No other
service of the node's project is admitted. The rule follows the node: a pass (`sync_front`, every 30 seconds while the
node serves, and at once when the front is created or a deploy is confirmed) sets each tenant service's record to the
front's address as it is now, or to none when the node has no front or the service is provisional (Rule 67), and the
service's ACL is patched to match. A front that is recreated at another address is followed by every tenant service; a
pass over a node where nothing changed writes nothing.
Source: `Block`, `blocks`, `block_prefix` and `egress_rules` in `crates/arca-netproject/src/tenant.rs`; `ingress`,
`egress` and `Service::apply` in `crates/arca/src/service.rs`; `free_address` in `crates/arca/src/handlers.rs`. Tests
`addresses_come_from_each_tenants_block_and_the_nodes_own_plan_skips_them`,
`the_nodes_own_plan_never_hands_out_an_address_inside_a_tenants_block`,
`a_tenants_nic_reaches_its_own_block_and_the_internet_and_nothing_else_of_the_subnet` and
`the_front_reaches_every_declared_port_of_a_tenants_service_and_nothing_else_does` in
`crates/arca/tests/tenants/isolation.rs`, `the_front_reaches_only_ordinary_services_and_follows_a_front_that_is_made_again`
in `crates/arca/tests/tenants/deploys.rs` (`sync_front` in `crates/arca/src/tenants.rs`), and the unit tests of
`crates/arca-netproject/src/tenant.rs`.

**Rule 65.** A verb given `tenant` acts in that tenant's project and sees nothing outside it; a verb given none sees
nothing of any tenant. A tenant's service is the entry `t-<id>-service-<name>`, so two tenants and the node may each
hold one of a name. A database inside a tenant is a service too: the postgres service template, linked with
`service_connect`. The `postgres_*` verbs act in the node's own project only and refuse a `tenant` in those words,
before anything is written. Its Incus instance is named
`<name>-<id>`: Incus refuses two instances of one name on one bridge, whatever their projects, because the name is the
instance's DNS name there (a live host showed it). The instance verbs and `tenant_usage` name an instance by that Incus
name. A name on another side answers "not found", exactly as a name that does not exist. A link joins two services of one side only, and a removal revokes links
among the services of its own project only. Nothing in a tenant is published, imported or released, has a host port, or
runs a command through the exec seam (Rule 50): a tenant's log is its console log. The one request that does reach a
tenant's guest is the file that carries a virtual machine's own stored secret (Rule 68). `service_publish`,
`service_unpublish`, `service_import` and `service_release` take no `tenant`: the field is not in their schema. A
snapshot verb given `tenant` takes its source from that tenant's project, lists the snapshots taken from it, and deletes
only those; given none, it lists no tenant's snapshot, refuses a tenant's project named by `project`, and does not find
a tenant's snapshot. Inside a tenant,
`instance_start`, `instance_stop` and `instance_restart` need no approval, from a token whose tool allowlist names the
verb: a stop is reversible, and each action is journaled with the token and the tenant. Outside a tenant a stop and a
restart still run only from an approved proposal.
Source: `scoped_entries`, `scoped_rows` and `tenant_arg` in `crates/arca/src/tenants.rs`; `ServiceHandler::call` and
`PostgresHandler::call` (the refusal) in `crates/arca/src/handlers.rs`; `Kernel::remove_service_spent` in `crates/arca/src/kernel.rs`.
Tests `each_side_sees_and_changes_only_its_own_services`, `a_link_joins_two_services_of_one_tenant_and_opens_one_port`
and `the_database_verbs_refuse_a_tenant_and_name_the_service_route` in
`crates/arca/tests/tenants/isolation.rs`, `usage_reports_what_each_instance_is_allowed_and_the_instance_verbs_act_in_one_tenant`
there (`LifecycleHandler::call` in `crates/arca/src/lifecycle.rs`), and `snapshot_verbs_with_a_tenant_stay_inside_that_tenant`
in `crates/arca/tests/snapshots.rs`. `crates/arca/tests/services/golden.rs` holds the request log of the services
suite on a node without the key: no rig of it sends another request than before.

**Rule 66.** A removal deletes an instance, reads it back absent, and only then deletes the ACL its NIC names. A plugin
declares the class-A kinds its class-B objects use (`Plugin::retired_after_plan`; `arca.service` and `arca.postgres`
declare their ACL). While the entry has a class-B object left, the reconciler does not revert such an object at once, as
Rule 17 otherwise states: the approved plan's run retires it after every class-B object of the entry is deleted and each
reads back absent. One the run could not retire stays live in the journal and makes the run partial, and the next
removal pass, which finds no class-B object left, retires it; the provider checks the entry's mark on it as on every
retirement. A tenant's removal follows the same order for everything in it, then removes the tenant's own entry: its
secrets after its instances, then its profile, and its project last. It runs only from a proposal the owner approved, as
a `service_remove` in a tenant does.
Source: `Reconciler::{remove_entry, execute_plan, retire_held}` in `crates/arca-reconcile/src/lib.rs`;
`TenantHandler::remove` in `crates/arca/src/tenants.rs`. Tests `service_remove_deletes_the_instance_and_only_then_its_acl`,
`a_later_removal_sweeps_the_acl_an_earlier_removal_left_behind`,
`an_instance_only_delete_rejection_is_a_partial_refusal_and_keeps_secrets` and
`the_fake_refuses_to_delete_an_acl_a_nic_still_names_as_incus_does` in `crates/arca/tests/services.rs`, and
`removing_a_tenant_takes_everything_of_it_in_order_and_leaves_the_others_as_they_were` and
`a_removal_that_cannot_finish_says_what_remains_and_keeps_the_project` in `crates/arca/tests/tenants/isolation.rs`.

**Rule 67.** A create inside a tenant may name a deploy, and what it makes is provisional for a while. `service_create`
takes `deploy`, inside a tenant only. A link is not part of a deploy by itself: it goes when one of its two services
goes, as in any removal, so `service_connect` takes no `deploy` and refuses the field. The deploy is
recorded on the entry and in the deploy journal (`tenant-deploys.jsonl` in the state directory), with the token that made
the call. A deploy is opened by its first create and takes more creates from that token only. It is provisional until
that token calls `deploy_confirm`, or until 15 minutes after its first journal line, whichever comes first; the time is
read from the journal, so a restart does not reset it. While it is provisional, that token may call `deploy_rollback`,
which removes exactly the entries the deploy created, with no approval: each instance is deleted and read back absent
before its ACL (Rule 66), then its secrets and volumes, and a service's links among the tenant's services are revoked as
in any removal. The rollback is journaled with the deploy and the token. A provisional service has no front rule
(Rule 64), and `tenant_usage` flags it. After the confirm, or the 15 minutes, the objects are ordinary: a rollback is
refused, the front reaches them from the next pass, and a removal needs the owner's approval. `deploy_remove` is that
removal for a whole deploy: an action proposal whose digest covers the tenant and the deploy, and whose one approval
removes every entry the deploy created, in the same order, and nothing else. It is refused while the deploy is
provisional and for a deploy the journal does not hold. Another token can neither add to, confirm nor roll back a
deploy. Nothing outside a tenant names a deploy.
Source: `open_deploy`, `deploys`, `Deploy::provisional` and `TenantHandler::deploy` in `crates/arca/src/tenants.rs`. Tests
`a_rollback_inside_the_window_removes_exactly_the_deploys_own_things`,
`after_a_confirm_a_rollback_is_refused_and_the_services_are_ordinary`,
`the_fifteen_minutes_run_from_the_journal_and_a_restart_does_not_reset_them`,
`a_deploy_is_for_a_create_inside_a_tenant_only`,
`one_owner_approval_removes_everything_a_confirmed_deploy_created_and_nothing_else` and
`the_front_reaches_only_ordinary_services_and_follows_a_front_that_is_made_again` in
`crates/arca/tests/tenants/deploys.rs`.

## A service that may talk to the kernel

**Rule 69.** Only a container of the node's own service project may be given a way to the kernel's MCP, and the way
gives no authority. A template declares it with `kernelAccess: { "port" }`. The service's record then names the port and
the socket, and its instance is created with one device, `kernel`: a proxy that listens inside the instance
(`bind=instance`) on `127.0.0.1:<port>` and connects from the host to the node's services socket (`listen.services`).
No ACL rule and no NIC key changes for it, and the kernel's own unix socket is as it was: the service's NIC still
rejects the bridge gateway, as every service's does, and the device's traffic reaches no bridge and no host port.

The services socket serves the MCP routes alone, each behind a bearer token, on every node and under every config. The
approval page, whose routes need no token, is merged into the routers of the other listeners and never into this one,
so a device can only ever reach a listener that cannot serve the page. Three checks keep a device on that socket and on
no other. A create takes the device's path from `listen.services` and from nowhere else. The plugin refuses a record
that names kernel access in a tenant, in another project, on a virtual machine, on a port below 1024 or one of the
service's own, or to anything but a plain absolute socket path, whatever wrote the record. And `serve` refuses to start
while any service's record names a socket the config does not serve as `listen.services`.

A create is refused before any write when the call names a tenant, when the node sets no `listen.services`, and when it
does not run from a proposal the owner approved: a direct `service_create` of such a template is `NotApproved` for a
token of any scope. The proposal states the template's `kernelAccess`, so the owner's approval binds it and the card
says it in words; an approval given before the template declared it, or for another port, creates nothing. A tenant's
project blocks proxy devices as well (Rule 62). The socket answers 401 to a call without a token and holds a token to
its scope and tool list, as every listener does, so a process in the instance may do exactly what the token it holds
allows. The device is part of the instance: it is created with it, and removed with it (Rule 66). A record made again
from an instance's marks reads the device back, for an instance of the node's own project alone and only when the
device connects to this node's own services socket: a device made by hand for another path is in no record.

The socket is mode 0600 in a directory that only the server's user (or root) may write to, under directories nobody
else can rename, and it serves that user and uid 0. Incus's proxy helper runs as uid 0, which is why uid 0 is served:
that admits every process of the host running as uid 0, and the proxy helper for any instance whose device names the
path. Arca makes such a device for a service of the node's own project alone, a tenant's project blocks proxy devices
(Rule 62), and a device added by hand is the act of someone who administers Incus. No other local user reaches the
socket, and it has no port another process could take while the server is down. Whoever connects holds nothing without
a token: the GET routes are held to a token's scope and tool list as the verbs they read are.
Source: `KernelAccess`, `KernelLink`, `kernel_device`, `kernel_link_of`, `kernel_connect_ok`, `Service::apply` and
`validate_template` in `crates/arca/src/service.rs`; `ListenConfig::services`, `NodeConfig::kernel_access` and `check`
in `crates/arca/src/config.rs`; `kernel_access_for`, `kernel_access_refusal`, `enrich_service_create`,
`ActionExecutor::run`, `adopted_config` and the `service_create` and `service_status` arms of `ServiceHandler` in
`crates/arca/src/handlers.rs`; `kernel_access_words` in `crates/arca/src/action_proposal.rs`; `mcp`, `serve_get`,
`serve_services`, `services_dir_ok`, `services_path_ok` and `owner_admitted` in `crates/arca-mcp/src/http.rs`; `check_bridged_start` and the
`services_router` of `crates/arca/src/cmd_serve.rs`. Tests in `crates/arca/tests/tenants/kernel_access.rs`
(`a_service_with_kernel_access_carries_one_device_of_exactly_this_shape_and_no_acl_rule_for_it`,
`an_ordinary_template_gets_no_such_device_and_cannot_ask_for_one`,
`nothing_in_a_tenant_gets_kernel_access_and_no_verb_installs_a_template`,
`a_node_gives_kernel_access_only_through_its_services_socket`,
`the_device_connects_to_the_services_socket_on_a_node_that_also_serves_its_approval_page`,
`serve_refuses_to_start_when_a_service_with_kernel_access_would_be_left_on_another_socket`,
`a_create_states_the_access_and_the_owners_card_says_it_in_words`,
`a_template_that_gains_kernel_access_after_its_card_was_shown_is_not_created_by_that_approval`,
`the_access_survives_a_restart_and_goes_with_the_service`), in `crates/arca/tests/services/kernel_access.rs`
(`the_plugin_makes_the_kernel_access_device_from_the_record_alone`,
`the_plugin_refuses_a_record_that_gives_kernel_access_to_anything_else`,
`a_record_made_again_from_an_instance_reads_the_kernel_access_its_device_shows`), in `crates/arca-mcp/tests/listen.rs`
(`the_services_socket_gives_its_peer_only_what_the_token_gives_and_serves_no_approval_page`,
`serve_services_refuses_a_path_or_a_directory_another_user_could_take`) and in `crates/arca/tests/e2e_mcp.rs`
(`the_services_socket_of_a_node_that_serves_its_approval_page_serves_mcp_alone`,
`a_proxy_listener_without_an_approval_origin_serves_no_page_route`), and
`metrics_and_alerts_are_held_to_the_tokens_tool_list` in `crates/arca/tests/export_contract.rs`. The device's behaviour on a live Incus is under
"Assumptions to confirm".

## A service that may reach one private address

**Rule 70.** A private address is opened to a service by two keys together, and through that service's own ACL alone.
The node's config names it (`kernel.serviceDestinations`: a service of the node's own project by name, one IPv4 address
in a private or tailnet range outside the service subnet, one tcp port), and the owner approves the create: a
`service_create` of a named service runs only from an approved proposal whose arguments carry the config's list, so the
approval binds it and the card shows each address. No call, template or proposal states a destination of its own: what
a call states is only compared with the config. The record then carries `destinations`, and `egress_rules` builds the
NIC's egress array from the record: each address is cut out of the reject of the private ranges (Incus weighs a reject
above an allow whatever the order, so an allow alone opens nothing), one allow opens tcp to it on the named ports, and
the last allow, the internet's, names every address but the opened ones. Whatever else is sent to an opened address
then matches no rule and meets the NIC's default egress action, which every service NIC sets to reject: that closes
every IP protocol, the ones a rule cannot name among them, so only tcp to the address on its port is newly reachable.
Three rejects of the address's own (every other tcp port, udp, icmp) state the same again. The reject of the host's
own addresses is never cut, and the network and broadcast addresses of the host's networks are never opened either.
With no destination the array is the one every service had before, so no existing ACL is written.

Nothing else is written for it: no other Incus object, and nothing of the host (no firewall rule, route or sysctl).
The array reaches Incus by the ACL's create or by `IncusPatchAclField` on the entry's own ACL (Rule 47), as every
egress array does.

Four checks keep a destination where the two keys put it. The config check refuses a range, a name, an address outside
the four ranges or inside the service subnet, the two addresses of the tailnet range a node's own Tailscale and a
cloud's metadata service answer on, a port of 0, more than 4 entries for a service and 16 in all. The plugin
refuses a record that carries `destinations` in a tenant, in another project, on a host bridge, or with an address or
port the config check would refuse, whatever wrote the record; a tenant's rules take no destination at all. A create is
refused before any write when it does not run from an approved proposal (`NotApproved`), when what the proposal states
is not what the config names now, when the address is the node's own, and for a virtual machine on a host
bridge. And `serve` refuses to start while a record carries a destination the config does not name for that service.

Known limit: the record, not the config, is what the rules are built from once the service exists, so taking an entry
out of the config closes nothing. The address stays open on Incus until the service is removed, and `serve` does not
start meanwhile.

Source: `Destination`, `refusal`, `without`, `cut`, `other_ports` and `by_address` in
`crates/arca-netproject/src/destination.rs`, with `node_own` and `NEVER`; `egress_rules` in
`crates/arca-netproject/src/tenant.rs`; `host_networks` in `crates/arca-providers/src/incus.rs`;
`ServiceConfig::destinations`, `destinations_ok`, `egress` and `Service::apply` in `crates/arca/src/service.rs`;
`ServiceDestination`, `KernelConfig::destinations_of` and `check_service_destinations` in `crates/arca/src/config.rs`;
`destinations_for`, `own_address_refusal`, `destination_problems`, `destinations_refusal`, `enrich_service_create` and
the `service_create` and `service_status` arms of `ServiceHandler` in `crates/arca/src/handlers.rs`;
`destinations_words` in `crates/arca/src/action_proposal.rs`; `check_bridged_start` in `crates/arca/src/cmd_serve.rs`.
Tests in `crates/arca-netproject/src/tenant.rs` (`a_service_with_no_destination_has_the_rules_it_always_had`,
`a_destination_opens_one_address_on_one_tcp_port_and_nothing_else`, which runs a model of Incus's documented rule over
the rules before and after, `a_destination_at_the_end_of_a_range_or_beside_another_opens_only_itself`,
`an_address_of_the_host_or_one_that_is_no_destination_is_never_opened`,
`the_rules_of_a_destination_are_written_as_incus_reads_them`), in `crates/arca-netproject/src/destination.rs`, in
`crates/arca/src/config.rs` (`the_destinations_are_empty_by_default_and_refused_unless_every_rule_holds`), in
`crates/arca/tests/services/destinations.rs`
(`the_plugin_builds_the_rules_from_the_record_and_writes_them_again_when_it_changes`,
`the_plugin_refuses_a_record_that_gives_a_destination_to_anything_else`) and in
`crates/arca/tests/tenants/destinations.rs`
(`a_named_service_is_created_only_from_a_proposal_whose_card_shows_the_address`,
`a_config_that_changes_after_the_card_was_shown_is_not_created_by_that_approval`,
`nothing_in_a_tenant_gets_a_destination`,
`serve_refuses_to_start_when_the_config_loses_a_destination_a_record_carries`,
`an_address_of_the_nodes_own_is_never_a_destination`,
`the_ends_of_the_nodes_own_networks_are_the_nodes_own_too`,
`a_machine_on_a_host_bridge_is_not_created_with_a_destination`). What Incus and the host do with these rules on a live
node is under "Assumptions to confirm".

## A function

**Rule 71.** A function is a service record with one more key, `function`, and nothing of it is owned another way: its
ACL, its instance and its secrets are the entry's own, created, patched and retired by the rules above. Its instance
carries `user.arca.function = true` beside the kernel's entry mark. Keys under `user.arca.` are the other system's
(Rule 3), and `classify_object` classes a fixed list of them as the entry's own on a service's instance; this key is
one more on that list in the node's own service project, and nowhere else: on a tenant's instance it is another
owner's key, as before. Its environment is its definition's alone: each plain value the definition gives, and for
each secret that is set the reference to its stored value. A record with any other environment name is refused,
whatever wrote it, and a linked service's secret change never writes one. The plugin refuses a function's record that is anything but a
container of the node's own project on the managed service network, created stopped, with no port, no host listener,
no kernel access, and exactly one opened address for each of its links.

Source: `Function`, `record_ok` and `template` in `crates/arca/src/function.rs`; `Service::apply` and `instance_body`
in `crates/arca/src/service.rs`; `classify_object` in `crates/arca-kernel/src/ownership.rs`. Tests in
`crates/arca/tests/functions.rs`
(`a_function_is_created_only_from_an_approved_proposal_whose_card_names_what_it_reaches`,
`functions_and_services_share_one_name_space_and_each_verb_keeps_to_its_kind`).

**Rule 72.** A function reaches only what its definition names. `function_egress_rules` builds its NIC's egress array:
a service's array (Rule 70) without its last rule, the allow that lets everything else out, and with one allow for
each link: tcp to the linked service's address on the linked port. A service's packets reach their neighbours on the
service /24 through that last allow, so a function has none of that reach either. A link whose address is outside the
service /24, is one of the node's own, or names port 0 opens nothing, whatever a record says. The linked service's
side is the ordinary grant of a link, bound to the function's uuid and address.

Source: `function_egress_rules` in `crates/arca-netproject/src/tenant.rs`, with its test
`a_functions_rules_are_a_services_without_the_way_out_and_with_one_allow_for_each_link`; `egress` in
`crates/arca/src/service.rs`; `create_function` in `crates/arca/src/function_handlers.rs`. Tests in
`crates/arca/tests/functions.rs` (`a_private_destination_comes_from_the_nodes_config_and_is_on_the_card`).

**Rule 73.** A run is not an effect. `arca_incus::function_run` is a path beside the effect journal, as the state
writer of the instance verbs is: it reads one instance, refuses it unless it is a container that carries the function
mark and has the uuid it was read with, and then writes only three things of it: its root disk, which Incus rebuilds
from an image (the store's copy whose fingerprint the function's record took at its create, or, when that copy is
gone, the function's own image by digest on its registry, and no other source: never the image the instance itself
names); its `state` (a start, a stop); and its console log, which it clears. A rebuild
keeps the instance's config, devices and uuid, so nothing a journalled effect wrote is changed by it.
An instance that has never been started is not rebuilt: its root is the one its create made from the image and no
program has run in it, and Incus cannot start a container that was rebuilt before its first start. Whether it has
started is read on the instance (`volatile.last_state.power`, which Incus writes at the first start and keeps), and
before that first start the instance must say it was made from the image the function's record took. Whoever may
write the instance through Incus can remove that key or write into a stopped container's root; that right already
decides what the function runs, since the entry point and the devices are config a rebuild keeps.
It forces a stop when a run passes its time limit, and nothing reaches it with a service's instance: the instance
verbs and their writer still force nothing, and refuse a function's instance. (The kernel's own delete of an instance,
and the folder and adoption writers, force a stop too, each of its own object, as before.) The kernel starts a run only for the instance whose uuid the effect
journal holds for the function's entry. A run writes one line to the run log and the end of its output, both in the
state directory, and nothing to the desired state or the effect journal.

Source: `FunctionRun` and `is_function` in `crates/arca-incus/src/function_run.rs`, with the tests in
`crates/arca-incus/tests/function_run.rs` (`a_service_is_never_read_started_or_force_stopped_from_here`,
`a_function_outside_the_family_or_recreated_is_refused`,
`a_root_is_rebuilt_from_the_recorded_copy_and_from_the_registry_only_when_that_is_gone`,
`a_container_that_never_started_is_not_rebuilt_and_one_that_has_run_is`); `FunctionRunner` in `crates/arca/src/function_run.rs`;
`InstanceLifecycle::target` in `crates/arca/src/lifecycle.rs`. Tests in `crates/arca/tests/functions.rs`
(`ten_runs_write_ten_lines_and_nothing_to_the_journal_or_the_desired_state`,
`a_run_starts_only_the_instance_arca_created_for_its_function`,
`every_run_starts_from_its_images_own_root_or_does_not_start`).

`function_create` and `function_remove` run only from a proposal the owner approved; a removal is the removal of a
service (the instance is deleted before its ACL), after the run that is going has been stopped. A service a function
links to is not removed while the function exists: the removal is refused when it is proposed, when it runs, and by
`Kernel::remove_service_spent` itself, under the kernel's lock, on the entries it is about to change.

## The image of a database

**Rule 74.** No call chooses the image a database runs. `postgres_create` takes its image from the node alone, through
both doors, a token's own call and a proposal the owner approves. On a node whose config sets no
`kernel.postgresImage` the database is the service that `service_create` makes from the node's `postgres-autopg`
template: `postgres_create` calls that handler with the name, the template, and the image and the content of the
template as they were when the call was checked (Rule 75), and nothing else. So the image is fetched by registry and digest as every template's is (Rule 11),
the handler compares both with the template it reads for the create, and a token's own `postgres_create` makes
no more than its own `service_create` of that template makes: a token whose tool list leaves out `service_create`
makes no database this way. A proposal is given the template's name and the image it pins when it is made, by the
node and by no caller, so the card shows the image and the approval binds it; one that states another image is
refused when it is proposed, and the create refuses when the template names another by then. On a node whose config
sets `kernel.postgresImage` the database is of the old kind (`arca.postgres`), from that image and no other. An image
or a template a call states is only compared with the node's own, and a refusal never repeats what the caller stated.

The password of a database made from the template is the template's to generate, as every generated secret is: Arca
invents none of its own. The card says the password is generated only when the node read that from its template;
from a template that generates none the service is created stopped, the card carries the `Start` row a create's card
has, and the answer says why.

A record of the old kind is not touched by any of this: the `arca.postgres` plugin and its record are as they were,
so an existing database's entry, digest, instance and ACL are the same after an upgrade. A database's proposal binds
the content of its template as every create's does (Rule 75): the script that opens the database, its environment
and its ports are part of what the card's last row names.

Source: `database_source`, `database_image_changed`, `enrich_postgres_create`, `database_card_fact`,
`PostgresHandler::create_from_template` and `PostgresHandler::call` in `crates/arca/src/handlers.rs`; `ActionKind::prepare`, `database_service_words` and the `PostgresCreate` rows in
`crates/arca/src/action_proposal.rs`. Tests in `crates/arca/tests/tenants/databases.rs`
(`the_shipped_database_template_generates_its_password`,
`on_a_clean_node_a_proposed_database_is_the_templates_service_with_its_image_fetched_by_digest`,
`a_database_made_from_the_template_is_reached_and_removed_with_the_service_verbs`,
`a_template_changed_between_the_card_and_the_approval_refuses_the_create`,
`no_call_chooses_the_image_of_a_database_through_either_door`,
`a_node_whose_config_names_the_image_keeps_its_databases_and_the_old_way_as_they_were`,
`a_database_takes_a_name_nothing_holds_and_a_failed_create_leaves_no_password`,
`a_template_that_generates_no_password_makes_a_stopped_database_and_the_card_says_so`,
`a_token_whose_tool_list_leaves_out_service_create_makes_no_database_from_the_template`,
`what_a_database_was_checked_against_and_what_it_is_created_from_are_one_read_of_the_template`,
`a_create_the_service_handler_refuses_leaves_no_password_behind`,
`a_node_whose_config_sets_up_no_services_refuses_a_database_in_those_words`) and in
`crates/arca/src/action_proposal.rs`
(`a_proposed_database_names_the_configured_image_or_the_template_the_kernel_filled_in`). That a clean node's Incus
fetches the template's image by digest is under "Assumptions to confirm".

## What the owner saw is what is created

**Rule 75.** A create that the owner approves is made from the template the card showed, and from no other. When a
`service_create` or a `postgres_create` is proposed, the node gives the proposal two things it reads from the
template then, and no caller does: `image`, the image the template names (`<image>@sha256:<digest>` when it gives a
digest, the bare name when it gives none, and for a virtual machine that names a version, ` running <version>` after
its own system image), and `templateDigest`, the fingerprint of the template's content: SHA-256 over the bytes of its
file and of each script it names (its entrypoint's, a machine's first-boot script), each part framed by its name and
its length. The bytes are hashed as they stand, so any edit, a changed space among them, is another template. Both
are arguments, so the approval's digest binds them and a restart keeps them; a caller that states either is only
compared with the node's own, and refused when it differs.

The card ends with them: an `Image` row, which for an image with no digest says in words that Incus takes what that
name gives when the service is created, and last a `Template content` row with the fingerprint whole. The create reads
the template's file and its scripts once (`load_template_content`), compares what the call states with that read, and
builds the service from that same read: its entrypoint and its first-boot script are the ones that were hashed, so
nothing stands between the check and the create. A template that was edited, or installed again by an upgrade, since
the card was shown creates nothing, and the refusal says to propose it again. A card that names no
`templateDigest` (a create, or a database made from the node's template, recorded before the node bound it) is
refused when its approval is submitted, before the approval is checked, so the owner's approval is not spent on it
and nothing runs; the create refuses such a call too. A token's own create has no card
and binds nothing; one that states a `templateDigest` or an `image` is held to it the same way. A proposal for a
template that does not load, or whose script cannot be read, is refused when it is made, so no card is shown for it.

What a card says of its template beyond the arguments is read from that same read: the `Start` row of a create whose
template leaves it stopped, and a database card's words about its password. So the rows of one card describe one
template, the one its fingerprint names.

Not bound, because they are not the template's: why a service is created stopped (that is read from the stored
secrets when the create runs), the node's own config, and what stands behind an image name that has no digest. The
kernel access and the private destinations of a create are bound by their own arguments (Rules 69 and 70).

The binding covers the create and nothing after it. The verbs that act on a service later read the template as it
is installed then: `service_connect` reads its `provides` and the secrets it declares, the Omni verbs read it, and
`instance_start` writes a machine's secrets by its list. A template edited after a service was created changes what
those verbs do for that service.

Known limits. A handler built without the node's config fills nothing in: it records the `image` a caller states,
and its card shows it. `serve` always gives the config; only tests build such a handler. A proposal from it is
either unbound, and never approved, or held by the create's own compare, so nothing is created from it. On a
reload, the reason in a kept `Start` row is the journal's own text: whoever can write the node's private journal
can change those words and nothing else of a card, and what runs is decided by the arguments' digest.

Source: `TemplateContent`, `load_template_content`, `template_digest`, `template_image`, `entrypoint_with` and
`first_boot_with` in `crates/arca/src/service.rs`; `enrich_service_create`, `enrich_postgres_create`,
`database_source`, `database_card_fact`, `template_not_the_one_stated`, `template_not_bound` and the compare in
`service_create` in `crates/arca/src/handlers.rs`; `Kernel::approve_action` in `crates/arca/src/kernel.rs`;
`ActionKind::unbound_template`, `image_words`, `template_content_words` and the closing rows of
`ActionKind::details_with_gpu` in `crates/arca/src/action_proposal.rs`. Tests in
`crates/arca/tests/tenants/template_binding.rs`
(`a_proposed_create_is_given_its_templates_image_and_content_and_the_card_shows_both`,
`a_pinned_local_image_is_on_the_card_by_its_fingerprint_and_is_what_the_instance_is_made_from`,
`a_template_changed_after_the_card_was_shown_is_not_created`,
`a_card_that_waits_across_an_upgrade_of_the_shipped_templates_is_refused_and_proposed_again`,
`a_database_proposal_binds_the_content_of_its_template_too`, `the_binding_holds_across_a_restart_of_the_node`,
`a_card_that_bound_no_content_is_refused_before_the_approval_is_spent`,
`a_tokens_own_create_has_no_card_and_one_that_states_the_content_is_held_to_it`,
`a_create_that_could_not_be_made_from_its_template_is_refused_when_it_is_proposed`), in
`crates/arca/src/service.rs`
(`a_templates_fingerprint_is_of_its_bytes_as_they_stand_and_no_byte_passes_between_parts`,
`the_image_of_a_template_is_what_boots_and_then_the_version_a_machine_runs`) and in
`crates/arca/src/action_proposal.rs`
(`the_image_row_says_what_boots_what_its_name_leaves_open_and_what_a_machine_runs`).

## A registry's credential

**Rule 76.** A registry's credential is a secret of the node, not of an entry: its files stand in the secret store
under `registry-` and 32 hex digits of the SHA-256 of the registry's host and port, a name of one length that two
hosts do not share and no entry has, and no entry may use any secret whose name begins `registry-`. The directory
holds the address the credential was set for, and the credential is given only when that address equals the one
asked for, byte for byte in lower case: never to another host. Only the provider reads them, at
the moment of an instance's create in a project of the node's own, never a tenant's, and hands them to the writer
beside the create's body. The writer checks the body as it is, which still refuses a credential in a server address,
and writes the credential only into the address of the body it sends. The desired state, the effect journal (whose
digest is of the body without it), a proposal and a card never hold it. A create whose image is on a registry the
node holds a credential for states `registry`, that registry's address, which a proposal fills in and the card says in
words: without it the create is refused, and so is one that states it in a call with no approved proposal behind it. A function's rebuild from the registry is sent the credential the same way.
Incus keeps the address in the fetched image's record: the writer turns `auto_update` off on such images after a
create that was sent the credential, whatever the create answered, the narrow path does the same after a rebuild, and
a removal deletes a cached image that was fetched with a credential once no instance of the project was made from it.

A sweep (`ImageSweep`, at the start of `arca serve` and every ten minutes, in the node's own service project and no
other) is a third path beside the effect journal, as a run is. It writes two things, each of a cached image whose own
record holds a credential in its address, and of no other image: `auto_update`, which it turns off, and the image
itself, which it deletes when no instance of the project is made from it and it has seen it so for fifteen minutes.
These belong to no entry and have no inverse, so they are not effects: each is a line of `registry-images.jsonl` in
the state directory (the project, the fingerprint, what and why), as a run is a line of the run log.

Source: `RegistryCredential` and `create_with_registry` in `crates/arca-incus/src/write.rs`; `ImageSweep`, `SWEEP_EVERY_MS` and `ORPHAN_AGE_MS` in
`crates/arca/src/registry.rs`; `fetched_with_credential` in `crates/arca-incus/src/lifecycle.rs`; `registry_credential`
and `registry_owner` in `crates/arca-providers/src/incus.rs`; `crates/arca/src/registry.rs`; `with_registry` and
`drop_fetched_images` in `crates/arca/src/function_handlers.rs`; `FunctionRun::fresh_root` and `drop_unused_image`
in `crates/arca-incus/src/function_run.rs`. Tests in `crates/arca/tests/functions.rs`
(`an_image_on_a_registry_that_asks_is_fetched_with_the_nodes_credential_which_rests_in_incus_alone`,
`a_registrys_credential_never_reaches_another_host_or_a_functions_environment`,
`the_sweep_settles_only_images_fetched_with_a_credential_and_deletes_only_an_old_orphan`,
`a_sweep_round_goes_on_past_an_image_that_incus_will_not_change`),
`crates/arca/tests/tenants/isolation.rs` (`a_tenants_image_is_fetched_without_the_nodes_registry_credential`),
`crates/arca-providers/tests/incus_provider.rs`
(`a_registrys_credential_has_a_name_of_its_own_goes_to_its_own_host_and_is_no_entrys_to_name`) and
`crates/arca-incus/tests/function_run.rs`
(`a_registry_rebuild_carries_the_credential_and_only_an_unused_cached_image_is_deleted`,
`the_turn_off_after_a_rebuild_goes_on_past_an_image_that_incus_will_not_change`).

## Out of scope

nftables rules, Caddy routes, tunnels and DNS records carry no Incus mark, so this model does not cover them. Their removal is
a separate rule set for the providers that own them.

The retained set is a shared handle that a reconciler updates through a clone of it (Rule 36). A provider built with no set is not
possible.

## Assumptions to confirm

These hold in the fake and are the basis of the rules above. None has yet been checked against a live Incus, and each check
must run before any live write.

- A clean node's Incus fetches the `postgres-autopg` template's image from its registry by digest when a database is
  created, and the database then answers a connected service as the role `app` (Rule 74). The fake only records the
  source; this has not been run on a live node.
- Incus orders ACL rules by action and not by their place in the list (drop, then reject, then allow), takes a list of
  ports and port ranges (`1-442,444-65535`) in `destination_port`, and names the protocols `tcp`, `udp`, `icmp4` and
  `icmp6` (Rule 70). Read from
  [the Incus documentation of network ACLs](https://linuxcontainers.org/incus/docs/main/howto/network_acls/); the model
  the tests run is that reading, and so is that a packet no rule matches meets the NIC's
  `security.acls.default.egress.action`. Still to confirm on a live Incus 7.6: a bridged NIC takes a reject of up to a
  hundred ranges, an allow of up to some hundred and thirty, and a port list with ranges; a packet of another IP
  protocol to an opened address is refused by the default action; Incus reports each host address's prefix length as
  `netmask`; and a packet to a destination leaves the node with the node's own address on the interface it leaves by
  (the service bridge's `ipv4.nat`), through a host firewall Arca does not touch.
- A proxy device with `bind=instance`, `listen=tcp:127.0.0.1:<port>` and `connect=unix:<path>` on a container reaches
  a unix socket of mode 0600 on the host, and the host sees the peer as uid 0, which the services socket serves (Rule
  69): confirmed on a live node (Incus 7.6, SELinux enforcing). Still to confirm: the same on a container of the
  restricted service project (`restricted.devices.proxy=allow`) as Arca creates it, on an application (OCI)
  container, under AppArmor, and that the device is in place again after a restart of the host. The fake stores the
  device and models none of this.
- A virtual machine from a public cloud image (`images:debian/13/cloud`) that Arca creates runs its
  `cloud-init.user-data` once at its first boot and writes the file its `cloud-init.vendor-data` names, on a host whose
  QEMU has no virtio-9p (the config share is the `agent:config` drive of Rule 59), and its guest agent then takes a file
  request for `/etc/arca-secrets` with the mode and owner asked for (Rule 68). The fake models the agent only as ready or
  not ready.
- A file `GET` of a directory in a virtual machine's guest answers 200 with `X-Incus-type: directory` and the directory's
  `X-Incus-mode`, `X-Incus-uid` and `X-Incus-gid`, and 404 when nothing is at the path; a guest whose agent is not up
  answers neither (Rule 68). Read from `fileSFTPGet` in Incus 7.6's `cmd/incusd/instance_file.go`. A directory `POST`
  for a path that exists is answered with success there (`fileSFTPPost`); Arca no longer sends one, and the fake can
  refuse it to show that.
- An instance's config PATCH that names one `user.*` key leaves every other key, device and profile as it was, does not
  restart the instance, and is refused with 412 under a stale `If-Match` (Rule 52). The adoption's Mark step relies on the same
  behaviour for a stopped instance; the import sends it to a running one.
- The other manager keeps treating an instance as its own while it also carries `user.arcakernel.entry` (Rule 53). That is
  the other manager's behaviour, not Incus's, and only a run against it can show it.

- Incus checks `If-Match` only on PUT and PATCH, so a DELETE has no compare step, and the tombstone is the only one (Rule 18).
- A PATCH merges the `config` map, and an empty value unsets a key (Rule 16). Confirmed on Incus 7.3 for a profile, an ACL and a
  volume, whose tombstones landed beside their other keys, and for a network. A project's PATCH does not merge: it replaces the
  whole config, which a live host showed and the Incus source explains (Rule 43). The source of Incus 7.6.0 and of its main
  branch still has it.
- Incus checks a project PUT's `If-Match` against the project's description and config, the pair its GET's ETag covers, and a PUT
  replaces both (Rule 43). Read from the Incus source (`projectGet` and `projectPut` in `cmd/incusd/api_project.go`); a live
  probe must confirm the PUT lands with its tombstone.
- An instance's uuid is its `volatile.uuid` key, and no other kind has a uuid (Rule 20). Confirmed for a volume on Incus 7.3: a
  custom volume has no `volatile.uuid` and no uuid field.
- Incus answers sync to the project, network, profile, ACL, zone, forward and bucket writes, and to an instance's or a volume's
  PATCH. It answers an instance's or a volume's create and delete with an operation (Rule 14).
- The wait's answer carries the operation's `id`, `status_code` and `status`, and 200 with `Success` is the only success (Rule 14).
- A failed operation leaves the object as it was (Rule 14). One exception was seen on a live host: an instance create whose
  start fails leaves the instance, stopped (Rule 60).
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
- Incus 7.3 keeps the ACLs of a project that does not enable `features.networks` in `default`: an ACL created with
  `?project=<kernel project>` lands there, and a GET in the kernel project is answered from there (Rule 40). Confirmed on a live
  host, which superseded the earlier ruling that an ACL stays in its kernel project.
- Incus gives a zone its own project under `features.networks.zones`, whether or not `features.networks` is set (Rule 29).
- Incus 7.3 has `restricted.networks.access`, and a restricted project refuses an instance NIC on a network the grant does not
  list (Rule 41). The grant is the only key this rule adds to a kernel project.
- Incus refuses to delete a bridge that an instance still attaches to (Rule 17).
- Incus refuses to delete an ACL that a NIC still names. The fake refuses it too. A service's and a database's ACL is
  therefore retired after the instance that names it (Rule 66).
- Incus weighs an ACL's reject rule above its allow rules, whatever their order, and keeps its own rules for a bridge's DHCP
  and DNS ahead of a NIC's ACL. So a tenant's NIC, whose ACL rejects the bridge gateway like every service's, still gets its
  address and resolves names (Rule 64). Confirmed on Incus 7.5.1 by the scratch probe: a tenant's container and virtual
  machine took their reserved addresses by DHCP, and the container resolved a name and reached the internet, while the
  gateway, another tenant's instance and a service of the node's own project answered neither a ping nor a tcp connect.
- With `limits.cpu`, `limits.memory` and `limits.disk` set on a project, Incus requires each instance to state its CPU and
  memory limits and its root disk size, directly or through a profile, and counts them (Rule 63). A tenant's container states
  its root disk size for that reason. On Incus 7.5.1 the scratch probe created containers and a virtual machine under such
  caps, and Incus itself refused an instance past `limits.instances`.
- Incus refuses a second instance of one name on one bridge, in any project: the name is its DNS name on that network. Seen
  on Incus 7.5.1 by the scratch probe; the fake does not model it. A tenant's instance carries its tenant's id for that reason
  (Rule 65).
- The scratch probe (`crates/arca/tests/tenants/real_incus.rs`, ignored by default) also showed, on Incus 7.5.1: a link
  inside a tenant opens the provider's port to the consumer and to no instance of another tenant; a source address of
  another tenant's block, set by hand on a tenant's NIC, reaches nothing; and a removal leaves no project and no ACL
  under the scratch tenant prefix. It did not send traffic from the front service: it read the front's rule back from the
  ACL only.
- IPv6 between tenants on the shared bridge, shown by the scratch probe on Incus 7.5.1. Incus accepts
  `security.ipv6_filtering` on a tenant's NIC beside the MAC and IPv4 filters, on a bridge with no IPv6 address. Tenant
  B's container held a link-local address, and from tenant A neither a ping nor a TCP connect to it was answered. One
  control run took the filter off both NICs by hand and tried again: still no ping and no TCP connect, and A's neighbour
  discovery for B's address failed. So the NIC's ACL alone blocks IPv6 between tenants (its egress rejects `::/0`, and
  its ingress rejects by default), and the filter is a second layer (Rule 64). The control was run once, with
  containers; a virtual machine's NIC was not tried.
- The host's own addresses, shown by the same probe: from a tenant's container, none of the host's IPv4 addresses, as
  its interfaces held them at the run, answered a ping or accepted a TCP connect on port 22, which the host listened
  on (Rule 64). IPv4 only; the host's IPv6 addresses were not tried one by one, and all IPv6 is rejected by the ACL.
- Incus 7.3 creates a project with `features.images` set to `"true"` when the create does not name it, and an image in the project's
  own store keeps Incus from deleting the project. Both were seen on a live host, which superseded the earlier assumption that a
  kernel project reads another project's image store (Rules 30, 42).
- An instance whose source names the image server and `simplestreams` pulls its image into the store its project reads, as a cached
  image (Rules 11, 42). The probe's alias `alpine/edge` is on that server.
- An image listing in a project with its own store names only that project's images, each with its 64-hex fingerprint, and Incus
  answers an image's DELETE with an operation (Rule 42).
- Incus accepts `restricted` and the five `features.*` keys on a project create (Rule 30).
- Incus lets a restricted project's own config drop `limits.containers`, `limits.cpu`, `limits.memory` and `limits.disk`,
  set `restricted.devices.nic` and `restricted.devices.proxy` to `allow` and drop `restricted.networks.access` by a PUT while
  instances run in it, and none of those writes touches a running instance (Rule 55). Read from the Incus source
  (`NetworkAllowed`, `checkRestrictions`); the fake models it; a live write must confirm it.
- With `agent.nic_config` set, the Incus agent renames the guest interface whose MAC is a NIC device's to that device's
  `name` when it starts, and the guest's network setup then configures the renamed interface (Rule 59). Read from the Incus
  source (`writeNICDevConfig`, `osReconfigureNetworkInterfaces`); a live boot must confirm it, and that Incus 7.6 has the key.
- Incus accepts a NIC on a managed bridge with a reserved `ipv4.address`, MAC and IPv4 filtering and ACLs for a virtual
  machine as it does for a container, and a virtual machine with no profile (Rule 56). Not yet created on a live host.
- Incus accepts `ipv6.address` set to `none` on a bridge create (Rule 31).
- Incus accepts `ipv4.nat` on a bridge create, and reports a network's config with the keys that were set on it and no others.
  A key Incus adds on read would refuse a patch of that network, which fails closed (Rule 31).
- Incus accepts `features.networks.zones` on a project create and reports it in the project's config (Rule 30).
- Incus answers a GET of an object that is gone with 404, which is how Rule 39 reads an absent object.
- Incus lists every project's operations at `/1.0/operations?all-projects=true&recursion=1`, as a map keyed by each operation's
  status in lower case. An operation at work is pending (105), running (103) or cancelling (104). A resource URL carries
  `?project=<p>` unless the operation's project is `default`, and an instance's create names `/1.0/instances/<name>`. A wait whose
  `timeout` passes is answered with the operation still running, and an operation Incus no longer holds is 404 (Rule 44). Read from
  the Incus source (`operationsGet` and `operationWaitGet` in `cmd/incusd/operations.go`, `render` in
  `internal/server/operations/operations.go`, and `instancesPost`); a live probe must confirm it.

- Incus ACL PATCH merges only config and replaces both rule arrays and the description (Rule 47). See
  [`networkACLPut` and `netACL.Update`, v7.5.1](https://github.com/lxc/incus/blob/v7.5.1/cmd/incusd/network_acls.go#L585-L631) and
  [v7.6.0](https://github.com/lxc/incus/blob/v7.6.0/cmd/incusd/network_acls.go#L591-L637).
  An omitted config map cannot receive existing keys. Both versions' `common.Etag` includes name, description, both arrays
  and config (`internal/server/network/acl/driver_common.go:202-205`). The fake models replacement of the non-config fields;
  a live probe remains pending approval.
