# AllMyStuff Fleet Computer Operation Promise

Status: binding product and engineering contract for the Fleet Filesystem implementation prototype.

This promise defines what AllMyStuff must do for the person operating a fleet and the conditions under which the work is considered complete. It is intentionally stricter than a feature description. A screen, protocol, or data structure that resembles this system but does not satisfy these guarantees is scaffolding, not completion.

The detailed namespace schema and canvas model live in [Fleet Filesystem and Canvas Architecture](fleet-filesystem-design.md). This promise is the higher-level contract those mechanisms must serve.

## The promise

AllMyStuff presents one personal computer whose storage and useful processing are distributed across the devices in a person's fleet.

The person does not administer a collection of synchronized computers. They allocate resources to AllMyStuff and choose policy. AllMyStuff decides where managed data and work should live, keeps the requested availability, repairs failures, and explains what it is doing.

Every member computer exposes exactly one real operating-system mount named Fleetfiles. That mount and the AllMyStuff Files UI address the same canonical namespace. The namespace contains one virtual Desktop. It is not a merged view of several native desktops and it is not grouped by source computer.

A device, disk, or native folder is a resource, import/export surface, failure domain, or interoperability adapter. It is not the organizing model presented to the person.

## Four pillars

Every design and implementation decision must preserve all four pillars.

| Pillar | Required outcome | Disallowed shortcut |
| --- | --- | --- |
| Performance | Browsing, metadata, and common opens feel local; data comes from the best usable replica; foreground work is prioritized. | Waiting for global scans, full-directory enumeration, or a remote coordinator for every interaction. |
| Efficiency | Work is incremental, deduplicated where safe, demand-driven, cancellable, and bounded in memory, disk, CPU, and traffic. | Fleet-wide polling, unbounded indexes, duplicate full copies without policy value, or synchronizing pointer motion. |
| Scalability | Tens of millions of files, tens of terabytes, many devices, and very large directories remain usable through paging, sharding, and bounded working sets. | A single layout document, one in-memory file table, or loading an entire directory/fleet before showing results. |
| Reliability | Committed data survives the failures allowed by policy; writes are verified and transactional; placement heals automatically and conflicts are visible. | Best-effort copy presented as durable, silent overwrite, split-brain guessing, or relying on a machine merely being online today. |

An optimization that improves one pillar by violating another is rejected unless the operator explicitly changes this promise.

## One computer, one namespace

The canonical user-visible hierarchy is the Fleet Filesystem.

- Fleet Home is the root experience in AllMyStuff.
- Desktop is one real logical directory in that namespace.
- Documents and other standard personal locations may be first-class fleet directories or saved anchors into the same namespace.
- Navigator is a conventional tree over this namespace, including the current location and ancestry.
- Recent, Shared, Available offline, and similar surfaces are bounded queries over the same objects. They do not create alternate hierarchies.
- Frames are nestable visual and policy groupings. They do not become directories merely because they contain icons.
- Files and folders retain one logical identity while their versions and replicas move among storage nodes.
- A machine's native filesystem remains reachable for import, export, migration, and recovery, but it does not appear as a peer root beside the Fleet Desktop.

There is no unified-desktop merge algorithm because there are not several managed desktops. Native desktops may be imported into or linked from the virtual Desktop under an explicit migration policy. The virtual Desktop itself is authoritative.

## Exactly one operating-system mount per member

Each capable fleet member must expose one mount for the complete Fleet Filesystem.

| Platform | User-visible result | Implementation requirement |
| --- | --- | --- |
| Windows | One stable drive or shell location named Fleetfiles | Reconnects without accumulating drive letters; Explorer operations map to fleet namespace transactions. |
| macOS | One stable Finder volume named Fleetfiles | Reconnects at a predictable mount point and retains Finder semantics where representable. |
| Linux | One stable filesystem mount named Fleetfiles | Uses a supported userspace filesystem adapter with explicit cache and offline behavior. |
| Mobile/constrained viewer | The same namespace in the app, without pretending an unavailable OS mount exists | Bounded metadata and content cache only. |

The mount is an adapter, not the authority. Restarting or remounting must not change object identity, create a second Desktop, or duplicate data.

Native file operations issued through the mount must use the same operation IDs, preconditions, conflict rules, policy checks, and Operations history as actions issued in the AllMyStuff UI. Explorer/Finder and the canvas must never race as two independent sources of truth.

## Managed storage pool

The Storage control panel is the person's single resource-management surface.

The person can:

- allocate an entire volume or a bounded quota on a volume;
- withdraw or reduce an allocation with an impact preview;
- identify removable, metered, battery-sensitive, slow, or always-on resources;
- set ordinary and critical-file durability;
- set minimum free-space/headroom rules;
- control offline availability, version retention, bandwidth windows, and energy preferences;
- view usable capacity after replication policy, not an inflated sum of raw disks;
- see health, degraded data, repair backlog, and the reason a resource is or is not eligible.

AllMyStuff must:

- place new content on eligible storage without asking the person to choose a machine for every file;
- maintain the configured number and diversity of verified replicas;
- avoid counting two replicas in the same failure domain as independent when policy requires diversity;
- rebalance gradually when capacity or fleet membership changes;
- repair missing, corrupt, stale, or withdrawn replicas automatically;
- preserve headroom and stop safely before a storage device is filled;
- verify content before advertising a replica as healthy;
- continue serving readable data during repair whenever any valid replica remains;
- expose manual overrides without making routine placement manual.

Raw native free space is not promised fleet capacity. The UI must distinguish raw allocated bytes, reserved headroom, physical stored bytes, deduplication savings, policy-adjusted usable capacity, and unavailable/degraded bytes.

## Distributed content

Managed file content is immutable by version.

- A logical file points to a current version.
- A version points to a bounded or paged content manifest.
- Content is split into verified chunks or another bounded immutable unit.
- Identical content may be deduplicated when encryption and privacy boundaries permit.
- Namespace listings never carry chunk lists or file bodies.
- A write is staged, hashed, replicated according to the commit policy, and then committed atomically into the namespace.
- An interrupted write cannot replace the last committed version.
- A partially received replica is hidden and does not count toward durability.
- Deletes create recoverable namespace/version state according to retention policy; later physical reclamation is separate and auditable.

For large folders or imports, AllMyStuff performs a bounded impact scan before commitment. It reports file count, directory count, logical bytes, unreadable entries, links, likely duration, available policy-adjusted capacity, and the expected network/storage effect. The scan is cancellable and does not silently follow links.

## Distributed processing

The fleet also provides a managed execution pool for work that supports the virtual computer.

Initial workload classes include:

- thumbnail and preview generation;
- hashing, chunking, encryption, and verification;
- search/index extraction where enabled;
- replica repair and rebalance;
- media conversion or other explicitly enabled personal workloads.

A workload has an idempotent job identity, bounded inputs, resource limits, priority, cancellation state, checkpoint/retry behavior, and an output commit rule. A worker cannot make an output visible merely by finishing locally; the coordinator verifies and commits it.

Interactive work takes priority over maintenance. Background work yields to device use, battery, thermals, bandwidth limits, and policy. Work stealing and retries must not cause duplicate visible results.

No device is permanently designated as the computer. Coordination and authority are replicated roles with explicit availability limits. A small fleet may co-locate every role; larger fleets may distribute metadata, content, indexing, and worker roles.

## Bounded fleet service profiles

Each member records a compact, time-decayed service profile for every other eligible member. The record exists to choose storage and workload candidates, not to monitor a person.

### Measurements

Measurements are derived from traffic and lifecycle events the system already needs whenever possible:

- observed online and offline intervals while the observer itself was running;
- connection establishment success and failure;
- request/response latency from real fleet operations;
- achieved transfer throughput;
- operation success, retry, timeout, corruption, and verification outcomes;
- available allocated capacity and headroom;
- removable, power, thermal, network-cost, and always-on characteristics exposed by that member;
- recent repair and workload completion behavior.

The system must not infer remote downtime during an interval in which the observing node itself was not operating. Unknown is not failure.

### Bounds and privacy

- Raw event histories are not retained indefinitely.
- Observations are aggregated into fixed-duration buckets and compacted into time-decayed summaries.
- Per-peer storage has a hard record and byte cap.
- Old buckets expire automatically without network coordination.
- Metrics stay inside the authenticated fleet and are fleet-encrypted at rest or represented as local observations.
- User activity content, filenames, file bodies, keystrokes, and application usage are not service-quality metrics.
- Aggregates are used for scheduling; they are not a hidden employee-monitoring surface.

### Candidate selection

Candidate selection first applies hard eligibility constraints:

1. authenticated fleet membership and compatible protocol;
2. resource allocation and sufficient headroom;
3. required failure-domain diversity;
4. online/reachable lease for immediate work, or explicit queued-offline eligibility;
5. power, metered-network, removable-media, and user policy;
6. content/key access and workload capability.

Eligible candidates are ranked using bounded aggregates such as availability, latency, throughput, verification reliability, capacity headroom, energy cost, and current load.

New or sparsely observed machines receive a conservative prior and an explicit low-confidence rating. They must not outrank a well-observed healthy member solely because they have no failures on record.

### Stability rules

Automated placement and scheduling must not flap.

- A current healthy placement remains unless a replacement is materially better, policy is violated, capacity is threatened, or the current member becomes unavailable.
- Scores use hysteresis, minimum residence time, and cooldown after failure or movement.
- Correlated metrics must not be counted as independent votes.
- A single transient latency spike cannot trigger migration.
- Repair restores policy before optional optimization/rebalancing.
- Background rebalancing has bounded concurrency, bandwidth, and daily movement budgets.
- Every decision records its hard constraints and decisive score factors in Operations.

No service-quality probe may become network yapping. Active probes are permitted only when an actual decision lacks fresh evidence, are rate-limited per peer, coalesced across waiting decisions, and back off after failure.

## Namespace, layout, and indexing bounds

The fleet namespace catalog is authoritative metadata. The canvas document is not.

- Objects, directory entries, versions, replicas, operations, grants, and native bindings use normalized, indexed tables or equivalent sharded records.
- Every directory listing is cursor-paged and version-aware.
- Very large directories split by stable ranges only when needed.
- Canvas layout stores only explicit placements, frames, background, and bounded presentation metadata.
- Unplaced objects receive deterministic virtual layout and create no layout record.
- Search, Recent, thumbnails, extracted metadata, and content indexes are disposable derived data with explicit budgets.
- Browsing a directory may adopt its visible page; it must not recursively index the tree.
- Watchers cover only active or policy-required scopes, use bounded leases, coalesce changes, and expire.
- A changed directory invalidates a page/range and refreshes subscribers. It does not broadcast a full tree.

The system must remain usable for a first customer with tens of millions of files and more than 40 TB. Benchmarks must include production-shaped name lengths, directory skew, tiny-file counts, large manifests, cold cache, offline nodes, and repair load.

## Visual and native interaction contract

The AllMyStuff Files UI is a first-class view of the Fleet Filesystem.

- The main canvas represents the virtual Desktop or current fleet directory.
- Initial desktop layout is column-major and uses host-native icon notches and spacing.
- Icon and label balance follows the host operating system closely.
- Thumbnails, native icons, shortcut overlays, hidden/system behavior, Recycle Bin/Trash, and native context menus are supplied by the platform adapter where applicable.
- Right-click moves an existing context menu to the newly targeted object; click-away closes it.
- Native context menus must be the real shell menu for native-representable selections, including Properties. A fallback is visibly identified and used only when the platform cannot provide the real menu.
- Inline rename, New Folder selected-name behavior, click-away acceptance, and multi-select follow host conventions.
- Frames have a drag handle distinct from title rename, support nested multi-select, and show live movement.
- Sidebars are resizable and hideable from controls inside the sidebars.
- The address bar is editable and navigates on Enter.
- Canvas zoom uses notched sizes, wheel zoom, and an explicit 100 percent reset.
- Background selection changes the actual canvas background and removes forced grid dots unless chosen.
- Folders and locations may open in additional AllMyStuff windows over the same namespace.
- Native mounts and the AllMyStuff UI remain mutually coherent through watchers and namespace transactions.

## Sharing contract

Sharing grants access to the same fleet objects. A share is not a fake file or folder.

- The sharing view shows the actual files and folders that are shared.
- A directory grant covers its descendants without materializing one grant per child.
- Recipient frames represent other fleets or people; the contained icons remain the shared objects.
- Dragging into a share frame previews and commits a real grant.
- Dragging out previews and commits a real revocation.
- Visual placement follows committed grant state and visibly reports pending or failed changes.
- Shared-with-me content enters the namespace through a grant-backed edge without losing issuer identity.
- Storage replication and access grants remain separate. Holding a replica does not grant access; receiving access does not imply a local replica.

## Operations promise

All meaningful asynchronous work appears in one quiet, persistent Operations surface.

The main Files toolbar shows one compact indicator with active, attention, and failed states. Opening it reveals:

- scans awaiting approval;
- writes, imports, exports, moves, and copies;
- replica placement, repair, verification, and rebalance;
- indexing/thumbnail jobs when material;
- share and revocation transactions;
- mount health and recovery;
- queued work waiting for a device, network, power, or policy condition.

Every operation reports:

- stable operation ID and idempotency scope;
- what the person requested;
- current phase and progress where knowable;
- affected logical objects and policy;
- source/target roles without leaking unnecessary native paths;
- why a candidate was selected;
- cancellation and rollback capability;
- retry state and next condition;
- final verification result;
- any residue requiring attention.

Cancellation is easy and phase-aware. Cancelling before commit removes staging. Cancelling after a namespace commit starts an explicit compensating operation; it does not pretend time reversed. Hidden staging is retained only when automatic rollback could not safely finish, and Operations explains recovery.

Completed history is bounded and compacted. Failures requiring attention are retained until resolved or explicitly dismissed. The system does not use a noisy footer, per-device status cards, repeated toasts, or polling counters as a substitute for Operations.

## Transaction and conflict rules

All mutations use stable operation IDs and preconditions.

| Event | Required behavior |
| --- | --- |
| Create or upload | Stage privately, verify, satisfy commit durability, reserve the name, then atomically publish. |
| Rename | Commit one logical directory-entry mutation; materialize native representations asynchronously where required. |
| Move | Change logical parent atomically. Copy bytes only if placement policy needs different replicas. |
| Copy | Create a new object identity that may initially reference the same immutable content. |
| Delete | Commit a tombstone and retention state; physical reclamation waits for policy and acknowledgement horizons. |
| Concurrent same-name creates | Keep both identities in a visible conflict state; never silently merge or overwrite. |
| Concurrent rename/move | Use preconditions and deterministic operation ordering; expose a conflict when intent cannot be preserved. |
| Offline mutation | Queue with its preconditions and label it uncommitted; reconcile or fail visibly on reconnect. |
| Replica corruption | Quarantine the replica, serve a verified copy, and repair from another failure domain. |
| Last verified replica endangered | Block destructive withdrawal when possible and demand an explicit, high-friction override with recovery impact. |
| Worker repeats after timeout | Deduplicate by job and output identity; at most one output becomes committed. |

Logical namespace commit, content durability, native materialization, and physical cleanup are distinct states. User-facing language must name the real state.

## Failure behavior

| Condition | User-visible and system behavior |
| --- | --- |
| One storage member disappears | Continue from other verified replicas; mark policy degraded; repair when an eligible destination exists. |
| A coordinator/metadata member disappears | Elect/use another authority when quorum permits; otherwise provide bounded cached reads and queue uncommitted changes honestly. |
| Network partition | Each side serves verified local data. Only the side with required authority may commit shared metadata; no split-brain success claims. |
| All replicas of a file are offline | Preserve namespace and availability state; queue open/download until a replica returns; never substitute same-named content. |
| Disk fills or crosses reserve | Stop new placement there, keep reads, and rebalance within budgets. |
| Corrupt content or hash mismatch | Never publish/count it; quarantine, record, and repair. |
| App or machine crashes mid-write | Last committed version remains; staging is recovered or collected safely. |
| Device leaves the fleet | Rehome required replicas before removal when possible; revoke its keys/authority; preserve audit state. |
| Metrics are stale or contradictory | Lower confidence, enforce hard constraints, prefer current healthy placement, and gather bounded evidence only when needed. |
| Operations history reaches its cap | Compact successful history; retain unresolved failures and policy violations. |
| Huge directory or file corpus | Return bounded pages and progressive results; no full-fleet blocking scan. |

## Security and privacy boundary

- Fleet identity and signed membership gate every namespace, storage, metric, and workload action.
- Fleet-root material derives separate keys for namespace metadata, content/manifests, canvas layout, service metrics, and grants.
- Venues may relay encrypted traffic but are not fleet members, storage replicas, metadata voters, or compute workers.
- A recipient receives narrowly scoped grant/content keys, never the fleet root key.
- A storage node may hold encrypted chunks without authority to browse the namespace.
- Native paths and opaque filesystem identifiers are disclosed only to adapters/nodes that require them.
- Remote commands are authorized again at execution time; possession of an old operation or route ID is insufficient.
- Removing a member prevents future decrypt/commit authority but does not claim to erase copies the person deliberately exported outside managed storage.

## Control-panel promise

The Files settings area contains one Storage section that answers, without requiring infrastructure expertise:

1. How much protected storage do I have?
2. How much is used, free, reserved, degraded, and pending repair?
3. Which devices/volumes have I allocated, and what is each contributing?
4. What durability and offline-availability policy is active?
5. Is my critical data currently protected?
6. What is AllMyStuff doing now, and why?
7. What will happen if I change or remove this allocation?

Advanced details may show physical bytes, deduplication, replica distribution, candidate confidence, throughput, and failure domains. The default view speaks in protected usable capacity and clear health.

A person must not need to select replica destinations, create drive mappings between their own machines, interpret mesh routes, or manually restart normal repair.

## Acceptance gates

The implemented prototype is not solid until it demonstrates all of the following on real fleet nodes.

### Correctness

- The same object created through Explorer/Finder and through the AllMyStuff UI has one fleet identity.
- Every member shows the same virtual Desktop after convergence.
- A remount/restart preserves namespace and object identities.
- Transaction interruption tests preserve the last committed version and hide partial data.
- Same-name, case, Unicode-normalization, link, shortcut, special-file, and cross-platform conflicts follow the explicit matrix.
- Replica verification detects injected corruption and heals from a valid copy.
- Cancellation works during scan, transfer, verification, and queued placement.

### Performance

- Warm metadata and directory navigation meet a documented local-interaction latency budget.
- Cold opens select a suitable replica without contacting every member.
- Large directories show the first bounded page without complete enumeration.
- Foreground work remains responsive while repair and indexing run.
- Mount operations do not serialize through a remote node when a valid local replica/cache can answer.

### Efficiency

- Idle steady state sends no periodic filesystem, canvas, or service-metric chatter.
- Watchers, cursors, subscriptions, active probes, jobs, staging, caches, and histories all have hard bounds and expiry.
- Pointer motion and frame dragging are local until one bounded commit.
- Candidate metrics reuse actual operations and lifecycle events; probes are exceptional and rate-limited.
- Repair/rebalance concurrency and bandwidth obey configured budgets.

### Scalability

- Synthetic tests cover at least tens of millions of namespace entries, large skewed directories, and more than 40 TB of represented content without loading it all.
- Catalog, manifest, and canvas sizes grow with their own sharded records, not as one fleet document.
- UI rendering is virtualized and proportional to visible/loaded items.
- Per-peer service-profile storage remains fixed by peer count and retention bounds.
- Offline convergence uses digests/ranges and does not replay the entire fleet on every reconnect.

### Reliability

- The configured replica policy is maintained across member restart, app restart, network partition, disk-full, corruption, and planned withdrawal tests.
- Placement avoids correlated failure domains according to policy.
- Metadata authority loss produces either quorum-safe continuation or an honest read-only/deferred state, never split-brain.
- Operations explains degraded state, selected repair destination, progress, cancellation, and final verification.
- Hysteresis tests prove noisy latency/availability samples do not cause placement or workload flapping.

### Cross-platform integration

- One stable mount named Fleetfiles appears on each supported desktop OS.
- The mount and AllMyStuff UI observe each other's changes through bounded live invalidation.
- Native context menus, Properties, icons, hidden/system items, shortcuts, Recycle Bin/Trash, inline rename, and New Folder behavior match the host where representable.
- Unsupported metadata or names are surfaced before lossy export; they are not silently rewritten.

## Decision ledger

The following decisions are settled unless this promise is deliberately amended:

1. AllMyStuff is a distributed personal computer surface, not a remote-drive organizer.
2. There is one canonical Fleet Filesystem.
3. There is one canonical virtual Desktop, not a merged collection of native desktops.
4. Every capable member receives exactly one real OS mount for the full Fleet Filesystem.
5. Storage devices are allocated resources; AllMyStuff owns routine placement, replication, repair, and availability.
6. Processing for filesystem services and enabled personal workloads is distributed across eligible fleet members.
7. Placement uses bounded, time-decayed, privacy-limited observations of real service quality.
8. Scheduling uses hard constraints before scoring and uses hysteresis/residence/cooldown to prevent flapping.
9. The namespace catalog, content manifests, replica state, operations, derived indexes, and canvas layout are separate bounded data planes.
10. The canvas never becomes the file table.
11. Browsing never implies recursive indexing.
12. Native mounts are the primary OS integration, while the Fleet namespace remains authoritative.
13. Operations is one quiet, persistent control and audit surface.
14. Shares grant access to actual fleet objects; shares are not filesystem objects themselves.
15. Desired durability and observed verified replicas are always represented separately.
16. Unknown availability or performance is not silently treated as success or failure.
17. Venues relay; they do not become trusted storage, compute, or metadata nodes.
18. The four pillars—performance, efficiency, scalability, and reliability—are release gates.

## Changing the promise

A later change must identify:

- which promise or settled decision changes;
- the user problem requiring it;
- the effect on all four pillars;
- security, privacy, conflict, and failure implications;
- migration and rollback behavior;
- updated acceptance tests.

Implementation convenience alone is not sufficient reason to weaken the promise.

