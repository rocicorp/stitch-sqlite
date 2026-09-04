//! The differential harness (design 415 §5): build sources through the recipe, stitch, then
//! prove the output against SQLite itself — `integrity_check`, `EXCEPT` parity both ways against
//! every source, the planner's use of every index, `sqlite_sequence`, the stat tables — across
//! the shapes the file format makes awkward and the refusals the contract promises.
//!
//! The oracle a linker has to match is a database built the ordinary way: the SAME loaders run
//! on ONE connection into ONE file ([`Lab::reference`]). Against it, [`assert_same_database`]
//! holds the stitched file to every query through every index — forward, backward, and by point
//! lookup — to SQLite's own tables, and to the shape of every b-tree page for page (`dbstat`, by
//! tree path). Page numbers are the one thing a linker is allowed to change.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags};
use stitch_sqlite::{
    cli, local_payload, open_source, seal_source, stitch, Encoding, JournalMode, Kind, Options,
    Report, SourceOptions, Stats, StitchError, Verify,
};
use tempfile::TempDir;

struct Lab {
    dir: TempDir,
}

impl Lab {
    fn new() -> Lab {
        Lab {
            dir: tempfile::Builder::new()
                .prefix("rindle-stitch-")
                .tempdir()
                .expect("tempdir"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Build one source through the recipe. `f` owns its transactions.
    fn build(
        &self,
        name: &str,
        opts: &SourceOptions,
        seal: JournalMode,
        f: impl FnOnce(&Connection),
    ) -> PathBuf {
        let conn = open_source(&self.path(name), opts).expect("open_source");
        f(&conn);
        seal_source(conn, seal).expect("seal_source")
    }

    fn build_default(&self, name: &str, f: impl FnOnce(&Connection)) -> PathBuf {
        self.build(name, &SourceOptions::default(), JournalMode::Delete, f)
    }

    /// The oracle: every loader on ONE connection into ONE file — a database built the ordinary
    /// way, under the same file-format pragmas the sources got.
    fn reference(
        &self,
        name: &str,
        opts: &SourceOptions,
        loaders: &[&dyn Fn(&Connection)],
    ) -> PathBuf {
        self.build(name, opts, JournalMode::Delete, |c| {
            for load in loaders {
                load(c);
            }
        })
    }

    fn stitch(
        &self,
        sources: &[PathBuf],
        opts: &Options,
    ) -> Result<(Report, PathBuf), StitchError> {
        let out = self.path("stitched.db");
        stitch(sources, &out, opts).map(|r| (r, out))
    }
}

fn full() -> Options {
    Options {
        verify: Verify::Full,
        ..Options::default()
    }
}

fn open(path: &Path) -> Connection {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open")
}

/// `file:<path>?immutable=1`, the way the linker spells it: `%`, `?`, `#` percent-encoded.
fn immutable_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for c in path.to_str().unwrap().chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3F"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri + "?immutable=1"
}

fn attach(conn: &Connection, path: &Path, alias: &str) {
    conn.execute(
        &format!("ATTACH DATABASE ?1 AS {alias}"),
        params![immutable_uri(path)],
    )
    .expect("attach");
}

fn user_tables(path: &Path) -> Vec<String> {
    let conn = Connection::open_with_flags(
        immutable_uri(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open source");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let names = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    names
}

/// Every user table of `source` is byte-for-byte the same relation in `dest`.
fn assert_parity(dest: &Path, source: &Path) {
    let conn = open(dest);
    attach(&conn, source, "src");
    for t in user_tables(source) {
        let q = |sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
        };
        let (a, b) = (
            q(&format!("SELECT count(*) FROM main.\"{t}\"")),
            q(&format!("SELECT count(*) FROM src.\"{t}\"")),
        );
        assert_eq!(a, b, "{t}: row count");
        let missing = q(&format!(
            "SELECT count(*) FROM (SELECT * FROM src.\"{t}\" EXCEPT SELECT * FROM main.\"{t}\")"
        ));
        let extra = q(&format!(
            "SELECT count(*) FROM (SELECT * FROM main.\"{t}\" EXCEPT SELECT * FROM src.\"{t}\")"
        ));
        assert_eq!(
            (missing, extra),
            (0, 0),
            "{t}: {missing} source rows missing, {extra} extra"
        );
    }
    conn.execute_batch("DETACH DATABASE src").unwrap();
}

fn assert_parity_all(dest: &Path, sources: &[PathBuf]) {
    for s in sources {
        assert_parity(dest, s);
    }
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

type Rows = Vec<Vec<Value>>;

fn rows(conn: &Connection, sql: &str, params: &[Value]) -> Rows {
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let n = stmt.column_count();
    stmt.query_map(params_from_iter(params.iter()), |r| {
        (0..n).map(|i| r.get::<_, Value>(i)).collect()
    })
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
    .collect::<Result<Rows, _>>()
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// One query, both files, the same rows in the same order.
fn assert_same_rows(stitched: &Connection, reference: &Connection, sql: &str, params: &[Value]) {
    let (x, y) = (rows(stitched, sql, params), rows(reference, sql, params));
    if x != y {
        let at = x.iter().zip(&y).position(|(p, q)| p != q);
        let show = |rows: &Rows| {
            at.and_then(|i| rows.get(i).map(|r| format!("{r:?}")))
                .map(|mut d| {
                    d.truncate(300);
                    d
                })
        };
        panic!(
            "{sql} {params:?}: stitched {} rows, reference {} rows; first difference at row {at:?}: \
             {:?} vs {:?}",
            x.len(),
            y.len(),
            show(&x),
            show(&y)
        );
    }
}

/// The predicates of the partial indexes the tests create: a scan through a partial index needs
/// one, and the linker will not supply it (it never reads the `CREATE INDEX` text).
const PARTIAL_PREDICATES: &[(&str, &str)] = &[
    ("t0_score_hi", "score > 0.5"),
    ("\u{ed}ndice", "x IS NOT NULL"),
    ("w", "\"where\" > 'a'"),
    ("e", "x IS NOT NULL"),
    ("wr_v", "v IS NOT NULL"),
];

/// The stitched file against a reference built the ordinary way, on everything but page
/// numbers: SQLite's own tables; every table in its own order; every index by a full scan
/// forward, a full scan backward, and point lookups for the first, middle, and last key (the
/// seek path, not only the leftmost one); and every b-tree page's place, cell count, payload,
/// and slack (`dbstat`, keyed by tree path). Then the stitched file is exactly its trees: no
/// freelist, no page outside a b-tree.
fn assert_same_database(stitched: &Path, reference: &Path) {
    let a = open(stitched);
    let b = open(reference);
    let same = |sql: &str| assert_same_rows(&a, &b, sql, &[]);

    same("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name");
    if !rows(
        &b,
        "SELECT 1 FROM sqlite_schema WHERE name = 'sqlite_sequence'",
        &[],
    )
    .is_empty()
    {
        same("SELECT name, seq FROM sqlite_sequence ORDER BY name");
    }

    let tables: Vec<String> = rows(
        &b,
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        &[],
    )
    .into_iter()
    .map(|r| match &r[0] {
        Value::Text(t) => t.clone(),
        v => panic!("{v:?}"),
    })
    .collect();
    for table in &tables {
        let t = quote(table);
        same(&format!("SELECT * FROM {t}"));
        let table_rows = rows(&b, &format!("SELECT * FROM {t}"), &[]);
        for index in rows(&b, &format!("PRAGMA index_list({t})"), &[]) {
            let (Value::Text(name), Value::Integer(partial)) = (&index[1], &index[4]) else {
                panic!("index_list row {index:?}");
            };
            let predicate = if *partial != 0 {
                let (_, pred) = PARTIAL_PREDICATES
                    .iter()
                    .find(|(n, _)| n == name)
                    .unwrap_or_else(|| panic!("{name}: add its predicate to PARTIAL_PREDICATES"));
                Some(*pred)
            } else {
                None
            };
            let i = quote(name);
            let scan = format!("SELECT * FROM {t} INDEXED BY {i}");
            let and = |extra: &str| match predicate {
                Some(p) => format!("{scan} WHERE {p} AND {extra}"),
                None => format!("{scan} WHERE {extra}"),
            };
            same(&match predicate {
                Some(p) => format!("{scan} WHERE {p}"),
                None => scan.clone(),
            });
            // Key columns: (cid, name, desc, coll). An expression column has no name (cid -2).
            let keys: Vec<(i64, Option<String>, i64, String)> =
                rows(&b, &format!("PRAGMA index_xinfo({i})"), &[])
                    .into_iter()
                    .filter(|r| r[5] == Value::Integer(1))
                    .map(|r| match (&r[1], &r[2], &r[3], &r[4]) {
                        (Value::Integer(cid), name, Value::Integer(desc), Value::Text(coll)) => (
                            *cid,
                            match name {
                                Value::Text(n) => Some(n.clone()),
                                _ => None,
                            },
                            *desc,
                            coll.clone(),
                        ),
                        _ => panic!("index_xinfo row {r:?}"),
                    })
                    .collect();
            if keys.iter().any(|(_, name, _, _)| name.is_none()) {
                continue; // an expression index: no column to order by or seek on
            }
            let backward = keys
                .iter()
                .map(|(_, name, desc, coll)| {
                    format!(
                        "{} COLLATE {} {}",
                        quote(name.as_deref().unwrap()),
                        quote(coll),
                        if *desc == 0 { "DESC" } else { "ASC" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            same(&format!(
                "{} ORDER BY {backward}",
                match predicate {
                    Some(p) => format!("{scan} WHERE {p}"),
                    None => scan.clone(),
                }
            ));
            let (cid, first_key, _, _) = &keys[0];
            let column = quote(first_key.as_deref().unwrap());
            let values: Vec<&Value> = table_rows
                .iter()
                .map(|r| &r[*cid as usize])
                .filter(|v| **v != Value::Null)
                .collect();
            if values.is_empty() {
                continue;
            }
            for probe in [0, values.len() / 2, values.len() - 1] {
                assert_same_rows(
                    &a,
                    &b,
                    &and(&format!("{column} = ?1")),
                    &[values[probe].clone()],
                );
            }
        }
    }

    let shape = "SELECT name, path, pagetype, ncell, payload, unused, mx_payload FROM dbstat \
                 WHERE name NOT IN ('sqlite_schema', 'sqlite_master', 'sqlite_sequence') \
                 AND name NOT LIKE 'sqlite_stat%' ORDER BY name, path";
    same(shape);
    assert!(
        rows(&b, shape, &[]).len() >= tables.len(),
        "the shape comparison saw fewer pages than there are tables"
    );
    let count = |sql: &str| -> i64 { a.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("PRAGMA freelist_count"),
        0,
        "the stitched file has no freelist"
    );
    assert_eq!(
        count("PRAGMA page_count"),
        count("SELECT count(*) FROM dbstat"),
        "every page of the stitched file belongs to a b-tree"
    );
}

fn integrity_ok(path: &Path) -> bool {
    let conn = open(path);
    let rows: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows == ["ok"]
}

/// `Report.dest_pages` / `bytes` describe the file as published — after the finish pass, which
/// can grow it (`sqlite_stat*`), not as linked.
fn assert_published_size(r: &Report, path: &Path) {
    let len = fs::metadata(path).unwrap().len();
    let pages: i64 = open(path)
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    assert_eq!(r.bytes, len, "{}: bytes", path.display());
    assert_eq!(i64::from(r.dest_pages), pages, "{}: pages", path.display());
    assert_eq!(len, pages as u64 * u64::from(r.page_size));
}

/// Whether the SQLite this build links can do wal2 — the vendored bedrock build can, stock
/// SQLite cannot. On a stock build the wal2 assertions below exercise wal instead and say so;
/// that the rindle workspace links a wal2-capable build is pinned by `rindle-cli`'s tests.
fn wal2_capable(lab: &Lab) -> bool {
    let mode: String = Connection::open(lab.path("wal2-probe.db"))
        .unwrap()
        .query_row("PRAGMA journal_mode = wal2", [], |r| r.get(0))
        .unwrap();
    if mode != "wal2" {
        eprintln!("wal2 is not available in this SQLite build (answered {mode:?}); using wal");
    }
    mode == "wal2"
}

fn journal_mode(path: &Path) -> String {
    open(path)
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap()
}

fn no_sidecars(path: &Path) {
    for s in ["-wal", "-wal2", "-shm", "-journal"] {
        let p = PathBuf::from(format!("{}{s}", path.display()));
        assert!(!p.exists(), "sidecar {} left behind", p.display());
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

type Loader = Box<dyn Fn(&Connection)>;

/// The five shapes of the spike, at test scale — as loaders, so the same closure builds a
/// table's source file for the stitch and its share of the reference file.
fn spike_loaders(rows: i64) -> Vec<(&'static str, Loader)> {
    let mut v: Vec<(&'static str, Loader)> = Vec::new();
    v.push(("t0", Box::new(move |c: &Connection| {
        c.execute_batch("BEGIN; CREATE TABLE t0(id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, payload BLOB);").unwrap();
        let mut ins = c.prepare("INSERT INTO t0 VALUES (?1, ?2, ?3, ?4)").unwrap();
        let mut rng = Rng(1);
        let big = vec![0xABu8; 6000];
        let small = vec![0x11u8; 16];
        for r in 0..rows {
            let x = rng.next();
            let payload: &[u8] = if r % 50 == 0 { &big } else { &small };
            ins.execute(params![r + 1, format!("name-{:08x}", x & 0xffff_ffff), (x % 1000) as f64 / 1000.0, payload]).unwrap();
        }
        drop(ins);
        c.execute_batch("COMMIT; CREATE INDEX t0_name ON t0(name); CREATE INDEX t0_score_hi ON t0(score) WHERE score > 0.5;").unwrap();
    })));
    v.push(("t1", Box::new(move |c: &Connection| {
        c.execute_batch("BEGIN; CREATE TABLE t1(k TEXT PRIMARY KEY, v INTEGER NOT NULL, note TEXT) WITHOUT ROWID;").unwrap();
        let mut ins = c.prepare("INSERT INTO t1 VALUES (?1, ?2, ?3)").unwrap();
        let mut rng = Rng(2);
        for r in 0..rows {
            let x = rng.next();
            ins.execute(params![format!("key-{r:012}"), (x % 100_000) as i64, format!("note {}", x % 977)]).unwrap();
        }
        drop(ins);
        c.execute_batch("COMMIT; CREATE INDEX t1_v ON t1(v);").unwrap();
    })));
    v.push((
        "t2",
        Box::new(move |c: &Connection| {
            c.execute_batch(
                "BEGIN; CREATE TABLE t2(id INTEGER PRIMARY KEY AUTOINCREMENT, a INTEGER, b TEXT);",
            )
            .unwrap();
            let mut ins = c.prepare("INSERT INTO t2(a, b) VALUES (?1, ?2)").unwrap();
            let mut rng = Rng(3);
            for _ in 0..rows {
                let x = rng.next();
                ins.execute(params![(x % 1_000_000) as i64, format!("b-{}", x % 5000)])
                    .unwrap();
            }
            drop(ins);
            c.execute_batch("COMMIT; CREATE INDEX t2_a ON t2(a);")
                .unwrap();
            // After the index build, so the freed leaf pages stay on the freelist.
            c.execute(
                "DELETE FROM t2 WHERE id BETWEEN ?1 AND ?2",
                params![rows / 2, rows / 2 + rows / 10],
            )
            .unwrap();
        }),
    ));
    for (i, name) in [(3, "t3"), (4, "t4")] {
        v.push((name, Box::new(move |c: &Connection| {
            c.execute_batch(&format!("BEGIN; CREATE TABLE t{i}(id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL, u TEXT UNIQUE);")).unwrap();
            let mut ins = c.prepare(&format!("INSERT INTO t{i} VALUES (?1, ?2, ?3, ?4, ?5)")).unwrap();
            let mut rng = Rng(10 + i as u64);
            for r in 0..rows {
                let x = rng.next();
                ins.execute(params![r + 1, (x % 50_000) as i64, format!("b{}", x % 300), (x % 10_007) as f64 * 0.5, format!("u-{i}-{r}")]).unwrap();
            }
            drop(ins);
            c.execute_batch(&format!(
                "COMMIT; CREATE INDEX t{i}_a ON t{i}(a); CREATE INDEX t{i}_bc ON t{i}(b, c DESC); CREATE INDEX t{i}_expr ON t{i}(lower(b));"
            )).unwrap();
        })));
    }
    v
}

fn spike_sources(lab: &Lab, rows: i64) -> Vec<PathBuf> {
    spike_loaders(rows)
        .iter()
        .map(|(name, load)| lab.build_default(&format!("{name}.db"), |c| load(c)))
        .collect()
}

/// The stitch against the oracle, over the spike's shapes: rowid and `WITHOUT ROWID` tables,
/// overflow chains, an `AUTOINCREMENT` sequence, a freelist left by a delete, plain / partial /
/// composite-with-DESC / expression indexes, and `UNIQUE` autoindexes.
#[test]
fn stitched_equals_normally_built() {
    let lab = Lab::new();
    let loaders = spike_loaders(2000);
    let sources: Vec<PathBuf> = loaders
        .iter()
        .map(|(name, load)| lab.build_default(&format!("{name}.db"), |c| load(c)))
        .collect();
    let refs: Vec<&dyn Fn(&Connection)> = loaders.iter().map(|(_, l)| &**l).collect();
    let reference = lab.reference("reference.db", &SourceOptions::default(), &refs);
    let (report, out) = lab.stitch(&sources, &full()).unwrap();
    assert_eq!(report.link.trees, 5 + 10 + 2);
    assert_same_database(&out, &reference);
}

/// The oracle has teeth: a reference with one row the stitch never saw fails on the first
/// table scan …
#[test]
#[should_panic(expected = "SELECT * FROM \"t0\"")]
fn oracle_sees_a_missing_row() {
    let lab = Lab::new();
    let loaders = spike_loaders(300);
    let sources: Vec<PathBuf> = loaders
        .iter()
        .map(|(name, load)| lab.build_default(&format!("{name}.db"), |c| load(c)))
        .collect();
    let extra = |c: &Connection| {
        c.execute_batch("INSERT INTO t0 VALUES (100000, 'extra', 0.1, x'00')")
            .unwrap();
    };
    let mut refs: Vec<&dyn Fn(&Connection)> = loaders.iter().map(|(_, l)| &**l).collect();
    refs.push(&extra);
    let reference = lab.reference("reference.db", &SourceOptions::default(), &refs);
    let (_, out) = lab.stitch(&sources, &full()).unwrap();
    assert_same_database(&out, &reference);
}

/// … and a reference holding the same rows in a differently built tree — the same table loaded
/// in reverse rowid order — passes every query and fails on the page shape.
#[test]
#[should_panic(expected = "FROM dbstat")]
fn oracle_sees_a_different_tree_shape() {
    let lab = Lab::new();
    let forward = |c: &Connection| {
        c.execute_batch("BEGIN; CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        for i in 1..=3000i64 {
            c.execute(
                "INSERT INTO t VALUES (?1, ?2)",
                params![i, format!("v{i:06}")],
            )
            .unwrap();
        }
        c.execute_batch("COMMIT; CREATE INDEX t_v ON t(v);")
            .unwrap();
    };
    let backward = |c: &Connection| {
        c.execute_batch("BEGIN; CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        for i in (1..=3000i64).rev() {
            c.execute(
                "INSERT INTO t VALUES (?1, ?2)",
                params![i, format!("v{i:06}")],
            )
            .unwrap();
        }
        c.execute_batch("COMMIT; CREATE INDEX t_v ON t(v);")
            .unwrap();
    };
    let source = lab.build_default("t.db", forward);
    let reference = lab.reference("reference.db", &SourceOptions::default(), &[&backward]);
    let (_, out) = lab.stitch(std::slice::from_ref(&source), &full()).unwrap();
    assert_same_database(&out, &reference);
}

/// `Options::threads` links the sources in parallel, each into a reserved destination page
/// range (design 415 §7). The reservation is computed to be EXACT — page 1, the freelist, the
/// skeleton-owned roots and the never-copied internal trees all come off it — so the parallel
/// output must span the same pages as the sequential one, hold the same b-trees page for page,
/// and carry no freelist. A hole would show up as `integrity_check`'s "Page N is never used",
/// which `full()` runs, and as a non-zero `freelist_count`, which `assert_same_database` checks.
#[test]
fn a_parallel_link_matches_the_sequential_link_exactly() {
    let lab = Lab::new();
    let sources = spike_sources(&lab, 3000);
    let sequential = lab.path("sequential.db");
    let seq = stitch(&sources, &sequential, &full()).expect("sequential stitch");
    let parallel = lab.path("parallel.db");
    let par = stitch(
        &sources,
        &parallel,
        &Options {
            threads: 4,
            ..full()
        },
    )
    .expect("parallel stitch");

    assert_eq!(seq.link_threads, 1);
    assert_eq!(par.link_threads, 4.min(sources.len()));
    assert_eq!(par.freelist_pages, 0, "the reservation must be exact");
    assert_eq!(seq.freelist_pages, 0);
    // Same pages walked, same pages emitted: only the ORDER of allocation differs.
    assert_eq!(par.link, seq.link);
    assert_eq!(par.dest_pages, seq.dest_pages);
    assert_eq!(par.bytes, seq.bytes);
    assert_parity_all(&parallel, &sources);
    assert_same_database(&parallel, &sequential);
}

/// One source is the degenerate case for range reservation, and `threads` above the source
/// count must clamp rather than reserve empty ranges.
#[test]
fn a_parallel_link_of_one_source_clamps_to_one_thread() {
    let lab = Lab::new();
    let sources = spike_sources(&lab, 500);
    let one = std::slice::from_ref(&sources[0]);
    let out = lab.path("one.db");
    let report = stitch(
        one,
        &out,
        &Options {
            threads: 8,
            ..full()
        },
    )
    .expect("stitch one source on 8 threads");
    assert_eq!(report.link_threads, 1);
    assert_eq!(report.freelist_pages, 0);
    assert_parity_all(&out, one);
}

#[test]
fn links_the_spike_shapes_and_passes_integrity_check() {
    let lab = Lab::new();
    let sources = spike_sources(&lab, 3000);
    let (report, out) = lab.stitch(&sources, &full()).expect("stitch");
    // 5 tables + 10 named indexes (two of them expression indexes) + 2 UNIQUE autoindexes; t1's
    // WITHOUT ROWID primary key is the table's own b-tree.
    assert_eq!(report.link.trees, 5 + 10 + 2);
    assert!(
        report.link.interior > 0,
        "interior pages: {:?}",
        report.link
    );
    assert!(report.link.overflow > 0 && report.link.leaf_with_overflow > 0);
    assert!(
        report.source_freelist_pages > 0,
        "t2's delete should have left freelist pages"
    );
    assert_eq!(report.verify, Verify::Full);
    assert_eq!(journal_mode(&out), "delete");
    no_sidecars(&out);
    assert_parity_all(&out, &sources);
    // sqlite_sequence carried over.
    let conn = open(&out);
    let seq: i64 = conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 't2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seq, 3000);
    // The output is a normal database: writes work, the indexes are live.
    conn.execute("INSERT INTO t2(a, b) VALUES (1, 'after')", [])
        .unwrap();
    let id: i64 = conn
        .query_row("SELECT max(id) FROM t2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(id, 3001);
    let plan: String = conn
        .query_row("EXPLAIN QUERY PLAN SELECT * FROM t3 WHERE a = 5", [], |r| {
            r.get(3)
        })
        .unwrap();
    assert!(plan.contains("t3_a"), "{plan}");
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT * FROM t0 WHERE score > 0.5",
            [],
            |r| r.get(3),
        )
        .unwrap();
    assert!(plan.contains("t0_score_hi"), "partial index unused: {plan}");
    assert!(!lab.path("stitched.db.stitch-tmp").exists());
}

#[test]
fn quick_and_none_verification_levels() {
    let lab = Lab::new();
    let sources = spike_sources(&lab, 500);
    let (r, out) = lab
        .stitch(
            &sources,
            &Options {
                verify: Verify::Quick,
                ..Options::default()
            },
        )
        .unwrap();
    assert_eq!(r.verify, Verify::Quick);
    assert!(integrity_ok(&out));
    fs::remove_file(&out).unwrap();
    let (r, out) = lab.stitch(&sources, &Options::default()).unwrap();
    assert_eq!(r.verify, Verify::None);
    assert_eq!(r.durations.verify_ms, 0);
    assert!(integrity_ok(&out));
    assert_parity_all(&out, &sources);
}

/// Payload sizes around every bend of the local-payload rule, on the smallest, a middling, and
/// the largest page size — table leaves AND index cells (leaf and interior).
#[test]
fn page_sizes_and_overflow_boundaries() {
    for &page_size in &[512u32, 1024, 65_536] {
        let lab = Lab::new();
        let usable = page_size as usize;
        let mut sizes: Vec<usize> = vec![0, 1, 7];
        for kind in [Kind::LeafTable, Kind::LeafIndex] {
            // Find X (the largest fully-local payload) by scanning; then bracket it and M.
            let x = (1..usable)
                .rev()
                .find(|&p| local_payload(kind, p, usable) == p)
                .unwrap();
            sizes.extend([x - 1, x, x + 1, x + 2]);
        }
        sizes.extend([
            usable - 4,
            usable - 3,
            usable,
            usable + 1,
            2 * usable - 8,
            2 * usable,
            3 * usable + 17,
        ]);
        sizes.sort_unstable();
        sizes.dedup();
        let opts = SourceOptions {
            page_size: Some(page_size),
            ..SourceOptions::default()
        };
        let load_a = |c: &Connection| {
            c.execute_batch(
                "BEGIN; CREATE TABLE blobs(id INTEGER PRIMARY KEY, n INTEGER, payload BLOB);",
            )
            .unwrap();
            let mut ins = c.prepare("INSERT INTO blobs VALUES (?1, ?2, ?3)").unwrap();
            let mut id = 0i64;
            for round in 0..3 {
                for &n in &sizes {
                    id += 1;
                    ins.execute(params![id, n as i64, vec![(n % 251) as u8 ^ round; n]])
                        .unwrap();
                }
            }
            drop(ins);
            c.execute_batch("COMMIT; CREATE INDEX blobs_n ON blobs(n);")
                .unwrap();
        };
        let load_b = |c: &Connection| {
            // Keys this long overflow index cells — on leaves and on interior pages.
            c.execute_batch("BEGIN; CREATE TABLE keys(id INTEGER PRIMARY KEY, k TEXT NOT NULL);")
                .unwrap();
            let mut ins = c.prepare("INSERT INTO keys VALUES (?1, ?2)").unwrap();
            let mut id = 0i64;
            for round in 0..40 {
                for &n in &sizes {
                    id += 1;
                    let mut k = format!("{round:03}-{id:06}-");
                    k.push_str(&"k".repeat(n));
                    ins.execute(params![id, k]).unwrap();
                }
            }
            drop(ins);
            c.execute_batch("COMMIT; CREATE INDEX keys_k ON keys(k); CREATE TABLE wr(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;").unwrap();
            c.execute_batch("INSERT INTO wr SELECT k, id FROM keys;")
                .unwrap();
        };
        let a = lab.build("a.db", &opts, JournalMode::Delete, load_a);
        let b = lab.build("b.db", &opts, JournalMode::Delete, load_b);
        let reference = lab.reference("reference.db", &opts, &[&load_a, &load_b]);
        let (report, out) = lab
            .stitch(&[a.clone(), b.clone()], &full())
            .unwrap_or_else(|e| panic!("page size {page_size}: {e}"));
        assert_eq!(report.page_size, page_size);
        assert!(
            report.link.overflow > 0,
            "page size {page_size}: no overflow pages exercised"
        );
        assert!(report.link.interior > 0);
        assert_parity_all(&out, &[a, b]);
        assert_same_database(&out, &reference);
    }
}

#[test]
fn utf16_sources_and_mixed_encodings() {
    let lab = Lab::new();
    let utf16 = SourceOptions {
        encoding: Some(Encoding::Utf16le),
        ..SourceOptions::default()
    };
    let load_a = |c: &Connection| {
        c.execute_batch("CREATE TABLE a(id INTEGER PRIMARY KEY, s TEXT); INSERT INTO a VALUES (1, 'héllo'), (2, '日本語'); CREATE INDEX a_s ON a(s);").unwrap();
    };
    let load_b = |c: &Connection| {
        c.execute_batch(
            "CREATE TABLE b(id INTEGER PRIMARY KEY, s TEXT); INSERT INTO b VALUES (1, 'wörld');",
        )
        .unwrap();
    };
    let a = lab.build("a.db", &utf16, JournalMode::Delete, load_a);
    let b = lab.build("b.db", &utf16, JournalMode::Delete, load_b);
    let reference = lab.reference("reference.db", &utf16, &[&load_a, &load_b]);
    let (report, out) = lab.stitch(&[a.clone(), b.clone()], &full()).unwrap();
    assert_eq!(report.encoding, "UTF-16le");
    assert_same_database(&out, &reference);
    let enc: String = open(&out)
        .query_row("PRAGMA encoding", [], |r| r.get(0))
        .unwrap();
    assert_eq!(enc, "UTF-16le");
    assert_parity_all(&out, &[a.clone(), b]);
    let c = lab.build_default("c.db", |c| {
        c.execute_batch("CREATE TABLE c(id INTEGER PRIMARY KEY);")
            .unwrap();
    });
    let err = stitch(&[a, c], &lab.path("mixed.db"), &Options::default())
        .expect_err("mixed encodings must be refused");
    assert!(
        matches!(
            err,
            StitchError::Incompatible {
                field: "encoding",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn empty_and_tiny_tables() {
    let lab = Lab::new();
    let load_a = |c: &Connection| {
        c.execute_batch("CREATE TABLE empty(id INTEGER PRIMARY KEY, v TEXT); CREATE INDEX empty_v ON empty(v); CREATE TABLE one(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO one VALUES (1, 'x'); CREATE INDEX one_v ON one(v);").unwrap();
    };
    let load_b = |c: &Connection| {
        c.execute_batch(
            "CREATE TABLE wr(k TEXT PRIMARY KEY) WITHOUT ROWID; CREATE TABLE noidx(a, b, c);",
        )
        .unwrap();
    };
    let a = lab.build_default("a.db", load_a);
    let b = lab.build_default("b.db", load_b);
    let reference = lab.reference(
        "reference.db",
        &SourceOptions::default(),
        &[&load_a, &load_b],
    );
    let (report, out) = lab.stitch(&[a.clone(), b.clone()], &full()).unwrap();
    assert_eq!(report.link.trees, 4 + 2);
    assert_eq!(report.link.interior, 0);
    assert_eq!(report.link.leaf, 6);
    assert_parity_all(&out, &[a, b]);
    assert_same_database(&out, &reference);
    let conn = open(&out);
    conn.execute("INSERT INTO empty VALUES (1, 'now')", [])
        .unwrap();
    conn.execute("INSERT INTO wr VALUES ('k')", []).unwrap();
    assert!(integrity_ok(&out));
}

#[test]
fn views_triggers_and_foreign_keys_across_sources() {
    let lab = Lab::new();
    let load_parents = |c: &Connection| {
        c.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO parent VALUES (1, 'p1'), (2, 'p2');").unwrap();
    };
    let load_children = |c: &Connection| {
        c.execute_batch(
            "CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id), v TEXT);
             CREATE TABLE audit(id INTEGER PRIMARY KEY AUTOINCREMENT, child_id INTEGER);
             CREATE TRIGGER child_ai AFTER INSERT ON child BEGIN INSERT INTO audit(child_id) VALUES (new.id); END;
             CREATE VIEW child_v AS SELECT id, upper(v) AS V FROM child;
             INSERT INTO child VALUES (10, 1, 'a'), (11, 2, 'b');",
        ).unwrap();
    };
    let parents = lab.build_default("parents.db", load_parents);
    let children = lab.build_default("children.db", load_children);
    let reference = lab.reference(
        "reference.db",
        &SourceOptions::default(),
        &[&load_parents, &load_children],
    );
    let (_, out) = lab
        .stitch(&[parents.clone(), children.clone()], &full())
        .unwrap();
    assert_parity_all(&out, &[parents, children]);
    assert_same_database(&out, &reference);
    let conn = open(&out);
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    let violations: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    assert!(
        conn.execute("INSERT INTO child VALUES (12, 99, 'orphan')", [])
            .is_err(),
        "FK must be enforced"
    );
    conn.execute("INSERT INTO child VALUES (12, 1, 'c')", [])
        .unwrap();
    let audits: i64 = conn
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(audits, 3, "the trigger came across and fired");
    let v: String = conn
        .query_row("SELECT V FROM child_v WHERE id = 12", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, "C");
}

#[test]
fn journal_modes_leave_a_clean_file() {
    let lab = Lab::new();
    let sources = spike_sources(&lab, 200);
    let wal2 = wal2_capable(&lab);
    for (mode, expect) in [
        (JournalMode::Wal, "wal"),
        (JournalMode::Wal2, "wal2"),
        (JournalMode::Delete, "delete"),
    ] {
        if mode == JournalMode::Wal2 && !wal2 {
            continue;
        }
        let out = lab.path(&format!("out-{expect}.db"));
        let r = stitch(
            &sources,
            &out,
            &Options {
                journal_mode: mode,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(r.journal_mode, mode);
        no_sidecars(&out);
        // Bytes 18/19 say the same thing the pragma will.
        let mut hdr = [0u8; 100];
        File::open(&out)
            .unwrap()
            .read_exact_at(&mut hdr, 0)
            .unwrap();
        let expected_byte = match mode {
            JournalMode::Delete => 1,
            JournalMode::Wal => 2,
            JournalMode::Wal2 => 3,
        };
        assert_eq!((hdr[18], hdr[19]), (expected_byte, expected_byte));
        assert_eq!(journal_mode(&out), expect);
        no_sidecars(&out);
        assert!(integrity_ok(&out));
    }
}

#[test]
fn stats_copy_rows_and_analyze() {
    let lab = Lab::new();
    let a = lab.build_default("a.db", |c| {
        c.execute_batch("CREATE TABLE a(id INTEGER PRIMARY KEY, v INTEGER); INSERT INTO a WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM n WHERE v < 500) SELECT v, v % 7 FROM n; CREATE INDEX a_v ON a(v); ANALYZE;").unwrap();
    });
    let b = lab.build_default("b.db", |c| {
        c.execute_batch("CREATE TABLE b(id INTEGER PRIMARY KEY, w TEXT); INSERT INTO b WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM n WHERE v < 300) SELECT v, 'w' || (v % 3) FROM n; CREATE INDEX b_w ON b(w); ANALYZE;").unwrap();
    });
    let stat_rows = |path: &Path| -> Vec<(String, String, String)> {
        let conn = open(path);
        let mut stmt = conn
            .prepare("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    };
    let mut expected = stat_rows(&a);
    expected.extend(stat_rows(&b));
    expected.sort();
    assert!(!expected.is_empty());

    let out = lab.path("copy.db");
    let r = stitch(
        &[a.clone(), b.clone()],
        &out,
        &Options {
            stats: Stats::CopyRows,
            verify: Verify::Full,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(r.stats, Stats::CopyRows);
    assert_eq!(stat_rows(&out), expected);

    let out2 = lab.path("analyze.db");
    let r2 = stitch(
        &[a.clone(), b.clone()],
        &out2,
        &Options {
            stats: Stats::Analyze,
            verify: Verify::Full,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        stat_rows(&out2),
        expected,
        "ANALYZE over the same rows yields the same stat1 rows"
    );
    assert_published_size(&r, &out);
    assert_published_size(&r2, &out2);

    let out3 = lab.path("skip.db");
    stitch(&[a, b], &out3, &Options::default()).unwrap();
    let has_stat: i64 = open(&out3)
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_stat, 0);
}

fn simple_source(lab: &Lab, name: &str, table: &str, rows: i64) -> PathBuf {
    let table = table.to_string();
    lab.build_default(name, move |c| {
        c.execute_batch(&format!(
            "BEGIN; CREATE TABLE {table}(id INTEGER PRIMARY KEY, v TEXT);"
        ))
        .unwrap();
        let mut ins = c
            .prepare(&format!("INSERT INTO {table} VALUES (?1, ?2)"))
            .unwrap();
        for i in 0..rows {
            ins.execute(params![i + 1, format!("value-{i:08}")])
                .unwrap();
        }
        drop(ins);
        c.execute_batch(&format!("COMMIT; CREATE INDEX {table}_v ON {table}(v);"))
            .unwrap();
    })
}

#[test]
fn refuses_contract_violations() {
    let lab = Lab::new();
    let a = simple_source(&lab, "a.db", "a", 2000);
    let b = simple_source(&lab, "b.db", "b", 10);

    // No sources.
    assert!(matches!(
        stitch(
            &Vec::<PathBuf>::new(),
            &lab.path("x.db"),
            &Options::default()
        ),
        Err(StitchError::NoSources)
    ));

    // A sidecar beside a source.
    let wal = PathBuf::from(format!("{}-wal", a.display()));
    File::create(&wal).unwrap();
    let err = stitch(
        &[a.clone(), b.clone()],
        &lab.path("x.db"),
        &Options::default(),
    )
    .err()
    .unwrap();
    assert!(matches!(err, StitchError::Sidecar { .. }), "{err}");
    fs::remove_file(&wal).unwrap();

    // Page size mismatch.
    let small = lab.build(
        "small.db",
        &SourceOptions {
            page_size: Some(1024),
            ..SourceOptions::default()
        },
        JournalMode::Delete,
        |c| {
            c.execute_batch("CREATE TABLE s(id INTEGER PRIMARY KEY);")
                .unwrap();
        },
    );
    let err = stitch(&[a.clone(), small], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(
        matches!(
            err,
            StitchError::Incompatible {
                field: "page_size",
                ..
            }
        ),
        "{err}"
    );

    // auto_vacuum on (built outside the recipe, which would have refused it).
    let av = lab.path("av.db");
    {
        let c = Connection::open(&av).unwrap();
        c.execute_batch("PRAGMA auto_vacuum = FULL; CREATE TABLE av(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO av VALUES (1, 'x');").unwrap();
    }
    let err = stitch(&[a.clone(), av], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(matches!(err, StitchError::AutoVacuum { .. }), "{err}");

    // The same table in two sources.
    let a2 = simple_source(&lab, "a2.db", "a", 5);
    let err = stitch(&[a.clone(), a2], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(
        matches!(err, StitchError::DuplicateObject { ref name, .. } if name == "a"),
        "{err}"
    );

    // A virtual table (FTS5 is in the build).
    let vt = lab.build_default("vt.db", |c| {
        c.execute_batch(
            "CREATE VIRTUAL TABLE ft USING fts5(body); INSERT INTO ft VALUES ('hello world');",
        )
        .unwrap();
    });
    let err = stitch(&[vt], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(matches!(err, StitchError::Unsupported { .. }), "{err}");

    // Destination exists.
    let out = lab.path("exists.db");
    stitch(std::slice::from_ref(&a), &out, &Options::default()).unwrap();
    let err = stitch(&[a.clone(), b.clone()], &out, &Options::default())
        .err()
        .unwrap();
    assert!(matches!(err, StitchError::DestinationExists(_)), "{err}");
    let r = stitch(
        &[a.clone(), b.clone()],
        &out,
        &Options {
            overwrite: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(r.sources, 2);
    assert_parity_all(&out, &[a.clone(), b.clone()]);

    // Not SQLite at all.
    let junk = lab.path("junk.db");
    fs::write(&junk, vec![0u8; 8192]).unwrap();
    let err = stitch(&[junk], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(matches!(err, StitchError::NotSqlite { .. }), "{err}");
}

/// Flip bytes in a source and make sure the walk refuses rather than links garbage.
#[test]
fn refuses_structurally_corrupt_sources() {
    let lab = Lab::new();
    let a = simple_source(&lab, "a.db", "a", 3000);
    let page_size = 4096u64;
    let bytes = fs::read(&a).unwrap();
    let pages = bytes.len() as u64 / page_size;
    let flag_of = |p: u64| bytes[((p - 1) * page_size) as usize];
    let leaf = (2..=pages)
        .find(|&p| flag_of(p) == 0x0d)
        .expect("a table leaf");
    let interior = (2..=pages)
        .find(|&p| flag_of(p) == 0x05)
        .expect("a table interior page");

    // 1. A leaf with an impossible flag byte.
    let bad = lab.path("badflag.db");
    fs::copy(&a, &bad).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&bad)
        .unwrap()
        .write_all_at(&[0x07], (leaf - 1) * page_size)
        .unwrap();
    let err = stitch(&[bad], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(
        matches!(err, StitchError::Corrupt { page, .. } if u64::from(page) == leaf),
        "{err}"
    );

    // 2. An interior page whose right-most pointer leaves the file.
    let bad = lab.path("badptr.db");
    fs::copy(&a, &bad).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&bad)
        .unwrap()
        .write_all_at(
            &0xFFFF_FFF0u32.to_be_bytes(),
            (interior - 1) * page_size + 8,
        )
        .unwrap();
    let err = stitch(&[bad], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(
        matches!(err, StitchError::Corrupt { page, .. } if u64::from(page) == interior),
        "{err}"
    );

    // 3. An interior page whose right-most pointer points back at itself (a cycle).
    let bad = lab.path("cycle.db");
    fs::copy(&a, &bad).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&bad)
        .unwrap()
        .write_all_at(
            &(interior as u32).to_be_bytes(),
            (interior - 1) * page_size + 8,
        )
        .unwrap();
    let err = stitch(&[bad], &lab.path("x.db"), &Options::default())
        .err()
        .unwrap();
    assert!(matches!(err, StitchError::Corrupt { .. }), "{err}");
    assert!(
        !lab.path("x.db").exists() && !lab.path("x.db.stitch-tmp").exists(),
        "nothing half-linked left behind"
    );
}

#[test]
fn source_recipe_contract() {
    let lab = Lab::new();
    // seal to wal2 → header 3/3 (wal → 2/2 on a stock build), no sidecars; the linker accepts it.
    let (seal, byte) = if wal2_capable(&lab) {
        (JournalMode::Wal2, 3)
    } else {
        (JournalMode::Wal, 2)
    };
    let a = lab.build("a.db", &SourceOptions::default(), seal, |c| {
        c.execute_batch(
            "CREATE TABLE a(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO a VALUES (1, 'x');",
        )
        .unwrap();
    });
    let mut hdr = [0u8; 100];
    File::open(&a).unwrap().read_exact_at(&mut hdr, 0).unwrap();
    assert_eq!((hdr[18], hdr[19]), (byte, byte));
    no_sidecars(&a);
    let b = lab.build(
        "b.db",
        &SourceOptions {
            cache_size_kib: Some(2048),
            synchronous: stitch_sqlite::Synchronous::Normal,
            ..SourceOptions::default()
        },
        JournalMode::Wal,
        |c| {
            c.execute_batch("CREATE TABLE b(id INTEGER PRIMARY KEY);")
                .unwrap();
        },
    );
    let (_, out) = lab.stitch(&[a.clone(), b.clone()], &full()).unwrap();
    assert_parity_all(&out, &[a.clone(), b]);
    // open_source refuses a file with content.
    let err = open_source(&a, &SourceOptions::default()).err().unwrap();
    assert!(matches!(err, StitchError::SourceNotFresh(_)), "{err}");
    // … and one with a sidecar.
    let fresh = lab.path("fresh.db");
    File::create(format!("{}-journal", fresh.display())).unwrap();
    let err = open_source(&fresh, &SourceOptions::default())
        .err()
        .unwrap();
    assert!(matches!(err, StitchError::Sidecar { .. }), "{err}");
}

#[test]
fn cli_round_trip() {
    let lab = Lab::new();
    let a = simple_source(&lab, "a.db", "a", 100);
    let b = simple_source(&lab, "b.db", "b", 100);
    let out = lab.path("cli.db");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    assert_eq!(
        cli::run(&[
            "--verify".into(),
            "full".into(),
            "--json".into(),
            "--quiet".into(),
            s(&out),
            s(&a),
            s(&b)
        ]),
        0
    );
    assert!(out.exists());
    assert_parity_all(&out, &[a.clone(), b.clone()]);
    assert_eq!(
        cli::run(&[s(&out), s(&a)]),
        1,
        "destination exists without --force"
    );
    let mode = if wal2_capable(&lab) { "wal2" } else { "wal" };
    assert_eq!(
        cli::run(&[
            "--force".into(),
            "--journal-mode".into(),
            mode.into(),
            s(&out),
            s(&a),
            s(&b)
        ]),
        0
    );
    assert_eq!(journal_mode(&out), mode);
    assert_eq!(cli::run(&[]), 2);
    assert_eq!(cli::run(&[s(&out)]), 2);
    assert_eq!(
        cli::run(&["--verify".into(), "loud".into(), s(&out), s(&a)]),
        2
    );
    assert_eq!(cli::run(&["--bogus".into(), s(&out), s(&a)]), 2);
    assert_eq!(cli::run(&["--help".into()]), 0);
}

/// The > 1 GiB lane (design 415 §1, the lock-byte page). Local-only: `./orient.sh tests` →
/// `stitch-lockbyte`, or `cargo test --release --test differential lock_byte_page -- --ignored`.
#[test]
#[ignore = "writes ~2.5 GiB under the temp dir; see infra/tests/tests.mjs `stitch-lockbyte`"]
fn lock_byte_page() {
    let lab = Lab::new();
    // 4000-byte payloads: one row per 4 KiB leaf, no overflow, ~1.07 GiB for 275 000 rows.
    let big = lab.build_default("big.db", |c| {
        c.execute_batch("BEGIN; CREATE TABLE big(id INTEGER PRIMARY KEY, payload BLOB);")
            .unwrap();
        let mut ins = c.prepare("INSERT INTO big VALUES (?1, ?2)").unwrap();
        let payload = vec![0x5Au8; 4000];
        for i in 0..275_000i64 {
            ins.execute(params![i + 1, &payload]).unwrap();
        }
        drop(ins);
        c.execute_batch("COMMIT;").unwrap();
    });
    let small = simple_source(&lab, "small.db", "small", 1000);
    let (report, out) = lab.stitch(&[big.clone(), small.clone()], &full()).unwrap();
    assert!(report.link.lock_page_skipped, "{:?}", report.link);
    assert!(report.dest_pages > 262_145);
    // The lock-byte page itself is zero.
    let mut page = vec![0u8; 4096];
    File::open(&out)
        .unwrap()
        .read_exact_at(&mut page, 262_144 * 4096)
        .unwrap();
    assert!(page.iter().all(|&b| b == 0));
    let n: i64 = open(&out)
        .query_row("SELECT count(*) FROM big", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 275_000);
    assert_parity(&out, &small);
}

/// `?`, `#`, and `%` in a source path are URI syntax to SQLite; the schema must come from the
/// same file the header and the pages do.
#[test]
fn source_paths_with_uri_syntax_open_the_right_file() {
    let lab = Lab::new();
    // Unencoded, `file:<dir>/q?a#b%25.db?immutable=1` would open `<dir>/q` — put a decoy there.
    lab.build_default("q", |c| {
        c.execute_batch(
            "CREATE TABLE decoy(id INTEGER PRIMARY KEY); INSERT INTO decoy VALUES (1);",
        )
        .unwrap();
    });
    let weird = lab.build_default("q?a#b%25.db", |c| {
        c.execute_batch(
            "CREATE TABLE weird(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO weird VALUES (1, 'x'), (2, 'y'); CREATE INDEX weird_v ON weird(v);",
        )
        .unwrap();
    });
    let (report, out) = lab.stitch(std::slice::from_ref(&weird), &full()).unwrap();
    assert_eq!(report.link.trees, 2);
    assert_eq!(user_tables(&out), ["weird"]);
    assert_parity(&out, &weird);
}

/// The destination, its temporary file, and their sidecars are what a stitch deletes; a source
/// at any of those paths is refused before anything is removed.
#[test]
fn refuses_a_source_the_stitch_would_replace() {
    let lab = Lab::new();
    let a = simple_source(&lab, "a.db", "a", 50);
    // A source under the temporary name.
    let tmp_named = simple_source(&lab, "out.db.stitch-tmp", "t", 50);
    let before = fs::read(&tmp_named).unwrap();
    let err = stitch(
        &[a.clone(), tmp_named.clone()],
        &lab.path("out.db"),
        &Options::default(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(err, StitchError::SourceIsDestination { ref path, .. } if *path == tmp_named),
        "{err}"
    );
    assert_eq!(
        fs::read(&tmp_named).unwrap(),
        before,
        "the source was touched"
    );
    assert!(!lab.path("out.db").exists());
    // A source as the destination, with overwrite on.
    let err = stitch(
        std::slice::from_ref(&a),
        &a,
        &Options {
            overwrite: true,
            ..Options::default()
        },
    )
    .err()
    .unwrap();
    assert!(
        matches!(err, StitchError::SourceIsDestination { .. }),
        "{err}"
    );
    assert_parity(&a, &a);
    // … and through a different spelling of the same file.
    let via_dotdot = lab.dir.path().join("sub/../a.db");
    fs::create_dir(lab.path("sub")).unwrap();
    let err = stitch(
        std::slice::from_ref(&a),
        &via_dotdot,
        &Options {
            overwrite: true,
            ..Options::default()
        },
    )
    .err()
    .unwrap();
    assert!(
        matches!(err, StitchError::SourceIsDestination { .. }),
        "{err}"
    );
}

/// A source in a legacy schema format (1–3) would be misread under the skeleton's format 4 — DESC
/// on an index means the opposite thing — so it is refused against the skeleton, not just
/// against the other sources.
#[test]
fn refuses_a_legacy_schema_format_source() {
    let lab = Lab::new();
    let legacy = lab.build_default("legacy.db", |c| {
        // The `legacy_file_format` pragma is gone from current SQLite; the dbconfig remains.
        c.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_LEGACY_FILE_FORMAT,
            true,
        )
        .unwrap();
        c.execute_batch(
            "CREATE TABLE l(id INTEGER PRIMARY KEY, v INTEGER); INSERT INTO l VALUES (1, 3), (2, 1), (3, 2); CREATE INDEX l_v ON l(v DESC);",
        )
        .unwrap();
    });
    let mut hdr = [0u8; 100];
    File::open(&legacy)
        .unwrap()
        .read_exact_at(&mut hdr, 0)
        .unwrap();
    assert_eq!(
        u32::from_be_bytes([hdr[44], hdr[45], hdr[46], hdr[47]]),
        1,
        "not a format-1 file"
    );
    let out = lab.path("out.db");
    let err = stitch(std::slice::from_ref(&legacy), &out, &full())
        .err()
        .unwrap();
    assert!(
        matches!(
            err,
            StitchError::Incompatible {
                field: "schema format",
                ..
            }
        ),
        "{err}"
    );
    assert!(!out.exists() && !lab.path("out.db.stitch-tmp").exists());
}

/// `Options::register` reaches the skeleton (so the expression index recreates), the finish
/// pass, and the verification connection (`integrity_check` evaluates expression indexes).
#[test]
fn registered_functions_reach_every_connection() {
    use rusqlite::functions::FunctionFlags;
    fn register(conn: &Connection) -> rusqlite::Result<()> {
        conn.create_scalar_function(
            "twice",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| Ok(ctx.get::<i64>(0)? * 2),
        )
    }
    let lab = Lab::new();
    let a = lab.build_default("a.db", |c| {
        register(c).unwrap();
        c.execute_batch(
            "CREATE TABLE a(id INTEGER PRIMARY KEY, n INTEGER); INSERT INTO a VALUES (1, 10), (2, 20), (3, 30); CREATE INDEX a_twice ON a(twice(n));",
        )
        .unwrap();
    });
    let out = lab.path("out.db");
    let err = stitch(std::slice::from_ref(&a), &out, &full())
        .expect_err("the skeleton cannot recreate the index without the function");
    assert!(matches!(err, StitchError::Sqlite { .. }), "{err}");
    for verify in [Verify::Quick, Verify::Full] {
        let out = lab.path(&format!("out-{verify:?}.db"));
        stitch(
            std::slice::from_ref(&a),
            &out,
            &Options {
                verify,
                register: Some(Box::new(register)),
                ..Options::default()
            },
        )
        .unwrap_or_else(|e| panic!("{verify:?}: {e}"));
        let conn = open(&out);
        register(&conn).unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM a WHERE twice(n) = 40",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("a_twice"), "{plan}");
        assert!(integrity_ok_with(&out, register));
    }
}

fn integrity_ok_with(path: &Path, register: fn(&Connection) -> rusqlite::Result<()>) -> bool {
    let conn = open(path);
    register(&conn).unwrap();
    let rows: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows == ["ok"]
}

/// The linker never reads a `CREATE INDEX` statement: a partial index's predicate can hide
/// behind comments, unicode identifiers, and quoting, and a `WITHOUT ROWID` table can carry
/// one, and the stitch neither cares nor probes it by SQL — the page-level probe covers it, and
/// the planner uses it afterwards.
#[test]
fn partial_indexes_need_no_predicate_from_the_linker() {
    let lab = Lab::new();
    let load_a = |c: &Connection| {
        c.execute_batch(
            "CREATE TABLE \"caf\u{e9}\"(id INTEGER PRIMARY KEY, x INTEGER, \"where\" TEXT);
             INSERT INTO \"caf\u{e9}\" VALUES (1, NULL, 'a'), (2, 5, 'b'), (3, 9, 'c');
             CREATE INDEX \u{ed}ndice ON \"caf\u{e9}\"(x) /* WHERE note */ WHERE x IS NOT NULL;
             CREATE INDEX w ON \"caf\u{e9}\"(\"where\") -- WHERE in a line comment
                 WHERE \"where\" > 'a';
             CREATE INDEX e ON \"caf\u{e9}\"((CASE WHEN x > 6 THEN 1 ELSE 0 END)) where x IS NOT NULL;",
        )
        .unwrap();
    };
    let load_b = |c: &Connection| {
        c.execute_batch(
            "CREATE TABLE wr(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;
             INSERT INTO wr VALUES ('a', 1), ('b', NULL), ('c', 3);
             CREATE INDEX wr_v ON wr(v) WHERE v IS NOT NULL;",
        )
        .unwrap();
    };
    let a = lab.build_default("a.db", load_a);
    let b = lab.build_default("b.db", load_b);
    let reference = lab.reference(
        "reference.db",
        &SourceOptions::default(),
        &[&load_a, &load_b],
    );
    let (report, out) = lab.stitch(&[a.clone(), b.clone()], &full()).unwrap();
    assert_eq!(report.link.trees, 2 + 4);
    assert_parity_all(&out, &[a, b]);
    assert_same_database(&out, &reference);
    let conn = open(&out);
    for (sql, index) in [
        (
            "SELECT * FROM \"caf\u{e9}\" WHERE x IS NOT NULL AND x = 5",
            "\u{ed}ndice",
        ),
        (
            "SELECT * FROM \"caf\u{e9}\" WHERE \"where\" > 'a' AND \"where\" = 'b'",
            "w",
        ),
        ("SELECT * FROM wr WHERE v IS NOT NULL AND v = 3", "wr_v"),
    ] {
        let plan: String = conn
            .query_row(&format!("EXPLAIN QUERY PLAN {sql}"), [], |r| r.get(3))
            .unwrap();
        assert!(plan.contains(index), "{sql}: {plan}");
    }
}
