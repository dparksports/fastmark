// SPDX-License-Identifier: Apache-2.0
//! Application state and winit event handling.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use notify::Watcher as _;

use fastmark_core::{search, Document, Parsed, PositionIndex};

use crate::browse::{visible_toc, FileTree, FilesView, FolderSearch};
use crate::images::{GpuImageSet, ImageStore};
use crate::layout::{buffer_height, BlockEstimator, LayoutCtx, LayoutParams, LINE_H};
use crate::render::{Quad, Renderer};
use crate::theme::{Theme, ViewTheme};

// Logical-pixel UI constants.
const MARGIN_X: f32 = 48.0;
const TOC_W: f32 = 264.0;
const SEARCH_H: f32 = 54.0;
const CONTENT_MAX_W: f32 = 1040.0;

/// Clickable row action, decided at render time.
enum RowAction {
    /// TOC entry jump; when `toggle_zone` > 0 the left zone folds instead.
    TocJump(u32),
    OpenFile(PathBuf),
    /// Toggle a file-tree folder (expand/collapse).
    TreeDir(PathBuf),
    FolderPick(usize),
    /// Open the native folder picker.
    PickFolder,
    /// Point the browser at a different root folder.
    SetRoot(PathBuf),
    /// Bottom-status pills.
    PillFolderSearch,
    PillTheme,
}

struct RowHit {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    toggle_zone: f32,
    action: RowAction,
}

pub struct DocState {
    pub path: PathBuf,
    pub doc: Arc<Document>,
    pub parsed: Arc<Parsed>,
    pub index: PositionIndex,
    pub title: String,
}

pub struct SearchState {
    pub query: String,
    pub hits: Vec<search::Hit>,
    pub cur: usize,
    pub truncated: bool,
}

struct Loaded {
    path: PathBuf,
    doc: Arc<Document>,
    parsed: Arc<Parsed>,
    anchor: Option<usize>,
}

/// A text buffer awaiting render: either a one-shot UI buffer or a cached
/// block buffer shared with the LRU.
enum BufRef {
    Local(Mutex<glyphon::Buffer>),
    Shared(Arc<Mutex<glyphon::Buffer>>),
}

pub struct App {
    pub start_file: Option<PathBuf>,
    pub theme: Theme,
    pub toc_open: bool,
    pub files_sidebar: bool,

    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub layout: Option<LayoutCtx>,
    pub images: ImageStore,
    pub gpu_images: GpuImageSet,

    pub doc: Option<DocState>,
    load_rx: mpsc::Receiver<Loaded>,
    load_tx: mpsc::Sender<Loaded>,
    loading: Arc<AtomicBool>,

    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: mpsc::Receiver<()>,
    watch_ping: Option<Instant>,

    pub scroll_target: f32,
    pub scroll_cur: f32,
    pub zoom: f32,
    pub search: Option<SearchState>,
    pub modifiers: winit::keyboard::ModifiersState,

    // Folder browsing + folder-wide search.
    pub files_view: Option<FilesView>,
    pub folder_search: Option<FolderSearch>,
    folder_launched: String,
    folder_gen: u64,

    // TOC state: collapsed heading blocks + list scroll offset.
    toc_collapsed: HashSet<u32>,
    toc_scroll: usize,

    // File-tree sidebar state.
    tree: Option<FileTree>,
    tree_dirty: bool,
    tree_scroll: usize,
    last_cursor: Option<(f32, f32)>,
    /// Row under the cursor (index into the previous frame's row_hits).
    hot_row: Option<usize>,
    recent: Vec<PathBuf>,

    dragging_scrollbar: bool,
    pending_click: bool,
    row_hits: Vec<RowHit>,
}

impl App {
    pub fn new(start_file: Option<PathBuf>, view_theme: ViewTheme) -> App {
        let (load_tx, load_rx) = mpsc::channel();
        let (watch_tx, watch_rx) = mpsc::channel();
        let watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if res.is_ok() {
                    let _ = watch_tx.send(());
                }
            })
            .ok();
        let theme = match view_theme {
            ViewTheme::Dark => Theme::dark(),
            ViewTheme::Light => Theme::light(),
            ViewTheme::System => Theme::from_system(),
        };
        App {
            start_file,
            theme,
            toc_open: true,
            files_sidebar: true,
            window: None,
            renderer: None,
            layout: None,
            images: ImageStore::new(),
            gpu_images: GpuImageSet::default(),
            doc: None,
            load_rx,
            load_tx,
            loading: Arc::new(AtomicBool::new(false)),
            watcher,
            watch_rx,
            watch_ping: None,
            scroll_target: 0.0,
            scroll_cur: 0.0,
            zoom: 1.0,
            search: None,
            modifiers: Default::default(),
            files_view: None,
            folder_search: None,
            folder_launched: String::new(),
            folder_gen: 0,
            toc_collapsed: HashSet::new(),
            toc_scroll: 0,
            tree: None,
            tree_dirty: true,
            tree_scroll: 0,
            last_cursor: None,
            hot_row: None,
            recent: crate::browse::load_recent(),
            dragging_scrollbar: false,
            pending_click: false,
            row_hits: Vec::new(),
        }
    }

    fn request_load(&mut self, path: PathBuf, anchor: Option<usize>) {
        if self.loading.swap(true, Ordering::Relaxed) {
            // A load is already in flight; the watcher will retrigger.
            self.loading.store(false, Ordering::Relaxed);
            return;
        }
        let tx = self.load_tx.clone();
        let loading = self.loading.clone();
        std::thread::spawn(move || {
            let result = (|| -> Option<Loaded> {
                let doc = Document::load(&path).ok()?;
                let text = doc.text().ok()?;
                let parsed = fastmark_core::parse(text);
                Some(Loaded {
                    path: path.clone(),
                    doc: Arc::new(doc),
                    parsed: Arc::new(parsed),
                    anchor,
                })
            })();
            match result {
                Some(loaded) => {
                    let _ = tx.send(loaded);
                }
                None => loading.store(false, Ordering::Relaxed),
            }
        });
    }

    fn install_watcher(&mut self, path: &std::path::Path) {
        if let Some(w) = self.watcher.as_mut() {
            // Watch the parent directory: editors save via rename-replace,
            // which breaks watches on the file itself.
            if let Some(dir) = path.parent() {
                let dir = if dir.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    dir.to_path_buf()
                };
                let _ = w.watch(&dir, notify::RecursiveMode::NonRecursive);
            }
        }
    }

    fn finish_load(&mut self, loaded: Loaded) {
        let title = loaded
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_string());
        let blocks = loaded.parsed.blocks.len();
        let mut index = PositionIndex::new(blocks);
        if let Some(layout) = self.layout.as_ref() {
            let mut est = BlockEstimator {
                parsed: &loaded.parsed,
                params: layout.params(),
            };
            index.fill_estimates(&mut est);
        }
        let anchor_scroll = loaded.anchor.and_then(|byte| {
            loaded
                .parsed
                .block_at_byte(byte)
                .map(|b| index.offset_of(b))
        });

        self.images.reset();
        self.gpu_images.clear();
        if let Some(layout) = self.layout.as_mut() {
            layout.clear();
        }
        if let Some(y) = anchor_scroll {
            self.scroll_cur = y.max(0.0);
            self.scroll_target = self.scroll_cur;
        }
        self.doc = Some(DocState {
            path: loaded.path.clone(),
            doc: loaded.doc,
            parsed: loaded.parsed,
            index,
            title,
        });
        if let Some(w) = self.window.as_ref() {
            let t = self
                .doc
                .as_ref()
                .map(|d| d.title.clone())
                .unwrap_or_default();
            w.set_title(&format!("{t} — fastmark"));
        }
        self.install_watcher(&loaded.path);
        if let Some(dir) = loaded.path.parent().map(|p| p.to_path_buf()) {
            self.ensure_files_view(dir);
        }
        self.loading.store(false, Ordering::Relaxed);
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    fn render_frame(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if self.layout.is_none() {
            let scale = window.scale_factor() as f32;
            self.layout = Some(LayoutCtx::new(LayoutParams {
                width_px: 800.0 * scale,
                font_px: 15.5 * scale,
            }));
        }
        let layout = self.layout.as_mut().unwrap();

        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let (vw, vh) = (size.width as f32, size.height as f32);
        let theme = self.theme;

        let toc_px = if self.toc_open || self.files_sidebar {
            TOC_W * scale
        } else {
            0.0
        };
        let avail = vw - toc_px - 2.0 * MARGIN_X * scale;
        let col_px = avail.min(CONTENT_MAX_W * scale);
        let font_px = 15.5 * self.zoom * scale;
        let params = LayoutParams {
            width_px: col_px.max(100.0),
            font_px,
        };

        // Hover comes from the cursor position against the previous frame's
        // row map (one frame of latency, zero event plumbing).
        self.hot_row = self.last_cursor.and_then(|(cx, cy)| {
            self.row_hits
                .iter()
                .position(|h| cx >= h.x && cx <= h.x + h.w && cy >= h.y && cy <= h.y + h.h)
        });
        if let Some(w) = self.window.clone() {
            let cursor = if self.hot_row.is_some() {
                winit::window::CursorIcon::Pointer
            } else {
                winit::window::CursorIcon::Default
            };
            w.set_cursor(cursor);
        }

        let mut quads: Vec<Quad> = Vec::new();
        let mut image_draws: Vec<(f32, f32, f32, f32, u32)> = Vec::new();
        let mut bufs: Vec<BufRef> = Vec::new();
        let mut areas_meta: Vec<(f32, f32, glyphon::Color)> = Vec::new();
        self.row_hits.clear();

        // ------------------------------------------------------------------
        // Mode dispatch (exclusive): folder search > file browser > document
        // ------------------------------------------------------------------

        // Folder-wide search results view.
        if let Some(fs) = self.folder_search.as_mut() {
            let bar_h = SEARCH_H * scale;
            let row_h = 34.0 * scale;
            let list_y = bar_h + 10.0 * scale;
            let fit = ((vh - list_y - 44.0 * scale) / row_h).floor().max(3.0) as usize;
            // Keep selection visible, then reborrow shared for rendering.
            if fs.sel < fs.offset {
                fs.offset = fs.sel;
            } else if fs.sel >= fs.offset + fit {
                fs.offset = fs.sel + 1 - fit;
            }
            let fs = &*fs;
            quads.push(Quad::new(0.0, 0.0, vw, bar_h, theme.bg));
            quads.push(Quad::new(0.0, bar_h - 1.0, vw, 1.0, theme.rule));
            let status = if fs.done {
                format!("{} hits", fs.results.len())
            } else {
                format!("{} hits · scanning…", fs.results.len())
            };
            let offset = fs.offset.min(fs.results.len());
            let qbuf = layout.ui_buffer(
                &format!("search folder: {}_", fs.query),
                font_px,
                theme.glyphon(theme.fg),
            );
            bufs.push(BufRef::Local(Mutex::new(qbuf)));
            areas_meta.push((20.0 * scale, 14.0 * scale, theme.glyphon(theme.fg)));
            let sbuf = layout.ui_buffer(&status, font_px * 0.9, theme.glyphon(theme.dim));
            bufs.push(BufRef::Local(Mutex::new(sbuf)));
            areas_meta.push((vw - 190.0 * scale, 16.0 * scale, theme.glyphon(theme.dim)));

            let x0 = toc_px + 24.0 * scale;
            let col2 = 300.0 * scale;
            for (vi, ri) in (offset..fs.results.len()).enumerate().take(fit) {
                let hit = &fs.results[ri];
                let y = list_y + vi as f32 * row_h;
                let selected = ri == fs.sel;
                if selected {
                    quads.push(Quad::new(0.0, y, vw, row_h, theme.overlay_bg));
                    quads.push(Quad::new(0.0, y, 3.0, row_h, theme.accent));
                }
                let name = hit
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let head = format!("{name}:{}", hit.line_no);
                let hbuf = layout.ui_buffer(
                    &head,
                    font_px * 0.92,
                    if selected {
                        theme.glyphon(theme.fg)
                    } else {
                        theme.glyphon(theme.accent)
                    },
                );
                bufs.push(BufRef::Local(Mutex::new(hbuf)));
                areas_meta.push((x0, y + 7.0 * scale, theme.glyphon(theme.fg)));
                let snippet: String = hit.snippet.chars().take(110).collect();
                let sbuf2 = layout.ui_buffer(
                    &snippet,
                    font_px * 0.92,
                    if selected {
                        theme.glyphon(theme.fg)
                    } else {
                        theme.glyphon(theme.dim)
                    },
                );
                bufs.push(BufRef::Local(Mutex::new(sbuf2)));
                areas_meta.push((x0 + col2, y + 7.0 * scale, theme.glyphon(theme.dim)));
                self.row_hits.push(RowHit {
                    x: 0.0,
                    y,
                    w: vw,
                    h: row_h,
                    toggle_zone: 0.0,
                    action: RowAction::FolderPick(ri),
                });
            }
            let fbuf = layout.ui_buffer(
                "Enter open · ↑↓ navigate · Esc close",
                font_px * 0.85,
                theme.glyphon(theme.faint),
            );
            bufs.push(BufRef::Local(Mutex::new(fbuf)));
            areas_meta.push((20.0 * scale, vh - 34.0 * scale, theme.glyphon(theme.faint)));

            let mut guards: Vec<MutexGuard<'_, glyphon::Buffer>> = Vec::with_capacity(bufs.len());
            for b in bufs.iter() {
                match b {
                    BufRef::Local(m) => guards.push(m.lock().unwrap()),
                    BufRef::Shared(a) => guards.push(a.lock().unwrap()),
                }
            }
            let areas: Vec<glyphon::TextArea> = guards
                .iter_mut()
                .zip(areas_meta.iter())
                .map(|(g, &(left, top, color))| glyphon::TextArea {
                    buffer: g,
                    left,
                    top,
                    scale: 1.0,
                    bounds: glyphon::TextBounds {
                        left: 0,
                        top: 0,
                        right: vw as i32,
                        bottom: vh as i32,
                    },
                    default_color: color,
                    custom_glyphs: &[],
                })
                .collect();
            renderer.draw(
                &mut layout.font_system,
                theme.clear(),
                &quads,
                &[],
                &mut self.gpu_images,
                areas,
            );
            return;
        }

        // File browser (no document open).
        if self.doc.is_none() {
            let Some(fv) = self.files_view.as_mut() else {
                let mut hint = layout.ui_buffer(
                    "Drop a Markdown file here  ·  or launch: fastmark <file>",
                    17.0 * scale,
                    theme.glyphon(theme.faint),
                );
                let areas = vec![glyphon::TextArea {
                    buffer: &mut hint,
                    left: (vw - 560.0 * scale).max(0.0) / 2.0 + 20.0 * scale,
                    top: vh / 2.0 - 12.0 * scale,
                    scale: 1.0,
                    bounds: glyphon::TextBounds::default(),
                    default_color: theme.glyphon(theme.fg),
                    custom_glyphs: &[],
                }];
                renderer.draw(
                    &mut layout.font_system,
                    theme.clear(),
                    &[],
                    &[],
                    &mut self.gpu_images,
                    areas,
                );
                return;
            };
            let root = fv.dir.clone();
            let row_base = self.row_hits.len();
            if self.files_sidebar {
                let recent = self.recent.clone();
                draw_tree_sidebar(
                    layout,
                    &theme,
                    &mut quads,
                    &mut bufs,
                    &mut areas_meta,
                    &mut self.row_hits,
                    self.tree.as_ref(),
                    &recent,
                    &root,
                    None,
                    toc_px,
                    vh,
                    scale,
                    font_px,
                    &mut self.tree_scroll,
                    self.hot_row,
                    row_base,
                );
            }
            let dir_name = fv
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| fv.dir.to_string_lossy().into_owned());
            let filtered = fv.filtered();
            let scanning = fv.scanning();
            let filter = fv.filter.clone();
            let sel = fv.sel;
            let row_h = 34.0 * scale;
            let list_y = 150.0 * scale;
            let fit = ((vh - list_y - 50.0 * scale) / row_h).floor().max(3.0) as usize;
            if sel < fv.offset {
                fv.offset = sel;
            } else if sel >= fv.offset + fit {
                fv.offset = sel + 1 - fit;
            }
            let offset = fv.offset.min(filtered.len());
            let x0 = toc_px + 24.0 * scale;
            let radius = 6.0 * scale;

            // Page header: 22px title + clickable breadcrumb subtitle.
            let tbuf = layout.ui_buffer(&dir_name, font_px * 1.45, theme.glyphon(theme.fg));
            bufs.push(BufRef::Local(Mutex::new(tbuf)));
            areas_meta.push((x0, 36.0 * scale, theme.glyphon(theme.fg)));
            let mut bx = x0;
            let mut anc = fv.dir.clone();
            let mut segments: Vec<(String, PathBuf)> = Vec::new();
            loop {
                let name = anc
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| anc.to_string_lossy().into_owned());
                segments.push((name, anc.clone()));
                match anc.parent() {
                    Some(p) if p != anc => anc = p.to_path_buf(),
                    _ => break,
                }
            }
            segments.reverse();
            for (i, (name, path)) in segments.iter().enumerate() {
                if i > 0 {
                    let sep =
                        layout.ui_buffer("\u{203a}", font_px * 0.8, theme.glyphon(theme.faint));
                    bufs.push(BufRef::Local(Mutex::new(sep)));
                    areas_meta.push((bx, 76.0 * scale, theme.glyphon(theme.faint)));
                    bx += 16.0 * scale;
                }
                let is_last = i + 1 == segments.len();
                let color = if is_last {
                    theme.glyphon(theme.dim)
                } else {
                    theme.glyphon(theme.faint)
                };
                let seg = layout.ui_buffer(name, font_px * 0.8, color);
                bufs.push(BufRef::Local(Mutex::new(seg)));
                areas_meta.push((bx, 76.0 * scale, color));
                let seg_w = name.chars().count() as f32 * font_px * 0.52 + 10.0 * scale;
                self.row_hits.push(RowHit {
                    x: bx,
                    y: 68.0 * scale,
                    w: seg_w,
                    h: 24.0 * scale,
                    toggle_zone: 0.0,
                    action: RowAction::SetRoot(path.clone()),
                });
                bx += seg_w;
                if bx > vw - 260.0 * scale {
                    break;
                }
            }

            // Pill bar.
            let mut px_ = x0;
            let pill_h = 26.0 * scale;
            let mut pill = |icon: char, text: &str, accent: bool, px_in: f32| -> f32 {
                let w = text.chars().count() as f32 * font_px * 0.6 + 44.0 * scale;
                quads.push(Quad::pill(
                    px_in,
                    100.0 * scale,
                    w,
                    pill_h,
                    if accent {
                        theme.sel_tint()
                    } else {
                        theme.pill_bg()
                    },
                ));
                let ic = layout.ui_icon(icon, font_px * 0.72, theme.glyphon(theme.dim));
                bufs.push(BufRef::Local(Mutex::new(ic)));
                areas_meta.push((
                    px_in + 10.0 * scale,
                    104.0 * scale,
                    theme.glyphon(theme.dim),
                ));
                let tx = layout.ui_buffer(text, font_px * 0.82, theme.glyphon(theme.dim));
                bufs.push(BufRef::Local(Mutex::new(tx)));
                areas_meta.push((
                    px_in + 26.0 * scale,
                    104.0 * scale,
                    theme.glyphon(theme.dim),
                ));
                w + 8.0 * scale
            };
            px_ += pill(
                crate::layout::icons::DOC,
                &format!("{} files", filtered.len()),
                false,
                px_,
            );
            if scanning {
                px_ += pill(
                    crate::layout::icons::CHEVRON_DOWN,
                    "scanning\u{2026}",
                    true,
                    px_,
                );
            }
            if !filter.is_empty() {
                pill(
                    crate::layout::icons::SEARCH,
                    &format!("filter: {filter}"),
                    true,
                    px_,
                );
            }

            // FAB (top-right): choose folder.
            let fab = 44.0 * scale;
            let fab_x = vw - fab - 28.0 * scale;
            quads.push(Quad::rounded(
                fab_x,
                36.0 * scale,
                fab,
                fab,
                theme.accent,
                fab / 2.0,
            ));
            let fib = layout.ui_icon(
                crate::layout::icons::FOLDER_OPEN,
                font_px * 1.0,
                glyphon::Color::rgb(255, 255, 255),
            );
            bufs.push(BufRef::Local(Mutex::new(fib)));
            areas_meta.push((
                fab_x + fab / 2.0 - font_px * 0.5,
                36.0 * scale + fab / 2.0 - font_px * 0.6,
                glyphon::Color::rgb(255, 255, 255),
            ));
            self.row_hits.push(RowHit {
                x: fab_x,
                y: 36.0 * scale,
                w: fab,
                h: fab,
                toggle_zone: 0.0,
                action: RowAction::PickFolder,
            });

            if filtered.is_empty() && !scanning {
                // Empty state.
                let eib = layout.ui_icon(
                    crate::layout::icons::FOLDER,
                    44.0 * scale,
                    theme.glyphon(theme.faint),
                );
                bufs.push(BufRef::Local(Mutex::new(eib)));
                areas_meta.push((
                    vw / 2.0 - 22.0 * scale,
                    vh / 2.0 - 120.0 * scale,
                    theme.glyphon(theme.faint),
                ));
                let et = layout.ui_buffer(
                    "No Markdown files in this folder",
                    font_px * 1.1,
                    theme.glyphon(theme.dim),
                );
                bufs.push(BufRef::Local(Mutex::new(et)));
                areas_meta.push((
                    vw / 2.0 - 130.0 * scale,
                    vh / 2.0 - 40.0 * scale,
                    theme.glyphon(theme.dim),
                ));
                let bw = 190.0 * scale;
                let bh = 40.0 * scale;
                quads.push(Quad::rounded(
                    vw / 2.0 - bw / 2.0,
                    vh / 2.0 + 10.0 * scale,
                    bw,
                    bh,
                    theme.accent,
                    8.0 * scale,
                ));
                let bb = layout.ui_buffer(
                    "Choose a folder\u{2026}",
                    font_px * 0.95,
                    glyphon::Color::rgb(255, 255, 255),
                );
                bufs.push(BufRef::Local(Mutex::new(bb)));
                areas_meta.push((
                    vw / 2.0 - 62.0 * scale,
                    vh / 2.0 + 20.0 * scale,
                    glyphon::Color::rgb(255, 255, 255),
                ));
                self.row_hits.push(RowHit {
                    x: vw / 2.0 - bw / 2.0,
                    y: vh / 2.0 + 10.0 * scale,
                    w: bw,
                    h: bh,
                    toggle_zone: 0.0,
                    action: RowAction::PickFolder,
                });
            } else {
                for (vi, &ei) in filtered.iter().enumerate().skip(offset).take(fit) {
                    let entry = &fv.entries[ei];
                    let row_i = vi;
                    let y = list_y + (row_i - offset) as f32 * row_h;
                    let selected = row_i == sel;
                    let hot = self.hot_row == Some(self.row_hits.len());
                    if selected {
                        quads.push(Quad::rounded(
                            8.0 * scale,
                            y,
                            vw - 16.0 * scale,
                            row_h,
                            theme.sel_tint(),
                            radius,
                        ));
                        quads.push(Quad::new(
                            8.0 * scale,
                            y + 3.0 * scale,
                            3.0,
                            row_h - 6.0 * scale,
                            theme.accent,
                        ));
                    } else if hot {
                        quads.push(Quad::rounded(
                            8.0 * scale,
                            y,
                            vw - 16.0 * scale,
                            row_h,
                            theme.hover(),
                            radius,
                        ));
                    }
                    let ibuf = layout.ui_icon(
                        crate::layout::icons::DOC,
                        font_px * 0.75,
                        theme.glyphon(theme.dim),
                    );
                    bufs.push(BufRef::Local(Mutex::new(ibuf)));
                    areas_meta.push((x0 + 8.0 * scale, y + 6.0 * scale, theme.glyphon(theme.dim)));
                    let nbuf = layout.ui_buffer(
                        &entry.name,
                        font_px,
                        if selected {
                            theme.glyphon(theme.fg)
                        } else {
                            theme.glyphon(theme.dim)
                        },
                    );
                    bufs.push(BufRef::Local(Mutex::new(nbuf)));
                    areas_meta.push((x0 + 30.0 * scale, y + 7.0 * scale, theme.glyphon(theme.fg)));
                    self.row_hits.push(RowHit {
                        x: 0.0,
                        y,
                        w: vw,
                        h: row_h,
                        toggle_zone: 0.0,
                        action: RowAction::OpenFile(entry.path.clone()),
                    });
                }
            }
            let fbuf = layout.ui_buffer(
                "type to filter · ↑↓ select · Enter open · Ctrl+Shift+F search folder",
                font_px * 0.85,
                theme.glyphon(theme.faint),
            );
            bufs.push(BufRef::Local(Mutex::new(fbuf)));
            areas_meta.push((x0, vh - 38.0 * scale, theme.glyphon(theme.faint)));

            let mut guards: Vec<MutexGuard<'_, glyphon::Buffer>> = Vec::with_capacity(bufs.len());
            for b in bufs.iter() {
                match b {
                    BufRef::Local(m) => guards.push(m.lock().unwrap()),
                    BufRef::Shared(a) => guards.push(a.lock().unwrap()),
                }
            }
            let areas: Vec<glyphon::TextArea> = guards
                .iter_mut()
                .zip(areas_meta.iter())
                .map(|(g, &(left, top, color))| glyphon::TextArea {
                    buffer: g,
                    left,
                    top,
                    scale: 1.0,
                    bounds: glyphon::TextBounds {
                        left: 0,
                        top: 0,
                        right: vw as i32,
                        bottom: vh as i32,
                    },
                    default_color: color,
                    custom_glyphs: &[],
                })
                .collect();
            renderer.draw(
                &mut layout.font_system,
                theme.clear(),
                &quads,
                &[],
                &mut self.gpu_images,
                areas,
            );
            return;
        }

        // Split DocState fields so the mutable position index and the
        // shared parse tree can be used in the same scope.
        let Some(ds) = self.doc.as_mut() else {
            return;
        };
        let DocState {
            path: ds_path,
            doc: ds_doc,
            parsed: ds_parsed,
            index,
            ..
        } = &mut *ds;

        // Current search hit (computed before split borrows complicate it).
        let cur_hit_byte = self
            .search
            .as_ref()
            .and_then(|s| s.hits.get(s.cur).map(|h| h.byte));

        if layout.ensure_params(params) {
            index.reset_measured();
        }
        {
            let mut est = BlockEstimator {
                parsed: ds_parsed,
                params,
            };
            index.fill_estimates(&mut est);
        }

        // Clamp scroll.
        let total = index.total();
        let max_scroll = (total - vh).max(0.0);
        self.scroll_target = self.scroll_target.clamp(0.0, max_scroll);
        let scroll = self.scroll_cur.clamp(0.0, max_scroll);

        let line_px = font_px * LINE_H;
        let content_x = toc_px + ((vw - toc_px - col_px) / 2.0).max(0.0);

        let mut i = index.first_at_or_after(scroll);
        let overscan_bottom = scroll + vh + 600.0;
        let blocks = &ds_parsed.blocks;

        while i < blocks.len() {
            let (y, _h) = index.block_pos(i);
            if y > overscan_bottom {
                break;
            }
            let block = &blocks[i];
            let idx = i as u32;
            match block {
                fastmark_core::Block::Rule => {
                    let (y0, h) = index.block_pos(i);
                    quads.push(Quad::new(
                        content_x,
                        y0 - scroll + h / 2.0 - 1.0,
                        col_px,
                        2.0,
                        theme.rule,
                    ));
                }
                fastmark_core::Block::Image { url, alt } => {
                    let base_dir = ds_path
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_default();
                    let caption = layout.get_or_build(idx, ds_doc, ds_parsed, &theme);
                    let cap_h = buffer_height(&caption.lock().unwrap());
                    if let Some(img) = self.gpu_images.get(idx) {
                        let (dw, dh) = (img.disp_w, img.disp_h);
                        image_draws.push((content_x, y - scroll, dw, dh, idx));
                        bufs.push(BufRef::Shared(caption));
                        areas_meta.push((
                            content_x,
                            y - scroll + dh + 4.0,
                            theme.glyphon(theme.faint),
                        ));
                        index.set_measured(i, dh + cap_h + 8.0);
                    } else {
                        self.images.request(idx, url, &base_dir, col_px);
                        let ph_h = (font_px * 12.0).min(vh * 0.7);
                        quads.push(Quad::new(
                            content_x,
                            y - scroll,
                            col_px,
                            ph_h,
                            theme.code_bg,
                        ));
                        bufs.push(BufRef::Shared(caption));
                        areas_meta.push((
                            content_x,
                            y - scroll + ph_h + 4.0,
                            theme.glyphon(theme.faint),
                        ));
                        index.set_measured(i, ph_h + cap_h + 8.0);
                    }
                    let _ = alt;
                }
                fastmark_core::Block::Code { .. } | fastmark_core::Block::Html { .. } => {
                    let arc = layout.get_or_build(idx, ds_doc, ds_parsed, &theme);
                    let h = buffer_height(&arc.lock().unwrap());
                    quads.push(Quad::new(
                        content_x - 12.0,
                        y - scroll,
                        col_px + 24.0,
                        h + 16.0,
                        theme.code_bg,
                    ));
                    bufs.push(BufRef::Shared(arc));
                    areas_meta.push((content_x, y - scroll + 8.0, theme.glyphon(theme.fg)));
                    index.set_measured(i, h + 16.0);
                }
                fastmark_core::Block::Table { .. } => {
                    let arc = layout.get_or_build(idx, ds_doc, ds_parsed, &theme);
                    let h = buffer_height(&arc.lock().unwrap());
                    quads.push(Quad::new(
                        content_x - 12.0,
                        y - scroll,
                        col_px + 24.0,
                        h + 16.0,
                        theme.code_bg,
                    ));
                    bufs.push(BufRef::Shared(arc));
                    areas_meta.push((content_x, y - scroll + 8.0, theme.glyphon(theme.fg)));
                    index.set_measured(i, h + 16.0);
                }
                fastmark_core::Block::Quote { depth, .. } => {
                    let arc = layout.get_or_build(idx, ds_doc, ds_parsed, &theme);
                    let h = buffer_height(&arc.lock().unwrap());
                    let indent = *depth as f32 * 22.0 * scale;
                    quads.push(Quad::new(
                        content_x + indent - 14.0 * scale,
                        y - scroll,
                        3.0,
                        h,
                        theme.quote_bar,
                    ));
                    bufs.push(BufRef::Shared(arc));
                    areas_meta.push((content_x + indent, y - scroll, theme.glyphon(theme.fg)));
                    index.set_measured(i, h);
                }
                fastmark_core::Block::ListItem { depth, .. } => {
                    let arc = layout.get_or_build(idx, ds_doc, ds_parsed, &theme);
                    let h = buffer_height(&arc.lock().unwrap());
                    let indent = *depth as f32 * 26.0 * scale;
                    bufs.push(BufRef::Shared(arc));
                    areas_meta.push((content_x + indent, y - scroll, theme.glyphon(theme.fg)));
                    index.set_measured(i, h);
                }
                _ => {
                    let arc = layout.get_or_build(idx, ds_doc, ds_parsed, &theme);
                    let h = buffer_height(&arc.lock().unwrap());
                    bufs.push(BufRef::Shared(arc));
                    areas_meta.push((content_x, y - scroll, theme.glyphon(theme.fg)));
                    index.set_measured(i, h);
                }
            }
            i += 1;
        }

        // Search hit marker: accent bar on the current hit's block.
        if let Some(byte) = cur_hit_byte {
            if let Some(b) = ds_parsed.block_at_byte(byte) {
                let (y, _) = index.block_pos(b);
                quads.push(Quad::new(
                    toc_px,
                    y - scroll - 2.0,
                    3.0,
                    line_px * 1.2,
                    theme.accent,
                ));
            }
        }

        // Sidebars: outline (synced, collapsible) or file tree.
        if self.files_sidebar {
            let (root, recent) = self
                .files_view
                .as_ref()
                .map(|fv| (fv.dir.clone(), self.recent.clone()))
                .unwrap_or((PathBuf::from("."), Vec::new()));
            let row_base = self.row_hits.len();
            draw_tree_sidebar(
                layout,
                &theme,
                &mut quads,
                &mut bufs,
                &mut areas_meta,
                &mut self.row_hits,
                self.tree.as_ref(),
                &recent,
                &root,
                Some(ds_path),
                toc_px,
                vh,
                scale,
                font_px,
                &mut self.tree_scroll,
                self.hot_row,
                row_base,
            );
        } else if self.toc_open {
            quads.push(Quad::new(0.0, 0.0, toc_px, vh, theme.sidebar_bg));
            // Active heading = last heading whose offset is above the
            // viewport top (plus a small lead-in). This runs every frame,
            // so the outline always tracks where the user is reading.
            let active_full = {
                let mut active = None;
                for (i, e) in ds_parsed.toc.iter().enumerate() {
                    let y = index.offset_of(e.block as usize);
                    if y <= scroll + 40.0 {
                        active = Some(i);
                    } else {
                        break;
                    }
                }
                active
            };
            let visible = visible_toc(&ds_parsed.toc, &self.toc_collapsed);
            // If the active heading is inside a collapsed branch, highlight
            // its nearest visible ancestor instead.
            let active_vis =
                active_full.map(|a| visible.partition_point(|&v| v <= a).saturating_sub(1));
            let entry_h = 30.0 * scale;
            let fit = ((vh - 26.0 * scale) / entry_h).floor().max(1.0) as usize;
            // Keep the active entry visible in the outline itself.
            if let Some(av) = active_vis {
                if av < self.toc_scroll {
                    self.toc_scroll = av;
                } else if av >= self.toc_scroll + fit {
                    self.toc_scroll = av + 1 - fit;
                }
            }
            let start = self.toc_scroll.min(visible.len());
            for (vi, &ti) in visible.iter().enumerate().skip(start).take(fit) {
                let e = &ds_parsed.toc[ti];
                let y = 18.0 * scale + (vi - start) as f32 * entry_h;
                let is_active = active_vis == Some(vi);
                let indent = (e.level as f32 - 1.0) * 13.0 * scale + 34.0 * scale;
                let hot = self.hot_row == Some(self.row_hits.len());
                if is_active {
                    quads.push(Quad::rounded(
                        8.0 * scale,
                        y - 4.0 * scale,
                        toc_px - 16.0 * scale,
                        entry_h - 2.0 * scale,
                        theme.sel_tint(),
                        6.0 * scale,
                    ));
                    quads.push(Quad::new(
                        8.0 * scale,
                        y - 1.0 * scale,
                        3.0,
                        entry_h - 8.0 * scale,
                        theme.accent,
                    ));
                } else if hot {
                    quads.push(Quad::rounded(
                        8.0 * scale,
                        y - 4.0 * scale,
                        toc_px - 16.0 * scale,
                        entry_h - 2.0 * scale,
                        theme.hover(),
                        6.0 * scale,
                    ));
                }
                // Fold arrow when this heading has children.
                let has_children = ds_parsed
                    .toc
                    .get(ti + 1)
                    .is_some_and(|next| next.level > e.level);
                let arrow = if !has_children {
                    "  "
                } else if self.toc_collapsed.contains(&e.block) {
                    "\u{25b8}"
                } else {
                    "\u{25be}"
                };
                let abuf = layout.ui_buffer(
                    arrow,
                    font_px * 0.8,
                    if has_children {
                        theme.glyphon(theme.dim)
                    } else {
                        theme.glyphon(theme.faint)
                    },
                );
                bufs.push(BufRef::Local(Mutex::new(abuf)));
                areas_meta.push((
                    indent - 22.0 * scale,
                    y + 1.0 * scale,
                    theme.glyphon(theme.dim),
                ));
                let label = if e.text.chars().count() > 30 {
                    format!("{}…", e.text.chars().take(29).collect::<String>())
                } else {
                    e.text.clone()
                };
                let buf = layout.ui_buffer(
                    &label,
                    font_px * if e.level <= 2 { 0.92 } else { 0.84 },
                    if is_active {
                        theme.glyphon(theme.fg)
                    } else {
                        theme.glyphon(theme.dim)
                    },
                );
                bufs.push(BufRef::Local(Mutex::new(buf)));
                areas_meta.push((
                    indent,
                    y,
                    if is_active {
                        theme.glyphon(theme.fg)
                    } else {
                        theme.glyphon(theme.dim)
                    },
                ));
                // One hit per row: the left `toggle_zone` pixels (up to the
                // fold arrow) toggle the branch, the rest jumps.
                self.row_hits.push(RowHit {
                    x: 0.0,
                    y: y - 5.0 * scale,
                    w: toc_px,
                    h: entry_h,
                    toggle_zone: if has_children { indent } else { 0.0 },
                    action: RowAction::TocJump(e.block),
                });
            }
        }

        // Scrollbar.
        if total > vh {
            let track_x = vw - 10.0;
            quads.push(Quad::new(track_x, 0.0, 6.0, vh, theme.sidebar_bg));
            let thumb_h = (vh * vh / total).max(40.0).min(vh);
            let thumb_y = if total - vh > 0.0 {
                scroll / (total - vh) * (vh - thumb_h)
            } else {
                0.0
            };
            quads.push(Quad::new(track_x, thumb_y, 6.0, thumb_h, theme.thumb));
        }

        // Search overlay.
        if let Some(s) = self.search.as_ref() {
            let bar_h = SEARCH_H * scale;
            quads.push(Quad::new(0.0, 0.0, vw, bar_h, theme.bg));
            quads.push(Quad::new(0.0, bar_h - 1.0, vw, 1.0, theme.rule));
            let count = if s.hits.is_empty() {
                "0 hits".to_string()
            } else if s.truncated {
                format!("{}/{}+", s.cur + 1, s.hits.len())
            } else {
                format!("{}/{}", s.cur + 1, s.hits.len())
            };
            let qbuf = layout.ui_buffer(
                &format!("find: {}_", s.query),
                font_px,
                theme.glyphon(theme.fg),
            );
            let cbuf = layout.ui_buffer(&count, font_px * 0.9, theme.glyphon(theme.dim));
            bufs.push(BufRef::Local(Mutex::new(qbuf)));
            areas_meta.push((toc_px + 20.0 * scale, 14.0 * scale, theme.glyphon(theme.fg)));
            bufs.push(BufRef::Local(Mutex::new(cbuf)));
            areas_meta.push((vw - 160.0 * scale, 16.0 * scale, theme.glyphon(theme.dim)));
        }

        // Bottom status bar: breadcrumb + current heading + action pills.
        {
            use crate::layout::icons;
            let bar_h = 28.0 * scale;
            let y0 = vh - bar_h;
            quads.push(Quad::new(0.0, y0, vw, bar_h, theme.sidebar_bg));
            quads.push(Quad::new(0.0, y0, vw, 1.0, theme.rule));
            let mut tx = 16.0 * scale;
            let file_title = ds_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let tbuf = layout.ui_buffer(&file_title, font_px * 0.78, theme.glyphon(theme.dim));
            bufs.push(BufRef::Local(Mutex::new(tbuf)));
            areas_meta.push((tx, y0 + 6.0 * scale, theme.glyphon(theme.dim)));
            tx += file_title.chars().count() as f32 * font_px * 0.48 + 12.0 * scale;
            // Current heading.
            let heading: String = {
                let mut cur = String::new();
                for e in &ds_parsed.toc {
                    if index.offset_of(e.block as usize) <= scroll + 40.0 {
                        cur = e.text.clone();
                    } else {
                        break;
                    }
                }
                cur
            };
            if !heading.is_empty() {
                let cut: String = heading.chars().take(56).collect();
                let sep = layout.ui_buffer("\u{203a}", font_px * 0.72, theme.glyphon(theme.faint));
                bufs.push(BufRef::Local(Mutex::new(sep)));
                areas_meta.push((tx, y0 + 6.0 * scale, theme.glyphon(theme.faint)));
                tx += 14.0 * scale;
                let hbuf = layout.ui_buffer(&cut, font_px * 0.78, theme.glyphon(theme.fg));
                bufs.push(BufRef::Local(Mutex::new(hbuf)));
                areas_meta.push((tx, y0 + 6.0 * scale, theme.glyphon(theme.fg)));
            }
            // Right-side pills: folder search + theme.
            let pill_h = 20.0 * scale;
            let mut draw_pill =
                |x: f32, icon: char, action: RowAction, layout: &mut LayoutCtx| -> f32 {
                    let w = 64.0 * scale;
                    let hot = self.hot_row == Some(self.row_hits.len());
                    let bg = if hot {
                        theme.sel_tint()
                    } else {
                        theme.pill_bg()
                    };
                    quads.push(Quad::pill(x, y0 + 4.0 * scale, w, pill_h, bg));
                    let ic = layout.ui_icon(icon, font_px * 0.68, theme.glyphon(theme.dim));
                    bufs.push(BufRef::Local(Mutex::new(ic)));
                    areas_meta.push((x + 10.0 * scale, y0 + 8.0 * scale, theme.glyphon(theme.dim)));
                    self.row_hits.push(RowHit {
                        x,
                        y: y0 + 4.0 * scale,
                        w,
                        h: pill_h,
                        toggle_zone: 0.0,
                        action,
                    });
                    w + 8.0 * scale
                };
            let mut px_ = vw - 16.0 * scale;
            let w2 = draw_pill(
                px_ - 64.0 * scale,
                icons::SEARCH,
                RowAction::PillFolderSearch,
                layout,
            );
            px_ -= w2;
            let _ = draw_pill(
                px_ - 64.0 * scale,
                icons::MOON,
                RowAction::PillTheme,
                layout,
            );
        }

        // Lock all buffers (in collection order) and build the text areas.
        let mut guards: Vec<MutexGuard<'_, glyphon::Buffer>> = Vec::with_capacity(bufs.len());
        for b in bufs.iter() {
            match b {
                BufRef::Local(m) => guards.push(m.lock().unwrap()),
                BufRef::Shared(a) => guards.push(a.lock().unwrap()),
            }
        }
        let areas: Vec<glyphon::TextArea> = guards
            .iter_mut()
            .zip(areas_meta.iter())
            .map(|(g, &(left, top, color))| glyphon::TextArea {
                buffer: g,
                left,
                top,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 0,
                    top: 0,
                    right: vw as i32,
                    bottom: vh as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            })
            .collect();

        renderer.draw(
            &mut layout.font_system,
            theme.clear(),
            &quads,
            &image_draws,
            &mut self.gpu_images,
            areas,
        );
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn run_search(&mut self) {
        let Some(ds) = self.doc.as_ref() else { return };
        let Some(s) = self.search.as_mut() else {
            return;
        };
        if s.query.is_empty() {
            s.hits.clear();
            s.cur = 0;
            s.truncated = false;
            return;
        }
        let Ok(text) = ds.doc.text() else { return };
        let (hits, truncated) = search::find(text, &s.query, true, 10_000);
        s.hits = hits;
        s.truncated = truncated;
        s.cur = 0;
    }

    fn jump_to_hit(&mut self) {
        let Some(ds) = self.doc.as_mut() else { return };
        let Some(hit) = self.search.as_ref().and_then(|s| s.hits.get(s.cur)) else {
            return;
        };
        if let Some(b) = ds.parsed.block_at_byte(hit.byte) {
            let y = ds.index.offset_of(b);
            let vh = self
                .window
                .as_ref()
                .map(|w| w.inner_size().height as f32)
                .unwrap_or(600.0);
            self.scroll_target = (y - vh * 0.25).max(0.0);
            self.scroll_cur = self.scroll_target;
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.4, 3.0);
        if let Some(ds) = self.doc.as_mut() {
            ds.index.reset_measured();
        }
        if let Some(layout) = self.layout.as_mut() {
            layout.clear();
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn scroll_by(&mut self, delta: f32) {
        self.scroll_target = (self.scroll_target + delta).max(0.0);
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn apply_theme_change(&mut self) {
        if let Some(w) = self.window.as_ref() {
            crate::theme::platform::apply_titlebar_theme(w, self.theme.dark);
        }
        if let Some(layout) = self.layout.as_mut() {
            layout.clear();
        }
        if let Some(ds) = self.doc.as_mut() {
            ds.index.reset_measured();
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn anchor_byte(&mut self) -> Option<usize> {
        let ds = self.doc.as_mut()?;
        let first = ds.index.first_at_or_after(self.scroll_cur);
        Some(
            ds.parsed
                .block_starts
                .get(first)
                .map(|&b| b as usize)
                .unwrap_or(0),
        )
    }

    // ------------------------------------------------------------------
    // Browsing helpers
    // ------------------------------------------------------------------

    fn redraw(&self) {
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Make sure the file list targets `dir` (recreates the walker when the
    /// document's folder changes).
    fn ensure_files_view(&mut self, dir: PathBuf) {
        let needs = self.files_view.as_ref().is_none_or(|f| f.dir != dir);
        if needs {
            self.files_view = Some(FilesView::new(dir));
        }
    }

    /// Open a file at a byte offset: jumps in place when already open,
    /// otherwise loads with an anchor.
    fn open_at(&mut self, path: PathBuf, byte: Option<usize>) {
        if let Some(ds) = self.doc.as_mut() {
            if ds.path == path {
                if let Some(b) = byte.and_then(|b| ds.parsed.block_at_byte(b)) {
                    let y = ds.index.offset_of(b);
                    let vh = self
                        .window
                        .as_ref()
                        .map(|w| w.inner_size().height as f32)
                        .unwrap_or(600.0);
                    self.scroll_target = (y - vh * 0.25).max(0.0);
                    self.scroll_cur = self.scroll_target;
                }
                self.redraw();
                return;
            }
        }
        self.request_load(path, byte);
    }

    /// Start (or restart) a folder-wide search over the current file list.
    fn start_folder_search(&mut self, query: String) {
        let dir = self
            .doc
            .as_ref()
            .and_then(|d| d.path.parent().map(|p| p.to_path_buf()))
            .or_else(|| self.files_view.as_ref().map(|f| f.dir.clone()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        self.ensure_files_view(dir);
        let files: Vec<PathBuf> = self
            .files_view
            .as_ref()
            .map(|f| f.entries.iter().map(|e| e.path.clone()).collect())
            .unwrap_or_default();
        self.folder_gen += 1;
        self.folder_launched = query.clone();
        self.folder_search = Some(FolderSearch::start(files, query, self.folder_gen));
        self.redraw();
    }

    fn dispatch_row(&mut self, action: &RowAction, in_toggle_zone: bool) {
        match action {
            RowAction::TocJump(block) => {
                if in_toggle_zone {
                    if self.toc_collapsed.contains(block) {
                        self.toc_collapsed.remove(block);
                    } else {
                        self.toc_collapsed.insert(*block);
                    }
                } else if let Some(ds) = self.doc.as_mut() {
                    self.scroll_target = ds.index.offset_of(*block as usize).max(0.0);
                    self.scroll_cur = self.scroll_target;
                }
                self.redraw();
            }
            RowAction::OpenFile(path) => self.open_at(path.clone(), None),
            RowAction::TreeDir(path) => {
                if let Some(tree) = self.tree.as_mut() {
                    if tree.expanded.contains(path) {
                        tree.expanded.remove(path);
                    } else {
                        tree.expanded.insert(path.clone());
                    }
                    self.tree_dirty = true;
                }
                self.redraw();
            }
            RowAction::PickFolder => {
                if let Some(w) = self.window.clone() {
                    if let Some(dir) = crate::theme::platform::pick_folder(&w) {
                        self.set_root(dir);
                    }
                }
            }
            RowAction::SetRoot(path) => self.set_root(path.clone()),
            RowAction::PillFolderSearch => {
                let q = self
                    .folder_search
                    .as_ref()
                    .map(|fs| fs.query.clone())
                    .unwrap_or_default();
                self.start_folder_search(q);
            }
            RowAction::PillTheme => {
                self.theme = if self.theme.dark {
                    Theme::light()
                } else {
                    Theme::dark()
                };
                self.apply_theme_change();
            }
            RowAction::FolderPick(i) => {
                if let Some((path, byte)) = self
                    .folder_search
                    .as_ref()
                    .and_then(|fs| fs.results.get(*i))
                    .map(|h| (h.path.clone(), h.byte))
                {
                    self.open_at(path, Some(byte));
                }
            }
        }
    }

    /// Point the whole browser (walker, tree, recent) at a new folder.
    fn set_root(&mut self, dir: PathBuf) {
        crate::browse::push_recent(&dir);
        self.recent = crate::browse::load_recent();
        self.files_view = Some(FilesView::new(dir));
        self.tree_scroll = 0;
        if let Some(tree) = self.tree.as_mut() {
            tree.expanded.clear();
        }
        self.tree_dirty = true;
        self.redraw();
    }

    /// Step the selection in whichever list view is active (browser or
    /// folder search); returns true when consumed.
    fn list_sel_step(&mut self, delta: i32) -> bool {
        if let Some(fs) = self.folder_search.as_mut() {
            if fs.results.is_empty() {
                return true;
            }
            let max = fs.results.len() - 1;
            fs.sel = (fs.sel as i32 + delta).clamp(0, max as i32) as usize;
            self.redraw();
            true
        } else if self.doc.is_none() {
            if let Some(fv) = self.files_view.as_mut() {
                let len = fv.filtered().len();
                if len == 0 {
                    return true;
                }
                fv.sel = (fv.sel as i32 + delta).clamp(0, len as i32 - 1) as usize;
                self.redraw();
            }
            true
        } else {
            false
        }
    }
}

/// Draw the left file-tree sidebar (with FOLDERS + RECENT sections, icons,
/// hover and selection tints) into the shared frame buffers.
#[allow(clippy::too_many_arguments)]
fn draw_tree_sidebar(
    layout: &mut LayoutCtx,
    theme: &Theme,
    quads: &mut Vec<Quad>,
    bufs: &mut Vec<BufRef>,
    areas_meta: &mut Vec<(f32, f32, glyphon::Color)>,
    row_hits: &mut Vec<RowHit>,
    tree: Option<&FileTree>,
    recent: &[PathBuf],
    current_root: &std::path::Path,
    cur_path: Option<&std::path::Path>,
    toc_px: f32,
    vh: f32,
    scale: f32,
    font_px: f32,
    tree_scroll: &mut usize,
    hot_row: Option<usize>,
    row_base: usize,
) {
    use crate::layout::icons;
    let radius = 6.0 * scale;
    quads.push(Quad::new(0.0, 0.0, toc_px, vh, theme.sidebar_bg));
    let label = |layout: &mut LayoutCtx,
                 bufs: &mut Vec<BufRef>,
                 areas_meta: &mut Vec<(f32, f32, glyphon::Color)>,
                 text: &str,
                 y: f32| {
        let b = layout.ui_buffer(
            &text.to_uppercase(),
            font_px * 0.72,
            theme.glyphon(theme.faint),
        );
        bufs.push(BufRef::Local(Mutex::new(b)));
        areas_meta.push((16.0 * scale, y, theme.glyphon(theme.faint)));
    };
    let entry_h = 30.0 * scale;
    let rows = tree.map(|t| t.rows.as_slice()).unwrap_or(&[]);
    let tree_fit = ((vh - 120.0 * scale) / entry_h).floor().max(1.0) as usize;
    *tree_scroll = (*tree_scroll).min(rows.len().saturating_sub(1));

    label(layout, bufs, areas_meta, "folders", 16.0 * scale);
    let mut y = 40.0 * scale;
    let mut row_i = row_base;
    for (vi, row) in rows.iter().enumerate().skip(*tree_scroll).take(tree_fit) {
        let y_row = y + (vi - *tree_scroll) as f32 * entry_h;
        let is_cur = !row.is_dir && cur_path == Some(row.path.as_path());
        let hot = hot_row == Some(row_i);
        if is_cur {
            quads.push(Quad::rounded(
                8.0 * scale,
                y_row,
                toc_px - 16.0 * scale,
                entry_h,
                theme.sel_tint(),
                radius,
            ));
            quads.push(Quad::new(
                8.0 * scale,
                y_row + 3.0 * scale,
                3.0,
                entry_h - 6.0 * scale,
                theme.accent,
            ));
        } else if hot {
            quads.push(Quad::rounded(
                8.0 * scale,
                y_row,
                toc_px - 16.0 * scale,
                entry_h,
                theme.hover(),
                radius,
            ));
        }
        let indent = 12.0 * scale + row.depth as f32 * 14.0 * scale;
        let glyph = if row.is_dir {
            if row.expanded {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            }
        } else {
            icons::DOC
        };
        let gbuf = layout.ui_icon(glyph, font_px * 0.72, theme.glyphon(theme.dim));
        bufs.push(BufRef::Local(Mutex::new(gbuf)));
        areas_meta.push((indent, y_row + 5.0 * scale, theme.glyphon(theme.dim)));
        if row.is_dir {
            let fbuf = layout.ui_icon(icons::FOLDER, font_px * 0.72, theme.glyphon(theme.dim));
            bufs.push(BufRef::Local(Mutex::new(fbuf)));
            areas_meta.push((
                indent + 14.0 * scale,
                y_row + 5.0 * scale,
                theme.glyphon(theme.dim),
            ));
        }
        let label_text = if row.name.chars().count() > 24 {
            let cut: String = row.name.chars().take(23).collect();
            cut + "\u{2026}"
        } else {
            row.name.clone()
        };
        let color = if is_cur {
            theme.glyphon(theme.fg)
        } else {
            theme.glyphon(theme.dim)
        };
        let nbuf = layout.ui_buffer(&label_text, font_px * 0.82, color);
        bufs.push(BufRef::Local(Mutex::new(nbuf)));
        let text_x = if row.is_dir {
            indent + 32.0 * scale
        } else {
            indent + 16.0 * scale
        };
        areas_meta.push((text_x, y_row + 3.0 * scale, color));
        let action = if row.is_dir {
            RowAction::TreeDir(row.path.clone())
        } else {
            RowAction::OpenFile(row.path.clone())
        };
        row_hits.push(RowHit {
            x: 0.0,
            y: y_row,
            w: toc_px,
            h: entry_h,
            toggle_zone: 0.0,
            action,
        });
        row_i += 1;
    }
    y += tree_fit as f32 * entry_h + 10.0 * scale;

    if !recent.is_empty() {
        label(layout, bufs, areas_meta, "recent", y + 4.0 * scale);
        y += 26.0 * scale;
        for dir in recent {
            if y + entry_h > vh - 8.0 {
                break;
            }
            let is_cur = dir == current_root;
            let hot = hot_row == Some(row_i);
            if is_cur {
                quads.push(Quad::rounded(
                    8.0 * scale,
                    y,
                    toc_px - 16.0 * scale,
                    entry_h,
                    theme.sel_tint(),
                    radius,
                ));
                quads.push(Quad::new(
                    8.0 * scale,
                    y + 3.0 * scale,
                    3.0,
                    entry_h - 6.0 * scale,
                    theme.accent,
                ));
            } else if hot {
                quads.push(Quad::rounded(
                    8.0 * scale,
                    y,
                    toc_px - 16.0 * scale,
                    entry_h,
                    theme.hover(),
                    radius,
                ));
            }
            let fbuf = layout.ui_icon(icons::FOLDER, font_px * 0.72, theme.glyphon(theme.dim));
            bufs.push(BufRef::Local(Mutex::new(fbuf)));
            areas_meta.push((24.0 * scale, y + 5.0 * scale, theme.glyphon(theme.dim)));
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned());
            let label_text = if name.chars().count() > 22 {
                let cut: String = name.chars().take(21).collect();
                cut + "\u{2026}"
            } else {
                name
            };
            let color = if is_cur {
                theme.glyphon(theme.fg)
            } else {
                theme.glyphon(theme.dim)
            };
            let nbuf = layout.ui_buffer(&label_text, font_px * 0.82, color);
            bufs.push(BufRef::Local(Mutex::new(nbuf)));
            areas_meta.push((42.0 * scale, y + 3.0 * scale, color));
            row_hits.push(RowHit {
                x: 0.0,
                y,
                w: toc_px,
                h: entry_h,
                toggle_zone: 0.0,
                action: RowAction::SetRoot(dir.clone()),
            });
            row_i += 1;
            y += entry_h;
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("fastmark")
            .with_inner_size(winit::dpi::LogicalSize::new(1180.0, 780.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(560.0, 400.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        crate::theme::platform::apply_titlebar_theme(&window, self.theme.dark);
        self.window = Some(window.clone());
        self.renderer = Some(pollster::block_on(Renderer::new(window.clone())));
        if let Some(file) = self.start_file.take() {
            self.request_load(file, None);
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            self.files_view = Some(FilesView::new(cwd));
        }
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(ds) = self.doc.as_mut() {
                    ds.index.reset_measured();
                }
                if let Some(layout) = self.layout.as_mut() {
                    layout.clear();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => {
                self.request_load(path, None);
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::MouseWheel { delta, .. } => {
                let line_h = self
                    .layout
                    .as_ref()
                    .map(|l| l.params().font_px * LINE_H)
                    .unwrap_or(60.0);
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 3.2 * line_h,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                if self.modifiers.control_key() {
                    self.zoom_by(if dy > 0.0 { 1.1 } else { 1.0 / 1.1 });
                } else if self.files_sidebar && {
                    let sb_scale = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor() as f32)
                        .unwrap_or(1.0);
                    self.last_cursor
                        .is_some_and(|(cx, _)| cx < TOC_W * sb_scale)
                } {
                    let dir = if dy > 0.0 { -1i32 } else { 1 };
                    let rows = self.tree.as_ref().map(|t| t.rows.len()).unwrap_or(0);
                    self.tree_scroll =
                        (self.tree_scroll as i32 + dir).clamp(0, rows as i32) as usize;
                    self.redraw();
                } else if self.folder_search.is_some() || self.doc.is_none() {
                    // List views: wheel steps the selection.
                    let steps = if dy.abs() < 8.0 {
                        0
                    } else {
                        (dy / (line_h.max(1.0) * 3.0)) as i32
                    };
                    let dir = if steps == 0 {
                        -dy.signum() as i32
                    } else {
                        -steps
                    };
                    let _ = self.list_sel_step(dir);
                } else {
                    self.scroll_by(dy);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (state, button) {
                (ElementState::Pressed, MouseButton::Left) => self.pending_click = true,
                (ElementState::Released, _) => {
                    self.dragging_scrollbar = false;
                    self.pending_click = false;
                }
                _ => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                self.last_cursor = Some((x, y));
                if self.dragging_scrollbar {
                    if let Some(ds) = self.doc.as_mut() {
                        let total = ds.index.total();
                        if let Some(w) = self.window.as_ref() {
                            let vh = w.inner_size().height as f32;
                            if total > vh {
                                let thumb_h = (vh * vh / total).max(40.0).min(vh);
                                let frac = ((y - thumb_h / 2.0) / (vh - thumb_h)).clamp(0.0, 1.0);
                                self.scroll_target = frac * (total - vh);
                                self.scroll_cur = self.scroll_target;
                                w.request_redraw();
                            }
                        }
                    }
                } else if self.pending_click {
                    self.pending_click = false;
                    let vw = self
                        .window
                        .as_ref()
                        .map(|w| w.inner_size().width as f32)
                        .unwrap_or(0.0);
                    // Row hits first (content), scrollbar as the fallback.
                    let mut handled = false;
                    for hit in std::mem::take(&mut self.row_hits) {
                        if x >= hit.x && x <= hit.x + hit.w && y >= hit.y && y <= hit.y + hit.h {
                            self.dispatch_row(&hit.action, x < hit.x + hit.toggle_zone);
                            handled = true;
                            break;
                        }
                    }
                    // Any hits not consumed are stale; re-render refreshes them.
                    if !handled && x > vw - 16.0 {
                        self.dragging_scrollbar = true;
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let ctrl = self.modifiers.control_key();
                let shift = self.modifiers.shift_key();
                let folder_mode = self.folder_search.is_some();
                let browser_mode = self.doc.is_none();
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        if folder_mode {
                            self.folder_search = None;
                            self.redraw();
                        } else if self.search.take().is_some() {
                            self.redraw();
                        } else if browser_mode
                            && self
                                .files_view
                                .as_ref()
                                .is_some_and(|f| !f.filter.is_empty())
                        {
                            if let Some(fv) = self.files_view.as_mut() {
                                fv.filter.clear();
                                fv.sel = 0;
                                fv.offset = 0;
                            }
                            self.redraw();
                        } else {
                            event_loop.exit();
                        }
                    }
                    Key::Named(NamedKey::F9) => {
                        self.toc_open = !self.toc_open;
                        if self.toc_open {
                            self.files_sidebar = false;
                        }
                        self.redraw();
                    }
                    Key::Named(NamedKey::F10) => {
                        self.files_sidebar = !self.files_sidebar;
                        if self.files_sidebar {
                            self.toc_open = false;
                            if let Some(dir) = self
                                .doc
                                .as_ref()
                                .and_then(|d| d.path.parent().map(|p| p.to_path_buf()))
                            {
                                self.ensure_files_view(dir);
                            }
                            self.tree_dirty = true;
                        }
                        self.redraw();
                    }
                    Key::Named(NamedKey::F8) => {
                        self.theme = if self.theme.dark {
                            Theme::light()
                        } else {
                            Theme::dark()
                        };
                        self.apply_theme_change();
                    }
                    Key::Named(NamedKey::F3) | Key::Named(NamedKey::Enter) => {
                        if folder_mode {
                            if let Some(fs) = self.folder_search.as_ref() {
                                if let Some(hit) = fs.results.get(fs.sel) {
                                    let (path, byte) = (hit.path.clone(), hit.byte);
                                    self.open_at(path, Some(byte));
                                }
                            }
                        } else if browser_mode {
                            let picked = self.files_view.as_ref().and_then(|fv| {
                                fv.filtered()
                                    .get(fv.sel)
                                    .map(|&i| fv.entries[i].path.clone())
                            });
                            if let Some(path) = picked {
                                self.open_at(path, None);
                            }
                        } else if let Some(s) = self.search.as_mut() {
                            if !s.hits.is_empty() {
                                s.cur = (s.cur + 1) % s.hits.len();
                                self.jump_to_hit();
                            }
                        }
                    }
                    Key::Named(NamedKey::Backspace) => {
                        if folder_mode {
                            if let Some(fs) = self.folder_search.as_mut() {
                                fs.query.pop();
                                fs.edited = Instant::now();
                                self.redraw();
                            }
                        } else if let Some(s) = self.search.as_mut() {
                            s.query.pop();
                            self.run_search();
                            self.jump_to_hit();
                            self.redraw();
                        } else if browser_mode {
                            if let Some(fv) = self.files_view.as_mut() {
                                fv.filter.pop();
                                fv.sel = 0;
                                fv.offset = 0;
                                self.redraw();
                            }
                        }
                    }
                    Key::Named(NamedKey::PageDown) if !folder_mode && !browser_mode => self
                        .scroll_by(
                            self.window
                                .as_ref()
                                .map(|w| w.inner_size().height as f32)
                                .unwrap_or(600.0)
                                * 0.9,
                        ),
                    Key::Named(NamedKey::PageUp) if !folder_mode && !browser_mode => self
                        .scroll_by(
                            -(self
                                .window
                                .as_ref()
                                .map(|w| w.inner_size().height as f32)
                                .unwrap_or(600.0)
                                * 0.9),
                        ),
                    Key::Named(NamedKey::ArrowDown) => {
                        if !self.list_sel_step(1) {
                            self.scroll_by(
                                self.layout
                                    .as_ref()
                                    .map(|l| l.params().font_px * LINE_H)
                                    .unwrap_or(24.0)
                                    * 3.0,
                            );
                        }
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        if !self.list_sel_step(-1) {
                            self.scroll_by(
                                -(self
                                    .layout
                                    .as_ref()
                                    .map(|l| l.params().font_px * LINE_H)
                                    .unwrap_or(24.0)
                                    * 3.0),
                            );
                        }
                    }
                    Key::Named(NamedKey::Home) if folder_mode || browser_mode => {
                        if let Some(fs) = self.folder_search.as_mut() {
                            fs.sel = 0;
                        }
                        if let Some(fv) = self.files_view.as_mut() {
                            fv.sel = 0;
                        }
                        self.redraw();
                    }
                    Key::Named(NamedKey::Home) => {
                        self.scroll_target = 0.0;
                        self.redraw();
                    }
                    Key::Named(NamedKey::End) if folder_mode => {
                        if let Some(fs) = self.folder_search.as_mut() {
                            fs.sel = fs.results.len().saturating_sub(1);
                            self.redraw();
                        }
                    }
                    Key::Named(NamedKey::End) => {
                        if let Some(ds) = self.doc.as_mut() {
                            let vh = self
                                .window
                                .as_ref()
                                .map(|w| w.inner_size().height as f32)
                                .unwrap_or(600.0);
                            self.scroll_target = (ds.index.total() - vh).max(0.0);
                        }
                        self.redraw();
                    }
                    Key::Named(NamedKey::Space)
                        if !folder_mode && !browser_mode && self.search.is_none() =>
                    {
                        self.scroll_by(
                            self.window
                                .as_ref()
                                .map(|w| w.inner_size().height as f32)
                                .unwrap_or(600.0)
                                * 0.85,
                        )
                    }
                    // Folder-wide keyword search across every file.
                    Key::Character(ref c) if ctrl && shift && (c == "f" || c == "F") => {
                        let query = self
                            .folder_search
                            .as_ref()
                            .map(|fs| fs.query.clone())
                            .unwrap_or_default();
                        self.start_folder_search(query);
                    }
                    Key::Character(ref c) if ctrl && (c == "f" || c == "F") => {
                        self.search = Some(SearchState {
                            query: String::new(),
                            hits: Vec::new(),
                            cur: 0,
                            truncated: false,
                        });
                        self.redraw();
                    }
                    Key::Character(ref c) if ctrl && (c == "=" || c == "+") => self.zoom_by(1.15),
                    Key::Character(ref c) if ctrl && c == "-" => self.zoom_by(1.0 / 1.15),
                    Key::Character(ref c) if ctrl && c == "0" => {
                        self.zoom = 1.0;
                        self.zoom_by(1.0);
                    }
                    // Free typing routes to whichever input is active.
                    Key::Character(ref c) if !ctrl && folder_mode => {
                        if let Some(ch) = c.chars().next() {
                            if !ch.is_control() {
                                if let Some(fs) = self.folder_search.as_mut() {
                                    fs.query.push(ch);
                                    fs.edited = Instant::now();
                                    self.redraw();
                                }
                            }
                        }
                    }
                    Key::Character(ref c) if !ctrl && self.search.is_some() => {
                        if let Some(s) = self.search.as_mut() {
                            if let Some(ch) = c.chars().next() {
                                if !ch.is_control() {
                                    s.query.push(ch);
                                    self.run_search();
                                    self.jump_to_hit();
                                    self.redraw();
                                }
                            }
                        }
                    }
                    Key::Character(ref c) if !ctrl && browser_mode => {
                        if let Some(ch) = c.chars().next() {
                            if !ch.is_control() {
                                if let Some(fv) = self.files_view.as_mut() {
                                    fv.filter.push(ch);
                                    fv.sel = 0;
                                    fv.offset = 0;
                                    self.redraw();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                self.render_frame();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Finish pending loads.
        let mut loaded_any = false;
        while let Ok(loaded) = self.load_rx.try_recv() {
            self.finish_load(loaded);
            loaded_any = true;
        }
        if loaded_any {
            self.redraw();
        }

        // File-list walker results invalidate the tree; rebuild when the
        // sidebar is (or is about to be) visible.
        if self.files_view.as_mut().is_some_and(|fv| fv.poll()) {
            self.tree_dirty = true;
            self.redraw();
        }
        if self.tree_dirty && (self.files_sidebar || self.doc.is_none()) {
            if let Some(fv) = self.files_view.as_ref() {
                let root = fv.dir.clone();
                let tree = self.tree.get_or_insert_with(FileTree::new);
                tree.rebuild(&fv.entries, &root);
            }
            self.tree_dirty = false;
        }

        // Folder search: progressive results + debounced re-scan on typing.
        if self.folder_search.as_mut().is_some_and(|fs| fs.poll()) {
            self.redraw();
        }
        if let Some(fs) = self.folder_search.as_ref() {
            if fs.edited.elapsed().as_millis() > 220 && fs.query != self.folder_launched {
                let q = fs.query.clone();
                self.start_folder_search(q);
            }
        }

        // File watcher debounce.
        let mut pinged = false;
        while self.watch_rx.try_recv().is_ok() {
            pinged = true;
        }
        if pinged {
            self.watch_ping = Some(Instant::now());
        }
        if let Some(t) = self.watch_ping {
            if t.elapsed().as_millis() > 250 && !self.loading.load(Ordering::Relaxed) {
                self.watch_ping = None;
                if let Some(path) = self.doc.as_ref().map(|d| d.path.clone()) {
                    let anchor = self.anchor_byte();
                    self.request_load(path, anchor);
                }
            }
        }

        // Image uploads.
        let decoded = self.images.poll();
        if !decoded.is_empty() {
            if let Some(renderer) = self.renderer.as_mut() {
                let col_w = self
                    .layout
                    .as_ref()
                    .map(|l| l.params().width_px)
                    .unwrap_or(800.0);
                let vh = self
                    .window
                    .as_ref()
                    .map(|w| w.inner_size().height as f32)
                    .unwrap_or(800.0);
                for d in decoded {
                    let (w, h) = (d.rgba.width() as f32, d.rgba.height() as f32);
                    let scale = (col_w / w).min(vh * 0.75 / h).min(1.0);
                    let (dw, dh) = (w * scale, h * scale);
                    renderer.upload_image(&mut self.gpu_images, d.block, &d.rgba, dw, dh);
                }
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }

        // Smooth scroll animation.
        if let Some(w) = self.window.as_ref() {
            let diff = self.scroll_target - self.scroll_cur;
            if diff.abs() > 0.5 {
                self.scroll_cur += diff * 0.28;
                w.request_redraw();
            } else if diff.abs() > 0.01 {
                self.scroll_cur = self.scroll_target;
                w.request_redraw();
            }
        }
    }
}
