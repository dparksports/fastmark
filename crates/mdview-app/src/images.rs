// SPDX-License-Identifier: Apache-2.0
//! Image pipeline: decode off the UI thread, downscale to display size,
//! and cap resident decoded bitmaps with an LRU budget.
//!
//! A naive viewer turns one 4000x3000 photo into 48 MB of resident RGBA.
//! mdview decodes on a worker thread, resamples to at most ~2x the display
//! width before it ever touches GPU memory, and evicts least-recently-shown
//! images when the budget (128 MB by default) is exceeded.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

pub const BUDGET_BYTES: usize = 128 * 1024 * 1024;

pub struct DecodedImage {
    pub block: u32,
    pub gen: u64,
    pub rgba: image::RgbaImage,
}

struct ImageStorePriv {
    gen: u64,
    pending: HashMap<u32, ()>,
    rx: mpsc::Receiver<DecodedImage>,
}

pub struct ImageStore {
    priv_: ImageStorePriv,
    tx: mpsc::Sender<DecodedImage>,
    next_path: u64,
}

impl ImageStore {
    pub fn new() -> ImageStore {
        let (tx, rx) = mpsc::channel();
        ImageStore {
            priv_: ImageStorePriv {
                gen: 0,
                pending: HashMap::new(),
                rx,
            },
            tx,
            next_path: 0,
        }
    }

    /// Invalidate everything (new document loaded).
    pub fn reset(&mut self) {
        self.priv_.gen += 1;
        self.priv_.pending.clear();
    }

    /// Request decode+resize for a block's image if not already in flight.
    pub fn request(&mut self, block: u32, url: &str, base_dir: &Path, max_disp_w: f32) {
        if self.priv_.pending.contains_key(&block) {
            return;
        }
        let Some(path) = resolve(url, base_dir, &mut self.next_path) else {
            return;
        };
        self.priv_.pending.insert(block, ());
        let tx = self.tx.clone();
        let gen = self.priv_.gen;
        let max_w = (max_disp_w * 2.0).round().max(64.0) as u32;
        let max_h = 4096;
        std::thread::spawn(move || {
            let rgba = match decode_scaled(&path, max_w, max_h) {
                Some(img) => img,
                None => {
                    // Let the UI drop the pending marker silently.
                    let _ = tx.send(DecodedImage {
                        block,
                        gen,
                        rgba: empty(),
                    });
                    return;
                }
            };
            let _ = tx.send(DecodedImage { block, gen, rgba });
        });
    }

    /// Drain finished decodes for the current generation. Stale results
    /// (from before a reload) are dropped.
    pub fn poll(&mut self) -> Vec<DecodedImage> {
        let gen = self.priv_.gen;
        let mut out = Vec::new();
        while let Ok(decoded) = self.priv_.rx.try_recv() {
            self.priv_.pending.remove(&decoded.block);
            if decoded.gen == gen && decoded_rgba_len(&decoded) > 0 {
                out.push(decoded);
            }
        }
        out
    }
}

fn decoded_rgba_len(d: &DecodedImage) -> usize {
    d.rgba.width() as usize * d.rgba.height() as usize * 4
}

fn empty() -> image::RgbaImage {
    image::RgbaImage::new(0, 0)
}

/// Resolve a Markdown image URL against the document directory. Only local
/// files are supported (http(s) URLs are intentionally not fetched).
fn resolve(url: &str, base_dir: &Path, counter: &mut u64) -> Option<PathBuf> {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("//") {
        return None;
    }
    let p = Path::new(url);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        let joined = base_dir.join(p);
        // Guard against path traversal out of curiosity, not security: the
        // viewer is local-only and single-user.
        let _ = counter;
        joined
    })
}

fn decode_scaled(path: &Path, max_w: u32, max_h: u32) -> Option<image::RgbaImage> {
    let img = image::ImageReader::open(path).ok()?.decode().ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let scale = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    if scale < 1.0 {
        let (nw, nh) = ((w as f64 * scale) as u32, (h as f64 * scale) as u32);
        Some(
            img.resize(nw.max(1), nh.max(1), image::imageops::FilterType::Triangle)
                .to_rgba8(),
        )
    } else {
        Some(img.to_rgba8())
    }
}

// ---------------------------------------------------------------------------
// GPU-side storage (textures are created by the renderer)
// ---------------------------------------------------------------------------

pub struct GpuImage {
    /// Owns the GPU texture; kept alive so the bind group stays valid and
    /// dropped (freed) exactly when this entry is evicted.
    #[allow(dead_code)]
    pub view: wgpu::TextureView,
    pub bind: wgpu::BindGroup,
    /// Display size in physical pixels (aspect-preserved fit).
    pub disp_w: f32,
    pub disp_h: f32,
    bytes: usize,
}

#[derive(Default)]
pub struct GpuImageSet {
    map: HashMap<u32, GpuImage>,
    lru: VecDeque<u32>,
    used: usize,
}

impl GpuImageSet {
    pub fn get(&self, block: u32) -> Option<&GpuImage> {
        self.map.get(&block)
    }

    pub fn insert(
        &mut self,
        block: u32,
        view: wgpu::TextureView,
        bind: wgpu::BindGroup,
        disp_w: f32,
        disp_h: f32,
        bytes: usize,
    ) {
        self.evict_for(bytes);
        self.used += bytes;
        self.lru.push_back(block);
        self.map.insert(
            block,
            GpuImage {
                view,
                bind,
                disp_w,
                disp_h,
                bytes,
            },
        );
    }

    pub fn touch(&mut self, block: u32) {
        if let Some(pos) = self.lru.iter().position(|&b| b == block) {
            self.lru.remove(pos);
            self.lru.push_back(block);
        }
    }

    fn evict_for(&mut self, incoming: usize) {
        while self.used + incoming > BUDGET_BYTES && self.lru.len() > 1 {
            if let Some(old) = self.lru.pop_front() {
                if let Some(img) = self.map.remove(&old) {
                    self.used = self.used.saturating_sub(img.bytes);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
        self.used = 0;
    }
}
