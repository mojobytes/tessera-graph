// SPDX-License-Identifier: BSL-1.1

//! Integration tests for Bolt protocol handshake — version negotiation.

use tessera_graph_protocol::{
    BOLT_MAGIC, BoltVersion, NO_VERSION_RESPONSE, SUPPORTED_VERSION, encode_version_response,
    negotiate_version, parse_version_proposal,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a 20-byte handshake buffer with `BOLT_MAGIC` and a single proposal in
/// the first slot.
fn make_handshake_with_proposal(proposal: [u8; 4]) -> [u8; 20] {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&proposal);
    handshake
}

// ── parse_version_proposal ────────────────────────────────────────────────────

#[test]
fn parse_version_proposal_bolt44() {
    // Wire bytes [padding=0, range=4, minor=4, major=4] → BE u32 = 0x00_04_04_04
    let (major, minor, range, padding) = parse_version_proposal(0x00_04_04_04);
    assert_eq!(major, 4);
    assert_eq!(minor, 4);
    assert_eq!(range, 4);
    assert_eq!(padding, 0);
}

#[test]
fn parse_version_proposal_zero() {
    let (major, minor, range, padding) = parse_version_proposal(0);
    assert_eq!((major, minor, range, padding), (0, 0, 0, 0));
}

#[test]
fn parse_version_proposal_bolt54() {
    // Bolt 5.4: wire bytes [0x00, 0x04, 0x04, 0x05] → BE u32 = 0x00_04_04_05
    let (major, minor, range, padding) = parse_version_proposal(0x00_04_04_05);
    assert_eq!(major, 5);
    assert_eq!(minor, 4);
    assert_eq!(range, 4);
    assert_eq!(padding, 0);
}

#[test]
fn parse_version_proposal_extracts_all_bytes() {
    // Wire bytes [padding=0, range=2, minor=3, major=1] → BE u32 = 0x00_02_03_01
    let (major, minor, range, padding) = parse_version_proposal(0x00_02_03_01);
    assert_eq!(major, 1);
    assert_eq!(minor, 3);
    assert_eq!(range, 2);
    assert_eq!(padding, 0);
}

#[test]
fn parse_version_proposal_returns_major_minor_range_order() {
    // Asymmetric values to verify tuple order: major=3, minor=7, range=2
    // Wire bytes [0x00, 0x02, 0x07, 0x03] → BE u32 = 0x00_02_07_03
    let (major, minor, range, padding) = parse_version_proposal(0x00_02_07_03);
    assert_eq!(major, 3);
    assert_eq!(minor, 7);
    assert_eq!(range, 2);
    assert_eq!(padding, 0);
}

#[test]
fn parse_version_proposal_preserves_nonzero_padding() {
    // Non-zero padding byte is returned (not validated by parse_version_proposal).
    let (major, minor, range, padding) = parse_version_proposal(0xFF_02_03_01);
    assert_eq!(major, 1);
    assert_eq!(minor, 3);
    assert_eq!(range, 2);
    assert_eq!(padding, 0xFF);
}

// ── negotiate_version ─────────────────────────────────────────────────────────

#[test]
fn negotiate_version_bolt44_accepted() {
    let handshake = make_handshake_with_proposal(0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_version_no_match() {
    // Bolt 6.0 — not supported
    let handshake = make_handshake_with_proposal(0x00_00_00_06u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), None);
}

#[test]
fn negotiate_version_wrong_magic() {
    // All-zeros — wrong magic preamble
    let handshake = [0u8; 20];
    assert_eq!(negotiate_version(&handshake), None);
}

#[test]
fn negotiate_accepts_second_proposal() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    // First proposal: Bolt 6.0 — no match
    handshake[4..8].copy_from_slice(&0x00_00_00_06u32.to_be_bytes());
    // Second proposal: Bolt 4.4 — match
    handshake[8..12].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_accepts_third_proposal() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x00_00_00_06u32.to_be_bytes());
    handshake[8..12].copy_from_slice(&0x00_00_00_05u32.to_be_bytes());
    // Third proposal: Bolt 4.4 — match
    handshake[12..16].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_accepts_fourth_proposal() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x00_00_00_06u32.to_be_bytes());
    handshake[8..12].copy_from_slice(&0x00_00_00_05u32.to_be_bytes());
    handshake[12..16].copy_from_slice(&0x00_00_00_03u32.to_be_bytes());
    // Fourth proposal: Bolt 4.4 — match
    handshake[16..20].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_all_zero_proposals_returns_none() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    // Remaining 16 bytes are all zero — no proposals
    assert_eq!(negotiate_version(&handshake), None);
}

#[test]
fn negotiate_range_covers_supported_minor() {
    // Bolt 4.x with range=4 starting at minor=4 covers 4.0–4.4; 4.4 is ours.
    let handshake = make_handshake_with_proposal(0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_range_does_not_cover_supported_minor() {
    // major=4, range=2, minor=2 → supports only 4.0–4.2; our 4.4 is out of range.
    let handshake = make_handshake_with_proposal(0x00_02_02_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), None);
}

// ── encode_version_response ───────────────────────────────────────────────────

#[test]
fn encode_version_response_wire_format_is_neo4j_compatible() {
    // Wire format: [0x00, 0x00, minor, major]
    let v = BoltVersion { major: 4, minor: 4 };
    let encoded = encode_version_response(Some(v));
    assert_eq!(encoded, [0x00, 0x00, v.minor, v.major]);
}

#[test]
fn encode_version_response_different_version() {
    // Verify with a version different from 4.4 to ensure encoding isn't
    // accidentally correct due to symmetric major/minor values.
    let v = BoltVersion { major: 5, minor: 3 };
    let encoded = encode_version_response(Some(v));
    assert_eq!(encoded, [0x00, 0x00, 0x03, 0x05]);
}

#[test]
fn encode_version_response_none_matches_no_version_constant() {
    let resp = encode_version_response(None);
    assert_eq!(resp, NO_VERSION_RESPONSE);
}

#[test]
fn encode_version_response_supported_version() {
    // Wire format: [0x00, 0x00, minor, major]
    let resp = encode_version_response(Some(SUPPORTED_VERSION));
    assert_eq!(resp[2], SUPPORTED_VERSION.minor);
    assert_eq!(resp[3], SUPPORTED_VERSION.major);
}

// ── NO_VERSION_RESPONSE constant ────────────────────────────────────────────

#[test]
fn no_version_response_is_all_zeros() {
    assert_eq!(NO_VERSION_RESPONSE, [0x00, 0x00, 0x00, 0x00]);
}

// ── BOLT_MAGIC constant ───────────────────────────────────────────────────────

#[test]
fn bolt_magic_is_correct_preamble() {
    assert_eq!(BOLT_MAGIC, [0x60, 0x60, 0xB0, 0x17]);
}

#[test]
fn supported_version_is_bolt_44() {
    assert_eq!(SUPPORTED_VERSION.major, 4);
    assert_eq!(SUPPORTED_VERSION.minor, 4);
}

// ── BoltVersion Display ──────────────────────────────────────────────────────

#[test]
fn bolt_version_display_formats_major_minor() {
    let v = BoltVersion { major: 4, minor: 4 };
    assert_eq!(v.to_string(), "4.4");
}

#[test]
fn bolt_version_display_supported() {
    assert_eq!(SUPPORTED_VERSION.to_string(), "4.4");
}

// ── Neo4j wire compatibility ────────────────────────────────────────────────

#[test]
fn neo4j_driver_proposal_bytes_are_accepted() {
    // The Python neo4j driver sends Version(4,4).to_bytes() = [0x00, 0x00, 0x04, 0x04].
    // Our negotiate_version must accept this.
    let handshake = make_handshake_with_proposal([0x00, 0x00, 0x04, 0x04]);
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn server_response_matches_neo4j_driver_expectation() {
    // The Python neo4j driver's Version.from_bytes() expects:
    //   b[0]=0, b[1]=0, b[3]=major, b[2]=minor
    // For Bolt 4.4 the response must be [0x00, 0x00, 0x04, 0x04].
    let resp = encode_version_response(Some(SUPPORTED_VERSION));
    assert_eq!(resp, [0x00, 0x00, 0x04, 0x04]);
}

#[test]
fn neo4j_driver_proposal_with_range_is_accepted() {
    // Python driver sends [0x00, 0x04, 0x04, 0x04] for Bolt 4.4 with range=4
    // (supports 4.0 through 4.4).
    let handshake = make_handshake_with_proposal([0x00, 0x04, 0x04, 0x04]);
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}
