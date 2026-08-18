//! The two modal overlays — the filesystem inspector and the output-log panel —
//! plus their pure geometry (shared by hit-test and draw) and the log-line
//! sanitiser. The drawing itself stays in the compositor; this holds the state
//! and the layout maths.

use super::*;
use std::sync::Mutex;

/// Rows of the inspector listing visible at once.
pub(super) const INSPECT_ROWS: usize = 14;
/// Max bytes previewed of a file.
pub(super) const INSPECT_PREVIEW_CAP: usize = 64 * 1024;

/// Modal state for inspecting one node's virtual filesystem. The listing and
/// any file preview are read live from the node's `fs` each frame, so this
/// holds only the navigation cursor.
pub(super) struct Inspector {
    /// The node whose filesystem is being browsed.
    pub(super) node: NodeId,
    /// Current directory, as an absolute path (`""`/`"/"` = root).
    pub(super) dir: String,
    /// A file within `dir` being previewed, if any.
    pub(super) file: Option<String>,
    /// Scroll offset into the current listing, in rows. An `f32` so trackpad
    /// pixel deltas accumulate; floored when mapping to entries.
    pub(super) scroll: f32,
}

impl Inspector {
    /// Join `dir` and a child `name` into an absolute path.
    pub(super) fn child_path(&self, name: &str) -> String {
        if self.dir.is_empty() {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.dir)
        }
    }
    /// Ascend to the parent directory.
    pub(super) fn go_up(&mut self) {
        self.file = None;
        self.scroll = 0.0;
        match self.dir.rfind('/') {
            Some(0) | None => self.dir.clear(),
            Some(i) => self.dir.truncate(i),
        }
    }
}

/// One cached background fetch: in flight, delivered, or known-absent.
enum Fetch<T> {
    Pending,
    Ready(T, std::time::Instant),
    Gone(std::time::Instant),
}

/// How long a delivered listing/preview stays fresh before a background
/// refresh — a live provider's changes show up without reopening the panel,
/// without hammering it every frame.
const BROWSE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// One shared cache map: fetches keyed by (node, path).
type FetchMap<T> = Arc<Mutex<HashMap<(NodeId, String), Fetch<T>>>>;

/// Listings and previews for inspector paths that cross a provider mount,
/// fetched on background threads. A provider call can block for seconds (a
/// dead provider costs its full timeout), so the render thread only ever
/// reads this cache: a missing entry spawns a fetch and renders empty until
/// the reply lands (usually the next frame — the conduit is an in-process
/// wake). Stale entries are served while a refresh runs behind them.
pub(super) struct ProviderBrowse {
    listings: FetchMap<Vec<wk_server::vfs::DirEntry>>,
    previews: FetchMap<Vec<u8>>,
}

impl ProviderBrowse {
    pub(super) fn new() -> Self {
        ProviderBrowse {
            listings: Arc::new(Mutex::new(HashMap::new())),
            previews: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The cached value for `key`, spawning `load` on a thread when the
    /// entry is missing or stale. Serves the stale value while refreshing.
    fn fetch<T: Clone + Send + 'static>(
        map: &FetchMap<T>,
        key: (NodeId, String),
        load: impl FnOnce() -> Option<T> + Send + 'static,
    ) -> Option<T> {
        let spawn = |key: (NodeId, String)| {
            let map = map.clone();
            std::thread::spawn(move || {
                let got = match load() {
                    Some(v) => Fetch::Ready(v, std::time::Instant::now()),
                    None => Fetch::Gone(std::time::Instant::now()),
                };
                map.lock().unwrap().insert(key, got);
            });
        };
        let mut g = map.lock().unwrap();
        match g.get_mut(&key) {
            Some(Fetch::Pending) => None,
            Some(Fetch::Ready(v, t)) => {
                let out = v.clone();
                if t.elapsed() >= BROWSE_TTL {
                    // Mark fresh so one refresh runs, not one per frame.
                    *t = std::time::Instant::now();
                    drop(g);
                    spawn(key);
                }
                Some(out)
            }
            Some(Fetch::Gone(t)) => {
                if t.elapsed() >= BROWSE_TTL {
                    *t = std::time::Instant::now();
                    drop(g);
                    spawn(key);
                }
                None
            }
            None => {
                g.insert(key.clone(), Fetch::Pending);
                drop(g);
                spawn(key);
                None
            }
        }
    }

    /// The listing for `dir` of `node`'s filesystem, through the cache.
    pub(super) fn listing(
        &self,
        node: NodeId,
        fs: &wk_server::vfs::SharedFs,
        dir: &str,
    ) -> Option<Vec<wk_server::vfs::DirEntry>> {
        let fs = fs.clone();
        let path = dir.to_string();
        Self::fetch(&self.listings, (node, path.clone()), move || {
            wk_server::vfs::list_dir_forwarded(&fs, &path)
        })
    }

    /// A bounded preview of `path` in `node`'s filesystem, through the cache.
    pub(super) fn preview(
        &self,
        node: NodeId,
        fs: &wk_server::vfs::SharedFs,
        path: &str,
        cap: usize,
    ) -> Option<Vec<u8>> {
        let fs = fs.clone();
        let p = path.to_string();
        Self::fetch(&self.previews, (node, p.clone()), move || {
            wk_server::vfs::read_file_forwarded(&fs, &p, cap)
        })
    }
}

/// The inspector's listing for `dir`: local filesystems answer directly on
/// the render thread as always; a path crossing a provider mount goes
/// through the background cache.
pub(super) fn inspect_listing(
    browse: &ProviderBrowse,
    node: &SharedNode,
    dir: &str,
) -> Vec<wk_server::vfs::DirEntry> {
    if wk_server::vfs::path_crosses_provider(&node.fs, dir) {
        browse.listing(node.id, &node.fs, dir).unwrap_or_default()
    } else {
        node.fs.lock().unwrap().list_dir(dir).unwrap_or_default()
    }
}

/// The inspector's file preview, with the same local/provider split.
pub(super) fn inspect_preview(
    browse: &ProviderBrowse,
    node: &SharedNode,
    path: &str,
    cap: usize,
) -> Option<Vec<u8>> {
    if wk_server::vfs::path_crosses_provider(&node.fs, path) {
        browse.preview(node.id, &node.fs, path, cap)
    } else {
        node.fs.lock().unwrap().read_file(path, cap)
    }
}

/// Modal state for viewing one node's output log — its captured stdout/stderr
/// ring (what `wk logs` streams). Content is read live from the node's
/// `term_io` each frame, so this holds only the scroll cursor.
pub(super) struct LogView {
    /// The node whose output is being shown.
    pub(super) node: NodeId,
    /// Lines scrolled up from the bottom. `0` pins the newest line to the
    /// bottom edge (a tail); increasing it reveals older output.
    pub(super) scroll: f32,
}

/// Strip ANSI/terminal control sequences from raw log bytes and split the
/// result into display lines, hard-wrapped at `cols` characters. The output
/// ring captures a node's raw stdout/stderr — for a terminal app that includes
/// escape codes — so the panel renders it as plain, readable text.
pub(super) fn log_lines(bytes: &[u8], cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let text = String::from_utf8_lossy(bytes);
    let mut cleaned = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: ESC [ params… final-byte (0x40..=0x7e).
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] … terminated by BEL or ST (ESC \).
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Other two-byte escapes (charset selects, etc.): drop the pair.
                _ => {}
            },
            '\r' => {} // line breaks come from '\n'; drop bare carriage returns
            '\n' => cleaned.push('\n'),
            '\t' => cleaned.push(' '),
            c if c.is_control() => {} // drop remaining control bytes
            c => cleaned.push(c),
        }
    }
    if cleaned.is_empty() {
        return Vec::new(); // no output → the panel shows its own placeholder
    }
    // A trailing newline would otherwise add a spurious blank final line.
    if cleaned.ends_with('\n') {
        cleaned.pop();
    }
    let mut out = Vec::new();
    for raw in cleaned.split('\n') {
        let chs: Vec<char> = raw.chars().collect();
        if chs.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut i = 0;
        while i < chs.len() {
            let end = (i + cols).min(chs.len());
            out.push(chs[i..end].iter().collect());
            i = end;
        }
    }
    out
}

/// The inspector's interactive regions for a listing: `(panel, close_btn,
/// up_row, entry_rows, preview)`. `entry_rows` pairs a row rect with its entry
/// index (after `scroll`). Pure geometry, shared by the click hit-test and the
/// draw so they never diverge, and unit-testable without a live `App`.
pub(super) type InspectRegions = (
    [f32; 4],
    [f32; 4],
    Option<[f32; 4]>,
    Vec<([f32; 4], usize)>,
    [f32; 4],
);

/// The inspector panel `(x, y, w, h, row_h)` centred on screen `fb`.
pub(super) fn inspect_layout(fb: [f32; 2]) -> (f32, f32, f32, f32, f32) {
    let row_h = MENU_H + 2.0;
    let w = (fb[0] * 0.6).clamp(360.0, 720.0);
    // Title + a listing + a preview strip; bounded to the screen.
    let h = (row_h * (INSPECT_ROWS as f32 + 6.0) + 8.0).min(fb[1] - 80.0);
    let x = (fb[0] - w) * 0.5;
    let y = (fb[1] - h) * 0.5;
    (x, y, w, h, row_h)
}

/// How many listing rows fit between the inspector's title and preview strip.
pub(super) fn inspect_rows_fit(fb: [f32; 2]) -> usize {
    let (_x, y, _w, h, row_h) = inspect_layout(fb);
    let preview_h = (h * 0.34).max(row_h * 3.0);
    let list_top = y + row_h;
    let list_bottom = y + h - preview_h;
    (((list_bottom - list_top) / row_h).floor() as usize).max(1)
}

/// The largest useful scroll offset: past it the last entry has already
/// reached the last row (the ".." row costs one slot of capacity).
pub(super) fn inspect_max_scroll(fb: [f32; 2], n_entries: usize, has_up: bool) -> usize {
    let visible = inspect_rows_fit(fb).saturating_sub(has_up as usize).max(1);
    n_entries.saturating_sub(visible)
}

pub(super) fn inspect_geom(
    fb: [f32; 2],
    n_entries: usize,
    has_up: bool,
    scroll: usize,
) -> InspectRegions {
    let (x, y, w, h, row_h) = inspect_layout(fb);
    let panel = [x, y, x + w, y + h];
    let close = {
        let s = row_h - 8.0;
        [x + w - s - 6.0, y + 4.0, x + w - 6.0, y + 4.0 + s]
    };
    // Bottom third is the preview strip; the listing fills the middle.
    let preview_h = (h * 0.34).max(row_h * 3.0);
    let list_top = y + row_h;
    let list_bottom = y + h - preview_h;
    let preview = [x, list_bottom, x + w, y + h];
    let rows_fit = inspect_rows_fit(fb);

    let up = has_up.then_some([x, list_top, x + w, list_top + row_h]);
    let mut rows = Vec::new();
    // The ".." row takes the first slot when present.
    let mut slot = if has_up { 1 } else { 0 };
    let mut idx = scroll;
    while slot < rows_fit && idx < n_entries {
        let ry = list_top + slot as f32 * row_h;
        rows.push(([x, ry, x + w, ry + row_h], idx));
        slot += 1;
        idx += 1;
    }
    (panel, close, up, rows, preview)
}

/// A compact human-readable byte count for the inspector.
pub(super) fn human_size(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} K", n as f64 / 1024.0)
    } else {
        format!("{:.1} M", n as f64 / (1024.0 * 1024.0))
    }
}
