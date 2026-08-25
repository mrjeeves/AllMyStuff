# Files mode: scale and data boundaries
> **Historical implementation note:** the filesystem-source-of-truth model below is superseded by [Fleet Filesystem and Canvas Architecture](fleet-filesystem-design.md). Its bounded-work rules remain requirements, but the canonical system uses a sharded fleet namespace catalog with lazy native mounts.


Files mode must remain useful on machines with tens of millions of files and
tens of terabytes of storage. The filesystem is the source of truth. Files mode
is not a crawler, catalog, backup index, or search engine.

## Invariants

| Surface | Reads | Persisted locally | Synced to the fleet |
| --- | --- | --- | --- |
| Folder view | One bounded page on demand | Bounded cursor/cache only | Nothing |
| Search box | Currently loaded page(s) | Nothing | Nothing |
| Preview | Selected file, with byte/type caps | Bounded disposable cache | Nothing |
| Recent | Explicitly opened items | Bounded device-local list | Nothing |
| Desktop canvas | Desktop directory, on demand | Explicit layout mutations | Opaque item reference, position, frame id |
| Frames | User-created geometry | Frame record | Frame id, geometry, nesting, appearance |
| Sharing map | Explicit grants only | Grant registry | Opaque shared-root/file id and target fleet |
| Folder share | One selected root | Opaque id to local path mapping | Opaque id, label, target fleet; never children |
| File share | One selected file | Opaque id to local path mapping | Opaque id, label, target fleet; never bytes/path |

Directory contents, absolute paths, thumbnails, sizes, timestamps, and file bytes
must never enter the fleet layout document. A folder share covers descendants at
authorization/read time; it never expands into one metadata record per child.

## Interaction budget

- Opening a directory performs bounded work and returns a page. It must not wait
  for the directory to be fully enumerated or globally sorted.
- At most a small number of directory cursors are retained. They expire and are
  discarded on refresh/navigation.
- The UI renders loaded pages only. Additional pages are pulled explicitly or by
  viewport proximity, never by a background crawl.
- Thumbnail and preview work starts only for visible/selected items, is size
  capped, and is cancellable.
- Pointer motion is local-only. One bounded mutation batch is emitted on drop.

## Layout documents

The current single LWW map is a migration source, not the final scaling unit.
Records are partitioned by semantic scope:

```text
fleet manifest
  desktop/<origin-device>     explicit desktop placements + frames
  sharing/<target-fleet>      explicit share frames + shared roots/files
  preferences/files           wallpaper token/preset and canvas preferences
```

Each partition has its own epoch, digest, record count, byte count, and tombstone
count. Peers exchange the small manifest first and request only mismatched
partitions. Patches contain the partition id and changed records. A tombstone
purge advances only the affected partition epoch, so compacting one busy desktop
does not invalidate the sharing map or every other device.

The UI subscribes only to the desktop and sharing partitions it is displaying.
Unknown partitions are persisted and forwarded by compatible nodes without being
materialized into the Files window.

## Identity and privacy

- Local filesystem identity is used when the OS provides a durable object id.
- Ambiguous directory entries (hard links, provider placeholders, replaced files)
  use an origin-scoped opaque fallback. No absolute path is embedded in synced ids.
- A synced item reference is meaningful only to the origin device. Other devices
  can preserve its layout but cannot derive or probe a local path from it.
- Shared roots/files use separately minted 128-bit ids. The source device alone
  resolves an id to a path, and every access re-checks the target fleet's live
  grant.

## Bounds and cleanup

- Enforce per-record, per-patch, per-partition record, per-partition byte, and
  manifest partition limits at the node boundary.
- Refuse new ids at a hard limit; never evict tombstones implicitly because that
  can resurrect deleted records.
- Mark unresolved layout references as orphaned. Cleanup is explicit and produces
  tombstones; it never infers deletion from an offline disk or temporarily missing
  provider.
- Owners/managers compact tombstones behind partition epoch barriers from Danger
  Zone. Offline peers converge by manifest/digest after reconnect.

## Failure behavior

| Condition | Behavior |
| --- | --- |
| Huge directory | Return the first bounded page quickly; show that more exists |
| Slow/offline volume | Cancel stale navigation; preserve the previous usable view |
| Directory changes between pages | Best-effort live view; refresh starts a new cursor |
| Missing removable disk | Preserve opaque layout/share records and mark unavailable |
| Renamed/moved item with durable OS id | Layout follows the object |
| Replaced item at the same path | New identity; old layout becomes orphaned |
| Concurrent frame edits | Per-entity LWW merge; normalize cycles deterministically |
| Share-frame drop fails | Revert optimistic placement; grant state remains authoritative |
| Peer has older schema | Ignore unknown partition messages; current records remain local |

## Revisit points

An optional content index can be a separate, explicitly enabled subsystem later.
It needs its own database, storage budget, inclusion roots, backpressure, and
retention UI. It must not be smuggled into the layout CRDT or enabled merely by
browsing a directory.
