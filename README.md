# stitch-sqlite

A SQLite **B-tree linker**: stitch N closed, one-table-per-file SQLite databases into ONE
database by relocating pages — never by re-inserting a row or rebuilding an index.

## Why

`ATTACH` + `INSERT … SELECT` can move a table's rows, but SQLite has no way to insert into an
index b-tree, so every index of the combined file is rebuilt single-threaded. Linking is the only
way to *keep* indexes that were built in parallel, one table per file, on N connections: the
interior child pointers, right-most pointers, and overflow-chain links of every b-tree are
rewritten through an allocate-at-discovery page map, and every other byte is copied.

The tool is generic. It links whatever `sqlite_schema` lists and knows nothing above SQLite; its
post-link SQL pass touches SQLite's own state only (`sqlite_sequence`, optionally `sqlite_stat*`,
the requested `journal_mode`). Add your own metadata to the output afterwards through an ordinary
connection to an ordinary SQLite file.

## The recipe

Sources must share their file-format parameters (page size, reserved bytes, text encoding,
schema format), be built with `auto_vacuum = NONE`, and be closed with no `-wal` / `-wal2` /
`-shm` / `-journal` sidecar. The library makes that hard to get wrong:

```rust
use stitch_sqlite::{open_source, seal_source, stitch, JournalMode, Options, SourceOptions};
use std::path::Path;

// The loader half (yours), one file per table — in practice on N threads / N connections.
let conn = open_source(Path::new("t1.db"), &SourceOptions::default())?;
conn.execute_batch("CREATE TABLE t1(id INTEGER PRIMARY KEY, name TEXT); CREATE INDEX t1_name ON t1(name);")?;
seal_source(conn, JournalMode::Delete)?;

// The linker half: one file out.
let report = stitch(&["t1.db", "t2.db"], Path::new("out.db"), &Options::default())?;
println!("{} pages, {} b-trees", report.dest_pages, report.link.trees);
```

For callers that are not Rust, the same recipe is a pragma list — open a fresh file,
`PRAGMA page_size = <agreed>; PRAGMA auto_vacuum = NONE; PRAGMA journal_mode = DELETE;`, load,
create the indexes, close — and the linker is a binary:

```
stitch-sqlite [options] <out.db> <source.db>...

  --verify none|quick|full        SQLite-level check of the output (default none)
  --journal-mode delete|wal|wal2  output journal mode (default delete)
  --stats skip|copy|analyze       sqlite_stat* handling (default skip; copy = the sources' rows)
  --force                         replace <out.db> if it exists
  --json                          print the report as JSON
  --quiet                         print nothing on success
```

## What is checked

Every source is inspected before a byte is written, and any contract violation is refused
rather than degraded. The walk checks every page it touches (b-tree flags, page numbers, cell
and pointer bounds, cycles), and a smoke probe reopens the output through SQLite, descends the
leftmost path of every tree, and has SQLite open every table and index. `--verify quick` adds
`PRAGMA quick_check` over the whole output, `--verify full` adds `PRAGMA integrity_check`.

The test suite is differential: the stitched file is held to a reference built the ordinary way
— every query through every index, forward, backward, and by point lookup, and the shape of every
b-tree page for page. See [`docs/DESIGN.md`](docs/DESIGN.md) for the design and its measurements.

## Which SQLite

Any. The page copy never goes through SQLite, and everything else uses stock SQLite; the crate
builds with `rusqlite`'s bundled SQLite. The one exception is **wal2**, a
[bedrock-branch](https://sqlite.org/src/timeline?r=bedrock) feature: reading a source sealed in
wal2 or emitting a wal2 output needs a wal2-capable build, and a stock build reports those cases
as errors with the reason. The tests fall back to wal on a stock build and say so.

Unix only for now: positional file I/O, inode comparison, and raw path bytes are the three seams.

## License

Apache-2.0.
