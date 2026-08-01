// SPDX-License-Identifier: BSL-1.1

//! Zeroising plaintext holder. No `Display`, no `Serialize`, no `ToString`.

/// Byte container that zeros itself on drop and redacts in `Debug`.
///
/// Move plaintext passwords through the auth pipeline inside this type.
/// Never log, never serialise, never embed in an error value.
pub struct SecretString(Box<[u8]>);

impl SecretString {
    /// Move the bytes of `s` into a zeroising box.
    ///
    /// The input `String` is consumed; the returned `SecretString` owns the
    /// only remaining copy. Drop of this value overwrites the bytes with
    /// zeros.
    #[must_use]
    pub fn new(s: String) -> Self {
        Self(s.into_bytes().into_boxed_slice())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        for b in &mut self.0 {
            *b = 0;
        }
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(<redacted {} bytes>)", self.0.len())
    }
}
