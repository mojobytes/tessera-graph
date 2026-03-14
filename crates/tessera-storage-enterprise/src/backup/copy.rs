use std::path::Path;

use crate::backup::manifest::FileEntry;
use crate::error::Result;

/// Copies a single file from `src_file_path` to `<dst_dir>/<name>`,
/// computing the CRC32 checksum of the bytes read.
///
/// Returns a [`FileEntry`] with the file's name, size, and checksum.
///
/// # Errors
///
/// Returns an I/O error if the source file cannot be read or the
/// destination file cannot be written.
#[allow(clippy::cast_possible_truncation)]
pub fn copy_file_with_checksum(
    src_file_path: &Path,
    dst_dir: &Path,
    name: &str,
) -> Result<FileEntry> {
    let bytes = std::fs::read(src_file_path)?;
    let crc32 = crc32fast::hash(&bytes);
    let size_bytes = bytes.len() as u64;

    let dst_path = dst_dir.join(name);
    std::fs::write(&dst_path, &bytes)?;

    Ok(FileEntry {
        name: name.to_owned(),
        size_bytes,
        crc32,
    })
}

/// Computes the CRC32 of a file's contents without copying it.
///
/// Used during verification to compare against the stored checksum.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be read.
pub fn checksum_file(path: &Path) -> Result<u32> {
    let bytes = std::fs::read(path)?;
    Ok(crc32fast::hash(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn copy_file_produces_identical_bytes() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let src_path = src_dir.path().join("data.bin");
        std::fs::write(&src_path, b"hello tessera").unwrap();

        let entry = copy_file_with_checksum(&src_path, dst_dir.path(), "data.bin").unwrap();

        let dst_bytes = std::fs::read(dst_dir.path().join("data.bin")).unwrap();
        assert_eq!(dst_bytes, b"hello tessera");
        assert_eq!(entry.name, "data.bin");
        assert_eq!(entry.size_bytes, 13);
    }

    #[test]
    fn crc32_checksum_is_deterministic() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir1 = TempDir::new().unwrap();
        let dst_dir2 = TempDir::new().unwrap();

        let src_path = src_dir.path().join("f.bin");
        std::fs::write(&src_path, b"deterministic").unwrap();

        let e1 = copy_file_with_checksum(&src_path, dst_dir1.path(), "f.bin").unwrap();
        let e2 = copy_file_with_checksum(&src_path, dst_dir2.path(), "f.bin").unwrap();
        assert_eq!(e1.crc32, e2.crc32);
        assert_ne!(e1.crc32, 0);
    }

    #[test]
    fn empty_file_copy_produces_zero_size_entry() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("empty.db");
        std::fs::write(&src_path, b"").unwrap();

        let entry = copy_file_with_checksum(&src_path, dst_dir.path(), "empty.db").unwrap();
        assert_eq!(entry.size_bytes, 0);
    }

    #[test]
    fn checksum_file_matches_crc32_of_contents() {
        let dir = TempDir::new().unwrap();
        let content = b"checksum_test_data";
        let path = dir.path().join("c.bin");
        std::fs::write(&path, content).unwrap();

        let crc = checksum_file(&path).unwrap();
        assert_eq!(crc, crc32fast::hash(content));
    }

    #[test]
    fn copy_nonexistent_file_returns_error() {
        let dst_dir = TempDir::new().unwrap();
        let result = copy_file_with_checksum(
            Path::new("/nonexistent/ghost.db"),
            dst_dir.path(),
            "ghost.db",
        );
        assert!(result.is_err());
    }
}
