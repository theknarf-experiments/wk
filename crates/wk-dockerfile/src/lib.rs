//! A Dockerfile parser (chumsky) for wk's wasm-container builds.
//!
//! wk builds OCI images whose entrypoint is a wasm component, so the supported
//! instruction set is the copy-and-configure subset: `FROM` (single stage),
//! `COPY`/`ADD` from the build context, `ENTRYPOINT`/`CMD` (exec or shell
//! form), `ENV`, `WORKDIR`, `LABEL`, and `RUN` — with the twist that a RUN
//! target must be a wasm CLI inside the rootfs built so far (there is no shell;
//! the embedder executes the component). Metadata-only instructions wk can't
//! honor (`EXPOSE`, `USER`, `VOLUME`, ...) parse but are recorded as ignored.
//!
//! The file is split into logical lines first (joining `\` continuations and
//! dropping `#` comments); each line's instruction grammar — quoted strings,
//! JSON exec arrays, `KEY=value` pairs, `--flags` — is a chumsky parser.

use chumsky::prelude::*;

/// One parsed instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum Instr {
    /// `FROM <image>` — the base image (`scratch` = empty rootfs).
    From { image: String },
    /// `COPY <src>... <dest>` (or `ADD` without url/tar semantics).
    Copy { srcs: Vec<String>, dest: String },
    /// `ENTRYPOINT [...]` or shell form.
    Entrypoint(Vec<String>),
    /// `CMD [...]` or shell form.
    Cmd(Vec<String>),
    /// `ENV K=V ...` (or the legacy `ENV K V`).
    Env(Vec<(String, String)>),
    /// `RUN <wasm> <args>...` — executed at build time by the embedder, which
    /// requires the target to be a wasm CLI inside the rootfs built so far.
    Run(Vec<String>),
    /// `WORKDIR /path`.
    Workdir(String),
    /// `LABEL k=v ...` — carried into the image config.
    Label(Vec<(String, String)>),
    /// A recognized-but-unsupported metadata instruction (EXPOSE, USER, ...),
    /// kept so the builder can warn.
    Ignored { keyword: String },
}

/// A parsed Dockerfile.
#[derive(Clone, Debug, PartialEq)]
pub struct Dockerfile {
    pub instructions: Vec<Instr>,
}

impl Dockerfile {
    /// The base image of the (single) `FROM`.
    pub fn from_image(&self) -> Option<&str> {
        self.instructions.iter().find_map(|i| match i {
            Instr::From { image } => Some(image.as_str()),
            _ => None,
        })
    }
}

/// Split source into logical lines: physical lines joined over trailing `\`
/// continuations, with blank and `#`-comment lines dropped. Each logical line
/// carries the 1-based number of its first physical line (for errors).
fn logical_lines(source: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (i, raw) in source.lines().enumerate() {
        let line = raw.trim_end();
        // A comment line is skipped even mid-continuation (Docker's rule).
        if pending.is_none() && (line.trim().is_empty() || line.trim_start().starts_with('#')) {
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        let (text, continues) = match line.strip_suffix('\\') {
            Some(head) => (head, true),
            None => (line, false),
        };
        match pending.as_mut() {
            Some((_, acc)) => {
                acc.push(' ');
                acc.push_str(text.trim());
            }
            None => pending = Some((i + 1, text.trim().to_string())),
        }
        if !continues {
            if let Some((n, acc)) = pending.take() {
                if !acc.trim().is_empty() {
                    out.push((n, acc));
                }
            }
        }
    }
    if let Some((n, acc)) = pending {
        if !acc.trim().is_empty() {
            out.push((n, acc));
        }
    }
    out
}

type Extra<'a> = extra::Err<Rich<'a, char>>;

/// A JSON-style double-quoted string with the usual escapes.
fn json_string<'a>() -> impl Parser<'a, &'a str, String, Extra<'a>> {
    let escape = just('\\').ignore_then(choice((
        just('"').to('"'),
        just('\\').to('\\'),
        just('/').to('/'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
    )));
    let normal = any().filter(|c: &char| *c != '"' && *c != '\\');
    choice((escape, normal))
        .repeated()
        .collect::<String>()
        .delimited_by(just('"'), just('"'))
}

/// The exec form: `["arg", "arg", ...]`.
fn exec_array<'a>() -> impl Parser<'a, &'a str, Vec<String>, Extra<'a>> {
    let ws = text::inline_whitespace();
    json_string()
        .padded_by(ws)
        .separated_by(just(','))
        .collect::<Vec<_>>()
        .delimited_by(just('['), just(']'))
        .padded_by(ws)
        .then_ignore(end())
}

/// One shell-form word: double-quoted (with escapes), single-quoted (raw), or a
/// bare run of non-whitespace.
fn word<'a>() -> impl Parser<'a, &'a str, String, Extra<'a>> {
    let single = any()
        .filter(|c: &char| *c != '\'')
        .repeated()
        .collect::<String>()
        .delimited_by(just('\''), just('\''));
    let bare = any()
        .filter(|c: &char| !c.is_whitespace() && *c != '"' && *c != '\'')
        .repeated()
        .at_least(1)
        .collect::<String>();
    choice((json_string(), single, bare))
}

/// Whitespace-separated shell-form words (at least one).
fn words<'a>() -> impl Parser<'a, &'a str, Vec<String>, Extra<'a>> {
    let ws = text::inline_whitespace();
    word()
        .padded_by(ws)
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .then_ignore(end())
}

/// `KEY=value` pairs (value quoted or bare), as used by ENV and LABEL.
fn kv_pairs<'a>() -> impl Parser<'a, &'a str, Vec<(String, String)>, Extra<'a>> {
    let ws = text::inline_whitespace();
    let key = any()
        .filter(|c: &char| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '-')
        .repeated()
        .at_least(1)
        .collect::<String>();
    let single = any()
        .filter(|c: &char| *c != '\'')
        .repeated()
        .collect::<String>()
        .delimited_by(just('\''), just('\''));
    let bare = any()
        .filter(|c: &char| !c.is_whitespace())
        .repeated()
        .collect::<String>();
    let value = choice((json_string(), single, bare));
    key.then_ignore(just('='))
        .then(value)
        .padded_by(ws)
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .then_ignore(end())
}

/// COPY/ADD arguments: `--flag[=v]`* then source... dest. Returns (flags, words).
type CopyArgs = (Vec<String>, Vec<String>);
fn copy_args<'a>() -> impl Parser<'a, &'a str, CopyArgs, Extra<'a>> {
    let ws = text::inline_whitespace();
    let flag = just("--")
        .ignore_then(
            any()
                .filter(|c: &char| !c.is_whitespace() && *c != '=')
                .repeated()
                .at_least(1)
                .collect::<String>(),
        )
        .then_ignore(just('=').ignore_then(word()).or_not());
    flag.padded_by(ws)
        .repeated()
        .collect::<Vec<_>>()
        .then(
            word()
                .padded_by(ws)
                .repeated()
                .at_least(2)
                .collect::<Vec<_>>(),
        )
        .then_ignore(end())
}

/// Render a chumsky error against its instruction line.
fn arg_err(line: usize, kw: &str, errs: Vec<Rich<char>>) -> String {
    let detail = errs
        .first()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "invalid arguments".to_string());
    format!("Dockerfile line {line}: {kw}: {detail}")
}

/// Parse Dockerfile source. Errors carry the 1-based source line.
pub fn parse(source: &str) -> Result<Dockerfile, String> {
    let mut instructions = Vec::new();
    let mut seen_from = false;
    for (line_no, line) in logical_lines(source) {
        let (kw_raw, rest) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k, r.trim()),
            None => (line.as_str(), ""),
        };
        let kw = kw_raw.to_ascii_uppercase();
        let instr = match kw.as_str() {
            "FROM" => {
                if seen_from {
                    return Err(format!(
                        "Dockerfile line {line_no}: multi-stage builds are not supported"
                    ));
                }
                seen_from = true;
                let image = rest.split_whitespace().next().unwrap_or_default();
                if image.is_empty() {
                    return Err(format!("Dockerfile line {line_no}: FROM needs an image"));
                }
                Instr::From {
                    image: image.to_string(),
                }
            }
            "COPY" | "ADD" => {
                let (flags, mut w) = copy_args()
                    .parse(rest)
                    .into_result()
                    .map_err(|e| arg_err(line_no, &kw, e))?;
                for f in &flags {
                    match f.as_str() {
                        // No uid/gid or permission bits in the vfs; harmless.
                        "chown" | "chmod" | "link" => {}
                        other => {
                            return Err(format!(
                                "Dockerfile line {line_no}: {kw} --{other} is not supported"
                            ))
                        }
                    }
                }
                let dest = w.pop().expect("at_least(2) guarantees a destination");
                Instr::Copy { srcs: w, dest }
            }
            "ENTRYPOINT" | "CMD" => {
                let argv = exec_array()
                    .parse(rest)
                    .into_result()
                    .or_else(|_| words().parse(rest).into_result())
                    .map_err(|e| arg_err(line_no, &kw, e))?;
                if kw == "ENTRYPOINT" {
                    Instr::Entrypoint(argv)
                } else {
                    Instr::Cmd(argv)
                }
            }
            "ENV" => match kv_pairs().parse(rest).into_result() {
                Ok(pairs) => Instr::Env(pairs),
                // Legacy `ENV KEY value with spaces`.
                Err(_) => match rest.split_once(char::is_whitespace) {
                    Some((k, v)) => Instr::Env(vec![(k.to_string(), v.trim().to_string())]),
                    None => {
                        return Err(format!(
                            "Dockerfile line {line_no}: ENV needs a key and a value"
                        ))
                    }
                },
            },
            "WORKDIR" => Instr::Workdir(rest.to_string()),
            "LABEL" => Instr::Label(
                kv_pairs()
                    .parse(rest)
                    .into_result()
                    .map_err(|e| arg_err(line_no, &kw, e))?,
            ),
            "RUN" => Instr::Run(
                exec_array()
                    .parse(rest)
                    .into_result()
                    .or_else(|_| words().parse(rest).into_result())
                    .map_err(|e| arg_err(line_no, &kw, e))?,
            ),
            "EXPOSE" | "USER" | "VOLUME" | "STOPSIGNAL" | "HEALTHCHECK" | "SHELL"
            | "MAINTAINER" | "ONBUILD" | "ARG" => Instr::Ignored { keyword: kw },
            other => {
                return Err(format!(
                    "Dockerfile line {line_no}: unknown instruction {other}"
                ))
            }
        };
        instructions.push(instr);
    }
    Ok(Dockerfile { instructions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_vim_dockerfile() {
        let src = r#"
# Vim as a wasm container: unmodified vim.wasm + its runtime files.
FROM scratch
COPY vim.wasm /vim.wasm
COPY vim-src/runtime /usr/share/vim/runtime
ENV VIMRUNTIME=/usr/share/vim/runtime
ENTRYPOINT ["/vim.wasm"]
CMD ["-u", "NONE"]
"#;
        let df = parse(src).expect("parses");
        assert_eq!(df.from_image(), Some("scratch"));
        assert_eq!(
            df.instructions,
            vec![
                Instr::From {
                    image: "scratch".into()
                },
                Instr::Copy {
                    srcs: vec!["vim.wasm".into()],
                    dest: "/vim.wasm".into()
                },
                Instr::Copy {
                    srcs: vec!["vim-src/runtime".into()],
                    dest: "/usr/share/vim/runtime".into()
                },
                Instr::Env(vec![("VIMRUNTIME".into(), "/usr/share/vim/runtime".into())]),
                Instr::Entrypoint(vec!["/vim.wasm".into()]),
                Instr::Cmd(vec!["-u".into(), "NONE".into()]),
            ]
        );
    }

    #[test]
    fn continuations_and_comments_join_lines() {
        let src = "FROM scratch\nCOPY a.txt \\\n     b.txt \\\n     /dir/\n# trailing comment\n";
        let df = parse(src).expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Copy {
                srcs: vec!["a.txt".into(), "b.txt".into()],
                dest: "/dir/".into()
            }
        );
    }

    #[test]
    fn shell_form_and_quotes() {
        let src =
            "FROM scratch\nENTRYPOINT /app.wasm --flag \"two words\"\nCMD run 'single quoted'\n";
        let df = parse(src).expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Entrypoint(vec![
                "/app.wasm".into(),
                "--flag".into(),
                "two words".into()
            ])
        );
        assert_eq!(
            df.instructions[2],
            Instr::Cmd(vec!["run".into(), "single quoted".into()])
        );
    }

    #[test]
    fn env_forms() {
        let src = "FROM scratch\nENV A=1 B=\"two words\" C='three word value'\nENV LEGACY spaced value here\n";
        let df = parse(src).expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Env(vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "three word value".into()),
            ])
        );
        assert_eq!(
            df.instructions[2],
            Instr::Env(vec![("LEGACY".into(), "spaced value here".into())])
        );
    }

    #[test]
    fn copy_flags_are_tolerated_but_from_is_not() {
        let df = parse("FROM scratch\nCOPY --chown=1:1 --chmod=755 a b /d\n").expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Copy {
                srcs: vec!["a".into(), "b".into()],
                dest: "/d".into()
            }
        );
        let err = parse("FROM scratch\nCOPY --from=builder /x /y\n").unwrap_err();
        assert!(err.contains("--from"), "unsupported flag named: {err}");
    }

    #[test]
    fn run_parses_exec_and_shell_forms() {
        // RUN is allowed at parse time; the *builder* enforces that the target
        // is a wasm file it can execute.
        let df = parse("FROM scratch\nRUN /gen.wasm --out /data\n").expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Run(vec!["/gen.wasm".into(), "--out".into(), "/data".into()])
        );
        let df = parse("FROM scratch\nRUN [\"/gen.wasm\", \"a b\"]\n").expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Run(vec!["/gen.wasm".into(), "a b".into()])
        );
    }

    #[test]
    fn multi_stage_is_rejected() {
        let err = parse("FROM scratch\nFROM other AS b\n").unwrap_err();
        assert!(err.contains("multi-stage"), "err was: {err}");
    }

    #[test]
    fn workdir_label_and_ignored() {
        let src = "FROM scratch\nWORKDIR /app\nLABEL a=1 b=2\nEXPOSE 8080\nUSER nobody\n";
        let df = parse(src).expect("parses");
        assert_eq!(df.instructions[1], Instr::Workdir("/app".into()));
        assert_eq!(
            df.instructions[2],
            Instr::Label(vec![("a".into(), "1".into()), ("b".into(), "2".into())])
        );
        assert_eq!(
            df.instructions[3],
            Instr::Ignored {
                keyword: "EXPOSE".into()
            }
        );
        assert_eq!(
            df.instructions[4],
            Instr::Ignored {
                keyword: "USER".into()
            }
        );
    }

    #[test]
    fn exec_array_escapes() {
        let df =
            parse("FROM scratch\nCMD [\"a \\\"quoted\\\" arg\", \"b\\\\c\"]\n").expect("parses");
        assert_eq!(
            df.instructions[1],
            Instr::Cmd(vec!["a \"quoted\" arg".into(), "b\\c".into()])
        );
    }

    #[test]
    fn unknown_instruction_is_an_error_with_its_line() {
        let err = parse("FROM scratch\n\nFLY to the moon\n").unwrap_err();
        assert!(err.contains("FLY"), "err was: {err}");
        assert!(err.contains("line 3"), "err was: {err}");
    }
}
