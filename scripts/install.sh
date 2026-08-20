#!/bin/sh
# AllMyStuff end-user installer.
#
# Tries (in order):
#   1. Download a pre-built release binary from GitHub for the current platform.
#   2. Fall back to building from source via cargo.
#
# Installs the `allmystuff` CLI, the `allmystuff-gui` desktop app (the app
# is small and makes a bare `allmystuff` open it — pass --no-gui for a
# CLI-only install on a headless box), and the standalone `amst` mesh
# terminal (`amst <machine>` opens a shell on any machine you own; --no-amst
# to skip), then makes sure the `myownmesh` daemon the app's live mode runs
# on is in place:
#
#   * an installed daemon that's new enough (>= the version pinned in
#     .myownmesh-rev) is used as-is;
#   * an older one is asked to update itself (`myownmesh update`);
#   * none at all → the latest MyOwnMesh release is installed next to
#     the app (same download + SHA-256 verification as the app itself).
#
# Pass --no-mesh to leave the daemon entirely alone. Mesh trouble never
# fails the install — the app always opens (demo graph) without it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mrjeeves/AllMyStuff/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/mrjeeves/AllMyStuff/main/scripts/install.sh | sh -s -- --no-gui
#   ./scripts/install.sh --dry-run
#
# POSIX sh-compatible so `curl … | sh` works under dash, ash/busybox sh, and
# bash alike. Avoid bash-only constructs ([[ ]], arrays, ${var^^}, etc.).

set -eu
if (set -o pipefail) 2>/dev/null; then
  set -o pipefail
fi

REPO="${ALLMYSTUFF_REPO:-mrjeeves/AllMyStuff}"
MESH_REPO="${MYOWNMESH_REPO:-mrjeeves/MyOwnMesh}"
DRY_RUN=false
PREFIX_DIR="${ALLMYSTUFF_PREFIX:-}"
FORCE_SOURCE=false
INSTALL_GUI=true
INSTALL_MESH=true
INSTALL_AMST=true
# Set by build_from_source so amst can reuse the same checkout/clone.
REPO_DIR=""

for arg in "$@"; do
  case "$arg" in
    --dry-run)     DRY_RUN=true ;;
    --from-source) FORCE_SOURCE=true ;;
    --no-gui)      INSTALL_GUI=false ;;
    --no-mesh)     INSTALL_MESH=false ;;
    --no-amst)     INSTALL_AMST=false ;;
    --prefix=*)    PREFIX_DIR="${arg#*=}" ;;
    *) ;;
  esac
done

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!!\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31mxxx\033[0m %s\n' "$*" >&2; }

OS_RAW="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS_RAW" in
  darwin) OS="macos" ;;
  linux)  OS="linux" ;;
  *)      OS="$OS_RAW" ;;
esac
ARCH_RAW="$(uname -m)"
case "$ARCH_RAW" in
  x86_64|amd64)  ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *)             ARCH="$ARCH_RAW" ;;
esac
ASSET="allmystuff-${OS}-${ARCH}.tar.gz"
GUI_ASSET="allmystuff-gui-${OS}-${ARCH}.tar.gz"
SERVE_ASSET="allmystuff-serve-${OS}-${ARCH}.tar.gz"
AMST_ASSET="amst-${OS}-${ARCH}.tar.gz"
MESH_ASSET="myownmesh-${OS}-${ARCH}.tar.gz"

# Pick install prefix. Prefer /usr/local/bin if writable (or sudo is cached);
# else ~/.local/bin so a no-sudo install still lands somewhere sensible.
if [ -z "$PREFIX_DIR" ]; then
  if [ -w /usr/local/bin ] || sudo -n true 2>/dev/null; then
    PREFIX_DIR="/usr/local/bin"
  else
    PREFIX_DIR="$HOME/.local/bin"
  fi
fi

install_binary() {
  src="$1"
  mkdir -p "$PREFIX_DIR" 2>/dev/null || sudo mkdir -p "$PREFIX_DIR"
  if [ -w "$PREFIX_DIR" ]; then
    install -m 0755 "$src" "$PREFIX_DIR/allmystuff"
  else
    sudo install -m 0755 "$src" "$PREFIX_DIR/allmystuff"
  fi
  log "Installed: $PREFIX_DIR/allmystuff"
}

install_gui_binary() {
  src="$1"
  mkdir -p "$PREFIX_DIR" 2>/dev/null || sudo mkdir -p "$PREFIX_DIR"
  if [ -w "$PREFIX_DIR" ]; then
    install -m 0755 "$src" "$PREFIX_DIR/allmystuff-gui"
  else
    sudo install -m 0755 "$src" "$PREFIX_DIR/allmystuff-gui"
  fi
  log "Installed: $PREFIX_DIR/allmystuff-gui"
}

install_serve_binary() {
  src="$1"
  mkdir -p "$PREFIX_DIR" 2>/dev/null || sudo mkdir -p "$PREFIX_DIR"
  if [ -w "$PREFIX_DIR" ]; then
    install -m 0755 "$src" "$PREFIX_DIR/allmystuff-serve"
  else
    sudo install -m 0755 "$src" "$PREFIX_DIR/allmystuff-serve"
  fi
  log "Installed: $PREFIX_DIR/allmystuff-serve"
}

ensure_drive_mount_helper() {
  [ "$OS" = "linux" ] || return 0
  if command -v mount.davfs >/dev/null 2>&1 || [ -x /sbin/mount.davfs ] || [ -x /usr/sbin/mount.davfs ]; then
    return 0
  fi
  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would install davfs2 for native Linux drive mapping"
    return 0
  fi
  if ! command -v apt-get >/dev/null 2>&1; then
    warn "Native Linux drive mapping needs the davfs2 package. Install it with your package manager."
    return 0
  fi
  log "Installing davfs2 for native Linux drive mapping…"
  if [ "$(id -u)" -eq 0 ]; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends davfs2 ||
      warn "Couldn't install davfs2; native drive mapping will remain unavailable until it is installed."
  elif sudo -n true 2>/dev/null; then
    sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends davfs2 ||
      warn "Couldn't install davfs2; native drive mapping will remain unavailable until it is installed."
  else
    warn "Native Linux drive mapping needs davfs2. Run: sudo apt-get install davfs2"
  fi
}

install_amst_binary() {
  src="$1"
  mkdir -p "$PREFIX_DIR" 2>/dev/null || sudo mkdir -p "$PREFIX_DIR"
  if [ -w "$PREFIX_DIR" ]; then
    install -m 0755 "$src" "$PREFIX_DIR/amst"
  else
    sudo install -m 0755 "$src" "$PREFIX_DIR/amst"
  fi
  log "Installed: $PREFIX_DIR/amst"
}

ensure_on_path() {
  case ":$PATH:" in
    *":$PREFIX_DIR:"*) return 0 ;;
  esac

  shell_name="$(basename "${SHELL:-bash}")"
  marker="# added by allmystuff installer"
  case "$shell_name" in
    zsh)
      rc="$HOME/.zshrc"
      line="export PATH=\"$PREFIX_DIR:\$PATH\"  $marker"
      ;;
    fish)
      rc="$HOME/.config/fish/config.fish"
      line="fish_add_path -g $PREFIX_DIR  $marker"
      ;;
    *)
      rc="$HOME/.bashrc"
      line="export PATH=\"$PREFIX_DIR:\$PATH\"  $marker"
      ;;
  esac

  if grep -qsF "$marker" "$rc" 2>/dev/null; then
    warn "$PREFIX_DIR not on current PATH; PATH already added to $rc — open a new terminal."
    return 0
  fi

  mkdir -p "$(dirname "$rc")"
  if printf '\n%s\n' "$line" >> "$rc" 2>/dev/null; then
    log "Added $PREFIX_DIR to PATH in $rc"
    log "Open a new terminal (or run: source $rc) for it to take effect."
  else
    warn "$PREFIX_DIR is not on PATH. Add this to your shell rc:"
    warn "  $line"
  fi
}

# Tracked for cleanup since POSIX sh has no function-scoped RETURN trap.
_TRY_RELEASE_TMP=""
_cleanup_try_release() {
  if [ -n "$_TRY_RELEASE_TMP" ] && [ -d "$_TRY_RELEASE_TMP" ]; then
    rm -rf "$_TRY_RELEASE_TMP"
  fi
  _TRY_RELEASE_TMP=""
}

try_release() {
  if ! command -v curl >/dev/null 2>&1; then
    warn "curl missing; skipping release download."
    return 1
  fi
  api="https://api.github.com/repos/${REPO}/releases/latest"
  log "Looking up latest release: $api"
  if ! json="$(curl -fsSL "$api" 2>/dev/null)"; then
    warn "GitHub releases unreachable (or no release yet)."
    return 1
  fi
  url="$(printf '%s' "$json" | grep -Eo "https://[^\"]+/${ASSET}" | head -n1 || true)"
  if [ -z "$url" ]; then
    warn "No release asset matched ${ASSET}."
    return 1
  fi
  sha_url="${url}.sha256"
  log "Downloading $url"
  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would download $url"
    return 0
  fi
  _TRY_RELEASE_TMP="$(mktemp -d)"
  trap _cleanup_try_release EXIT INT TERM
  curl -fsSL "$url" -o "$_TRY_RELEASE_TMP/$ASSET"
  if curl -fsSL "$sha_url" -o "$_TRY_RELEASE_TMP/$ASSET.sha256" 2>/dev/null; then
    # set -e is suspended while this function runs inside the `if !` at the
    # bottom of the script, so the check has to gate explicitly.
    if ! (cd "$_TRY_RELEASE_TMP" && (sha256sum -c "$ASSET.sha256" 2>/dev/null || shasum -a 256 -c "$ASSET.sha256")); then
      err "SHA256 verification failed for $ASSET — not installing it."
      exit 1
    fi
  else
    warn "No SHA256 sidecar; skipping integrity check."
  fi
  tar -xzf "$_TRY_RELEASE_TMP/$ASSET" -C "$_TRY_RELEASE_TMP"
  install_binary "$_TRY_RELEASE_TMP/allmystuff"
  _cleanup_try_release
  trap - EXIT INT TERM
  return 0
}

_TRY_GUI_TMP=""
_cleanup_try_gui() {
  if [ -n "$_TRY_GUI_TMP" ] && [ -d "$_TRY_GUI_TMP" ]; then
    rm -rf "$_TRY_GUI_TMP"
  fi
  _TRY_GUI_TMP=""
}

# Best-effort GUI install: fetch the portable `allmystuff-gui` tarball
# and drop it next to the CLI. Returns non-zero (without aborting the
# overall install) if the asset is missing or unreachable — an older
# release may predate the GUI binary, and the CLI is the part that
# must succeed.
try_release_gui() {
  if ! command -v curl >/dev/null 2>&1; then
    return 1
  fi
  api="https://api.github.com/repos/${REPO}/releases/latest"
  if ! json="$(curl -fsSL "$api" 2>/dev/null)"; then
    warn "GitHub releases unreachable; skipping GUI."
    return 1
  fi
  url="$(printf '%s' "$json" | grep -Eo "https://[^\"]+/${GUI_ASSET}" | head -n1 || true)"
  if [ -z "$url" ]; then
    warn "No GUI asset matched ${GUI_ASSET} in the latest release."
    return 1
  fi
  sha_url="${url}.sha256"
  log "Downloading $url"
  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would download $url"
    return 0
  fi
  _TRY_GUI_TMP="$(mktemp -d)"
  trap _cleanup_try_gui EXIT INT TERM
  curl -fsSL "$url" -o "$_TRY_GUI_TMP/$GUI_ASSET"
  if curl -fsSL "$sha_url" -o "$_TRY_GUI_TMP/$GUI_ASSET.sha256" 2>/dev/null; then
    # Explicit gate — set -e is suspended in this function's call context.
    if ! (cd "$_TRY_GUI_TMP" && (sha256sum -c "$GUI_ASSET.sha256" 2>/dev/null || shasum -a 256 -c "$GUI_ASSET.sha256")); then
      warn "SHA256 verification failed for $GUI_ASSET — not installing the GUI."
      _cleanup_try_gui
      trap - EXIT INT TERM
      return 1
    fi
  else
    warn "No SHA256 sidecar for GUI; skipping integrity check."
  fi
  tar -xzf "$_TRY_GUI_TMP/$GUI_ASSET" -C "$_TRY_GUI_TMP"
  install_gui_binary "$_TRY_GUI_TMP/allmystuff-gui"
  _cleanup_try_gui
  trap - EXIT INT TERM
  return 0
}

_TRY_SERVE_TMP=""
_cleanup_try_serve() {
  if [ -n "$_TRY_SERVE_TMP" ] && [ -d "$_TRY_SERVE_TMP" ]; then
    rm -rf "$_TRY_SERVE_TMP"
  fi
  _TRY_SERVE_TMP=""
}

# Best-effort node install: fetch the portable `allmystuff-serve` tarball
# and drop it next to the CLI. This is the headless node `allmystuff serve`
# runs (and `allmystuff service` installs); without it those two commands
# print a hint pointing here. Returns non-zero (without aborting the overall
# install) if the asset is missing — an older release may predate it.
try_release_serve() {
  if ! command -v curl >/dev/null 2>&1; then
    return 1
  fi
  api="https://api.github.com/repos/${REPO}/releases/latest"
  if ! json="$(curl -fsSL "$api" 2>/dev/null)"; then
    warn "GitHub releases unreachable; skipping the node binary."
    return 1
  fi
  url="$(printf '%s' "$json" | grep -Eo "https://[^\"]+/${SERVE_ASSET}" | head -n1 || true)"
  if [ -z "$url" ]; then
    warn "No node asset matched ${SERVE_ASSET} in the latest release."
    return 1
  fi
  sha_url="${url}.sha256"
  log "Downloading $url"
  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would download $url"
    return 0
  fi
  _TRY_SERVE_TMP="$(mktemp -d)"
  trap _cleanup_try_serve EXIT INT TERM
  curl -fsSL "$url" -o "$_TRY_SERVE_TMP/$SERVE_ASSET"
  if curl -fsSL "$sha_url" -o "$_TRY_SERVE_TMP/$SERVE_ASSET.sha256" 2>/dev/null; then
    if ! (cd "$_TRY_SERVE_TMP" && (sha256sum -c "$SERVE_ASSET.sha256" 2>/dev/null || shasum -a 256 -c "$SERVE_ASSET.sha256")); then
      warn "SHA256 verification failed for $SERVE_ASSET — not installing the node binary."
      _cleanup_try_serve
      trap - EXIT INT TERM
      return 1
    fi
  else
    warn "No SHA256 sidecar for the node binary; skipping integrity check."
  fi
  tar -xzf "$_TRY_SERVE_TMP/$SERVE_ASSET" -C "$_TRY_SERVE_TMP"
  install_serve_binary "$_TRY_SERVE_TMP/allmystuff-serve"
  _cleanup_try_serve
  trap - EXIT INT TERM
  return 0
}

_TRY_AMST_TMP=""
_cleanup_try_amst() {
  if [ -n "$_TRY_AMST_TMP" ] && [ -d "$_TRY_AMST_TMP" ]; then
    rm -rf "$_TRY_AMST_TMP"
  fi
  _TRY_AMST_TMP=""
}

# Best-effort amst (AMSTerm) install: fetch the portable `amst` tarball and
# drop it next to the CLI — the standalone mesh terminal (`amst <machine>`
# opens a shell on a machine you own). Returns non-zero (without aborting the
# overall install) if the asset is missing — an older release may predate it.
# This installs only the binary; the app launcher and the "AMSTerm here"
# right-click menu are scripts/install-amst's job.
try_release_amst() {
  if ! command -v curl >/dev/null 2>&1; then
    return 1
  fi
  api="https://api.github.com/repos/${REPO}/releases/latest"
  if ! json="$(curl -fsSL "$api" 2>/dev/null)"; then
    warn "GitHub releases unreachable; skipping amst."
    return 1
  fi
  url="$(printf '%s' "$json" | grep -Eo "https://[^\"]+/${AMST_ASSET}" | head -n1 || true)"
  if [ -z "$url" ]; then
    warn "No amst asset matched ${AMST_ASSET} in the latest release."
    return 1
  fi
  sha_url="${url}.sha256"
  log "Downloading $url"
  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would download $url"
    return 0
  fi
  _TRY_AMST_TMP="$(mktemp -d)"
  trap _cleanup_try_amst EXIT INT TERM
  curl -fsSL "$url" -o "$_TRY_AMST_TMP/$AMST_ASSET"
  if curl -fsSL "$sha_url" -o "$_TRY_AMST_TMP/$AMST_ASSET.sha256" 2>/dev/null; then
    if ! (cd "$_TRY_AMST_TMP" && (sha256sum -c "$AMST_ASSET.sha256" 2>/dev/null || shasum -a 256 -c "$AMST_ASSET.sha256")); then
      warn "SHA256 verification failed for $AMST_ASSET — not installing amst."
      _cleanup_try_amst
      trap - EXIT INT TERM
      return 1
    fi
  else
    warn "No SHA256 sidecar for amst; skipping integrity check."
  fi
  tar -xzf "$_TRY_AMST_TMP/$AMST_ASSET" -C "$_TRY_AMST_TMP"
  install_amst_binary "$_TRY_AMST_TMP/amst"
  _cleanup_try_amst
  trap - EXIT INT TERM
  return 0
}

# amst builds with a plain `cargo build --release --bin amst` — no media or
# Tauri toolchain — so unlike the GUI/node it can be built from source here
# too. Reuses the checkout build_from_source already resolved (REPO_DIR).
build_amst_from_source() {
  if [ -z "$REPO_DIR" ]; then
    return 1
  fi
  log "Building amst from source…"
  ( cd "$REPO_DIR" && cargo build --release --bin amst ) || return 1
  built="$REPO_DIR/target/release/amst"
  if [ ! -x "$built" ]; then
    return 1
  fi
  install_amst_binary "$built"
}

# ---------------------------------------------------------------------------
# The mesh daemon. The desktop app's live mode runs on `myownmesh`
# (demo mode needs nothing), so the installer makes sure a usable daemon
# is in place — without ever failing the app install over it:
#
#   1. installed and new enough (>= the .myownmesh-rev pin) → used as-is;
#   2. installed but older → asked to update itself (`myownmesh update`);
#   3. missing → the latest MyOwnMesh release is installed next to the
#      app, where the app finds it without any PATH refresh.

install_mesh_binary() {
  src="$1"
  mkdir -p "$PREFIX_DIR" 2>/dev/null || sudo mkdir -p "$PREFIX_DIR"
  if [ -w "$PREFIX_DIR" ]; then
    install -m 0755 "$src" "$PREFIX_DIR/myownmesh"
  else
    sudo install -m 0755 "$src" "$PREFIX_DIR/myownmesh"
  fi
  log "Installed: $PREFIX_DIR/myownmesh"
}

_TRY_MESH_TMP=""
_cleanup_try_mesh() {
  if [ -n "$_TRY_MESH_TMP" ] && [ -d "$_TRY_MESH_TMP" ]; then
    rm -rf "$_TRY_MESH_TMP"
  fi
  _TRY_MESH_TMP=""
}

# Fetch the daemon tarball from MyOwnMesh's latest release (SHA-256
# verified, like the app's own assets) and install it next to the app.
try_release_mesh() {
  if ! command -v curl >/dev/null 2>&1; then
    return 1
  fi
  api="https://api.github.com/repos/${MESH_REPO}/releases/latest"
  if ! json="$(curl -fsSL "$api" 2>/dev/null)"; then
    warn "MyOwnMesh releases unreachable."
    return 1
  fi
  url="$(printf '%s' "$json" | grep -Eo "https://[^\"]+/${MESH_ASSET}" | head -n1 || true)"
  if [ -z "$url" ]; then
    warn "No release asset matched ${MESH_ASSET} in MyOwnMesh's latest release."
    return 1
  fi
  sha_url="${url}.sha256"
  log "Downloading $url"
  _TRY_MESH_TMP="$(mktemp -d)"
  trap _cleanup_try_mesh EXIT INT TERM
  if ! curl -fsSL "$url" -o "$_TRY_MESH_TMP/$MESH_ASSET"; then
    _cleanup_try_mesh
    trap - EXIT INT TERM
    return 1
  fi
  if curl -fsSL "$sha_url" -o "$_TRY_MESH_TMP/$MESH_ASSET.sha256" 2>/dev/null; then
    if ! (cd "$_TRY_MESH_TMP" && (sha256sum -c "$MESH_ASSET.sha256" 2>/dev/null || shasum -a 256 -c "$MESH_ASSET.sha256")); then
      warn "SHA256 verification failed for $MESH_ASSET — not installing the daemon."
      _cleanup_try_mesh
      trap - EXIT INT TERM
      return 1
    fi
  else
    warn "No SHA256 sidecar for the daemon; skipping integrity check."
  fi
  if ! tar -xzf "$_TRY_MESH_TMP/$MESH_ASSET" -C "$_TRY_MESH_TMP"; then
    _cleanup_try_mesh
    trap - EXIT INT TERM
    return 1
  fi
  install_mesh_binary "$_TRY_MESH_TMP/myownmesh"
  _cleanup_try_mesh
  trap - EXIT INT TERM
  return 0
}

# version_ge A B — true when dotted version A >= B, comparing the numeric
# major/minor/patch fields.
version_ge() {
  IFS=. read -r a1 a2 a3 <<EOF
$1
EOF
  IFS=. read -r b1 b2 b3 <<EOF
$2
EOF
  a1="${a1%%[!0-9]*}"; a2="${a2%%[!0-9]*}"; a3="${a3%%[!0-9]*}"
  b1="${b1%%[!0-9]*}"; b2="${b2%%[!0-9]*}"; b3="${b3%%[!0-9]*}"
  a1="${a1:-0}"; a2="${a2:-0}"; a3="${a3:-0}"
  b1="${b1:-0}"; b2="${b2:-0}"; b3="${b3:-0}"
  if [ "$a1" -ne "$b1" ]; then [ "$a1" -gt "$b1" ]; return; fi
  if [ "$a2" -ne "$b2" ]; then [ "$a2" -gt "$b2" ]; return; fi
  [ "$a3" -ge "$b3" ]
}

# The minimum daemon version this app wants: the rev pinned in
# .myownmesh-rev, read from the checkout when running from one, fetched
# from the repo otherwise. Prints nothing when the pin is unreachable or
# isn't a version tag (a sha pin can't be compared) — any installed
# daemon passes then.
mesh_min_version() {
  rev=""
  if [ -f "$0" ] && [ -f "$(dirname "$0")/../.myownmesh-rev" ]; then
    rev="$(cat "$(dirname "$0")/../.myownmesh-rev" 2>/dev/null || true)"
  fi
  if [ -z "$rev" ] && command -v curl >/dev/null 2>&1; then
    rev="$(curl -fsSL "https://raw.githubusercontent.com/${REPO}/main/.myownmesh-rev" 2>/dev/null || true)"
  fi
  rev="$(printf '%s' "$rev" | tr -d '[:space:]')"
  case "$rev" in
    v[0-9]*) printf '%s' "${rev#v}" ;;
    *) ;;
  esac
}

# `myownmesh --version` → "0.2.9" (empty when it prints no version we can
# recognize). Scans the output for the first semver-looking token rather than
# trusting a fixed column, and falls back to stderr — a build suffix or a
# version printed on stderr must not make a good daemon look unanswered.
installed_mesh_version() {
  v="$("$1" --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+(\.[0-9]+)?' | head -n1)"
  if [ -z "$v" ]; then
    v="$("$1" --version 2>&1 | grep -Eo '[0-9]+\.[0-9]+(\.[0-9]+)?' | head -n1)"
  fi
  printf '%s' "$v"
}

ensure_mesh() {
  # Prefer a daemon sitting next to the app (where we'd install one —
  # the app checks there first too), then PATH.
  existing=""
  if [ -x "$PREFIX_DIR/myownmesh" ]; then
    existing="$PREFIX_DIR/myownmesh"
  elif command -v myownmesh >/dev/null 2>&1; then
    existing="$(command -v myownmesh)"
  fi
  min="$(mesh_min_version || true)"

  if [ -n "$existing" ]; then
    ver="$(installed_mesh_version "$existing" || true)"
    if [ -n "$ver" ] && { [ -z "$min" ] || version_ge "$ver" "$min"; }; then
      if [ -n "$min" ]; then
        log "Mesh: using the installed myownmesh v$ver at $existing (needs v$min+)."
      else
        log "Mesh: using the installed myownmesh v$ver at $existing."
      fi
      return 0
    fi
    if [ -n "$ver" ]; then
      log "Mesh: installed myownmesh is v$ver but this release wants v$min+."
    else
      log "Mesh: $existing didn't answer --version."
    fi
    if [ "$DRY_RUN" = "true" ]; then
      log "(dry-run) would ask it to update itself: myownmesh update"
      return 0
    fi
    log "Asking it to update itself (myownmesh update)…"
    # Its own output lands here; the re-check below is what decides.
    "$existing" update || true
    ver="$(installed_mesh_version "$existing" || true)"
    if [ -n "$ver" ] && { [ -z "$min" ] || version_ge "$ver" "$min"; }; then
      log "Mesh: myownmesh is now v$ver."
    elif [ -z "$ver" ]; then
      # It ran `update` but prints no version we can read. That's a
      # version-probe miss, not a failed update — don't cry wolf.
      log "Mesh: myownmesh is installed and responded to 'update', but didn't"
      log "report a readable version. Assuming it's fine; the app will use it."
    else
      warn "Mesh: couldn't bring myownmesh up to v${min:-a readable version} (see above)."
      warn "The app still runs — an older daemon just lacks the newer mesh"
      warn "features. Retry later with: myownmesh update"
    fi
    return 0
  fi

  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would install the myownmesh daemon ($MESH_ASSET) next to the app"
    return 0
  fi
  log "Mesh: no myownmesh daemon found — installing it next to the app…"
  if try_release_mesh; then
    ver="$(installed_mesh_version "$PREFIX_DIR/myownmesh" || true)"
    log "Mesh: installed myownmesh${ver:+ v$ver} — the app starts it automatically."
  else
    warn "Mesh: couldn't fetch the daemon. The app still opens (demo graph);"
    warn "for live machines, re-run this installer later or use MyOwnMesh's:"
    warn "  curl -fsSL https://raw.githubusercontent.com/${MESH_REPO}/main/scripts/install.sh | sh -s -- --no-gui"
  fi
}

build_from_source() {
  log "Building from source…"
  if ! command -v cargo >/dev/null 2>&1; then
    err "cargo not found. Install Rust via https://rustup.rs first."
    exit 1
  fi
  if ! command -v git >/dev/null 2>&1; then
    err "git is required to build from source."
    exit 1
  fi
  if [ -f Cargo.toml ] && [ -d crates/allmystuff-cli ]; then
    repo_dir="$(pwd)"
    log "Using current directory as source: $repo_dir"
  else
    repo_dir="$(mktemp -d)/AllMyStuff"
    log "Cloning into $repo_dir"
    if [ "$DRY_RUN" != "true" ]; then
      git clone --depth 1 "https://github.com/${REPO}.git" "$repo_dir"
    fi
  fi
  # Expose the resolved source dir so amst can build from the same checkout.
  REPO_DIR="$repo_dir"
  if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would build in $repo_dir"
    return 0
  fi
  ( cd "$repo_dir" && cargo build --release --bin allmystuff )
  built="$repo_dir/target/release/allmystuff"
  if [ ! -x "$built" ]; then
    err "Build did not produce $built"
    exit 1
  fi
  install_binary "$built"
}

INSTALLED_FROM_RELEASE=false
if [ "$FORCE_SOURCE" = "true" ] || ! try_release; then
  build_from_source
else
  INSTALLED_FROM_RELEASE=true
fi

# Desktop app (allmystuff-gui). On by default — it's small and lets a
# bare `allmystuff` open the app. `--no-gui` skips it. Only attempted
# on the release path; building the GUI from source needs the full
# Tauri/pnpm toolchain, which is out of scope for a curl|sh installer.
GUI_INSTALLED=false
if [ "$INSTALL_GUI" = "true" ]; then
  if [ "$INSTALLED_FROM_RELEASE" = "true" ]; then
    if try_release_gui; then
      GUI_INSTALLED=true
    else
      err "Full AllMyStuff install failed: the GUI binary was not installed."
      exit 1
    fi
  elif [ "$DRY_RUN" = "true" ]; then
    log "(dry-run) would install the GUI binary ($GUI_ASSET) next to allmystuff"
  else
    warn "Built the CLI from source; skipping the GUI binary (needs the Tauri/pnpm toolchain)."
    warn "Build it with:  cd gui && pnpm install && pnpm tauri build"
  fi
fi

# The headless node binary (allmystuff-serve) — what `allmystuff serve` runs
# and `allmystuff service` installs. Installed on every release install (it's
# the whole point of a headless --no-gui box, and small enough to ship with a
# desktop install too). A from-source CLI build skips it: it links the media
# toolchain, out of scope for a curl|sh installer.
SERVE_INSTALLED=false
if [ "$INSTALLED_FROM_RELEASE" = "true" ]; then
  if try_release_serve; then
    SERVE_INSTALLED=true
  else
    err "Full AllMyStuff install failed: allmystuff-serve was not installed."
    exit 1
  fi
elif [ "$DRY_RUN" = "true" ]; then
  log "(dry-run) would install the node binary ($SERVE_ASSET) next to allmystuff"
else
  warn "Built the CLI from source; skipping the node binary (needs the media toolchain)."
  warn "Build it with:  cargo build --release --manifest-path node/Cargo.toml"
fi

if [ "$SERVE_INSTALLED" = "true" ] || [ "$DRY_RUN" = "true" ]; then
  ensure_drive_mount_helper
fi

# AMSTerm (amst) — the standalone mesh terminal: a shell on any machine you
# own, from your own terminal. On by default (--no-amst skips it). Unlike the
# GUI/node it installs in BOTH paths: the portable binary on a release install,
# or `cargo build --bin amst` from source (it needs no media/Tauri toolchain).
# Only the binary lands here; the app launcher and "AMSTerm here" right-click
# menu are scripts/install-amst's job.
AMST_INSTALLED=false
if [ "$INSTALL_AMST" != "true" ]; then
  log "Skipping AMSTerm (--no-amst)."
elif [ "$DRY_RUN" = "true" ]; then
  if [ "$INSTALLED_FROM_RELEASE" = "true" ]; then
    log "(dry-run) would install the amst binary ($AMST_ASSET) next to allmystuff"
  else
    log "(dry-run) would build and install amst from source (cargo build --bin amst)"
  fi
elif [ "$INSTALLED_FROM_RELEASE" = "true" ]; then
  if try_release_amst; then
    AMST_INSTALLED=true
  else
    err "Full AllMyStuff install failed: amst was not installed."
    exit 1
  fi
else
  if build_amst_from_source; then
    AMST_INSTALLED=true
  else
    warn "Couldn't build amst from source; install it separately: scripts/install-amst.sh --from-source"
  fi
fi

# Mesh daemon — see the block above ensure_mesh for the rules. Both the
# desktop app *and* the headless node (`allmystuff serve`) run on it, so it's
# installed whenever either of them is; a from-source build skips it (a GUI
# built from gui/ bundles its own, and scan/capabilities need no daemon).
if [ "$INSTALL_MESH" != "true" ]; then
  log "Skipping the mesh daemon (--no-mesh)."
elif [ "$GUI_INSTALLED" = "true" ] || [ "$SERVE_INSTALLED" = "true" ]; then
  ensure_mesh
elif [ "$DRY_RUN" = "true" ]; then
  ensure_mesh
else
  log "Mesh: skipped — neither the desktop app nor the node binary was"
  log "installed (only they use the daemon; scan/capabilities don't)."
fi

if [ "$DRY_RUN" != "true" ]; then
  ensure_on_path
fi

log "Done."
log ""
log "Quick start:"
if [ "$INSTALL_GUI" = "true" ]; then
  log "  allmystuff                 # open the desktop app"
fi
log "  allmystuff scan            # pretty inventory of this machine"
log "  allmystuff capabilities    # what this machine would expose on the mesh"
log "  allmystuff update          # update to the latest release"
if [ "$SERVE_INSTALLED" = "true" ] || [ "$DRY_RUN" = "true" ]; then
  log "  allmystuff serve           # run this machine on the mesh, headless (no GUI)"
  log "  allmystuff service install # …and keep it running across reboots (one service runs"
  log "                             # both the node and the myownmesh daemon)"
fi
if [ "$AMST_INSTALLED" = "true" ] || [ "$DRY_RUN" = "true" ]; then
  log "  amst                       # open a shell on THIS machine over the mesh (AMSTerm)"
  log "  amst <machine>             # …or on another machine you own (amst --list to see them)"
fi
if [ "$INSTALL_GUI" = "true" ]; then
  log ""
  log "The app opens into a demo graph even with no mesh. Live machines run"
  log "on the 'myownmesh' daemon (handled above), which the app starts and"
  log "manages automatically."
  if [ "$INSTALL_MESH" != "true" ]; then
    log "You skipped it (--no-mesh) — when you want live mode:"
    log "  curl -fsSL https://raw.githubusercontent.com/${MESH_REPO}/main/scripts/install.sh | sh -s -- --no-gui"
  fi
fi
