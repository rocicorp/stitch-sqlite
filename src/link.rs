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

impl LinkStats {
    /// Fold another source's counts in (the parallel walk gives each thread its own).
    pub(crate) fn merge(&mut self, other: &LinkStats) {
        self.pages += other.pages;
        self.interior += other.interior;
        self.leaf += other.leaf;
        self.leaf_with_overflow += other.leaf_with_overflow;
        self.overflow += other.overflow;
        self.trees += other.trees;
        self.lock_page_skipped |= other.lock_page_skipped;
    }
}

/// Put `leaves` on the destination's freelist and return `(first trunk, total freelist pages)`.
///
/// The safety net for the reserved-range walk: a page inside a source's range that the walk
/// never used would otherwise be reachable from nothing, and `integrity_check` reports exactly
/// that ("Page N is never used"). Reservation is computed to be exact, so this normally gets an
/// empty slice and the output keeps the empty freelist `differential.rs` asserts.
pub(crate) fn write_freelist(
    dst: &File,
    page_size: usize,
    usable: usize,
    leaves: &[u32],
) -> Result<(u32, u32), StitchError> {
    if leaves.is_empty() {
        return Ok((0, 0));
    }
    // A trunk page is [next trunk][leaf count][leaf page numbers…]; one of the spare pages
    // becomes each trunk. Built back to front so every trunk knows its successor.
    let per_trunk = (usable - 8) / 4;
    let mut page = vec![0u8; page_size];
    let (mut trunk, mut total) = (0u32, 0u32);
    let mut rest = leaves;
    while !rest.is_empty() {
        // One of the spare pages becomes this trunk; the rest of the slice fills its leaf array.
        let take = (rest.len() - 1).min(per_trunk);
        let split = rest.len() - take;
        let trunk_page = rest[split - 1];
        page.iter_mut().for_each(|b| *b = 0);
        put32(&mut page, 0, trunk);
        put32(&mut page, 4, take as u32);
        for (i, &leaf) in rest[split..].iter().enumerate() {
            put32(&mut page, 8 + 4 * i, leaf);
        }
        let off = (u64::from(trunk_page) - 1) * page_size as u64;
        dst.write_all_at(&page, off)
            .map_err(io_err(format!("write freelist trunk page {trunk_page}")))?;
        trunk = trunk_page;
        total += take as u32 + 1;
        rest = &rest[..split - 1];
    }
    Ok((trunk, total))
}

/// Hands out destination page numbers in discovery order, never the lock-byte page.
///
/// `limit` is the exclusive end of the range this allocator may use. It is `None` for the
/// sequential walk (one allocator, the whole tail of the file) and `Some` when sources are
/// linked in parallel, each into a reserved range (§7). Overrunning a reserved range would
/// scribble on the next source's pages, so it is an error, not a debug assertion.
pub(crate) struct Allocator {
    next: u32,
    lock: u32,
    limit: Option<u32>,
    skipped: bool,
}

impl Allocator {
    pub(crate) fn new(first_free: u32, lock_byte_page: u32) -> Allocator {
        Allocator {
            next: first_free,
            lock: lock_byte_page,
            limit: None,
            skipped: false,
        }
    }

    /// An allocator confined to `[first_free, limit)` — one source's reserved range.
    pub(crate) fn ranged(first_free: u32, lock_byte_page: u32, limit: u32) -> Allocator {
        Allocator {
            limit: Some(limit),
            ..Allocator::new(first_free, lock_byte_page)
        }
    }

    pub(crate) fn alloc(&mut self) -> Result<u32, StitchError> {
        if self.next == self.lock {
            self.next += 1;
            self.skipped = true;
        }
        let p = self.next;
        if self.limit.is_some_and(|end| p >= end) {
            return Err(StitchError::Verification {
                stage: "link",
                detail: format!(
                    "a source needed destination page {p}, past the {} its header reserved \
                     — the file changed under the linker, or its header is inconsistent",
                    self.limit.unwrap_or(p)
                ),
            });
        }
        self.next += 1;
        Ok(p)
    }

    /// The first page this allocator has NOT handed out.
    pub(crate) fn next_free(&self) -> u32 {
        self.next
    }

    /// Pages the destination now spans (the skipped lock-byte page included).
    pub(crate) fn pages(&self) -> u32 {
        self.next - 1
    }

    pub(crate) fn skipped_lock(&self) -> bool {
        self.skipped
    }
}

/// Target bytes per read chunk / write run. 512 KiB was the knee on the 4-vCPU spike box: the
/// syscall count is already down ~100x, and read amplification is still 1.18x (a bigger chunk
/// reads more of the file than the walk consumes; a smaller one gives the syscalls back).
/// See `spikes/stitch-perf/FINDINGS.md`.
const CHUNK_BYTES: usize = 512 * 1024;
/// Open read chunks and open write runs. The walk emits AND consumes two interleaved ascending
/// streams — the pages of the current subtree, and the overflow pages allocated behind them,
/// since a leaf's overflow is allocated after the leaf itself. One slot per side therefore
/// breaks on every leaf that overflows: measured, a single write run coalesced 600 966 pages
/// into 482 268 writes (runs of 1.25 pages), where four slots make it 3 498 (172 pages each).
const SLOTS: usize = 4;

/// One buffered run of consecutive pages, on either side of the copy.
struct Slot {
    /// Page number of `buf[0]`; meaningless while `count == 0`.
    first: u32,
    count: u32,
    buf: Vec<u8>,
    /// Last use, for LRU eviction.
    touched: u64,
}

/// Page I/O for one source→destination walk: chunked reads and run-coalesced writes, `SLOTS`
/// of each so the two interleaved streams do not evict one another.
struct PageIo {
    page_size: usize,
    chunk_pages: u32,
    reads: Vec<Slot>,
    writes: Vec<Slot>,
    tick: u64,
}

impl PageIo {
    fn new(page_size: usize) -> PageIo {
        let chunk_pages = (CHUNK_BYTES / page_size).max(1);
        let slot = || Slot {
            first: 0,
            count: 0,
            buf: vec![0u8; page_size * chunk_pages],
            touched: 0,
        };
        PageIo {
            page_size,
            chunk_pages: chunk_pages as u32,
            reads: (0..SLOTS).map(|_| slot()).collect(),
            writes: (0..SLOTS).map(|_| slot()).collect(),
            tick: 0,
        }
    }

    fn offset(&self, pgno: u32) -> u64 {
        (u64::from(pgno) - 1) * self.page_size as u64
    }

    /// Copy source page `pgno` into `out`, reading a chunk around it when it is not already
    /// held. A short read at end-of-file is fine as long as the wanted page itself arrived.
    fn read(&mut self, src: &File, pgno: u32, out: &mut [u8]) -> std::io::Result<()> {
        self.tick += 1;
        let tick = self.tick;
        let page_size = self.page_size;
        if let Some(i) = self
            .reads
            .iter()
            .position(|s| s.count > 0 && pgno >= s.first && pgno - s.first < s.count)
        {
            let s = &mut self.reads[i];
            s.touched = tick;
            let at = (pgno - s.first) as usize * page_size;
            out.copy_from_slice(&s.buf[at..at + page_size]);
            return Ok(());
        }
        let victim = self
            .reads
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (s.count > 0, s.touched))
            .map(|(i, _)| i)
            .expect("SLOTS > 0");
        let off = self.offset(pgno);
        let s = &mut self.reads[victim];
        let got = read_at_most(src, &mut s.buf, off)?;
        if got < page_size {
            s.count = 0;
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        s.first = pgno;
        s.count = (got / page_size) as u32;
        s.touched = tick;
        out.copy_from_slice(&s.buf[..page_size]);
        Ok(())
    }

    /// Buffer destination page `pgno`, extending whichever open run it continues.
    fn write(&mut self, dst: &File, pgno: u32, page: &[u8]) -> std::io::Result<()> {
        self.tick += 1;
        let tick = self.tick;
        let page_size = self.page_size;
        let cap = self.chunk_pages;
        if let Some(i) = self
            .writes
            .iter()
            .position(|s| s.count > 0 && pgno == s.first + s.count && s.count < cap)
        {
            let s = &mut self.writes[i];
            let at = s.count as usize * page_size;
            s.buf[at..at + page_size].copy_from_slice(page);
            s.count += 1;
            s.touched = tick;
            return Ok(());
        }
        let victim = self
            .writes
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (s.count > 0, s.touched))
            .map(|(i, _)| i)
            .expect("SLOTS > 0");
        self.flush_slot(dst, victim)?;
        let s = &mut self.writes[victim];
        s.buf[..page_size].copy_from_slice(page);
        s.first = pgno;
        s.count = 1;
        s.touched = tick;
        Ok(())
    }

    fn flush_slot(&mut self, dst: &File, i: usize) -> std::io::Result<()> {
        let (first, count) = (self.writes[i].first, self.writes[i].count);
        if count == 0 {
            return Ok(());
        }
        let off = self.offset(first);
        let n = count as usize * self.page_size;
        dst.write_all_at(&self.writes[i].buf[..n], off)?;
        self.writes[i].count = 0;
        Ok(())
    }

    /// Flush every open run, lowest offset first so writeback stays ascending.
    fn flush(&mut self, dst: &File) -> std::io::Result<()> {
        loop {
            let Some(i) = self
                .writes
                .iter()
                .enumerate()
                .filter(|(_, s)| s.count > 0)
                .min_by_key(|(_, s)| s.first)
                .map(|(i, _)| i)
            else {
                return Ok(());
            };
            self.flush_slot(dst, i)?;
        }
    }
}

/// `read_at` until the buffer is full or the file ends; returns the bytes read.
fn read_at_most(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], off + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

pub(crate) struct Linker<'a> {
    source: &'a Source,
    src: File,
    dst: &'a File,
    page_size: usize,
    usable: usize,
    io: PageIo,
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
        let page_size = source.header.page_size as usize;
        Ok(Linker {
            source,
            src,
            dst,
            page_size,
            usable: source.header.usable(),
            io: PageIo::new(page_size),
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

    fn read(&mut self, pgno: u32, buf: &mut [u8]) -> Result<(), StitchError> {
        // The context string is built only on failure: the walk calls this once per page, and
        // formatting the source path every time was a per-page allocation.
        match self.io.read(&self.src, pgno, buf) {
            Ok(()) => Ok(()),
            Err(e) => Err(io_err(format!(
                "read page {pgno} of {}",
                self.source.path.display()
            ))(e)),
        }
    }

    fn write(&mut self, pgno: u32, buf: &[u8]) -> Result<(), StitchError> {
        match self.io.write(self.dst, pgno, buf) {
            Ok(()) => Ok(()),
            Err(e) => Err(io_err(format!("write destination page {pgno}"))(e)),
        }
    }

    /// Push every buffered run to the destination. MUST be called once the source's last tree
    /// is linked — the walk leaves up to `SLOTS` runs open, so nothing is guaranteed on disk
    /// until this returns.
    pub(crate) fn flush(&mut self) -> Result<(), StitchError> {
        match self.io.flush(self.dst) {
            Ok(()) => Ok(()),
            Err(e) => Err(io_err("flush the destination write buffers")(e)),
        }
    }

    /// Rewrite the overflow pointer at `ptr_off` in `page` (source page `sp`) and copy the chain
    /// behind it, page by page, allocating each destination page as its predecessor is met.
    fn relocate_overflow(
        &mut self,
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
        let mut dst_pgno = w.alloc.alloc()?;
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
                w.alloc.alloc()?
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
        &mut self,
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
                    let nd = w.alloc.alloc()?;
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
                let nd = w.alloc.alloc()?;
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

    /// Count a tree's pages without allocating or writing one — the b-tree pages plus their
    /// overflow chains. Used to reserve a source's destination range EXACTLY (§7 + the
    /// stitch-perf findings): a source's pages are page 1, its freelist, and its b-trees, and
    /// the trees the linker does NOT copy (`sqlite_sequence`, `sqlite_stat*`) have to come off
    /// the reservation or the range is over-sized and leaves a hole. Those trees are tiny by
    /// nature, so this costs almost nothing.
    pub(crate) fn count_tree(&mut self, root: u32) -> Result<u64, StitchError> {
        let mut buf = vec![0u8; self.page_size];
        let mut obuf = vec![0u8; self.page_size];
        let mut stack = vec![root];
        let (mut pages, mut visited) = (0u64, 0u64);
        self.check_pgno(root, root)?;
        while let Some(sp) = stack.pop() {
            visited += 1;
            if visited > u64::from(self.source.page_count) {
                return Err(self.corrupt(sp, "b-tree visits more pages than the file holds (cycle)"));
            }
            self.read(sp, &mut buf)?;
            let kind = Kind::from_flag(buf[0])
                .ok_or_else(|| self.corrupt(sp, format!("unexpected b-tree flag {:#04x}", buf[0])))?;
            let hdr = kind.header_len();
            let ncell = be16(&buf, 3);
            let array_end = hdr + 2 * ncell;
            if array_end > self.usable {
                return Err(self.corrupt(sp, format!("{ncell} cells overrun the page")));
            }
            pages += 1;
            for i in 0..ncell {
                let off = be16(&buf, hdr + 2 * i);
                if off < array_end || off >= self.usable {
                    return Err(self.corrupt(sp, format!("cell {i} at offset {off} lies outside")));
                }
                let mut cursor = off;
                if kind.is_interior() {
                    if off + 4 > self.usable {
                        return Err(self.corrupt(sp, format!("cell {i} child pointer overruns")));
                    }
                    let child = be32(&buf, off);
                    self.check_pgno(sp, child)?;
                    stack.push(child);
                    cursor += 4;
                    if kind == Kind::InteriorTable {
                        continue;
                    }
                }
                let (payload, n) = varint(&buf[cursor..self.usable])
                    .ok_or_else(|| self.corrupt(sp, format!("cell {i}: payload varint runs off")))?;
                cursor += n;
                if kind == Kind::LeafTable {
                    let (_rowid, n) = varint(&buf[cursor..self.usable])
                        .ok_or_else(|| self.corrupt(sp, format!("cell {i}: rowid varint runs off")))?;
                    cursor += n;
                }
                let payload = payload as usize;
                let local = local_payload(kind, payload, self.usable);
                if payload > local {
                    if cursor + local + 4 > self.usable {
                        return Err(self.corrupt(sp, "overflow pointer lies outside the usable area"));
                    }
                    let mut next = be32(&buf, cursor + local);
                    while next != 0 {
                        visited += 1;
                        if visited > u64::from(self.source.page_count) {
                            return Err(self.corrupt(next, "overflow chain never ends (cycle)"));
                        }
                        self.check_pgno(sp, next)?;
                        self.read(next, &mut obuf)?;
                        pages += 1;
                        next = be32(&obuf, 0);
                    }
                }
            }
            if kind.is_interior() {
                let right = be32(&buf, 8);
                self.check_pgno(sp, right)?;
                stack.push(right);
            }
        }
        Ok(pages)
    }
}

#[cfg(test)]
mod tests {
    use super::Allocator;

    #[test]
    fn allocator_skips_the_lock_byte_page_once() {
        let mut a = Allocator::new(10, 12);
        assert_eq!(a.alloc().unwrap(), 10);
        assert_eq!(a.alloc().unwrap(), 11);
        assert_eq!(a.alloc().unwrap(), 13);
        assert_eq!(a.alloc().unwrap(), 14);
        assert!(a.skipped_lock());
        assert_eq!(a.pages(), 14);
        let mut b = Allocator::new(2, 1_000_000);
        b.alloc().unwrap();
        assert!(!b.skipped_lock());
        assert_eq!(b.pages(), 2);
    }

    #[test]
    fn a_ranged_allocator_refuses_to_leave_its_range() {
        let mut a = Allocator::ranged(10, 1_000_000, 12);
        assert_eq!(a.alloc().unwrap(), 10);
        assert_eq!(a.alloc().unwrap(), 11);
        assert!(a.alloc().is_err());
        assert_eq!(a.next_free(), 12);
    }
}
