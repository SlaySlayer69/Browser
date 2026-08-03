//! Copies the `ui` folder next to the built executable.
//!
//! The chrome is served from disk through WebView2's virtual-host mapping
//! rather than embedded in the binary. That keeps the UI editable without a
//! rebuild and, more importantly, lets WebView2 read the files directly — an
//! embedded copy would have to be served through a custom scheme handler, and
//! every stylesheet and script would then cost a round trip into our process.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

fn main() {
    // Re-run whenever anything in ui/ changes.
    println!("cargo:rerun-if-changed=ui");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = manifest_dir.join("ui");
    if !source.is_dir() {
        return;
    }

    // OUT_DIR is target/<profile>/build/<crate>-<hash>/out; the executable
    // lands three levels up.
    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };
    let Some(target_dir) = PathBuf::from(out_dir).ancestors().nth(3).map(Path::to_path_buf) else {
        return;
    };

    let destination = target_dir.join("ui");
    if let Err(error) = copy_tree(&source, &destination) {
        // A failed copy is worth surfacing but not worth failing the build:
        // `cargo test` on a non-Windows host does not need the UI at all.
        println!("cargo:warning=could not stage ui/: {error}");
    }
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            // Skip the copy when the destination is already up to date, so an
            // incremental build does not rewrite the whole tree.
            let should_copy = match (entry.metadata()?.modified(), fs::metadata(&target).and_then(|m| m.modified())) {
                (Ok(src), Ok(dst)) => src > dst,
                _ => true,
            };
            if should_copy {
                fs::copy(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}
