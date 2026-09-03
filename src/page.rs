//! B-tree page format (sqlite.org/fileformat.html §1.6): page flags, cell pointer arrays, varints,
//! and the local-payload rule that decides where a cell's overflow pointer sits.

/// The four b-tree page kinds, by the flag byte at page offset 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    InteriorIndex,
    InteriorTable,
    LeafIndex,
    LeafTable,
}

impl Kind {
    pub fn from_flag(flag: u8) -> Option<Kind> {
        Some(match flag {
            0x02 => Kind::InteriorIndex,
            0x05 => Kind::InteriorTable,
            0x0a => Kind::LeafIndex,
            0x0d => Kind::LeafTable,
            _ => return None,
        })
    }

    /// The page header is 12 bytes on interior pages (it carries the right-most pointer at
    /// offset 8) and 8 on leaves.
    pub fn header_len(self) -> usize {
        if self.is_interior() {
            12
        } else {
            8
        }
    }

    pub fn is_interior(self) -> bool {
        matches!(self, Kind::InteriorIndex | Kind::InteriorTable)
    }

    pub fn is_index(self) -> bool {
        matches!(self, Kind::InteriorIndex | Kind::LeafIndex)
    }
}

pub(crate) fn be16(b: &[u8], off: usize) -> usize {
    u16::from_be_bytes([b[off], b[off + 1]]) as usize
}

/// A SQLite varint: 1–9 bytes, big-endian, 7 bits per byte, the 9th byte carrying 8 bits.
/// `None` when the slice ends first. Returns (value, encoded length).
pub fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..8 {
        let c = *b.get(i)?;
        v = (v << 7) | u64::from(c & 0x7f);
        if c & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    let last = *b.get(8)?;
    Some(((v << 8) | u64::from(last), 9))
}

/// How many bytes of a `payload`-byte cell are stored on the page itself (the rest is an
/// overflow chain, and the 4-byte pointer to it follows the local bytes). The `X`/`M`/`K`
/// rule from fileformat.html §1.6: `X` is the local maximum (different for table leaves and
/// index cells), `M` the minimum, and `K` the size that leaves the last overflow page as full
/// as possible.
pub fn local_payload(kind: Kind, payload: usize, usable: usize) -> usize {
    let x = if kind.is_index() {
        ((usable - 12) * 64 / 255) - 23
    } else {
        usable - 35
    };
    if payload <= x {
        return payload;
    }
    let m = ((usable - 12) * 32 / 255) - 23;
    let k = m + ((payload - m) % (usable - 4));
    if k <= x {
        k
    } else {
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_lengths() {
        assert_eq!(varint(&[0x05]), Some((5, 1)));
        assert_eq!(varint(&[0x81, 0x00]), Some((128, 2)));
        assert_eq!(varint(&[0x80]), None);
        let nine = [0xffu8; 9];
        assert_eq!(varint(&nine).map(|(_, n)| n), Some(9));
    }

    #[test]
    fn local_payload_matches_the_spec_at_4k() {
        // U = 4096: table-leaf X = 4061; index X = 1002; M = 489; U - 4 = 4092.
        assert_eq!(local_payload(Kind::LeafTable, 4061, 4096), 4061);
        // P = 4062: K = 489 + (3573 % 4092) = 4062 > X, so M.
        assert_eq!(local_payload(Kind::LeafTable, 4062, 4096), 489);
        assert_eq!(local_payload(Kind::LeafIndex, 1002, 4096), 1002);
        // P = 1003 on an index page: K = 1003 > 1002, so M.
        assert_eq!(local_payload(Kind::InteriorIndex, 1003, 4096), 489);
        // P = 6000: K = 489 + (5511 % 4092) = 1908 — fits a table leaf, not an index cell.
        assert_eq!(local_payload(Kind::LeafTable, 6000, 4096), 1908);
        assert_eq!(local_payload(Kind::LeafIndex, 6000, 4096), 489);
        // P = 20000: K = 489 + (19511 % 4092) = 3632.
        assert_eq!(local_payload(Kind::LeafTable, 20_000, 4096), 3632);
    }

    #[test]
    fn local_payload_never_exceeds_usable_at_the_extremes() {
        for &u in &[480usize, 512, 1024, 4096, 65_536] {
            for p in [0usize, 1, u, u + 1, 3 * u, 100_000] {
                for kind in [Kind::LeafTable, Kind::LeafIndex, Kind::InteriorIndex] {
                    let l = local_payload(kind, p, u);
                    assert!(l <= p.max(1));
                    assert!(l + 4 <= u, "kind {kind:?} p {p} u {u} local {l}");
                }
            }
        }
    }
}
