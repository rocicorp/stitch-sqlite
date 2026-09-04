# 415 — SQLite B-tree linker: stitching one-table-per-file databases into one

**Status: implemented 2026-09-03** — `rust/stitch-sqlite` (library + the standalone
binary + the `rindle stitch` alias) with the §5 differential harness green: the spike's shapes,
page sizes 512 / 1 024 / 65 536 around every bend of the local-payload rule, UTF-16 sources,
empty and single-leaf tables, views/triggers/foreign keys across sources, all three journal modes,
`sqlite_stat*` copy and recompute, every §2 refusal, and three corrupt-source shapes; the > 1 GiB
lock-byte lane is the local-only `stitch-lockbyte` lane. The spike that decided it — [`spikes/sqlite-stitch/`](../spikes/sqlite-stitch/), a ~300-line linker plus a
harness, linked against the vendored hctree-bedrock wal2 amalgamation — stitched six one-table
files (12 M rows, 19 b-trees, 1 055 MiB, overflow chains, a `WITHOUT ROWID` table, an
`AUTOINCREMENT` table, a partial index, an `sqlite_autoindex_*`, 920 source freelist pages, and an
output that crosses the 1 GiB lock-byte page) into one wal2 file in **9.9 s**, `PRAGMA
integrity_check` clean, every row and every index accounted for; the ATTACH + `INSERT … SELECT` +
`CREATE INDEX` alternative took **44.2 s** on the same files, 29.2 s of it rebuilding indexes the
linker simply keeps, and building the same rows and indexes the way the genesis does today — one
connection, one file, sequentially — took **56.0 s** against 28.5 s for the parallel build plus
the stitch. [`FINDINGS.md`](../spikes/sqlite-stitch/FINDINGS.md) has the run.

**Question this answers:** rindle's Postgres bootstrap decodes a `COPY` and inserts rows through one
SQLite connection, then builds each index single-threaded. If the loader instead writes **one SQLite
file per table** — in parallel, each with its own indexes — can the files be **linked** into the one
database rindle serves, at copy speed, without decoding a row? Yes; this doc says why, how, what the
loader must promise, and where the tool sits. **Deployment order (decided 2026-09-03): Zero
first, rindle later.** The tool is a generic SQLite linker: it links whatever `sqlite_schema` lists
and knows nothing above SQLite — no rindle bookkeeping, no Zero bookkeeping. Every consumer adds
its own metadata **after** the stitch, through a normal connection (§3.3, §6.0).

Predecessors: [`406-DIRECT-POSTGRES-SOURCE-PARITY-PLAN.md`](406-DIRECT-POSTGRES-SOURCE-PARITY-PLAN.md)
§7.2–§7.3 (genesis as journal frames; the open "producer cold start" optimization),
[`404-POSTGRES-AUTHORITY-SPOOL-FREE-INITIAL-SNAPSHOT-DESIGN.md`](404-POSTGRES-AUTHORITY-SPOOL-FREE-INITIAL-SNAPSHOT-DESIGN.md)
(the one-connection DELETE-mode direct construction, 10 GiB in ~129 s, and the DELETE→wal2 handoff
this design reuses verbatim), [`../docs/SQLITE_WAL2.md`](../docs/SQLITE_WAL2.md) (the vendored
build; Hctree masters vs wal2 followers), and
[`../rust/rindle-cli/src/backup.rs`](../rust/rindle-cli/src/backup.rs) (`sqlite_header`, the one
place the repo already reads a SQLite file header).

---

## 0. Verdict

It is a **linker, not a concatenation**, and under the constraints the loader controls it is a
small, boring, fast one:

- **Feasible and small.** SQLite page references are absolute page numbers in exactly four
  places — interior-cell child pointers, the interior page's right-most pointer, overflow-chain
  `next` pointers, and the freelist/ptrmap machinery we never copy. Leaf pages have **no sibling
  pointers**. So a table whose rows do not overflow is moved by rewriting only its interior pages
  (0.4 % of the pages in the spike) and byte-copying the rest.
- **The win is the indexes.** `ATTACH` + `INSERT … SELECT` can move a table's rows in bulk but
  can never move an index — SQLite has no way to insert into an index b-tree, so every index is
  rebuilt (a sort + a b-tree build, single-threaded). Linking is the only way to *keep* an index
  that was built in parallel. Table rows come along for free.
- **The loader's constraints remove every hard case.** Same library ⇒ same page size, reserved
  bytes, encoding, schema format; `auto_vacuum = NONE` (the vendored default) ⇒ no pointer-map
  pages and no root-placement rules; closed files ⇒ no hot WAL. What is left is a page copy with
  pointer relocation and a handful of header fields.
- **What it does not buy.** One giant table's index build is still one thread; the sorter can use
  worker threads (`PRAGMA threads`, §8) but the b-tree build cannot. Splitting one table across
  files is a real extension (§9) but not v1.
- **Verification is the expensive part, deliberately.** `PRAGMA integrity_check` read the 1 GiB
  output in 55.8 s — 5.7× the stitch. The linker's own structural checks are free; the full check
  must be opt-in.

---

## 1. Why this is a linker — the file-format facts, pinned to the vendored source

Everything below is [sqlite.org/fileformat.html](https://sqlite.org/fileformat.html), checked
against `rust/vendor/libsqlite3-sys/sqlite3/sqlite3.c` (3.54.0, hctree-bedrock) where the build
could differ from stock.

| Fact | Consequence for the linker |
| --- | --- |
| Page 1 = 100-byte header + the `sqlite_schema` b-tree root. `sqlite_schema.rootpage` names every table's and index's root. | The output's page 1 and schema are written by **SQLite itself** (the skeleton, §3.1). Source page 1 is never copied — so a source's journal mode, change counter, and schema layout are irrelevant. |
| Interior pages (flags `0x05` table, `0x02` index) hold a 4-byte child pointer per cell + a right-most pointer at header offset 8. | These are the pointers to rewrite. 1 084 of 269 277 pages in the spike. |
| Leaf pages (`0x0d` table, `0x0a` index) have no sibling links. A cell whose payload exceeds the local maximum ends with a 4-byte overflow pointer; overflow pages start with a 4-byte `next`. | Leaves without overflow copy byte-for-byte. Finding the overflow pointer means walking the cell pointer array and applying the local-payload formula (`X`, `M`, `K` in the spec) per cell — cheap, and the spike's `local_table_leaf` / `local_index` do exactly that. |
| `WITHOUT ROWID` tables are index b-trees; `UNIQUE`/`PRIMARY KEY` constraints create `sqlite_autoindex_<t>_<n>` b-trees with `sql = NULL`. | The page type byte, not the schema row, drives parsing. Autoindexes are recreated by the skeleton's `CREATE TABLE` and matched to source trees **by name**. |
| The page containing byte `0x40000000` (`PENDING_BYTE`) is never used; SQLite's allocator skips it. | The output allocator skips page `1 GiB / page_size + 1` (262 145 at 4 KiB) and leaves it zero. The spike output crossed it. |
| `auto_vacuum ≠ NONE` adds pointer-map pages (reverse pointers) and constrains root placement. `SQLITE_DEFAULT_AUTOVACUUM` is `0` in the vendored build (`sqlite3.c:17334`) and no rindle opener changes it. | Refuse any source with header offset 52 or 64 non-zero. Nothing else to do. |
| Header offset 28 (page count) is trusted only when offset 24 (change counter) == offset 92 (version-valid-for). | Write all three. Sources' counts are taken from file length, not the header. |
| **wal2** is header bytes 18/19 = **3**: `lockBtree` opens wal2 when `page1[19]==3` (`sqlite3.c:78115–78139`), `sqlite3BtreeSetVersion` asserts `1|2|3` (`:86574`). `rindle-cli`'s inspector already documents `1==rollback, 2==wal, 3==wal2`. | The output is created in `journal_mode = DELETE` and switched with `PRAGMA journal_mode = wal2` after linking — 404's handoff, unchanged. The spike's output reopens as `wal2`. |
| Reserved bytes per page (offset 20) are `0` — no codec, no checksum VFS in the build; `vendor/libsqlite3-sys/build.rs` sets no flag that touches the file format. Default page size 4 096 (`rindle-cli/src/backup.rs:1570` pins it). Encoding UTF-8 (offset 56 = 1). Schema format 4. | Assert equality across sources and skeleton; the loader guarantees it. |
| **Hctree** is a different file format. Replicator masters are Hctree; followers, standalone `rindled`, the producer scratch, and the portable backup base are ordinary b-tree/wal2 files (`docs/SQLITE_WAL2.md`). | The linker targets b-tree files only. An Hctree master gets its data the way it does today (logical import). |

SQLite exposes **no** API for this: `sqlite3_backup` and `VACUUM INTO` copy whole databases, and
there is no "insert into index". So it is an offline file-format tool. Given the constraints, that
is a feature — it depends on the documented format, not on the amalgamation.

### 1.1 The alternative that looks parallel but is not

`BEGIN CONCURRENT` (in the build) lets several connections write one file, and at commit
`btreeFixUnlocked` (`sqlite3.c:79198`) relocates every page the transaction allocated past the
file's new end. Two things kill it for parallel index builds: a `CREATE INDEX` flips the
transaction to `CONCURRENT_SCHEMA` (`:109071`, `:112987`), which puts page 1 back into the
conflict set (`:86651`) — and every committing writer that grew the file touched page 1 — so
concurrent index builds conflict and retry from scratch; and the relocation is a page-by-page
rewrite of the index just built. The daemon deleted its own `BEGIN CONCURRENT` derivation probe
for adjacent reasons (306 S5). Separate files + a linker is the shape that actually parallelizes.

**Measured 2026-09-04** — [`../spikes/wal2-concurrent-import/`](../spikes/wal2-concurrent-import/),
which builds the same 6-table / 3 M-row / 12-index database six ways against the vendored
amalgamation. The claim above holds and the margin is wide: 6 threads on one wal2 file with
ordinary `BEGIN IMMEDIATE` is **0.89–0.93×** of one thread (wal2 adds no second writer — one
`WAL_WRITE_LOCK`, `walWriteLock()` is journal-mode-agnostic); with `BEGIN CONCURRENT` the *data*
wave does parallelize (**3.0–3.2×**, zero conflicts on disjoint tables) but the *index* wave is
**0.77–0.81×** and needs 36–39 conflict-retries to land 12 indexes, for **1.09–1.17×** end to
end — **1.23–1.41×** even when the indexes are built serially to sidestep the conflicts — against
**3.5–3.6×** for the same tables built as six separate files, before the linker runs. Two
mechanisms, both pinned in [`FINDINGS.md`](../spikes/wal2-concurrent-import/FINDINGS.md): DDL is a
*global* barrier under `BEGIN CONCURRENT` (a `CREATE INDEX` conflicts with every other transaction,
and every in-flight concurrent *data* transaction conflicts with it), and a concurrent transaction
**cannot spill its page cache** (`pagerStress`: `if( pPager->pAllRead ) return SQLITE_OK;`), so its
resident set grows with the transaction rather than with `cache_size` — +119 MiB for a 168 MiB
one-transaction load, against +384 KiB for the same load in plain mode.

---

## 2. The contract — what the loader promises, and what each promise removes

The producer of the files is a loader that knows it is feeding a linker — Zero's initial sync
first, rindle's genesis later (§6) — so these are cheap to keep, and the linker **refuses** on any
violation rather than degrading. The contract is about the **file**, not the caller's code: a
loader in any language, on any SQLite build that produces these header values, qualifies.

| Promise | Checked how | Hazard it removes |
| --- | --- | --- |
| Same **file-format parameters** — page size, reserved bytes, encoding, schema format. Building every file with the same library and pragmas is the easy way to get them; the header is what is checked | header offsets 16–23, 44, 56 equal across sources and skeleton | mismatched page size / reserved bytes / encoding / schema format |
| Sources hold **user tables and their indexes, nothing else required** | nothing — the linker reads `sqlite_schema` and links every b-tree it lists | the linker depending on, reading, or writing any consumer's metadata. Zero's `_zero.*` tables, rindle's `_rindle_*` tables, and any other bookkeeping are the consumer's, **added after the stitch** (§3.3). A source that happens to contain such a table is linked like any other table |
| `auto_vacuum = NONE` | offsets 52 and 64 are `0` | pointer-map pages, root-placement rules |
| Every file checkpointed and **closed** | no `-wal`, `-wal2`, `-shm`, `-journal` sidecar exists | a stale main file behind a hot WAL; hot rollback journals |
| One logical unit per file (a table + its indexes; several tables is fine too) | schema rows read through SQLite | nothing — the linker walks whatever `sqlite_schema` lists; "one table per file" is the loader's parallelism unit, not the linker's requirement |
| Distinct object names across files | skeleton `CREATE` fails on a duplicate | two tables claiming one name |
| No virtual tables in v1 (§8) | `type = 'table'` with `CREATE VIRTUAL TABLE` refused | shadow-table initialization racing our pages |

Not required: the same **journal mode** (page 1 is never copied), the same **row counts**, an empty
**freelist** (freelist pages are simply not reachable from any root and are left behind — the
spike sources carried 920), the same **schema cookie**, or any particular **page order** inside a
source.

---

## 3. Algorithm — as spiked, with the two decisions that matter

### 3.1 The skeleton: let SQLite lay out page 1

Open the output with `PRAGMA page_size`, `encoding`, `auto_vacuum = NONE`, `journal_mode = DELETE`;
in one transaction run every source's `sqlite_schema.sql` — tables, then indexes, views, triggers —
skipping `sqlite_sequence`/`sqlite_stat*` (re-derived) and rows with `sql = NULL` (autoindexes come
with their table). Read back `SELECT name, rootpage FROM sqlite_schema`. Close. Now the output has
a correct page 1, a correct schema table (spanning pages if large), one **empty root page per
b-tree**, and SQLite — not us — chose every root. 1 ms in the spike.

### 3.2 The walk: allocate-at-discovery, O(depth) memory, one read per source page

For each source b-tree (`rootpage > 1`, not internal), starting from `(src_root → dst_root)`:

```
pop (sp, dp); read source page sp
interior (0x05 / 0x02):
    for each cell: child := be32(cell); nd := alloc(); write nd over child; push (child, nd)
                   (0x02 only) if the cell's payload overflows: relocate_overflow(ptr in cell)
    right-most := header[8..12]; nd := alloc(); overwrite; push
leaf (0x0d / 0x0a):
    for each cell: compute local payload size; if it overflows: relocate_overflow(ptr in cell)
write page at dp
relocate_overflow(ptr): nd := alloc(); overwrite ptr; then for each page of the chain:
    read; next := be32(0); nd' := next == 0 ? 0 : alloc(); overwrite; write at nd; advance
alloc(): next++, skipping the lock-byte page
```

Two properties fall out. **No page map**: a page's destination number is decided when its parent
is processed and travels with it on the stack, so memory is the tree depth, not the page count.
**Each source page is read exactly once and written exactly once**: interior and leaf pages during
the walk, overflow pages when their owning cell is met. The destination is written in discovery
order — a parent's children get consecutive numbers — which puts a leaf run contiguous behind its
parent, a better layout than the source's insertion order.

Cost model from the spike (4 KiB pages, 1 GiB): 1 084 interior pages rewritten, 40 000 leaves had
one 4-byte overflow pointer rewritten, 188 193 leaves were byte copies, 40 000 overflow pages had
their `next` rewritten. Parsing is a per-cell varint walk; the run was I/O-bound at 152 MiB/s on
the VM's disk with the fsync counted separately (2.9 s for 1 GiB).

### 3.3 Finish

Set header offsets 28, 24, 92; `set_len` to `pages × page_size` (the lock-byte hole stays zero);
fsync. Then through SQL, **SQLite's own state only**: `INSERT INTO sqlite_sequence` the sources'
rows (the skeleton's table is empty until a row is inserted, so plain `INSERT`); optionally
`ANALYZE` or copy `sqlite_stat1` rows; `PRAGMA journal_mode = <requested>` if the caller asked for
one (default: leave the file as SQLite wrote it, `DELETE`). Write to `<dest>.stitch-tmp` and
rename at the very end, so a crash leaves no half-linked file under the real name. A source at
the destination, at the temporary path, or at a sidecar of either is refused before anything is
removed (by inode, so no spelling of the path slips past): a source is never written.

That is the whole post-pass, and the line is deliberate: `sqlite_sequence` and `sqlite_stat*` are
state the **file format** requires to be consistent with the b-trees, so the linker owns them.
Everything above SQLite — a consumer's replication state, schema sidecars, version tables,
markers — is **metadata the consumer adds after the stitch** through an ordinary connection to an
ordinary SQLite file. The linker neither requires it in the sources nor writes it to the output.

### 3.4 What is verified for free, and what is not

Free, during the walk: the page type byte is one of four; every page number read is in
`2..=source_pages` and not the source's page 1; cell offsets and pointer offsets fall inside the
usable area; the number of pages visited per tree never exceeds the source page count (cycle
guard). Those catch corrupt input before it becomes corrupt output. Not free: index↔table
consistency and record decoding — that is `PRAGMA integrity_check`, O(output) reads, opt-in.

---

## 4. Spike results (2026-09-03, 4-vCPU VM, one virtual disk, vendored 3.54.0 wal2)

Six one-table sources, 2 M rows each, built in parallel the 404 way (DELETE-mode load, indexes,
`journal_mode = wal2`, closed): a rowid table with 6 KB blobs every 50th row (overflow), a `WITHOUT
ROWID` table, an `AUTOINCREMENT` table with a range deleted after its index build (freelist), and
three rowid tables with a `UNIQUE` column and two indexes each.

| Step | Time | Notes |
| --- | --- | --- |
| build 6 sources, parallel | 18.7 s | 6 threads on 4 vCPUs; 1 055 MiB, 920 freelist pages, headers `3/3` |
| **stitch** | **9.85 s** | skeleton 1 ms · link 6.92 s (152 MiB/s) · fsync + `sqlite_sequence` + wal2 2.92 s |
| `PRAGMA integrity_check` | 55.8 s | `ok`; 5.7× the stitch — must be opt-in |
| parity (`EXCEPT` both ways, 6 tables; planner uses all 10 indexes; `sqlite_sequence`) | — | 0 problems |
| baseline: ATTACH + `INSERT … SELECT` + `CREATE INDEX`, one thread | 44.2 s | 29.2 s of it is `CREATE INDEX` |
| today's shape: all six tables loaded, then all indexes, one connection, one file | **56.0 s** | load 27.4 s + `CREATE INDEX` 28.6 s: the sequential build the genesis / 404 path does |
| reference: `cp` + `sync` of the 1 GiB output | 16.6 s | the linker costs no more than a plain file copy on this disk |

Stitch vs. the one-thread merge of the same files: **4.5×**. Build-in-parallel + stitch vs.
today's sequential single-connection build of the same rows and indexes: **28.5 s vs 56.0 s**,
2.0× on four vCPUs with six tables — the parallel half scales with cores and tables, the stitch
is a third of the index-build time it replaces, and the ATTACH baseline (44.2 s) is flattered
besides, since it reads already-decoded SQLite rows rather than a `COPY` stream. The output
crossed page 262 145 (the lock-byte page) and opened clean. Pages: 269 277 = 1 084 interior + 228 193 leaf (40 000 with an overflow pointer
rewritten, 188 193 byte-copied) + 40 000 overflow.

---

## 5. The crate — `stitch-sqlite`

**`rust/stitch-sqlite`** (named `rindle-stitch-sqlite` on 2026-09-03 and renamed the same day
for publication — the crate is generic and the prefix said otherwise): a library with one
linking entry point,
the **source-file recipe** Rust callers build against, and a **standalone binary of the same name**
— the form the first consumer uses. Zero is TypeScript; it spawns the binary, which cargo-dist
ships for the workspace's targets (`[package.metadata.dist] dist = true` plus a line in
`scripts/stamp-dist-crate-version.sh`, or the release skips it). A napi binding is the later option
if spawning a process ever becomes the problem; nothing in the design depends on which. The
`rindle stitch` subcommand in `rindle-cli` is a thin alias for rindle operators.

For callers not in Rust, the recipe is a **documented pragma list**, not a function: open a fresh
file, `PRAGMA page_size = <agreed>; PRAGMA auto_vacuum = NONE; PRAGMA journal_mode = DELETE;
PRAGMA foreign_keys = OFF;`, load, create the indexes, close, make sure no
`-wal`/`-wal2`/`-shm`/`-journal` sidecar remains. Any SQLite build producing those header values
qualifies (§2). The `foreign_keys = OFF` is the one line the harness added to the recipe: the
vendored build enforces foreign keys **by default** (`SQLITE_DEFAULT_FOREIGN_KEYS=1`), and a
one-table file cannot see the table its rows reference, so a per-table load fails on the first
child row unless enforcement is off for the load. Enforcement is per connection; the stitched
output carries the constraints and the consumer turns them on when it opens the file.

```rust
/// The §2 contract as code: open a FRESH file with the pragmas the linker requires — page size,
/// `auto_vacuum = NONE`, `journal_mode = DELETE`, the caller's `synchronous`/`cache_size` — so a
/// loader cannot drift from the contract by forgetting one. The loader then does whatever it does
/// (COPY decode, INSERT, CREATE INDEX) on the returned connection.
pub fn open_source(path: &Path, opts: &SourceOptions) -> Result<Connection, StitchError>;
/// Close a loaded source: optional `ANALYZE`, switch to `journal_mode`, close, and check that no
/// sidecar survived. A sealed file is exactly what `stitch` accepts.
pub fn seal_source(conn: Connection, journal_mode: JournalMode) -> Result<(), StitchError>;

pub struct Options {
    /// Default `None`: no SQLite-level check of the output (§5.1 says what is still checked).
    pub verify: Verify,             // None (default) | Quick (PRAGMA quick_check) | Full (PRAGMA integrity_check)
    pub journal_mode: JournalMode,  // Delete (default: the file as SQLite wrote it) | Wal | Wal2
    pub stats: Stats,               // Skip (default) | CopyRows (sqlite_stat1 rows via SQL) | Analyze
    /// SQL functions the skeleton's `CREATE INDEX` may need (an expression index over `regexp`).
    /// The crate knows nothing about rindle's SQL surface; rindle callers pass `rindle_regex`'s
    /// registration.
    pub register: Option<fn(&Connection) -> rusqlite::Result<()>>,
}
pub fn stitch(sources: &[&Path], dest: &Path, opts: &Options) -> Result<Report, StitchError>;
```

- Depends on `rusqlite 0.32` `bundled` (the vendored SQLite via the workspace patch) for the
  skeleton, the schema read, the post-pass SQL, and verification — and on nothing else in the
  workspace. It therefore lives **outside** `rindle` (wasm-clean, C-toolchain-free) like every
  other SQLite-linking member, and it does not depend on `rindle-regex`, `rindle-replica`, or the
  Postgres crates: the `register` hook is how rindle-specific SQL functions reach the skeleton.
- `StitchError` is a taxonomy, not a string: `Incompatible { field, a, b }`, `Sidecar(path)`,
  `AutoVacuum(path)`, `Corrupt { page, why }`, `DuplicateObject(name)`, `Unsupported(kind)`,
  `Io`, `Sqlite`.
- `Report` carries what the spike prints: pages by kind, bytes, per-phase durations, whether the
  lock-byte page was skipped, verification outcome.
- Tests: `tests/differential.rs` — build sources through the recipe, stitch, then prove the
  output against SQLite itself: `integrity_check`, `EXCEPT` parity both ways per table, the
  planner's use of every index, `sqlite_sequence`, the stat tables. And against the **oracle**
  a linker has to match — the same loaders run on one connection into one file, a database
  built the ordinary way: the stitched file must agree on every query through every index
  (a full scan forward, a full scan backward, point lookups for the first/middle/last key),
  on SQLite's own tables, and on every b-tree page's place, cell count, payload, and slack
  (`dbstat`, keyed by tree path — page numbers are the one thing a linker may change); then
  it must be exactly its trees, no freelist, no page outside a b-tree. Two negative tests
  keep the oracle honest: a reference with one extra row fails on the table scan, and the
  same rows loaded in reverse order pass every query and fail on the shape. Shapes: the spike's five, page
  sizes 512 / 1 024 / 65 536 with payloads bracketing `X` and `M` for table leaves and index cells
  (leaf and interior overflow), UTF-16le, empty and one-row tables, views/triggers/FKs across
  sources, all three journal modes (header bytes checked), `Stats::CopyRows`/`Analyze`, every
  refusal in §2/§8, three corrupt-source shapes (bad flag, out-of-range pointer, cycle) with the
  temp file cleaned up, the recipe's own contract, and the CLI. `cargo test --workspace` picks the
  crate up with no new CI lane; the > 1 GiB lock-byte lane is `stitch-lockbyte` in
  `infra/tests/tests.mjs`, `ci: null` (a 1 GiB write for one allocator branch the unit test pins).
- Register in `README.md`'s crate table; `infra/tests/ci-sync.mjs` needs nothing for a plain
  member.
- **Published as its own repository**, [`rocicorp/stitch-sqlite`](https://github.com/rocicorp/stitch-sqlite),
  through josh: `scripts/publish-stitch-sqlite.sh` is the only place the mapping lives. The
  public history is ONE import commit — the monorepo commit tagged `stitch-sqlite/import`,
  squashed, so nothing before it is visible — then one commit per monorepo commit after it
  that touches a published path, with that commit's message and a `Monorepo-Commit` trailer.
  josh renders it with the composition filter `:[::LICENSE, ::docs/DESIGN.md=designs/415-…,
  :/rust/stitch-sqlite, :/rust/rindle-stitch-sqlite]` (file mappings first so the reverse pass
  sends LICENSE and the doc back to their monorepo homes; the old crate path stays because josh
  does not follow renames) and a squash file naming the import and every later commit; a
  second pass rewrites josh's token messages. `pull` ports public commits back by path onto a
  review branch, keeping author, dates, and message plus a `Public-Commit` trailer, and a
  ported commit renders as the public commit itself — so the round trip is exact even though
  GitHub signs the commits it creates. Rules: **publish from `main`** (a rendered commit is a
  function of the monorepo commit; rebasing or amending published commits re-renders them and
  the public main stops being a fast-forward — the script refuses; `--force` re-anchors while
  nobody has built on the public history), merge ported commits without squashing, and push the
  import tag to the monorepo remote. The crate builds against stock SQLite (`rusqlite`
  bundled); its wal2 assertions fall back to wal there and say so, and the workspace's wal2
  link is pinned by `rindle-cli`'s `workspace_build_is_wal2_capable`. The crate's own CI
  workflow lives at `rust/stitch-sqlite/.github/workflows/ci.yml`, inert here, live there.

### 5.0 Which SQLite it needs — any, except for wal2

The page copy never goes through SQLite, and the skeleton, the schema read, and the post-link
pass use nothing beyond stock SQLite. The one exception is **wal2**, a bedrock-branch feature the
vendored build carries and stock does not: reading a source *sealed* in wal2 (header bytes 18/19
= 3 — stock `lockBtree` answers `SQLITE_NOTADB` to `page1[19] > 2`) or *emitting* a wal2 output
needs a wal2-capable build. Measured 2026-09-03 by compiling the crate outside the workspace, with
no `[patch.crates-io]`, against rusqlite's stock bundled **3.46.0**: 9 of the 12 differential
tests pass unchanged — the linker, every page size, UTF-16, stats, views/triggers/FKs, every
refusal, the corrupt-source shapes — and the 3 that fail are exactly the wal2 cases (a wal2
output, a wal2-sealed source, the CLI's `--journal-mode wal2`), each with the reason in the
error: `Verification { stage: "journal_mode", … "a wal2 output needs a wal2-capable build" }` and
`Unsupported { … "the source is in wal2 format … seal sources with journal_mode delete or wal, or
build the linker against a wal2-capable SQLite" }`. Those three tests stay as they are: inside the
workspace they pin that the crate links the vendored build, the way `rindle-sqlite/tests/wal2.rs`
does. The shipped binary is built in the workspace and is wal2-capable; a consumer sealing its
sources in `delete` (the recipe's default) and asking for a `delete` or `wal` output can build the
crate against any SQLite.

### 5.1 What `Verify::None` still checks (decided 2026-09-03: it is the default)

"None" means **no SQLite-level pass over the output** — no `quick_check`, no `integrity_check`,
nothing O(output). It does not mean the linker trusts its input. Two layers stay on at every
level, because they are part of doing the walk correctly rather than checking it afterwards:

1. **The walk's structural checks (§3.4)**, free: the page type byte is one of the four b-tree
   flags; every page number read lies in `2..=source_pages`; every cell offset, child pointer,
   and overflow pointer lies inside the usable area; the pages visited per source never exceed
   the source's page count (cycle guard); every source b-tree has a root in the skeleton. A
   source with a broken tree is refused with `Corrupt { page, why }` before a byte reaches the
   output.
2. **The finish smoke probe**, O(trees × depth) pages — microseconds: the output is reopened
   through SQLite anyway (the `sqlite_sequence` merge and the `journal_mode` switch), and while it
   is open the linker checks `PRAGMA journal_mode` returns what was asked and `sqlite_schema`
   holds the expected object count; walks, through the file, the leftmost path of every linked
   tree from its root to a leaf (every page a b-tree page of the root's family — index or table —
   interior until the last, every pointer inside the output); and has SQLite open every tree it
   can name — one `SELECT … LIMIT 1` per table, one `INDEXED BY` probe per index. A wrong root or
   a mis-patched header cannot survive it. The one tree SQL cannot name is a partial index: the
   planner uses it only under a `WHERE` that implies the index's own, and that predicate lives
   nowhere but the `CREATE INDEX` text. The linker does not parse SQL — SQLite parsed it, at
   skeleton time — so partial indexes (`PRAGMA index_list` says which) get the page-level walk
   alone, and the differential tests, which know each predicate, check the planner's use.

What `None` cannot catch is corruption *inside cell payloads* of a source — the linker never
decodes a record — and index↔table inconsistency a source already had. Both mean the source was
already bad when the same library sealed it seconds earlier, i.e. a disk-level event, which is
what the 235 drills and `Verify::Full` exist for. `Full` reads the whole output (55.8 s per GiB on
the spike's disk, ~90 minutes for a 100 GB base); `Quick` skips the index↔table check but still
reads every page. Neither belongs on the default path; the CLI exposes both and the drills pass
`--verify full`. And the producer path already pays one: `PortableWal2Applier::finish` runs
`PRAGMA quick_check` on every base it trusts (`verify_sqlite_main`), so a stitched base handed to
the 235 snapshot writer gets an O(size) check there whether or not the linker ran one — a second
reason the linker's default is `None`.

---

## 6. Where it plugs in

### 6.0 First deployment target: Zero (decided 2026-09-03)

Zero's `zero-cache` builds its SQLite replica from Postgres by an initial sync — one `COPY` per
published table inside an exported snapshot, into a SQLite built from
[zero-sqlite3](https://github.com/rocicorp/zero-sqlite3), the bedrock lineage whose production
define set is the one the workspace's vendored amalgamation is built with (`docs/SQLITE_WAL2.md`).
That is the linker's contract already: same file-format parameters, `auto_vacuum` off, one table
per file if the loader chooses to. The shape is

```
COPY t1 ─▶ t1.db (Zero's table shape + indexes) ─┐
COPY t2 ─▶ t2.db                                  ├─ N sessions on ONE exported snapshot ─▶ stitch-sqlite ─▶ replica.db ─▶ Zero adds its metadata
COPY tN ─▶ tN.db                                  ┘
```

The linker sees files, not Zero: the per-row version columns, the index set, and the table names
are whatever the loader wrote; Zero's own replication-state tables are written **after** the
stitch, by Zero, through its own connection, exactly as if the file had been built the old way.
The two things that are Zero's to get right are the ones in §6.1 that no linker can check — every
session on the same exported snapshot, and every index created before the file is sealed.
Rindle contributes the binary and the contract; it has no code in the loop.

### 6.0.1 Later: rindle

**406 §7.3, the producer cold start**, is the slot the doc already reserves, and the same tool
fits it unchanged. Today genesis (`rindle-cdc-gateway/src/genesis.rs`) `COPY`s each published table through
one Postgres session and emits `rows` frames of 4 096 changes / 32 MiB into the journal; the
producer and every follower **apply those frames row by row** through `ApplyConsumer` with the
capture hook on. 404's direct construction (`rindle-replica/src/initial_snapshot.rs`) is faster
but still one connection, one `INSERT` per row, indexes after. The linker changes the shape:

```
COPY t1 ─▶ t1.db (load, then indexes) ─┐
COPY t2 ─▶ t2.db                       ├─ N sessions in parallel ─▶ stitch ─▶ wal2 base ─▶ 235 snapshot
COPY tN ─▶ tN.db                       ┘
```

Each per-table build is a `journal_mode = DELETE` load on its own connection — no writer
contention, no WAL, no capture hook — followed by that table's PK/unique index
(`rindle_replica_pk_*` where the PK is not a rowid alias, `rindle-cdc-apply/src/store.rs:749`) and
any advisory indexes. The stitched file is the first 235 base; the journal frames still stream so
the archive stays complete from birth (406 §7.2) — the producer just no longer has to *replay* them
to get its scratch. The rindle bookkeeping tables (`_rindle_columns`, `_rindle_tables`, source
offsets, the bootstrap marker) are small and ride as one more source file or as post-pass SQL; the
linker does not know about them.

**Standalone `rindled`** importing a Postgres database has the same shape without the plane.

### 6.1 The loader half is the caller's (decided 2026-09-03)

The crate ships no loader. Whoever wants to stitch writes the per-table files — through
`open_source`/`seal_source`, so the §2 contract is met by construction, not by reading it. That
splits the responsibilities cleanly; these are the caller's, and the linker can neither do nor
check them:

| The caller must | Because |
| --- | --- |
| Give every session the **same snapshot** (`SET TRANSACTION SNAPSHOT` on each of the N Postgres sessions against the one exported snapshot) | The linker stitches whatever it is given; N files from N inconsistent reads make one consistent-looking, wrong database. This is the one correctness property that moves to the caller — genesis does it on one session today (`genesis.rs`), N sessions need it N times |
| Keep **one table in one file** (several tables per file is fine) | Two files claiming one table is `DuplicateObject`; splitting a table is §9, not v1 |
| Create the **indexes before sealing** | Indexes are the point; an index created afterwards on the stitched file is a single-threaded rebuild. `ApplyStore`'s `CREATE UNIQUE INDEX IF NOT EXISTS rindle_replica_pk_*` registration stays idempotent over a source that already carries it |
| Put a trigger and every table it writes **in the same file** | A trigger fires during the load; SQLite triggers cannot cross databases anyway |
| Add its **own metadata after the stitch** — Zero's replication-state tables; rindle's `_rindle_columns`, `_rindle_tables`, source offsets, the bootstrap marker — through an ordinary connection to the output | The linker links what `sqlite_schema` lists and knows nothing above SQLite (§3.3) |
| Size the **parallelism**: sessions × `cache_size` must fit RAM; `PRAGMA threads = k` per connection for the sorter (`SQLITE_MAX_WORKER_THREADS` is 8 in the build, default 0) | The linker sees sealed files, not the build |
| Handle a **crashed build**: a source left with a hot `-journal` is refused, never recovered by the linker | Recovering someone else's journal is how a linker loses a race (§8) |
| Load with **`foreign_keys = OFF`** (the recipe does) | The build enforces FKs by default and a one-table file cannot see the parent table; `PRAGMA foreign_key_check` on the stitched output is the consumer's post-stitch step if it wants proof |

None of these is new work for the genesis: it already snapshots, already creates the PK index,
already writes the bookkeeping tables. What changes is that it does the first three N times, in
parallel, into N files.

**Not** a fit: Hctree masters (§1), live databases, and anything a single `INSERT … SELECT` already
does in seconds.

---

## 7. Sizing the win honestly

Per gigabyte of output on the spike's disk: link ≈ 7 s + fsync ≈ 3 s. On an NVMe that sustains
2 GB/s the linker is CPU-bound on the varint walk long before it is I/O-bound, and the walk is
trivially parallel *across sources* (each source is an independent tree walk into disjoint page
ranges — reserve a range per source from the sizes in the headers, minus freelist pages, and the
allocator never contends). The **parallel build** is where the hours go: N tables on N cores, each
`CREATE INDEX` on its own connection with `PRAGMA threads = k` for the sort. What does *not* shrink
is one table's b-tree build; §9 is the answer if a single table dominates.

### 7.1 Measured and implemented (2026-09-04)

[`../spikes/stitch-perf/`](../spikes/stitch-perf/) ports the walk to C with the I/O strategy
switchable and measures each candidate on 6 sources × 2 M rows (600 966 pages, 2 347 MiB, 4 vCPU,
median of 3). Two changes landed in the crate:

| | link s | fsync s | link+fsync |
| --- | ---: | ---: | ---: |
| as shipped before — 1 thread, one `pread` + one `pwrite` per page | 4.37 | 3.06 | 7.43 |
| chunked reads + run-coalesced writes | 1.95 | 2.52 | 4.51 |
| … + sources linked in parallel into reserved ranges (`threads = 4`) | **0.61** | 2.16 | 2.78 |

The **link** is 7.2× faster and its 1.2 M syscalls become 6 384; what is left is the destination
`fsync`, which is the disk (~900 MiB/s here) and which no linker change removes. So the link stops
being the part that does not scale, and the stitch becomes bounded by the output write.

Two findings worth carrying:

- **Both sides of the copy need K buffered slots, not one.** The walk emits and consumes two
  interleaved ascending streams — the current subtree's pages, and the overflow pages allocated
  *behind* them, since a leaf's overflow is allocated after the leaf. A single write run therefore
  coalesced 600 966 pages into only 482 268 writes (runs of 1.25 pages); four LRU slots make it
  3 498. A single read window is worse than useless: it re-read 3.2 bytes per byte used and made
  batching **slower** than the baseline on warm sources. `link.rs` uses 4 slots of 512 KiB a side.
- **§7's reservation formula is not quite exact.** "Minus freelist pages" leaves out two terms:
  the linked trees' ROOTS come from the skeleton, and the internal trees (`sqlite_sequence`,
  `sqlite_stat*`) are never copied. So
  `reserve = page_count − 1 − freelist_count − internal_tree_pages − linked_tree_count`, with
  `internal_tree_pages` from a count-only walk of trees that are one or two pages by nature.
  Over-reserving leaves a hole, and a hole is not cosmetic: `integrity_check` reports
  "Page N is never used", and `differential.rs` asserts the output has no freelist at all. A
  leftover-to-freelist path stays as the safety net.

`Options::threads` / `--threads N` selects it; the default is still the sequential walk.

---

## 8. Awkward cases and their disposition

| Case | v1 |
| --- | --- |
| Corrupt or hand-edited source | The walk's structural checks (§3.4) refuse; `Verify::Full` is the backstop. A linker cannot make a corrupt tree valid and must not pretend to |
| Overflow chains | Handled (spike: 40 000 chains). The pointer sits after `local` bytes of payload; `local` follows the spec's `X`/`M`/`K` rule and differs for table vs index cells |
| `WITHOUT ROWID`, `UNIQUE`/PK autoindexes, partial / `DESC` / multi-column / expression indexes, collations | Handled — all are b-trees; the skeleton recreates them from `sql`, autoindexes by name. Expression indexes need their functions registered at skeleton time (§5) |
| `AUTOINCREMENT` | `sqlite_sequence` rows merged by SQL (spike verified) |
| `sqlite_stat1`/`stat4` | Not linked. `Stats::CopyRows` copies the rows by SQL (exact, cheap); `Analyze` recomputes |
| Freelist pages in a source | Unreachable from any root ⇒ never copied (spike: 920). The output has whatever freelist the skeleton had (none) |
| Views, triggers, FKs | Schema only; recreated by the skeleton. Triggers cannot fire — the skeleton runs no DML |
| Virtual tables (FTS5, R\*Tree) | **Refused in v1.** Shadow tables are ordinary b-trees and would link, but `CREATE VIRTUAL TABLE` in the skeleton initializes shadow rows we would then overwrite — plausible, untested, not needed for a Postgres mirror |
| Reserved bytes ≠ 0 / codec / checksum VFS | Refused unless equal across sources and skeleton; per-page checksums would need recomputation after a pointer rewrite — not our build |
| Different page size across sources | Refused. The loader controls it |
| Output > 1 GiB | Lock-byte page skipped (spike crossed it). > 2^31 pages is beyond SQLite anyway |
| Crash mid-stitch | Temp file + rename; the skeleton is valid SQLite at every point, the temp name is never opened by anyone else |
| Hot WAL / journal on a source | Refused on sidecar presence. Not "checkpointed for you": a linker that opens sources read-write is a linker that can lose a race |
| Same table in two sources | Refused (`DuplicateObject`). Merging two b-trees of one table is §9 |

---

## 9. Extension — sharding one table across files (not v1)

When one table dominates, its `COPY` can be range-split by primary key into files with
**disjoint, ordered rowid ranges**. Their table b-trees concatenate: the shard roots become children
of a new interior level whose cell keys are each shard's maximum rowid, after equalizing tree
heights (SQLite requires every leaf at the same depth, and `integrity_check` checks it). That is
a few extra pages, still no row decoding. The shards' **indexes** do not concatenate — their key
ranges interleave — so a sharded table's indexes are built after the stitch, still in parallel
with the other tables' builds. For rowid tables only; a `WITHOUT ROWID` table sharded by key
concatenates the same way (its "table" is an index b-tree ordered by that key). Worth its own
doc once a real dataset shows one table taking most of the wall clock.

---

## 10. Decisions

Decided 2026-09-03 (review of the spike):

1. **Name and home: `stitch-sqlite`** at `rust/stitch-sqlite`; a `rindle stitch`
   subcommand in `rindle-cli` beside `rindle backup inspect`, which already parses the header.
2. **The loader half is the caller's.** The crate ships the recipe (`open_source`/`seal_source`,
   §5) and nothing Postgres-shaped; §6.1 lists what that leaves with the caller. When the genesis
   adopts it, its N-session `COPY → t.db` builder lives in `rindle-cdc-gateway`.
3. **Default `Verify::None`** — no SQLite-level pass over the output. The walk's structural
   checks and the finish smoke probe are not verification levels and are always on (§5.1); the
   drills run `Full`.

4. **The linker emits a normal SQLite file** (decided 2026-09-03); it never writes the 235 snapshot
   form. The 235 snapshot *is* a plain SQLite main file split at fixed offsets into independently
   zstd-compressed parts plus a marker written last (`rindle-backup/src/snapshot.rs`), and the
   producer uploads its checkpointed, closed scratch as-is — "no second copy on disk"
   (`rindle-backup-sqlite/src/producer.rs`). The stitched file is exactly that input. A direct
   emitter would be a second writer of the format to keep byte-compatible and drilled, would
   bypass the guards the portable-base path applies before it trusts a file
   (`validate_portable_schema`, the source-cursor check against the expected cid,
   `verify_sqlite_main`, empty sidecars — `PortableWal2Applier::finish`), and would save one
   sequential read of a file still warm in the page cache while the upload dominates. The plain
   file is also the artifact every other consumer wants: standalone `rindled` import, a follower
   install by rename, `sqlite3` on a laptop. **Consequence for the caller (§6.1):** the stitched
   base must carry the source-cursor row at the genesis end cid and a schema the portable-base
   validation accepts, so that the *existing* snapshot path takes it unchanged.

5. **Generic tool, metadata after the stitch, Zero first** (decided 2026-09-03). The linker
   requires no consumer bookkeeping in the sources and writes none to the output; its post-pass
   is SQLite's own state (`sqlite_sequence`, optional `sqlite_stat1`) and nothing else (§3.3).
   Every consumer adds its metadata afterwards through a normal connection. The first
   deployment is Zero's initial sync, consuming the standalone binary (§6.0); rindle's producer
   cold start comes later and needs nothing the tool does not already do. **Amendment to the
   §5 defaults this implies:** `journal_mode` defaults to `Delete` — the output is the file as
   SQLite wrote it, openable by any build — and a consumer that wants `wal`/`wal2` either passes
   it or sets it when it opens the file for its metadata anyway. (The earlier `Wal2` default was
   a rindle assumption, not a decision.)

Nothing is open. The crate shipped the same day (§5); what remains is its first consumer.

## References

- [sqlite.org/fileformat.html](https://sqlite.org/fileformat.html) — header, b-tree pages, cell
  formats, the local-payload rule, overflow, freelist, the lock-byte page.
- `rust/vendor/libsqlite3-sys/sqlite3/sqlite3.c` — `lockBtree` (:78115), `sqlite3BtreeSetVersion`
  (:86574), `SQLITE_DEFAULT_AUTOVACUUM` (:17334), `SQLITE_MAX_WORKER_THREADS` 8 /
  `SQLITE_DEFAULT_WORKER_THREADS` 0 (:15977, :15980), `btreeFixUnlocked` (:79198),
  `CONCURRENT_SCHEMA` (:19292).
- [`spikes/sqlite-stitch/`](../spikes/sqlite-stitch/) — the linker, the harness, the run.
