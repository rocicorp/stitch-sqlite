//! # stitch-sqlite — a SQLite B-tree linker
//!
//! Given N **closed** SQLite files that share their file-format parameters (page size, reserved
//! bytes, encoding, schema format) and were built with `auto_vacuum = NONE`, produce ONE database
//! holding every table and index by **relocating pages**: interior child pointers, right-most
//! pointers, and overflow-chain links are rewritten through an allocate-at-discovery page map;
//! every other byte is copied. No row is decoded, no index is rebuilt. Design:
//! [`docs/DESIGN.md`](https://github.com/rocicorp/stitch-sqlite/blob/main/docs/DESIGN.md)
//! (design 415 in the rindle monorepo, which this repository is published from).
//!
//! Why: `ATTACH` + `INSERT … SELECT` can move a table's rows but SQLite has no way to insert
//! into an index b-tree, so every index is rebuilt single-threaded. Linking is the only way to
//! *keep* indexes that were built in parallel, one table per file, on N connections.
//!
//! What it is not: the tool is generic. It links whatever `sqlite_schema` lists and knows
//! nothing above SQLite — no consumer bookkeeping is required in the sources or written to the
//! output. Its post-link SQL pass touches SQLite's **own** state only (`sqlite_sequence`,
//! optionally `sqlite_stat*`, the requested `journal_mode`). Every consumer adds its metadata
//! afterwards through an ordinary connection to an ordinary SQLite file.
//!
//! ```no_run
//! use stitch_sqlite::{open_source, seal_source, stitch, JournalMode, Options, SourceOptions};
//! use std::path::Path;
//!
//! # fn main() -> Result<(), stitch_sqlite::StitchError> {
//! // The loader half (yours): one file per table, built through the recipe so the §2 contract
//! // holds by construction. In practice these run on N threads / N connections.
//! let conn = open_source(Path::new("t1.db"), &SourceOptions::default())?;
//! conn.execute_batch("CREATE TABLE t1(id INTEGER PRIMARY KEY, name TEXT); CREATE INDEX t1_name ON t1(name);")
//!     .map_err(|e| stitch_sqlite::StitchError::Sqlite { context: "load".into(), source: e })?;
//! seal_source(conn, JournalMode::Delete)?;
//!
//! // The linker half: one file out.
//! let report = stitch(&["t1.db", "t2.db"], Path::new("out.db"), &Options::default())?;
//! println!("{} pages, {} b-trees", report.dest_pages, report.link.trees);
//! # Ok(()) }
//! ```
//!
//! **Which SQLite does it need?** Any. The page copy never goes through SQLite, and the skeleton,
//! the schema read, and the post-link pass use nothing beyond stock SQLite. The one exception is
//! **wal2**, a bedrock-branch feature: reading a source sealed in wal2 (header bytes 18/19 = 3)
//! or emitting a wal2 output needs a wal2-capable build — inside the rindle workspace the
//! `[patch.crates-io]` redirect supplies one; a stock `rusqlite` `bundled` build links, passes
//! every other test, and reports the wal2 cases as `StitchError::Unsupported` /
//! `StitchError::Verification` with the reason.
//!
//! For callers that are not Rust (the first consumer, Zero's initial sync, spawns the
//! `stitch-sqlite` binary), the recipe is a pragma list: open a fresh file,
//! `PRAGMA page_size = <agreed>; PRAGMA auto_vacuum = NONE; PRAGMA journal_mode = DELETE;`, load,
//! create the indexes, close, and make sure no `-wal`/`-wal2`/`-shm`/`-journal` sidecar remains.

#![forbid(unsafe_code)]

pub mod cli;
mod error;
mod header;
mod link;
mod page;
mod skeleton;
mod source;
mod verify;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params_from_iter, Connection};
use serde::Serialize;

pub use error::StitchError;
pub use header::Header;
pub use link::LinkStats;
pub use page::{local_payload, varint, Kind};
pub use source::{SchemaRow, Source, StatRow};

use error::{io_err, sql_err};

/// A hook that registers SQL functions on the destination skeleton before its `CREATE INDEX`
/// statements run — an expression index over a custom function will not recreate without it.
/// The crate knows nothing about any consumer's SQL surface; rindle callers pass
/// `rindle_regex`'s registration.
pub type RegisterFn = dyn Fn(&Connection) -> rusqlite::Result<()>;

/// How much of the OUTPUT is checked through SQLite after the link (design 415 §5.1). The
/// walk's structural checks and the smoke probe are always on and are not a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verify {
    /// No SQLite-level pass over the output. **Default.**
    #[default]
    None,
    /// `PRAGMA quick_check`: reads every page, skips index↔table consistency.
    Quick,
    /// `PRAGMA integrity_check`: the whole thing. Costs more than the stitch.
    Full,
}

/// The output's journal mode. `Delete` (the default) leaves the file exactly as SQLite wrote it,
/// openable by any build; `Wal2` needs a wal2-capable SQLite such as the vendored one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JournalMode {
    #[default]
    Delete,
    Wal,
    Wal2,
}

impl JournalMode {
    pub fn pragma_value(self) -> &'static str {
        match self {
            JournalMode::Delete => "delete",
            JournalMode::Wal => "wal",
            JournalMode::Wal2 => "wal2",
        }
    }
}

/// What happens to `sqlite_stat1` / `sqlite_stat4`, whose PAGES are never linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stats {
    /// Nothing: the output has no statistics until someone runs `ANALYZE`. **Default.**
    #[default]
    Skip,
    /// Copy the sources' rows by SQL — exact and cheap (the sources ran `ANALYZE` per table).
    CopyRows,
    /// Run `ANALYZE` on the output: O(output), recomputed.
    Analyze,
}

/// Options for [`stitch`].
pub struct Options {
    pub verify: Verify,
    pub journal_mode: JournalMode,
    pub stats: Stats,
    /// Replace an existing destination (its sidecars included). Off by default.
    pub overwrite: bool,
    pub register: Option<Box<RegisterFn>>,
    /// How many sources to link at once (design 415 §7). `1` — the default — is the sequential
    /// walk with one allocator over the tail of the file. Above that, each source is given a
    /// **reserved, disjoint destination page range** computed from its header, and the walks run
    /// on that many threads writing to disjoint regions of one file. Measured on 6 sources ×
    /// 2 M rows: the link phase goes 1.54 s → 0.61 s at 4 (`spikes/stitch-perf/FINDINGS.md`).
    /// Clamped to the number of sources; more threads than cores does not help.
    pub threads: usize,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            verify: Verify::default(),
            journal_mode: JournalMode::default(),
            stats: Stats::default(),
            overwrite: false,
            register: None,
            threads: 1,
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("verify", &self.verify)
            .field("journal_mode", &self.journal_mode)
            .field("stats", &self.stats)
            .field("overwrite", &self.overwrite)
            .field("threads", &self.threads)
            .field("register", &self.register.as_ref().map(|_| "fn"))
            .finish()
    }
}

/// Per-phase wall-clock time.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Durations {
    pub skeleton_ms: u128,
    /// The walk: every source page read once, every destination page written once.
    pub link_ms: u128,
    /// The one `fsync` of the linked file, separately: on a virtual disk it can cost more than a
    /// small link itself, and folding it into `link_ms` would misstate the copy rate.
    pub fsync_ms: u128,
    pub finish_ms: u128,
    pub verify_ms: u128,
}

/// What a stitch did.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub destination: PathBuf,
    pub sources: usize,
    pub page_size: u32,
    pub encoding: &'static str,
    /// Pages the output spans (the skipped lock-byte page included, when crossed).
    pub dest_pages: u32,
    pub bytes: u64,
    /// Freelist pages the sources carried; unreachable from any root, so never copied.
    pub source_freelist_pages: u64,
    /// Sources linked at once. `1` is the sequential walk; above that each source got a
    /// reserved destination range (`Options::threads`).
    pub link_threads: usize,
    /// Pages on the OUTPUT's freelist. Normally `0`: reservation is exact, so a parallel link
    /// leaves no hole. Non-zero means a source carried pages reachable from nothing, which the
    /// linker parks on the freelist rather than leaving unaccounted for.
    pub freelist_pages: u32,
    pub link: LinkStats,
    pub durations: Durations,
    pub verify: Verify,
    pub journal_mode: JournalMode,
    pub stats: Stats,
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

fn remove_if_present(path: &Path) -> Result<(), StitchError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(format!("remove {}", path.display()))(e)),
    }
}

fn remove_with_sidecars(path: &Path) -> Result<(), StitchError> {
    remove_if_present(path)?;
    for sidecar in source::sidecar_paths(path) {
        remove_if_present(&sidecar)?;
    }
    Ok(())
}

/// Link `sources` into `dest`.
///
/// Every source is inspected first (header, sidecars, `sqlite_schema`) and any contract
/// violation is refused before a byte is written. The output is built under a temporary name
/// beside `dest` and renamed into place at the very end, so a crash leaves nothing half-linked
/// under the real name.
pub fn stitch(
    sources: &[impl AsRef<Path>],
    dest: &Path,
    opts: &Options,
) -> Result<Report, StitchError> {
    if sources.is_empty() {
        return Err(StitchError::NoSources);
    }
    let sources: Vec<Source> = sources
        .iter()
        .map(|p| source::inspect(p.as_ref()))
        .collect::<Result<_, _>>()?;
    for other in &sources[1..] {
        source::check_compatible(&sources[0], other)?;
    }
    skeleton::check_duplicates(&sources)?;
    let file_name = dest
        .file_name()
        .ok_or_else(|| StitchError::Io {
            context: format!("destination {} has no file name", dest.display()),
            source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
        })?
        .to_string_lossy()
        .into_owned();
    let tmp = dest.with_file_name(format!("{file_name}.stitch-tmp"));
    check_no_source_is_replaced(&sources, dest, &tmp)?;
    if dest.exists() && !opts.overwrite {
        return Err(StitchError::DestinationExists(dest.to_path_buf()));
    }
    remove_with_sidecars(&tmp)?;
    let result = link_into(&sources, dest, &tmp, opts);
    if result.is_err() {
        // Nothing half-linked survives a refusal; the real destination was never touched.
        let _ = remove_with_sidecars(&tmp);
    }
    result
}

/// The destination, the temporary file, and the sidecars of both are the paths a stitch
/// deletes; a source must be none of them. Compared by inode, so links and `..` cannot hide one.
fn check_no_source_is_replaced(
    sources: &[Source],
    dest: &Path,
    tmp: &Path,
) -> Result<(), StitchError> {
    use std::os::unix::fs::MetadataExt;
    let ident = |p: &Path| fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let targets: Vec<(PathBuf, (u64, u64))> = [dest, tmp]
        .into_iter()
        .flat_map(|p| std::iter::once(p.to_path_buf()).chain(source::sidecar_paths(p)))
        .filter_map(|p| ident(&p).map(|id| (p, id)))
        .collect();
    for s in sources {
        let Some(id) = ident(&s.path) else { continue };
        if let Some((target, _)) = targets.iter().find(|(_, tid)| *tid == id) {
            return Err(StitchError::SourceIsDestination {
                path: s.path.clone(),
                target: target.clone(),
            });
        }
    }
    Ok(())
}

/// Reopen the linked file through SQLite with the caller's functions registered — the finish
/// pass and the opt-in verification both need them (an expression index over a custom function
/// is evaluated by `integrity_check`).
fn open_dest(tmp: &Path, opts: &Options, context: &'static str) -> Result<Connection, StitchError> {
    let conn = Connection::open(tmp).map_err(sql_err(context))?;
    if let Some(register) = &opts.register {
        register(&conn).map_err(sql_err("register SQL functions on the destination"))?;
    }
    Ok(conn)
}

/// One source's reserved destination range, `[base, end)`.
struct Reservation {
    base: u32,
    end: u32,
}

/// Reserve a contiguous destination range per source (design 415 §7), sized **exactly**.
///
/// Every page of a source is page 1, a freelist page, or a b-tree page — there are no
/// pointer-map pages, because the contract refuses `auto_vacuum`. Of the b-tree pages, the
/// linker does not allocate the ROOT of each linked tree (the skeleton owns those), and does not
/// copy the internal trees at all (`sqlite_sequence`, `sqlite_stat*`). So
///
/// ```text
/// reserve = page_count − 1 − freelist_count − internal_tree_pages − linked_tree_count
/// ```
///
/// §7 gives the first three terms; the last two are what the spike found missing. Reserving an
/// upper bound instead leaves a hole in the output, and a hole is not cosmetic — SQLite's
/// `integrity_check` reports "Page N is never used". `internal_tree_pages` costs a count-only
/// walk of trees that are one or two pages by nature.
fn reserve_ranges(
    sources: &[Source],
    dst: &File,
    first_free: u32,
    lock: u32,
) -> Result<(Vec<Reservation>, u32), StitchError> {
    let mut out = Vec::with_capacity(sources.len());
    let mut next = first_free;
    for s in sources {
        let trees = s.schema.iter().filter(|r| r.is_linked_tree()).count() as u64;
        let internal_roots: Vec<u32> = s
            .schema
            .iter()
            .filter(|r| r.rootpage > 1 && source::is_internal_name(&r.name))
            .map(|r| r.rootpage)
            .collect();
        let mut internal = 0u64;
        if !internal_roots.is_empty() {
            // Only pay for a Linker (and its buffers) when there is something to count.
            let mut linker = link::Linker::open(s, dst)?;
            for root in internal_roots {
                internal += linker.count_tree(root)?;
            }
        }
        let reserve = u64::from(s.page_count)
            .checked_sub(1)
            .and_then(|n| n.checked_sub(u64::from(s.header.freelist_count)))
            .and_then(|n| n.checked_sub(internal))
            .and_then(|n| n.checked_sub(trees))
            .ok_or_else(|| StitchError::Corrupt {
                path: s.path.clone(),
                page: 1,
                why: format!(
                    "{} pages cannot hold page 1 + {} freelist + {internal} internal + {trees} roots",
                    s.page_count, s.header.freelist_count
                ),
            })?;
        let base = next;
        let mut end = u32::try_from(reserve)
            .ok()
            .and_then(|n| base.checked_add(n))
            .ok_or_else(|| StitchError::Verification {
                stage: "link",
                detail: "the linked output would exceed 2^32 pages".into(),
            })?;
        // The allocator skips the lock-byte page, so a range spanning it needs one more slot.
        if base <= lock && lock < end {
            end += 1;
        }
        out.push(Reservation { base, end });
        next = end;
    }
    Ok((out, next))
}

/// Link every source in parallel into its reserved range. Threads write to disjoint regions of
/// one destination handle — `write_all_at` takes `&self` and needs no coordination.
///
/// Returns `(pages the output spans, pages inside the ranges the walks did not use)`. The second
/// is normally empty; see [`link::write_freelist`].
#[allow(clippy::too_many_arguments)]
fn link_parallel(
    sources: &[Source],
    skeleton: &skeleton::Skeleton,
    dst: &File,
    first_free: u32,
    lock: u32,
    threads: usize,
    stats: &mut LinkStats,
) -> Result<(u32, Vec<u32>), StitchError> {
    let (ranges, end) = reserve_ranges(sources, dst, first_free, lock)?;
    let mut used: Vec<u32> = Vec::with_capacity(sources.len());
    let indices: Vec<usize> = (0..sources.len()).collect();
    for wave in indices.chunks(threads) {
        let results: Vec<Result<(LinkStats, u32), StitchError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = wave
                .iter()
                .map(|&i| {
                    let (source, range) = (&sources[i], &ranges[i]);
                    scope.spawn(move || -> Result<(LinkStats, u32), StitchError> {
                        let mut linker = link::Linker::open(source, dst)?;
                        let mut alloc = link::Allocator::ranged(range.base, lock, range.end);
                        let mut st = LinkStats::default();
                        for row in source.schema.iter().filter(|r| r.is_linked_tree()) {
                            let &root = skeleton.roots.get(&row.name).ok_or_else(|| {
                                StitchError::NoRoot {
                                    name: row.name.clone(),
                                }
                            })?;
                            linker.link_tree(row.rootpage, root, &mut alloc, &mut st)?;
                        }
                        linker.flush()?;
                        st.lock_page_skipped = alloc.skipped_lock();
                        Ok((st, alloc.next_free()))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        Err(StitchError::Verification {
                            stage: "link",
                            detail: "a linker thread panicked".into(),
                        })
                    })
                })
                .collect()
        });
        for r in results {
            let (st, next_free) = r?;
            stats.merge(&st);
            used.push(next_free);
        }
    }
    let mut spare = Vec::new();
    for (range, &next_free) in ranges.iter().zip(&used) {
        spare.extend((next_free..range.end).filter(|&p| p != lock));
    }
    Ok((end - 1, spare))
}

fn link_into(
    sources: &[Source],
    dest: &Path,
    tmp: &Path,
    opts: &Options,
) -> Result<Report, StitchError> {
    let template = sources[0].header.clone();
    let page_size = template.page_size;

    // 1. Skeleton.
    let t = Instant::now();
    let skeleton = skeleton::create(tmp, sources, opts.register.as_deref())?;
    let skeleton_ms = ms(t.elapsed());

    // 2. Link.
    let t = Instant::now();
    let dst = OpenOptions::new()
        .read(true)
        .write(true)
        .open(tmp)
        .map_err(io_err("open destination for linking"))?;
    let mut page1 = vec![0u8; page_size as usize];
    dst.read_exact_at(&mut page1, 0)
        .map_err(io_err("read destination page 1"))?;
    let dest_header = Header::parse(tmp, &page1)?;
    for (field, got, want) in [
        (
            "page_size",
            u64::from(dest_header.page_size),
            u64::from(template.page_size),
        ),
        (
            "reserved bytes",
            u64::from(dest_header.reserved),
            u64::from(template.reserved),
        ),
        (
            "encoding",
            u64::from(dest_header.encoding),
            u64::from(template.encoding),
        ),
        // Format 4 is where DESC on an index means descending and serial types 8/9 exist; a
        // legacy-format source's pages would be misread under the skeleton's format.
        (
            "schema format",
            u64::from(dest_header.schema_format),
            u64::from(template.schema_format),
        ),
    ] {
        if got != want {
            return Err(StitchError::Incompatible {
                path: tmp.to_path_buf(),
                field,
                value: got.to_string(),
                expected: want.to_string(),
            });
        }
    }
    let dest_len = dst.metadata().map_err(io_err("stat destination"))?.len();
    let first_free = (dest_len / u64::from(page_size)) as u32 + 1;
    let lock = template.lock_byte_page();
    let threads = opts.threads.max(1).min(sources.len());
    let mut stats = LinkStats::default();
    let (linked_pages, spare) = if threads > 1 {
        link_parallel(sources, &skeleton, &dst, first_free, lock, threads, &mut stats)?
    } else {
        let mut alloc = link::Allocator::new(first_free, lock);
        for s in sources {
            let mut linker = link::Linker::open(s, &dst)?;
            for row in s.schema.iter().filter(|r| r.is_linked_tree()) {
                let &root = skeleton
                    .roots
                    .get(&row.name)
                    .ok_or_else(|| StitchError::NoRoot {
                        name: row.name.clone(),
                    })?;
                linker.link_tree(row.rootpage, root, &mut alloc, &mut stats)?;
            }
            // The walk leaves buffered runs open; nothing is on disk until this returns.
            linker.flush()?;
        }
        stats.lock_page_skipped = alloc.skipped_lock();
        (alloc.pages(), Vec::new())
    };
    let (freelist_trunk, freelist_pages) =
        link::write_freelist(&dst, page_size as usize, template.usable(), &spare)?;
    dst.set_len(u64::from(linked_pages) * u64::from(page_size))
        .map_err(io_err("set destination length"))?;
    header::patch_after_link(&mut page1, linked_pages, freelist_trunk, freelist_pages);
    dst.write_all_at(&page1, 0)
        .map_err(io_err("write destination header"))?;
    let link_ms = ms(t.elapsed());
    let t = Instant::now();
    dst.sync_all().map_err(io_err("fsync destination"))?;
    drop(dst);
    let fsync_ms = ms(t.elapsed());

    // 3. Finish: SQLite's own state, through SQL, then the smoke probe.
    let t = Instant::now();
    {
        let conn = open_dest(tmp, opts, "reopen destination")?;
        for s in sources {
            for (name, seq) in &s.sequences {
                conn.execute(
                    "INSERT INTO sqlite_sequence(name, seq) VALUES (?1, ?2)",
                    rusqlite::params![name, seq],
                )
                .map_err(sql_err("merge sqlite_sequence"))?;
            }
        }
        match opts.stats {
            Stats::Skip => {}
            Stats::Analyze => conn.execute_batch("ANALYZE").map_err(sql_err("ANALYZE"))?,
            Stats::CopyRows => copy_stats(&conn, sources)?,
        }
        if opts.journal_mode != JournalMode::Delete {
            let mode: String = conn
                .query_row(
                    &format!("PRAGMA journal_mode = {}", opts.journal_mode.pragma_value()),
                    [],
                    |r| r.get(0),
                )
                .map_err(sql_err("set journal_mode"))?;
            if mode != opts.journal_mode.pragma_value() {
                return Err(StitchError::Verification {
                    stage: "journal_mode",
                    detail: format!(
                        "asked for {}, SQLite answered {mode:?} (a wal2 output needs a wal2-capable build)",
                        opts.journal_mode.pragma_value()
                    ),
                });
            }
        }
        verify::smoke(
            tmp,
            &conn,
            sources,
            &skeleton.roots,
            page_size,
            template.usable(),
            opts.journal_mode.pragma_value(),
        )?;
    }
    let finish_ms = ms(t.elapsed());

    // 4. Opt-in verification over the whole output.
    let t = Instant::now();
    if opts.verify != Verify::None {
        let conn = open_dest(tmp, opts, "reopen destination to verify")?;
        match opts.verify {
            Verify::Quick => verify::quick(&conn)?,
            Verify::Full => verify::full(&conn)?,
            Verify::None => {}
        }
    }
    let verify_ms = ms(t.elapsed());

    // 5. Publish. A closed WAL-mode connection removes its sidecars; sweep any that linger.
    for sidecar in source::sidecar_paths(tmp) {
        remove_if_present(&sidecar)?;
    }
    // The size of what is published: the finish pass may have grown the file past the link
    // (`sqlite_stat*` under `Stats::Analyze` / `CopyRows`, a large `sqlite_sequence`).
    let bytes = fs::metadata(tmp)
        .map_err(io_err("stat the linked file"))?
        .len();
    let dest_pages = (bytes / u64::from(page_size)) as u32;
    if opts.overwrite {
        remove_with_sidecars(dest)?;
    }
    fs::rename(tmp, dest).map_err(io_err(format!("rename into {}", dest.display())))?;

    Ok(Report {
        destination: dest.to_path_buf(),
        sources: sources.len(),
        page_size,
        encoding: template.encoding_name(),
        dest_pages,
        bytes,
        link_threads: threads,
        freelist_pages,
        source_freelist_pages: sources
            .iter()
            .map(|s| u64::from(s.header.freelist_count))
            .sum(),
        link: stats,
        durations: Durations {
            skeleton_ms,
            link_ms,
            fsync_ms,
            finish_ms,
            verify_ms,
        },
        verify: opts.verify,
        journal_mode: opts.journal_mode,
        stats: opts.stats,
    })
}

/// `Stats::CopyRows`: make the stat tables exist (a bounded `ANALYZE` — `analysis_limit = 1`
/// looks at one row per index — is the only way to create `sqlite_stat*`), empty them, and
/// insert the sources' rows verbatim.
fn copy_stats(conn: &Connection, sources: &[Source]) -> Result<(), StitchError> {
    if sources.iter().all(|s| s.stats.is_empty()) {
        return Ok(());
    }
    conn.execute_batch("PRAGMA analysis_limit = 1; ANALYZE; PRAGMA analysis_limit = 0;")
        .map_err(sql_err("create sqlite_stat tables"))?;
    for table in ["sqlite_stat1", "sqlite_stat4"] {
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .map_err(sql_err("check stat table"))?;
        if exists == 0 {
            continue;
        }
        conn.execute_batch(&format!("DELETE FROM {table}"))
            .map_err(sql_err(format!("clear {table}")))?;
        for row in sources
            .iter()
            .flat_map(|s| s.stats.iter())
            .filter(|r| r.table == table)
        {
            let placeholders = (1..=row.values.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute(
                &format!("INSERT INTO {table} VALUES ({placeholders})"),
                params_from_iter(row.values.iter()),
            )
            .map_err(sql_err(format!("copy {table} row")))?;
        }
    }
    Ok(())
}

// ───────────────────────────── the source-file recipe ─────────────────────────────

/// Text encoding for a fresh source. Every source of one stitch must agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16le,
    Utf16be,
}

/// `PRAGMA synchronous` while loading a source. The default, `Off`, is the bulk-load posture:
/// a source is rebuilt from its input if the machine dies, and `seal_source` fsyncs at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Synchronous {
    #[default]
    Off,
    Normal,
    Full,
}

/// Knobs for [`open_source`]. Everything the §2 contract requires is set unconditionally;
/// these are the parts the loader may choose.
#[derive(Debug, Clone, Default)]
pub struct SourceOptions {
    /// `PRAGMA page_size`; `None` keeps the library default (4096 in the vendored build).
    pub page_size: Option<u32>,
    /// `PRAGMA encoding`; `None` keeps the library default (UTF-8).
    pub encoding: Option<Encoding>,
    pub synchronous: Synchronous,
    /// `PRAGMA cache_size = -<KiB>`; `None` keeps the library default. Sessions × cache must fit
    /// RAM when N sources build in parallel.
    pub cache_size_kib: Option<u32>,
}

/// The §2 contract as code: open a **fresh** file with the pragmas the linker requires — page
/// size, `auto_vacuum = NONE`, `journal_mode = DELETE`, `foreign_keys = OFF` (a one-table file
/// cannot see the tables it references), the caller's `synchronous`/`cache_size` — so a loader
/// cannot drift from the contract by forgetting one. Load and create indexes on the returned
/// connection, then hand it to [`seal_source`].
pub fn open_source(path: &Path, opts: &SourceOptions) -> Result<Connection, StitchError> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > 0 {
            return Err(StitchError::SourceNotFresh(path.to_path_buf()));
        }
    }
    for sidecar in source::sidecar_paths(path) {
        if sidecar.exists() {
            return Err(StitchError::Sidecar {
                path: path.to_path_buf(),
                sidecar,
            });
        }
    }
    let conn = Connection::open(path).map_err(sql_err(format!("create {}", path.display())))?;
    let mut pragmas = String::new();
    if let Some(ps) = opts.page_size {
        pragmas.push_str(&format!("PRAGMA page_size = {ps}; "));
    }
    if let Some(enc) = opts.encoding {
        let name = match enc {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16le => "UTF-16le",
            Encoding::Utf16be => "UTF-16be",
        };
        pragmas.push_str(&format!("PRAGMA encoding = '{name}'; "));
    }
    // Enforcement is per connection and the vendored build turns it ON by default
    // (`SQLITE_DEFAULT_FOREIGN_KEYS=1`): a one-table file cannot satisfy a reference to a table
    // that lives in another file, so the recipe turns it off for the load. The stitched output
    // carries the constraints; the consumer enforces them when it opens the file.
    pragmas.push_str(
        "PRAGMA foreign_keys = OFF; PRAGMA auto_vacuum = NONE; PRAGMA journal_mode = DELETE; ",
    );
    pragmas.push_str(match opts.synchronous {
        Synchronous::Off => "PRAGMA synchronous = OFF; ",
        Synchronous::Normal => "PRAGMA synchronous = NORMAL; ",
        Synchronous::Full => "PRAGMA synchronous = FULL; ",
    });
    if let Some(kib) = opts.cache_size_kib {
        pragmas.push_str(&format!("PRAGMA cache_size = -{kib}; "));
    }
    conn.execute_batch(&pragmas)
        .map_err(sql_err("source pragmas"))?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(sql_err("PRAGMA journal_mode"))?;
    let autovac: i64 = conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .map_err(sql_err("PRAGMA auto_vacuum"))?;
    if mode != "delete" || autovac != 0 {
        return Err(StitchError::Verification {
            stage: "open_source",
            detail: format!("journal_mode {mode:?}, auto_vacuum {autovac} — not the contract"),
        });
    }
    Ok(conn)
}

/// Close a loaded source: switch it to `journal_mode` (rollback-journal `Delete` needs no switch),
/// close the connection, and check that no sidecar survived. Returns the path. A sealed file is
/// exactly what [`stitch`] accepts.
pub fn seal_source(conn: Connection, journal_mode: JournalMode) -> Result<PathBuf, StitchError> {
    let path = conn
        .path()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| StitchError::Verification {
            stage: "seal_source",
            detail: "the connection has no file path (in-memory or temporary database)".into(),
        })?;
    if journal_mode != JournalMode::Delete {
        let mode: String = conn
            .query_row(
                &format!("PRAGMA journal_mode = {}", journal_mode.pragma_value()),
                [],
                |r| r.get(0),
            )
            .map_err(sql_err("seal: set journal_mode"))?;
        if mode != journal_mode.pragma_value() {
            return Err(StitchError::Verification {
                stage: "seal_source",
                detail: format!(
                    "asked for {}, SQLite answered {mode:?}",
                    journal_mode.pragma_value()
                ),
            });
        }
    }
    conn.close().map_err(|(_, e)| sql_err("seal: close")(e))?;
    File::open(&path)
        .and_then(|f| f.sync_all())
        .map_err(io_err(format!("seal: fsync {}", path.display())))?;
    for sidecar in source::sidecar_paths(&path) {
        if sidecar.exists() {
            return Err(StitchError::Sidecar {
                path: path.clone(),
                sidecar,
            });
        }
    }
    Ok(path)
}
