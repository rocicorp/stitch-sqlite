//! The walk (design 415 §3.2): allocate-at-discovery, O(depth) memory, every source page read
//! once and written once, with the structural checks of §3.4 on every page it touches.

use std::fs::File;
use std::os::unix::fs::FileExt;

use serde::Serialize;

use crate::error::{io_err, StitchError};
use crate::header::{be32, put32};
use crate::page::{be16, local_payload, varint, Kind};
use crate::source::Source;

/// What the walk did, by page kind.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct LinkStats {
    /// Every page written to the destination (roots included).
    pub pages: u64,
    /// Interior pages: child pointers and the right-most pointer rewritten.
    pub interior: u64,
    /// Leaf pages, of which …
    pub leaf: u64,
    /// … these had at least one overflow pointer rewritten; the rest were byte copies.
    pub leaf_with_overflow: u64,
    /// Overflow pages: the 4-byte `next` rewritten.
    pub overflow: u64,
    /// B-trees linked (tables, indexes, autoindexes).
    pub trees: u64,
    /// Whether the 1 GiB lock-byte page fell inside the output and was skipped.
    pub lock_page_skipped: bool,
}

/// Hands out destination page numbers in discovery order, never the lock-byte page.
pub(crate) struct Allocator {
    next: u32,
    lock: u32,
    skipped: bool,
}

impl Allocator {
    pub(crate) fn new(first_free: u32, lock_byte_page: u32) -> Allocator {
        Allocator {
            next: first_free,
            lock: lock_byte_page,
            skipped: false,
        }
    }

    pub(crate) fn alloc(&mut self) -> u32 {
        if self.next == self.lock {
            self.next += 1;
            self.skipped = true;
        }
        let p = self.next;
        self.next += 1;
        p
    }

    /// Pages the destination now spans (the skipped lock-byte page included).
    pub(crate) fn pages(&self) -> u32 {
        self.next - 1
    }

    pub(crate) fn skipped_lock(&self) -> bool {
        self.skipped
    }
}

pub(crate) struct Linker<'a> {
    source: &'a Source,
    src: File,
    dst: &'a File,
    page_size: usize,
    usable: usize,
}

/// The mutable state of one tree walk: the allocator and stats it shares with the other trees,
/// the cycle guard, and the scratch page for overflow chains.
struct Walk<'w> {
    alloc: &'w mut Allocator,
    st: &'w mut LinkStats,
    visited: u64,
    obuf: Vec<u8>,
}

impl<'a> Linker<'a> {
    pub(crate) fn open(source: &'a Source, dst: &'a File) -> Result<Linker<'a>, StitchError> {
        let src =
            File::open(&source.path).map_err(io_err(format!("open {}", source.path.display())))?;
        Ok(Linker {
            source,
            src,
            dst,
            page_size: source.header.page_size as usize,
            usable: source.header.usable(),
        })
    }

    fn corrupt(&self, page: u32, why: impl Into<String>) -> StitchError {
        StitchError::Corrupt {
            path: self.source.path.clone(),
            page,
            why: why.into(),
        }
    }

    /// A page number found inside `referrer` must name a real page other than page 1.
    fn check_pgno(&self, referrer: u32, pgno: u32) -> Result<(), StitchError> {
        if pgno < 2 || pgno > self.source.page_count {
            return Err(self.corrupt(
                referrer,
                format!(
                    "points at page {pgno}, outside 2..={}",
                    self.source.page_count
                ),
            ));
        }
        Ok(())
    }

    fn read(&self, pgno: u32, buf: &mut [u8]) -> Result<(), StitchError> {
        let off = (u64::from(pgno) - 1) * self.page_size as u64;
        self.src.read_exact_at(buf, off).map_err(io_err(format!(
            "read page {pgno} of {}",
            self.source.path.display()
        )))
    }

    fn write(&self, pgno: u32, buf: &[u8]) -> Result<(), StitchError> {
        let off = (u64::from(pgno) - 1) * self.page_size as u64;
        self.dst
            .write_all_at(buf, off)
            .map_err(io_err(format!("write destination page {pgno}")))
    }

    /// Rewrite the overflow pointer at `ptr_off` in `page` (source page `sp`) and copy the chain
    /// behind it, page by page, allocating each destination page as its predecessor is met.
    fn relocate_overflow(
        &self,
        sp: u32,
        page: &mut [u8],
        ptr_off: usize,
        w: &mut Walk<'_>,
    ) -> Result<(), StitchError> {
        if ptr_off + 4 > self.usable {
            return Err(self.corrupt(sp, "overflow pointer lies outside the usable area"));
        }
        let mut src_pgno = be32(page, ptr_off);
        self.check_pgno(sp, src_pgno)?;
        let mut dst_pgno = w.alloc.alloc();
        put32(page, ptr_off, dst_pgno);
        loop {
            w.visited += 1;
            if w.visited > u64::from(self.source.page_count) {
                return Err(self.corrupt(src_pgno, "overflow chain never ends (cycle)"));
            }
            self.read(src_pgno, &mut w.obuf)?;
            let next = be32(&w.obuf, 0);
            let dst_next = if next == 0 {
                0
            } else {
                self.check_pgno(src_pgno, next)?;
                w.alloc.alloc()
            };
            put32(&mut w.obuf, 0, dst_next);
            self.write(dst_pgno, &w.obuf)?;
            w.st.overflow += 1;
            w.st.pages += 1;
            if next == 0 {
                return Ok(());
            }
            src_pgno = next;
            dst_pgno = dst_next;
        }
    }

    /// Link one b-tree: `src_root` in the source becomes `dst_root` in the destination.
    pub(crate) fn link_tree(
        &self,
        src_root: u32,
        dst_root: u32,
        alloc: &mut Allocator,
        st: &mut LinkStats,
    ) -> Result<(), StitchError> {
        let mut buf = vec![0u8; self.page_size];
        let mut w = Walk {
            alloc,
            st,
            visited: 0,
            obuf: vec![0u8; self.page_size],
        };
        let mut stack: Vec<(u32, u32)> = vec![(src_root, dst_root)];
        let mut kids: Vec<(u32, u32)> = Vec::new();
        self.check_pgno(src_root, src_root)?;
        while let Some((sp, dp)) = stack.pop() {
            w.visited += 1;
            if w.visited > u64::from(self.source.page_count) {
                return Err(
                    self.corrupt(sp, "b-tree visits more pages than the file holds (cycle)")
                );
            }
            self.read(sp, &mut buf)?;
            let kind = Kind::from_flag(buf[0]).ok_or_else(|| {
                self.corrupt(sp, format!("unexpected b-tree flag {:#04x}", buf[0]))
            })?;
            let hdr = kind.header_len();
            let ncell = be16(&buf, 3);
            let array_end = hdr + 2 * ncell;
            if array_end > self.usable {
                return Err(self.corrupt(sp, format!("{ncell} cells overrun the page")));
            }
            let mut touched = false;
            kids.clear();
            for i in 0..ncell {
                let off = be16(&buf, hdr + 2 * i);
                if off < array_end || off >= self.usable {
                    return Err(self.corrupt(
                        sp,
                        format!("cell {i} at offset {off} lies outside the cell area"),
                    ));
                }
                let mut cursor = off;
                if kind.is_interior() {
                    if off + 4 > self.usable {
                        return Err(
                            self.corrupt(sp, format!("cell {i} child pointer overruns the page"))
                        );
                    }
                    let child = be32(&buf, off);
                    self.check_pgno(sp, child)?;
                    let nd = w.alloc.alloc();
                    put32(&mut buf, off, nd);
                    kids.push((child, nd));
                    cursor += 4;
                    if kind == Kind::InteriorTable {
                        continue; // interior table cells carry a rowid and nothing else
                    }
                }
                let (payload, n) = varint(&buf[cursor..self.usable]).ok_or_else(|| {
                    self.corrupt(
                        sp,
                        format!("cell {i}: payload size varint runs off the page"),
                    )
                })?;
                cursor += n;
                if kind == Kind::LeafTable {
                    let (_rowid, n) = varint(&buf[cursor..self.usable]).ok_or_else(|| {
                        self.corrupt(sp, format!("cell {i}: rowid varint runs off the page"))
                    })?;
                    cursor += n;
                }
                let payload = payload as usize;
                let local = local_payload(kind, payload, self.usable);
                if payload > local {
                    touched = true;
                    self.relocate_overflow(sp, &mut buf, cursor + local, &mut w)?;
                }
            }
            if kind.is_interior() {
                let right = be32(&buf, 8);
                self.check_pgno(sp, right)?;
                let nd = w.alloc.alloc();
                put32(&mut buf, 8, nd);
                kids.push((right, nd));
                // Visit left to right: push in reverse so the leftmost child pops first.
                stack.extend(kids.iter().rev().copied());
                w.st.interior += 1;
            } else {
                w.st.leaf += 1;
                if touched {
                    w.st.leaf_with_overflow += 1;
                }
            }
            self.write(dp, &buf)?;
            w.st.pages += 1;
        }
        w.st.trees += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Allocator;

    #[test]
    fn allocator_skips_the_lock_byte_page_once() {
        let mut a = Allocator::new(10, 12);
        assert_eq!(a.alloc(), 10);
        assert_eq!(a.alloc(), 11);
        assert_eq!(a.alloc(), 13);
        assert_eq!(a.alloc(), 14);
        assert!(a.skipped_lock());
        assert_eq!(a.pages(), 14);
        let mut b = Allocator::new(2, 1_000_000);
        b.alloc();
        assert!(!b.skipped_lock());
        assert_eq!(b.pages(), 2);
    }
}
