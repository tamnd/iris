# Fuzzing

Three targets, against the places where iris reads a number somebody else chose.

`fuzz_format` parses arbitrary bytes as a container. A dataset is an untrusted input, so parsing one must not panic, must not read out of bounds, and must not allocate against a length field it has not checked.

`fuzz_window` slides a window over a fixed file in whatever order the input asks for. The file is filled with a pattern that is a function of each byte's own offset, so a byte read from the wrong place is wrong on its face, and every address the current view does not cover is checked for being unreadable rather than read. The stress test in `crates/iris-source` walks one fixed stride on all four platforms on every change, which is the access pattern a scan has. This target is for the order nobody thought of, and it earned its place in the first minute it ran by finding a zero length read at exactly the end of the file, where the view that would cover it is empty and both platforms refuse a mapping of no bytes.

`fuzz_guard` is the interesting one. It reads its input as instructions for building a sound batch, corrupts it a few times in the ways a broken or hostile decoder would, and hands the result to `iris_guard::check`. Anything the guard accepts is then built with Arrow's own validation turned off and read value by value. That is the whole point of the target: building through `ArrayData::try_new` would mean Arrow catching whatever the guard missed, and the target would prove nothing about the guard. With validation off, the guard is the only thing standing between those numbers and the reads, and the sanitizer is what notices when it was wrong.

Random bytes almost never describe a batch that gets as far as being accepted, which is why the input is read as a recipe rather than as a batch. A target that spends its whole budget being refused for the same dull reason is a target that tests the first branch and nothing after it.

## Running one

```
cargo +nightly fuzz run fuzz_guard -- -max_total_time=60
cargo +nightly fuzz run fuzz_format -- -max_total_time=60
cargo +nightly fuzz run fuzz_window -- -max_total_time=60
```

CI runs all three for half an hour every night, and the guard for a full day every Saturday on hardware we own, because a hosted runner stops a job at six hours. The nightly run and the soak share one corpus cache per target, so the long run feeds the short ones.

## When it finds something

Reproduce it, minimise it, fix it, and then add the case to `crates/iris-guard/src/corpus.rs` rather than leaving it as a file in `fuzz/artifacts`. A corpus of adversarial batches that anybody writing a host or a decoder can run is worth more than a crash file only this repository has. A window crash goes the same way, into a named test in `crates/iris-source/tests/window.rs`, so the case runs on all four platforms on every change rather than only when somebody fuzzes.

```
cargo +nightly fuzz run fuzz_guard fuzz/artifacts/fuzz_guard/crash-...
cargo +nightly fuzz tmin fuzz_guard fuzz/artifacts/fuzz_guard/crash-...
```
