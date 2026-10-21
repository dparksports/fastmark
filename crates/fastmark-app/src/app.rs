// SPDX-License-Identifier: Apache-2.0
//! Application state and winit event handling.

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

use crate::images::{GpuImageSet, ImageStore};
use crate::layout::{buffer_height, BlockEstimator, LayoutCtx, LayoutParams, LINE_H};
use crate::render::{Quad, Renderer};
use crate::theme::{Theme, ViewTheme};

// Logical-pixel UI constants.
const MARGIN_X: f32 = 48.0;
const TOC_W: f32 = 264.0;
const SEARCH_H: f32 = 54.0;
const CONTENT_MAX_W: f32 = 1040.0;

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

    dragging_scrollbar: bool,
    pending_click: bool,
    toc_hits: Vec<(f32, f32, f32, f32, u32)>, // x, y, w, h, block idx
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
            dragging_scrollbar: false,
            pending_click: false,
            toc_hits: Vec::new(),
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

        let toc_px = if self.toc_open { TOC_W * scale } else { 0.0 };
        let avail = vw - toc_px - 2.0 * MARGIN_X * scale;
        let col_px = avail.min(CONTENT_MAX_W * scale);
        let font_px = 15.5 * self.zoom * scale;
        let params = LayoutParams {
            width_px: col_px.max(100.0),
            font_px,
        };

        let mut quads: Vec<Quad> = Vec::new();
        let mut image_draws: Vec<(f32, f32, f32, f32, u32)> = Vec::new();
        let mut bufs: Vec<BufRef> = Vec::new();
        let mut areas_meta: Vec<(f32, f32, glyphon::Color)> = Vec::new();

        // No document: centered hint.
        let Some(ds) = self.doc.as_mut() else {
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

        // Split DocState fields so the mutable position index and the
        // shared parse tree can be used in the same scope.
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

        // TOC sidebar.
        self.toc_hits.clear();
        if self.toc_open {
            quads.push(Quad::new(0.0, 0.0, toc_px, vh, theme.sidebar_bg));
            let cur_block = {
                let mut cur = None;
                for e in &ds_parsed.toc {
                    let y = index.offset_of(e.block as usize);
                    if y <= scroll + 40.0 {
                        cur = Some(e.block);
                    } else {
                        break;
                    }
                }
                cur
            };
            let mut ty = 18.0 * scale;
            let entry_h = 30.0 * scale;
            for e in ds_parsed.toc.iter().take(400) {
                if ty + entry_h > vh {
                    break;
                }
                let indent = (e.level as f32 - 1.0) * 13.0 * scale + 20.0 * scale;
                let is_cur = cur_block == Some(e.block);
                if is_cur {
                    quads.push(Quad::new(
                        0.0,
                        ty - 5.0 * scale,
                        toc_px,
                        entry_h,
                        theme.overlay_bg,
                    ));
                    quads.push(Quad::new(0.0, ty - 5.0 * scale, 3.0, entry_h, theme.accent));
                }
                let label = if e.text.chars().count() > 34 {
                    format!("{}…", e.text.chars().take(33).collect::<String>())
                } else {
                    e.text.clone()
                };
                let buf = layout.ui_buffer(
                    &label,
                    font_px * if e.level <= 2 { 0.92 } else { 0.84 },
                    if is_cur {
                        theme.glyphon(theme.fg)
                    } else {
                        theme.glyphon(theme.dim)
                    },
                );
                bufs.push(BufRef::Local(Mutex::new(buf)));
                areas_meta.push((
                    indent,
                    ty,
                    if is_cur {
                        theme.glyphon(theme.fg)
                    } else {
                        theme.glyphon(theme.dim)
                    },
                ));
                self.toc_hits
                    .push((0.0, ty - 5.0 * scale, toc_px, entry_h, e.block));
                ty += entry_h;
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
                    let scale = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor() as f32)
                        .unwrap_or(1.0);
                    let vw = self
                        .window
                        .as_ref()
                        .map(|w| w.inner_size().width as f32)
                        .unwrap_or(0.0);
                    if self.toc_open && x < TOC_W * scale {
                        for &(_hx, hy, _hw, hh, block) in &self.toc_hits {
                            if y >= hy && y <= hy + hh {
                                if let Some(ds) = self.doc.as_mut() {
                                    self.scroll_target =
                                        ds.index.offset_of(block as usize).max(0.0);
                                    self.scroll_cur = self.scroll_target;
                                    if let Some(w) = self.window.as_ref() {
                                        w.request_redraw();
                                    }
                                }
                                break;
                            }
                        }
                    } else if x > vw - 16.0 {
                        self.dragging_scrollbar = true;
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let ctrl = self.modifiers.control_key();
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        if self.search.take().is_some() {
                            if let Some(w) = self.window.as_ref() {
                                w.request_redraw();
                            }
                        } else {
                            event_loop.exit();
                        }
                    }
                    Key::Named(NamedKey::F9) => {
                        self.toc_open = !self.toc_open;
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
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
                        if let Some(s) = self.search.as_mut() {
                            if !s.hits.is_empty() {
                                s.cur = (s.cur + 1) % s.hits.len();
                                self.jump_to_hit();
                            }
                        }
                    }
                    Key::Named(NamedKey::Backspace) => {
                        if let Some(s) = self.search.as_mut() {
                            s.query.pop();
                            self.run_search();
                            self.jump_to_hit();
                            if let Some(w) = self.window.as_ref() {
                                w.request_redraw();
                            }
                        }
                    }
                    Key::Named(NamedKey::PageDown) => self.scroll_by(
                        self.window
                            .as_ref()
                            .map(|w| w.inner_size().height as f32)
                            .unwrap_or(600.0)
                            * 0.9,
                    ),
                    Key::Named(NamedKey::PageUp) => self.scroll_by(
                        -(self
                            .window
                            .as_ref()
                            .map(|w| w.inner_size().height as f32)
                            .unwrap_or(600.0)
                            * 0.9),
                    ),
                    Key::Named(NamedKey::ArrowDown) => self.scroll_by(
                        self.layout
                            .as_ref()
                            .map(|l| l.params().font_px * LINE_H)
                            .unwrap_or(24.0)
                            * 3.0,
                    ),
                    Key::Named(NamedKey::ArrowUp) => self.scroll_by(
                        -(self
                            .layout
                            .as_ref()
                            .map(|l| l.params().font_px * LINE_H)
                            .unwrap_or(24.0)
                            * 3.0),
                    ),
                    Key::Named(NamedKey::Home) => {
                        self.scroll_target = 0.0;
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
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
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                    }
                    Key::Named(NamedKey::Space) if self.search.is_none() => self.scroll_by(
                        self.window
                            .as_ref()
                            .map(|w| w.inner_size().height as f32)
                            .unwrap_or(600.0)
                            * 0.85,
                    ),
                    Key::Character(ref c) if ctrl && (c == "f" || c == "F") => {
                        self.search = Some(SearchState {
                            query: String::new(),
                            hits: Vec::new(),
                            cur: 0,
                            truncated: false,
                        });
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                    }
                    Key::Character(ref c) if ctrl && (c == "=" || c == "+") => self.zoom_by(1.15),
                    Key::Character(ref c) if ctrl && c == "-" => self.zoom_by(1.0 / 1.15),
                    Key::Character(ref c) if ctrl && c == "0" => {
                        self.zoom = 1.0;
                        self.zoom_by(1.0);
                    }
                    Key::Character(ref c) if !ctrl && self.search.is_some() => {
                        if let Some(s) = self.search.as_mut() {
                            if let Some(ch) = c.chars().next() {
                                if !ch.is_control() {
                                    s.query.push(ch);
                                    self.run_search();
                                    self.jump_to_hit();
                                    if let Some(w) = self.window.as_ref() {
                                        w.request_redraw();
                                    }
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
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
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
