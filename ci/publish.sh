#!/usr/bin/env bash
# Publishes the workspace to crates.io in dependency order.
#
# The order matters because crates.io will not accept a crate whose dependencies are not there yet,
# and it is written down here rather than derived because a wrong answer is a half published release
# that somebody has to clean up by hand.
#
# The delays matter more. crates.io limits new crate names much harder than new versions of an
# existing crate, so the first release of this workspace is the slow one and every release after it
# is quick. The script tells the two apart by asking crates.io whether the name exists, and it skips
# anything that is already published at this version so that a rerun after a failure picks up where
# it stopped instead of starting again.
#
# Usage: ci/publish.sh <version> [--dry-run]
set -euo pipefail

VERSION="${1:?usage: ci/publish.sh <version> [--dry-run]}"
DRY_RUN="${2:-}"

# Dependency order. iris-abi first because everything else is downstream of it, and iris last
# because it is the command line tool and depends on most of the tree.
CRATES=(
  iris-abi
  iris-format
  iris-guard
  iris-native
  iris-source
  iris-trust
  iris-decoder
  iris-vm
  iris-runtime
  iris
)

# How long to wait after publishing a name that did not exist before, once the burst is used up.
# crates.io lets a handful of new names through at once and then throttles to roughly one every ten
# minutes, and being throttled halfway through a workspace is much more annoying than being slow.
NEW_CRATE_DELAY="${NEW_CRATE_DELAY:-620}"

# How many new names go through before the throttle starts. Set one below what crates.io documents,
# because being wrong in this direction costs ten minutes and being wrong in the other direction
# costs a failed release.
NEW_CRATE_BURST="${NEW_CRATE_BURST:-4}"

# How long to wait after publishing a new version of a crate that already exists. This one only has
# to be long enough for the index to catch up, which cargo already waits for, so it is short.
EXISTING_CRATE_DELAY="${EXISTING_CRATE_DELAY:-20}"

api() {
  curl --silent --show-error --location \
    --header "User-Agent: iris release (https://github.com/tamnd/iris)" \
    "https://crates.io/api/v1/crates/$1"
}

# Three answers: "missing" for a name nobody has taken, "stale" for a name that exists without this
# version, and "done" for a name that already has it.
state_of() {
  local body
  body="$(api "$1")"
  if printf '%s' "$body" | grep -q '"errors"'; then
    echo "missing"
  elif printf '%s' "$body" | grep -q "\"num\":\"$VERSION\""; then
    echo "done"
  else
    echo "stale"
  fi
}

new_names=0
for crate in "${CRATES[@]}"; do
  state="$(state_of "$crate")"
  case "$state" in
    done)
      echo "== $crate $VERSION is already on crates.io, skipping"
      continue
      ;;
    missing) echo "== $crate is a new name, publishing $VERSION" ;;
    stale) echo "== $crate exists, publishing $VERSION" ;;
  esac

  if [ -n "$DRY_RUN" ]; then
    cargo publish --locked --dry-run -p "$crate"
    continue
  fi

  # A single retry, because the failure this guards against is a rate limit or an index that has not
  # caught up, and both of those are fixed by waiting. Anything that fails twice is a real problem
  # and should stop the release rather than be retried into a worse state.
  if ! cargo publish --locked -p "$crate"; then
    echo "-- $crate did not publish, waiting $NEW_CRATE_DELAY seconds and trying once more"
    sleep "$NEW_CRATE_DELAY"
    cargo publish --locked -p "$crate"
  fi

  if [ "$state" = missing ]; then
    new_names=$((new_names + 1))
    if [ "$new_names" -ge "$NEW_CRATE_BURST" ]; then
      echo "-- the burst is used up, waiting $NEW_CRATE_DELAY seconds before the next new name"
      sleep "$NEW_CRATE_DELAY"
      continue
    fi
  fi
  sleep "$EXISTING_CRATE_DELAY"
done

echo "done"
