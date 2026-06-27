#[allow(warnings)]
mod bindings;

use bindings::Guest;
use std::fs;
use std::path::Path;

struct Component;

impl Guest for Component {
    fn run() {
        let result = (|| -> std::io::Result<()> {
            fs::create_dir("/work")?;
            fs::write("/work/note.txt", b"hello vfs")?;
            let content = fs::read_to_string("/work/note.txt")?;
            println!("[fs-demo] read back: {content:?}");

            let mut names: Vec<String> = fs::read_dir("/work")?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            println!("[fs-demo] dir entries: {names:?}");

            fs::remove_file("/work/note.txt")?;
            println!("[fs-demo] exists after remove: {}", Path::new("/work/note.txt").exists());
            Ok(())
        })();
        println!("[fs-demo] result: {result:?}");
    }
}

bindings::export!(Component with_types_in bindings);
