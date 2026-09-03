//! What is checked after the link (design 415 §5.1): the always-on smoke probe, and the opt-in
//! `quick_check` / `integrity_check` passes over the whole output.
//!
//! The smoke probe has two halves. The **page-level** half descends the leftmost path of every
//! linked b-tree, root to leaf, through the file: every page on the path carries a b-tree flag
//! of the root's family, interior until the last, and every child pointer lands inside the
//! output. The **SQL** half asks SQLite to open each tree: one `SELECT … LIMIT 1` per table and
//! one `INDEXED BY` probe per index. A partial index is the one tree SQL cannot name without its
//! predicate, and the predicate lives only in the `CREATE INDEX` text; the linker does not parse
//! SQL, so partial indexes (found through `PRAGMA index_list`) get the page-level half alone.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use rusqlite::{params, Connection};

use crate::error::{io_err, sql_err, StitchError};
use crate::header::be32;
use crate::page::{be16, Kind};
use crate::source::Source;

const STAGE: &str = "smoke probe";

fn fail(stage: &'static str, detail: impl Into<String>) -> StitchError {
    StitchError::Verification {
        stage,
        detail: detail.into(),
    }
}

pub(crate) fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// O(trees × depth): the file reopens, the journal mode is what was asked, every expected
/// object is in `sqlite_schema`, the leftmost path of every tree is well-formed, and SQLite
/// opens every tree it can name.
pub(crate) fn smoke(
    dest: &Path,
    conn: &Connection,
    sources: &[Source],
    roots: &HashMap<String, u32>,
    page_size: u32,
    usable: usize,
    expected_mode: &str,
) -> Result<(), StitchError> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(sql_err("PRAGMA journal_mode"))?;
    if mode != expected_mode {
        return Err(fail(
            STAGE,
            format!("journal_mode is {mode:?}, expected {expected_mode:?}"),
        ));
    }
    let file = File::open(dest).map_err(io_err("open destination for the page probe"))?;
    let len = file
        .metadata()
        .map_err(io_err("stat destination for the page probe"))?
        .len();
    let pages = (len / u64::from(page_size)) as u32;
    let mut buf = vec![0u8; page_size as usize];
    for s in sources {
        for row in s.schema.iter().filter(|r| r.is_linked_tree()) {
            let &root = roots.get(&row.name).ok_or_else(|| StitchError::NoRoot {
                name: row.name.clone(),
            })?;
            descend(
                &file,
                &mut buf,
                &row.name,
                root,
                pages,
                usable,
                row.kind == "index",
            )?;
        }
    }
    let partial = partial_indexes(conn, sources)?;
    for s in sources {
        for row in s.schema.iter().filter(|r| r.is_expected_object()) {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
                    params![row.name],
                    |r| r.get(0),
                )
                .map_err(sql_err("count schema object"))?;
            if n != 1 {
                return Err(fail(
                    STAGE,
                    format!("{:?} appears {n} times in sqlite_schema", row.name),
                ));
            }
            let probe = match row.kind.as_str() {
                "table" => format!("SELECT 1 FROM {} LIMIT 1", quote(&row.name)),
                // The planner may use a partial index only under a WHERE that implies the
                // index's own; without the predicate, `INDEXED BY` has no query solution.
                "index" if partial.contains(&row.name) => continue,
                "index" => format!(
                    "SELECT 1 FROM {} INDEXED BY {} LIMIT 1",
                    quote(&row.tbl_name),
                    quote(&row.name)
                ),
                _ => continue,
            };
            let mut stmt = conn
                .prepare(&probe)
                .map_err(|e| fail(STAGE, format!("{:?}: {probe}: {e}", row.name)))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| fail(STAGE, format!("{:?}: {probe}: {e}", row.name)))?;
            rows.next()
                .map_err(|e| fail(STAGE, format!("{:?}: {probe}: {e}", row.name)))?;
        }
    }
    Ok(())
}

/// Root to leftmost leaf of one tree, through the file. `must_be_index` holds for a schema
/// `index`; a `table` may be either family, since a `WITHOUT ROWID` table is an index b-tree.
fn descend(
    file: &File,
    buf: &mut [u8],
    name: &str,
    root: u32,
    pages: u32,
    usable: usize,
    must_be_index: bool,
) -> Result<(), StitchError> {
    let page_size = buf.len() as u64;
    let mut pgno = root;
    let mut family: Option<bool> = None;
    let mut depth = 0u32;
    loop {
        if pgno < 2 || pgno > pages {
            return Err(fail(
                STAGE,
                format!("{name:?}: page {pgno} lies outside the output's 2..={pages}"),
            ));
        }
        depth += 1;
        if depth > pages {
            return Err(fail(
                STAGE,
                format!("{name:?}: the leftmost path never reaches a leaf (cycle)"),
            ));
        }
        file.read_exact_at(buf, (u64::from(pgno) - 1) * page_size)
            .map_err(io_err(format!("read page {pgno} of {name:?}")))?;
        let kind = Kind::from_flag(buf[0]).ok_or_else(|| {
            fail(
                STAGE,
                format!("{name:?}: page {pgno} has b-tree flag {:#04x}", buf[0]),
            )
        })?;
        match family {
            None if must_be_index && !kind.is_index() => {
                return Err(fail(
                    STAGE,
                    format!("{name:?}: root page {pgno} is a table page, not an index page"),
                ));
            }
            None => family = Some(kind.is_index()),
            Some(is_index) if is_index != kind.is_index() => {
                return Err(fail(
                    STAGE,
                    format!("{name:?}: page {pgno} changes b-tree family on the way down"),
                ));
            }
            Some(_) => {}
        }
        if !kind.is_interior() {
            return Ok(());
        }
        // The leftmost child: the first cell's child pointer, or the right-most pointer on an
        // interior page with no cells.
        let ncell = be16(buf, 3);
        pgno = if ncell == 0 {
            be32(buf, 8)
        } else {
            let off = be16(buf, kind.header_len());
            if off + 4 > usable {
                return Err(fail(
                    STAGE,
                    format!("{name:?}: page {pgno}: the first cell lies outside the usable area"),
                ));
            }
            be32(buf, off)
        };
    }
}

/// Every partial index in the output, by name, from `PRAGMA index_list` on each table — the
/// skeleton's SQLite parsed the `CREATE INDEX`, so it is the one that knows.
fn partial_indexes(conn: &Connection, sources: &[Source]) -> Result<HashSet<String>, StitchError> {
    let mut partial = HashSet::new();
    for s in sources {
        for table in s
            .schema
            .iter()
            .filter(|r| r.kind == "table" && !r.is_internal())
        {
            let pragma = format!("PRAGMA index_list({})", quote(&table.name));
            let mut stmt = conn.prepare(&pragma).map_err(sql_err(pragma.clone()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>("name")?, r.get::<_, i64>("partial")?))
                })
                .map_err(sql_err(pragma.clone()))?;
            for row in rows {
                let (name, is_partial) = row.map_err(sql_err(pragma.clone()))?;
                if is_partial != 0 {
                    partial.insert(name);
                }
            }
        }
    }
    Ok(partial)
}

fn pragma_rows(conn: &Connection, pragma: &str) -> Result<Vec<String>, StitchError> {
    conn.prepare(pragma)
        .map_err(sql_err(pragma.to_string()))?
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql_err(pragma.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err(pragma.to_string()))
}

/// `PRAGMA quick_check`: every page read, index↔table consistency skipped.
pub(crate) fn quick(conn: &Connection) -> Result<(), StitchError> {
    let rows = pragma_rows(conn, "PRAGMA quick_check")?;
    if rows != ["ok"] {
        return Err(fail("quick_check", rows.join("; ")));
    }
    Ok(())
}

/// `PRAGMA integrity_check`: the full pass, indexes included.
pub(crate) fn full(conn: &Connection) -> Result<(), StitchError> {
    let rows = pragma_rows(conn, "PRAGMA integrity_check")?;
    if rows != ["ok"] {
        return Err(fail("integrity_check", rows.join("; ")));
    }
    Ok(())
}
