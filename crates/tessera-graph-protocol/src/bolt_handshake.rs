// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! Bolt protocol handshake — version negotiation.

/// Magic preamble bytes that start every Bolt connection.
pub const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Response indicating no supported version was found.
pub const NO_VERSION_RESPONSE: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

/// A Bolt protocol version (major.minor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoltVersion {
    /// Major version number.
    pub major: u8,
    /// Minor version number.
    pub minor: u8,
}

impl std::fmt::Display for BoltVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The Bolt version this server supports.
pub const SUPPORTED_VERSION: BoltVersion = BoltVersion { major: 4, minor: 4 };

/// Parse a version proposal `u32` into `(major, minor, range, padding)`.
///
/// Wire format (big-endian u32): `[padding, range, minor, major]` where:
/// - byte 0 = padding (should be 0x00 per spec; not enforced — see below)
/// - byte 1 = range (how many minor versions back from `minor` are supported)
/// - byte 2 = minor version (the highest minor supported)
/// - byte 3 = major version
///
/// This matches the Neo4j Bolt specification and the Python driver's
/// `Version.to_bytes()` / `Version.from_bytes()` layout.
///
/// The padding byte is returned but **not validated**: the Bolt specification
/// states it should be `0x00`, but real-world drivers may not enforce this.
/// We follow a lenient approach (same as Neo4j server) to maximise
/// interoperability. Callers that need strict validation can check
/// `padding == 0` themselves.
///
/// Returns the tuple `(major, minor, range, padding)`.
#[must_use]
pub const fn parse_version_proposal(raw: u32) -> (u8, u8, u8, u8) {
    let bytes = raw.to_be_bytes();
    (bytes[3], bytes[2], bytes[1], bytes[0])
}

/// Negotiate the protocol version from a 20-byte client handshake.
///
/// The first 4 bytes must be [`BOLT_MAGIC`]. The remaining 16 bytes are
/// four big-endian version proposals. Returns [`SUPPORTED_VERSION`] if any
/// proposal's range covers it, or `None` if no proposal matches.
///
/// The returned version is always the server's own [`SUPPORTED_VERSION`],
/// not the client's maximum minor — the server advertises a single version,
/// and the negotiation checks whether the client can accept it.
///
/// If the same version appears in multiple proposal slots, the first match
/// wins — duplicates are harmless.
#[must_use]
pub fn negotiate_version(handshake: &[u8; 20]) -> Option<BoltVersion> {
    if handshake[..4] != BOLT_MAGIC {
        return None;
    }
    for chunk in handshake[4..].chunks_exact(4) {
        // chunks_exact(4) guarantees exactly 4 bytes per chunk.
        let raw = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let (major, minor, range, _padding) = parse_version_proposal(raw);
        if major == 0 && minor == 0 {
            continue;
        }
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
/// Wire format: `[0x00, 0x00, minor, major]` — matches the Neo4j Bolt
/// specification. The Python driver's `Version.from_bytes()` reads
/// `major = b[3]`, `minor = b[2]`.
///
/// If `version` is `None`, returns [`NO_VERSION_RESPONSE`].
#[must_use]
pub const fn encode_version_response(version: Option<BoltVersion>) -> [u8; 4] {
    match version {
        Some(v) => [0x00, 0x00, v.minor, v.major],
        None => NO_VERSION_RESPONSE,
    }
}
