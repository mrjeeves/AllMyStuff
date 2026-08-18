#!/usr/bin/env bash
# Resolve every Cargo dependency represented by a repo pin, then prove the
# committed lock agrees.  The desktop/node MyOwnMesh dependency is a release
# sidecar rather than a Rust crate; the mobile workspace is the one Cargo lock
# that must resolve `.myownmesh-rev`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PIN="$(tr -d '[:space:]' < "$ROOT/.myownmesh-rev")"
MANIFEST="$ROOT/gui/mobile/Cargo.toml"
LOCK="$ROOT/gui/mobile/Cargo.lock"
MODE="${1:-sync}"

fail() { echo "error: $*" >&2; exit 1; }

case "$MODE" in
  sync|--check) ;;
  *) fail "usage: $0 [--check]" ;;
esac

[[ "$PIN" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][A-Za-z0-9.-]+)?$ ]] \
  || fail ".myownmesh-rev contains invalid release pin '$PIN'"

for package in myownmesh myownmesh-core; do
  grep -Eq "^${package} = .*tag = \"${PIN}\"" "$MANIFEST" \
    || fail "$MANIFEST does not pin $package to .myownmesh-rev ($PIN)"
done

lock_matches_pin() {
  [[ -f "$LOCK" ]] || return 1
  grep -q 'source = "git+https://github.com/mrjeeves/MyOwnMesh?tag=' "$LOCK" \
    && ! grep 'source = "git+https://github.com/mrjeeves/MyOwnMesh?tag=' "$LOCK" \
      | grep -Fvq "?tag=${PIN}#"
}

if [[ "$MODE" == "sync" ]] && ! lock_matches_pin; then
  # Cargo resolves all crates from this tagged git source together, including
  # the transitive services/signaling/updater packages recorded in the lock.
  cargo update --manifest-path "$MANIFEST" -p myownmesh -p myownmesh-core
fi

[[ -f "$LOCK" ]] || fail "$LOCK is missing"
mapfile -t sources < <(grep 'source = "git+https://github.com/mrjeeves/MyOwnMesh?tag=' "$LOCK" || true)
((${#sources[@]} > 0)) || fail "$LOCK contains no pinned MyOwnMesh packages"
for source in "${sources[@]}"; do
  [[ "$source" == *"?tag=${PIN}#"* ]] \
    || fail "$LOCK disagrees with .myownmesh-rev ($PIN): $source"
done

echo "pinned Cargo locks agree with MyOwnMesh $PIN"
