//! `stitch-sqlite`: the standalone binary (design 415 §5). All logic is in the library's
//! `cli` module so `rindle stitch` can share it.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(stitch_sqlite::cli::run(&args));
}
