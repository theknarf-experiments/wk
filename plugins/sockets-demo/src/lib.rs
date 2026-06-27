#[allow(warnings)]
mod bindings;

use bindings::Guest;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};

struct Component;

impl Guest for Component {
    fn run() {
        let result = (|| -> std::io::Result<()> {
            // A self-id so the two instances' messages are distinguishable.
            let id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);

            // The shared socket file is mounted into every instance at
            // `/shared/sock`; opening it connects us to one of its two ends.
            let mut sock = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/shared/sock")?;

            writeln!(sock, "hello from {id}")?;
            sock.flush()?;
            println!("[sockets-demo] {id} sent greeting");

            // Block until the peer writes its line — a blocking socket read.
            let mut line = String::new();
            BufReader::new(sock).read_line(&mut line)?;
            println!("[sockets-demo] {id} got: {:?}", line.trim());
            Ok(())
        })();
        if let Err(e) = result {
            println!("[sockets-demo] error: {e}");
        }
    }
}

bindings::export!(Component with_types_in bindings);
