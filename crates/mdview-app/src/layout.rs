// SPDX-License-Identifier: Apache-2.0
//! Block layout: builds shaped `cosmic-text` buffers for blocks on demand
//! and keeps a small LRU of them. Only blocks near the viewport are ever
//! built; the rest of the document exists as cheap height estimates.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cosmic_text::LineEnding;
use glyphon::{Attrs, AttrsList, BufferLine, Family, Metrics, Shaping, Style, Weight};
use mdview_core::{Block, Document, HeightEstimator, Line, Parsed, Style as MvStyle};

use crate::theme::Theme;

/// Everything that invalidates cached layout when it changes.
#[derive(Clone, Copy, PartialEq)]
pub struct LayoutParams {
    /// Column width in physical pixels.
    pub width_px: f32,
    /// Base font size in physical pixels (already includes DPI + zoom).
    pub font_px: f32,
}

pub const LINE_H: f32 = 1.45;

/// Height of a fully shaped buffer, in physical pixels.
pub fn buffer_height(buf: &glyphon::Buffer) -> f32 {
    let mut h = 0.0f32;
    for run in buf.layout_runs() {
        h = run.line_top + run.line_height;
    }
    if h <= 0.0 {
        buf.metrics().line_height
    } else {
        h
    }
}

fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.85,
        2 => 1.5,
        3 => 1.28,
        4 => 1.14,
        5 => 1.04,
        _ => 1.0,
    }
}

fn style_attrs<'a>(style: MvStyle, base: Attrs<'a>, theme: &'a Theme) -> Attrs<'a> {
    let mut a = base;
    if style.contains(MvStyle::BOLD) {
        a = a.weight(Weight::BOLD);
    }
    if style.contains(MvStyle::ITALIC) {
        a = a.style(Style::Italic);
    }
    if style.contains(MvStyle::MONO) {
        a = a.family(Family::Monospace);
    }
    if style.contains(MvStyle::LINK) {
        a = a.color(theme.glyphon(theme.accent));
    } else if style.contains(MvStyle::DIM) || style.contains(MvStyle::STRIKE) {
        a = a.color(theme.glyphon(theme.dim));
    }
    a
}

/// Build the shaped buffer for one block. Pure function of
/// (block, document, params, theme).
pub fn build_block_buffer(
    font_system: &mut glyphon::FontSystem,
    doc: &Document,
    parsed: &Parsed,
    idx: usize,
    params: LayoutParams,
    theme: &Theme,
) -> glyphon::Buffer {
    let block = &parsed.blocks[idx];
    let font_px = params.font_px;

    let (font_px, mono) = match block {
        Block::Heading { level, .. } => (font_px * heading_scale(*level), false),
        Block::Code { .. } | Block::Html { .. } => (font_px * 0.92, true),
        _ => (font_px, false),
    };
    let metrics = Metrics::new(font_px, font_px * LINE_H);
    let mut buffer = glyphon::Buffer::new(font_system, metrics);
    let base = Attrs::new().family(if mono {
        Family::Monospace
    } else {
        Family::SansSerif
    });

    let mono_scale = 0.92;
    let push_lines =
        |buffer: &mut glyphon::Buffer, lines: &[Line], marker: &str, mono_mode: bool| {
            for (li, line) in lines.iter().enumerate() {
                let mut text = String::with_capacity(line.text_len() as usize + 4);
                if li == 0 && !marker.is_empty() {
                    text.push_str(marker);
                }
                let mut spans: Vec<(std::ops::Range<usize>, Attrs)> = Vec::new();
                if li == 0 && !marker.is_empty() {
                    spans.push((0..marker.len(), base.clone()));
                }
                for piece in &line.pieces {
                    let start = text.len();
                    let s = match &piece.text {
                        mdview_core::PieceText::Source { start: s0, end } => {
                            doc.slice_str((*s0 as usize)..(*end as usize))
                        }
                        mdview_core::PieceText::Synth(s) => *s,
                        mdview_core::PieceText::Owned(s) => s,
                    };
                    text.push_str(s);
                    let attrs = if mono_mode {
                        base.clone()
                    } else {
                        style_attrs(piece.style, base.clone(), theme)
                    };
                    spans.push((start..start + s.len(), attrs));
                }
                let mut attrs_list = AttrsList::new(&base);
                for (range, attrs) in spans {
                    if range.start < range.end {
                        attrs_list.add_span(range, &attrs);
                    }
                }
                buffer.lines.push(BufferLine::new(
                    text,
                    LineEnding::Lf,
                    attrs_list,
                    Shaping::Advanced,
                ));
            }
        };

    match block {
        Block::Heading { lines, .. } | Block::Paragraph { lines } | Block::Quote { lines, .. } => {
            push_lines(&mut buffer, lines, "", false);
        }
        Block::ListItem { lines, marker, .. } => push_lines(&mut buffer, lines, marker, false),
        Block::Code { lines, .. } | Block::Html { lines } => {
            push_lines(&mut buffer, lines, "", true);
        }
        Block::Table { rows, aligns } => {
            // Monospace grid with padded, aligned columns.
            let n_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            let mut widths = vec![2usize; n_cols];
            for row in rows {
                for (c, cell) in row.iter().enumerate() {
                    if c < n_cols {
                        widths[c] =
                            widths[c].max(unicode_width::UnicodeWidthStr::width(cell.as_str()));
                    }
                }
            }
            for (ri, row) in rows.iter().enumerate() {
                let mut text = String::new();
                let mut spans: Vec<(std::ops::Range<usize>, Attrs)> = Vec::new();
                for (c, cell) in row.iter().enumerate() {
                    if c > 0 {
                        text.push_str("  ");
                    }
                    let w = widths.get(c).copied().unwrap_or(0);
                    let cw = unicode_width::UnicodeWidthStr::width(cell.as_str());
                    let (lpad, rpad) = match aligns.get(c).unwrap_or(&mdview_core::TableAlign::None)
                    {
                        mdview_core::TableAlign::Right => (w.saturating_sub(cw), 0),
                        mdview_core::TableAlign::Center => {
                            let p = w.saturating_sub(cw);
                            (p / 2 + p % 2, p / 2)
                        }
                        _ => (0, w.saturating_sub(cw)),
                    };
                    for _ in 0..lpad {
                        text.push(' ');
                    }
                    let start = text.len();
                    text.push_str(cell);
                    let attrs = if ri == 0 {
                        base.clone().weight(Weight::BOLD)
                    } else {
                        base.clone()
                    };
                    spans.push((start..start + cell.len(), attrs));
                    for _ in 0..rpad {
                        text.push(' ');
                    }
                }
                let mut attrs_list = AttrsList::new(&base);
                for (range, attrs) in spans {
                    if range.start < range.end {
                        attrs_list.add_span(range, &attrs);
                    }
                }
                buffer.lines.push(BufferLine::new(
                    text,
                    LineEnding::Lf,
                    attrs_list,
                    Shaping::Advanced,
                ));
                if ri == 0 {
                    let sep: String = widths
                        .iter()
                        .map(|w| "\u{2500}".repeat(w + 2))
                        .collect::<Vec<_>>()
                        .join("\u{253c}");
                    let faint = base.clone().color(theme.glyphon(theme.faint));
                    let mut attrs_list = AttrsList::new(&faint);
                    attrs_list.add_span(0..sep.len(), &faint);
                    buffer.lines.push(BufferLine::new(
                        sep,
                        LineEnding::Lf,
                        attrs_list,
                        Shaping::Advanced,
                    ));
                }
            }
            let _ = mono_scale;
        }
        Block::Image { alt, .. } => {
            // Caption line under the bitmap.
            let text = format!("\u{2197} {alt}");
            let faint = base.color(theme.glyphon(theme.faint));
            let mut attrs_list = AttrsList::new(&faint);
            attrs_list.add_span(0..text.len(), &faint);
            buffer.lines.push(BufferLine::new(
                text,
                LineEnding::Lf,
                attrs_list,
                Shaping::Advanced,
            ));
        }
        Block::Rule => {}
    }

    buffer.set_size(font_system, Some(params.width_px), None);
    buffer.shape_until_scroll(font_system, false);
    buffer
}

/// LRU cache of shaped block buffers.
pub struct LayoutCtx {
    pub font_system: glyphon::FontSystem,
    params: LayoutParams,
    cache: HashMap<u32, Arc<Mutex<glyphon::Buffer>>>,
    lru: VecDeque<u32>,
    cap: usize,
    /// Buffers built since the last frame (for stats).
    pub built_last_frame: u32,
}

impl LayoutCtx {
    pub fn new(params: LayoutParams) -> LayoutCtx {
        LayoutCtx {
            font_system: glyphon::FontSystem::new(),
            params,
            cache: HashMap::new(),
            lru: VecDeque::new(),
            cap: 128,
            built_last_frame: 0,
        }
    }

    pub fn params(&self) -> LayoutParams {
        self.params
    }

    /// Returns true if the params changed (caller must reset estimates).
    pub fn ensure_params(&mut self, params: LayoutParams) -> bool {
        if self.params != params {
            self.params = params;
            self.clear();
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.lru.clear();
    }

    pub fn get_or_build(
        &mut self,
        idx: u32,
        doc: &Document,
        parsed: &Parsed,
        theme: &Theme,
    ) -> Arc<Mutex<glyphon::Buffer>> {
        if let Some(b) = self.cache.get(&idx) {
            // Move to the back of the LRU.
            if let Some(pos) = self.lru.iter().position(|&i| i == idx) {
                self.lru.remove(pos);
                self.lru.push_back(idx);
            }
            return b.clone();
        }
        let buffer = build_block_buffer(
            &mut self.font_system,
            doc,
            parsed,
            idx as usize,
            self.params,
            theme,
        );
        let arc = Arc::new(Mutex::new(buffer));
        self.cache.insert(idx, arc.clone());
        self.lru.push_back(idx);
        while self.lru.len() > self.cap {
            if let Some(old) = self.lru.pop_front() {
                self.cache.remove(&old);
            }
        }
        self.built_last_frame += 1;
        arc
    }

    /// Build a tiny one-off buffer (UI chrome: search bar, empty state).
    pub fn ui_buffer(
        &mut self,
        text: &str,
        font_px: f32,
        color: glyphon::Color,
    ) -> glyphon::Buffer {
        let metrics = Metrics::new(font_px, font_px * LINE_H);
        let mut buffer = glyphon::Buffer::new(&mut self.font_system, metrics);
        buffer.set_text(
            &mut self.font_system,
            text,
            &Attrs::new().family(Family::SansSerif).color(color),
            Shaping::Advanced,
        );
        buffer.set_size(&mut self.font_system, Some(10_000.0), None);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }
}

// ---------------------------------------------------------------------------
// Height estimation (see mdview_core::PositionIndex)
// ---------------------------------------------------------------------------

pub struct BlockEstimator<'a> {
    pub parsed: &'a Parsed,
    pub params: LayoutParams,
}

impl BlockEstimator<'_> {
    fn line_h(&self) -> f32 {
        self.params.font_px * LINE_H
    }
}

impl HeightEstimator for BlockEstimator<'_> {
    fn estimate(&mut self, idx: usize) -> f32 {
        let block = &self.parsed.blocks[idx];
        let lh = self.line_h();
        match block {
            Block::Code { lines, .. } | Block::Html { lines } => {
                lines.len() as f32 * lh * 0.92 + 16.0
            }
            Block::Table { rows, .. } => (rows.len() * 2) as f32 * lh * 0.92 + 16.0,
            Block::Image { .. } => self.params.font_px * 16.0,
            Block::Rule => 2.0,
            Block::Heading { level, .. } => {
                let scale = heading_scale(*level);
                let cpl = (self.params.width_px / (self.params.font_px * 0.52)).max(8.0) / scale;
                let est_lines = (block.text_len() as f32 / cpl)
                    .ceil()
                    .max(block.min_lines() as f32);
                est_lines * lh * scale
            }
            _ => {
                let cpl = (self.params.width_px / (self.params.font_px * 0.52)).max(8.0);
                let est_lines = (block.text_len() as f32 / cpl)
                    .ceil()
                    .max(block.min_lines() as f32);
                est_lines * lh
            }
        }
    }

    fn block_margin(&self, idx: usize) -> f32 {
        let lh = self.line_h();
        match &self.parsed.blocks[idx] {
            Block::Heading { level, .. } => match level {
                1 => lh * 1.1,
                2 => lh * 0.9,
                _ => lh * 0.7,
            },
            Block::Paragraph { .. } => lh * 0.55,
            Block::Quote { .. } => lh * 0.35,
            Block::ListItem { .. } => lh * 0.22,
            Block::Code { .. } | Block::Html { .. } => lh * 0.6,
            Block::Table { .. } => lh * 0.6,
            Block::Image { .. } => lh * 0.5,
            Block::Rule => lh * 0.9,
        }
    }
}
