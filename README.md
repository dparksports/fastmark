# fastmark

**A fast, memory-efficient, native Markdown viewer for Windows (macOS/Linux core ready).**

[![CI](https://github.com/dparksports/fastmark/actions/workflows/ci.yml/badge.svg)](https://github.com/dparksports/fastmark/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-9ca3af)

fastmark renders Markdown the way editors render code: a **streaming pipeline**
that never materializes the document as a rich-text tree. The file stays
memory-mapped, the parser produces a flat list of blocks addressed by *byte
ranges*, and only the blocks near your viewport are ever laid out. The result
opens 100 MB documents instantly, idles around **40–60 MB of RAM**, and keeps
scrolling at display refresh rate no matter where you are in the file.

```text
fastmark README.md        # view a file
fastmark --bench big.md   # headless benchmark: load, parse, shape every block
```

You can also just **drop any `.md` file onto the window**.

## Why it's fast and small

![Architecture](docs/architecture.svg)

- **Zero-copy text.** UTF-8 files are mapped read-only; every rendered glyph is
  sliced straight out of the mapping. The OS page cache owns the memory and
  reclaims it under pressure — a 500 MB file costs almost nothing until you
  scroll to it.
- **Streaming parse.** [pulldown-cmark] emits events; we build a flat block
  list in one pass. No DOM, no AST. Re-parsing a 10 MB file takes tens of
  milliseconds, which is what makes live-reload free.
- **Virtualized layout.** Only blocks near the viewport are shaped
  ([cosmic-text] via [glyphon]); the rest carry cheap height estimates that
  converge to real measurements as you visit them. Layout memory is
  O(viewport), not O(document).
- **GPU rendering.** wgpu (DirectX 12 on Windows) draws an instanced-quad
  chrome pass, textured image quads, and a glyph-atlas text pass. Nothing
  CPU-rasterizes.
- **Disciplined images.** Decoded off-thread, downsampled to ≤2× display size
  before touching GPU memory, and evicted by an LRU with a hard byte budget
  (128 MB). A 4000×3000 photo never becomes 48 MB of resident RGBA.
- **Windows-realistic files.** UTF-8/UTF-16 BOMs, ANSI (Windows-1252) legacy
  files, CRLF, and network shares (SMB paths skip the mmap and read instead).

![Memory](docs/memory.svg)

## Design

Layered-navy theme with one accent, rounded geometry, hover states, and
Segoe Fluent line icons — depth comes from background-lightness steps, never
borders. The left tree has FOLDERS/RECENT sections; the browser home has a
page header with clickable breadcrumbs, a pill status bar, and a circular
folder-picker FAB; the reader has a bottom status bar with the current
heading and action pills.

## Features

- **File browser home** — launch without arguments and fastmark lists every
  Markdown file in the working folder (recursive, background-scanned). Type
  to filter, `↑↓` select, `Enter` or click to open — or drop any `.md` file
  onto the window.
- **Folder-wide keyword search** — `Ctrl+Shift+F` searches every word in
  every file of the folder (mmap'd one at a time, streamed results with
  file:line and snippet). `Enter` jumps straight to the hit.
- **File tree on the left** (`F10`, on by default) — a real expandable
  folder tree of the working directory, derived from the background walker
  with zero extra filesystem scans: folders appear only where Markdown
  lives, click ▸/▾ to expand, click a file to open, wheel to scroll, and
  the current document stays highlighted. Visible both in the browser home
  and while reading.
- **Synced, collapsible outline** — the TOC tracks what you're reading and
  auto-scrolls to keep the active heading visible; click a heading's ▸/▾ to
  fold or unfold any branch.
- Headings, paragraphs, **emphasis**, `inline code`, links, strikethrough
- Fenced/indented code blocks, block quotes, nested & task lists, rules
- GFM tables (rendered as aligned monospace grids)
- Block images (PNG/JPEG/GIF/WebP/BMP) with caption + placeholder frame
- Incremental in-document find — `Ctrl+F`, `Enter`/`F3` next, `Esc` close
- Live reload on file change, keeping your scroll position
- Dark/light theme following the Windows personalization setting — `F8`
- Smooth scrolling, per-monitor DPI awareness, dark title bar, no console
  window (GUI subsystem with console re-attach for CLI modes)
- Zoom: `Ctrl+wheel`, `Ctrl+=`, `Ctrl+-`, `Ctrl+0`

## Keyboard shortcuts

| Key | Action |
|---|---|
| typing / `Backspace` | filter the file list (browser) or edit the search query |
| `↑↓` / wheel | move selection (lists) or scroll (document) |
| `Enter` / click | open selected file / jump to search hit |
| `PgUp`/`PgDn`, `Space`, `Home`/`End` | scroll the document |
| `Ctrl+Shift+F` | search every word across the folder |
| `Ctrl+F` | find within the open document (`Enter`/`F3` next, `Esc` close) |
| `F9` | outline sidebar (synced, collapsible branches) |
| `F10` | file-tree sidebar (expand folders, click to open) |
| `Ctrl+wheel`, `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | zoom |
| `F8` | toggle dark/light theme |
| `Ctrl+O` | choose a folder (native picker) |
| `Esc` | close search / clear filter / quit |

## Virtualized scrolling

![Virtualization](docs/virtualization.svg)

Every block begins as a height *estimate* (characters ÷ column width, minimum
line count). Blocks entering the viewport get truly shaped once, and the
measured height replaces the estimate. A prefix-sum index then answers
"which blocks intersect this scroll offset" with a binary search. The
scrollbar reflects running estimates and settles as you explore.

## Building

```bash
git clone https://github.com/dparksports/fastmark
cd fastmark
cargo build --release
# Windows: target\release\fastmark.exe
```

Requires a stable Rust toolchain (1.82+). On Linux, install the usual
Wayland/X11 development packages (`libxkbcommon-dev libwayland-dev libx11-dev
libxcb1-dev libxrandr-dev libxi-dev libxcursor-dev`).

### Benchmarking

```bash
fastmark --bench-gen 102400 > big.md   # synthesize a 100 MB markdown file
fastmark --bench big.md                 # load / parse / layout timings + RSS
```

The **CI bench job** runs this on every push against a synthetic 25 MB
document — check the latest [CI runs](https://github.com/dparksports/fastmark/actions)
for live numbers.

Measured on CI (windows-latest runner, release build, headless `--bench`
mode, [run 39549333608](https://github.com/dparksports/fastmark/actions/runs/39549333608)):

| Document | Blocks | Load (mmap) | Parse | First screen¹ | Full layout (stress¹) |
|---|---|---|---|---|---|
| samples/demo.md (1.8 KB) | 34 | 0.1 ms | 0.3 ms | 17.2 ms | 3.8 ms · peak RSS 12.5 MB |
| synthetic 25 MB | 354,071 | 9.9 ms | 267.5 ms | 94.6 ms | 60.6 s · peak RSS 255.6 MB |

¹ *First screen* shapes the ~100 blocks a viewport actually shows on open —
including one-time shaping warm-up (the demo's subsequent full layout of the
same 34 blocks takes 3.8 ms). *Stress* shapes every block; the viewer never
does. Steady-state viewer memory is the mapped pages you touched
(OS-reclaimable), block metadata, and the bounded layout/image/glyph caches —
the flat line in the chart above. One-time font scan on a cold runner: ~1.7 s
(~23 ms warm).

## Project layout

```
crates/fastmark-core   platform-independent pipeline: mmap loader, encoding
                     detection, streaming parser, block model, search,
                     position index (fully unit-tested, no GPU deps)
crates/fastmark-app    winit + wgpu + glyphon viewer: virtualized layout,
                     image store, file watching, TOC, find, themes
docs/                architecture infographics (SVG)
samples/             demo + bench documents
```

## Known limitations (v1)

- Deep jumps into never-visited territory land on estimated heights first and
  settle as blocks are measured.
- Tables render as monospace grids; no syntax highlighting in code blocks yet
  (planned: lazy syntect per visible block).
- Links are styled but not clickable in-document; local images only (no
  network fetches, by design).
- Folder search is keyword-based (case-insensitive substring, ASCII fold);
  true semantic search (embedding models) is a roadmap item — the scan
  architecture is ready for a scorer hook.
- Search highlight is block-level (accent bar); glyph-precise highlight is
  planned via cosmic-text cursor APIs.

## License

Apache-2.0 — see [LICENSE](LICENSE).

[pulldown-cmark]: https://crates.io/crates/pulldown-cmark
[cosmic-text]: https://crates.io/crates/cosmic-text
[glyphon]: https://crates.io/crates/glyphon
