# Fleetfiles PR completion plan

Status: active tracker for the current Files canvas pull request.

This is not a roadmap or a limitations report. Every open item is implementation work for this pull request. The [Operation Promise](OPERATION-PROMISE.md) defines completion; the [architecture](fleet-filesystem-design.md) defines data structures and boundaries.

## Rules

1. This stays one pull request. Milestones are dependency order, not separate releases.
2. A control is complete only with an executor, observable state, failure behavior, recovery or cancellation, and tests.
3. The namespace is authoritative. Canvas, mount, native filesystems, search, and sharing are adapters or projections.
4. Desired policy and observed state remain separate. Configuration is never presented as achieved durability.
5. Happy-path code may be replaced when it conflicts with the promise. No compatibility layer may become a second authority.
6. Update this tracker in the same commit as completed code and proof.
7. A screenshot or one successful two-node transfer is not proof.

Statuses are **ACTIVE**, **READY**, **WAITING**, and **DONE** after the proof gate passes.

## Dependency order

    F0 durable operations
      -> F1 authoritative namespace
          -> F2 transactional versions and content
              -> F3 UI and mount mutation adapters
              -> F4 allocated storage and truthful capacity
                  -> F5 placement, repair, and policy
          -> F6 conflicts, tombstones, and retention
          -> F7 canvas, Search, Recent, and live invalidation
          -> F8 canonical-object sharing
    F3 + F5 + F6 + F7 + F8
      -> F9 cross-platform semantics
      -> F10 scale, partition, and failure gates

This order prevents collisions: adapters share operation IDs; placement targets immutable versions instead of paths; canvas and grants use stable object IDs; search remains disposable; and repair never infers durability from policy.

## Execution board

| ID | Status | Outcome | Proof gate |
| --- | --- | --- | --- |
| F0 | ACTIVE | Durable operation ledger and coordinator | Restart and duplicate-ID tests cannot lose state or publish twice |
| F1 | WAITING | Paged authoritative namespace with stable object and entry IDs | UI and mount preserve one identity through mutation and restart |
| F2 | WAITING | Immutable versions, bounded manifests/chunks, verified atomic commits | Crash and corruption cannot replace the last committed version |
| F3 | WAITING | UI and mount use the same preconditioned mutations | Finder/Explorer and canvas converge without watcher races |
| F4 | WAITING | Allocations enforce quota, reserve, and failure domains | Uneven capacity, exhaustion, reduction, and withdrawal tests pass |
| F5 | WAITING | Placement, full backlog drain, reconciliation, repair, rebalance | Restart, partition, missed event, corruption, and disk-full maintain policy |
| F6 | WAITING | Visible conflicts, tombstones, and retention | Concurrent edits preserve intent; restore and purge cases pass |
| F7 | WAITING | Sharded canvas, fleet background, bounded derived views | Layout converges and huge working sets stay bounded |
| F8 | WAITING | File and directory grants on canonical objects | Share/revoke exposes pending, committed, and failed states |
| F9 | WAITING | Native semantics over the common transaction path | Windows/macOS mounts, names, menus, icons, hidden items, and Trash pass |
| F10 | WAITING | Evidence for all four pillars | Real two-node and 10M-entry/40-TB-shaped gates pass budgets |

## F0 — durable operations

- [x] Persist operation ID, idempotency scope, intent, object IDs, preconditions, phase, progress, policy, retry condition, cancellation, verification, and residue.
- [ ] Separate requested operations from worker attempts.
- [ ] Model scan, staging, transfer, verification, namespace commit, materialization, compensation, completion, and failure.
- [x] Recover nonterminal work after restart and compact successful history without dropping unresolved failures.
- [x] Replace the in-memory transfer deque and feed the current Operations panel from the durable store.
- [x] Publish quiet summaries, not polling or per-chunk UI chatter.
- [ ] Test idempotency, restart, cancellation, compaction, and duplicate workers.

Exit: an interrupted operation resumes or fails actionably after restart, and Operations shows the durable record.

## F1–F3 — namespace, versions, and one mutation path

- [ ] Promote page adoption into versioned namespace shards with separate object, entry, binding, version, replica, grant, and authority records.
- [ ] Add cursor paging and stable page versions; replace the 500,000-entry correctness ceiling with bounded caches over sharded growth.
- [ ] Define cross-platform case, Unicode, names, links, packages, and special entries.
- [ ] Store immutable versions in bounded manifests and verified chunks.
- [ ] Stage, stream, hash, verify, satisfy durability, reserve the name, and publish atomically.
- [ ] Hide partial content, recover staging, resume transfers, and remove whole-file base64 transfer.
- [ ] Derive separate namespace, content, canvas, and grant keys from the fleet root.
- [ ] Route create, upload, rename, move, copy, and delete from UI and mount through one coordinator.
- [ ] Use watchers for bounded invalidation and explicit import, never as the commit protocol.
- [ ] Route shell verbs, inline rename, drag/drop, navigator, and additional windows through canonical objects.

Exit: UI and mount actions produce one mutation, one Operations record, and one identity; crash-at-every-phase retains the last committed version.

## F4–F5 — storage, capacity, placement, and repair

- [ ] Place managed chunks in enabled volume allocations instead of the application-state Desktop.
- [ ] Enforce quota, reserve, availability, removal, battery/metered policy, and failure-domain eligibility.
- [ ] Calculate protected capacity from feasible replica sets, never total quota divided by copy count.
- [ ] Report allocation, reserve, physical use, deduplication, protected usable, degraded, and unavailable bytes separately.
- [ ] Preview reduction/withdrawal and protect the last verified replica.
- [ ] Choose targets using hard constraints before bounded service-profile scoring.
- [ ] Add hysteresis, residence, cooldown, and move cost to prevent flapping.
- [ ] Drain offline queues fully in bounded rescheduled batches.
- [ ] Reconcile overflow, restart, reconnect, and missed events with digests/ranges.
- [ ] Quarantine corruption, repair replicas, and rebalance within bandwidth and energy budgets.

Exit: capacity is truthful and policy remains satisfied or honestly degraded through restart, partition, corruption, disk-full, backlog, and withdrawal without yapping.

## F6 — conflicts, tombstones, and retention

- [ ] Replace path last-writer-wins with preconditioned object and entry operations.
- [ ] Preserve concurrent names and incompatible edits as visible conflicts.
- [ ] Make rename/move logical mutations rather than observed delete/create pairs.
- [ ] Commit tombstones and retained versions before reclamation.
- [ ] Track acknowledgement horizons so directory deletion cannot erase newer child state.
- [ ] Connect retention and Danger Zone purge to real state, impact preview, and Operations.

Exit: conflict, offline edit, recursive delete/newer-child, restore, expiry, and purge tests pass.

## F7–F8 — canvas, derived views, and sharing

- [ ] Replace the capped canvas JSON file with a manifest and sparse spatial shards containing explicit layout only.
- [ ] Keep deterministic column-major virtual placement and avoid quadratic work over nonvisible objects.
- [ ] Store wallpaper as fleet metadata backed by a portable managed asset.
- [ ] Use visible-scope subscriptions with bounded leases, coalescing, expiry, and reconnect invalidation.
- [ ] Make Search and Recent bounded disposable queries over canonical IDs and virtualize rendering.
- [ ] Store grants separately from replicas and namespace objects.
- [ ] Show actual granted objects; directory grants cover descendants.
- [ ] Make file/folder share and revoke cancellable operations with pending, failed, and committed states.
- [ ] Insert Shared-with-me through grant-backed edges and narrowly wrapped keys.

Exit: layout, wallpaper, navigation, and grants converge across fleets while large working sets stay bounded.

## F9–F10 — platform and release proof

- [ ] Maintain one stable Fleetfiles mount per supported desktop and recover startup without accumulating mounts.
- [ ] Reject or encode incompatible names and metadata before commit.
- [ ] Supply native icons, overlays, hidden defaults, Recycle Bin/Trash, Properties, and real single/multi-selection menus.
- [ ] Preserve or disclose loss of ACLs, xattrs/resource forks, streams, links, packages, sparse traits, and placeholders.
- [ ] Test restart, delay, duplication, reordering, disconnect, partition, reconnect, disk-full, corruption, interrupted staging, withdrawal, stale metrics, and retries.
- [ ] Test tens of millions of entries and over 40 TB of represented content without allocating it.
- [ ] Record navigation, open, convergence, repair, CPU, memory, disk, and idle-traffic budgets.
- [ ] Prove bounds on watchers, queues, staging, caches, history, profiles, canvas shards, and indexes.
- [ ] Run the gate on this Windows machine and the Mac; retain reproducible commands and artifacts.

Exit: every Operation Promise gate has automated evidence or a reproducible captured real-machine result.

## Collision register

| Collision | Resolution |
| --- | --- |
| Watcher vs. namespace authority | Watchers invalidate/import; preconditioned namespace operations commit |
| Path replication vs. identity | Replicate immutable versions by object ID; paths are entry metadata |
| Mount cache vs. canvas | Both project the namespace and ledger; neither syncs directly to the other |
| Canvas vs. file count | Spatial shards hold explicit layout only; virtual positions create no record |
| Search vs. correctness | Search is disposable and never authorizes mutations |
| Sharing vs. replication | Grants confer access; placement confers storage responsibility |
| Shell fidelity vs. safety | Shell verbs enter the common operation path through the mount adapter |
| Retention vs. deletion | Commit tombstones first; reclaim after retention and acknowledgement |
| Scoring vs. flapping | Hard constraints, hysteresis, residence, cooldown, and move cost precede relocation |
| Capacity vs. intent | Report feasible protected capacity and verified state, not configured quota as durability |

## Immediate next action

Implement F0 in the existing node SQLite infrastructure, migrate the transfer-operation publisher to it, and keep the Files Operations panel reading that durable source. F1 begins after F0 restart and idempotency tests pass.
