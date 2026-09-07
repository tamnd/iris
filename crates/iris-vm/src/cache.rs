//! Keeping compiled decoders on disk so the next process does not compile them again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iris_format::Digest;

use crate::error::Result;
use crate::module::{Program, Vm};

/// What the key is a hash of, so that a value from a later arrangement is a different key.
const TAG: &[u8] = b"iris compilation cache v1\n";

/// What a stored artefact is called, so a directory shared with something else is still legible.
const SUFFIX: &str = ".iris-aot";

/// Tells one temporary name from another within a process.
///
/// The process identifier separates two processes and this separates two threads of one, which are
/// the two ways a directory can have two writes to the same entry in flight at once.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory of compiled decoders, and what has happened to it.
///
/// # What is in the key
///
/// The digest of the module and the [`fingerprint`](Vm::fingerprint) of the engine, hashed together
/// under a tag. Between them those cover the four things that decide what a compile produces: the
/// decoder, the version of Wasmtime, the target triple, and the configuration the compiler ran
/// under. The last three come out of Wasmtime rather than out of a list kept here, because Wasmtime
/// is the one that knows which of its settings reach the compiler.
///
/// The point of the key is what it makes impossible rather than what it makes fast. An artefact is
/// machine code for one target compiled by one compiler, so an entry reached by a runtime that would
/// have compiled it differently is not a stale answer, it is the wrong instructions. Putting all
/// four in the key means such an entry is never found in the first place, and an upgrade that
/// changes any of them simply misses everything and refills.
///
/// # What happens when it goes wrong
///
/// Nothing that stops a scan. Every failure here falls back to compiling: a directory that cannot be
/// read, a file that is not there, an artefact Wasmtime will not accept, a write that fails because
/// the disk is full. A cache is an optimisation and an optimisation that can fail an open is worse
/// than no cache at all.
#[derive(Debug)]
pub(crate) struct Cache {
    dir: PathBuf,
    fingerprint: Digest,
    reused: AtomicU64,
    stored: AtomicU64,
}

impl Cache {
    /// A cache in this directory, for engines with this fingerprint.
    pub(crate) fn new(dir: PathBuf, fingerprint: Digest) -> Self {
        Self {
            dir,
            fingerprint,
            reused: AtomicU64::new(0),
            stored: AtomicU64::new(0),
        }
    }

    /// The compiled form of these bytes, from the directory if it is there and by compiling if not.
    ///
    /// A miss compiles once rather than twice. Producing an artefact and loading it back is the same
    /// work as compiling normally, so the cache costs a write on the first open of a decoder and
    /// nothing on any other.
    ///
    /// # Errors
    ///
    /// Only what compiling would return. A cache that cannot be read or written is a cache that is
    /// not used.
    pub(crate) fn compile(&self, vm: &Vm, wasm: &[u8], decoder: &str) -> Result<Program> {
        let path = self.path(wasm);
        if let Some(program) = reuse(vm, &path, decoder) {
            self.reused.fetch_add(1, Ordering::Relaxed);
            return Ok(program);
        }

        let artefact = vm.precompile(wasm)?;
        if self.store(&path, &artefact) {
            self.stored.fetch_add(1, Ordering::Relaxed);
        }

        #[allow(unsafe_code, reason = "the compilation cache, see the note in lib.rs")]
        let loaded = {
            // SAFETY: these are the bytes this engine's own compiler produced a moment ago, in this
            // process, and they have not been anywhere since.
            unsafe { vm.load(&artefact, decoder) }
        };

        // An artefact this engine produced and will not take back is not a thing that should happen,
        // and it is still not a reason to fail an open when compiling the ordinary way is right
        // there. It does mean the file just written is one nothing will ever load, which the next
        // open will find out about and fall through in the same way.
        match loaded {
            Ok(program) => Ok(program),
            Err(_) => vm.compile_directly(wasm, decoder),
        }
    }

    /// How many opens were served out of the directory.
    pub(crate) fn reused(&self) -> u64 {
        self.reused.load(Ordering::Relaxed)
    }

    /// How many artefacts were written into it.
    pub(crate) fn stored(&self) -> u64 {
        self.stored.load(Ordering::Relaxed)
    }

    /// Where these module bytes would be kept.
    fn path(&self, wasm: &[u8]) -> PathBuf {
        let mut key = Vec::from(TAG);
        key.extend_from_slice(Digest::of(wasm).as_bytes());
        key.extend_from_slice(self.fingerprint.as_bytes());
        self.dir.join(format!("{}{SUFFIX}", Digest::of(&key)))
    }

    /// Writes an artefact, and says whether it got there.
    ///
    /// Written beside the destination and renamed onto it, so a reader never sees a partial file and
    /// two processes compiling the same decoder at once end with one of the two artefacts rather
    /// than with halves of both. The temporary name carries the process and a counter because two
    /// threads of one process can be here at the same time as well.
    fn store(&self, path: &Path, artefact: &[u8]) -> bool {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return false;
        }

        let temporary = self.dir.join(format!(
            "{}.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SUFFIX.trim_start_matches('.')
        ));
        if std::fs::write(&temporary, artefact).is_err() {
            return false;
        }
        if std::fs::rename(&temporary, path).is_err() {
            // Leaving the temporary behind would be leaving rubbish that nothing ever collects.
            drop(std::fs::remove_file(&temporary));
            return false;
        }
        true
    }
}

/// Loads what is at this path, if anything there loads.
///
/// A missing file is the ordinary case and is not worth distinguishing from a file that will not
/// load, because the answer to both is to compile. That is also what makes an interrupted write
/// harmless: a half written artefact is refused by Wasmtime, and the next open replaces it.
#[allow(unsafe_code, reason = "the compilation cache, see the note in lib.rs")]
fn reuse(vm: &Vm, path: &Path, decoder: &str) -> Option<Program> {
    let artefact = std::fs::read(path).ok()?;

    // SAFETY: the bytes come from a path this process derived from the engine's own fingerprint, in
    // a directory the operator named, and nothing but `Cache::store` ever writes one. That is the
    // promise being kept, and it is the same promise a host makes about the decoder resolver:
    // storage the host controls is trusted, and storage somebody else can write is not storage this
    // may be pointed at.
    unsafe { vm.load(&artefact, decoder) }.ok()
}
