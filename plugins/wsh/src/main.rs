//! `wsh` — wk's little shell, busybox-style: one wasm binary where the shell
//! and every "external" command are in-process builtins (WASI has no
//! fork/exec, so a multicall binary is how a shell works in a sandbox). It is
//! an ordinary `fn main()` program using std only, compiled to a
//! `wasi:cli/command` component for wasm32-wasip2 — so `std::net` sockets ride
//! wk's fabric, `std::fs` is the node's in-memory filesystem, and nothing here
//! is wk-specific except the `wk` builtin (a client for the API node's
//! `api:1337` endpoint, speaking the newline-JSON wire protocol).
//!
//! Features: pipelines `|`, redirections `> >> <`, `&&`/`||`/`;`, single and
//! double quotes, `$VAR`/`${VAR}`/`$?` expansion, shell + exported variables,
//! `-c "script"` and script-file execution, and a coreutils-flavored builtin
//! set (`help` lists them) including `curl` (plain HTTP over the fabric) and
//! `wk` (drive the workspace through a wired Api node).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

// ---------------------------------------------------------------------------
// Shell state and entry
// ---------------------------------------------------------------------------

struct Shell {
    /// Shell variables (assignments); `export` copies them into the process
    /// env so `env` and `$VAR` fallback see them.
    vars: BTreeMap<String, String>,
    /// Last pipeline's exit status (`$?`).
    status: i32,
    /// Current directory (tracked by the shell; paths resolve against it).
    cwd: String,
    /// Positional parameters for a script (`$1`..).
    args: Vec<String>,
    /// Set by `exit`.
    quit: Option<i32>,
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let mut sh = Shell {
        vars: BTreeMap::new(),
        status: 0,
        cwd: "/".into(),
        args: Vec::new(),
        quit: None,
    };

    // `wsh -c "script"` | `wsh script.sh [args...]` | interactive REPL.
    match argv.get(1).map(String::as_str) {
        Some("-c") => {
            let script = argv.get(2).cloned().unwrap_or_default();
            sh.args = argv.iter().skip(3).cloned().collect();
            sh.run_script(&script);
        }
        Some(path) if !path.starts_with('-') => match std::fs::read_to_string(path) {
            Ok(script) => {
                sh.args = argv.iter().skip(2).cloned().collect();
                sh.run_script(&script);
            }
            Err(e) => {
                eprintln!("wsh: {path}: {e}");
                sh.status = 127;
            }
        },
        _ => repl(&mut sh),
    }
    std::process::exit(sh.quit.unwrap_or(sh.status));
}

fn repl(sh: &mut Shell) {
    println!("\x1b[1;36mwsh\x1b[0m — wk's shell (busybox-style: every command is a builtin)");
    println!("type \x1b[33mhelp\x1b[0m for commands, \x1b[33mexit\x1b[0m (or Ctrl-D) to quit");
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        print!("\x1b[32mwsh:{}$\x1b[0m ", sh.cwd);
        let _ = io::stdout().flush();
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        sh.run_script(&line);
        if sh.quit.is_some() {
            break;
        }
    }
    println!();
}

impl Shell {
    /// Run a whole script: lines, `;`-separated lists, `&&`/`||` chains.
    fn run_script(&mut self, script: &str) {
        for line in script.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            for list in split_top(line, ';') {
                self.run_chain(&list);
                if self.quit.is_some() {
                    return;
                }
            }
        }
    }

    /// Run one `a && b || c` chain, left to right, short-circuiting.
    fn run_chain(&mut self, chain: &str) {
        let mut rest = chain.trim();
        let mut prev_op: Option<&str> = None;
        loop {
            let (seg, op, tail) = split_chain_once(rest);
            let run = match prev_op {
                None => true,
                Some("&&") => self.status == 0,
                _ => self.status != 0, // "||"
            };
            if run {
                self.run_pipeline(seg.trim());
                if self.quit.is_some() {
                    return;
                }
            }
            match op {
                Some(o) => {
                    prev_op = Some(o);
                    rest = tail.trim_start();
                }
                None => break,
            }
        }
    }

    /// Run one pipeline `cmd1 | cmd2 | cmd3` with redirections on any stage.
    fn run_pipeline(&mut self, pipeline: &str) {
        let stages = split_top(pipeline, '|');
        let mut input: Vec<u8> = Vec::new();
        let n = stages.len();
        for (i, stage) in stages.iter().enumerate() {
            let Some(mut cmd) = self.parse_command(stage) else {
                self.status = 2;
                return;
            };
            if let Some(f) = cmd.stdin_from.take() {
                match std::fs::read(self.abs(&f)) {
                    Ok(b) => input = b,
                    Err(e) => {
                        eprintln!("wsh: {f}: {e}");
                        self.status = 1;
                        return;
                    }
                }
            }
            if cmd.argv.is_empty() {
                // Pure assignments: `NAME=value`.
                for (k, v) in cmd.assigns {
                    self.vars.insert(k, v);
                }
                self.status = 0;
                continue;
            }
            let (out, status) = self.run_builtin(&cmd.argv, &input);
            self.status = status;
            let last = i == n - 1;
            match (&cmd.stdout_to, last) {
                (Some((f, append)), _) => {
                    let path = self.abs(f);
                    let res = if *append {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .and_then(|mut fh| fh.write_all(&out))
                    } else {
                        std::fs::write(&path, &out)
                    };
                    if let Err(e) = res {
                        eprintln!("wsh: {f}: {e}");
                        self.status = 1;
                    }
                    input = Vec::new();
                }
                (None, true) => {
                    let _ = io::stdout().write_all(&out);
                    let _ = io::stdout().flush();
                    input = Vec::new();
                }
                (None, false) => input = out,
            }
        }
    }

    /// Absolute-ize a path against the shell's cwd.
    fn abs(&self, p: &str) -> String {
        if p.starts_with('/') {
            p.to_string()
        } else if self.cwd == "/" {
            format!("/{p}")
        } else {
            format!("{}/{p}", self.cwd)
        }
    }
}

/// One parsed command: argv, leading assignments, and redirections.
struct Cmd {
    argv: Vec<String>,
    assigns: Vec<(String, String)>,
    stdout_to: Option<(String, bool)>, // (file, append)
    stdin_from: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsing: quotes, expansion, redirections
// ---------------------------------------------------------------------------

/// Split on `sep` at the top level (outside quotes). By the time this runs
/// for `|`, `&&`/`||` chains were already peeled off by [`split_chain_once`].
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                c2 if c2 == sep => {
                    out.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Split the first `a && rest` / `a || rest` off a chain (top level only).
fn split_chain_once(s: &str) -> (String, Option<&'static str>, &str) {
    let b = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i + 1 < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'\'' || c == b'"' {
                    quote = Some(c);
                } else if c == b'&' && b[i + 1] == b'&' {
                    return (s[..i].to_string(), Some("&&"), &s[i + 2..]);
                } else if c == b'|' && b[i + 1] == b'|' {
                    return (s[..i].to_string(), Some("||"), &s[i + 2..]);
                }
            }
        }
        i += 1;
    }
    (s.to_string(), None, "")
}

impl Shell {
    /// Tokenize one command with quoting + `$` expansion; peel off leading
    /// `NAME=value` assignments and trailing redirections.
    fn parse_command(&self, s: &str) -> Option<Cmd> {
        let mut tokens: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut has_cur = false;
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' => {
                    if has_cur {
                        tokens.push(std::mem::take(&mut cur));
                        has_cur = false;
                    }
                }
                '\'' => {
                    has_cur = true;
                    for q in chars.by_ref() {
                        if q == '\'' {
                            break;
                        }
                        cur.push(q);
                    }
                }
                '"' => {
                    has_cur = true;
                    let mut inner = String::new();
                    for q in chars.by_ref() {
                        if q == '"' {
                            break;
                        }
                        inner.push(q);
                    }
                    cur.push_str(&self.expand(&inner));
                }
                '$' => {
                    has_cur = true;
                    let name = read_var_name(&mut chars);
                    cur.push_str(&self.var(&name));
                }
                '\\' => {
                    has_cur = true;
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                _ => {
                    has_cur = true;
                    cur.push(c);
                }
            }
        }
        if has_cur {
            tokens.push(cur);
        }

        let mut cmd = Cmd {
            argv: Vec::new(),
            assigns: Vec::new(),
            stdout_to: None,
            stdin_from: None,
        };
        let mut i = 0;
        // Leading assignments.
        while i < tokens.len() {
            match tokens[i].split_once('=') {
                Some((k, v))
                    if cmd.argv.is_empty()
                        && !k.is_empty()
                        && k.chars().all(|c| c.is_alphanumeric() || c == '_') =>
                {
                    cmd.assigns.push((k.to_string(), v.to_string()));
                    i += 1;
                }
                _ => break,
            }
        }
        // argv + redirections.
        while i < tokens.len() {
            match tokens[i].as_str() {
                ">" | ">>" | "<" => {
                    let op = tokens[i].clone();
                    let Some(f) = tokens.get(i + 1) else {
                        eprintln!("wsh: missing redirection target");
                        return None;
                    };
                    match op.as_str() {
                        ">" => cmd.stdout_to = Some((f.clone(), false)),
                        ">>" => cmd.stdout_to = Some((f.clone(), true)),
                        _ => cmd.stdin_from = Some(f.clone()),
                    }
                    i += 2;
                }
                _ => {
                    cmd.argv.push(tokens[i].clone());
                    i += 1;
                }
            }
        }
        Some(cmd)
    }

    fn expand(&self, s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' {
                let name = read_var_name(&mut chars);
                out.push_str(&self.var(&name));
            } else {
                out.push(c);
            }
        }
        out
    }

    fn var(&self, name: &str) -> String {
        if name == "?" {
            return self.status.to_string();
        }
        if let Ok(n) = name.parse::<usize>() {
            if n >= 1 {
                return self.args.get(n - 1).cloned().unwrap_or_default();
            }
        }
        self.vars
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .unwrap_or_default()
    }
}

/// Read a `$`-variable name: `{NAME}`, `NAME`, or `?`.
fn read_var_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut name = String::new();
    if chars.peek() == Some(&'{') {
        chars.next();
        for q in chars.by_ref() {
            if q == '}' {
                break;
            }
            name.push(q);
        }
        return name;
    }
    if chars.peek() == Some(&'?') {
        chars.next();
        return "?".into();
    }
    while let Some(&q) = chars.peek() {
        if q.is_alphanumeric() || q == '_' {
            name.push(q);
            chars.next();
        } else {
            break;
        }
    }
    name
}

// ---------------------------------------------------------------------------
// Builtins
// ---------------------------------------------------------------------------

const HELP: &str = "\
files:  ls cat cp mv rm mkdir rmdir touch head tail wc pwd cd find
text:   echo printf grep sort uniq cut tr seq basename dirname xxd
shell:  env export unset set test [ true false sleep date which help exit
net:    curl (plain http over the fabric)   wk (the wk API via an Api wire)
extras: pipes a|b, redirs > >> <, && || ;, quotes, $VAR ${VAR} $?, wsh -c";

impl Shell {
    /// Dispatch one builtin: `(argv, stdin bytes)` → `(stdout bytes, status)`.
    fn run_builtin(&mut self, argv: &[String], input: &[u8]) -> (Vec<u8>, i32) {
        let name = argv[0].as_str();
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let mut out: Vec<u8> = Vec::new();
        macro_rules! say {
            ($($t:tt)*) => { let _ = writeln!(out, $($t)*); };
        }
        let status = match name {
            "help" => {
                say!("{HELP}");
                0
            }
            "true" | ":" => 0,
            "false" => 1,
            "exit" => {
                self.quit = Some(args.first().and_then(|a| a.parse().ok()).unwrap_or(0));
                0
            }
            "echo" => {
                let (rest, nl) = match args.first() {
                    Some(&"-n") => (&args[1..], false),
                    _ => (&args[..], true),
                };
                let _ = write!(out, "{}", rest.join(" "));
                if nl {
                    let _ = writeln!(out);
                }
                0
            }
            "printf" => {
                // %s %d and \n \t only — enough for scripts.
                let fmt = args.first().copied().unwrap_or("");
                let mut vals = args.iter().skip(1);
                let mut s = String::new();
                let mut ch = fmt.chars();
                while let Some(c) = ch.next() {
                    match c {
                        '%' => match ch.next() {
                            Some('s') | Some('d') => s.push_str(vals.next().copied().unwrap_or("")),
                            Some('%') => s.push('%'),
                            _ => {}
                        },
                        '\\' => match ch.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(o) => s.push(o),
                            None => {}
                        },
                        _ => s.push(c),
                    }
                }
                let _ = write!(out, "{s}");
                0
            }
            "pwd" => {
                say!("{}", self.cwd);
                0
            }
            "cd" => {
                let to = args.first().copied().unwrap_or("/");
                let path = normalize(&self.abs(to));
                if std::fs::read_dir(&path).is_ok() {
                    self.cwd = path;
                    0
                } else {
                    say!("cd: {to}: no such directory");
                    1
                }
            }
            "ls" => self.bi_ls(&args, &mut out),
            "cat" => {
                if args.is_empty() {
                    out.extend_from_slice(input);
                    0
                } else {
                    let mut st = 0;
                    for f in &args {
                        match std::fs::read(self.abs(f)) {
                            Ok(b) => out.extend_from_slice(&b),
                            Err(e) => {
                                say!("cat: {f}: {e}");
                                st = 1;
                            }
                        }
                    }
                    st
                }
            }
            "mkdir" => {
                let mut st = 0;
                let p_flag = args.contains(&"-p");
                for f in args.iter().filter(|a| !a.starts_with('-')) {
                    let path = self.abs(f);
                    let r = if p_flag {
                        std::fs::create_dir_all(&path)
                    } else {
                        std::fs::create_dir(&path)
                    };
                    if let Err(e) = r {
                        say!("mkdir: {f}: {e}");
                        st = 1;
                    }
                }
                st
            }
            "rmdir" => {
                let mut st = 0;
                for f in &args {
                    if let Err(e) = std::fs::remove_dir(self.abs(f)) {
                        say!("rmdir: {f}: {e}");
                        st = 1;
                    }
                }
                st
            }
            "rm" => {
                let rec = args.iter().any(|a| a.starts_with('-') && a.contains('r'));
                let force = args.iter().any(|a| a.starts_with('-') && a.contains('f'));
                let mut st = 0;
                for f in args.iter().filter(|a| !a.starts_with('-')) {
                    let path = self.abs(f);
                    let r = if rec {
                        std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path))
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(e) = r {
                        if !force {
                            say!("rm: {f}: {e}");
                            st = 1;
                        }
                    }
                }
                st
            }
            "touch" => {
                let mut st = 0;
                for f in &args {
                    let path = self.abs(f);
                    if std::fs::metadata(&path).is_err() {
                        if let Err(e) = std::fs::write(&path, b"") {
                            say!("touch: {f}: {e}");
                            st = 1;
                        }
                    }
                }
                st
            }
            "cp" => self.bi_cp(&args, &mut out),
            "mv" => {
                if args.len() != 2 {
                    say!("usage: mv <src> <dst>");
                    1
                } else if let Err(e) = std::fs::rename(self.abs(args[0]), self.abs(args[1])) {
                    say!("mv: {e}");
                    1
                } else {
                    0
                }
            }
            "head" | "tail" => {
                let n: usize = flag_value(&args, "-n")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10);
                let text = self.slurp(&args, input, &["-n"]);
                let lines: Vec<&str> = text.lines().collect();
                let picked: Vec<&&str> = if name == "head" {
                    lines.iter().take(n).collect()
                } else {
                    lines.iter().skip(lines.len().saturating_sub(n)).collect()
                };
                for l in picked {
                    say!("{l}");
                }
                0
            }
            "wc" => {
                let text = self.slurp(&args, input, &[]);
                let l = text.lines().count();
                let w = text.split_whitespace().count();
                let c = text.len();
                if args.contains(&"-l") {
                    say!("{l}");
                } else if args.contains(&"-w") {
                    say!("{w}");
                } else if args.contains(&"-c") {
                    say!("{c}");
                } else {
                    say!("{l} {w} {c}");
                }
                0
            }
            "grep" => {
                let icase = args.contains(&"-i");
                let invert = args.contains(&"-v");
                let count = args.contains(&"-c");
                let nums = args.contains(&"-n");
                let pos: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).copied().collect();
                let Some(pat) = pos.first() else {
                    say!("usage: grep [-ivnc] <pattern> [file...]");
                    return (out, 2);
                };
                let pat_l = pat.to_lowercase();
                let text = self.slurp(&pos[1..].to_vec(), input, &[]);
                let mut hits = 0;
                for (i, l) in text.lines().enumerate() {
                    let m = if icase {
                        l.to_lowercase().contains(&pat_l)
                    } else {
                        l.contains(pat)
                    };
                    if m != invert {
                        hits += 1;
                        if !count {
                            if nums {
                                say!("{}:{l}", i + 1);
                            } else {
                                say!("{l}");
                            }
                        }
                    }
                }
                if count {
                    say!("{hits}");
                }
                i32::from(hits == 0)
            }
            "sort" => {
                let text = self.slurp(&args, input, &[]);
                let mut lines: Vec<&str> = text.lines().collect();
                if args.contains(&"-n") {
                    lines.sort_by_key(|l| l.trim().parse::<i64>().unwrap_or(0));
                } else {
                    lines.sort_unstable();
                }
                if args.contains(&"-r") {
                    lines.reverse();
                }
                if args.contains(&"-u") {
                    lines.dedup();
                }
                for l in lines {
                    say!("{l}");
                }
                0
            }
            "uniq" => {
                let text = self.slurp(&args, input, &[]);
                let counted = args.contains(&"-c");
                let mut prev: Option<&str> = None;
                let mut cnt = 0usize;
                let flush = |out: &mut Vec<u8>, p: Option<&str>, c: usize| {
                    if let Some(p) = p {
                        if counted {
                            let _ = writeln!(out, "{c:4} {p}");
                        } else {
                            let _ = writeln!(out, "{p}");
                        }
                    }
                };
                for l in text.lines() {
                    if prev == Some(l) {
                        cnt += 1;
                    } else {
                        flush(&mut out, prev, cnt);
                        prev = Some(l);
                        cnt = 1;
                    }
                }
                flush(&mut out, prev, cnt);
                0
            }
            "cut" => {
                let text = self.slurp(&args, input, &["-d", "-f", "-c"]);
                // `-c LIST`: character ranges (`1-24`, `3-`, `-8`, `2`), the
                // form that truncates long output — comma-separated, 1-based.
                if let Some(spec) = flag_value(&args, "-c") {
                    for l in text.lines() {
                        let chars: Vec<char> = l.chars().collect();
                        let mut picked = String::new();
                        for range in spec.split(',') {
                            let (from, to) = match range.split_once('-') {
                                Some((a, "")) => (num(a).max(1) as usize, chars.len()),
                                Some(("", b)) => (1, num(b) as usize),
                                Some((a, b)) => (num(a).max(1) as usize, num(b) as usize),
                                None => {
                                    let n = num(range).max(1) as usize;
                                    (n, n)
                                }
                            };
                            for i in from..=to.min(chars.len()) {
                                if let Some(c) = chars.get(i - 1) {
                                    picked.push(*c);
                                }
                            }
                        }
                        say!("{picked}");
                    }
                    return (out, 0);
                }
                let delim = flag_value(&args, "-d").unwrap_or("\t");
                let fields: Vec<usize> = flag_value(&args, "-f")
                    .map(|f| f.split(',').filter_map(|n| n.parse().ok()).collect())
                    .unwrap_or_default();
                for l in text.lines() {
                    let parts: Vec<&str> = l.split(delim).collect();
                    let picked: Vec<&str> = fields
                        .iter()
                        .filter_map(|&i| parts.get(i.saturating_sub(1)).copied())
                        .collect();
                    say!("{}", picked.join(delim));
                }
                0
            }
            "tr" => {
                let del = args.contains(&"-d");
                let pos: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).copied().collect();
                let text = String::from_utf8_lossy(input).to_string();
                if del {
                    let set: Vec<char> = pos.first().map(|s| s.chars().collect()).unwrap_or_default();
                    let kept: String = text.chars().filter(|c| !set.contains(c)).collect();
                    let _ = write!(out, "{kept}");
                } else if pos.len() >= 2 {
                    let from: Vec<char> = pos[0].chars().collect();
                    let to: Vec<char> = pos[1].chars().collect();
                    let mapped: String = text
                        .chars()
                        .map(|c| match from.iter().position(|&f| f == c) {
                            Some(i) => *to.get(i).or(to.last()).unwrap_or(&c),
                            None => c,
                        })
                        .collect();
                    let _ = write!(out, "{mapped}");
                } else {
                    say!("usage: tr <from> <to> | tr -d <set>");
                    return (out, 2);
                }
                0
            }
            "seq" => {
                let (a, b) = match args.len() {
                    1 => (1, args[0].parse().unwrap_or(1)),
                    2 => (args[0].parse().unwrap_or(1), args[1].parse().unwrap_or(1)),
                    _ => {
                        say!("usage: seq [from] <to>");
                        return (out, 2);
                    }
                };
                for i in a..=b {
                    say!("{i}");
                }
                0
            }
            "basename" => {
                say!(
                    "{}",
                    args.first()
                        .map(|p| p.trim_end_matches('/').rsplit('/').next().unwrap_or(p))
                        .unwrap_or("")
                );
                0
            }
            "dirname" => {
                let p = args.first().copied().unwrap_or("");
                let d = p.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
                say!("{}", if d.is_empty() { "/" } else { d });
                0
            }
            "find" => {
                let root = normalize(&self.abs(args.first().copied().unwrap_or(".")));
                fn walk(dir: &str, out: &mut Vec<u8>) {
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        let mut names: Vec<_> = rd
                            .flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect();
                        names.sort();
                        for n in names {
                            let p = if dir == "/" {
                                format!("/{n}")
                            } else {
                                format!("{dir}/{n}")
                            };
                            let _ = writeln!(out, "{p}");
                            if std::fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
                                walk(&p, out);
                            }
                        }
                    }
                }
                say!("{root}");
                walk(&root, &mut out);
                0
            }
            "xxd" => {
                let data = if args.is_empty() {
                    input.to_vec()
                } else {
                    std::fs::read(self.abs(args[0])).unwrap_or_default()
                };
                for (i, chunk) in data.chunks(16).enumerate() {
                    let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                    let ascii: String = chunk
                        .iter()
                        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                        .collect();
                    say!("{:08x}: {:<48} {ascii}", i * 16, hex.join(" "));
                }
                0
            }
            "env" | "set" => {
                for (k, v) in &self.vars {
                    say!("{k}={v}");
                }
                let mut evars: Vec<_> = std::env::vars().collect();
                evars.sort();
                for (k, v) in evars {
                    if !self.vars.contains_key(&k) {
                        say!("{k}={v}");
                    }
                }
                0
            }
            "export" => {
                // Single-threaded program: mutating the env is sound.
                for a in &args {
                    match a.split_once('=') {
                        Some((k, v)) => unsafe { std::env::set_var(k, v) },
                        None => {
                            if let Some(v) = self.vars.get(*a) {
                                unsafe { std::env::set_var(a, v) };
                            }
                        }
                    }
                }
                0
            }
            "unset" => {
                for a in &args {
                    self.vars.remove(*a);
                    unsafe { std::env::remove_var(a) };
                }
                0
            }
            "test" | "[" => {
                let mut a: Vec<&str> = args.clone();
                if name == "[" && a.last() == Some(&"]") {
                    a.pop();
                }
                i32::from(!eval_test(self, &a))
            }
            "sleep" => {
                let secs: f64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(0.0);
                std::thread::sleep(std::time::Duration::from_millis((secs * 1000.0) as u64));
                0
            }
            "date" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                say!("{now}");
                0
            }
            "which" | "type" => {
                for a in &args {
                    say!("{a}: wsh builtin");
                }
                0
            }
            "clear" => {
                let _ = write!(out, "\x1b[2J\x1b[H");
                0
            }
            "wsh" | "sh" => {
                // Nested script runs in this same shell (no processes here).
                if args.first() == Some(&"-c") {
                    let script = args.get(1).copied().unwrap_or("").to_string();
                    self.run_script(&script);
                    self.status
                } else if let Some(f) = args.first() {
                    match std::fs::read_to_string(self.abs(f)) {
                        Ok(s) => {
                            self.run_script(&s);
                            self.status
                        }
                        Err(e) => {
                            say!("wsh: {f}: {e}");
                            127
                        }
                    }
                } else {
                    0
                }
            }
            "curl" => bi_curl(&args, input, &mut out),
            "wk" => bi_wk(&args, &mut out),
            other => {
                say!("wsh: {other}: not found (every command is a builtin; see `help`)");
                127
            }
        };
        (out, status)
    }

    /// Read the named files, else stdin. `value_flags` are this command's
    /// flags that take a separate value (`head -n 5`), so the value isn't
    /// mistaken for a filename — which flag takes a value is per-command
    /// (`wc -c` counts bytes; `cut -c 1-8` selects characters).
    fn slurp(&self, args: &[&str], input: &[u8], value_flags: &[&str]) -> String {
        let mut files: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = args[i];
            if a.starts_with('-') {
                if value_flags.contains(&a) {
                    i += 2; // skip the flag and its value
                    continue;
                }
                i += 1;
                continue;
            }
            files.push(a);
            i += 1;
        }
        if files.is_empty() {
            String::from_utf8_lossy(input).to_string()
        } else {
            let mut s = String::new();
            for f in files {
                if let Ok(t) = std::fs::read_to_string(self.abs(f)) {
                    s.push_str(&t);
                }
            }
            s
        }
    }

    fn bi_ls(&self, args: &[&str], out: &mut Vec<u8>) -> i32 {
        let long = args.contains(&"-l");
        let target = args
            .iter()
            .find(|a| !a.starts_with('-'))
            .copied()
            .unwrap_or(".");
        let path = normalize(&self.abs(target));
        let Ok(rd) = std::fs::read_dir(&path) else {
            if std::fs::metadata(&path).is_ok() {
                let _ = writeln!(out, "{target}");
                return 0;
            }
            let _ = writeln!(out, "ls: {target}: no such file or directory");
            return 1;
        };
        let mut entries: Vec<(String, bool, u64)> = rd
            .flatten()
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let meta = e.metadata().ok();
                (
                    name,
                    meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                    meta.map(|m| m.len()).unwrap_or(0),
                )
            })
            .collect();
        entries.sort();
        for (name, is_dir, len) in entries {
            let slash = if is_dir { "/" } else { "" };
            if long {
                let t = if is_dir { 'd' } else { '-' };
                let _ = writeln!(out, "{t} {len:>8} {name}{slash}");
            } else {
                let _ = writeln!(out, "{name}{slash}");
            }
        }
        0
    }

    fn bi_cp(&self, args: &[&str], out: &mut Vec<u8>) -> i32 {
        let rec = args.contains(&"-r");
        let pos: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).copied().collect();
        if pos.len() != 2 {
            let _ = writeln!(out, "usage: cp [-r] <src> <dst>");
            return 1;
        }
        let (src, dst) = (self.abs(pos[0]), self.abs(pos[1]));
        fn copy_tree(src: &str, dst: &str) -> io::Result<()> {
            if std::fs::metadata(src)?.is_dir() {
                std::fs::create_dir_all(dst)?;
                for e in std::fs::read_dir(src)?.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    copy_tree(&format!("{src}/{n}"), &format!("{dst}/{n}"))?;
                }
                Ok(())
            } else {
                std::fs::write(dst, std::fs::read(src)?)
            }
        }
        let r = if rec {
            copy_tree(&src, &dst)
        } else {
            std::fs::read(&src).and_then(|b| std::fs::write(&dst, b))
        };
        match r {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(out, "cp: {e}");
                1
            }
        }
    }
}

/// The value following a flag (`-n 5`), or glued to it (`-n5`).
fn flag_value<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| *a == flag)
        .and_then(|i| args.get(i + 1))
        .copied()
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix(flag).filter(|r| !r.is_empty()))
        })
}

/// Normalize `.`/`..`/`//` out of an absolute path.
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `test` / `[` — the subset scripts actually use.
fn eval_test(sh: &Shell, a: &[&str]) -> bool {
    match a {
        [] => false,
        ["!", rest @ ..] => !eval_test(sh, rest),
        ["-f", p] => std::fs::metadata(sh.abs(p)).map(|m| m.is_file()).unwrap_or(false),
        ["-d", p] => std::fs::metadata(sh.abs(p)).map(|m| m.is_dir()).unwrap_or(false),
        ["-e", p] => std::fs::metadata(sh.abs(p)).is_ok(),
        ["-z", s] => s.is_empty(),
        ["-n", s] => !s.is_empty(),
        [x, "=", y] | [x, "==", y] => x == y,
        [x, "!=", y] => x != y,
        [x, "-eq", y] => num(x) == num(y),
        [x, "-ne", y] => num(x) != num(y),
        [x, "-lt", y] => num(x) < num(y),
        [x, "-gt", y] => num(x) > num(y),
        [x, "-le", y] => num(x) <= num(y),
        [x, "-ge", y] => num(x) >= num(y),
        [s] => !s.is_empty(),
        _ => false,
    }
}

fn num(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// curl — plain HTTP/1.1 over std::net (the fabric, via wasi:sockets)
// ---------------------------------------------------------------------------

fn bi_curl(args: &[&str], input: &[u8], out: &mut Vec<u8>) -> i32 {
    let mut method: Option<&str> = None;
    let mut body: Option<Vec<u8>> = None;
    let mut headers: Vec<&str> = Vec::new();
    let mut include = false;
    let mut url = "";
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-X" => {
                method = args.get(i + 1).copied();
                i += 2;
            }
            "-d" => {
                body = Some(match args.get(i + 1) {
                    Some(&"@-") | None => input.to_vec(),
                    Some(v) => v.as_bytes().to_vec(),
                });
                i += 2;
            }
            "-H" => {
                if let Some(h) = args.get(i + 1) {
                    headers.push(h);
                }
                i += 2;
            }
            "-i" => {
                include = true;
                i += 1;
            }
            "-s" => i += 1,
            u => {
                url = u;
                i += 1;
            }
        }
    }
    let Some(rest) = url.strip_prefix("http://") else {
        let _ = writeln!(out, "curl: only plain http:// works here (no TLS in the sandbox)");
        return 2;
    };
    let (hostport, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    let (host, port) = hostport
        .rsplit_once(':')
        .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p)))
        .unwrap_or((hostport, 80));
    let method = method.unwrap_or(if body.is_some() { "POST" } else { "GET" });

    let mut stream = match std::net::TcpStream::connect((host, port)) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "curl: connect {host}:{port}: {e}");
            return 7;
        }
    };
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    if let Some(b) = &body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    let _ = stream.write_all(req.as_bytes());
    if let Some(b) = &body {
        let _ = stream.write_all(b);
    }
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    if include {
        out.extend_from_slice(&resp);
    } else {
        match resp.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(p) => out.extend_from_slice(&resp[p + 4..]),
            None => out.extend_from_slice(&resp),
        }
    }
    0
}

// ---------------------------------------------------------------------------
// wk — the workspace API over a wired Api node (api:1337, newline-JSON)
// ---------------------------------------------------------------------------

const WK_HELP: &str = "\
usage: wk <command>
  ps               list nodes (via GetSnapshot)
  snapshot         print the raw Snapshot JSON
  send '<json>'    send a raw ClientMsg line, print the reply
  token            print this node's own capability token (/run/wk/token)
requires a wire to an Api node; authority = this node's token";

fn bi_wk(args: &[&str], out: &mut Vec<u8>) -> i32 {
    match args.first().copied() {
        Some("token") => match std::fs::read_to_string("/run/wk/token") {
            Ok(t) => {
                let _ = writeln!(out, "{}", t.trim());
                0
            }
            Err(_) => {
                let _ = writeln!(out, "wk: no token at /run/wk/token");
                1
            }
        },
        Some("snapshot") => wk_roundtrip("\"GetSnapshot\"", out, |line, out| {
            let _ = writeln!(out, "{line}");
            0
        }),
        Some("ps") => wk_roundtrip("\"GetSnapshot\"", out, |line, out| {
            // Tiny field extraction, no JSON dependency: enough for eyes;
            // `wk snapshot` has the unabridged truth.
            let Some(nodes) = line.split("\"nodes\":[").nth(1) else {
                let _ = writeln!(out, "{line}");
                return 1;
            };
            let _ = writeln!(out, "{:<28} {:<10} {:<12} RUNNING", "ID", "KIND", "NAME");
            for obj in nodes.split("{\"id\":").skip(1) {
                let field = |k: &str| -> String {
                    obj.split(&format!("\"{k}\":\"", k = k))
                        .nth(1)
                        .and_then(|r| r.split('"').next())
                        .unwrap_or("")
                        .to_string()
                };
                let id = obj.split('"').nth(1).unwrap_or("").to_string();
                let running = obj
                    .split("\"running\":")
                    .nth(1)
                    .map(|r| r.starts_with("true"))
                    .unwrap_or(false);
                let _ = writeln!(
                    out,
                    "{:<28} {:<10} {:<12} {running}",
                    id,
                    field("kind"),
                    field("name")
                );
            }
            0
        }),
        Some("send") => {
            let Some(json) = args.get(1) else {
                let _ = writeln!(out, "usage: wk send '<ClientMsg json>'");
                return 2;
            };
            wk_roundtrip(json, out, |line, out| {
                let _ = writeln!(out, "{line}");
                0
            })
        }
        _ => {
            let _ = writeln!(out, "{WK_HELP}");
            2
        }
    }
}

/// Send one JSON line to `api:1337` and hand the one-line reply to `show`.
fn wk_roundtrip(msg: &str, out: &mut Vec<u8>, show: impl Fn(&str, &mut Vec<u8>) -> i32) -> i32 {
    let stream = match std::net::TcpStream::connect(("api", 1337)) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "wk: connect api:1337: {e} (is this node wired to an Api node?)");
            return 1;
        }
    };
    let mut w = &stream;
    if w.write_all(msg.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
        let _ = writeln!(out, "wk: send failed");
        return 1;
    }
    let mut reader = io::BufReader::new(&stream);
    let mut line = String::new();
    match io::BufRead::read_line(&mut reader, &mut line) {
        Ok(n) if n > 0 => show(line.trim_end(), out),
        _ => {
            let _ = writeln!(out, "wk: no reply");
            1
        }
    }
}
