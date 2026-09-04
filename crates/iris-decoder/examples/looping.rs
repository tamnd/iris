//! A decoder that never comes back, so that something can prove the host stops it.
//!
//! Every other decoder in this repository is one somebody here wrote and is trying to be correct.
//! That is exactly the wrong thing to test metering against, because a decoder that returns proves
//! nothing about a decoder that does not. This one is the hostile case written down: it opens
//! normally, agrees to everything, and then spins forever on the first scan.
//!
//! It is a plain example rather than a fixture, so it is built from source every time the gate test
//! runs and cannot drift away from the SDK the way a committed `.wasm` would.

use iris_abi::CapabilitySet;
use iris_decoder::{Decoder, Request, Result, Session, Sink, Source, export_decoder};

/// A decoder whose scan does not terminate.
struct Looping;

impl Decoder for Looping {
    const NAME: &'static str = "looping";
    const REQUIRES: CapabilitySet = CapabilitySet::new();

    fn open(_session: &Session, _source: &mut dyn Source) -> Result<Self> {
        Ok(Self)
    }

    fn scan(
        &mut self,
        _request: &Request<'_>,
        _source: &mut dyn Source,
        _sink: &mut dyn Sink,
    ) -> Result<()> {
        spin()
    }
}

/// The loop, kept in one function so there is one place to say why it is written this way.
///
/// The counter and the `black_box` are not decoration. An empty loop is a loop with no side effects,
/// and a compiler is entitled to do surprising things with one. Reading and writing a value the
/// optimiser has been told it cannot see through makes this a loop that is certainly still there in
/// the module the host compiles, which is the only version of it worth testing.
fn spin() -> ! {
    let mut counter: u64 = 0;
    loop {
        counter = core::hint::black_box(counter).wrapping_add(1);
    }
}

export_decoder!(Looping);

/// A decoder is a `cdylib` and has no `main`. An example is a binary, so it gets one, and this one
/// stops exactly where every other example would carry on.
///
/// The handshake is worth running because it is what makes this a decoder rather than a module that
/// hangs. It opens, it agrees terms, it looks entirely ordinary, and then the scan never comes back.
/// The scan is not run here, for the obvious reason.
fn main() {
    use iris_abi::{ABI_MAJOR, ABI_MINOR, Hello};
    use iris_decoder::{Instance, Resident, record};

    let source = [0u8; 64];
    let hello = Hello {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        window_bytes: 0,
        max_batch_rows: 256,
        offered: CapabilitySet::new(),
        source_bytes: source.len() as u64,
    };

    let mut instance = Instance::<Looping>::new();
    let greeting = record(|w| hello.encode(w)).expect("a Hello always fits");
    instance.input(greeting.len()).copy_from_slice(&greeting);
    instance.start(&mut Resident::new(&source));

    println!(
        "This decoder opened normally and would never return from a scan. It exists so a host can \
         be caught stopping it. Run the iris-runtime gate test to see that happen."
    );
}
