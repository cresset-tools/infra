#!/usr/bin/env bash
# Update the pinned cresset-tools/mageos-maker source — and every hash that
# moved with it — in hosts/origin/mageos-maker.nix.
#
# The module pins four hashes:
#   - src.rev / src.hash  the fetchFromGitHub source (rev + its FOD hash)
#   - npmDepsHash         buildNpmPackage's node_modules FOD (package-lock.json)
#   - vendorHash          buildComposerProject's vendor/ FOD (composer.lock)
#
# rev + src.hash move on every source bump. npmDepsHash / vendorHash only
# move when the corresponding lockfile changed, so this script diffs the
# lockfiles between the old pin and the new rev and recomputes only what's
# needed:
#   - npmDepsHash via `prefetch-npm-deps` (pure, no build, arch-independent)
#   - vendorHash  via the fake-hash rebuild trick — this realizes the FOD,
#                 which is aarch64-linux, so it needs a build host (the box,
#                 same as `nix run .#switch`). Pass --build-host for it.
#
# Usage:
#   scripts/update-mageos-maker.sh [REV] [--build-host root@HOST]
#
#   REV            git rev/branch to pin (default: origin/main HEAD)
#   --build-host   remote builder for the vendorHash recompute (only used
#                  when composer.lock changed)
#
# Re-running with no changes is a no-op (idempotent): it re-pins the same
# rev/hashes and reports "unchanged".
set -euo pipefail

OWNER=cresset-tools
REPO=mageos-maker
BRANCH=main

repo_root() { cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd; }
ROOT="$(repo_root)"
NIXFILE="$ROOT/hosts/origin/mageos-maker.nix"

REV=""
BUILD_HOST=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --build-host) BUILD_HOST="$2"; shift 2 ;;
    --build-host=*) BUILD_HOST="${1#*=}"; shift ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) REV="$1"; shift ;;
  esac
done

nix() { command nix --extra-experimental-features 'nix-command flakes' "$@"; }
raw() { curl -fsSL "https://raw.githubusercontent.com/$OWNER/$REPO/$1/$2"; }
# Read the value of `<key> = "...";` from the nix file (first match).
nixval() { sed -n "s/.*$1 = \"\\([^\"]*\\)\";.*/\\1/p" "$NIXFILE" | head -n1; }
# Replace the value of `<key> = "...";` in place (first occurrence only).
# key/value travel via env, not string-interpolation into the perl program:
# sha256 hashes contain / and + which would break the s/// delimiters.
setval() {
  SETVAL_KEY="$1" SETVAL_VAL="$2" perl -0pi -e \
    'BEGIN { $done = 0; $k = quotemeta $ENV{SETVAL_KEY}; $v = $ENV{SETVAL_VAL} }
     s/(\b$k = ")[^"]*(";)/${1}${v}${2}/ unless $done++' "$NIXFILE"
}

[ -f "$NIXFILE" ] || { echo "not found: $NIXFILE" >&2; exit 1; }

if [ -z "$REV" ]; then
  echo "resolving $OWNER/$REPO $BRANCH HEAD…" >&2
  REV="$(git ls-remote "https://github.com/$OWNER/$REPO" "$BRANCH" | cut -f1)"
fi
[ -n "$REV" ] || { echo "could not resolve rev" >&2; exit 1; }

OLD_REV="$(nixval rev)"
echo "old rev: $OLD_REV" >&2
echo "new rev: $REV" >&2
if [ "$OLD_REV" = "$REV" ]; then
  echo "rev unchanged — re-verifying hashes anyway." >&2
fi

# --- src hash (always) ---
echo "prefetching source hash…" >&2
SRC_HASH="$(nix run nixpkgs#nix-prefetch-github -- "$OWNER" "$REPO" --rev "$REV" 2>/dev/null \
  | jq -r '.hash // .sha256')"
[ -n "$SRC_HASH" ] && [ "$SRC_HASH" != null ] || { echo "failed to prefetch src hash" >&2; exit 1; }
echo "  src.hash = $SRC_HASH" >&2

# --- did the lockfiles change between old and new rev? ---
lock_changed() {
  local f="$1"
  local o n
  o="$(raw "$OLD_REV" "$f" | sha256sum | cut -d' ' -f1)" || return 0
  n="$(raw "$REV" "$f" | sha256sum | cut -d' ' -f1)" || return 0
  [ "$o" != "$n" ]
}

NPM_HASH="$(nixval npmDepsHash)"
if lock_changed package-lock.json; then
  echo "package-lock.json changed — recomputing npmDepsHash…" >&2
  tmp="$(mktemp)"; raw "$REV" package-lock.json > "$tmp"
  NPM_HASH="$(nix run nixpkgs#prefetch-npm-deps -- "$tmp")"
  rm -f "$tmp"
  echo "  npmDepsHash = $NPM_HASH" >&2
else
  echo "package-lock.json unchanged — keeping npmDepsHash." >&2
fi

VENDOR_HASH="$(nixval vendorHash)"
VENDOR_CHANGED=0
if lock_changed composer.lock; then
  VENDOR_CHANGED=1
  echo "composer.lock changed — vendorHash must be recomputed." >&2
else
  echo "composer.lock unchanged — keeping vendorHash." >&2
fi

# --- write rev + the hashes we have so far ---
setval rev "$REV"
setval hash "$SRC_HASH"          # the fetchFromGitHub hash (first `hash =`)
setval npmDepsHash "$NPM_HASH"
[ "$VENDOR_CHANGED" = 0 ] && setval vendorHash "$VENDOR_HASH"

# --- vendorHash via fake-hash rebuild (only when composer.lock changed) ---
if [ "$VENDOR_CHANGED" = 1 ]; then
  if [ -z "$BUILD_HOST" ]; then
    echo "ERROR: composer.lock changed but no --build-host given." >&2
    echo "vendorHash is an aarch64 FOD; re-run with --build-host root@<box>." >&2
    exit 1
  fi
  FAKE="sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
  setval vendorHash "$FAKE"
  echo "building to extract vendorHash (on $BUILD_HOST)…" >&2
  err="$(mktemp)"
  if nix build "$ROOT#nixosConfigurations.origin.config.system.build.toplevel" \
        --build-host "$BUILD_HOST" --no-link --print-build-logs 2>"$err"; then
    echo "build succeeded with fake vendorHash — unexpected; leaving as-is." >&2
  fi
  GOT="$(grep -Eo 'got: +sha256-[A-Za-z0-9+/=]+' "$err" | head -n1 | grep -Eo 'sha256-[A-Za-z0-9+/=]+')"
  rm -f "$err"
  [ -n "$GOT" ] || { echo "could not extract vendorHash from build output" >&2; exit 1; }
  echo "  vendorHash = $GOT" >&2
  setval vendorHash "$GOT"
fi

echo >&2
echo "updated $NIXFILE:" >&2
grep -nE 'rev =|hash =|vendorHash|npmDepsHash' "$NIXFILE" >&2
