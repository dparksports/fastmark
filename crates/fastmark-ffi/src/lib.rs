// SPDX-License-Identifier: Apache-2.0
//! C ABI over `fastmark-core` for the native .NET shell.
//!
//! Design: the Rust side owns loading, parsing, and search (the
//! memory-efficient engine); the .NET side owns all UI. Blocks are pulled
//! on demand, one JSON document per *visible* block, through a buffer-fill
//! convention - so a 1.4 M-block document costs the UI nothing until the
//! user scrolls to it.
//!
//! Every string function returns the number of bytes needed (including the
//! NUL terminator). When `buf` is non-null and `cap` is sufficient, the
//! bytes plus NUL are written.

use std::ffi::{c_char, CStr, CString};
use std::sync::Mutex;

use fastmark_core::{Block, Document, Parsed, PieceText, Style};

struct DocBundle {
    doc: Document,
    parsed: Parsed,
}

static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn set_error(msg: String) {
    *LAST_ERROR.lock().unwrap() = Some(msg);
}

fn style_bits(s: Style) -> u32 {
    let mut bits = 0;
    if s.contains(Style::BOLD) {
        bits |= 1;
    }
    if s.contains(Style::ITALIC) {
        bits |= 2;
    }
    if s.contains(Style::MONO) {
        bits |= 4;
    }
    if s.contains(Style::LINK) {
        bits |= 8;
    }
    if s.contains(Style::DIM) {
        bits |= 16;
    }
    if s.contains(Style::STRIKE) {
        bits |= 32;
    }
    bits
}

fn piece_text<'a>(doc: &'a Document, pt: &PieceText) -> std::borrow::Cow<'a, str> {
    match pt {
        PieceText::Source { start, end } => {
            doc.slice_str((*start as usize)..(*end as usize)).into()
        }
        PieceText::Synth(s) => std::borrow::Cow::Borrowed(*s),
        PieceText::Owned(s) => std::borrow::Cow::Owned(s.to_string()),
    }
}

fn lines_json(doc: &Document, lines: &[fastmark_core::Line]) -> serde_json::Value {
    serde_json::Value::Array(
        lines
            .iter()
            .map(|line| {
                let mut text = String::with_capacity(line.text_len() as usize + 4);
                let mut spans = Vec::new();
                for piece in &line.pieces {
                    let start = text.len();
                    let t = piece_text(doc, &piece.text);
                    text.push_str(&t);
                    spans.push(serde_json::json!([start, t.len(), style_bits(piece.style)]));
                }
                serde_json::json!({ "text": text, "spans": spans })
            })
            .collect(),
    )
}

fn block_json(doc: &Document, parsed: &Parsed, idx: usize) -> String {
    let block = &parsed.blocks[idx];
    let value = match block {
        Block::Heading { level, lines } => serde_json::json!({
            "kind": "heading", "level": level, "lines": lines_json(doc, lines),
        }),
        Block::Paragraph { lines } => {
            serde_json::json!({ "kind": "paragraph", "lines": lines_json(doc, lines) })
        }
        Block::Quote { depth, lines } => serde_json::json!({
            "kind": "quote", "depth": depth, "lines": lines_json(doc, lines),
        }),
        Block::ListItem {
            depth,
            marker,
            lines,
        } => serde_json::json!({
            "kind": "item", "depth": depth, "marker": marker,
            "lines": lines_json(doc, lines),
        }),
        Block::Code { lang, lines } => serde_json::json!({
            "kind": "code", "lang": lang, "lines": lines_json(doc, lines),
        }),
        Block::Html { lines } => {
            serde_json::json!({ "kind": "html", "lines": lines_json(doc, lines) })
        }
        Block::Table { rows, aligns } => serde_json::json!({
            "kind": "table", "rows": rows,
            "aligns": aligns.iter().map(|a| match a {
                fastmark_core::TableAlign::None => 0,
                fastmark_core::TableAlign::Left => 1,
                fastmark_core::TableAlign::Center => 2,
                fastmark_core::TableAlign::Right => 3,
            }).collect::<Vec<_>>(),
        }),
        Block::Image { url, alt } => {
            serde_json::json!({ "kind": "image", "url": url, "alt": alt })
        }
        Block::Rule => serde_json::json!({ "kind": "rule" }),
    };
    value.to_string()
}

fn fill(buf: *mut c_char, cap: usize, s: &str) -> usize {
    let needed = s.len() + 1;
    if !buf.is_null() && cap >= needed {
        let bytes = s.as_bytes();
        // SAFETY: caller-provided buffer of `cap` bytes; we write at most
        // `needed` bytes (text + NUL), staying in bounds.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
            *buf.add(bytes.len()) = 0;
        }
    }
    needed
}

macro_rules! doc_ref {
    ($handle:expr) => {{
        if $handle.is_null() {
            return 0;
        }
        // SAFETY: handles originate from fm_open and live until fm_release.
        unsafe { &*($handle as *const DocBundle) }
    }};
}

/// Load and parse a Markdown document. Returns null on failure (details in
/// `fm_last_error`). Caller releases with `fm_release`.
#[no_mangle]
pub extern "C" fn fm_open(path: *const c_char) -> *mut DocBundle {
    if path.is_null() {
        set_error("null path".into());
        return std::ptr::null_mut();
    }
    // SAFETY: C string from the caller, read once.
    let path = unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned();
    match Document::load(std::path::Path::new(&path)) {
        Ok(doc) => match doc.text().ok().map(fastmark_core::parse) {
            Some(parsed) => Box::into_raw(Box::new(DocBundle { doc, parsed })).cast(),
            None => {
                set_error("document is not valid UTF-8 family".into());
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            set_error(format!("{e}"));
            std::ptr::null_mut()
        }
    }
}

/// Release a document handle.
#[no_mangle]
pub extern "C" fn fm_release(handle: *mut DocBundle) {
    if !handle.is_null() {
        // SAFETY: handle came from fm_open; single release by contract.
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[no_mangle]
pub extern "C" fn fm_encoding(handle: *mut DocBundle, buf: *mut c_char, cap: usize) -> usize {
    let b = doc_ref!(handle);
    fill(buf, cap, b.doc.encoding)
}

#[no_mangle]
pub extern "C" fn fm_block_count(handle: *mut DocBundle) -> u32 {
    let b = doc_ref!(handle);
    b.parsed.blocks.len() as u32
}

/// JSON for one block (see lib docs for the schema). Buffer-fill.
#[no_mangle]
pub extern "C" fn fm_block_json(
    handle: *mut DocBundle,
    idx: u32,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    let b = doc_ref!(handle);
    let idx = idx as usize;
    if idx >= b.parsed.blocks.len() {
        set_error("block index out of range".into());
        return 0;
    }
    fill(buf, cap, &block_json(&b.doc, &b.parsed, idx))
}

/// Table of contents: [{level, block, byte, text}]. Buffer-fill.
#[no_mangle]
pub extern "C" fn fm_toc_json(handle: *mut DocBundle, buf: *mut c_char, cap: usize) -> usize {
    let b = doc_ref!(handle);
    let toc: Vec<serde_json::Value> = b
        .parsed
        .toc
        .iter()
        .map(|e| {
            serde_json::json!({
                "level": e.level,
                "block": e.block,
                "byte": b.parsed.block_starts[e.block as usize],
                "text": e.text,
            })
        })
        .collect();
    fill(buf, cap, &serde_json::Value::Array(toc).to_string())
}

/// Case-insensitive substring search in the open document:
/// [{byte, len}]. Buffer-fill.
#[no_mangle]
pub extern "C" fn fm_search_json(
    handle: *mut DocBundle,
    needle: *const c_char,
    limit: u32,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    let b = doc_ref!(handle);
    if needle.is_null() {
        return 0;
    }
    // SAFETY: C string from the caller.
    let needle = unsafe { CStr::from_ptr(needle) }
        .to_string_lossy()
        .into_owned();
    let Ok(text) = b.doc.text() else { return 0 };
    let (hits, _) = fastmark_core::search::find(text, &needle, true, limit as usize);
    let arr: Vec<serde_json::Value> = hits
        .iter()
        .map(|h| serde_json::json!({ "byte": h.byte, "len": h.len }))
        .collect();
    fill(buf, cap, &serde_json::Value::Array(arr).to_string())
}

/// Map a source byte offset to its block index.
#[no_mangle]
pub extern "C" fn fm_byte_to_block(handle: *mut DocBundle, byte: u32) -> u32 {
    let b = doc_ref!(handle);
    b.parsed
        .block_at_byte(byte as usize)
        .map_or(u32::MAX, |i| i as u32)
}

/// Last error as a C string copy. Buffer-fill.
#[no_mangle]
pub extern "C" fn fm_last_error(buf: *mut c_char, cap: usize) -> usize {
    let msg = LAST_ERROR.lock().unwrap().clone().unwrap_or_default();
    fill(buf, cap, &msg)
}

/// Line window around a byte offset: {"snippet": "...", "line": n}.
/// For search-result previews. Buffer-fill.
#[no_mangle]
pub extern "C" fn fm_line_at(
    handle: *mut DocBundle,
    byte: u32,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    let b = doc_ref!(handle);
    let Ok(text) = b.doc.text() else { return 0 };
    let bytes = text.as_bytes();
    let byte = (byte as usize).min(bytes.len());
    let start = bytes[..byte]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let end = bytes[start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |i| start + i);
    let line_no = bytes[..start].iter().filter(|&&c| c == b'\n').count() + 1;
    let mut lo = start.max(byte.saturating_sub(48));
    let mut hi = (byte + 96).min(end);
    while lo > start && !text.is_char_boundary(lo) {
        lo -= 1;
    }
    while hi < end && !text.is_char_boundary(hi) {
        hi += 1;
    }
    let mut snippet = text[lo..hi].replace(['\n', '\r', '\t'], " ");
    if lo > start {
        snippet.insert(0, '\u{2026}');
    }
    if hi < end {
        snippet.push('\u{2026}');
    }
    if snippet.len() > 160 {
        let cut = snippet
            .char_indices()
            .take_while(|(i, _)| *i < 160)
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());
        snippet.truncate(cut);
        snippet.push('\u{2026}');
    }
    fill(
        buf,
        cap,
        &serde_json::json!({ "snippet": snippet, "line": line_no }).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_dump_blocks() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/demo.md");
        let path = CString::new(p.to_string_lossy().as_bytes()).unwrap();
        let h = fm_open(path.as_ptr());
        assert!(!h.is_null());
        let count = fm_block_count(h);
        assert!(count > 10, "demo has plenty of blocks, got {count}");
        // First block is a heading; query with a growing buffer.
        let mut cap = fm_block_json(h, 0, std::ptr::null_mut(), 0);
        assert!(cap > 10);
        let mut buf = vec![0i8; cap];
        cap = fm_block_json(h, 0, buf.as_mut_ptr(), buf.len());
        let s: String = buf[..cap - 1].iter().map(|&b| b as u8 as char).collect();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "heading");
        assert_eq!(v["level"], 1);
        // TOC + search + byte mapping.
        let mut cap = fm_toc_json(h, std::ptr::null_mut(), 0);
        assert!(cap > 10);
        let mut buf2 = vec![0i8; cap];
        fm_toc_json(h, buf2.as_mut_ptr(), buf2.len());
        let needle = CString::new("markdown").unwrap();
        let mut cap = fm_search_json(h, needle.as_ptr(), 100, std::ptr::null_mut(), 0);
        assert!(cap > 2);
        let mut buf3 = vec![0i8; cap];
        fm_search_json(h, needle.as_ptr(), 100, buf3.as_mut_ptr(), buf3.len());
        fm_release(h);
    }

    #[test]
    fn byte_to_block_roundtrip() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/demo.md");
        let path = CString::new(p.to_string_lossy().as_bytes()).unwrap();
        let h = fm_open(path.as_ptr());
        assert!(fm_byte_to_block(h, 0) != u32::MAX);
        fm_release(h);
    }
}
