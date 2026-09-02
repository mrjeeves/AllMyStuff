# Fleetfiles current implementation

Fleetfiles is the logical filesystem presented by Files mode. A path such as
**Desktop/Projects/plan.md** belongs to the fleet namespace; it does not
identify the computer or disk currently holding the bytes. The Navigator
starts at **Fleetfiles**, while **Local copies** is the explicit diagnostic
view for physical device paths.

This document describes behavior implemented in the current release. The
[Fleet Filesystem and Canvas Architecture](fleet-filesystem-design.md) remains
the product contract, and the
[Fleetfiles completion plan](FLEETFILES-COMPLETION-PLAN.md) tracks the later
identity, quorum, conflict, mount, and failure-injection gates.

## User behavior

- **Fleetfiles** is the normal navigation root. Its folders are read from the
  logical namespace database, independent of which device stores or currently
  materializes a body.
- **Local copies > This PC/device** shows only that computer's native
  Fleetfiles working tree. Entering it reveals the Devices tree and displays a
  **Local copies only** banner. Normal logical navigation does not highlight or
  automatically expand a device.
- The header search menu switches between **Search this Folder** and
  **Search Fleetfiles**. Fleet-wide search uses a disposable SQLite trigram
  path index and returns keyset-paged current logical entries; it neither walks
  device disks nor downloads file bodies.
- Navigator folders are cursor-paged. The main list continues loading bounded
  pages and virtualizes rendered rows/items, so a large folder is not truncated
  to one UI page or rendered into one enormous DOM.
- Recent items always use the smallest sidebar icon treatment.
- Opening a logical file uses a verified local allocation/cache body when
  possible. If necessary, the node requests that exact immutable version from
  an online fleet member, verifies its size and SHA-256 hash, caches it, and
  materializes the working copy. Browsing metadata alone does not download file
  bodies.
- **Version History** shows the append-only version ledger. Restoring an older
  file retrieves its body from the fleet when it is not local and writes it
  back through the working adapter as a new current version; clocks are never
  rewound and the replaced version remains in history.
- The Files mode button gains a storage-attention indicator when logical use
  approaches or exceeds protected usable fleet capacity.

## Data planes

    Files UI / native working root
                 |
                 | bounded logical pages and watched local mutations
                 v
     SQLite namespace + version ledger
          |                        |
          | metadata/history       | immutable content
          v                        v
     every fleet member      allocated content-v1 stores
          |                        |
          +---- presence-driven reconciliation ----+
                                   |
                             bounded cache-v1
                         for on-demand open/restore

The node persists these distinct responsibilities:

| State | Purpose |
|---|---|
| path_versions | Current logical winner or tombstone for each managed path |
| fleetfiles_path_search | Rebuildable trigram index over current logical paths |
| version_history | Append-only metadata for every observed version |
| metadata_history_queue | Exact per-peer metadata versions not yet acknowledged |
| content_version_queue | Exact per-target bodies not yet verified and acknowledged |
| replica_receipts | Observed successful durable placements; never inferred from policy |
| allocation content-v1/ | Verified immutable durable bodies under enabled storage budgets |
| working-root cache-v1/ | Verified, bounded on-demand bodies that do not count as replicas |
| working-root outbound-v1/ | Exact queued body snapshots retained until all target queues acknowledge |

The user-visible Fleetfiles root is a working materialization adapter, not the
namespace authority and not automatically a durable storage allocation.

## Replication and reconnect recovery

Local filesystem notifications and a bounded startup walk enter the same
capture path. A file mutation is hashed, appended to the logical version
ledger, snapshotted if an offline send needs stable bytes, and queued
independently for:

1. namespace/history delivery to every fleet member; and
2. content delivery only to selected enabled storage devices.

Queues are append-only by **(target, path, counter, actor)**. Reconnect drains
them in bounded batches until empty or until reachability/policy pauses the
work. A late older version is appended to history but cannot replace the
current winner. A historical body is stored immutably without overwriting the
current working file.

Upgraded peers advertise **fleetfiles-ledger-v1**. On peer presence/reconnect:

1. queued deltas drain first;
2. the peers compare cached SHA-256 digests of their append-only ledgers;
3. equal ledgers stop after the constant-size probe/reply;
4. divergent peers exchange keyset-paged, byte-bounded ledger records in the
   direction indicated by their entry counts; and
5. the digest is checked again, with at most three passes to cover concurrent
   writes.

This gives a newly joined or long-offline member the complete logical namespace
and version knowledge without a periodic crawler or database-file replication.
The feature advertisement prevents upgraded nodes from sending the new
anti-entropy protocol to older releases; their existing content queue continues
to drain, and retained metadata waits for upgrade.

## File history and space

History is on by default. The storage policy defaults to 30 days and can retain
more when allocated space permits.

- Current logical bodies are never history-reclamation candidates.
- Historical bodies older than the policy window yield first under allocation
  pressure.
- If a current write still cannot fit, the oldest unpinned historical bodies
  inside the window may yield.
- A body still needed by an outbound queue cannot be reclaimed.
- Version metadata remains after body reclamation, so history remains
  explainable even when a body has expired everywhere.
- A restore succeeds from any reachable peer that still holds the exact
  verified body. The hydrated cache is bounded to 128 bodies and 2 GiB, with a
  seven-day age target; the most recently requested body is kept even when one
  file exceeds that soft byte budget.

## Scale and network budgets

| Work | Bound |
|---|---|
| Logical directory API | 1–512 entries per keyset page |
| Fleet-wide search API | 1–512 current paths per keyset page; no result-count ceiling |
| History API | 1–256 versions per page |
| Ledger reconciliation page | At most 128 entries and approximately 32 KiB encoded metadata |
| Offline metadata acknowledgement | Up to 32 records per byte-bounded message |
| File transfer frame | 40 KiB raw bytes, below the data-channel envelope after base64 |
| Concurrent background Fleetfiles streams | 2 |
| Equal-ledger reconnect traffic | One probe/reply; regression-tested below 512 encoded bytes regardless of ledger size |
| Off-LAN metadata reconciliation | Paced at 512 KiB/s |
| Off-LAN background content | Aggregate paced by rebalanceGiBPerDay; default 50 GiB/day |
| LAN background content | Not artificially rate-limited; still chunked, queued, verified, and concurrency-bounded |

The daemon's nominated ICE pair supplies the link class. Only host-to-host is
treated as LAN; WAN and unknown links take the conservative gates. A
**rebalanceGiBPerDay** value of zero pauses off-LAN background bodies before a
receiver opens a staging transfer. User-requested open/restore is foreground
work and is not held to the background rebalance rate, but it fetches only the
selected immutable body and still uses bounded chunks and verified cache space.

There is no Fleetfiles timer or heartbeat. Network work begins with a local
mutation, an allocation/connection event, a user open/restore, or peer
presence/reconnect.

## Failure and integrity behavior

- Metadata and body messages are accepted only on the authenticated fleet
  network from an authorized fleet sender.
- Portable paths reject traversal, cross-platform reserved names, and
  oversized components.
- Partial bodies stay in hidden staging, never count as replicas, and are not
  materialized.
- A transfer commits only after declared size and SHA-256 both match.
- Existing materialized files are hash-checked against the current logical
  version before open; stale local bytes do not satisfy a logical open.
- Durable placement enforces allocation quota and reserve. Current data may
  reclaim history but never another current winner.
- Cache-only hydration is recorded separately and cannot inflate replica
  counts or protected-capacity claims.

## APIs

The node, Tauri bridge, and TypeScript client expose:

| Command | Result |
|---|---|
| fleetfiles_logical_list | One logical directory page |
| fleetfiles_logical_search | One indexed, keyset-paged logical search page |
| fleetfiles_version_history | One version-history page |
| fleetfiles_materialize | Verified local path, hydrating the current body if needed |
| fleetfiles_restore_version | Restored local path after exact-version hydration |

These commands operate on Fleetfiles-relative logical paths. Physical device
paths remain in the Local copies and remote-files adapters.

## Beyond this release

This release establishes a database-backed logical path/version system and
managed content plane. The completion tracker still requires stable
object/entry identities for rename and hard-link semantics, quorum-committed
namespace operations, visible concurrent-edit conflicts, acknowledged
tombstone horizons, deterministic repair/rebalance placement, canonical-object
sharing, and full multi-platform failure/scale gates before the complete
Operation Promise can be claimed.
