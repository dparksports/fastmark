// SPDX-License-Identifier: Apache-2.0
//! Streaming Markdown parse into a flat block model.
//!
//! `pulldown-cmark` is a pull parser: instead of building a DOM, it emits
//! events. We consume them once and produce a flat `Vec<Block>` where every
//! rendered glyph is still addressed by *source byte range* - the only owned
//! text is what cannot be sliced out of the source (table cells, synthesized
//! list markers, un-borrowable inline snippets).
//!
//! Consequences:
//! - parse memory is O(blocks + style transitions), never O(file) for text
//! - re-parsing a 100 MB file costs one streaming pass
//! - byte offsets recorded here power search jumps and reload anchoring

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Inline style bit set applied to a piece of rendered text.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Style(pub u8);

impl Style {
    pub const BOLD: Style = Style(1 << 0);
    pub const ITALIC: Style = Style(1 << 1);
    pub const MONO: Style = Style(1 << 2);
    pub const LINK: Style = Style(1 << 3);
    pub const DIM: Style = Style(1 << 4);
    pub const STRIKE: Style = Style(1 << 5);

    pub const fn with(self, other: Style) -> Style {
        Style(self.0 | other.0)
    }
    pub const fn without(self, other: Style) -> Style {
        Style(self.0 & !other.0)
    }
    pub const fn contains(self, other: Style) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Where the rendered text of a piece comes from.
pub enum PieceText {
    /// Slice of the document source (the zero-copy fast path).
    Source { start: u32, end: u32 },
    /// Fixed text synthesized by the viewer (list markers, breaks).
    Synth(&'static str),
    /// Text that could not be addressed as a source slice; owned copy.
    /// Rare: inline code with entities, footnote refs, table cells.
    Owned(Box<str>),
}

impl PieceText {
    pub fn len(&self) -> u32 {
        match self {
            PieceText::Source { start, end } => end - start,
            PieceText::Synth(s) => s.len() as u32,
            PieceText::Owned(s) => s.len() as u32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One rendered line (soft wraps happen later, in the text engine).
pub struct Piece {
    /// Byte offset of this piece within the line's concatenated text.
    pub dst: u32,
    pub text: PieceText,
    pub style: Style,
}

#[derive(Default)]
pub struct Line {
    pub pieces: Vec<Piece>,
}

impl Line {
    pub fn text_len(&self) -> u32 {
        self.pieces.last().map_or(0, |p| p.dst + p.text.len())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableAlign {
    None,
    Left,
    Center,
    Right,
}

/// A top-level block. Paragraph-like blocks own `lines`; text stays in the
/// source mapping and is only copied into a shaped buffer when (and for as
/// long as) the block is near the viewport.
pub enum Block {
    Heading {
        level: u8,
        lines: Vec<Line>,
    },
    Paragraph {
        lines: Vec<Line>,
    },
    Quote {
        depth: u8,
        lines: Vec<Line>,
    },
    ListItem {
        depth: u8,
        marker: String,
        lines: Vec<Line>,
    },
    Code {
        lang: Option<String>,
        lines: Vec<Line>,
    },
    Html {
        lines: Vec<Line>,
    },
    Table {
        rows: Vec<Vec<String>>,
        aligns: Vec<TableAlign>,
    },
    Image {
        url: String,
        alt: String,
    },
    Rule,
}

pub struct TocEntry {
    pub level: u8,
    /// Index into `Parsed::blocks`.
    pub block: u32,
    pub text: String,
}

pub struct Parsed {
    pub blocks: Vec<Block>,
    pub toc: Vec<TocEntry>,
    /// First source byte of each block, ascending; binary-search target for
    /// byte -> block jumps (search hits, reload anchors).
    pub block_starts: Vec<u32>,
}

impl Parsed {
    /// Block index containing the given source byte offset. Bytes before
    /// the first recorded start belong to block 0.
    pub fn block_at_byte(&self, byte: usize) -> Option<usize> {
        if self.block_starts.is_empty() {
            return None;
        }
        let byte = byte as u32;
        Some(
            self.block_starts
                .partition_point(|&s| s <= byte)
                .saturating_sub(1),
        )
    }
}

impl Block {
    /// Total rendered characters, for scroll height estimation.
    pub fn text_len(&self) -> usize {
        fn lines_len(lines: &[Line]) -> usize {
            lines.iter().map(|l| l.text_len() as usize).sum()
        }
        match self {
            Block::Heading { lines, .. }
            | Block::Paragraph { lines }
            | Block::Quote { lines, .. }
            | Block::ListItem { lines, .. }
            | Block::Code { lines, .. }
            | Block::Html { lines } => lines_len(lines),
            Block::Table { rows, .. } => rows.iter().flatten().map(|c| c.len()).sum(),
            Block::Image { .. } => 64,
            Block::Rule => 0,
        }
    }

    /// Minimum number of laid-out lines (hard breaks / code lines / rows).
    pub fn min_lines(&self) -> usize {
        match self {
            Block::Heading { lines, .. }
            | Block::Paragraph { lines }
            | Block::Quote { lines, .. }
            | Block::ListItem { lines, .. }
            | Block::Code { lines, .. }
            | Block::Html { lines } => lines.len().max(1),
            Block::Table { rows, .. } => rows.len().max(1),
            Block::Image { .. } => 3,
            Block::Rule => 1,
        }
    }
}

pub fn parse(text: &str) -> Parsed {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut b = Builder::default();
    for (ev, range) in Parser::new_ext(text, opts).into_offset_iter() {
        b.event(ev, range, text);
    }
    b.finish()
}

// ---------------------------------------------------------------------------
// Single-pass builder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    toc: Vec<TocEntry>,
    starts: Vec<u32>,
    min_byte: Option<u32>,

    // container state
    quote_depth: u8,
    list_depth: u8,
    ordered: Vec<Option<u64>>,
    inside_item: u32,
    para_suppressed: bool,

    // inline style state
    style: Style,

    // current rich block
    rich: Option<Rich>,
    code: Option<CodeAccum>,
    table: Option<TableAccum>,

    // heading text accumulation (for the TOC)
    heading_level: Option<u8>,
    toc_text: String,

    // block-image detection
    image_url: Option<String>,
    image_alt: String,
    image_candidate: bool,
}

struct Rich {
    kind: RichKind,
    lines: Vec<Line>,
    cur: Vec<Piece>,
    dst: u32,
    image: Option<(String, String)>,
}

enum RichKind {
    Paragraph,
    Heading(u8),
    Quote(u8),
    Item { depth: u8, marker: String },
}

struct CodeAccum {
    lang: Option<String>,
    lines: Vec<Line>,
}

struct TableAccum {
    aligns: Vec<TableAlign>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
}

impl Builder {
    fn event(&mut self, ev: Event, range: Range<usize>, text: &str) {
        let r = (range.start as u32, range.end as u32);
        match ev {
            Event::Start(tag) => self.start_tag(tag, range.start as u32),
            Event::End(end) => self.end_tag(end),
            Event::Text(t) => {
                if self.table.is_some() {
                    if let Some(tb) = self.table.as_mut() {
                        if let Some(c) = tb.row.last_mut() {
                            c.push_str(&t)
                        }
                    }
                } else if self.code.is_some() {
                    self.code_chunk(r, &t, text);
                } else {
                    if let Some(alt) = self.image_alt_pending() {
                        alt.push_str(&t);
                    }
                    if self.heading_level.is_some() {
                        self.toc_text.push_str(&t);
                    }
                    self.push_piece(r, &t, text, Style::default());
                }
            }
            Event::Code(t) => {
                if self.table.is_some() {
                    if let Some(tb) = self.table.as_mut() {
                        if let Some(c) = tb.row.last_mut() {
                            c.push_str(&t)
                        }
                    }
                } else {
                    if self.heading_level.is_some() {
                        self.toc_text.push_str(&t);
                    }
                    self.push_piece(r, &t, text, Style::MONO);
                }
            }
            Event::InlineHtml(t) => {
                self.push_owned(t.to_string(), Style::DIM.with(Style::MONO));
            }
            Event::Html(t) => {
                self.flush_rich();
                let mut acc = CodeAccum {
                    lang: None,
                    lines: Vec::new(),
                };
                code_chunk_into(&mut acc, r, &t, text);
                if !acc.lines.is_empty() {
                    self.push_block(Block::Html { lines: acc.lines }, Some(range.start as u32));
                }
            }
            Event::SoftBreak => {
                if self.table.is_some() {
                    if let Some(tb) = self.table.as_mut() {
                        if let Some(c) = tb.row.last_mut() {
                            c.push(' ')
                        }
                    }
                } else {
                    self.push_synth(" ");
                    if self.heading_level.is_some() {
                        self.toc_text.push(' ');
                    }
                }
            }
            Event::HardBreak => self.line_break(),
            Event::Rule => {
                self.flush_rich();
                self.push_block(Block::Rule, Some(range.start as u32));
            }
            Event::TaskListMarker(done) => {
                self.push_synth(if done { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(t) => {
                let s = format!("[^{t}]");
                self.push_owned(s, Style::DIM);
            }
            _ => {}
        }
    }

    fn image_alt_pending(&mut self) -> Option<&mut String> {
        if self.image_url.is_some() {
            Some(&mut self.image_alt)
        } else {
            None
        }
    }

    fn start_tag(&mut self, tag: Tag, at: u32) {
        match tag {
            Tag::Paragraph => {
                if self.rich.is_some() {
                    // Paragraph inside an open item/quote: keep accumulating.
                    self.para_suppressed = true;
                } else {
                    self.begin_rich(self.default_kind());
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_rich();
                let lvl = heading_level(level);
                self.heading_level = Some(lvl);
                self.toc_text.clear();
                self.begin_rich(RichKind::Heading(lvl));
            }
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::List(start) => {
                self.list_depth += 1;
                self.ordered.push(start);
            }
            Tag::Item => {
                self.flush_rich();
                self.inside_item += 1;
                let marker = match self.ordered.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "\u{2022} ".to_string(),
                };
                self.begin_rich(RichKind::Item {
                    depth: self.list_depth,
                    marker,
                });
            }
            Tag::CodeBlock(kind) => {
                self.flush_rich();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().map(String::from),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some(CodeAccum {
                    lang,
                    lines: Vec::new(),
                });
            }
            Tag::Table(aligns) => {
                self.flush_rich();
                self.table = Some(TableAccum {
                    aligns: aligns.iter().map(|a| table_align(*a)).collect(),
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::TableHead => {}
            Tag::TableRow => {
                if let Some(tb) = self.table.as_mut() {
                    tb.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(tb) = self.table.as_mut() {
                    tb.row.push(String::new());
                }
            }
            Tag::Emphasis => self.style = self.style.with(Style::ITALIC),
            Tag::Strong => self.style = self.style.with(Style::BOLD),
            Tag::Strikethrough => self.style = self.style.with(Style::STRIKE),
            Tag::Link { .. } => self.style = self.style.with(Style::LINK),
            Tag::Image { dest_url, .. } => {
                self.image_url = Some(dest_url.to_string());
                self.image_alt.clear();
                // Candidate for a standalone block image: nothing rendered
                // yet in the current paragraph.
                self.image_candidate = self
                    .rich
                    .as_ref()
                    .is_none_or(|r| r.lines.is_empty() && r.cur.is_empty());
            }
            _ => {}
        }
        let _ = at;
    }

    fn end_tag(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => {
                if self.para_suppressed {
                    self.para_suppressed = false;
                } else {
                    self.flush_rich();
                }
            }
            TagEnd::Heading(_) => {
                self.flush_rich();
                self.heading_level = None;
            }
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.ordered.pop();
            }
            TagEnd::Item => {
                self.inside_item = self.inside_item.saturating_sub(1);
                self.flush_rich();
            }
            TagEnd::CodeBlock => {
                if let Some(acc) = self.code.take() {
                    let lang = acc.lang;
                    let lines = acc.lines;
                    self.push_block(Block::Code { lang, lines }, None);
                }
            }
            TagEnd::Table => {
                if let Some(mut tb) = self.table.take() {
                    let mut rows = tb.rows;
                    if !tb.row.is_empty() {
                        rows.push(std::mem::take(&mut tb.row));
                    }
                    self.push_block(
                        Block::Table {
                            rows,
                            aligns: tb.aligns,
                        },
                        None,
                    );
                }
            }
            TagEnd::TableRow => {
                if let Some(tb) = self.table.as_mut() {
                    if !tb.row.is_empty() {
                        tb.rows.push(std::mem::take(&mut tb.row));
                    }
                }
            }
            TagEnd::TableCell => {}
            TagEnd::TableHead => {
                // The header row is complete; move it into `rows`.
                if let Some(tb) = self.table.as_mut() {
                    if !tb.row.is_empty() {
                        tb.rows.push(std::mem::take(&mut tb.row));
                    }
                }
            }
            TagEnd::Emphasis => self.style = self.style.without(Style::ITALIC),
            TagEnd::Strong => self.style = self.style.without(Style::BOLD),
            TagEnd::Strikethrough => self.style = self.style.without(Style::STRIKE),
            TagEnd::Link => self.style = self.style.without(Style::LINK),
            TagEnd::Image => {
                if let Some(url) = self.image_url.take() {
                    if self.image_candidate {
                        if let Some(r) = self.rich.as_mut() {
                            r.image = Some((url, std::mem::take(&mut self.image_alt)));
                        }
                    }
                }
                self.image_candidate = false;
                self.image_alt.clear();
            }
            _ => {}
        }
    }

    fn default_kind(&self) -> RichKind {
        if self.inside_item > 0 {
            RichKind::Item {
                depth: self.list_depth,
                marker: String::new(),
            }
        } else if self.quote_depth > 0 {
            RichKind::Quote(self.quote_depth)
        } else {
            RichKind::Paragraph
        }
    }

    fn begin_rich(&mut self, kind: RichKind) {
        self.rich = Some(Rich {
            kind,
            lines: Vec::new(),
            cur: Vec::new(),
            dst: 0,
            image: None,
        });
    }

    fn push_piece(
        &mut self,
        r: (u32, u32),
        cow: &pulldown_cmark::CowStr,
        text: &str,
        extra: Style,
    ) {
        self.ensure_rich();
        let style = self.style.with(extra);
        // Use the source slice when the event text matches it byte-for-byte
        // (the common, zero-copy case); otherwise fall back to an owned copy.
        let piece_text = match text.get(r.0 as usize..r.1 as usize) {
            Some(slice) if slice == cow.as_ref() => PieceText::Source {
                start: r.0,
                end: r.1,
            },
            _ => PieceText::Owned(Box::from(cow.as_ref())),
        };
        self.append_piece(piece_text, style);
    }

    fn push_owned(&mut self, s: String, style: Style) {
        self.ensure_rich();
        self.append_piece(PieceText::Owned(Box::from(s)), self.style.with(style));
    }

    fn push_synth(&mut self, s: &'static str) {
        self.ensure_rich();
        self.append_piece(PieceText::Synth(s), self.style);
    }

    fn append_piece(&mut self, pt: PieceText, style: Style) {
        // Text appended while not inside an image kills block-image
        // candidacy ("![img](a) more text" is a paragraph, not an image).
        if self.image_candidate && self.image_url.is_none() {
            self.image_candidate = false;
        }
        let rich = self.rich.as_mut().expect("rich block");
        if let PieceText::Source { start, .. } = &pt {
            self.min_byte = Some(self.min_byte.map_or(*start, |m| m.min(*start)));
        }
        let dst = rich.dst;
        rich.dst += pt.len();
        rich.cur.push(Piece {
            dst,
            text: pt,
            style,
        });
    }

    fn line_break(&mut self) {
        if self.image_candidate && self.image_url.is_none() {
            self.image_candidate = false;
        }
        self.ensure_rich();
        let rich = self.rich.as_mut().expect("rich block");
        let pieces = std::mem::take(&mut rich.cur);
        rich.dst = 0;
        rich.lines.push(Line { pieces });
    }

    fn ensure_rich(&mut self) {
        if self.rich.is_none() {
            self.begin_rich(self.default_kind());
        }
    }

    /// Append a code/HTML chunk, splitting on newlines. Line pieces address
    /// the source directly when the event text is a verbatim slice.
    fn code_chunk(&mut self, r: (u32, u32), cow: &pulldown_cmark::CowStr, text: &str) {
        let Some(acc) = self.code.as_mut() else {
            return;
        };
        code_chunk_into(acc, r, cow, text);
    }

    fn flush_rich(&mut self) {
        let Some(mut rich) = self.rich.take() else {
            return;
        };
        self.para_suppressed = false;
        // Close the trailing line.
        if !rich.cur.is_empty() || rich.lines.is_empty() {
            let pieces = std::mem::take(&mut rich.cur);
            rich.lines.push(Line { pieces });
        }
        let has_text = rich.lines.iter().any(|l| l.text_len() > 0);
        if !has_text && rich.image.is_none() {
            return;
        }
        let block = match rich.image.take() {
            Some((url, alt)) => Block::Image { url, alt },
            None => match rich.kind {
                RichKind::Paragraph => Block::Paragraph { lines: rich.lines },
                RichKind::Heading(level) => {
                    let b = Block::Heading {
                        level,
                        lines: rich.lines,
                    };
                    let idx = self.blocks.len() as u32;
                    self.toc.push(TocEntry {
                        level,
                        block: idx,
                        text: self.toc_text.trim().to_string(),
                    });
                    b
                }
                RichKind::Quote(depth) => Block::Quote {
                    depth,
                    lines: rich.lines,
                },
                RichKind::Item { depth, marker } => Block::ListItem {
                    depth,
                    marker,
                    lines: rich.lines,
                },
            },
        };
        self.push_block(block, None);
    }

    fn push_block(&mut self, block: Block, hint: Option<u32>) {
        let start = self
            .min_byte
            .take()
            .or(hint)
            .or_else(|| self.starts.last().copied())
            .unwrap_or(0);
        self.starts.push(start);
        self.blocks.push(block);
    }

    fn finish(mut self) -> Parsed {
        self.flush_rich();
        if let Some(acc) = self.code.take() {
            let lang = acc.lang;
            let lines = acc.lines;
            self.push_block(Block::Code { lang, lines }, None);
        }
        Parsed {
            blocks: self.blocks,
            toc: self.toc,
            block_starts: self.starts,
        }
    }
}

/// Append a code/HTML chunk to an accumulator, splitting on newlines. Line
/// pieces address the source directly when the event text is a verbatim
/// slice of it (the common case); otherwise the chunk is copied.
fn code_chunk_into(acc: &mut CodeAccum, r: (u32, u32), cow: &pulldown_cmark::CowStr, text: &str) {
    let verbatim = text.get(r.0 as usize..r.1 as usize) == Some(cow.as_ref());
    let mut src = r.0;
    let mut rest = cow.as_ref();
    loop {
        match rest.find('\n') {
            Some(nl) => {
                let line_len = nl as u32;
                let pt = if verbatim {
                    PieceText::Source {
                        start: src,
                        end: src + line_len,
                    }
                } else {
                    PieceText::Owned(Box::from(&rest[..nl]))
                };
                src += line_len + 1;
                acc.lines.push(Line {
                    pieces: vec![Piece {
                        dst: 0,
                        text: pt,
                        style: Style::MONO,
                    }],
                });
                rest = &rest[nl + 1..];
            }
            None => {
                if !rest.is_empty() {
                    let pt = if verbatim {
                        PieceText::Source {
                            start: src,
                            end: src + rest.len() as u32,
                        }
                    } else {
                        PieceText::Owned(Box::from(rest))
                    };
                    acc.lines.push(Line {
                        pieces: vec![Piece {
                            dst: 0,
                            text: pt,
                            style: Style::MONO,
                        }],
                    });
                }
                break;
            }
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn table_align(a: pulldown_cmark::Alignment) -> TableAlign {
    match a {
        pulldown_cmark::Alignment::None => TableAlign::None,
        pulldown_cmark::Alignment::Left => TableAlign::Left,
        pulldown_cmark::Alignment::Center => TableAlign::Center,
        pulldown_cmark::Alignment::Right => TableAlign::Right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_blocks(src: &str) -> Vec<Block> {
        parse(src).blocks
    }

    #[test]
    fn headings_and_toc() {
        let p = parse("# Title\n\ntext\n\n## Sub *head*\n");
        assert_eq!(p.blocks.len(), 3);
        assert_eq!(p.toc.len(), 2);
        assert_eq!(p.toc[0].text, "Title");
        assert_eq!(p.toc[1].text, "Sub head");
        assert_eq!(p.toc[0].block, 0);
        assert!(matches!(&p.blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(&p.blocks[2], Block::Heading { level: 2, .. }));
        // Starts record the first source byte of rendered text ("Title" at
        // 2, "text" at 9, "Sub" at 18) - monotonic, which is what the
        // byte->block lookup requires.
        assert_eq!(p.block_starts, vec![2, 9, 18]);
    }

    #[test]
    fn styles_are_bitflags() {
        let blocks = parse_blocks("*i* **b** `c` [l](https://x) ~~s~~");
        let Block::Paragraph { lines } = &blocks[0] else {
            panic!("paragraph")
        };
        let styles: Vec<Style> = lines[0].pieces.iter().map(|p| p.style).collect();
        assert!(styles.iter().any(|s| s.contains(Style::ITALIC)));
        assert!(styles.iter().any(|s| s.contains(Style::BOLD)));
        assert!(styles.iter().any(|s| s.contains(Style::MONO)));
        assert!(styles.iter().any(|s| s.contains(Style::LINK)));
        assert!(styles.iter().any(|s| s.contains(Style::STRIKE)));
    }

    #[test]
    fn zero_copy_source_pieces() {
        let blocks = parse_blocks("hello *world* tail");
        let Block::Paragraph { lines } = &blocks[0] else {
            panic!()
        };
        for p in &lines[0].pieces {
            assert!(
                matches!(p.text, PieceText::Source { .. }),
                "piece should be zero-copy"
            );
        }
    }

    #[test]
    fn code_block_lines() {
        let blocks = parse_blocks("```rust\nfn a() {}\nlet x = 1;\n```\n");
        let Block::Code { lang, lines } = &blocks[0] else {
            panic!()
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(lines.len(), 2);
        assert!(lines
            .iter()
            .all(|l| matches!(l.pieces[0].text, PieceText::Source { .. })));
    }

    #[test]
    fn list_markers_and_tasks() {
        let blocks = parse_blocks("- one\n- [ ] two\n3. three\n");
        // loose-less tight list: three items
        assert_eq!(blocks.len(), 3);
        let Block::ListItem { marker, .. } = &blocks[0] else {
            panic!()
        };
        assert_eq!(marker, "\u{2022} ");
    }

    #[test]
    fn quote_depth() {
        let blocks = parse_blocks("> quoted\n\nplain\n");
        let Block::Quote { depth, .. } = &blocks[0] else {
            panic!()
        };
        assert_eq!(*depth, 1);
        assert!(matches!(&blocks[1], Block::Paragraph { .. }));
    }

    #[test]
    fn tables_own_cells() {
        let blocks = parse_blocks("| a | b |\n|---|:-:|\n| 1 | 2 |\n");
        let Block::Table { rows, aligns } = &blocks[0] else {
            panic!()
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a".to_string(), "b".to_string()]);
        assert_eq!(aligns[1], TableAlign::Center);
    }

    #[test]
    fn block_image_detected() {
        let blocks = parse_blocks("![alt text](pic.png)\n");
        let Block::Image { url, alt } = &blocks[0] else {
            panic!()
        };
        assert_eq!(url, "pic.png");
        assert_eq!(alt, "alt text");
    }

    #[test]
    fn hard_break_splits_lines() {
        let blocks = parse_blocks("first  \nsecond\n");
        let Block::Paragraph { lines } = &blocks[0] else {
            panic!()
        };
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn byte_to_block_lookup() {
        let p = parse("# h\n\npara one\n\n# h2\n\npara two\n");
        // starts: "h"@2, "para one"@5, "h2"@17, "para two"@21
        let mid_para = p.block_at_byte(7).unwrap();
        assert!(matches!(&p.blocks[mid_para], Block::Paragraph { .. }));
        let at_h2 = p.block_at_byte(17).unwrap();
        assert!(matches!(&p.blocks[at_h2], Block::Heading { .. }));
        let at_top = p.block_at_byte(0).unwrap();
        assert!(matches!(&p.blocks[at_top], Block::Heading { .. }));
        let last = p.block_at_byte(900).unwrap();
        assert!(matches!(&p.blocks[last], Block::Paragraph { .. }));
    }
}
