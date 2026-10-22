// SPDX-License-Identifier: Apache-2.0
//! Folder browsing and folder-wide search.
//!
//! The file list is built by a background walker (cheap), and folder search
//! reuses the core document loader - every file is mmap'd, scanned
//! line-by-line with the same case-insensitive matcher as in-document
//! search, and dropped. Nothing is ever fully resident: memory stays
//! proportional to one file's touched pages at a time.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use fastmark_core::{search, Document};

/// Directory names never worth descending into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    // Windows data trees and legacy junctions - never where user documents
    // live, and junctions can loop.
    "AppData",
    "Application Data",
    "Local Settings",
    "$RECYCLE.BIN",
    "ProgramData",
    ".venv",
    "node_modules",
    "target",
    "out",
    "build",
    "dist",
    "__pycache__",
];
/// Hard cap on scanned files - keeps pathological folders bounded.
const MAX_FILES: usize = 4000;
/// Per-file hit cap during folder search.
const MAX_HITS_PER_FILE: usize = 50;
/// Total result cap for the UI.
pub const MAX_RESULTS: usize = 500;

pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
}

pub struct FilesView {
    pub dir: PathBuf,
    pub entries: Vec<FileEntry>,
    rx: mpsc::Receiver<FileEntry>,
    scanning: bool,
    pub filter: String,
    pub sel: usize,
    pub offset: usize,
}

impl FilesView {
    pub fn new(dir: PathBuf) -> FilesView {
        let (tx, rx) = mpsc::channel();
        let walk_dir = dir.clone();
        std::thread::spawn(move || walk_md(&walk_dir, &tx));
        FilesView {
            dir,
            entries: Vec::new(),
            rx,
            scanning: true,
            filter: String::new(),
            sel: 0,
            offset: 0,
        }
    }

    /// Drain walker results; true when the view changed. The walker drops
    /// its sender on exit, which flips `scanning` off via Disconnected.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(entry) => {
                    self.entries.push(entry);
                    changed = true;
                    if self.entries.len() >= MAX_FILES {
                        self.scanning = false;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.scanning = false;
                    break;
                }
            }
        }
        changed
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    /// Indices of entries whose file name matches the filter (ASCII
    /// case-insensitive substring).
    pub fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle: String = self
            .filter
            .chars()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.to_ascii_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }
}

fn walk_md(dir: &Path, tx: &mpsc::Sender<FileEntry>) {
    let mut stack = vec![dir.to_path_buf()];
    let mut count = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in read.flatten() {
            // DirEntry::metadata is cached from the enumeration on Windows.
            let Ok(md) = entry.metadata() else { continue };
            let path = entry.path();
            if md.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.')
                    || name.starts_with('$')
                    || SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name))
                {
                    continue;
                }
                if is_hidden_or_reparse(&md) {
                    continue;
                }
                stack.push(path);
            } else if md.is_file() {
                if is_hidden_or_reparse(&md) {
                    continue;
                }
                let name = entry.file_name();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if matches!(ext, "md" | "markdown" | "mdx") {
                    if tx
                        .send(FileEntry {
                            path: path.clone(),
                            name: name.to_string_lossy().into_owned(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    count += 1;
                    if count >= MAX_FILES {
                        return;
                    }
                }
            }
        }
    }
}

/// Hidden or reparse-point (junction/symlink) entries never belong in a
/// document browser, and following them risks cycles. No-ops elsewhere.
#[cfg(windows)]
fn is_hidden_or_reparse(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let attrs = md.file_attributes();
    attrs & FILE_ATTRIBUTE_HIDDEN != 0 || attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_hidden_or_reparse(_md: &std::fs::Metadata) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Folder-wide keyword search
// ---------------------------------------------------------------------------

pub struct FolderHit {
    pub path: PathBuf,
    pub byte: usize,
    pub snippet: String,
    pub line_no: usize,
}

pub struct FolderSearch {
    pub query: String,
    pub results: Vec<FolderHit>,
    pub sel: usize,
    pub offset: usize,
    pub done: bool,
    gen: u64,
    rx: mpsc::Receiver<(u64, Option<Vec<FolderHit>>)>,
    pub edited: Instant,
}

impl FolderSearch {
    pub fn start(files: Vec<PathBuf>, query: String, gen: u64) -> FolderSearch {
        let (tx, rx) = mpsc::channel();
        let q = query.clone();
        std::thread::spawn(move || {
            for path in files {
                let batch = scan_file(&path, &q);
                if tx.send((gen, Some(batch))).is_err() {
                    return;
                }
            }
            let _ = tx.send((gen, None));
        });
        FolderSearch {
            query,
            results: Vec::new(),
            sel: 0,
            offset: 0,
            done: false,
            gen,
            rx,
            edited: Instant::now(),
        }
    }

    /// Drain scan batches; true when results changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok((gen, batch)) = self.rx.try_recv() {
            if gen != self.gen {
                continue;
            }
            match batch {
                Some(hits) => {
                    let room = MAX_RESULTS.saturating_sub(self.results.len());
                    self.results.extend(hits.into_iter().take(room));
                    changed = true;
                }
                None => {
                    self.done = true;
                }
            }
        }
        changed
    }
}

fn scan_file(path: &Path, query: &str) -> Vec<FolderHit> {
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(doc) = Document::load(path) else {
        return Vec::new();
    };
    let Ok(text) = doc.text() else {
        return Vec::new();
    };
    let (hits, _) = search::find(text, query, true, MAX_HITS_PER_FILE);
    hits.into_iter()
        .map(|h| {
            let (snippet, line_no) = hit_snippet(text, h.byte);
            FolderHit {
                path: path.to_path_buf(),
                byte: h.byte,
                snippet,
                line_no,
            }
        })
        .collect()
}

/// A short window of the line containing `byte`, plus its 1-based line
/// number, for the results list.
pub fn hit_snippet(text: &str, byte: usize) -> (String, usize) {
    let bytes = text.as_bytes();
    let byte = byte.min(bytes.len());
    let start = bytes[..byte]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let end = bytes[start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |i| start + i);
    let line_no = bytes[..start].iter().filter(|&&c| c == b'\n').count() + 1;

    // Window around the hit, clamped to char boundaries.
    let mut s = start.max(byte.saturating_sub(48));
    let mut e = (byte + 96).min(end);
    while s > start && !text.is_char_boundary(s) {
        s -= 1;
    }
    while e < end && !text.is_char_boundary(e) {
        e += 1;
    }
    let mut snippet = text[s..e].replace(['\n', '\r', '\t'], " ");
    if s > start {
        snippet.insert(0, '…');
    }
    if e < end {
        snippet.push('…');
    }
    if snippet.len() > 140 {
        snippet.truncate(
            snippet
                .char_indices()
                .take_while(|(i, _)| *i < 140)
                .last()
                .map_or(0, |(i, c)| i + c.len_utf8()),
        );
        snippet.push('…');
    }
    (snippet, line_no)
}

/// Compute the visible TOC entries given the collapsed set. A collapsed
/// heading hides all deeper descendants until a sibling-or-shallower
/// heading appears.
pub fn visible_toc(toc: &[fastmark_core::TocEntry], collapsed: &HashSet<u32>) -> Vec<usize> {
    let mut out = Vec::with_capacity(toc.len());
    let mut hide_deeper_than: Option<u8> = None;
    for (i, e) in toc.iter().enumerate() {
        if let Some(max_level) = hide_deeper_than {
            if e.level > max_level {
                continue;
            }
            hide_deeper_than = None;
        }
        out.push(i);
        if collapsed.contains(&e.block) {
            hide_deeper_than = Some(e.level);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// File tree sidebar
// ---------------------------------------------------------------------------

/// One visible row of the left-hand file tree.
pub struct TreeRow {
    pub depth: u32,
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
}

/// A lazily-expanded directory tree derived from the walker's flat file
/// list - folders appear only where Markdown files live beneath them, so
/// there is never a second filesystem scan and empty branches don't clutter
/// the view. `rows` is rebuilt (cheap, one pass) whenever the walker finds
/// new files or a folder is expanded/collapsed.
pub struct FileTree {
    pub expanded: HashSet<PathBuf>,
    pub rows: Vec<TreeRow>,
}

impl FileTree {
    pub fn new() -> FileTree {
        FileTree {
            expanded: HashSet::new(),
            rows: Vec::new(),
        }
    }

    pub fn rebuild(&mut self, entries: &[FileEntry], root: &Path) {
        // children: dir -> (name, path, is_dir); sorted dirs-first, then
        // case-insensitive by name.
        let mut children: HashMap<PathBuf, Vec<(String, PathBuf, bool)>> = HashMap::new();
        for e in entries {
            let Ok(rel) = e.path.strip_prefix(root) else {
                continue;
            };
            let comps: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            if comps.is_empty() {
                continue;
            }
            let mut dir = root.to_path_buf();
            for name in &comps[..comps.len() - 1] {
                let child = dir.join(name);
                children
                    .entry(dir.clone())
                    .or_default()
                    .push((name.clone(), child.clone(), true));
                dir = child;
            }
            children.entry(dir).or_default().push((
                comps.last().unwrap().clone(),
                e.path.clone(),
                false,
            ));
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| {
                b.2.cmp(&a.2)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            });
        }
        let mut rows = Vec::new();
        walk_tree(root, 0, &children, &self.expanded, &mut rows);
        self.rows = rows;
    }
}

fn walk_tree(
    dir: &Path,
    depth: u32,
    children: &HashMap<PathBuf, Vec<(String, PathBuf, bool)>>,
    expanded: &HashSet<PathBuf>,
    rows: &mut Vec<TreeRow>,
) {
    let Some(kids) = children.get(dir) else {
        return;
    };
    for (name, path, is_dir) in kids {
        let exp = expanded.contains(path);
        rows.push(TreeRow {
            depth,
            name: name.clone(),
            path: path.clone(),
            is_dir: *is_dir,
            expanded: exp,
        });
        if *is_dir && exp {
            walk_tree(path, depth + 1, children, expanded, rows);
        }
    }
}

/// Recent folders, persisted as plain lines (most recent first) under
/// %APPDATA%\fastmark\recent.txt (or ~/.config/fastmark elsewhere).
pub fn recent_file() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("fastmark").join("recent.txt")
}

pub fn load_recent() -> Vec<PathBuf> {
    let mut seen: Vec<String> = Vec::new();
    std::fs::read_to_string(recent_file())
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let p = PathBuf::from(l.trim());
                    if !p.is_dir() || is_hidden_dir(&p) {
                        return None;
                    }
                    // Case-insensitive dedupe (paths are unique per folder).
                    let key = p.to_string_lossy().to_lowercase();
                    if seen.contains(&key) {
                        return None;
                    }
                    seen.push(key);
                    Some(p)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Hidden folders (dot-prefixed or Windows hidden attribute) never belong
/// in the recent list.
fn is_hidden_dir(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
        || {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                std::fs::metadata(p)
                    .map(|m| m.file_attributes() & 0x2 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
}

pub fn push_recent(dir: &Path) {
    let mut list = load_recent();
    list.retain(|p| p != dir);
    list.insert(0, dir.to_path_buf());
    list.truncate(8);
    let path = recent_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = list
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(path, body + "\n");
}
