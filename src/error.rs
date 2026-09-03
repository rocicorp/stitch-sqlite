//! The error taxonomy (design 415 §5). Every variant names the file and, where there is one, the
//! page — a linker that says "corrupt" without saying where is a linker nobody can debug.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Why a stitch was refused or failed. The linker **refuses** on any contract violation rather
/// than degrading (design 415 §2): none of these is recoverable by retrying with the same inputs.
#[derive(Debug)]
pub enum StitchError {
    /// The file is not a SQLite 3 database, or its header is not one the linker understands.
    NotSqlite {
        path: PathBuf,
        why: String,
    },
    /// A file-format parameter differs from the first source's (page size, reserved bytes,
    /// text encoding, schema format) — the §2 contract is about these header values.
    Incompatible {
        path: PathBuf,
        field: &'static str,
        value: String,
        expected: String,
    },
    /// A `-wal` / `-wal2` / `-shm` / `-journal` file sits beside the source: it was not closed
    /// clean, and its main file may be stale behind the sidecar. The linker never recovers
    /// someone else's journal.
    Sidecar {
        path: PathBuf,
        sidecar: PathBuf,
    },
    /// The source has `auto_vacuum` on (pointer-map pages, root-placement rules).
    AutoVacuum {
        path: PathBuf,
    },
    /// A source b-tree is structurally broken: bad page flag, out-of-range page number, a cell
    /// or pointer outside the usable area, a cycle. Refused before a byte reaches the output.
    Corrupt {
        path: PathBuf,
        page: u32,
        why: String,
    },
    /// Two sources define the same table, index, view, or trigger.
    DuplicateObject {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    /// A schema object the linker deliberately does not handle in v1 (virtual tables).
    Unsupported {
        path: PathBuf,
        name: String,
        why: String,
    },
    /// The destination skeleton has no root page for a source b-tree — the skeleton's
    /// `CREATE` statements and the source's `sqlite_schema` disagree.
    NoRoot {
        name: String,
    },
    /// `stitch` was called with no sources.
    NoSources,
    /// A source is the destination, its temporary file, or a sidecar of either — paths the
    /// stitch deletes. A source is never written.
    SourceIsDestination {
        path: PathBuf,
        target: PathBuf,
    },
    /// The destination already exists and `Options::overwrite` is off.
    DestinationExists(PathBuf),
    /// `open_source` was asked to open a file that already has content.
    SourceNotFresh(PathBuf),
    /// A post-link check failed: the smoke probe, `quick_check`, or `integrity_check`.
    Verification {
        stage: &'static str,
        detail: String,
    },
    Io {
        context: String,
        source: io::Error,
    },
    Sqlite {
        context: String,
        source: rusqlite::Error,
    },
}

impl fmt::Display for StitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StitchError::NotSqlite { path, why } => {
                write!(f, "{}: not a SQLite database: {why}", path.display())
            }
            StitchError::Incompatible {
                path,
                field,
                value,
                expected,
            } => write!(
                f,
                "{}: {field} is {value}, but the first source has {expected} — every source must \
                 share the file-format parameters (design 415 §2)",
                path.display()
            ),
            StitchError::Sidecar { path, sidecar } => write!(
                f,
                "{}: sidecar {} exists — the source must be checkpointed and closed before linking",
                path.display(),
                sidecar.display()
            ),
            StitchError::AutoVacuum { path } => write!(
                f,
                "{}: auto_vacuum is on; sources must be built with auto_vacuum = NONE",
                path.display()
            ),
            StitchError::Corrupt { path, page, why } => {
                write!(f, "{}: page {page}: {why}", path.display())
            }
            StitchError::DuplicateObject {
                name,
                first,
                second,
            } => write!(
                f,
                "object {name:?} is defined by both {} and {}",
                first.display(),
                second.display()
            ),
            StitchError::Unsupported { path, name, why } if name.is_empty() => {
                write!(f, "{}: {why}", path.display())
            }
            StitchError::Unsupported { path, name, why } => {
                write!(f, "{}: {name:?}: {why}", path.display())
            }
            StitchError::NoRoot { name } => {
                write!(f, "destination skeleton has no root page for {name:?}")
            }
            StitchError::NoSources => write!(f, "no source databases given"),
            StitchError::SourceIsDestination { path, target } => write!(
                f,
                "{}: is also {}, which the stitch would replace — a source is never written",
                path.display(),
                target.display()
            ),
            StitchError::DestinationExists(path) => write!(
                f,
                "{} already exists (pass overwrite / --force to replace it)",
                path.display()
            ),
            StitchError::SourceNotFresh(path) => write!(
                f,
                "{}: open_source needs a fresh (absent or empty) file",
                path.display()
            ),
            StitchError::Verification { stage, detail } => {
                write!(f, "{stage} failed: {detail}")
            }
            StitchError::Io { context, source } => write!(f, "{context}: {source}"),
            StitchError::Sqlite { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for StitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StitchError::Io { source, .. } => Some(source),
            StitchError::Sqlite { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub(crate) fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> StitchError {
    let context = context.into();
    move |source| StitchError::Io { context, source }
}

pub(crate) fn sql_err(context: impl Into<String>) -> impl FnOnce(rusqlite::Error) -> StitchError {
    let context = context.into();
    move |source| StitchError::Sqlite { context, source }
}
