# Contributing to AllMyStuff

Thank you for helping make AllMyStuff more reliable and easier to use.

Start with the [README](README.md) for the product, the
[documentation map](docs/README.md) for the right guide, and
[ARCHITECTURE.md](ARCHITECTURE.md) when you need process or crate boundaries.

## Quick start

Install [`just`](https://just.systems), then:

```sh
just setup
just dev
```

`just setup` installs the development prerequisites for the current platform.
`just dev` builds the node sidecar, installs the front-end packages, and runs
the Tauri app with hot reload. The pinned MyOwnMesh runtime is prepared as part
of the build.

The toolchain is:

- Rust stable, pinned by `rust-toolchain.toml`.
- Node 22 and pnpm 10 for the Svelte front end.
- Tauri's native webview and audio dependencies.
- Xcode command-line tools on macOS, WebView2 and MSVC on Windows, or the
  WebKitGTK, GTK, Soup, and ALSA development libraries on Linux.

## Repository workflow

The `justfile` is the supported interface for routine repository work:

```sh
just pull                 # discard local edits, fetch, and update the current branch
just checkout main        # clean first, then check out a fetched branch
just build                # build the root Rust workspace
just dev                  # run the complete desktop app with hot reload
just serve                # run the node headless from source
just term Tracy-Laptop    # open AMSTerm from source
just check                # run all checks represented in CI
just gui-build            # build the native desktop bundle
```

**`just pull` is intentionally destructive.** It resets tracked changes before
fetching and pulling. Commit or copy work you want to keep before running it.

Use `just restart` when a development node or mesh daemon survived a previous
run. It stops this machine's local stack and starts `just dev` again.

## Repository layout

| Path | Purpose |
|---|---|
| `crates/` | Shared, lightweight Rust model, protocol, inventory, updater, terminal, service, and support crates. |
| `node/` | The full node engine and `allmystuff-serve` binary. It owns sessions, media, input, terminals, files, drives, sites, KVM integration, and the node control socket. |
| `gui/src/` | The shared Svelte interface used by desktop, mobile, and browser preview. |
| `gui/src-tauri/` | The desktop Tauri shell. It is a client of the node control socket. |
| `gui/mobile/` | The mobile Tauri shell. It embeds the capture-less node and mesh runtime because mobile platforms cannot spawn the desktop sidecars. |
| `scripts/` | Installation, release, pin synchronization, and development helpers. |

The root, node, desktop Tauri, and mobile directories are separate Cargo
workspaces. This keeps the ordinary library build fast and prevents native
media and webview dependencies from leaking into every command.

## Focused checks

```sh
just fmt-check       # root Rust formatting
just lint            # root workspace Clippy
just test            # root workspace tests
just node-check      # node formatting, Clippy, and tests
just gui-check       # front-end tests, typecheck, and production build
```

Run the narrowest relevant check while working. Run `just check` before
publishing a change that crosses workspaces or changes shared protocol types.

Every pull request runs:

- Root Rust checks on Linux, macOS, and Windows.
- Node checks on Linux, macOS, and Windows.
- Desktop Tauri backend checks on Linux, macOS, and Windows.
- Front-end tests, typechecking, and production build.
- Android compilation for the mobile configuration.

The workflow definitions under `.github/workflows/` are the authority when
this summary and CI ever disagree.

## Run command-line tools from source

```sh
cargo run -p allmystuff-cli -- scan
cargo run -p allmystuff-cli -- capabilities
cargo run -p allmystuff-term --bin amst -- --list
cargo run --manifest-path node/Cargo.toml --bin allmystuff-serve
```

Installed command behavior belongs in [Using AllMyStuff](docs/USING-ALLMYSTUFF.md),
not in this contributor guide.

## Work with a local MyOwnMesh checkout

The app normally uses the release pinned in `.myownmesh-rev`. During mesh
development, set `MYOWNMESH_BIN` to a specific local binary or build a sibling
`../MyOwnMesh` checkout. The build and runtime prefer those explicit
development sources.

Set `ALLMYSTUFF_SKIP_SIDECAR=1` only for an intentional offline or
externally-managed development run.

When changing a suite pin, run:

```sh
just pin-locks
```

That updates the Cargo locks represented by pin files and verifies that the
resolved revisions agree.

## Release

Maintainers cut a release from a clean, current branch on macOS or Linux:

```sh
just release 1.2.3
```

The recipe updates all workspace versions and represented locks, commits the
release, pushes it, and pushes the version tag. The tag starts the release
workflow. Do not create a second release or manually push another tag for the
same version.

Release-key setup is documented in [RELEASE-SIGNING.md](RELEASE-SIGNING.md).

## Conventions

- Keep Rust formatted and Clippy-clean with warnings denied.
- Keep the Rust and TypeScript graph and authorization models in sync.
- Add fixture tests for parsers and platform-specific system formats.
- Preserve one-way authorization semantics. Visibility is not permission.
- Prefer a focused document and a link over copying the same explanation into
  several files.
- Match the surrounding naming and comment density.

For changes that affect user-visible behavior, update the
[user guide](docs/USING-ALLMYSTUFF.md) or README in the same pull request.
