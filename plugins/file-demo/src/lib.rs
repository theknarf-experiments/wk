#[allow(warnings)]
mod bindings;

use bindings::Guest;
use std::fs;
use std::io::Write;
use std::time::Duration;

struct Component;

/// The first regular file in our (otherwise empty) root — the file node the
/// host wired into us, if any.
fn connected_file() -> Option<String> {
    for entry in fs::read_dir("/").ok()?.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            return Some(format!("/{}", entry.file_name().to_string_lossy()));
        }
    }
    None
}

impl Guest for Component {
    fn run() {
        // A self-id so two instances are distinguishable in the shared file.
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);

        println!("[file-demo] {id}: isolated — waiting for a file to be connected…");
        let path = loop {
            if let Some(p) = connected_file() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        println!("[file-demo] {id}: got {path:?}; saying hello");

        // Announce ourselves once into the shared file.
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&path) {
            let _ = writeln!(f, "hello from {id}");
        }

        // Watch the shared file — when another instance is wired to the same
        // file node, its line shows up here too.
        loop {
            let content = fs::read_to_string(&path).unwrap_or_default();
            println!("[file-demo] {id} sees:\n{}", content.trim_end());
            std::thread::sleep(Duration::from_millis(700));
        }
    }
}

bindings::export!(Component with_types_in bindings);
