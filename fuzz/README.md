# Fuzzing

Two targets, against the two places where iris reads a number somebody else chose.

`fuzz_format` parses arbitrary bytes as a container. A dataset is an untrusted input, so parsing one must not panic, must not read out of bounds, and must not allocate against a length field it has not checked.

`fuzz_guard` is the interesting one. It reads its input as instructions for building a sound batch, corrupts it a few times in the ways a broken or hostile decoder would, and hands the result to `iris_guard::check`. Anything the guard accepts is then built with Arrow's own validation turned off and read value by value. That is the whole point of the target: building through `ArrayData::try_new` would mean Arrow catching whatever the guard missed, and the target would prove nothing about the guard. With validation off, the guard is the only thing standing between those numbers and the reads, and the sanitizer is what notices when it was wrong.

Random bytes almost never describe a batch that gets as far as being accepted, which is why the input is read as a recipe rather than as a batch. A target that spends its whole budget being refused for the same dull reason is a target that tests the first branch and nothing after it.

## Running one

```
cargo +nightly fuzz run fuzz_guard -- -max_total_time=60
cargo +nightly fuzz run fuzz_format -- -max_total_time=60
```

CI runs both for half an hour every night, and the guard for a full day every Saturday on hardware we own, because a hosted runner stops a job at six hours. Both share one corpus cache, so the long run feeds the short ones.

## When it finds something

Reproduce it, minimise it, fix it, and then add the case to `crates/iris-guard/src/corpus.rs` rather than leaving it as a file in `fuzz/artifacts`. A corpus of adversarial batches that anybody writing a host or a decoder can run is worth more than a crash file only this repository has.

```
cargo +nightly fuzz run fuzz_guard fuzz/artifacts/fuzz_guard/crash-...
cargo +nightly fuzz tmin fuzz_guard fuzz/artifacts/fuzz_guard/crash-...
```
