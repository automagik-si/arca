# `instance_import`: a virtual machine from an outside hypervisor, moved into Incus

Status: K1 design, with the parts that need no cluster implemented and tested on fakes (this crate, the
`arca_incus::vm_import` writer, the `instance_import` and `instance_import_undo` verbs). Nothing here has run against a
real Ceph cluster or a real outside hypervisor yet.

## Summary

1. `instance_import` takes the outside hypervisor's own description of one VM (the `onevm show -x` XML that an
   operator read and passes in; the kernel never talks to the outside hypervisor) and makes ONE owner card.
2. The approved plan makes an Incus VM with the same CPUs, memory, firmware, disks, MACs and inbound rules, copies each
   disk from the source pool into the node's Incus Ceph pool by a full, sparse copy, and starts it.
3. Network and keys reach the guest through an Incus cloud-init seed built from the source's context CD.
4. Read-back after start: the guest agent, and the source's address answering on port 22. Undo
   (`instance_import_undo`): the Incus VM is stopped and removed; the source is never written.
5. Ceph grant asked: one new client, read-only on the source pool (`profile rbd-read-only`); the write into the Incus
   pool uses the Ceph identity Incus already uses for that pool. Powering the source off and on is the operator's step.

## Words

- **outside hypervisor**: the system that runs the VM today. Arca reads nothing from it.
- **source description**: the XML the outside hypervisor gives for one VM, passed to Arca as text.
- **source pool**: the Ceph pool that holds the source VM's disks. Incus does not manage it. It is on the Ceph cluster
  of the node's Incus Ceph pool, or on another Ceph cluster the node can reach (see "Two Ceph clusters").
- **target pool**: the node's Incus storage pool whose driver is `ceph`.

## What the action takes

```json
{
  "name": "web-1",
  "project": "people",
  "source": "<VM>…</VM>",
  "pool": "remote",
  "bridge": "br-lan",
  "cpu": 4,
  "memoryMib": 8192,
  "userData": true,
  "start": true
}
```

`name`, `project`, `source` and `pool` are required. `bridge` is required when the source has a NIC. `cpu` and
`memoryMib` replace the source's size (right-sizing is the owner's decision, made on the card). `userData: false` leaves
the source's user data out of the seed. `start: false` leaves the VM stopped after the copy, with no read-back.

The source description travels inside one MCP request, which the HTTP door caps at 64 KiB, so it may be at most 48 KiB:
Arca reads only `ID`, `NAME`, `STATE` and `TEMPLATE`, and the operator may leave out `HISTORY_RECORDS`, `MONITORING` and
`USER_TEMPLATE`.

The source description is parsed when the proposal is made, so a description Arca cannot read, or one it cannot move
(a disk that is not on Ceph, NICs on two bridges), is refused before the owner is asked. Before the proposal is made,
the text is cut down to what Arca reads (`source::reduce`): the elements the parser reads keep their text, every other
element is left out, and a context key the seed does not carry stays only as an empty element, so the card names it
but its value (`PASSWORD`, `CRYPTED_PASSWORD`, `START_SCRIPT`, …) never reaches the proposals file, the propose answer
or `plan_list`. The proposal binds that reduced text: the card shows what it says, and the approval covers exactly
that text. Reducing a reduced text gives the same text, so the run, which prepares the stored arguments again, binds
the same digest.

## The plan, step by step

The card names each step. Steps 2 to 6 run once, from the approved proposal.

1. **The operator powers the source off** in the outside hypervisor (power off or undeploy, never delete). This is a
   step on the card, not Arca's: the kernel has no access to the outside hypervisor.
2. **Arca checks the source disks are free.** For each disk it asks Ceph who watches the image (`rbd status`). A running
   VM's QEMU holds a watch on its disks; a watched disk stops the import before anything is created, with the words
   "power the source off first". A copy of a disk that a running guest writes is not a copy anyone can boot.
3. **Arca makes the Incus objects**, stopped, each carrying the mark `user.arcakernel.import` = the first 16 hex digits of
   the source description's sha256:
   - the network ACL `arca-import-<name>`, when the source has security-group rules (below);
   - one custom block volume `<name>-disk<N>` in the target pool for each disk after the first, the size of its
     source disk;
   - the VM, with `source: none` (an empty root of exactly the source's size, in MiB), `limits.cpu`, `limits.memory`,
     the firmware keys, the NICs, the disk devices and the cloud-init keys (below).
4. **Arca copies each disk** from the source pool into the Ceph image Incus made for it (below).
5. **Arca starts the VM** (unless `start: false`).
6. **Arca reads it back**: whether the Incus agent answers, and whether the source's address accepts a TCP connection
   on port 22, tried for up to five minutes. The answer says each in words.

If step 2, 3, 4 or 5 fails, Arca removes what it made (the VM, its volumes and its ACL, each only when it carries this
import's mark) and the answer says so. A failed read-back (step 6) leaves the VM running, so the operator can look at
its console; the answer says the read-back failed and that `instance_import_undo` removes it.

**Undo** is `instance_import_undo {project, name}`, its own owner card: the VM is stopped and deleted with its
volumes and its ACL, each only when it carries an import mark. The source is untouched by both actions, so the way back
is the operator powering the source on again in the outside hypervisor. That step is on the card as well.

## Size and firmware

| Source | Incus |
|---|---|
| `VCPU` (1 when absent) | `limits.cpu` |
| `MEMORY` (MiB) | `limits.memory` = `<n>MiB` |
| `MEMORY_MAX` (memory hot-plug ceiling) | not carried; the card names it |
| `OS/FIRMWARE` empty or `BIOS` | `security.csm: "true"`, `security.secureboot: "false"` |
| `OS/FIRMWARE` `UEFI` or a path to an OVMF file, `FIRMWARE_SECURE` not `YES` | `security.secureboot: "false"` |
| the same with `FIRMWARE_SECURE` `YES` | `security.secureboot: "true"` |
| `OS/MACHINE` (`pc-i440fx-*`, `pc-q35-*`) | Incus has one machine type (q35); an i440fx source is named on the card, since the guest sees new PCI addresses (its NIC names change; its network is matched by MAC, below) |

A UEFI source's firmware variables (its boot entries) are not carried: the guest boots from its disk's fallback loader
(`\EFI\BOOT\BOOTX64.EFI`), which the card says. Most distribution installs write it.

## Disks: how each is copied

Every source disk on Ceph (`TM_MAD` `ceph`) is copied. Its Ceph image is:

| Source disk | Image in the source pool |
|---|---|
| a persistent image (`CLONE` `NO`) | `SOURCE` itself |
| a non-persistent image (`CLONE` `YES`) | `SOURCE-<VMID>-<DISK_ID>`, a copy or a copy-on-write clone of `SOURCE` |
| a volatile disk (`TYPE` `fs` or `swap`, no image) | `<POOL_NAME>/one-sys-<VMID>-<DISK_ID>` |
| a CD-ROM (`TYPE` `CDROM`) | not copied; named on the card as not carried |

The first disk by boot order (`OS/BOOT` `disk<N>`, else the lowest `DISK_ID`) becomes the VM's root disk; every other
disk becomes a custom block volume attached as a disk device, in the source's order (`boot.priority` keeps the root
first). The bus follows `DEV_PREFIX`: `vd` is `io.bus: virtio-blk`, `sd` and `hd` keep Incus's `virtio-scsi`, so a guest
whose `fstab` names `/dev/vda` still finds it.

### Three ways to copy, compared

| | A. COW clone of a snapshot, then flatten | B. Full copy (chosen) | C. Snapshot copy while running, then a delta |
|---|---|---|---|
| How | `rbd snap create` + `rbd snap protect` on the source image, `rbd clone` into the target pool, `rbd flatten` later | read the whole source image and write it into the Incus image (`rbd export-diff` piped into `rbd import-diff`, which reads and writes only allocated extents) | B from a snapshot while the source runs, then power off and copy only what changed since (`export-diff --from-snap`) |
| Downtime | seconds | the copy of the disk's used data (the used size, not the provisioned size) | the delta only |
| Writes on the source pool | yes: a snapshot, its protection, a child record | none | yes: a snapshot |
| Ceph grant on the source pool | read and write | **read-only** | read and write |
| The new disk depends on the source pool | until the flatten ends | never | never |
| Fits Incus | no: Incus made its own image; the clone must replace it under Incus (`rbd rm` + `rbd mv`), outside Incus's knowledge | yes: Incus makes the image, Arca fills it | yes, the same as B |

B is chosen for K1: it needs nothing but read access to the source pool, leaves no link between the two pools, and
writes only into an image Incus made. Its cost is downtime: the used data of every disk is copied while the source is
off. A is the way to cut that to seconds later, at the price of a write grant on the source pool and a replaced Incus
image; C cuts it to the delta at the price of a source snapshot. Either is a later card of its own.

### A clone of a base image

Many source disks are copy-on-write clones of a shared base image. B reads through to the base
(`rbd export-diff` without `--from-snap` includes the parent's data), so the copy is flat: the new disk needs neither
the base nor the source pool. Read-only access to the source pool covers the base, which is in the same pool. The cost
is space: each moved clone holds its own copy of the base's data, instead of sharing it. Carrying the base once as an
Incus image and cloning from it would keep the sharing, but needs a diff of each clone against its base applied on the
Incus side, which neither Incus nor `rbd` does for an image Incus made; it is not worth it for disks whose used data is
tens of GiB.

### Where the copy lands

Arca asks Incus for the target pool's config (`ceph.osd.pool_name`, `ceph.cluster_name`, `ceph.user.name`) and names
the image Incus made by Incus's Ceph naming: `virtual-machine_<project>_<name>.block` for the root,
`custom_<project>_<volume>.block` for a volume (the project prefix is left out in the `default` project). Before writing,
it reads the image (`rbd info`) and refuses one whose size is not the source disk's size: a wrong name or a wrong size
stops the import before a byte is written. The P2 test on a throwaway pool checks these names on the node's Incus
version before the first real import.

The copy is two processes and a pipe, no shell:

```
rbd --cluster <source cluster> --id <source client> export-diff <source pool>/<image> -
  | rbd --cluster <ceph.cluster_name of the target pool> --id <ceph.user.name of the target pool> import-diff - <target osd pool>/<incus image>
```

### Two Ceph clusters

The source pool may be on another Ceph cluster than the target pool, in either direction: nothing in the import says
which cluster is which. `kernel.instanceImport.cephCluster` names the source's cluster by its config file on the node
(`/etc/ceph/<name>.conf`, with that cluster's `mon_host` and `fsid`), and the source client's keyring sits beside it
(`/etc/ceph/<name>.client.<client>.keyring`). The target pool's cluster is the one Incus names for it
(`ceph.cluster_name`, default `ceph`). Each `rbd` reads its own cluster's config and key, so the reader holds no key of
the target cluster and the writer none of the source's. The source cluster is only ever read: `status`, `info` and
`export-diff` to the pipe; a recorded-command test holds the run to that, in both directions and on a failed copy.

The copy streams through the node: the reader's output is the writer's input, a pipe the kernel never buffers, so the
node holds at most the pipe and each `rbd` process's own in-flight requests (by default ten objects of 4 MiB, tens of
MiB per process), whatever the disk's size. Its time is the data the disk holds (`export-diff` sends no unwritten
extent) over the slowest of the source cluster's read, the network between the node and the source cluster's OSDs, and
the target cluster's write. On 1 Gbit/s that is about 110 MB/s: about 10 seconds per GiB held, so 100 GiB in about 16
minutes. On 10 Gbit/s the clusters are usually the limit, a few hundred MB/s.

The card's `Ceph` row names both clusters, as the node's config and Incus say when the proposal is made, and the run's
answer says `Copied from the Ceph cluster <source> … to the Ceph cluster <target>`.

## Ceph access

- **Source pool: read-only.** A new client, by default `client.arca-import`, with
  `mon 'allow r' osd 'profile rbd-read-only pool=<source pool>'`. It covers `rbd status` (watchers), `rbd info`
  (size, parent), and `export-diff`, including a clone's parent in the same pool. It needs no snapshot: the source is
  off, so its head is stable. The mon side is `allow r`, not `profile rbd`: `profile rbd` also grants
  `osd blocklist add`, which librbd uses only to fence a dead holder of an image's exclusive lock. A read-only open
  never takes that lock, so `export-diff` never needs it, and a client with it could fence a running VM on the cluster.
  The P2 test on a throwaway pool confirms the three commands under exactly this grant.
- **Target pool: no new grant.** The write uses the Ceph identity Incus already uses for that pool (its
  `ceph.user.name`), whose keyring is already on the node.

The kernel's node config names the source pool, the client and the Ceph cluster (`kernel.instanceImport`, below). A
node without it refuses `instance_import` when the proposal is made, in words.

## NICs and inbound rules

Each source NIC becomes an Incus NIC with the same MAC (`hwaddr`) on the host bridge `bridge` names, in the source's
order (`eth0`, `eth1`, …). Arca reads the bridge from Incus:

- an Incus-managed bridge: `network: <bridge>`, and the ACL below on the NIC;
- a host bridge Incus does not manage: `nictype: bridged`, `parent: <bridge>`. Such a NIC cannot hold an Incus network
  ACL, so a source with security-group rules is refused there, before anything is made: dropping a source's inbound
  deny would open the VM.

The source's security groups are allow-lists, per direction: traffic that no rule of the NIC's groups accepts is dropped
(replies to accepted traffic pass). The VM template carries every rule of the groups its NICs name
(`SECURITY_GROUP_RULE`). They become one ACL, `arca-import-<name>` (one per NIC, `-eth<n>`, when several NICs name
groups), attached to the NIC that named them, with `security.acls.default.ingress.action: drop` and
`security.acls.default.egress.action: drop`; Incus ACLs are stateful as well. A source whose groups hold no outbound rule
therefore sends nothing out, as it did; the card says so.

| Source rule | ACL rule |
|---|---|
| `RULE_TYPE` `inbound` / `outbound` | `ingress` / `egress`, `action: allow` |
| `PROTOCOL` `TCP`, `UDP`, `ICMP`, `ICMPv6`, `ALL` | `tcp`, `udp`, `icmp4`, `icmp6`, no protocol |
| `RANGE` `22,8000:8100` | `destination_port` `22,8000-8100` |
| `IP` + `SIZE` | `source` (ingress) or `destination` (egress) as the range `<first>-<last>` |
| `ICMP_TYPE` | `icmp_type` |
| a rule that names only `NETWORK_ID`, or `PROTOCOL` `IPSEC` | not carried; the card lists it. Leaving it out drops what it allowed; carrying it as "any address" would open more than the source did |

## The cloud-init seed

The guests run cloud-init (or the outside hypervisor's own context tool) reading the context CD. That CD does not come
with the VM: without a seed, cloud-init on the next boot finds no datasource, falls back to DHCP on the first NIC, and the
guest loses its fixed address. So Arca gives Incus a seed, and Incus hands it to the guest as a NoCloud drive (the disk
device `cloud-init` with `source: cloud-init:config`).

- `cloud-init.network-config`: network config version 2. One ethernet per source NIC, matched by its MAC, with the
  context's `ETH<n>_IP`/`ETH<n>_MASK` as an address, `ETH<n>_GATEWAY` as the default route (first NIC that has one),
  `ETH<n>_DNS` and `ETH<n>_SEARCH_DOMAIN` as name servers, `ETH<n>_MTU`. IPv6 (`ETH<n>_IP6`, `ETH<n>_IP6_PREFIX_LENGTH`,
  `ETH<n>_IP6_GATEWAY`) the same way. A NIC whose context has no address gets `dhcp4: true`. Only when the context says
  `NETWORK` `YES`; otherwise the guest kept its own network config and no network config is given.
- `cloud-init.vendor-data`: a cloud-config with `ssh_authorized_keys` (each line of `SSH_PUBLIC_KEY`), `hostname`
  (`SET_HOSTNAME`, else `HOSTNAME`), and `ssh_deletekeys: false`, so the guest keeps its SSH host keys even though
  cloud-init sees a new instance.
- `cloud-init.user-data`: the source's `USER_DATA` verbatim (decoded first when `USERDATA_ENCODING` is `base64`),
  unless `userData: false`. Cloud-init treats the moved VM as a new instance, so per-instance modules of that user data
  (`runcmd`, users, packages) run again on the first boot: the card says so, with the line count, so the owner can choose
  `userData: false`.
- Not carried, and named on the card: `START_SCRIPT`/`START_SCRIPT_BASE64`, `PASSWORD`/`CRYPTED_PASSWORD`, `FILES_DS`
  and any other context key. They already ran on the guest's first boot.

The seed is text the kernel writes; no value from the description reaches it unquoted (every scalar is a quoted YAML
string).

## The kernel's side

- `crates/arca-vmimport`: parsing the description into a typed source, the plan (the Incus bodies, the copies, the
  seed, the ACL, the card rows), the disk copy trait with its `rbd` implementation and a fake, and the run with its undo.
  No I/O of its own except through those traits.
- `arca_incus::vm_import`: the Incus writer for an import. It sends: a network read, a storage-pool read, an ACL
  create, a custom volume create, an instance create with `source: none`, a start, an instance state read, a forced stop
  and the deletes. Every write after a create reads its object first and refuses one without the import mark, so no
  other instance, volume or ACL is ever changed.
- `instance_import` and `instance_import_undo`: doctor-scoped verbs that run only from an approved action proposal
  (`action_propose`), with an owner card each.
- Node config:

  ```json
  "kernel": { "instanceImport": { "sourcePool": "src-pool", "cephClient": "arca-import", "cephCluster": "ceph" } }
  ```

  `cephClient` defaults to `arca-import` and `cephCluster` to `ceph`. `cephCluster` is the source pool's cluster, which
  may be another than the Incus pool's. The kernel runs `rbd`, so the node needs `ceph-common`, the source cluster's
  config at `/etc/ceph/<cluster>.conf` and the client's keyring at `/etc/ceph/<cluster>.client.<client>.keyring`, both
  readable by the kernel.

The imported VM carries no `user.arcakernel.entry`: to the journal writer and to the instance verbs it is not Arca's
yet. Taking it into a project with a person (K3) and a person's start, stop and console (K4) are later steps.

## Not in K1

- A source disk that is not on Ceph.
- NICs on more than one bridge (K2 carries per-NIC bridges), and fixed addresses held by Incus (`ipv4.address`): the
  address lives in the guest's own config, from the seed.
- GPU or PCI passthrough, memory hot-plug, NUMA pinning, the source's snapshots (only the head is copied).
- The fast copies A and C above.

## Tests

The parser, the plan, the Incus bodies, the seed, the ACL and the card are tested on made-up descriptions: BIOS and
UEFI, one and two disks, a clone of a base image, a volatile disk, a CD-ROM, a security group, a description with no
NIC. The run is tested with a fake Incus and a fake disk copy, including each failure and its undo. The `rbd` copy is
tested by the exact argument lists it would run. The Incus writer is tested against `arca_incus::FakeIncus`.
