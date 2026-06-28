//! `wsh` — a tiny toy shell. It's an ordinary `fn main()` program using only
//! std (io/env/fs), compiled to a standard `wasi:cli/command` component: nothing
//! wk-specific. wk runs it in a terminal node, which is the whole point — a
//! recompiled CLI tool "just works", reading a line at a time (cooked mode) and
//! printing ANSI. `ls`/`cat` read the node's in-memory filesystem, so wiring a
//! file node into this shell makes the file show up.

use std::io::{self, Write};

fn prompt(out: &mut impl Write) {
    let _ = write!(out, "\x1b[32mwsh$\x1b[0m ");
    let _ = out.flush();
}

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    println!("\x1b[1;36mwsh\x1b[0m — a standard wasi:cli command running in wk");
    println!("type \x1b[33mhelp\x1b[0m for commands, \x1b[33mexit\x1b[0m (or Ctrl-D) to quit");
    prompt(&mut out);

    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => break, // EOF (Ctrl-D) or error
            Ok(_) => {}
        }
        let cmd = line.trim();
        let (name, args) = match cmd.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (cmd, ""),
        };

        match name {
            "" => {}
            "help" => {
                println!("builtins: help  echo <text>  env  ls  cat <file>  clear  pwd  exit")
            }
            "echo" => println!("{args}"),
            "env" => {
                let mut vars: Vec<_> = std::env::vars().collect();
                vars.sort();
                for (k, v) in vars {
                    println!("{k}={v}");
                }
            }
            "pwd" => println!("/"),
            "clear" => print!("\x1b[2J\x1b[H"),
            "ls" => match std::fs::read_dir("/") {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    for n in names {
                        println!("{n}");
                    }
                }
                Err(e) => println!("ls: {e}"),
            },
            "cat" => {
                if args.is_empty() {
                    println!("usage: cat <file>");
                } else {
                    let path = if args.starts_with('/') {
                        args.to_string()
                    } else {
                        format!("/{args}")
                    };
                    match std::fs::read_to_string(&path) {
                        Ok(s) => {
                            print!("{s}");
                            if !s.ends_with('\n') {
                                println!();
                            }
                        }
                        Err(e) => println!("cat: {args}: {e}"),
                    }
                }
            }
            "exit" | "quit" => break,
            other => println!("\x1b[31m{other}: command not found\x1b[0m"),
        }
        let _ = out.flush();
        prompt(&mut out);
    }
    println!("\nbye");
}
