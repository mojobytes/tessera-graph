use crate::error::{EnterpriseError, Result};

/// Metadata for a single file in the backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Filename (no path prefix).
    pub name: String,
    /// Size in bytes at the time of backup.
    pub size_bytes: u64,
    /// CRC32 checksum of the file contents.
    pub crc32: u32,
}

/// Manifest written alongside backup files.
///
/// Serialized as a simple line-based text format:
/// ```text
/// tessera_backup_v1
/// created_at=<unix_secs>
/// snapshot_lsn=<lsn>
/// files=<count>
/// <name> <size_bytes> <crc32_hex>
/// ...
/// ```
#[derive(Debug, Clone)]
pub struct BackupManifest {
    /// Unix timestamp (seconds) when the backup was created.
    pub created_at_unix_secs: u64,
    /// The WAL LSN at the consistency point.
    pub snapshot_lsn: u64,
    /// Ordered list of files in the backup.
    pub files: Vec<FileEntry>,
}

impl BackupManifest {
    /// Returns the number of files listed in the manifest.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Serializes the manifest to a UTF-8 string.
    #[must_use]
    pub fn serialize(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        writeln!(out, "tessera_backup_v1").unwrap();
        writeln!(out, "created_at={}", self.created_at_unix_secs).unwrap();
        writeln!(out, "snapshot_lsn={}", self.snapshot_lsn).unwrap();
        writeln!(out, "files={}", self.files.len()).unwrap();
        for f in &self.files {
            writeln!(out, "{} {} {:08x}", f.name, f.size_bytes, f.crc32).unwrap();
        }
        out
    }

    /// Parses a manifest from its serialized text representation.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseError::ManifestCorrupt`] if any field is missing
    /// or cannot be parsed.
    pub fn parse(s: &str) -> Result<Self> {
        let mut lines = s.lines();

        let header = lines.next().ok_or_else(|| corrupt("missing header"))?;
        if header != "tessera_backup_v1" {
            return Err(corrupt("unknown format version"));
        }

        let created_at_unix_secs = parse_u64_field(lines.next(), "created_at")?;
        let snapshot_lsn = parse_u64_field(lines.next(), "snapshot_lsn")?;
        let file_count = parse_usize_field(lines.next(), "files")?;

        let mut files = Vec::with_capacity(file_count);
        for i in 0..file_count {
            let line = lines.next().ok_or_else(|| {
                corrupt(format!("expected file entry {i}, found end of manifest"))
            })?;
            let entry = parse_file_entry(line)?;
            files.push(entry);
        }

        Ok(Self {
            created_at_unix_secs,
            snapshot_lsn,
            files,
        })
    }
}

// ── Private helpers ──────────────────────────────────────────────────

fn corrupt(msg: impl Into<String>) -> EnterpriseError {
    EnterpriseError::ManifestCorrupt(msg.into())
}

fn parse_u64_field(line: Option<&str>, key: &str) -> Result<u64> {
    let line = line.ok_or_else(|| corrupt(format!("missing field '{key}'")))?;
    let value = line
        .strip_prefix(&format!("{key}="))
        .ok_or_else(|| corrupt(format!("malformed field '{key}': got '{line}'")))?;
    value
        .parse::<u64>()
        .map_err(|_| corrupt(format!("field '{key}' is not a u64: '{value}'")))
}

#[allow(clippy::cast_possible_truncation)]
fn parse_usize_field(line: Option<&str>, key: &str) -> Result<usize> {
    parse_u64_field(line, key).map(|v| v as usize)
}

fn parse_file_entry(line: &str) -> Result<FileEntry> {
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return Err(corrupt(format!("malformed file entry: '{line}'")));
    }
    let name = parts[0].to_owned();
    let size_bytes = parts[1]
        .parse::<u64>()
        .map_err(|_| corrupt(format!("invalid size in file entry: '{}'", parts[1])))?;
    let crc32 = u32::from_str_radix(parts[2], 16)
        .map_err(|_| corrupt(format!("invalid crc32 in file entry: '{}'", parts[2])))?;
    Ok(FileEntry {
        name,
        size_bytes,
        crc32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_serialize_parse() {
        let original = BackupManifest {
            created_at_unix_secs: 1_700_000_000,
            snapshot_lsn: 42,
            files: vec![
                FileEntry {
                    name: "nodes.db".to_owned(),
                    size_bytes: 4096,
                    crc32: 0xDEAD_BEEF,
                },
                FileEntry {
                    name: "graph.meta".to_owned(),
                    size_bytes: 4096,
                    crc32: 0xCAFE_BABE,
                },
            ],
        };

        let serialized = original.serialize();
        let parsed = BackupManifest::parse(&serialized).unwrap();

        assert_eq!(parsed.created_at_unix_secs, original.created_at_unix_secs);
        assert_eq!(parsed.snapshot_lsn, original.snapshot_lsn);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].name, "nodes.db");
        assert_eq!(parsed.files[0].crc32, 0xDEAD_BEEF);
        assert_eq!(parsed.files[1].name, "graph.meta");
    }

    #[test]
    fn manifest_parse_empty_files_list() {
        let m = BackupManifest {
            created_at_unix_secs: 0,
            snapshot_lsn: 1,
            files: vec![],
        };
        let s = m.serialize();
        let parsed = BackupManifest::parse(&s).unwrap();
        assert_eq!(parsed.files.len(), 0);
    }

    #[test]
    fn manifest_parse_returns_error_on_garbage() {
        let result = BackupManifest::parse("not a manifest");
        assert!(result.is_err());
    }

    #[test]
    fn manifest_file_count() {
        let m = BackupManifest {
            created_at_unix_secs: 0,
            snapshot_lsn: 0,
            files: vec![
                FileEntry {
                    name: "a".to_owned(),
                    size_bytes: 1,
                    crc32: 0,
                },
                FileEntry {
                    name: "b".to_owned(),
                    size_bytes: 2,
                    crc32: 1,
                },
                FileEntry {
                    name: "c".to_owned(),
                    size_bytes: 3,
                    crc32: 2,
                },
            ],
        };
        assert_eq!(m.file_count(), 3);
    }
}
