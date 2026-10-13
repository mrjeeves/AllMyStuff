# Fleetfiles reliability review — October 2026

Scope: local allocation setup, watcher/capture scheduling, durable outgoing
queues, inbound transfer lifecycle, and the Files storage-health indicator.
This is a source review with local regression tests, not a multi-machine
performance certification. The report of lag is credible: the receive path
performed a complete hardware inventory scan for every file chunk. No
before/after throughput number has been measured.

## Changes in this PR

- **macOS allocation fallback:** compare Unix filesystem device IDs instead of
  path prefixes. `/Users` and `/System/Volumes/Data` can refer to the same Data
  volume through firmlinks. A fallback on another filesystem is rejected.
- **Transfer hot path:** resolve allocation roots at FileBegin/Directory/Delete,
  not on FileChunk/FileCommit. Discover disks only, once per reconciliation,
  rather than probing CPU, GPU, audio, cameras, USB and listening services.
- **Unavailable allocations:** exponential retry delays from 5 to 300 seconds
  on subsequent operations; log only the first/changed failure and recovery.
  A changed allocation resets the retry deadline. Successful roots are not
  cached across reconciliations, so removed volumes are discovered again.
- **Peer failure isolation:** enqueue metadata for all recipients before the
  first network send, and continue attempting other recipients if one send
  fails. Failed sends keep their durable queue records for reconnect recovery.

- **Late transfer commit:** recheck the current version under the namespace
  lock before replacing the working file. An older transfer finishing after a
  newer one retains its historical body without rolling back the file or
  current metadata. A regression test demonstrated the rollback before the fix.

## Remaining findings, in priority order

### P1 — watcher overflow does not reconcile lost changes

`FleetfilesReplica::start_watcher` drops notifications when its 4,096-item
channel is full. The watcher loop in `Mesh::start` also drops new paths when
its coalescing map reaches 4,096. Both paths warn that reconciliation is
required; neither schedules a repair walk. The startup walk visits existing
paths only, so a deletion missed while offline/overflowed is not repaired by
that walk either.

Required follow-up: a single bounded repair worker, persistent dirty state,
and reconciliation of previously managed paths missing from disk. Test a
burst larger than both limits, including deletes, and restart halfway through.

### P1 — abandoned inbound transfers consume admission slots indefinitely

`FleetfilesReplica::begin_file` admits at most 32 operations. `commit_file`
removes an operation, but there is no idle deadline or peer-disconnect cleanup
for a FileBegin whose sender disappears. Thirty-two abandoned operations can
make later transfers fail until the node restarts. Partial staging files also
need cleanup on error and restart.

Required follow-up: inactivity-based expiry refreshed by chunks, cancellation
on peer loss, and bounded staging cleanup. Test interruption before the first
chunk, mid-file, and immediately before commit; a fresh transfer must succeed.

### P1 — local capture is committed before replication intent is durable

`capture` updates `path_versions` before `replicate_fleetfiles_change` persists
content and queues all targets. A crash or local I/O error in between leaves
an observed version with incomplete replication intent. Re-capturing unchanged
bytes returns None. Ledger reconciliation can recover metadata knowledge, but
it does not reconstruct all missing content-placement work.

Required follow-up: a transactional local outbox (with a recoverable immutable
body) and an idempotent dispatcher. Inject failure at each boundary and check
that exact-version content work survives restart.

### P1 — opening a file can wait an hour before trying another peer

`Mesh::fetch_fleetfiles_body` tries peers sequentially with a one-hour wait for
BodyRequest completion. A peer that appears reachable but stops responding can
hold up an interactive open/restore even when another device has the body.

Required follow-up: separate a short acceptance deadline from a progress-based
transfer deadline, with cancellation and alternate-peer failover. Test a first
peer that accepts and stalls while a second peer can supply the exact version.

### P2 — network stalls block local capture scheduling

Local capture and outbound backlog drains share the two-permit
`fleetfiles_transfer_gate`. A drain holds its permit across an entire backlog,
network acknowledgements, and WAN pacing. Two slow peers can prevent local
changes from even being captured. The coalescer spawns a task for each ready
path; the semaphore bounds active work, but not the number of tasks waiting.

Required follow-up: bounded capture and transport queues with separate budgets,
short/fair transport batches, and a timed retry scheduler independent of fresh
presence events. Measure capture latency while two peers time out and while
one has a large backlog. The peer isolation change above still attempts peers
sequentially, so it does not eliminate timeout latency.

### P2 — storage health reflects requested capacity, not verified availability

`Mesh::fleetfiles_capacity` sums enabled allocations. The Files badge in
`gui/src/ui/App.svelte` counts enabled devices and compares logical use to that
capacity. An enabled allocation whose root cannot be created can therefore
contribute to apparent protected capacity. A successful plan write alone does
not prove usable storage or enough verified copies.

Required follow-up: expose allocation readiness and per-version replica
coverage, distinguish configured capacity from observed protection in the UI,
and test permission denial, offline peers, and full disks.

### P2 — synchronous disk work still runs in async handlers

Capture hashing is already offloaded, but content persistence, inbound commit
hashing/fsync/copy, and several SQLite operations run synchronously from async
mesh handlers. Removing full inventory scans addresses an avoidable repeated
cost; it does not bound the latency of large-file commits.

Required follow-up: bounded blocking workers and timing instrumentation for
capture, queue wait, send, verify, commit, and visible UI refresh. Keep
metadata browsing responsive while committing large files.

## Acceptance campaign still needed

Use at least two real devices and record file counts, byte sizes, latency,
queue depth, and verified copies: small-file bursts; large files; fast repeated
edits; simultaneous edits/deletes; sleep/wake; reconnect; permissions revoked;
full disks; removable-volume removal; watcher overflow; and process termination
at each persistence boundary. Do not treat unit-test success as proof that all
Fleetfiles reliability issues are resolved.

## Validation for this change

On macOS, `CARGO_NET_OFFLINE=true just node-check` passed formatting, Clippy
with warnings denied, all 410 library tests, and both serve-binary tests.
New coverage exercises the real macOS Data-volume/Users identity, an injected
permission-denied fallback, wrong-filesystem rejection, retry timing/error
suppression/recovery, allocation-change reset, chunk-path classification, and
an older in-flight transfer finishing after a newer one. The last case failed
before the commit-time check and passed after it. The first sandboxed run
blocked two unrelated OS watcher/socket tests; the complete rerun with those
facilities available passed. Windows/Linux runtime checks and the multi-device
campaign above remain outstanding.
