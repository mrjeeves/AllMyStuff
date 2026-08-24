# Files canvas

Files is the middle AllMyStuff mode: Normal remains the relationship-first view,
Files is an OS-familiar file workspace, and Advanced remains the full graph.

The feature deliberately separates two planes:

- The **data plane** is native and local. Directory entries, previews, file bytes,
  filesystem mutations, and OS launch/reveal operations never enter canvas sync.
- The **canvas control plane** is fleet-wide. File positions, frame geometry,
  frame names/colors, nesting, and tombstones are persisted by the node and
  converge over the authenticated closed fleet network.

That separation lets every fleet member retain the same map without turning a
folder browse or thumbnail load into network traffic. Camera state (pan/zoom),
selection, search text, and the session's Recent list are intentionally local UI
state; they do not describe the shared canvas.

## Files settings and retention

Settings has a desktop-only **Files** tab for the default Canvas/Details view,
thumbnail size, hidden-file visibility, and the preview sidebar. These
presentation preferences use this device's local storage; changing them emits no
fleet message. The tab also reports live canvas records, tombstones, and the
current document epoch.

Tombstones are normally permanent because they are what prevents an old offline
snapshot from resurrecting a deleted frame or placement. A signed fleet owner or
manager may explicitly compact them in **Danger Zone**. A purge:

1. removes the local tombstones and advances a Lamport-ordered document epoch;
2. broadcasts one epoch barrier followed by bounded chunks of live records; and
3. rejects every later patch or snapshot from an older epoch.

The epoch and records are captured/persisted atomically. If broadcast is
temporarily unavailable, the durable local epoch is replayed by the next
presence/digest exchange. Repeating an equal barrier is a no-op, and purging zero
tombstones does not advance the epoch or send anything.

This is intentionally a destructive fleet action: edits made by a device that
was offline before the purge are discarded when it reconnects. Pre-epoch builds
retain their own tombstones but cannot contribute new canvas edits to upgraded
peers after the first purge; the UI therefore says to update older devices first.

## Native behavior

The Files workspace uses the host filesystem and OS conventions rather than
inventing a second virtual filesystem:

- Quick access, Recent, and This PC/Locations live in the left sidebar.
- Thumbnail and Details views share the current native directory listing.
- Image thumbnails are lazy, bounded to 1 MiB per file, and loaded only near the
  viewport. The inspector loads supported image/text previews up to 4 MiB.
- Double-click navigates folders or asks the OS to open files. Context actions
  open, reveal in Explorer/Finder/Files, rename, copy the native path, or move to
  the OS Trash/Recycle Bin. There is no permanent-delete command.
- Frame creation is a draw gesture. Moving a frame moves nested frames and items;
  network persistence occurs once on pointer-up, in bounded chunks.

Reference patterns: [Apple sidebars](https://developer.apple.com/design/human-interface-guidelines/sidebars),
[Apple drag and drop](https://developer.apple.com/design/human-interface-guidelines/drag-and-drop),
[Apple toolbars](https://developer.apple.com/design/human-interface-guidelines/toolbars),
[Windows Explorer integration](https://learn.microsoft.com/en-us/windows/win32/shell/developing-with-windows-explorer),
[Windows controls](https://learn.microsoft.com/en-us/windows/apps/develop/ui/controls/), and
[Windows context menus](https://learn.microsoft.com/en-us/windows/apps/develop/ui/controls/menus-and-context-menus).

## Identity matrix

The canvas never matches a file by display name alone.

| Surface | Primary identity | Scope / normalization | Fallback and collision behavior |
|---|---|---|---|
| Local file or folder on Unix | filesystem device + inode | originating node + containing-directory id | Path is used only if metadata cannot provide identity; POSIX case is preserved. |
| Local file or folder on Windows | volume serial + 64-bit file index from an open handle | originating node + containing-directory id | Canonical path fallback is case-folded and hashed before entering fleet metadata. Case-only rename remains the same native object. |
| Symbolic link / reparse point | the link object's own identity, not its target | same as a local entry | Windows opens the reparse point itself. A path-scoped link fallback prevents two links to one target collapsing. |
| Hard links in one directory | shared native identity plus directory-entry name only when a duplicate is observed | originating node + containing-directory id | Both entries remain independently placeable. Renaming/removing one of a duplicate pair can change the disambiguator; this fails as a fresh placement rather than conflating two icons. |
| Directory canvas | native directory identity | originating node | Renaming the directory retains its frames; replacing a directory at the same path does not inherit the old layout. |
| Remote fleet file (future data-plane integration) | origin node + remote stable file id | authenticated fleet | Do not match a remote item to a local path or same-named file. Until the remote protocol carries a stable id, use origin + remote path and label the reference as path-backed. |
| Shared file grant | canonical uploader/source node + grant/file token | sharing relationship | Never infer identity from label, recipient-local mount point, or downloaded filename. |
| Frame | random UUID under map + directory scope | fleet document | Frame title is mutable metadata, never identity. |

## Convergence and traffic

Each canvas entity is an independent last-writer-wins register stamped with a
Lamport counter and the originating node id. The document epoch uses the same
ordering. Actor id breaks equal-counter ties,
so every peer chooses the same winner. Unrelated offline edits do not overwrite
one another. Deletion creates a tombstone; tombstones are not trimmed to make
space because an old offline snapshot could otherwise resurrect deleted state.

Local gestures emit no patch during pointer motion. Pointer-up persists a bounded
batch, and the node fans it over `allmystuff/files-canvas/v1` only on the closed
fleet network. A first sighting or new boot gets a chunked snapshot. A same-boot
transport reconnect exchanges a tiny deterministic digest; a full snapshot is
sent only when the documents differ. There is no canvas timer, directory-listing
gossip, thumbnail gossip, acknowledgement storm, or echo of merged records.

The document is persisted atomically as `allmystuff-files-canvas.json`. Records
are capped at 20,000, values at 8 KiB, local apply calls at 512 mutations, and
wire chunks at 16 records. At the record cap, updates to known ids still converge
but new ids fail closed.

## Edge-case and collision matrix

| Case | Expected result | Chatter / security rule | Collision check |
|---|---|---|---|
| Drag an item or frame | Optimistic local motion; one logical save on pointer-up | No pointer-move messages; large nested moves use bounded chunks | A remote update arriving mid-drag may be superseded by the final local gesture for that entity only. |
| Draw a frame around items/frames | Captured top-level entities become children without flattening their existing descendants | One pointer-up batch | A frame cannot parent itself or any descendant. Tightest containing frame wins. |
| Move a parent frame | Descendant frames and currently scoped child items move together | No continuous network traffic | The batch limit and UI chunk size align; a large hierarchy cannot create an oversized wire message. |
| Rename a file | Native id retains placement | No canvas patch is needed | Existing target names fail closed. Windows case-only rename is allowed; Unix remains case-sensitive. |
| Move within a filesystem | Native id remains stable; entering another directory uses that directory's canvas | No automatic path-based match | A same-named file at the destination never inherits placement. |
| Move across filesystems / copy-replace | New native identity, therefore a fresh placement | No guessing or background scan | Prevents replacement-at-same-path from inheriting stale metadata. |
| Symlink to a file already shown | Link and target are separate icons | Preview/open may follow OS behavior; identity does not | Two links to the same target do not collapse. |
| Hard links | Separate icons only when both entries collide in one listing | Local disambiguation only | The rare rename instability is preferable to merging two visible entries. |
| Hidden file toggled | Listing visibility changes; placement remains | Local-only preference | Re-showing the same native object restores its placement. |
| File deleted outside AllMyStuff | Entry disappears; its layout record remains dormant | No filesystem watcher or deletion gossip | A different replacement id at the same path does not reuse it. Record GC needs an explicit fleet-safe retirement design. |
| Trash from the context menu | OS Trash/Recycle Bin operation | Local filesystem operation only | Pre-existing target/race errors surface; no irreversible unlink fallback. |
| Two offline peers edit different entities | Both edits survive merge | Digest detects missed partition traffic | Per-entity records avoid whole-document last-write collisions. |
| Two offline peers edit the same entity | Higher `(counter, actor)` stamp wins everywhere | Duplicate delivery is idempotent | Deterministic actor tie-break avoids split-brain layout. |
| A device edits an entity carrying another actor's much higher counter | The new local stamp advances above the highest counter observed anywhere in the document | No extra message; the normal one-shot patch is sufficient | The edit cannot look successful locally and then be rejected by every peer as older. |
| Two offline peers create opposite frame-parent links | The lexicographically smallest frame in the merged cycle becomes a root | Repair is deterministic and local; the next edit persists it | Local cycle prevention alone is insufficient, so merged hierarchies are normalized too. |
| Offline peer carries a pre-delete snapshot | Tombstone wins and remains retained | Snapshot chunks may repeat safely | Tombstone is never evicted merely to admit new state. |
| Owner/manager purges tombstones | Tombstones disappear behind a strictly newer epoch; live records remain | One barrier plus bounded live snapshot; no message when the count is zero | Every pre-purge patch is rejected, including a stale live record whose tombstone was removed. |
| Ordinary member attempts an epoch advance | Drop it without changing the document | Sender must match an owner/manager entry in signed governance state after canonical transport-suffix removal | Being on the closed fleet channel is necessary but not sufficient to compact history. |
| Delayed duplicate purge barrier | No-op | Equal/older barriers return before governance lookup or broadcast | A duplicate cannot erase an edit authored after the purge. |
| Purge broadcast fails while fleet is offline | Local purge remains durable and the UI reports success | No retry timer; the next event-driven digest exchange replays barrier + snapshot | Avoids both an ambiguous rollback and background network yapping. |
| Offline old-version peer reconnects after purge | Its epoch-zero canvas messages are ignored by upgraded peers | New peers may still send compatible live snapshot records, but never accept pre-epoch writes | Stale state cannot resurrect; user is warned to update old devices before purging. |
| Two owners purge concurrently | Higher `(counter, actor)` epoch wins everywhere | Each purge is a bounded one-shot operation | The losing epoch and any edits scoped to it cannot overwrite the winner; no split-brain barrier. |
| Purge races a snapshot read | Snapshot carries the matching epoch and record set from one lock | No mixed pre/post-purge payload | Old records are never mislabeled with the new epoch. |
| Same process reconnects after a partition | Digest mismatch triggers bidirectional snapshot exchange | One small probe per presence event; no timer | Equal documents generate no response. |
| New/restarted fleet node | Current document arrives in bounded chunks | Only authenticated fleet members may send/receive | Older builds ignore the unknown channel; new builds retain state for later peers. |
| Patch arrives on public/person/CEC network | Drop it | Arrival network and signed fleet roster are both required | A familiar node id on the wrong network is insufficient. |
| Malformed, oversized, or unknown record | Drop invalid record and keep current document | Bounded ids/kinds/values/counts | A rejected local batch is prevalidated before any record changes. |
| Duplicate delivery over multiple paths | Merge is a no-op after the first copy | No echo broadcast on inbound merge | Stamp comparison prevents repeated side effects. |
| Device leaves the fleet | Its existing authored layout remains ordinary document history | It can no longer send on/authenticate to the fleet channel | Purging authorship requires an explicit product action, not an implicit disconnect. |
| Fleet has two files with the same name/path shape | They remain distinct by origin + native identity | Labels are presentation only | No cross-device basename or normalized-path join. |

## Sharing map boundary

The Sharing map is a second presentation of the same fleet-wide canvas system.
It currently arranges the existing Personally stored, Shared with me, and Shared
out relationships plus user frames. It does not claim that file bytes have moved
or that storage has been optimized. That decision remains downstream: the canvas
provides an understandable ownership/sharing model before a future allocator
uses real drive capacity, durability, availability, and policy inputs.
