//! Stands in for Everything so evbar can be tried without it. Indexes the
//! names under one directory once at startup, then serves the IPC window.
//!
//!     cargo run --example mock_everything -- C:\Windows 200000
//!
//! Only use this when the real Everything is not running: both register the
//! same window class.

use std::path::Path;

use evbar::mock::{self, Entry};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| "C:\\Windows".into());
    let limit: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200_000);

    let mut corpus = Vec::new();
    let mut stack = vec![Path::new(&root).to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            if corpus.len() >= limit {
                break;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let folder = meta.is_dir();
            if folder && !meta.file_type().is_symlink() {
                stack.push(entry.path());
            }
            corpus.push(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: dir.to_string_lossy().into_owned(),
                folder,
                size: if folder { 0 } else { meta.len() },
            });
        }
    }
    eprintln!("mock Everything: indexed {} entries under {root}", corpus.len());
    mock::run(None, corpus);
}
