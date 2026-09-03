//! The destination skeleton (design 415 §3.1): SQLite itself lays out page 1, the schema table,
//! and one empty root page per b-tree. The linker reads the roots back; it never chooses one.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;

use crate::error::{sql_err, StitchError};
use crate::source::Source;
use crate::RegisterFn;

pub(crate) struct Skeleton {
    /// `sqlite_schema.name → rootpage` for every b-tree the skeleton owns.
    pub roots: HashMap<String, u32>,
}

/// Refuse two sources that define one object before anything is written.
pub(crate) fn check_duplicates(sources: &[Source]) -> Result<(), StitchError> {
    let mut seen: HashMap<&str, &Path> = HashMap::new();
    for s in sources {
        for row in s.schema.iter().filter(|r| r.is_expected_object()) {
            if let Some(first) = seen.insert(&row.name, &s.path) {
                return Err(StitchError::DuplicateObject {
                    name: row.name.clone(),
                    first: first.to_path_buf(),
                    second: s.path.clone(),
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn create(
    dest: &Path,
    sources: &[Source],
    register: Option<&RegisterFn>,
) -> Result<Skeleton, StitchError> {
    let template = &sources[0].header;
    let conn = Connection::open(dest)
        .map_err(sql_err(format!("create destination {}", dest.display())))?;
    if let Some(register) = register {
        register(&conn).map_err(sql_err("register SQL functions on the destination"))?;
    }
    let encoding = match template.encoding {
        1 => "UTF-8",
        2 => "UTF-16le",
        3 => "UTF-16be",
        _ => unreachable!("Header::parse admits 1..=3"),
    };
    // Page size and encoding must precede the first write; the rest is the §2 contract.
    conn.execute_batch(&format!(
        "PRAGMA page_size = {}; PRAGMA encoding = '{encoding}'; PRAGMA auto_vacuum = NONE; \
         PRAGMA journal_mode = DELETE; PRAGMA synchronous = OFF;",
        template.page_size
    ))
    .map_err(sql_err("destination pragmas"))?;
    conn.execute_batch("BEGIN")
        .map_err(sql_err("begin skeleton"))?;
    // Tables first (their autoindexes come with them), then indexes, then views (which may
    // reference tables from any source), then triggers. Internal tables are re-derived, not
    // recreated; rows without `sql` are autoindexes the table already made.
    for kind in ["table", "index", "view", "trigger"] {
        for s in sources {
            for row in s
                .schema
                .iter()
                .filter(|r| r.kind == kind && !r.is_internal())
            {
                if let Some(sql) = &row.sql {
                    conn.execute_batch(sql).map_err(sql_err(format!(
                        "recreate {} {:?} from {}",
                        row.kind,
                        row.name,
                        s.path.display()
                    )))?;
                }
            }
        }
    }
    conn.execute_batch("COMMIT")
        .map_err(sql_err("commit skeleton"))?;
    let roots = conn
        .prepare("SELECT name, rootpage FROM sqlite_schema WHERE rootpage > 0")
        .map_err(sql_err("prepare roots"))?
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
        })
        .map_err(sql_err("query roots"))?
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(sql_err("read roots"))?;
    for s in sources {
        for row in s.schema.iter().filter(|r| r.is_linked_tree()) {
            if !roots.contains_key(&row.name) {
                return Err(StitchError::NoRoot {
                    name: row.name.clone(),
                });
            }
        }
    }
    Ok(Skeleton { roots })
}
