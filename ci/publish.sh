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

# crates.io wants a user agent that says who is calling and answers 403 to anything that does not
# send one, curl's default included. That matters more than it looks: a 403 body is neither the
# crate nor an error about the crate, so a check that only looks at the body reads it as a name that
# exists, and the whole point of this script is telling those two apart.
#
# Three answers: "missing" for a name nobody has taken, "stale" for a name that exists without this
# version, and "done" for a name that already has it. Anything else stops the release, because
# guessing here is how a first publish walks into the rate limit at full speed.
state_of() {
  local response status body
  response="$(curl --silent --show-error --location --write-out '\n%{http_code}' \
    --header "User-Agent: iris release (https://github.com/tamnd/iris)" \
    "https://crates.io/api/v1/crates/$1")"
  status="${response##*$'\n'}"
  body="${response%$'\n'*}"

  case "$status" in
    404) echo "missing" ;;
    200)
      if printf '%s' "$body" | grep -q "\"num\":\"$VERSION\""; then
        echo "done"
      else
        echo "stale"
      fi
      ;;
    *)
      echo "crates.io answered $status for $1, which is neither yes nor no" >&2
      return 1
      ;;
  esac
}

# A dry run is one command rather than ten, because `cargo publish --dry-run` on a single crate
# resolves its dependencies against crates.io, and on a first release the crate it depends on is not
# there yet, so every package after the first one fails for a reason that has nothing to do with the
# release. `cargo package --workspace` builds a temporary registry out of the workspace, so each
# package is verified against the versions that are about to go out rather than the ones that exist.
if [ -n "$DRY_RUN" ]; then
  echo "== packaging and verifying every crate, uploading nothing"
  cargo package --workspace --locked
  echo "done"
  exit 0
fi

# The token is checked before anything is uploaded, because the way this fails otherwise is bad out
# of proportion to the mistake. cargo does not find out the token is wrong until it has packaged,
# verified and uploaded a crate, and the retry in the loop below then waits ten minutes and does the
# whole thing again before giving up. One request here turns a twenty minute failure into a one
# second one. Nothing prints the token, only what crates.io thought of it.
if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
  echo "CARGO_REGISTRY_TOKEN is not set, so there is nothing to publish with" >&2
  exit 1
fi
me="$(curl --silent --output /dev/null --write-out '%{http_code}' \
  --header "User-Agent: iris release (https://github.com/tamnd/iris)" \
  --header "Authorization: $CARGO_REGISTRY_TOKEN" \
  "https://crates.io/api/v1/me")"
if [ "$me" != "200" ]; then
  echo "crates.io answered $me when asked who this token belongs to, so it is not a token it will" >&2
  echo "accept. A token set from a shell file with the quotes still around it is the usual cause." >&2
  exit 1
fi
echo "== the token is one crates.io recognises"

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
