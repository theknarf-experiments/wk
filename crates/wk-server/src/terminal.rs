//! Terminal nodes: wk runs a non-graphical wasm guest with its stdio wired to a
//! VT terminal. The guest's stdout/stderr is parsed by `alacritty_terminal` into
//! a cell grid the client renders, and keyboard input is delivered to the
//! guest's stdin — so a recompiled CLI/TUI app (one day, vim) runs in a window.
//!
//! The guest writes ANSI like it would to any TTY (isatty is true, $TERM is set,
//! $COLUMNS/$LINES report the grid). There's no OS pty: stdout is a shared byte
//! queue the client drains into the parser, and stdin is a shared queue the
//! client fills from the keyboard (the parser also writes here for terminal
//! replies, e.g. cursor-position reports).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use alacritty_terminal::Term;

use wasmtime_wasi::cli::{IsTerminal, StdinStream, StdoutStream};
use wasmtime_wasi_io::async_trait;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{InputStream, OutputStream, StreamError, StreamResult};

/// Fixed terminal grid size; guests are told this via `$COLUMNS`/`$LINES`.
pub const COLS: usize = 80;
pub const ROWS: usize = 24;

/// Default foreground/background of the grid, in RGB.
const FG: [u8; 3] = [198, 200, 205];
const BG: [u8; 3] = [16, 16, 22];

struct InpState {
    buf: VecDeque<u8>,
    waker: Option<Waker>,
    closed: bool,
}

/// The terminal's line-discipline mode, mirroring the termios flags that matter:
/// `canonical` (ICANON) = cooked, line-buffered/edited input delivered on Enter;
/// `echo` (ECHO) = echo typed characters. A guest sets this through the
/// `wk:tty/control` interface (host side in `crate::tty`); the client reads it to
/// decide whether to run the cooked line discipline. A fresh terminal is cooked.
#[derive(Clone, Copy)]
pub struct TtyMode {
    pub echo: bool,
    pub canonical: bool,
}

impl Default for TtyMode {
    fn default() -> Self {
        TtyMode {
            echo: true,
            canonical: true,
        }
    }
}

/// The guest's stdio, shared between its thread and the client. `out` is
/// stdout/stderr (guest → client), `inp` is stdin (client → guest), `tty` is the
/// line-discipline mode the guest requests via `wk:tty/control`.
pub struct TermIo {
    out: Mutex<VecDeque<u8>>,
    inp: Mutex<InpState>,
    tty: Mutex<TtyMode>,
    /// The grid size (cols, rows) the client has sized this terminal to. The
    /// guest reads it via `wk:tty/control`/`ioctl(TIOCGWINSZ)`; changing it wakes
    /// a blocked read (see `winch`) so the guest can raise SIGWINCH.
    size: Mutex<(u16, u16)>,
}

pub type SharedTermIo = Arc<TermIo>;

impl TermIo {
    pub fn new() -> SharedTermIo {
        Arc::new(TermIo {
            out: Mutex::new(VecDeque::new()),
            inp: Mutex::new(InpState {
                buf: VecDeque::new(),
                waker: None,
                closed: false,
            }),
            tty: Mutex::new(TtyMode::default()),
            size: Mutex::new((COLS as u16, ROWS as u16)),
        })
    }

    /// The current grid size (cols, rows).
    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    /// Record a new grid size. The guest's terminal shim polls this (via
    /// `wk:tty/control`) and raises SIGWINCH when it changes — WASI has no way to
    /// wake a blocked read, so delivery is poll-driven on the guest side.
    pub fn set_size(&self, cols: u16, rows: u16) {
        *self.size.lock().unwrap() = (cols, rows);
    }

    /// Set the line-discipline mode (called by the `wk:tty/control` host impl
    /// when the guest runs `tcsetattr`).
    pub fn set_tty(&self, echo: bool, canonical: bool) {
        *self.tty.lock().unwrap() = TtyMode { echo, canonical };
    }

    /// The current line-discipline mode.
    pub fn tty(&self) -> TtyMode {
        *self.tty.lock().unwrap()
    }

    /// Whether the terminal is in raw mode (non-canonical) — the guest gets
    /// keystrokes verbatim and draws its own screen.
    pub fn is_raw(&self) -> bool {
        !self.tty.lock().unwrap().canonical
    }

    fn write_out(&self, b: &[u8]) {
        let mut o = self.out.lock().unwrap();
        // Bound the backlog so a guest that floods stdout can't grow it forever.
        if o.len() < (4 << 20) {
            o.extend(b.iter().copied());
        }
    }

    /// Take everything the guest has written to stdout since the last call.
    pub fn drain_out(&self) -> Vec<u8> {
        self.out.lock().unwrap().drain(..).collect()
    }

    /// Deliver bytes to the guest's stdin (keyboard input or terminal replies).
    pub fn feed_in(&self, b: &[u8]) {
        let mut i = self.inp.lock().unwrap();
        i.buf.extend(b.iter().copied());
        if let Some(w) = i.waker.take() {
            w.wake();
        }
    }

    /// Close stdin: a blocked guest read returns EOF so the guest can exit.
    pub fn close(&self) {
        let mut i = self.inp.lock().unwrap();
        i.closed = true;
        if let Some(w) = i.waker.take() {
            w.wake();
        }
    }
}

/// `WasiCtxBuilder` stdout/stderr handle for a terminal node.
pub fn stdout(io: &SharedTermIo) -> impl StdoutStream + 'static {
    StdoutHandle(io.clone())
}

/// `WasiCtxBuilder` stdin handle for a terminal node.
pub fn stdin(io: &SharedTermIo) -> impl StdinStream + 'static {
    StdinHandle(io.clone())
}

struct StdoutHandle(SharedTermIo);
impl IsTerminal for StdoutHandle {
    fn is_terminal(&self) -> bool {
        true
    }
}
impl StdoutStream for StdoutHandle {
    // Unused: we override `p2_stream` (component guests use the p2 path).
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(tokio::io::sink())
    }
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(OutPipe(self.0.clone()))
    }
}

struct OutPipe(SharedTermIo);
#[async_trait]
impl Pollable for OutPipe {
    async fn ready(&mut self) {}
}
impl OutputStream for OutPipe {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.0.write_out(&bytes);
        Ok(())
    }
    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }
    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(1 << 16)
    }
}

struct StdinHandle(SharedTermIo);
impl IsTerminal for StdinHandle {
    fn is_terminal(&self) -> bool {
        true
    }
}
impl StdinStream for StdinHandle {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncRead + Send + Sync> {
        Box::new(tokio::io::empty())
    }
    fn p2_stream(&self) -> Box<dyn InputStream> {
        Box::new(InPipe(self.0.clone()))
    }
}

struct InPipe(SharedTermIo);
#[async_trait]
impl Pollable for InPipe {
    async fn ready(&mut self) {
        InReady(self.0.clone()).await
    }
}
impl InputStream for InPipe {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        let mut i = self.0.inp.lock().unwrap();
        if i.buf.is_empty() {
            if i.closed {
                return Err(StreamError::Closed);
            }
            return Ok(Bytes::new());
        }
        let n = size.min(i.buf.len());
        let data: Vec<u8> = i.buf.drain(..n).collect();
        Ok(Bytes::from(data))
    }
}

/// Resolves when the guest's stdin has bytes (or was closed), parking otherwise.
struct InReady(SharedTermIo);
impl Future for InReady {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut i = self.0.inp.lock().unwrap();
        if !i.buf.is_empty() || i.closed {
            Poll::Ready(())
        } else {
            i.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Pushes terminal replies (e.g. cursor-position reports) back to guest stdin.
struct EventProxy(SharedTermIo);
impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.0.feed_in(text.as_bytes());
        }
    }
}

/// Grid dimensions for `alacritty_terminal`. The default (`COLS`×`ROWS`) is just
/// a launch-time guess; the client resizes the grid to fit the node's window.
#[derive(Clone, Copy)]
struct GridSize {
    cols: usize,
    rows: usize,
}
impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// One rendered cell: position, glyph, and resolved colours (`bg = None` means
/// the default terminal background, which the client needn't fill).
pub struct CellView {
    pub col: u16,
    pub row: u16,
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: Option<[u8; 3]>,
}

/// A terminal: an `alacritty_terminal` grid driven by the guest's stdout bytes.
pub struct Terminal {
    term: Term<EventProxy>,
    parser: Processor,
    /// The line being edited, in cooked mode (delivered to the guest on Enter).
    line: Vec<u8>,
    /// The guest's stdio, so the terminal can read the line-discipline mode the
    /// guest set through `wk:tty/control` (raw/echo/canonical).
    io: SharedTermIo,
}

impl Terminal {
    pub fn new(io: SharedTermIo) -> Self {
        let (cols, rows) = io.size();
        let dims = GridSize {
            cols: cols as usize,
            rows: rows as usize,
        };
        let term = Term::new(Config::default(), &dims, EventProxy(io.clone()));
        Terminal {
            term,
            parser: Processor::new(),
            line: Vec::new(),
            io,
        }
    }

    /// Resize the grid to `cols`×`rows` (both clamped to at least 1). Reflows the
    /// alacritty grid and records the size on the shared `TermIo`, which wakes a
    /// blocked guest read so it can raise SIGWINCH and redraw.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.io.size() == (cols, rows) {
            return;
        }
        self.term.resize(GridSize {
            cols: cols as usize,
            rows: rows as usize,
        });
        self.io.set_size(cols, rows);
    }

    /// Whether the guest has put the terminal in raw mode.
    pub fn is_raw(&self) -> bool {
        self.io.is_raw()
    }

    /// Feed keyboard bytes through a cooked-mode line discipline (the default a
    /// terminal app expects when it hasn't gone raw): echo what's typed, edit
    /// the line with Backspace, and deliver a whole line to the guest's stdin on
    /// Enter (with `\n`). Ctrl-C discards the line; Ctrl-D on an empty line is
    /// end-of-input. Escape sequences (arrow keys etc.) are swallowed. (A future
    /// raw-mode/termios path would bypass this and forward bytes verbatim.)
    pub fn key_input(&mut self, bytes: &[u8], io: &SharedTermIo) {
        // Honor termios ECHO: on for a normal prompt, off for e.g. a password
        // entry (canonical but no echo). Line editing still works either way.
        let echo = io.tty().echo;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            match b {
                b'\r' | b'\n' => {
                    self.feed(b"\r\n");
                    self.line.push(b'\n');
                    io.feed_in(&self.line);
                    self.line.clear();
                }
                0x7f | 0x08 if !self.line.is_empty() => {
                    self.line.pop();
                    if echo {
                        self.feed(b"\x08 \x08");
                    }
                }
                0x03 => {
                    self.line.clear();
                    self.feed(b"^C\r\n");
                }
                // Ctrl-D on an empty line is end-of-input.
                0x04 if self.line.is_empty() => io.close(),
                // Swallow a CSI sequence so arrows etc. don't enter the line.
                0x1b if bytes.get(i) == Some(&b'[') => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                0x20..=0x7e => {
                    self.line.push(b);
                    if echo {
                        self.feed(&[b]);
                    }
                }
                _ => {}
            }
        }
    }

    /// Feed guest stdout bytes through the VT parser, updating the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Raw vs cooked comes from the guest via `wk:tty/control` (read off the
        // shared `TermIo`), not from sniffing the byte stream. In cooked mode do
        // ONLCR — a bare LF also returns the carriage — so naive `println!`
        // guests don't stair-step; a raw TUI emits its own CRLF.
        if self.io.is_raw() {
            self.parser.advance(&mut self.term, bytes);
            return;
        }
        let mut buf = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if b == b'\n' {
                buf.push(b'\r');
            }
            buf.push(b);
        }
        self.parser.advance(&mut self.term, &buf);
    }

    /// The non-blank cells currently displayed.
    pub fn cells(&self) -> Vec<CellView> {
        let rows = self.term.grid().screen_lines();
        let mut out = Vec::new();
        for indexed in self.term.grid().display_iter() {
            let cell = indexed.cell;
            if cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            let row = indexed.point.line.0;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= rows {
                continue;
            }

            let fg0 = resolve(cell.fg).unwrap_or(FG);
            let bg0 = resolve(cell.bg);
            let (fg, bg) = if cell.flags.contains(Flags::INVERSE) {
                (bg0.unwrap_or(BG), Some(fg0))
            } else {
                (fg0, bg0)
            };

            if cell.c == ' ' && bg.is_none() {
                continue;
            }
            out.push(CellView {
                col: col as u16,
                row: row as u16,
                ch: cell.c,
                fg,
                bg,
            });
        }
        out
    }

    /// The cursor cell, if it is visible (cursor shown and not scrolled back).
    pub fn cursor(&self) -> Option<(usize, usize)> {
        if self.term.grid().display_offset() != 0 {
            return None;
        }
        if !self.term.mode().contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let p = self.term.grid().cursor.point;
        let row = p.line.0;
        if row < 0 || row as usize >= self.term.grid().screen_lines() {
            return None;
        }
        Some((p.column.0, row as usize))
    }
}

/// Resolve a VT colour to RGB. `None` means "use the default" (so a default
/// background can be skipped when rendering).
fn resolve(c: Color) -> Option<[u8; 3]> {
    match c {
        Color::Spec(Rgb { r, g, b }) => Some([r, g, b]),
        Color::Indexed(i) => Some(xterm256(i)),
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => None,
        Color::Named(n) => Some(xterm256(named_index(n))),
    }
}

fn named_index(n: NamedColor) -> u8 {
    use NamedColor::*;
    match n {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        White => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        _ => 7,
    }
}

/// The standard xterm 256-colour palette.
fn xterm256(i: u8) -> [u8; 3] {
    const BASE: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            let i = i - 16;
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            [conv(i / 36), conv((i % 36) / 6), conv(i % 6)]
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_sgr_into_grid() {
        let mut term = Terminal::new(TermIo::new());
        // "hi" in default colour, then a red "X".
        term.feed(b"hi\x1b[31mX");
        let cells = term.cells();
        let at = |c: u16, r: u16| cells.iter().find(|cv| cv.col == c && cv.row == r);

        assert_eq!(at(0, 0).unwrap().ch, 'h');
        assert_eq!(at(1, 0).unwrap().ch, 'i');
        let x = at(2, 0).unwrap();
        assert_eq!(x.ch, 'X');
        assert_eq!(x.fg, [205, 0, 0], "SGR 31 = red");
        // Cursor sits just past the last glyph.
        assert_eq!(term.cursor(), Some((3, 0)));
    }

    #[test]
    fn bare_newline_returns_carriage() {
        // With LNM enabled by default, a lone `\n` drops to the next row AND
        // returns to column 0 (so `println!`-style output doesn't stair-step).
        let mut term = Terminal::new(TermIo::new());
        term.feed(b"a\nb");
        let cells = term.cells();
        assert!(cells
            .iter()
            .any(|c| c.ch == 'a' && c.row == 0 && c.col == 0));
        assert!(cells
            .iter()
            .any(|c| c.ch == 'b' && c.row == 1 && c.col == 0));
    }

    #[test]
    fn stdin_pipe_reads_then_closes() {
        let io = TermIo::new();
        io.feed_in(b"abc");
        let mut pipe = InPipe(io.clone());
        let got = pipe.read(10).unwrap();
        assert_eq!(&got[..], b"abc");
        assert!(pipe.read(10).unwrap().is_empty(), "no data left");
        io.close();
        assert!(matches!(pipe.read(10), Err(StreamError::Closed)));
    }

    #[test]
    fn stdout_pipe_drains() {
        let io = TermIo::new();
        io.write_out(b"out");
        assert_eq!(io.drain_out(), b"out");
        assert!(io.drain_out().is_empty());
    }

    #[test]
    fn resize_grows_the_grid_and_publishes_the_size() {
        let io = TermIo::new();
        let mut term = Terminal::new(io.clone());
        assert_eq!(io.size(), (COLS as u16, ROWS as u16));

        // Draw a cell near the bottom-right of a bigger grid, then confirm it's
        // visible only after the grid is resized to include it.
        term.resize(100, 40);
        assert_eq!(io.size(), (100, 40), "size published for the guest to read");
        term.feed(b"\x1b[38;90HX"); // cursor to row 38, col 90 (1-based), print X
        let cells = term.cells();
        assert!(
            cells
                .iter()
                .any(|c| c.ch == 'X' && c.row == 37 && c.col == 89),
            "cell in the enlarged grid is rendered"
        );
    }

    #[test]
    fn raw_mode_follows_the_shared_tty_state() {
        // Raw mode is driven by the guest through wk:tty/control (shared TermIo),
        // not by a byte-stream escape. A fresh terminal is cooked.
        let io = TermIo::new();
        let mut term = Terminal::new(io.clone());
        assert!(!term.is_raw());

        // Cooked: a bare LF gets a carriage return (ONLCR), so text doesn't
        // stair-step.
        term.feed(b"a\nb");
        let cells = term.cells();
        assert!(cells
            .iter()
            .any(|c| c.ch == 'b' && c.col == 0 && c.row == 1));

        // Guest goes raw (canonical off): the terminal reports it, and ONLCR
        // stops (the app is responsible for its own CRLF).
        io.set_tty(false, false);
        assert!(term.is_raw());
        let mut term = Terminal::new(io.clone());
        term.feed(b"a\nb");
        let cells = term.cells();
        assert!(cells
            .iter()
            .any(|c| c.ch == 'b' && c.col == 1 && c.row == 1));
    }

    #[test]
    fn cooked_line_discipline_edits_then_submits() {
        let io = TermIo::new();
        let mut term = Terminal::new(io.clone());
        // Type 'a', 'b', Backspace, 'c', Enter — the line is "ac\n".
        term.key_input(b"ab\x7fc\r", &io);

        let mut pipe = InPipe(io.clone());
        assert_eq!(&pipe.read(64).unwrap()[..], b"ac\n", "whole line on Enter");

        // The grid echoed a and c; the backspaced b was erased.
        let cells = term.cells();
        assert!(cells.iter().any(|c| c.ch == 'a'));
        assert!(cells.iter().any(|c| c.ch == 'c'));
        assert!(!cells.iter().any(|c| c.ch == 'b'), "backspaced char erased");
    }
}
