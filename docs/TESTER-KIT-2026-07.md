# Tester kit — verifying the encoder line for upstream review

> **Historical record from July 2026:** this kit belongs to the encoder handoff
> captured against AllMyStuff 0.2.46. Commands and expected results may no
> longer match the current release. See the [documentation map](README.md) for
> current testing and contributor guidance.

_For Chris's review and security push. Everything here runs against the
curated `codex/video-pipeline-upstream-pr` branch. The companion document is
[`INTEGRATION-REPORT-2026-07.md`](INTEGRATION-REPORT-2026-07.md) (what
changed, blast radius, and wire compatibility). Fork-only handoffs,
measurement harnesses, dumps, and future-work notes are intentionally absent
from the upstream branch; nothing experimental is on by default._

## 0 · The source target

Review and validation use the branch itself. No prototype installer,
captured log, binary fixture, local tool configuration, or generated output is
part of the proposed diff. Versioning remains untouched at upstream 0.2.46,
and `.myownmesh-rev` remains unchanged.

## 1 · Static verification (any box, ~10 minutes)

```powershell
git fetch origin codex/video-pipeline-upstream-pr
git fetch upstream main
git switch codex/video-pipeline-upstream-pr
# The whole proposed upstream delta:
git diff --stat upstream/main...HEAD
# The only wire-visible surface (30 additive serde-default lines):
git diff upstream/main...HEAD -- crates/allmystuff-protocol/src/app.rs
# The daemon boundary (every daemon call is a pre-existing API):
git diff upstream/main...HEAD -- node/src/mesh.rs
# Confirm the daemon pin is untouched:
git diff upstream/main...HEAD -- .myownmesh-rev   # empty
```

Security-review shortlist (the surfaces worth your push, in order):
1. `crates/allmystuff-protocol/src/app.rs` — new fields arrive from
   remote peers: `est_kbps: u32`, `delay_trend_us_per_s: i32`,
   `lost_ts_us: Option<u64>`, `Tune.game/mode`. All plain data; the
   consumers clamp/gate (AIMD clamps to [8 Mbps, posture ceiling];
   feedback only accepted from the route's own active peer — the
   pre-existing `is_active() && peer == from` gate in
   `crates/allmystuff-session/src/lib.rs`).
2. `node/src/d3d11va.rs` — an in-house HEVC bitstream parser consuming
   REMOTE data. Scoped and defensive by design: every read
   bounds-checked (`BitReader` errors on truncation, never panics),
   `rbsp` capped, out-of-scope streams (10-bit, scaling lists,
   long-term refs, B slices, oversized RPS/tile grids, implausible
   geometry) rejected with named errors, and every failure soft-resets
   the session (bridge re-keys). Fuzzing this parser is a worthwhile
   security-push target; the entry point is `D3d11vaHevc::decode`.
3. `node/src/nvenc.rs` / `nvdec.rs` — FFI structs, size-asserted;
   inputs are local (our own encoder/driver), not remote.
4. `node/src/mesh.rs` `note_video_arrival` — remote-driven state
   (per-route arrival map): bounded (one entry per route, windows
   trimmed by time), no allocation proportional to remote input sizes
   beyond the per-minute sample vec.
5. `node/src/os_perf.rs` — process-local OS levers, all best-effort,
   nothing remote-reachable.

## 2 · Build + unit gate (dev box, ~10 minutes warm)

```powershell
cd node
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features       # the capture-less/viewer build
cargo test
cd ../gui
pnpm install --frozen-lockfile
pnpm check
pnpm build
cd ..
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The Ubuntu CI runner additionally executes strict Clippy and tests with
`--features hwenc` after installing the FFmpeg development libraries.

On macOS, errors mentioning a missing libc
`QOS_CLASS_USER_INTERACTIVE` or a missing `EncodeOutcome.input_ts` mean the
checkout predates the branch's cross-platform fix. Fetch and switch the exact
branch above. The current source declares the Darwin QoS binding locally and
initializes the VideoToolbox timestamp field; both macOS CI jobs compile that
path.

## 3 · Hardware proofs (NVIDIA box, ~5 minutes)

The claims and the tests that pin them — all skip clean (passing)
without the silicon:

| Claim | Command | Expected |
|---|---|---|
| Lossless is bit-exact, NVENC→NVDEC | `cargo test --release nvdec_hevc_lossless_round_trip -- --nocapture` | `60/60 byte-exact` |
| Lossless is bit-exact through the VENDOR-NEUTRAL rung | `cargo test --release d3d11va_hevc_lossless_round_trip -- --nocapture` | `60/60 byte-exact @1280×718 (crop live)` |
| …and survives real pacer chunking | `cargo test --release d3d11va_survives_pacer_chunking -- --nocapture` | `30/30 byte-exact` |
| SIMD convert is byte-equal to scalar | `cargo test --release simd_lane_matches -- --nocapture` | pass (skips without AVX2) |
| GDR = no IDR walls, decodes clean | `cargo test --release nvenc_intra_refresh -- --nocapture` | pass |
| Sub-quantum pacing sleeps | `cargo test --release precise_sleep -- --nocapture` | completes inside the test's asserted timing bound |

## 4 · Field smoke (two machines, ~20 minutes)

1. Build and install the candidate on both. Start a console session; confirm the
   Mode control cycles **Balanced → Game → Studio → Studio·LL** (one
   bandwidth warning on first Studio entry).
2. The standard node log remains available at the node state root; an installed
   Windows service writes under `%ProgramData%\AllMyStuff\logs\`. Verbose
   development logs and the 1 Hz hardware telemetry sampler are **off by
   default**. Enable verbose logs with the local Development diagnostics toggle
   or `ALLMYSTUFF_CWD_LOG=1`; enable telemetry with
   `ALLMYSTUFF_TELEMETRY=1` (or a diagnostic build carrying
   `--features field-telemetry`). The session lines to inspect are:
   - `video out …: raw N Mbps → wire N Mbps · age N ms (p95) · scale · encode …`
     — sender layers + M1 capture-age span.
   - `pace gaps: … requested avg → actual avg · worst · >1 ms err % · daemon write avg µs/chunk`
     — pacing honesty + the daemon-pipe await split.
   - `video in …: chunk-trains n · implied p5/p50 Mbps · est · delay trend`
     — the viewer's bandwidth estimate from your own paced bursts.
   - `video decode …: fps · ms/frame · wire → nv12 → rgba Mbps` — viewer layers.
   - `video rate …: X → Y Mbps (ceiling Z)` — every closed-loop step
     (Game posture only by default).
   - `HEVC decoder for …: NVDEC (…)` or `D3D11VA (… vendor-neutral)` —
     which glass Studio·LL crossed.
   - With telemetry explicitly enabled: CPU/thread split, per-engine GPU busy,
     VRAM, and monitor topology.
3. Per-posture checks:
   - **Game**: no periodic keyframe pulses on the wire (GDR); pull the
     network cable for 2 s → picture heals in a wave, log shows the
     wave restart (3-frame short heal if you do it twice in 10 s).
   - **Studio·LL** on an NVIDIA pair: decoder line says NVDEC. Set
     `ALLMYSTUFF_HEVC_DECODER=d3d11va` on the viewer and restart it:
     same picture, decoder line says D3D11VA — that's the cross-vendor
     path exercised on NVIDIA glass.
   - **WAN dial-down rehearsal** (single site): `clumsy` at 20–40 Mbps
     ±20 ms on the viewer; in Game the `video rate` line should step
     down within ~5 s and climb back after ~30 s clean.
4. Kill switches if anything misbehaves (all env, viewer/host as noted):
   `ALLMYSTUFF_RATE_ADAPT=0` (host) · `ALLMYSTUFF_PACED_SLICES=0`
   (host) · `ALLMYSTUFF_NVENC=0` (host, pins the vendor MFT and disables
   the direct lossless rung) · `ALLMYSTUFF_HEVC_DECODER=nvdec` (viewer).
   Opt-ins that default OFF: `ALLMYSTUFF_GPU_LANE=1` (experimental shared-
   texture capture; also required to exercise the quarantined HEVC path),
   `ALLMYSTUFF_NVENC=1` (direct SDK for normal lossy postures),
   `ALLMYSTUFF_MMCSS=1`, `ALLMYSTUFF_GPU_SCHED=1`,
   `ALLMYSTUFF_AUTO_ADAPT=1`, `ALLMYSTUFF_RATE_ADAPT=1` (all-postures
   form). Full dial table:
   integration report §5.

## 5 · PR hygiene and target

The upstream review branch is `codex/video-pipeline-upstream-pr`, targeting
`mrjeeves/AllMyStuff:main` in PR #192. It contains production code, automated
regression tests, and the two maintainer-facing review documents only.

Before updating the PR, verify that the delta does not contain fork-only
handoffs, `docs/fork/**`, field probes, ignored benchmarks or soaks, generated
installers, logs, dumps, local tool configuration, or build artifacts:

```powershell
git diff --name-status upstream/main...HEAD
git diff --check upstream/main...HEAD
git ls-tree -r --name-only HEAD | rg '(^|/)(docs/fork|HANDOFF\.md|node/examples|target|node_modules)(/|$)|\.(exe|pdb|log|bmp|dump)$'
rg -n '#\[ignore|ALLMYSTUFF_(SOAK|DIAG_DIR|HEVC_DUMP)' node
```

The fork-local draft PR is only a CI trigger. Do not merge it. Close it after
the upstream branch has completed its own merge-target workflows.
