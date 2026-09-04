## What this changes

<!-- One paragraph. What was wrong, and what it is now. -->

## Why

<!-- Link the issue or the milestone gate this moves. If it moves neither, say what it is for. -->

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Public items are documented
- [ ] Every new `unsafe` block has a comment saying why it is sound

## If this touches the ABI

<!-- Delete this section if it does not. -->

- [ ] There is a written compatibility note in the pull request body
- [ ] An old decoder against a new host is described, and tested
- [ ] A new decoder against an old host is described, and tested

## If this claims a performance change

<!-- Delete this section if it does not. -->

- [ ] The number cites a claim identifier and a run identifier from iris-bench
- [ ] The confidence interval and the sample size are stated, not only the median
- [ ] The machine class is stated
