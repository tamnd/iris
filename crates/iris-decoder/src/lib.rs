//! Guest side SDK for writing iris decoders.
//!
//! A decoder is a `wasm32-unknown-unknown` module that speaks the ABI in [`iris_abi`]. Writing one
//! should be writing a decode loop, and everything else in that sentence is what this crate is for.
//!
//! # What a decoder looks like
//!
//! Implement [`Decoder`], then call [`export_decoder`] once. That is the whole surface.
//!
//! ```
//! use iris_abi::{Capability, CapabilitySet};
//! use iris_decoder::{Batch, Decoder, Request, Session, Sink, Source, export_decoder};
//!
//! /// Eight byte little endian integers, one column, no nulls, no compression.
//! struct Bytes8 {
//!     rows: u64,
//! }
//!
//! impl Decoder for Bytes8 {
//!     const NAME: &'static str = "bytes8";
//!     const REQUIRES: CapabilitySet = CapabilitySet::new().with(Capability::RANDOM_ACCESS);
//!
//!     fn open(session: &Session, _source: &mut dyn Source) -> iris_decoder::Result<Self> {
//!         Ok(Self {
//!             rows: session.source_bytes() / 8,
//!         })
//!     }
//!
//!     fn scan(
//!         &mut self,
//!         request: &Request<'_>,
//!         source: &mut dyn Source,
//!         sink: &mut dyn Sink,
//!     ) -> iris_decoder::Result<()> {
//!         let start = request.row_start().min(self.rows);
//!         let count = request.row_count().min(self.rows - start);
//!         if count == 0 {
//!             return Ok(());
//!         }
//!
//!         let mut batch = Batch::new(count);
//!         let bytes = source.range(start * 8, count * 8)?;
//!         batch.array(count, 0).buffer(&[]).buffer(bytes);
//!         sink.emit(&batch)
//!     }
//! }
//!
//! export_decoder!(Bytes8);
//! ```
//!
//! # What the host sees
//!
//! [`export_decoder`] puts four functions in the module, and they are the only thing a host needs
//! to know about a decoder built with this crate.
//!
//! | Export | What it does |
//! | --- | --- |
//! | `iris_source(len: u32) -> u32` | Makes room for the source and says where to write it |
//! | `iris_input(len: u32) -> u32` | Makes room for one record and says where to write it |
//! | `iris_start() -> u64` | Reads the `Hello` in the input buffer and answers it |
//! | `iris_scan() -> u64` | Reads the `ScanRequest` in the input buffer and runs it |
//!
//! The two that return a `u64` return an answer packed as address in the high half and length in
//! the low half, or zero for no answer at all. Batches do not come back that way. They go out
//! through `iris.emit(ptr, len)` as they are produced, because a scan that produces a thousand
//! batches should not have to hold a thousand batches.
//!
//! Bytes come in the same way and in the opposite direction. When a decoder asks for a range the
//! host did not send up front, the SDK calls `iris.require_range(at, len, dst)` and the host fills
//! a buffer the guest allocated. That call is allowed to take as long as the host needs, including
//! stopping the module entirely and resuming it later, and nothing in the guest can tell that it
//! happened.
//!
//! Every answer is a record, so a host reads its tag to find out what happened. A `HelloAck` means
//! the decoder opened, a `Refusal` means it did not and says why, and nothing at all after a scan
//! means the scan finished and every batch it was going to produce has already arrived.
//!
//! # What the host does not see
//!
//! Guest memory. The host never hands the guest an address and the guest never follows one, so
//! there is no unsafe code in this crate at all. `iris.require_range` did not change that: the
//! decoder names a buffer it allocated itself and the host writes into that, exactly as it does for
//! the source and for every record. The failure mode of a lying host stays a wrong answer rather
//! than a corrupt guest.
//!
//! It also did not change any decoder. A decode loop written against [`Source`] in M1, when the
//! whole file was copied in up front, is the same loop when the file is forty gigabytes in an
//! object store and every range is a request. That is what the signature was for: `range` borrows
//! from `&mut self`, so a range that cannot be held across the next one was already a rule the
//! borrow checker enforced rather than a rule a decoder author had to remember.

mod batch;
mod decoder;
mod error;
mod harness;
mod instance;
mod session;

#[cfg(target_arch = "wasm32")]
pub mod guest;

pub use batch::Batch;
pub use decoder::{Decoder, Resident, Sink, Source};
pub use error::{Error, Result};
pub use harness::{Collect, Recorded};
pub use instance::{Instance, record};
pub use session::{Request, Session};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Makes a type that implements [`Decoder`] into the decoder this module exports.
///
/// Call it once, at the top level of a crate that builds a `cdylib` for `wasm32-unknown-unknown`.
/// It expands to nothing on any other target, so the same crate still builds for the host and its
/// decode loop can be tested there with [`Instance`], [`Resident`] and [`Collect`].
///
/// The macro is deliberately thin. Everything it could have generated lives in [`Instance`]
/// instead, because a bug in a generated function is much harder to read than a bug in a normal
/// one, and because a decoder author who wants to test the thing the host will actually call needs
/// it to be callable without a host.
#[macro_export]
macro_rules! export_decoder {
    ($decoder:ty) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            ::std::thread_local! {
                static INSTANCE: ::core::cell::RefCell<$crate::Instance<$decoder>> =
                    const { ::core::cell::RefCell::new($crate::Instance::new()) };
                static SOURCE: ::core::cell::RefCell<::std::vec::Vec<u8>> =
                    const { ::core::cell::RefCell::new(::std::vec::Vec::new()) };
                // Where a range the host had to fetch lands. Separate from SOURCE because SOURCE is
                // what the host copied in up front and this is what it sends one range at a time,
                // and a decoder that reads a resident file never touches this one at all.
                static SCRATCH: ::core::cell::RefCell<::std::vec::Vec<u8>> =
                    const { ::core::cell::RefCell::new(::std::vec::Vec::new()) };
            }

            #[allow(
                unsafe_code,
                reason = "an exported symbol is the only way a host can find the decoder at all"
            )]
            #[unsafe(no_mangle)]
            extern "C" fn iris_source(len: u32) -> u32 {
                SOURCE.with_borrow_mut(|source| {
                    source.clear();
                    source.resize($crate::guest::size(len), 0);
                    $crate::guest::address(source)
                })
            }

            #[allow(
                unsafe_code,
                reason = "an exported symbol is the only way a host can find the decoder at all"
            )]
            #[unsafe(no_mangle)]
            extern "C" fn iris_input(len: u32) -> u32 {
                INSTANCE.with_borrow_mut(|instance| {
                    $crate::guest::address(instance.input($crate::guest::size(len)))
                })
            }

            #[allow(
                unsafe_code,
                reason = "an exported symbol is the only way a host can find the decoder at all"
            )]
            #[unsafe(no_mangle)]
            extern "C" fn iris_start() -> u64 {
                SOURCE.with_borrow(|resident| {
                    SCRATCH.with_borrow_mut(|scratch| {
                        INSTANCE.with_borrow_mut(|instance| {
                            let mut source = $crate::guest::HostSource::new(resident, scratch);
                            $crate::guest::packed(instance.start(&mut source))
                        })
                    })
                })
            }

            #[allow(
                unsafe_code,
                reason = "an exported symbol is the only way a host can find the decoder at all"
            )]
            #[unsafe(no_mangle)]
            extern "C" fn iris_scan() -> u64 {
                SOURCE.with_borrow(|resident| {
                    SCRATCH.with_borrow_mut(|scratch| {
                        INSTANCE.with_borrow_mut(|instance| {
                            let mut source = $crate::guest::HostSource::new(resident, scratch);
                            let mut sink = $crate::guest::HostSink::new();
                            $crate::guest::packed(instance.scan(&mut source, &mut sink))
                        })
                    })
                })
            }
        };
    };
}
