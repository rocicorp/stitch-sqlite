//! The command line (the `stitch-sqlite` binary; `rindle stitch` delegates here).

use std::path::Path;

use crate::{stitch, JournalMode, Options, Report, Stats, Verify};

pub const USAGE: &str = "\
usage: stitch-sqlite [options] <out.db> <source.db>...

Link closed one-table-per-file SQLite databases into one file by relocating pages (design 415).
Sources must share page size / reserved bytes / encoding, have auto_vacuum = NONE, and be closed
with no -wal/-wal2/-shm/-journal sidecar. Nothing above SQLite is written: add your own metadata
to <out.db> afterwards.

options:
  --verify none|quick|full      SQLite-level check of the output (default none; the walk's
                                structural checks and the smoke probe always run)
  --journal-mode delete|wal|wal2  output journal mode (default delete: the file as SQLite wrote it)
  --stats skip|copy|analyze     sqlite_stat* handling (default skip; copy = the sources' rows)
  --threads N                   link N sources at once, each into a reserved destination page
                                range (default 1: the sequential walk)
  --force                       replace <out.db> if it exists
  --json                        print the report as JSON
  --quiet                       print nothing on success
  -h, --help";

/// Run with the arguments after the program name. Returns the process exit code:
/// `0` success, `1` a stitch error, `2` a usage error.
pub fn run(args: &[String]) -> i32 {
    let mut opts = Options::default();
    let mut json = false;
    let mut quiet = false;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    let usage_err = |msg: &str| -> i32 {
        eprintln!("error: {msg}\n\n{USAGE}");
        2
    };
    while i < args.len() {
        let a = args[i].as_str();
        let value = |i: &mut usize| -> Option<&str> {
            *i += 1;
            args.get(*i).map(String::as_str)
        };
        match a {
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--force" => opts.overwrite = true,
            "--threads" => {
                opts.threads = match value(&mut i).map(str::parse::<usize>) {
                    Some(Ok(n)) if n >= 1 => n,
                    other => {
                        return usage_err(&format!(
                            "--threads: expected a positive integer, got {other:?}"
                        ))
                    }
                }
            }
            "--json" => json = true,
            "--quiet" => quiet = true,
            "--verify" => {
                opts.verify = match value(&mut i) {
                    Some("none") => Verify::None,
                    Some("quick") => Verify::Quick,
                    Some("full") => Verify::Full,
                    other => {
                        return usage_err(&format!(
                            "--verify: expected none|quick|full, got {other:?}"
                        ))
                    }
                }
            }
            "--journal-mode" => {
                opts.journal_mode = match value(&mut i) {
                    Some("delete") => JournalMode::Delete,
                    Some("wal") => JournalMode::Wal,
                    Some("wal2") => JournalMode::Wal2,
                    other => {
                        return usage_err(&format!(
                            "--journal-mode: expected delete|wal|wal2, got {other:?}"
                        ))
                    }
                }
            }
            "--stats" => {
                opts.stats = match value(&mut i) {
                    Some("skip") => Stats::Skip,
                    Some("copy") => Stats::CopyRows,
                    Some("analyze") => Stats::Analyze,
                    other => {
                        return usage_err(&format!(
                            "--stats: expected skip|copy|analyze, got {other:?}"
                        ))
                    }
                }
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return usage_err(&format!("unknown option {flag}"));
            }
            _ => positional.push(a),
        }
        i += 1;
    }
    let Some((dest, sources)) = positional.split_first() else {
        return usage_err("missing <out.db>");
    };
    if sources.is_empty() {
        return usage_err("at least one <source.db> is required");
    }
    match stitch(sources, Path::new(dest), &opts) {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(s) => println!("{s}"),
                    Err(e) => {
                        eprintln!("error: serialize report: {e}");
                        return 1;
                    }
                }
            } else if !quiet {
                print!("{}", human(&report));
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// The human report: one line of totals, one of page accounting, one of timings.
pub fn human(r: &Report) -> String {
    let l = &r.link;
    let total = r.durations.skeleton_ms
        + r.durations.link_ms
        + r.durations.fsync_ms
        + r.durations.finish_ms;
    let mib = r.bytes as f64 / 1_048_576.0;
    let rate = if r.durations.link_ms > 0 {
        mib / (r.durations.link_ms as f64 / 1000.0)
    } else {
        0.0
    };
    let mut out = format!(
        "{}: {} source(s) → {} b-trees, {} pages ({mib:.1} MiB, {} B pages, {}), journal_mode {}\n",
        r.destination.display(),
        r.sources,
        l.trees,
        r.dest_pages,
        r.page_size,
        r.encoding,
        r.journal_mode.pragma_value()
    );
    out.push_str(&format!(
        "  linked {} pages: {} interior rewritten, {} leaf ({} with overflow pointers rewritten, {} byte-copied), {} overflow; {} source freelist pages left behind{}\n",
        l.pages,
        l.interior,
        l.leaf,
        l.leaf_with_overflow,
        l.leaf - l.leaf_with_overflow,
        l.overflow,
        r.source_freelist_pages,
        if l.lock_page_skipped { "; lock-byte page skipped" } else { "" }
    ));
    out.push_str(&format!(
        "  {total} ms (skeleton {} ms, link {} ms = {rate:.0} MiB/s, fsync {} ms, finish {} ms){}\n",
        r.durations.skeleton_ms,
        r.durations.link_ms,
        r.durations.fsync_ms,
        r.durations.finish_ms,
        match r.verify {
            Verify::None => String::new(),
            Verify::Quick => format!(", quick_check ok in {} ms", r.durations.verify_ms),
            Verify::Full => format!(", integrity_check ok in {} ms", r.durations.verify_ms),
        }
    ));
    out
}
