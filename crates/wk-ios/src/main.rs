//! The smoke binary: run wk's boot checks and print them.
//!
//! Built for an iOS target it runs under `xcrun simctl spawn`, which is how
//! the checks reach a simulator without an app bundle; built for the host it
//! is a quick way to see the same output.

fn main() {
    let arg = std::env::args().nth(1);
    let report = wk_ios::boot(arg.as_deref().map(std::path::Path::new));
    for line in &report.lines {
        println!("{line}");
    }
    std::process::exit(i32::from(!report.ok));
}
