//! Time a crawl: `cargo run --release --example crawl -- [folder] [threads...]`.
//! The folder is an index path (`/C:/Users/me`) or a native one; "/" (the
//! default) is every fixed drive.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = match args.first().map(String::as_str) {
        None | Some("/") => b"/".to_vec(),
        Some(p) => fsearch::paths::from_native(std::path::Path::new(p)).expect("an absolute path on a drive"),
    };
    let threads: Vec<usize> = args.iter().skip(1).filter_map(|t| t.parse().ok()).collect();
    for t in if threads.is_empty() { vec![8] } else { threads } {
        let start = Instant::now();
        let ls = fsearch::walk::scan(&root, t);
        let took = start.elapsed();
        let entries: usize = ls.iter().map(|l| l.ents.len()).sum();
        println!("{t:2} threads: {entries} entries, {} folders in {took:.2?}", ls.len());
    }
}
