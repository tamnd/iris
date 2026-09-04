//! The compatibility rules, written down as tests so that breaking one is a red build rather than a
//! surprise two years from now in somebody else's data lake.
//!
//! The rules are: a record may grow at the end, a new record tag may appear, and a new capability
//! bit may appear. Anything else is a break.

use iris_abi::{
    ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet, Error, Hello, HelloAck, Message, Projection,
    RangeRequest, Reader, Refusal, RefusalReason, ScanRequest, Tag, Writer, negotiate,
};

fn buf() -> [u8; 512] {
    [0; 512]
}

fn host_hello() -> Hello {
    Hello {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        window_bytes: 64 << 20,
        max_batch_rows: 8192,
        offered: CapabilitySet::new()
            .with(Capability::REQUIRE_RANGE)
            .with(Capability::SLIDING_WINDOW)
            .with(Capability::PROJECTION),
    }
}

#[test]
fn every_record_survives_a_round_trip() {
    let mut storage = buf();
    let mut w = Writer::new(&mut storage);

    let hello = host_hello();
    let ack = HelloAck {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        required: CapabilitySet::new().with(Capability::REQUIRE_RANGE),
        optional: CapabilitySet::new().with(Capability::PROJECTION),
        decoder_id: "round-trip",
    };
    let scan = ScanRequest {
        row_start: 1_000_000_000_000,
        row_count: 8192,
        flags: 0,
        projection: Projection::from_bytes(&[1, 0, 0, 0, 7, 0, 0, 0]).unwrap(),
        filter: b"whatever the two sides agreed on",
    };
    let range = RangeRequest {
        offset: 1 << 40,
        len: 1 << 20,
    };
    let refusal = Refusal::new(RefusalReason::POLICY, "not today");

    hello.encode(&mut w).unwrap();
    ack.encode(&mut w).unwrap();
    scan.encode(&mut w).unwrap();
    range.encode(&mut w).unwrap();
    refusal.encode(&mut w).unwrap();
    let n = w.position();

    let mut r = Reader::new(&storage[..n]);
    assert_eq!(r.message().unwrap(), Message::Hello(hello));
    assert_eq!(r.message().unwrap(), Message::HelloAck(ack));
    assert_eq!(r.message().unwrap(), Message::ScanRequest(scan));
    assert_eq!(r.message().unwrap(), Message::RangeRequest(range));
    assert_eq!(r.message().unwrap(), Message::Refusal(refusal));
    assert!(r.is_empty());
}

/// The case the whole design is for. A decoder is compiled today, the host grows two fields on the
/// end of `Hello` next year, and the decoder keeps working without being rebuilt.
#[test]
fn a_record_may_grow_at_the_end() {
    let hello = host_hello();
    let mut storage = buf();
    let mut w = Writer::new(&mut storage);

    // This is what a later version of the host would write: every field this build knows about, in
    // the same order, and then some it does not.
    w.record(Tag::HELLO, Hello::VERSION, |w| {
        w.u16(hello.abi_major)?;
        w.u16(hello.abi_minor)?;
        w.u32(0)?;
        w.u64(hello.window_bytes)?;
        w.u64(hello.max_batch_rows)?;
        w.var_bytes(hello.offered.as_bytes())?;
        w.u64(0xdead_beef)?;
        w.var_str("a field from the future")
    })
    .unwrap();
    let n = w.position();

    let mut r = Reader::new(&storage[..n]);
    assert_eq!(r.message().unwrap(), Message::Hello(hello));
    // And the reader is left after the whole record, not in the middle of it.
    assert!(r.is_empty());
}

/// The other half of the same rule. A record may only grow, so a payload that is missing a field
/// this build expects has to be an error and not a default.
#[test]
fn a_record_may_not_shrink() {
    let mut storage = buf();
    let mut w = Writer::new(&mut storage);
    w.record(Tag::HELLO, Hello::VERSION, |w| {
        w.u16(ABI_MAJOR)?;
        w.u16(ABI_MINOR)?;
        w.u32(0)
    })
    .unwrap();
    let n = w.position();

    let mut r = Reader::new(&storage[..n]);
    assert!(matches!(r.message(), Err(Error::Truncated { .. })));
}

#[test]
fn an_unknown_record_is_stepped_over() {
    let mut storage = buf();
    let mut w = Writer::new(&mut storage);

    let unknown = Tag(0xFF42);
    w.record(unknown, 9, |w| w.var_str("something invented later"))
        .unwrap();
    let range = RangeRequest { offset: 4, len: 8 };
    range.encode(&mut w).unwrap();
    let n = w.position();

    let mut r = Reader::new(&storage[..n]);
    match r.message().unwrap() {
        Message::Unknown(header) => {
            assert_eq!(header.tag, unknown);
            assert!(header.tag.is_experimental());
        }
        other => panic!("expected an unknown record, got {other:?}"),
    }
    assert_eq!(r.message().unwrap(), Message::RangeRequest(range));
    assert!(r.is_empty());
}

#[test]
fn a_known_record_at_an_unknown_version_is_an_error() {
    let mut storage = buf();
    let mut w = Writer::new(&mut storage);
    w.record(Tag::RANGE_REQUEST, RangeRequest::VERSION + 1, |w| {
        w.u64(0)?;
        w.u64(0)
    })
    .unwrap();
    let n = w.position();

    let mut r = Reader::new(&storage[..n]);
    assert!(matches!(r.message(), Err(Error::UnsupportedVersion { .. })));
}

#[test]
fn a_truncated_buffer_does_not_panic() {
    let mut storage = buf();
    let mut w = Writer::new(&mut storage);
    host_hello().encode(&mut w).unwrap();
    let n = w.position();

    for cut in 0..n {
        let mut r = Reader::new(&storage[..cut]);
        // The only requirement is that it comes back rather than panicking or reading past the end.
        let _ = r.message();
    }
}

#[test]
fn negotiation_agrees_on_what_both_sides_asked_for() {
    let hello = host_hello();
    let ack = HelloAck {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        required: CapabilitySet::new().with(Capability::REQUIRE_RANGE),
        optional: CapabilitySet::new()
            .with(Capability::PROJECTION)
            .with(Capability::RESUMABLE),
        decoder_id: "negotiator",
    };

    let agreed = negotiate(&hello, &ack).unwrap();
    assert!(agreed.has(Capability::REQUIRE_RANGE));
    assert!(agreed.has(Capability::PROJECTION));
    // Offered but never asked for.
    assert!(!agreed.has(Capability::SLIDING_WINDOW));
    // Asked for but not offered, and only optional, so it is simply off.
    assert!(!agreed.has(Capability::RESUMABLE));
}

#[test]
fn a_missing_required_capability_names_itself() {
    let hello = host_hello();
    let ack = HelloAck {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        required: CapabilitySet::new().with(Capability::FILTER_PUSHDOWN),
        optional: CapabilitySet::new(),
        decoder_id: "picky",
    };

    let refusal = negotiate(&hello, &ack).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::MISSING_CAPABILITY);
    assert_eq!(refusal.capability, Capability::FILTER_PUSHDOWN);
}

/// A decoder from the future can require a capability that did not have a name when this host was
/// built. Truncating the bitset would turn that into "requires nothing" and run it anyway, which is
/// the one failure mode that produces wrong answers instead of an error.
#[test]
fn a_required_capability_from_the_future_is_refused() {
    let mut wide = [0u8; CapabilitySet::BYTES + 8];
    let last = wide.len() - 1;
    wide[last] = 0b1000_0000;
    let required = CapabilitySet::from_bytes(&wide);
    assert!(required.has_bits_beyond_this_build());

    let ack = HelloAck {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        required,
        optional: CapabilitySet::new(),
        decoder_id: "from the future",
    };

    let refusal = negotiate(&host_hello(), &ack).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::MISSING_CAPABILITY);
}

#[test]
fn a_major_version_mismatch_is_refused_in_both_directions() {
    let hello = host_hello();
    let mut ack = HelloAck {
        abi_major: ABI_MAJOR + 1,
        abi_minor: 0,
        required: CapabilitySet::new(),
        optional: CapabilitySet::new(),
        decoder_id: "too new",
    };
    assert_eq!(
        negotiate(&hello, &ack).unwrap_err().reason,
        RefusalReason::ABI_TOO_NEW
    );

    let older = Hello {
        abi_major: ABI_MAJOR + 2,
        ..hello
    };
    ack.decoder_id = "too old";
    assert_eq!(
        negotiate(&older, &ack).unwrap_err().reason,
        RefusalReason::ABI_TOO_OLD
    );
}

#[test]
fn the_minor_version_settles_on_the_lower_of_the_two() {
    let hello = Hello {
        abi_minor: 7,
        ..host_hello()
    };
    let ack = HelloAck {
        abi_major: ABI_MAJOR,
        abi_minor: 3,
        required: CapabilitySet::new(),
        optional: CapabilitySet::new(),
        decoder_id: "older",
    };
    assert_eq!(negotiate(&hello, &ack).unwrap().abi_minor, 3);
}

#[test]
fn a_capability_set_round_trips_through_its_trimmed_form() {
    let set = CapabilitySet::new()
        .with(Capability::REQUIRE_RANGE)
        .with(Capability::RESUMABLE);
    // Trailing zero bytes are dropped, so an almost empty set costs one byte and not thirty two.
    assert_eq!(set.as_bytes().len(), 1);
    assert_eq!(CapabilitySet::from_bytes(set.as_bytes()), set);
    assert!(CapabilitySet::new().as_bytes().is_empty());
    assert!(CapabilitySet::new().is_empty());
}

#[test]
fn a_projection_is_a_list_of_columns_and_not_a_mask() {
    // Column indices are 32 bits wide, so a table can have more columns than any bitmask worth
    // putting in a header could describe.
    let far_out: u32 = 3_000_000_000;
    let mut raw = [0u8; 8];
    raw[..4].copy_from_slice(&far_out.to_le_bytes());
    raw[4..].copy_from_slice(&7u32.to_le_bytes());

    let p = Projection::from_bytes(&raw).unwrap();
    assert_eq!(p.len(), 2);
    let cols: Vec<u32> = p.iter().collect();
    assert_eq!(cols, vec![far_out, 7]);

    assert!(Projection::from_bytes(&[0, 0, 0]).is_err());
    assert!(Projection::ALL.is_empty());
}

/// The prior art capped a projection at a 64 bit mask. The main table in `ClickBench` has 105
/// columns, so that cap is not theoretical.
#[test]
fn a_projection_covers_more_than_sixty_four_columns() {
    let mut raw = Vec::new();
    for col in 0u32..105 {
        raw.extend_from_slice(&col.to_le_bytes());
    }
    let p = Projection::from_bytes(&raw).unwrap();
    assert_eq!(p.len(), 105);
    assert_eq!(p.iter().last(), Some(104));
}

/// The test issue #11 asks for, in both directions.
///
/// A decoder is compiled today against the fields a scan request has today. Next year the host
/// grows two more and starts sending them. The decoder has not been rebuilt and cannot be, because
/// it is sitting inside somebody's dataset. It has to read the fields it knows and get the right
/// answer.
#[test]
fn a_decoder_built_against_a_shorter_request_reads_a_longer_one() {
    let scan = ScanRequest {
        row_start: 4_000_000_000,
        row_count: 65_536,
        flags: 0,
        projection: Projection::from_bytes(&[3, 0, 0, 0]).unwrap(),
        filter: b"",
    };

    let mut storage = buf();
    let mut w = Writer::new(&mut storage);
    // The host from next year. Same tag, same layout version, because appending is not a break.
    w.record(Tag::SCAN_REQUEST, ScanRequest::VERSION, |w| {
        w.u64(scan.row_start)?;
        w.u64(scan.row_count)?;
        w.u64(scan.flags)?;
        w.var_bytes(scan.projection.as_bytes())?;
        w.var_bytes(scan.filter)?;
        w.u64(7)?;
        w.var_str("a limit clause, or whatever we think of in 2028")
    })
    .unwrap();
    let n = w.position();

    // This build reads it and gets exactly what was sent, ignoring what it does not know about.
    let mut r = Reader::new(&storage[..n]);
    assert_eq!(r.message().unwrap(), Message::ScanRequest(scan));
    assert!(r.is_empty());

    // And a reader even older than this one, which only ever knew the first four fields, is also
    // fine. This is the literal shape of the compatibility claim.
    let mut r = Reader::new(&storage[..n]);
    let (header, mut p) = r.record().unwrap();
    assert_eq!(header.tag, Tag::SCAN_REQUEST);
    assert_eq!(p.u64().unwrap(), scan.row_start);
    assert_eq!(p.u64().unwrap(), scan.row_count);
    assert_eq!(p.u64().unwrap(), scan.flags);
    assert_eq!(p.var_bytes().unwrap(), scan.projection.as_bytes());
    assert!(r.is_empty());
}

#[test]
fn a_writer_that_runs_out_of_room_says_so() {
    let mut small = [0u8; 16];
    let mut w = Writer::new(&mut small);
    assert!(matches!(
        host_hello().encode(&mut w),
        Err(Error::BufferFull { .. })
    ));
}
