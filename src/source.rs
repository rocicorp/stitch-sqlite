//! A source file: its header, its page count, and what its `sqlite_schema` says — read once, up
//! front, before any page is touched.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

use crate::error::{io_err, sql_err, StitchError};
use crate::header::Header;

/// One row of `sqlite_schema`.
#[derive(Debug, Clone)]
pub struct SchemaRow {
    /// `table`, `index`, `view`, or `trigger`.
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    /// `0` for views, triggers, and virtual tables.
    pub rootpage: u32,
    /// `None` for the `sqlite_autoindex_*` b-trees a `UNIQUE`/`PRIMARY KEY` constraint creates.
    pub sql: Option<String>,
}

impl SchemaRow {
    /// Bookkeeping SQLite owns. Their PAGES are never linked; their ROWS are re-derived by the
    /// post-link SQL pass (`sqlite_sequence` always, `sqlite_stat*` on request).
    pub fn is_internal(&self) -> bool {
        is_internal_name(&self.name)
    }

    /// A b-tree the linker moves: a table or index with a root page, not internal.
    pub fn is_linked_tree(&self) -> bool {
        self.rootpage > 1 && !self.is_internal()
    }

    /// Everything the destination skeleton must end up containing under this name.
    pub fn is_expected_object(&self) -> bool {
        !self.is_internal() && (self.sql.is_some() || self.rootpage > 0)
    }
}

pub(crate) fn is_internal_name(name: &str) -> bool {
    name == "sqlite_sequence" || name.starts_with("sqlite_stat")
}

/// A row of `sqlite_stat1` / `sqlite_stat4`, copied verbatim under `Stats::CopyRows`.
#[derive(Debug, Clone)]
pub struct StatRow {
    pub table: &'static str,
    pub values: Vec<Value>,
}

/// A source database, inspected and ready to link.
#[derive(Debug)]
pub struct Source {
    pub path: PathBuf,
    pub header: Header,
    /// Pages in the file — from the header when it is trusted, else from the file length.
    pub page_count: u32,
    pub schema: Vec<SchemaRow>,
    /// `sqlite_sequence` rows, merged into the destination by SQL.
    pub sequences: Vec<(String, i64)>,
    pub stats: Vec<StatRow>,
}

pub(crate) const SIDECARS: [&str; 4] = ["-wal", "-wal2", "-shm", "-journal"];

pub(crate) fn sidecar_paths(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    SIDECARS
        .iter()
        .map(move |s| PathBuf::from(format!("{}{s}", path.display())))
}

/// The one way the linker reads a closed source through SQLite: `immutable=1` takes no locks
/// and opens no `-shm`, which is correct only because a source with a sidecar was already
/// refused.
pub(crate) fn open_immutable(path: &Path) -> Result<Connection, StitchError> {
    let uri = immutable_uri(path)?;
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_err(format!("open {} read-only", path.display())))
}

/// `file://<absolute path>?immutable=1`, with the three bytes SQLite's URI parser treats
/// specially — `%`, `?`, `#` — percent-encoded, so a source named `a?b.db` opens `a?b.db` and
/// not `a` with a parameter `b.db`. The path goes absolute first: after `file:`, a `//` names an
/// authority, and an empty one is the only spelling that cannot be misread.
fn immutable_uri(path: &Path) -> Result<OsString, StitchError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(io_err("resolve the working directory"))?
            .join(path)
    };
    let mut uri = b"file://".to_vec();
    for &b in abs.as_os_str().as_bytes() {
        match b {
            b'%' => uri.extend_from_slice(b"%25"),
            b'?' => uri.extend_from_slice(b"%3F"),
            b'#' => uri.extend_from_slice(b"%23"),
            _ => uri.push(b),
        }
    }
    uri.extend_from_slice(b"?immutable=1");
    Ok(OsString::from_vec(uri))
}

pub(crate) fn inspect(path: &Path) -> Result<Source, StitchError> {
    for sidecar in sidecar_paths(path) {
        if sidecar.exists() {
            return Err(StitchError::Sidecar {
                path: path.to_path_buf(),
                sidecar,
            });
        }
    }
    let file = File::open(path).map_err(io_err(format!("open {}", path.display())))?;
    let len = file
        .metadata()
        .map_err(io_err(format!("stat {}", path.display())))?
        .len();
    let mut first = [0u8; 100];
    file.read_exact_at(&mut first, 0)
        .map_err(|_| StitchError::NotSqlite {
            path: path.to_path_buf(),
            why: format!("only {len} bytes long"),
        })?;
    let header = Header::parse(path, &first)?;
    let page_size = u64::from(header.page_size);
    if len % page_size != 0 {
        return Err(StitchError::NotSqlite {
            path: path.to_path_buf(),
            why: format!("length {len} is not a multiple of the page size {page_size}"),
        });
    }
    let file_pages = (len / page_size) as u32;
    let page_count = if header.page_count_is_valid() && header.page_count != 0 {
        if header.page_count > file_pages {
            return Err(StitchError::Corrupt {
                path: path.to_path_buf(),
                page: 1,
                why: format!(
                    "header says {} pages but the file holds {file_pages}",
                    header.page_count
                ),
            });
        }
        header.page_count
    } else {
        file_pages
    };
    if header.largest_root != 0 || header.incremental_vacuum != 0 {
        return Err(StitchError::AutoVacuum {
            path: path.to_path_buf(),
        });
    }

    // A stock SQLite (bytes 18/19 ≤ 2) refuses a wal2-format file with SQLITE_NOTADB; say why.
    let conn = open_immutable(path).map_err(|e| {
        if header.read_format == 3 {
            StitchError::Unsupported {
                path: path.to_path_buf(),
                name: String::new(),
                why: format!(
                    "the source is in wal2 format (header bytes 18/19 = 3) and the SQLite this \
                     linker was built with cannot read it ({e}); seal sources with journal_mode \
                     delete or wal, or build the linker against a wal2-capable SQLite such as the \
                     rindle workspace's vendored one"
                ),
            }
        } else {
            e
        }
    })?;
    let schema = conn
        .prepare("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema ORDER BY rowid")
        .map_err(sql_err("prepare sqlite_schema"))?
        .query_map([], |r| {
            Ok(SchemaRow {
                kind: r.get(0)?,
                name: r.get(1)?,
                tbl_name: r.get(2)?,
                rootpage: r.get::<_, i64>(3)?.max(0) as u32,
                sql: r.get(4)?,
            })
        })
        .map_err(sql_err("query sqlite_schema"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err(format!("read sqlite_schema of {}", path.display())))?;
    for row in &schema {
        // fileformat.html §2.6: a `table` row with no root page is a virtual table.
        if row.kind == "table" && row.rootpage == 0 {
            return Err(StitchError::Unsupported {
                path: path.to_path_buf(),
                name: row.name.clone(),
                why: "virtual tables are not linked in v1 (design 415 §8)".into(),
            });
        }
        if row.rootpage > page_count {
            return Err(StitchError::Corrupt {
                path: path.to_path_buf(),
                page: row.rootpage,
                why: format!(
                    "root of {:?} lies beyond the file's {page_count} pages",
                    row.name
                ),
            });
        }
    }
    let has = |name: &str| schema.iter().any(|r| r.name == name);
    let sequences = if has("sqlite_sequence") {
        conn.prepare("SELECT name, seq FROM sqlite_sequence")
            .map_err(sql_err("prepare sqlite_sequence"))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql_err("query sqlite_sequence"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err("read sqlite_sequence"))?
    } else {
        Vec::new()
    };
    let mut stats = Vec::new();
    for table in ["sqlite_stat1", "sqlite_stat4"] {
        if !has(table) {
            continue;
        }
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table}"))
            .map_err(sql_err(format!("prepare {table}")))?;
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..n)
                    .map(|i| r.get::<_, Value>(i))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(sql_err(format!("query {table}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err(format!("read {table}")))?;
        stats.extend(rows.into_iter().map(|values| StatRow { table, values }));
    }
    Ok(Source {
        path: path.to_path_buf(),
        header,
        page_count,
        schema,
        sequences,
        stats,
    })
}

/// Every source must share the first source's file-format parameters (design 415 §2).
pub(crate) fn check_compatible(first: &Source, other: &Source) -> Result<(), StitchError> {
    let (a, b) = (&first.header, &other.header);
    let mismatch = |field: &'static str, value: String, expected: String| {
        Err(StitchError::Incompatible {
            path: other.path.clone(),
            field,
            value,
            expected,
        })
    };
    if a.page_size != b.page_size {
        return mismatch(
            "page_size",
            b.page_size.to_string(),
            a.page_size.to_string(),
        );
    }
    if a.reserved != b.reserved {
        return mismatch(
            "reserved bytes",
            b.reserved.to_string(),
            a.reserved.to_string(),
        );
    }
    if a.encoding != b.encoding {
        return mismatch(
            "encoding",
            b.encoding_name().to_string(),
            a.encoding_name().to_string(),
        );
    }
    if a.schema_format != b.schema_format {
        return mismatch(
            "schema format",
            b.schema_format.to_string(),
            a.schema_format.to_string(),
        );
    }
    Ok(())
}
