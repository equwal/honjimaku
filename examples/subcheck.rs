//! Runs the upload check over files already on disk, to see what it would refuse:
//!
//!     cargo run --example subcheck -- ja /path/to/subtitles
use jimaku::subcheck::{check, Format, Script};
use std::path::Path;

fn walk(dir: &Path, script: Script, counts: &mut (usize, usize)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, script, counts);
            continue;
        }
        let Some(format) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Format::from_extension)
        else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else { continue };
        match check(&bytes, format, script) {
            Ok(_) => counts.0 += 1,
            Err(why) => {
                counts.1 += 1;
                println!("REFUSED {}: {why}", path.display());
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let script = Script::from_code(&args.next().expect("language code: ja, zh or any"));
    let root = args.next().expect("a directory");
    let mut counts = (0, 0);
    walk(Path::new(&root), script, &mut counts);
    println!("{} pass, {} refused", counts.0, counts.1);
}
