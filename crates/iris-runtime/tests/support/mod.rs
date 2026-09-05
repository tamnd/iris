//! The decoders the gate tests drive, compiled for wasm32 on the way in.
//!
//! # Why it builds the decoder rather than checking one in
//!
//! A committed `.wasm` fixture is a binary nobody reads, built by a toolchain nobody remembers,
//! that keeps passing after the source it came from has stopped matching it. The interesting
//! failure here is the ABI drifting away from the SDK, and a stale fixture is precisely the thing
//! that hides it. So the decoder is compiled from `crates/iris-decoder/examples/fixedwidth.rs`
//! every time these tests run.
//!
//! The cost is that running the test suite needs the wasm32 target installed. `rust-toolchain.toml`
//! asks for it, so rustup puts it there without anybody thinking about it, and the failure message
//! says what to do if somebody is running a toolchain that ignored the file.
//!
//! The nested cargo gets its own target directory. Cargo's lock is per target directory, so
//! building into the one the outer cargo is holding would deadlock rather than fail, which is a
//! much worse way to find out.
//!
//! It also gets a lock of its own, and that one is not obvious. nextest runs every test in its own
//! process, so the tests across these files are that many cargo invocations against one target
//! directory. Cargo's lock makes them build one at a time, which is all it promises. It does not
//! cover the reads afterwards, and cargo finishes a build by moving the example from `deps` to
//! `examples`, which it does by removing the destination and linking it again. It does that every
//! time, even when nothing needed compiling. So a process that has just released the lock and is
//! reading the files can be reading them while the next process removes one, and the read fails with
//! a missing file after cargo said it built it. The lock here is held across the build and the reads
//! together, which is the only arrangement where what was built is still there to be read.
//!
//! Both examples are built by one cargo invocation rather than one each. Almost all of the time
//! goes on the SDK and its dependencies, which is paid once either way, so the second decoder is
//! close to free and two nested builds would not be.
//!
//! Every test file here gets its own copy of this module, and no one file uses all of it, which is
//! what the allow below is for. Splitting it up so that each file sees only what it calls would mean
//! two ways to run a nested cargo, which is the thing worth avoiding.
#![allow(dead_code)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The workspace root, from this crate's manifest.
pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is two levels above this crate")
}

/// The decoder modules these tests drive.
pub(crate) struct Modules {
    /// The decoder that reads the fixture.
    pub(crate) fixedwidth: Vec<u8>,
    /// The decoder that never returns.
    pub(crate) looping: Vec<u8>,
}

/// Compiles the decoders for wasm32, once per test binary.
pub(crate) fn modules() -> &'static Modules {
    static MODULES: OnceLock<Modules> = OnceLock::new();
    MODULES.get_or_init(|| {
        let root = workspace_root();
        let target_dir = root.join("target").join("gate-wasm");

        // Held until the end of this block, so that no other test process is part way through a
        // build while this one reads what that build produced. See the note at the top of the file.
        std::fs::create_dir_all(&target_dir).expect("creating the nested target directory");
        let guard = File::create(target_dir.join("gate.lock")).expect("creating the build lock");
        guard.lock().expect("taking the build lock");

        let mut cargo = Command::new(env!("CARGO"));
        cargo
            .current_dir(&root)
            .args([
                "build",
                "--release",
                "-p",
                "iris-decoder",
                "--example",
                "fixedwidth",
                "--example",
                "looping",
                "--target",
                "wasm32-unknown-unknown",
                "--target-dir",
            ])
            .arg(&target_dir);

        // The flags the outer build is running under are not the flags this build wants. Coverage
        // instrumentation is the one that matters: it is on for the whole workspace when the
        // coverage job runs, and it does not apply to a target with no operating system under it.
        for leaked in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "LLVM_PROFILE_FILE",
        ] {
            cargo.env_remove(leaked);
        }

        let out = cargo.output().expect("cargo is on the path, it ran this");
        assert!(
            out.status.success(),
            "building the decoders for wasm32 failed. If the target is missing, run\n  \
             rustup target add wasm32-unknown-unknown\n\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let built = target_dir
            .join("wasm32-unknown-unknown")
            .join("release")
            .join("examples");
        let read = |name: &str| {
            let path = built.join(format!("{name}.wasm"));
            std::fs::read(&path)
                .unwrap_or_else(|err| panic!("cargo said it built {}: {err}", path.display()))
        };

        Modules {
            fixedwidth: read("fixedwidth"),
            looping: read("looping"),
        }
    })
}

/// The decoder that reads the fixture.
pub(crate) fn decoder_module() -> &'static [u8] {
    &modules().fixedwidth
}

/// The decoder that agrees to everything and then spins forever on the first scan.
pub(crate) fn looping_module() -> &'static [u8] {
    &modules().looping
}
