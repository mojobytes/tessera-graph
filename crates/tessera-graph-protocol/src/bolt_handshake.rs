// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Bolt protocol handshake — version negotiation.

/// Magic preamble bytes that start every Bolt connection.
pub const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// A Bolt protocol version (major.minor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoltVersion {
    /// Major version number.
    pub major: u8,
    /// Minor version number.
    pub minor: u8,
}

/// The Bolt version this server supports.
pub const SUPPORTED_VERSION: BoltVersion = BoltVersion { major: 4, minor: 4 };

/// Parse a version proposal `u32` (from the handshake) into `(major, range, minor)`.
///
/// Wire format: `0x00_MM_RR_PP` where:
/// - `MM` = major version (0–255)
/// - `RR` = range (how many minor versions back from `PP` are supported)
/// - `PP` = minor version (the highest minor supported)
#[must_use]
pub const fn parse_version_proposal(raw: u32) -> (u8, u8, u8) {
    let bytes = raw.to_be_bytes();
    // bytes[0] is padding (always 0)
    let major = bytes[1];
    let range = bytes[2];
    let minor = bytes[3];
    (major, range, minor)
}

/// Negotiate the protocol version from a 20-byte client handshake.
///
/// The first 4 bytes must be [`BOLT_MAGIC`]. The remaining 16 bytes are
/// four `u32` big-endian version proposals. Returns the first proposal that
/// matches the server's supported version, or `None` if no match.
#[must_use]
pub fn negotiate_version(handshake: &[u8; 20]) -> Option<BoltVersion> {
    if handshake[..4] != BOLT_MAGIC {
        return None;
    }
    for i in 0..4 {
        let offset = 4 + i * 4;
        let raw = u32::from_be_bytes([
            handshake[offset],
            handshake[offset + 1],
            handshake[offset + 2],
            handshake[offset + 3],
        ]);
        if raw == 0 {
            continue;
        }
        let (major, range, minor) = parse_version_proposal(raw);
        if major == SUPPORTED_VERSION.major {
            let min_minor = minor.saturating_sub(range);
            if min_minor <= SUPPORTED_VERSION.minor && SUPPORTED_VERSION.minor <= minor {
                return Some(SUPPORTED_VERSION);
            }
        }
    }
    None
}

/// Encode the server's version response as 4 bytes.
///
/// If `version` is `None`, returns `[0, 0, 0, 0]` (no supported version).
#[must_use]
// Closures are not yet stable in const fn, so map_or cannot be used here.
#[allow(clippy::option_if_let_else)]
pub const fn encode_version_response(version: Option<BoltVersion>) -> [u8; 4] {
    match version {
        Some(v) => [0x00, v.major, v.minor, 0x00],
        None => [0x00, 0x00, 0x00, 0x00],
    }
}
