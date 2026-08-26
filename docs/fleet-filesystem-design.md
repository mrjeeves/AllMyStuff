# Fleet Filesystem and Canvas Architecture

Status: canonical product and implementation direction for the Files mode.

This architecture implements the higher-level [Fleet Computer Operation Promise](OPERATION-PROMISE.md). Where a local-filesystem prototype or implementation shortcut conflicts with that promise, the promise wins.

This document supersedes the local-filesystem-first assumptions in `FILES-CANVAS.md` and `files-scale-design.md`. Native filesystems remain important adapters and storage locations, but they are not the user-facing organizing model.

## Product invariant

AllMyStuff presents one fleet-wide filesystem.

A file or folder has one logical fleet identity even when its bytes, native bindings, replicas, shares, and canvas appearances span many machines. The infinite canvas is the primary filesystem browser and organizer. Devices describe where data is available; they are not separate top-level file browsers.

The core axes must stay independent:

| Axis | Meaning | Must not be confused with |
| --- | --- | --- |
| Logical identity | The fleet object a person names, opens, moves, and shares | A path on one device |
| Physical placement | Verified replicas or native bindings | Requested placement |
| Visual placement | Sparse canvas positions and frame membership | Directory containment |
| Access | Grants to fleets or people | Visual membership alone |

A feature that cannot identify which axis it changes is not ready to ship.

## User mental model

- There is one Fleet Home, not one home per computer.
- Folders are real namespace containers.
- Files and folders may exist on one or many devices without changing their identity.
- Frames are nestable visual and policy groupings drawn behind objects. They do not silently become folders.
- A frame may express a share relationship, a placement policy, or pure organization. Its type and consequences are visible.
- Sharing shows the actual files and folders being shared. A share is not a fake filesystem object.
- Dragging an object into or out of a share frame performs a real grant or revoke after an explicit, reversible preview.
- Quick Access becomes Navigator: saved anchors and queries into the same fleet namespace. It never exposes a parallel local hierarchy.
- Recent, Shared with me, Offline, On this device, and similar views are queries over the same objects.
- Native context menus, inline rename, multi-select, settable backgrounds, drag selection, and nested frames remain first-class interactions.

## Component model

```text
                         fleet key / authenticated mesh
                                      |
              +-----------------------+-----------------------+
              |                       |                       |
       namespace service       metadata/index service    transfer service
       object + directory      optional query shards     manifests + chunks
       pages + mutations       and recent activity       replica verification
              |                       |                       |
              +-----------------------+-----------------------+
                                      |
                               canvas service
                         sparse layout + frames only
                                      |
                  +-------------------+-------------------+
                  |                   |                   |
             Windows adapter     macOS adapter       Linux adapter
             paths, IDs, shell   paths, IDs, shell   paths, IDs, shell
```

These are logical services. A small fleet may run all roles on the same nodes. Large fleets distribute them.

## Data contracts

### The file-table decision

The fleet needs its own authoritative namespace catalog. Calling it a “file table” is directionally right, but one row per path is not sufficient. The minimum normalized catalog is:

| Table | Authority | Primary purpose |
| --- | --- | --- |
| `objects` | Fleet namespace | Stable identity, kind, current version, lifecycle |
| `directory_entries` | Fleet namespace | Parent, display name, ordering key, and the object reached by that name |
| `mounts` | Fleet namespace + adapter | Lazy boundary into a native filesystem |
| `native_bindings` | Observed by a device | Maps an entry/object to opaque native identity and current locator |
| `versions` | Fleet content plane | Immutable content manifest roots |
| `replicas` | Observed by storage nodes | Desired policy separate from verified presence |
| `namespace_ops` | Fleet metadata authority | Idempotent rename/move/create/delete transaction log |
| `canvas_appearances` | Canvas authority | Sparse explicit positions for directory entries |
| `frames` / `frame_members` | Canvas authority | Nestable visual/policy grouping |
| `grants` | Access authority | Subtree or object access; never expanded per descendant |

This is a catalog, not a content index:

- Every adopted filesystem entry gets a namespace record.
- File contents, extracted text, thumbnails, EXIF, and search tokens do not live in namespace rows.
- A lazy native mount can expose pages before its full descendant set has been adopted.
- A background catalog walk may eventually account for every native entry, but namespace correctness never waits for that crawl.
- Search and Recent are disposable derived indexes/logs, not authority.

`objects` and `directory_entries` must be separate. Most files have one of each, but the distinction handles native hard links, grant-backed mounts, rename/move/delete, several canvas appearances, shortcuts, and conflict recovery without inventing duplicate files or merging unrelated paths.

Canvas selection, inline rename, native menus, and drag/move target a directory entry. Content versions and replicas target an object. A grant targets an entry/subtree or object explicitly. Code must not accept an untyped generic “file ID” at these boundaries.

### Object

```text
object_id        random stable fleet ID
kind             file | directory | link | special
current_version  version ID, null for a directory
created_at
updated_at
tombstone        null or deletion record
attributes       bounded portable metadata
```

### DirectoryEntry

```text
entry_id         random stable fleet ID
parent_object_id containing logical directory
object_id        object reached by this entry
display_name     exact fleet-visible spelling
name_key         versioned exact comparison/order key
portable_collision_key  case/normalization-folded compatibility key
entry_kind       ordinary | mount | grant_mount | system
revision
tombstone
```

The live uniqueness constraint is `(parent_object_id, name_key)`. `name_key` is exact and case-sensitive after versioned fleet normalization, so valid source entries are never merged merely because another OS cannot represent them. `portable_collision_key` detects case-folding and normalization collisions before placement. Exact spelling remains in `display_name`; encrypted raw native spelling stays with the binding for lossless round trips.

### Mount

```text
mount_id
root_entry_id
device_id
adapter
native_root_ciphertext
adoption_cursor
state             online | offline | scanning | degraded
```

A mount is a lazy namespace boundary, not a device card. Its children become ordinary fleet entries as pages are observed. The device is placement metadata and may be replaced or replicated without changing the root entry identity.

### NativeBinding

```text
binding_id
entry_id
object_id
device_id
mount_id
adapter
opaque_native_id       filesystem-native identity when available
opaque_parent_id       native parent identity when available
local_path_ciphertext  fleet-encrypted, disclosed only where required
raw_native_name_ciphertext
binding_generation
observed_at
status                 present | missing | moved | conflicted
```

A path is a mutable locator, never an object ID. Matching by display name, path spelling, drive letter, case, Unicode normalization, shortcut target, or icon is forbidden. Native hard-link identity may associate multiple bindings and entries with one object; directory identity must not be inferred from path text.

### VersionManifest

```text
version_id
object_id
content_hash
size
chunking_version
chunk_refs[] or paged chunk-list root
portable_metadata
created_by
created_at
```

Large manifests are paged. Namespace listings never include chunk lists.

### ReplicaState

```text
version_id
device_id
desired_policy         optional
observed_state         absent | queued | transferring | verifying | verified | stale | failed
verified_at
last_error
```

Desired policy and observed fact are separate records. The UI may say “Keep on Studio PC” for policy and “Verified on Studio PC” only after verification.

### CanvasShard

```text
canvas_id
shard_key
records:
  entry_id -> explicit appearance position, z-order, optional visual overrides
  frame_id -> bounds, nesting, title, type, policy reference
background
revision / operation clock
```

Only explicitly placed objects have layout records. A directory with ten million children does not create ten million canvas records. Unplaced children use deterministic virtual layout until moved.

### Frame

```text
frame_id
canvas_id
parent_frame_id
type          visual | share | placement
bounds
title
policy_ref    null for visual frames
```

Containment is computed from explicit membership operations, not guessed continuously from rectangle overlap. Moving a frame moves its visual descendants without changing namespace parents.

### Grant

```text
grant_id
root_entry_id or object_id
recipient_fleet_id
rights
inheritance     subtree for directories, object-only for files
state           pending | active | revoking | revoked | failed
```

Directory grants cover descendants through namespace ancestry. Access evaluation must not materialize a grant row for every child.

## Adopting native files without crawling the world

AllMyStuff must work for fleets with tens of millions of files and tens of terabytes.

Native content is adopted lazily:

1. A configured native root becomes a namespace mount.
2. Its immediate directory page is enumerated only when viewed, queried, shared, or needed by policy.
3. Entries receive durable fleet object IDs and native bindings transactionally.
4. Descendants remain unresolved until touched. Directory summaries are approximate and asynchronous.
5. Filesystem journals or watchers update already-adopted bindings. They do not require a full fleet crawl.
6. Background indexing is opt-in, budgeted, resumable, and independent from namespace correctness.

This permits a unified filesystem without requiring a universal content index.

## Partitioning and scale

No single fleet-wide layout document, directory response, or manifest is allowed.

- Namespace: ordered, cursor-paged child records per parent and name-range shard.
- Objects: hash-sharded by object ID.
- Versions: hash-sharded manifests; chunk references use paged trees.
- Canvas: one manifest plus spatial shards per canvas; only explicit layout records.
- Frames: sharded with their canvas and separately indexed by policy target.
- Replicas: sharded by version and device; observed state is appendable and compactable.
- Search/index: optional derived shards with independent retention.
- Activity/Recent: bounded per-fleet and per-user logs, never embedded in object records.
- Tombstones: retained by policy, compacted only when all relevant replicas have acknowledged the deletion horizon.

### Local storage and distributed authority

Use a transactional embedded database (SQLite is the initial choice) as the local materialized catalog on each node. SQLite is an implementation detail, not the fleet sync format:

- Metadata authorities store complete assigned namespace shards in indexed tables and WAL mode.
- Ordinary desktop nodes may store full shards or bounded caches according to their role.
- Phones, viewers, and constrained nodes keep page/object caches plus their pending operations; they do not ingest the fleet catalog.
- Peers replicate versioned operations, snapshots, and cursor pages, never a live database file.
- A corrupted or lost projection can be rebuilt from a trusted shard snapshot plus committed operations.

The critical directory index is ordered by `(parent_object_id, name_key, entry_id)`. This makes stable cursor pagination and live-name uniqueness cheap. Secondary indexes cover object ID, native opaque ID within a mount, current version, tombstone horizon, and outstanding operations. Search has a separate derived store so query indexing cannot block a rename.

Ten million namespace entries are expected to occupy gigabytes, not terabytes: exact size depends heavily on name length, retained history, and index count and must be benchmarked with a production-shaped corpus. That footprint is appropriate for designated metadata nodes on a 40 TB fleet, but not for every client.

Each namespace shard has an explicit authority set and term:

- One durable voter is valid for a small fleet and makes its availability limitation visible.
- Three durable voters provide quorum fault tolerance.
- Two voters require both for a commit; the system does not invent split-brain availability.
- Non-authority nodes may read cached pages and queue preconditioned operations while offline, but may not present them as committed.
- Very large directories split by versioned name ranges. Ordinary directories remain one shard to avoid premature distributed transactions.
- A move spanning two authority shards is one idempotent operation coordinated by the source shard, with prepared destination reservation and recovery records on both sides.

Fleet-root keys derive separate encryption/authentication keys for namespace shards, canvas shards, manifests, and bindings. A recipient share receives a narrowly wrapped object/grant key; it never receives the owner's fleet root key. Venues relay ciphertext and do not hold catalog keys.

A node may serve one or more roles:

| Role | Small fleet | Large fleet |
| --- | --- | --- |
| Namespace/metadata | Every durable peer may replicate | A small durable quorum plus read caches |
| Content | Any storage device | Selective replicas and storage-class nodes |
| Index | Optional local index | Dedicated resumable indexers |
| Canvas | Metadata peers | Metadata quorum plus spatial caches |
| Venue/relay | Optional | Stateless routing; never namespace authority |

A venue services connections and encrypted traffic but does not become a fleet node, metadata voter, or trusted storage replica merely by relaying it.

## Operations and conflict behavior

Namespace mutations are idempotent operations with stable operation IDs.

```text
proposed -> authorized -> committed metadata -> materializing -> verified
                                      \-> failed/retryable
```

The logical mutation commits only after authorization and conflict checks. Native materialization may follow asynchronously when a backing adapter is involved. UI language reflects the exact state.

- Rename updates one directory entry. Native bindings materialize its name per adapter.
- Move changes a directory entry's `parent_object_id`; it does not copy bytes unless placement policy also changes.
- Copy creates a new object identity and initially may reference the same immutable version.
- Delete creates a tombstone; native deletion and replica cleanup are tracked separately.
- Restoring a tombstone creates a new namespace mutation while preserving object history.
- Concurrent rename uses operation ordering and exposes a conflict rather than guessing.
- Concurrent same-name creation yields a visible name conflict resolution; it never merges objects.
- Offline mutations queue with preconditions and fail visibly if those preconditions no longer hold.
- External native moves are matched only by opaque native identity or verified content evidence, never name alone.

## Sharing

Sharing is an access relationship over fleet objects.

The sharing lens renders the same files and folders as the normal canvas, with frames for recipient fleets. An object may appear in multiple share frames without being duplicated. A shared directory visually represents one subtree grant, not millions of child grant records.

Dragging into a share frame previews the recipient, rights, inherited descendants, and online/offline state. On confirmation it creates or updates a real grant. Dragging out previews and performs revocation. Visual membership changes only when the grant reaches the corresponding committed state; failures remain visible and retryable.

“Shared with me” objects keep their issuer-owned identity and are mounted through a grant-backed namespace edge. The recipient cannot confuse a remote grant with a local replica.

## Canvas and navigation behavior

The primary Files surface is one infinite Fleet Home canvas. Directory navigation focuses or enters a namespace region without changing to a device-specific browser.

Navigator may contain:

- Fleet Home
- saved folders
- saved canvas regions
- Recent
- Shared with me / Shared out
- On this device
- Available offline
- user-defined searches

These are anchors or queries. They do not own files and do not create a second hierarchy.

Device presence appears as object properties, placement filters, and optional placement frames. It is not a footer strip of computer cards. Storage and sharing lenses are overlays on the same objects, not three disconnected applications.

Native behavior is preserved at the edge:

- Right-click asks the host OS shell for the real menu for native-backed selections.
- Fleet-only commands are visibly separated and never replace expected OS commands.
- Inline rename and New Folder match host conventions.
- Multi-select applies across objects and frames with deterministic mixed-selection rules.
- Backgrounds are canvas metadata and synchronize fleet-wide.
- Native icons, hidden-file rules, shortcuts, and special objects come from the platform adapter.

## OS representation matrix

The fleet catalog keeps portable identity and OS representation separate. Adapter metadata is typed and versioned rather than poured into one unbounded attributes blob.

| Native concern | Catalog representation | Materialization rule |
| --- | --- | --- |
| Case sensitivity | Exact `name_key` plus case-folded `portable_collision_key` | Preserve both logical entries; block or explicitly rename only on an incompatible target |
| Unicode normalization | Exact normalized fleet spelling plus encrypted raw native name | Preserve display spelling; round-trip raw native form on its binding |
| Reserved names/characters | Adapter capability conflict | Do not silently sanitize; request a target spelling |
| Hard link | Multiple entries/bindings to one object | Preserve link relationship where supported; otherwise materialize independent copy with disclosure |
| Symlink/junction | Link object plus typed target, never followed for identity | Recreate only inside allowed scope; otherwise preserve as unsupported link |
| Windows `.lnk` / `.url` | Ordinary link-file object with adapter metadata | Shell icon and menu come from Windows; target never replaces object identity |
| macOS alias/package | Adapter-specific presentation metadata | Show native semantics on macOS; remain a file/directory elsewhere |
| Hidden/system flags | Binding attributes plus optional portable preference | Host display defaults apply; changing visibility does not rename or move |
| Recycle Bin / Trash / This PC | Adapter-owned special entry with capabilities | Never replicate as ordinary content or traverse as a path |
| ACL/owner/mode | Versioned adapter ACL attachment plus fleet grants | Preserve when target supports it; fleet sharing never pretends to be an OS ACL |
| ADS/resource forks/xattrs | Named side streams in the version manifest | Preserve supported streams; surface loss before incompatible export |
| Sparse/compressed/encrypted files | Logical byte version plus storage traits | Verification uses logical content; native traits are best-effort placement properties |
| Cloud placeholder | Binding with hydration/availability state | Opening may hydrate; “verified replica” requires locally verified bytes |
| Device/FIFO/socket | Special non-content object | Do not replicate bytes or invoke as a normal file |
| Open/locked file | Observed binding state and version snapshot boundary | Retry or snapshot through OS facilities; never claim a stable version from a torn read |
| Timestamp precision | Nanosecond-capable canonical value plus source precision | Do not invent precision or use timestamp equality as identity |

The four collision domains remain independent:

1. **Namespace collision:** two live entries want the same exact logical name in one parent. Resolve at the namespace operation; never merge objects.
2. **Adapter collision:** the logical namespace is valid, but a target OS cannot represent it. Keep the namespace committed and mark that binding unmaterialized.
3. **Version collision:** one object has concurrent content successors. Preserve both immutable versions until a user or policy resolves them.
4. **Canvas collision:** appearances overlap visually. Reflow or allow overlap according to canvas rules without changing names, identity, replicas, or grants.


## Edge and collision matrix
| Situation | Identity decision | UI/state decision | Collision avoided |
| --- | --- | --- | --- |
| Same name on two devices | Two entry and object IDs until explicitly reconciled | Show a namespace conflict in one parent | Accidental merge by path/name |
| Case-only rename across Windows/macOS | One entry; target adapter capability checked | Preview unsupported materialization | Oscillating renames |
| Unicode-normalization difference | Keep exact fleet names and raw binding names | Flag incompatible targets | Duplicate-looking entries |
| File moved externally | Match opaque native ID first | Update binding; keep object position | Delete/create flicker |
| Native ID unavailable | Use journal correlation plus verified evidence | Ask on ambiguity | Hash/name false match |
| Shortcut or URL file | Object remains link; target is metadata | Native icon/menu from adapter | Treating target as the object |
| Symlink/junction escapes a mount | Binding boundary validation | Mark inaccessible | Namespace/security escape |
| Device offline during move | Logical op with explicit precondition | Queued, not “done” | Intent shown as reality |
| Device reconnects with stale version | Version IDs differ | Conflict or policy-directed reconcile | Last-writer data loss |
| Entry appearance in nested visual frames | Explicit membership/order | One namespace identity | Geometry changing ownership |
| Object in two share frames | Separate grants to same object | One icon may have multiple share badges/lenses | Duplicate filesystem objects |
| Folder share with millions of children | One inherited subtree grant | Aggregate summary, lazy descendants | Grant explosion |
| Drag across share and placement frames | Distinct operation previews | Require an unambiguous target action | Share/replica policy collision |
| Delete while transfer active | Tombstone wins namespace; transfer canceled or quarantined | Show cleanup pending | Resurrected deleted file |
| Rename while menu/inline edit open elsewhere | Preconditions on revision | Resolve or reject stale edit | Silent overwrite |
| Background/layout edited offline | Canvas operations merge by record | Visible conflict only for same record | Whole-document LWW loss |
| Frame deletion with policy | Policy consequence preview | Keep objects; revoke/remove policy explicitly | Files deleted with decoration |
| Huge directory opened | Cursor pages and viewport virtualization | Progressive stable layout | Unbounded List response |
| Rapid presence changes | Debounced observed state with expiry | No success toasts for heartbeats | Flapping and network yapping |
| Relay/venue outage | No identity or authority change | Route degradation only | Venue mistaken for fleet owner |

## Traffic discipline

- Every listing and event stream is bounded, cursor-based, cancellable, and backpressured.
- Viewport demand drives directory pages, icons, thumbnails, and canvas shards.
- Presence uses leases with jittered renewal and hysteresis; no per-object chatter.
- Filesystem changes are journal-coalesced and batch acknowledged.
- Replica progress is rate-limited and summarized.
- Derived indexes compact independently and may be discarded/rebuilt.
- Retries use idempotency keys, exponential backoff, jitter, and per-peer budgets.
- Offline peers do not generate repeated user-visible failures.
- The system records counters for pages, bytes, retries, queue depth, stale leases, and verification latency.

## Existing foundations and missing contracts

Already present:

- Authenticated fleet routes and remote file operations.
- Remote folder browsing and file-manager surfaces.
- Opaque folder share IDs with path-boundary validation.
- Fleet-synchronized canvas metadata and native Windows shell integration.

Required before claiming a unified filesystem:

- Stable fleet object IDs and native binding records.
- Cursor-paged, cancellable directory enumeration.
- Namespace mutations with idempotency and preconditions.
- Version manifests and verified replica state.
- Sharded sparse canvas records and nested frame membership.
- Object-based subtree grants integrated with the namespace.
- Lazy adoption and bounded change-journal processing.
- Navigator queries over the namespace.

## Delivery slices

1. **Bounded identity surface:** extend file listing with optional opaque native IDs, pagination, cancellation, capability negotiation, and compatibility tests.
2. **Fleet namespace root:** persist fleet object/native binding records and expose a paged unified root. Origin device is metadata, not hierarchy.
3. **Real mutations:** route create, rename, move, and delete through idempotent namespace operations and materialize them through adapters.
4. **Content/version plane:** immutable manifests, chunk transfer, verification, and truthful placement state.
5. **Canvas sharding:** sparse spatial shards, nested frames, deterministic virtual placement, and fleet-wide background.
6. **Sharing lens:** grants on the same object IDs; drag-to-share/revoke with committed-state feedback.
7. **Derived navigation:** Recent, search, offline, and device filters as bounded queries.

Each slice must work end to end before its controls appear. Controls that only store intent without an executing and observing engine are not product features.

## Tradeoffs and revisit points

- A logical namespace adds metadata coordination but removes device/path coupling from the user model.
- Lazy adoption means untouched native descendants are not immediately searchable; this is preferable to mandatory crawling at fleet scale.
- Immutable versions simplify verification and deduplication but require explicit conflict handling.
- Native shell fidelity is strongest for native-backed selections; mixed fleet-only selections need a clear AllMyStuff command surface beside, not disguised as, OS commands.
- Metadata quorum size, chunking algorithm, case policy, tombstone horizon, and offline conflict UX require measurement and may evolve behind versioned contracts.

