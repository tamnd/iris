# Releasing

Three things happen when a version goes out, and they are deliberately three separate workflows rather than one. A build can be rerun. A GitHub release can be edited. A crates.io publish cannot be taken back, only yanked, so it is the one that is triggered by hand.

## Versions

While the major version is zero, a minor tracks a completed milestone and a patch is everything in between. So `v0.1.0` is the tree where M0 finished, `v0.1.1` is work done during M1, and `v0.2.0` is the tree where M1 finished. A reader can tell from a version number which milestones are behind it, which is more useful than a counter that goes up whenever a release feels due.

The version lives in one place, `[workspace.package]` in the root `Cargo.toml`, and every crate inherits it. They move together on purpose: ten crates on nine different versions is a support burden nobody in this project is being paid to carry.

## Cutting one

1. Open a release pull request that bumps `version` in the root `Cargo.toml` and the `version` on each path dependency, and run `cargo update -w` so the lockfile follows.
2. Merge it.
3. Tag the merge commit and push the tag. The `Release` workflow builds binaries for five targets, checksums them, attests their provenance and opens a draft release.
4. Edit the draft, then publish it.

The tag points at the release commit rather than at whatever is on the default branch when you get around to it. That matters when work for the next milestone has already started landing, which it usually has.

## Publishing to crates.io

crates.io starts at `0.1.1` rather than at `0.1.0`, and that is deliberate. `v0.1.0` was tagged at the tree where M0 finished, before any of the publishing machinery existed, and by the time it did exist the command line package had been renamed because `iris-cli` belongs to somebody else. Publishing `0.1.0` from a tree that is not the one the tag points at would make the tag a lie for the sake of a round number.

The `Publish` workflow is manual, takes the version as an input, and defaults to a dry run. Run it as a dry run first. It checks that the version matches the tree, builds, tests, and then packages every crate without uploading anything.

The dry run is one `cargo package --workspace` rather than ten `cargo publish --dry-run` calls, and the difference matters on a first release. A single crate dry run resolves that crate's dependencies against crates.io, so on a first release everything downstream of `iris-abi` fails to resolve for a reason that has nothing to do with whether the release is good. Packaging the workspace builds a temporary registry out of the workspace itself, so each crate is verified against the versions that are about to go out.

When that passes, run it again with the dry run box unticked.

### Why it is slow the first time

crates.io limits how fast new crate *names* can be created much harder than it limits new versions of a crate that already exists. This workspace publishes ten crates, so the first release has to wait out that limit, and `ci/publish.sh` handles it: it asks crates.io about each name, publishes the ones that are new, and waits about ten minutes between them once the initial burst is used up. Expect the first publish to take upward of an hour. Every publish after it takes a few minutes.

The script also skips anything already published at the requested version, so a run that fails halfway through can be rerun and picks up where it stopped rather than starting again.

### Order

Dependency order, written down in the script rather than derived, because a wrong answer is a half published release somebody has to clean up by hand:

`iris-abi`, `iris-format`, `iris-guard`, `iris-native`, `iris-source`, `iris-trust`, `iris-decoder`, `iris-vm`, `iris-runtime`, `iris`.

The command line tool is the package named `iris`, not `iris-cli`, because `iris-cli` on crates.io belongs to somebody else and has since well before this project existed. The binary it installs is called `iris` either way.

### The token

`CARGO_REGISTRY_TOKEN` is a repository secret and is read only by the `Publish` workflow. It is not in the repository, it is not in any script, and nothing else can see it.

`ci/publish.sh` looks at the shape of the token before it uploads anything. That check exists because of how the alternative fails: cargo does not look at the token until it has packaged, verified and uploaded a crate, at which point the retry in the script waits ten minutes and does the whole thing again before giving up, so a wrong token costs twenty minutes to find out about. The mistake it catches is a token copied out of a shell file with the quotes still around it, which crates.io rejects as not being a token at all.

It is a shape check rather than an authorisation check, and that is a limitation rather than a choice. crates.io has no read only endpoint that will say whether a token is live: `/me` is reserved for the website and refuses every API token, and the endpoints that do accept one ignore it. A token that is well formed but revoked still gets found the slow way.

## The fleet

Three self hosted runners exist alongside the hosted ones, and the `Fleet` workflow uses them. They are labelled by hardware rather than by hostname, because a hostname tells a reader nothing they can compare against:

| Label | What it is | Jobs |
| --- | --- | --- |
| `epyc-6c-12gb` | AMD EPYC virtual machine, 6 vCPUs, 12 GB, Ubuntu 24.04 | 4 |
| `epyc-8c-24gb` | AMD EPYC virtual machine, 8 vCPUs, 24 GB, Ubuntu 24.04 | 6 |
| `i9-13900k-64gb` | Intel Core i9-13900K, 32 threads, 64 GB, Windows 11 | 16 |

The job count is set per machine rather than left to cargo, which sizes itself by core count and pays no attention to memory. Building Wasmtime wants well over a gigabyte per job and the smaller machines here do not have that much per core.

A fourth machine, four cores and six gigabytes, is not in the fleet. It is already a Kubernetes node and sits at a load average around fifty with about a hundred megabytes free, so a build there would take hours, thrash, and slow down the thing the machine is actually for. Its runner was removed rather than left idle, so that what is registered and what is used are the same list.

Nothing in the `Fleet` workflow runs on a pull request, and that is a security property rather than an oversight. A self hosted runner executes whatever the workflow tells it to, so letting a pull request from a fork reach one hands an arbitrary person a shell on a machine we own. Push to the default branch, a schedule and a manual dispatch are trusted inputs. A fork's pull request is not.

The two EPYC machines are shared tenancy virtual machines, so they run the test suite, which is a correctness question, and they do not produce any number with a duration in it. The one machine that does is the i9-13900K, and the reasoning behind that split is written down in the iris-bench machine notes rather than repeated here.
