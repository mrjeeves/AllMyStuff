# AllMyStuff — one-command operations.
# Install `just` (https://just.systems) then run `just setup` to get going.
#
# `set shell` is used on Linux/macOS. On Windows the global
# `windows-shell` override routes recipes through PowerShell so they find
# `pnpm.cmd` / `node.exe` via the Windows PATH. Recipes with bash-specific
# syntax need a `[windows]` variant; recipes that just call cross-platform
# tools (cargo, pnpm, git) work in both shells unmodified.
set shell := ["bash", "-cu"]
set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]

default: help

help:
    @just --list

# Install dev prerequisites: the WebKitGTK/GTK + ALSA libs the Tauri +
# cpal backend links against, plus Rust, Node, and pnpm. After this,
# `just dev` runs the app — the mesh daemon is bundled automatically by
# the GUI's first build (it fetches the rev pinned in .myownmesh-rev).
[unix]
[doc("Install dev prerequisites (WebKit/GTK/ALSA, Rust, Node, pnpm).")]
setup:
    @./scripts/bootstrap.sh

[windows]
[doc("Install dev prerequisites (Rust, Node, pnpm, WebView2).")]
setup:
    @& .\scripts\bootstrap.ps1

# Run the desktop app (Tauri + Svelte) with hot reload. We build the
# `allmystuff-serve` node binary first: the GUI is a thin client that spawns it
# (one node per machine), and the GUI's build.rs bundles whatever's in
# `node/target` as a sidecar — so without this the app would ship a stub and
# spawn nothing. The mesh daemon underneath is still bundled automatically
# (build.rs fetches the rev pinned in .myownmesh-rev the first time), and the
# node spawns it. So `just dev` gives you the full app with live mesh.
[unix]
[doc("Run the app with hot reload (node + mesh daemon bundled automatically).")]
dev *ARGS:
    @cargo build --manifest-path node/Cargo.toml --bin allmystuff-serve
    @cd gui && pnpm install --silent && pnpm tauri dev {{ARGS}}

[windows]
[doc("Run the app with hot reload (node + mesh daemon bundled automatically).")]
dev *ARGS:
    @cargo build --manifest-path node/Cargo.toml --bin allmystuff-serve
    @cd gui; pnpm install --silent; pnpm tauri dev {{ARGS}}

# Build the library workspace (no webview / Tauri compile).
build:
    @cargo build --workspace

build-release:
    @cargo build --workspace --release

# Build the desktop bundle (.deb / .AppImage / .dmg / .msi).
[unix]
[doc("Build the desktop bundle.")]
gui-build:
    @cd gui && pnpm install --silent && pnpm tauri build

[windows]
[doc("Build the desktop bundle.")]
gui-build:
    @cd gui; pnpm install --silent; pnpm tauri build

# Scan this machine from the CLI.
scan:
    @cargo run -p allmystuff-cli -- scan

# Show the capabilities this machine would expose on the mesh.
caps:
    @cargo run -p allmystuff-cli -- capabilities

# Open a mesh terminal (`amst`) from source — a shell on a machine you own.
# No MACHINE opens one on this machine; needs a node running (`just serve`).
term *ARGS:
    @cargo run -p allmystuff-term --bin amst -- {{ARGS}}

# Run this machine on the mesh, headless (the node `allmystuff serve` runs).
# Builds from the node workspace and spawns the myownmesh daemon itself.
serve *ARGS:
    @cargo run --manifest-path node/Cargo.toml --bin allmystuff-serve -- {{ARGS}}

# Stop this machine's whole mesh stack: the AllMyStuff node (`allmystuff-serve`)
# and the `myownmesh` daemon it spawns. Use it for a clean slate between `just
# dev` runs, or to clear an *orphaned* daemon — on macOS a hard Ctrl-C out of
# `just dev` can leave the daemon running (no kernel parent-death signal there),
# and the next run silently reuses it. The node is SIGTERM'd first so it shuts
# down cleanly (which kills its own daemon child); then any leftover daemon is
# swept. Killing the daemon alone is pointless in dev — the node doesn't respawn
# it — so this always takes down both; restart with `just dev`.
[unix]
[doc("Kill this machine's mesh stack (node + myownmesh daemon).")]
kill:
    @pkill -TERM -f '[a]llmystuff-serve' 2>/dev/null; pkill -f '[m]yownmesh.* serve' 2>/dev/null; echo 'stopped the node + mesh daemon (whatever was running)'

[windows]
[doc("Kill this machine's mesh stack (node + myownmesh daemon).")]
kill:
    @Get-Process allmystuff-serve,myownmesh,myownmesh-* -ErrorAction SilentlyContinue | Stop-Process -Force; Write-Output "mesh stack stopped (allmystuff-serve + myownmesh)"; exit 0

# Clean restart: kill the mesh stack, then start the app fresh.
[doc("Kill the mesh stack, then `just dev`.")]
restart *ARGS: kill
    @just dev {{ARGS}}

# Discard local changes, pull the latest, and fetch every remote branch — a
# pristine tree so `just dev` starts clean each time (clears stray lockfile /
# build-artifact edits a dev run leaves behind), with all of origin's branches
# fetched and ready to check out. If the current branch no longer exists on
# origin (a feature branch merged & deleted out from under the clone), the
# old `git pull` died with "no such ref was fetched" — now the gone upstream
# is detected and the recipe drops back to the default branch before pulling.
# Bash logic, so this one carries a [windows] PowerShell twin.
[unix]
[doc("Discard local changes + pull + fetch all; falls back to the default branch when yours is gone on origin.")]
pull:
    #!/usr/bin/env bash
    set -euo pipefail
    git reset --hard HEAD
    git fetch --all --prune
    branch=$(git rev-parse --abbrev-ref HEAD)
    if ! git show-ref --verify --quiet "refs/remotes/origin/$branch"; then
        default=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD | sed 's|^origin/||' || true)
        default=${default:-main}
        echo "→ origin no longer has '$branch' (merged & deleted?) — switching to '$default'"
        git checkout "$default"
    fi
    git pull

[windows]
[doc("Discard local changes + pull + fetch all; falls back to the default branch when yours is gone on origin.")]
pull:
    @$branch = git rev-parse --abbrev-ref HEAD; git reset --hard HEAD; git fetch --all --prune; git show-ref --verify --quiet "refs/remotes/origin/$branch"; if ($LASTEXITCODE -ne 0) { $default = git symbolic-ref --quiet --short refs/remotes/origin/HEAD; if ($default) { $default = $default -replace '^origin/','' } else { $default = 'main' }; Write-Host "origin no longer has '$branch' (merged & deleted?) - switching to '$default'"; git checkout $default }; git pull

# `git checkout` with a clean slate first: `pull` runs ahead of it (discard
# local changes, pull, and fetch every remote branch), so the tree is pristine
# and whatever branch you name is already fetched and ready. Args still pass
# straight through — `just checkout main`, `just checkout -b feature`,
# `just checkout -- file` all behave like the git command, minus typing `git`.
[doc("just pull (clean + fetch all), then git checkout (e.g. `just checkout main`).")]
checkout *args: pull
    @git checkout {{args}}

# The one-liner clean start: stop the mesh stack, pull a pristine tree, then run
# the app. `kill` and `pull` run first (in order), then `just dev` (the OS
# variant resolves itself). `just go -- <args>` forwards to `dev`.
[doc("just kill + just pull + just dev.")]
go *ARGS: kill pull
    @just dev {{ARGS}}

fmt:
    @cargo fmt --all

fmt-check:
    @cargo fmt --all --check

lint:
    @cargo clippy --workspace --all-targets -- -D warnings

test:
    @cargo test --workspace --no-fail-fast

# The headless node engine lives in its own workspace (heavy media deps), so
# its fmt/clippy/test don't ride the root `--workspace` flags — run them here.
[unix]
node-check:
    @cd node && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

[windows]
node-check:
    @cd node; cargo fmt --check; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo clippy --all-targets -- -D warnings; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo test

# Test + typecheck + build the Svelte front-end (no webview needed).
[unix]
[doc("Test + typecheck + build the front-end.")]
gui-check:
    @cd gui && pnpm install --frozen-lockfile && pnpm test && pnpm check && pnpm build

[windows]
[doc("Test + typecheck + build the front-end.")]
gui-check:
    @cd gui; pnpm install --frozen-lockfile; pnpm test; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; pnpm check; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; pnpm build

# Everything CI runs: Rust fmt + clippy + test (library workspace + the node
# engine), then the GUI typecheck/build.
check: fmt-check lint test node-check gui-check

# Resolve and verify every Cargo dependency represented by a suite pin. Today
# that is the mobile workspace's embedded MyOwnMesh graph; desktop/node consume
# MyOwnMesh as a release sidecar and therefore have no Cargo lock entry.
[doc("Update Cargo locks from the suite pin files and verify they agree.")]
pin-locks:
    @bash ./scripts/sync-pinned-locks.sh

# Cut a release: bump every crate's version (+ the GUI/node sub-workspaces),
# commit, push, then push the `v{{VERSION}}` tag to trigger the workflow.
# Mirrors MyOwnMesh / MyOwnLLM — the user runs `just release 0.2.0` and the
# release.yml workflow verifies the manifests, builds the per-platform bundles
# + portable binaries, and publishes the GitHub release. Bash script — the
# release flow runs from a Linux/macOS box.
#
# We trigger by pushing the tag (not `gh workflow run`) so the build is
# deterministic. The tag is an immutable ref pinned to the bump commit, so the
# workflow's `actions/checkout` lands exactly on the version we just committed.
# The old `gh workflow run release.yml` used workflow_dispatch, which resolves
# `main` at dispatch time and raced the preceding `git push`: GitHub could
# still see the pre-bump HEAD, check that out, build the old version, and fail
# the "Verify tag matches manifest versions" gate (this is what sank v0.1.21 —
# the run built the 0.1.20 commit). release.yml keeps its workflow_dispatch
# trigger as a manual escape hatch for re-running an existing tag.
[unix]
[doc("Cut a release: bump versions, commit, push, tag to trigger the workflow.")]
release VERSION:
    @./scripts/bump-version.sh {{VERSION}}
    @bash ./scripts/sync-pinned-locks.sh
    @if ! git diff --quiet .myownmesh-rev Cargo.toml Cargo.lock gui/src-tauri/Cargo.toml gui/src-tauri/Cargo.lock gui/package.json node/Cargo.toml node/Cargo.lock gui/mobile/Cargo.toml gui/mobile/Cargo.lock gui/mobile/tauri.conf.json gui/mobile/package.json; then \
        git add .myownmesh-rev Cargo.toml Cargo.lock crates/*/Cargo.toml gui/src-tauri/Cargo.toml gui/src-tauri/Cargo.lock gui/package.json node/Cargo.toml node/Cargo.lock gui/mobile/Cargo.toml gui/mobile/Cargo.lock gui/mobile/tauri.conf.json gui/mobile/package.json; \
        git commit -m "chore(release): {{VERSION}}"; \
    fi
    @git push
    @git tag v{{VERSION}}
    @git push origin v{{VERSION}}
    @echo ""
    @echo "✓ pushed tag v{{VERSION}} — the release workflow is building."
    @echo "  watch it:   gh run watch"
    @echo "  list runs:  gh run list --workflow=release.yml"
