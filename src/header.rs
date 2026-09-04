//! The 100-byte database header (sqlite.org/fileformat.html §1.3), read from sources and
//! patched — three fields only — on the destination.

use crate::error::StitchError;
use std::path::Path;

pub(crate) const MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// SQLite's `PENDING_BYTE`: the page containing this file offset is never used by any database,
/// and an allocator writing past 1 GiB has to skip it.
pub(crate) const PENDING_BYTE: u64 = 0x4000_0000;

const OFF_PAGE_SIZE: usize = 16;
const OFF_WRITE_FORMAT: usize = 18;
const OFF_READ_FORMAT: usize = 19;
const OFF_RESERVED: usize = 20;
const OFF_CHANGE_COUNTER: usize = 24;
const OFF_PAGE_COUNT: usize = 28;
const OFF_FREELIST_TRUNK: usize = 32;
const OFF_FREELIST_COUNT: usize = 36;
const OFF_SCHEMA_COOKIE: usize = 40;
const OFF_SCHEMA_FORMAT: usize = 44;
const OFF_LARGEST_ROOT: usize = 52;
const OFF_ENCODING: usize = 56;
const OFF_INCREMENTAL_VACUUM: usize = 64;
const OFF_VERSION_VALID_FOR: usize = 92;

pub(crate) fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

pub(crate) fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/// The header fields the linker reads. Fields it never looks at (default cache size, user
/// version, application id, the SQLite version stamp) are not modelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub page_size: u32,
    /// Bytes 18 / 19: `1` rollback, `2` wal, `3` wal2 (the vendored wal2 build; stock SQLite
    /// refuses `3`).
    pub write_format: u8,
    pub read_format: u8,
    pub reserved: u8,
    pub change_counter: u32,
    /// Offset 28 — trusted only when `change_counter == version_valid_for`.
    pub page_count: u32,
    pub freelist_trunk: u32,
    pub freelist_count: u32,
    pub schema_cookie: u32,
    pub schema_format: u32,
    /// Offset 52: non-zero only under `auto_vacuum`.
    pub largest_root: u32,
    /// Offset 56: `1` UTF-8, `2` UTF-16le, `3` UTF-16be.
    pub encoding: u32,
    pub incremental_vacuum: u32,
    pub version_valid_for: u32,
}

impl Header {
    pub(crate) fn parse(path: &Path, p: &[u8]) -> Result<Header, StitchError> {
        let bad = |why: &str| StitchError::NotSqlite {
            path: path.to_path_buf(),
            why: why.to_string(),
        };
        if p.len() < 100 || &p[..16] != MAGIC {
            return Err(bad("missing the `SQLite format 3` header"));
        }
        let raw = u16::from_be_bytes([p[OFF_PAGE_SIZE], p[OFF_PAGE_SIZE + 1]]);
        let page_size = if raw == 1 { 65_536 } else { u32::from(raw) };
        if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(bad(&format!(
                "page size {page_size} is not a power of two in 512..=65536"
            )));
        }
        if p[21] != 64 || p[22] != 32 || p[23] != 32 {
            return Err(bad("payload fractions are not 64/32/32"));
        }
        let h = Header {
            page_size,
            write_format: p[OFF_WRITE_FORMAT],
            read_format: p[OFF_READ_FORMAT],
            reserved: p[OFF_RESERVED],
            change_counter: be32(p, OFF_CHANGE_COUNTER),
            page_count: be32(p, OFF_PAGE_COUNT),
            freelist_trunk: be32(p, OFF_FREELIST_TRUNK),
            freelist_count: be32(p, OFF_FREELIST_COUNT),
            schema_cookie: be32(p, OFF_SCHEMA_COOKIE),
            schema_format: be32(p, OFF_SCHEMA_FORMAT),
            largest_root: be32(p, OFF_LARGEST_ROOT),
            encoding: be32(p, OFF_ENCODING),
            incremental_vacuum: be32(p, OFF_INCREMENTAL_VACUUM),
            version_valid_for: be32(p, OFF_VERSION_VALID_FOR),
        };
        if h.usable() < 480 {
            return Err(bad("usable page size below 480 bytes"));
        }
        if !(1..=3).contains(&h.encoding) {
            return Err(bad(&format!("unknown text encoding {}", h.encoding)));
        }
        if h.write_format > 3 || h.read_format > 3 {
            return Err(bad("unknown file format version (bytes 18/19)"));
        }
        Ok(h)
    }

    /// Page size minus the reserved bytes at the end of every page.
    pub fn usable(&self) -> usize {
        self.page_size as usize - self.reserved as usize
    }

    /// The page number the allocator must skip (design 415 §1).
    pub fn lock_byte_page(&self) -> u32 {
        (PENDING_BYTE / u64::from(self.page_size)) as u32 + 1
    }

    pub fn page_count_is_valid(&self) -> bool {
        self.change_counter == self.version_valid_for
    }

    pub fn encoding_name(&self) -> &'static str {
        match self.encoding {
            1 => "UTF-8",
            2 => "UTF-16le",
            3 => "UTF-16be",
            _ => "unknown",
        }
    }

    pub fn journal_format_name(&self) -> &'static str {
        match self.read_format {
            1 => "rollback",
            2 => "wal",
            3 => "wal2",
            _ => "unknown",
        }
    }
}

/// After linking: the new page count, the freelist head (normally empty — see
/// `link::write_freelist`), and the change counter bumped in both places so SQLite trusts the
/// count (`change_counter == version_valid_for`, fileformat.html §1.3.7).
pub(crate) fn patch_after_link(page1: &mut [u8], pages: u32, freelist_trunk: u32, freelist: u32) {
    put32(page1, OFF_PAGE_COUNT, pages);
    put32(page1, OFF_FREELIST_TRUNK, freelist_trunk);
    put32(page1, OFF_FREELIST_COUNT, freelist);
    let cc = be32(page1, OFF_CHANGE_COUNTER).wrapping_add(1);
    put32(page1, OFF_CHANGE_COUNTER, cc);
    put32(page1, OFF_VERSION_VALID_FOR, cc);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(page_size: u16) -> Vec<u8> {
        let mut p = vec![0u8; 100];
        p[..16].copy_from_slice(MAGIC);
        p[16..18].copy_from_slice(&page_size.to_be_bytes());
        p[18] = 1;
        p[19] = 1;
        p[21] = 64;
        p[22] = 32;
        p[23] = 32;
        put32(&mut p, OFF_SCHEMA_FORMAT, 4);
        put32(&mut p, OFF_ENCODING, 1);
        p
    }

    #[test]
    fn parses_and_encodes_the_64k_page_size() {
        let p = header_bytes(1);
        let h = Header::parse(Path::new("x"), &p).unwrap();
        assert_eq!(h.page_size, 65_536);
        assert_eq!(h.lock_byte_page(), 16_385);
        let h4 = Header::parse(Path::new("x"), &header_bytes(4096)).unwrap();
        assert_eq!(h4.lock_byte_page(), 262_145);
    }

    #[test]
    fn patch_bumps_both_counters() {
        let mut p = header_bytes(4096);
        put32(&mut p, OFF_CHANGE_COUNTER, 7);
        put32(&mut p, OFF_VERSION_VALID_FOR, 7);
        patch_after_link(&mut p, 1234, 0, 0);
        let h = Header::parse(Path::new("x"), &p).unwrap();
        assert_eq!(h.page_count, 1234);
        assert_eq!(h.change_counter, 8);
        assert_eq!(h.freelist_count, 0);
        assert!(h.page_count_is_valid());
        patch_after_link(&mut p, 1234, 99, 3);
        let h = Header::parse(Path::new("x"), &p).unwrap();
        assert_eq!(h.freelist_trunk, 99);
        assert_eq!(h.freelist_count, 3);
    }

    #[test]
    fn rejects_non_sqlite() {
        assert!(matches!(
            Header::parse(Path::new("x"), b"hello"),
            Err(StitchError::NotSqlite { .. })
        ));
    }
}
