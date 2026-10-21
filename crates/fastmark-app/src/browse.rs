// SPDX-License-Identifier: Apache-2.0
//! Folder browsing and folder-wide search.
//!
//! The file list is built by a background walker (cheap), and folder search
//! reuses the core document loader - every file is mmap'd, scanned
//! line-by-line with the same case-insensitive matcher as in-document
//! search, and dropped. Nothing is ever fully resident: memory stays
//! proportional to one file's touched pages at a time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use fastmark_core::{search, Document};

/// Directory names never worth descending into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
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
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
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
