# The iris ABI

This is the contract between a host and a decoder. It is the only part of iris that can ossify, because a decoder that somebody built and shipped inside a dataset three years ago has to keep running against a host built today, and nobody can go back and recompile it.

Everything else in the project can be rewritten. This cannot, so it gets written down.

The implementation is `crates/iris-abi`, which is `no_std`, has no dependencies at all, and contains no unsafe code. Those three properties are checked by CI rather than promised in a comment, because this crate ends up inside every decoder anybody writes and anything it pulls in, everybody pays for.

## The shape

Every message that crosses the boundary is a record. A record is a tag, a layout version, a length, and a payload.

| Field | Width | Meaning |
| --- | --- | --- |
| `tag` | `u16` | Which record this is |
| `version` | `u16` | Which layout of that record the payload uses |
| `len` | `u32` | How many bytes of payload follow, not counting padding |

That is eight bytes, and the payload starts right after it. Everything is little-endian. Every machine anybody is going to run this on is little-endian, and pretending otherwise would cost real instructions in the hot path in exchange for a portability nobody is asking for. If that ever stops being true it is a new major version and not a runtime flag.

A payload is laid out as its fixed-width fields in declaration order, followed by its variable-length fields. A variable-length field is a `u32` byte count, then that many bytes, then zero padding up to the next multiple of eight. Records themselves are padded the same way, so a record that starts eight-byte aligned has a payload that starts eight-byte aligned. The padding costs at most seven bytes per field and it buys the option of reading a fixed-width run by pointing at it rather than copying it out field by field.

## What is allowed to change

Three things are allowed, and they are the reason the framing looks the way it does.

A record may grow at the end. A reader that was compiled before the new field existed reads the fields it knows and stops, and the length in the header tells it where the record ended, so stopping early lands it on the next record rather than in the middle of this one. Growing a record does not bump its layout version.

A new record tag may be added. A reader that does not recognise a tag steps over the whole record using the length and carries on. In the Rust API that arrives as `Message::Unknown` rather than an error.

A new capability bit may be added. A side that does not have it simply does not offer it, and the other side decides what to do about that.

## What is not allowed to change

Removing a field, reordering fields, changing what a field means, and changing what a capability bit means are all breaking. So is shrinking a record, which is the same thing as removing a field from the end.

When one of those has to happen, the record's layout version goes up and the reader for the old version stays in the tree next to the new one. A version bump is not a "how new is this" counter and it is not a release marker. It only moves when a reader that guessed would be worse than a reader that stopped.

The rules above are held by the tests in `crates/iris-abi/tests/forward_compat.rs`. Breaking one of them is a red build rather than a surprise two years from now in somebody else's data lake.

## Versions

`ABI_MAJOR` is zero today, which means the record layouts are still allowed to move. Taking it to one means the layouts are frozen, and that is a milestone with a written compatibility note behind it rather than something that happens because a refactor felt finished.

A major version mismatch is fatal in both directions and produces a refusal that says which direction it was. A minor version mismatch is not fatal: the two sides settle on the lower of the two minor versions and get on with it. Minor goes up when a field, a record or a capability is added.

## Capabilities

A capability is one bit meaning "this side can do this thing". The host says what it offers, the decoder says what it requires and what it would merely like, and if the decoder requires something the host does not offer then the two of them stop.

Capabilities exist instead of a feature level number because capabilities compose and version numbers do not. A decoder that needs sliding windows and a decoder that needs filter pushdown are not ordered relative to each other, and giving them version numbers would mean every host has to implement every feature in order to claim the number.

| Bit | Name | Meaning |
| --- | --- | --- |
| 0 | `require-range` | The decoder pulls bytes of the source by asking for ranges rather than being handed the whole thing |
| 1 | `sliding-window` | The host can move a window over a source larger than the guest can address |
| 2 | `projection` | The decoder honours a column projection |
| 3 | `filter-pushdown` | The decoder honours a filter pushed down to it |
| 4 | `random-access` | The decoder can start at an arbitrary row rather than only at the beginning |
| 5 | `stateless` | The decoder keeps nothing between calls, so the host may reuse one instance freely |
| 6 | `resumable` | The decoder can be interrupted partway through and resumed |

On the wire a capability set is a variable-length byte string, so a later version can make it wider. This build holds 32 bytes, which is 256 capabilities, in a fixed array with no allocation.

There is a trap here worth naming. A decoder built against a later version of the ABI can require a capability that did not have a name when the host was compiled, and a host that just truncated the bitset would read that as "requires nothing" and run the decoder anyway. That is the one failure mode in the whole negotiation that produces wrong answers instead of an error, so the decoder is checked for set bits past the end of what the host understands before anything else is compared, and finding one is a refusal.

## Refusing

Negotiation ends in an agreement both sides can name, or in a refusal that says what the problem was. It never ends in one side quietly ignoring something it did not understand.

A refusal carries a reason code, the capability that was missing when that is what happened, and a line of text for a human. The text is there because "this did not work" is not an actionable message. Somebody has to be able to read the failure and know which capability to go and implement.

The reasons are missing capability, ABI version too new, ABI version too old, unsupported record, malformed record, resource limit, and refused by policy. The last one is deliberate: a host that can do what was asked and is choosing not to should say so rather than pretending it cannot.

## The records

`Hello` is the host introducing itself. It carries the ABI major and minor version, how many bytes of the source the host is willing to keep visible at once, the largest row count it will ask for in one scan, the capabilities it offers, and how many bytes the source has in total. A window size of zero means the host will map the whole source and the decoder never has to think about windows. A source size of zero means the host is not saying, which is allowed because a decoder that reads forwards from the start does not need to know and only a decoder that has to find its own footer does.

The source size is the first field that was appended to a record after the ABI shipped, so it is also the first real exercise of the grow-at-the-end rule rather than a test of it. A host built before the field existed writes a `Hello` that stops after the capability set, and a decoder built after it reads zero and carries on. The reader that makes this work is `Reader::opt_u64`, which returns nothing when the payload has already ended and an error when there are some bytes left but fewer than eight. Those two situations look similar and are not: the first is an older writer and the second is a record that was cut in half, and reading a short value as a default is how a corrupt record turns into a wrong answer instead of an error.

`HelloAck` is the decoder answering. It carries the ABI version it was built against, the capabilities it requires, the capabilities it would like, and a name for itself that nothing interprets.

`Refusal` is either side declining, as described above.

`ScanRequest` is the host asking for a run of rows. Row start and row count are 64 bits wide. A row count that fits in 32 bits is a limit somebody will hit, and building limits into a format that ossifies is exactly the mistake this project exists to avoid.

The projection on a scan request is a list of 32-bit column indices, not a bitmask. A bitmask has to pick a width, and whatever width it picks becomes the maximum number of columns the format can ever describe. Wide tables are where a columnar format is supposed to win, so putting a ceiling on the column count is the wrong place to save four bytes. An empty projection means every column.

`RangeRequest` is the decoder asking for bytes of the source it has not been given. This is the record the whole design turns on. The decoder says which bytes it needs and the host decides how to get them, which keeps file handles, caching, prefetch, object store credentials and retry policy on the host side of the boundary where they can be fixed without recompiling anybody's decoder.

`Batch` is the decoder handing back one batch of decoded rows, and it is described in the next section.

Tags from `0xFF00` up are reserved for private extensions and will never be assigned a meaning by us, so anybody can use them for their own records without worrying about a future version of iris colliding with them.

## The scan response

A batch says how many rows it has and then describes the Arrow arrays behind them as a flat list of nodes and a flat list of buffers, both in the pre-order the schema puts its fields in.

| Field | Width | Meaning |
| --- | --- | --- |
| `rows` | `u64` | How many rows this batch holds |
| `flags` | `u64` | Reserved, zero today |
| `nodes` | variable | Sixteen bytes per array: a length and a null count |
| `buffers` | variable | Sixteen bytes per buffer: an offset in guest memory and a length |

This is the shape Arrow IPC uses, and it is the right one here for the same reason. The host already has the schema, so the schema decides how many nodes and how many buffers there should be, and the batch only has to supply them. A batch that disagrees with the schema is a decoder bug the host can catch by counting, which is a much better failure than a batch that carries its own idea of the shape and is believed.

Neither list carries a count. The length in the record header bounds both of them, so a batch cannot claim a million columns without being large enough to describe a million columns. That is the same rule the container format follows and it is the difference between allocation safety being structural and allocation safety being something a reviewer has to remember. A list whose byte length is not a whole number of entries is malformed rather than rounded down.

The buffers are not in the record. They are in the decoder's memory and the offsets say where. Whether those offsets are inside the decoder's memory, and whether the bytes at them are a valid Arrow array, are two separate questions and the record answers neither of them. The host has to check both, and it is in a position to, because it owns the memory the guest is running in.

Batches do not come back the way a `HelloAck` does. A scan that produces a thousand batches should not have to hold a thousand batches, so each one goes out through a host import as it is produced, and the answer to the scan call itself is empty when the scan finished and a `Refusal` when it did not. A host that gets nothing back knows every batch it was going to get has already arrived.

## What a decoder module exports

The ABI is records, and this is the small amount of WebAssembly around them. The guest SDK generates it, so a decoder author never writes any of it, but a host implementer has to know it and it would ossify along with everything else here.

| Export | What it does |
| --- | --- |
| `iris_source(len: u32) -> u32` | Makes room for the source and says where to write it |
| `iris_input(len: u32) -> u32` | Makes room for one record and says where to write it |
| `iris_start() -> u64` | Reads the `Hello` in the input buffer and answers it |
| `iris_scan() -> u64` | Reads the `ScanRequest` in the input buffer and runs it |

There is one import, `iris.emit(ptr: u32, len: u32) -> u32`, which is how a batch record leaves the guest.

The two calls that return a `u64` return an answer packed as an address in the high half and a length in the low half, with zero meaning no answer at all. That is a packed return rather than an out pointer because a wasm function can return a `u64` and cannot return two `u32` values, and because writing the answer through a pointer the host supplied would put the guest in the business of following host addresses, which is exactly what the next paragraph is about.

The host never hands the guest an address and the guest never follows one. The host asks the guest for a buffer, the guest allocates it and says where it is, and the host writes into it. That costs a copy of the source and buys a boundary where the failure mode of a host that lies is a wrong answer rather than a corrupt guest. It is the right trade while the source is resident and copied once. It stops being the right trade at M4, where a window slides many times over a source too large to copy, and at that point `require-range` starts being used and this table grows. Decoders do not change, because a decoder never sees any of this.

## What is not decided yet

The filter on a scan request is opaque bytes today. Deciding what goes in there is a real design problem and it is not worth doing before there is a decoder that would use it.

Metering, cancellation and the exact trap behaviour on a deadline are M2 work, and the `resumable` capability is the placeholder that keeps room for them.
