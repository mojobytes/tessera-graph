// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Integration tests for Bolt protocol handshake — version negotiation.

use tessera_graph_protocol::{
    BOLT_MAGIC, BoltVersion, SUPPORTED_VERSION, encode_version_response, negotiate_version,
    parse_version_proposal,
};

// ── parse_version_proposal ────────────────────────────────────────────────────

#[test]
fn parse_version_proposal_bolt44() {
    // Wire encoding for Bolt 4.4: major=4, range=4, minor=4 → 0x00_04_04_04
    let (major, range, minor) = parse_version_proposal(0x00_04_04_04);
    assert_eq!(major, 4);
    assert_eq!(range, 4);
    assert_eq!(minor, 4);
}

#[test]
fn parse_version_proposal_zero() {
    let (major, range, minor) = parse_version_proposal(0);
    assert_eq!((major, range, minor), (0, 0, 0));
}

#[test]
fn parse_version_proposal_bolt54() {
    // Bolt 5.4: major=5, range=4, minor=4 → 0x00_05_04_04
    let (major, range, minor) = parse_version_proposal(0x00_05_04_04);
    assert_eq!(major, 5);
    assert_eq!(range, 4);
    assert_eq!(minor, 4);
}

#[test]
fn parse_version_proposal_extracts_all_bytes() {
    // 0x00_01_02_03 → padding=0, major=1, range=2, minor=3
    let (major, range, minor) = parse_version_proposal(0x00_01_02_03);
    assert_eq!(major, 1);
    assert_eq!(range, 2);
    assert_eq!(minor, 3);
}

// ── negotiate_version ─────────────────────────────────────────────────────────

#[test]
fn negotiate_version_bolt44_accepted() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    let result = negotiate_version(&handshake);
    assert_eq!(result, Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_version_no_match() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    // Bolt 6.0 — not supported
    handshake[4..8].copy_from_slice(&0x00_06_00_00u32.to_be_bytes());
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
    handshake[4..8].copy_from_slice(&0x00_06_00_00u32.to_be_bytes());
    // Second proposal: Bolt 4.4 — match
    handshake[8..12].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_accepts_third_proposal() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x00_06_00_00u32.to_be_bytes());
    handshake[8..12].copy_from_slice(&0x00_05_00_00u32.to_be_bytes());
    // Third proposal: Bolt 4.4 — match
    handshake[12..16].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_accepts_fourth_proposal() {
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x00_06_00_00u32.to_be_bytes());
    handshake[8..12].copy_from_slice(&0x00_05_00_00u32.to_be_bytes());
    handshake[12..16].copy_from_slice(&0x00_03_00_00u32.to_be_bytes());
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
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    // major=4, range=4, minor=4 → supports 4.0 through 4.4
    handshake[4..8].copy_from_slice(&0x00_04_04_04u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), Some(SUPPORTED_VERSION));
}

#[test]
fn negotiate_range_does_not_cover_supported_minor() {
    // major=4, range=2, minor=2 → supports only 4.0–4.2; our 4.4 is out of range.
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x00_04_02_02u32.to_be_bytes());
    assert_eq!(negotiate_version(&handshake), None);
}

// ── encode_version_response ───────────────────────────────────────────────────

#[test]
fn encode_version_response_some() {
    let resp = encode_version_response(Some(BoltVersion { major: 4, minor: 4 }));
    assert_eq!(resp, [0x00, 0x04, 0x04, 0x00]);
}

#[test]
fn encode_version_response_none() {
    let resp = encode_version_response(None);
    assert_eq!(resp, [0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn encode_version_response_supported_version() {
    let resp = encode_version_response(Some(SUPPORTED_VERSION));
    assert_eq!(resp[1], SUPPORTED_VERSION.major);
    assert_eq!(resp[2], SUPPORTED_VERSION.minor);
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
