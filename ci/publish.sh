#!/usr/bin/env bash
# Publishes the workspace to crates.io in dependency order.
#
# The order matters because crates.io will not accept a crate whose dependencies are not there yet,
# and it is written down here rather than derived because a wrong answer is a half published release
# that somebody has to clean up by hand.
#
# The waiting matters more. crates.io limits new crate names much harder than new versions of an
# existing crate, so the first release of this workspace is the slow one and every release after it
# is quick. When it throttles a publish it says in the response exactly when the next name is due, so
# this script reads that and sleeps until then rather than guessing. It also skips anything already
# published at this version, so a rerun after a failure picks up where it stopped instead of starting
# again.
#
# Usage: ci/publish.sh <version> [--dry-run]
set -euo pipefail

VERSION="${1:?usage: ci/publish.sh <version> [--dry-run]}"
DRY_RUN="${2:-}"

# Dependency order. iris-abi first because everything else is downstream of it, and iris last
# because it is the command line tool and depends on most of the tree.
#
# This list was wrong once, in the way a hand written list goes wrong: iris-native sat above
# iris-trust and depends on it, which nothing noticed until crates.io refused the upload with three
# names already published. The order is still written down rather than derived, because it is read
# by anybody trying to understand the release, but it is now checked against the manifests before
# anything is uploaded.
CRATES=(
  iris-abi
  iris-format
  iris-guard
  iris-source
  iris-trust
  iris-native
  iris-decoder
  iris-vm
  iris-runtime
  iris
)

# Refuses a list that publishes a crate before something it depends on.
#
# A half published release is the expensive failure here, because crates.io does not let a version be
# taken back. Reading the manifests is a second of work and it is the difference between finding that
# out now and finding it out with three names already gone.
check_order() {
  local crate dep published=() bad=0
  for crate in "${CRATES[@]}"; do
    while read -r dep; do
      [ -n "$dep" ] || continue
      if ! printf '%s\n' "${published[@]:-}" | grep -qx "$dep"; then
        echo "$crate depends on $dep and is published before it" >&2
        bad=1
      fi
    done < <(grep -oE '^iris-[a-z]+' "crates/$crate/Cargo.toml" | sort -u)
    published+=("$crate")
  done

  if [ "$bad" -ne 0 ]; then
    echo "the publish order in this script is not a dependency order, so nothing is uploaded" >&2
    return 1
  fi
  echo "== the publish order matches the manifests"
}

# How long to wait before retrying when crates.io throttled us and did not say when to come back.
# Its limit on new names is roughly one every ten minutes, so ten minutes and a bit is the guess.
RETRY_DELAY="${RETRY_DELAY:-620}"

# On top of whatever time crates.io names, because its clock is not this machine's clock and coming
# back one second early costs another full interval.
RETRY_MARGIN="${RETRY_MARGIN:-60}"

# The longest this script will keep waiting on a single crate before giving up on the whole release.
# Giving up is not expensive: the loop below skips anything already published, so a rerun picks up
# where this one stopped.
MAX_WAIT_PER_CRATE="${MAX_WAIT_PER_CRATE:-2400}"

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

# Seconds from now until a date crates.io named, or nothing if the date cannot be read. GNU date and
# BSD date want to be asked in completely different ways, so both are tried and the caller falls back
# to a fixed wait if neither works.
seconds_until() {
  local when="$1" epoch now
  epoch="$(date -u -d "$when" +%s 2>/dev/null || true)"
  if [ -z "$epoch" ]; then
    epoch="$(date -u -j -f '%a, %d %b %Y %H:%M:%S %Z' "$when" +%s 2>/dev/null || true)"
  fi
  [ -n "$epoch" ] || return 1
  now="$(date -u +%s)"
  echo $((epoch - now))
}

# Publishes one crate, waiting out the rate limit for as long as crates.io asks.
#
# The limit on new names is the whole difficulty of a first release. crates.io lets a small number of
# new names through and then hands out roughly one every ten minutes, and it says in the 429 exactly
# when the next one is due. Reading that is better than guessing at it: a guess that is too short
# burns an attempt and a guess that is too long adds up over ten crates.
#
# Anything that is not a 429 gets one retry and then stops the release. Those failures are index lag,
# which one wait fixes, or a real problem, which no amount of waiting fixes.
publish_crate() {
  local crate="$1" log waited=0 wait until
  log="$(mktemp)"
  trap 'rm -f "$log"' RETURN

  while :; do
    if cargo publish --locked -p "$crate" 2>&1 | tee "$log"; then
      return 0
    fi

    if ! grep -q '429 Too Many Requests' "$log"; then
      echo "-- $crate did not publish and it was not the rate limit, waiting $RETRY_DELAY seconds" \
        "and trying once more"
      sleep "$RETRY_DELAY"
      cargo publish --locked -p "$crate"
      return 0
    fi

    until="$(sed -n 's/.*Please try again after \(.*\) and see.*/\1/p' "$log" | head -n 1)"
    wait=""
    if [ -n "$until" ]; then
      wait="$(seconds_until "$until" || true)"
    fi
    if [ -z "$wait" ] || [ "$wait" -lt 1 ]; then
      wait="$RETRY_DELAY"
    fi
    wait=$((wait + RETRY_MARGIN))

    waited=$((waited + wait))
    if [ "$waited" -gt "$MAX_WAIT_PER_CRATE" ]; then
      echo "$crate has been rate limited for longer than $MAX_WAIT_PER_CRATE seconds, so this run" >&2
      echo "stops here. Everything published so far stays published and rerunning this workflow" >&2
      echo "picks up at $crate rather than starting again." >&2
      return 1
    fi

    echo "-- crates.io is throttling new names, next attempt at $crate in $wait seconds"
    sleep "$wait"
  done
}

# A dry run is one command rather than ten, because `cargo publish --dry-run` on a single crate
# resolves its dependencies against crates.io, and on a first release the crate it depends on is not
# there yet, so every package after the first one fails for a reason that has nothing to do with the
# release. `cargo package --workspace` builds a temporary registry out of the workspace, so each
# package is verified against the versions that are about to go out rather than the ones that exist.
check_order

if [ -n "$DRY_RUN" ]; then
  echo "== packaging and verifying every crate, uploading nothing"
  cargo package --workspace --locked
  echo "done"
  exit 0
fi

# The token is looked at before anything is uploaded, because the way a bad one fails otherwise is
# bad out of proportion to the mistake. cargo does not look at the token until it has packaged the
# crate, verified it and uploaded it, and the retry in the loop below then waits ten minutes and
# does the whole thing again before giving up, so a pair of stray quotes costs twenty minutes to
# find out about.
#
# This is a shape check and not an authorisation check, and the difference is worth being honest
# about. crates.io has no read only endpoint that answers whether a token is live: /me is reserved
# for the website and refuses every API token, and the endpoints that do accept one ignore it. So
# what is checked here is the failure that actually happened, which is a token stored from a shell
# file with the quotes still around it. A token that is well formed but revoked, or one without
# publish scope, is still found the slow way. Nothing here prints the token.
if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
  echo "CARGO_REGISTRY_TOKEN is not set, so there is nothing to publish with" >&2
  exit 1
fi
if [[ ! $CARGO_REGISTRY_TOKEN =~ ^[A-Za-z0-9]{20,}$ ]]; then
  echo "CARGO_REGISTRY_TOKEN is not shaped like a crates.io token, which is twenty or more letters" >&2
  echo "and digits and nothing else. Quotes carried over from a shell file are the usual cause, and" >&2
  echo "crates.io answers that with a 401 about the token format after it has taken the upload." >&2
  exit 1
fi
echo "== the token is shaped like a crates.io token"

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

  publish_crate "$crate"
  sleep "$EXISTING_CRATE_DELAY"
done

echo "done"
