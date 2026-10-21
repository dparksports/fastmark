// SPDX-License-Identifier: Apache-2.0
//! fastmark - a fast, memory-efficient Markdown viewer.
//!
//! Native pipeline: mmap -> streaming pull parse -> flat block model ->
//! virtualized layout (cosmic-text) -> wgpu rendering. See docs/ for the
//! architecture diagrams.

// GUI subsystem: launching fastmark never spawns a console window. Debug
// builds keep the console for `cargo run` logs. CLI modes re-attach the
// parent console in `main` so --help/--version/--bench still print when
// invoked from a terminal (and inherit piped stdio under shells like Git
// Bash regardless).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod browse;
mod images;
mod layout;
mod render;
mod theme;

use std::path::PathBuf;
use std::time::Instant;

use app::App;
use theme::ViewTheme;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "\
fastmark - a fast, memory-efficient Markdown viewer

USAGE:
    fastmark [OPTIONS] [FILE]

ARGS:
    <FILE>    Markdown file to open (you can also drop a file onto the window)

OPTIONS:
    --light        force light theme
    --dark         force dark theme (default: follow system)
    --bench FILE   headless benchmark: load, parse, and lay out every block
    --bench-gen KB write a synthetic KB-sized markdown file to stdout
    -h, --help     print help
    -V, --version  print version

KEYS:
    wheel / PgUp / PgDn / arrows / Space / Home / End   scroll
    Ctrl+wheel, Ctrl+=, Ctrl+-, Ctrl+0                  zoom
    Ctrl+F find (type to search, Enter/F3 next, Esc close)
    F9 outline sidebar    F8 theme toggle    Esc quit
";

/// With the `windows` subsystem the process starts without stdio. When a
/// CLI mode is used from a real console (cmd/PowerShell/Windows Terminal),
/// re-attach to the parent's console so println output lands there. Under
/// piped stdio (Git Bash, CI) the inherited handles already work and this
/// call simply fails harmlessly.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console_if_cli() {
    let cli_mode = std::env::args().any(|a| {
        matches!(
            a.as_str(),
            "--help" | "-h" | "--version" | "-V" | "--bench" | "--bench-gen"
        )
    });
    if cli_mode {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        // SAFETY: AttachConsole with the parent PID is the documented use;
        // failure (no parent console) is expected and ignorable.
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(any(not(windows), debug_assertions))]
fn attach_parent_console_if_cli() {}

fn main() {
    attach_parent_console_if_cli();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut file: Option<PathBuf> = None;
    let mut view = ViewTheme::System;
    let mut bench: Option<PathBuf> = None;
    let mut bench_gen: Option<u64> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("fastmark {VERSION}");
                return;
            }
            "--light" => view = ViewTheme::Light,
            "--dark" => view = ViewTheme::Dark,
            "--bench" => {
                bench = it.next().map(PathBuf::from);
            }
            "--bench-gen" => {
                bench_gen = it.next().and_then(|v| v.parse().ok());
            }
            other if !other.starts_with('-') && file.is_none() => {
                file = Some(PathBuf::from(other));
            }
            _ => {}
        }
    }

    if let Some(kb) = bench_gen {
        print_synthetic(kb);
        return;
    }

    if let Some(path) = bench {
        run_bench(&path);
        return;
    }

    let event_loop = winit::event_loop::EventLoop::new().unwrap();
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
    let mut app = App::new(file, view);
    event_loop.run_app(&mut app).unwrap();
}

fn run_bench(path: &std::path::Path) {
    let t0 = Instant::now();
    let doc = match fastmark_core::Document::load(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fastmark: cannot load {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let t_load = t0.elapsed();

    let t1 = Instant::now();
    let text = match doc.text() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fastmark: {e}");
            std::process::exit(1);
        }
    };
    let bytes = text.len();
    let parsed = fastmark_core::parse(text);
    let t_parse = t1.elapsed();

    // Full layout, headless: shape every block the way the viewport would.
    let t2 = Instant::now();
    let params = layout::LayoutParams {
        width_px: 1000.0,
        font_px: 15.5,
    };
    let mut font_system = glyphon::FontSystem::new();
    let t_fonts = t2.elapsed();

    // Viewport layout: what the viewer actually does on the first frame -
    // shape one screenful of blocks, not the document.
    let t3 = Instant::now();
    let viewport_blocks = parsed.blocks.len().min(100);
    for idx in 0..viewport_blocks {
        let _ = layout::build_block_buffer(
            &mut font_system,
            &doc,
            &parsed,
            idx,
            params,
            &theme::Theme::dark(),
        );
    }
    let t_viewport = t3.elapsed();
    let rss_viewport = theme::platform::current_rss().unwrap_or(0);

    // Stress phase: shape *every* block (the viewer never does this; it
    // exists to expose worst-case throughput and allocator behavior).
    let t4 = Instant::now();
    let mut total_lines = 0usize;
    for idx in 0..parsed.blocks.len() {
        let buf = layout::build_block_buffer(
            &mut font_system,
            &doc,
            &parsed,
            idx,
            params,
            &theme::Theme::dark(),
        );
        total_lines += buf.layout_runs().count();
    }
    let t_layout = t4.elapsed();

    let peak = theme::platform::peak_rss().unwrap_or(0);
    println!("fastmark bench: {}", path.display());
    println!("  encoding       : {}", doc.encoding);
    println!(
        "  document       : {:.1} MB ({} bytes)",
        bytes as f64 / 1e6,
        bytes
    );
    println!("  blocks         : {}", parsed.blocks.len());
    println!("  laid-out lines : {total_lines}");
    println!("  load (mmap)    : {:.1} ms", t_load.as_secs_f64() * 1e3);
    println!("  parse          : {:.1} ms", t_parse.as_secs_f64() * 1e3);
    println!("  font scan      : {:.1} ms", t_fonts.as_secs_f64() * 1e3);
    println!(
        "  first screen   : {:.1} ms ({} blocks), RSS {:.1} MB",
        t_viewport.as_secs_f64() * 1e3,
        viewport_blocks,
        rss_viewport as f64 / 1e6
    );
    println!(
        "  full layout    : {:.1} ms ({} blocks) [stress test; the viewer never lays out the whole document]",
        t_layout.as_secs_f64() * 1e3,
        parsed.blocks.len()
    );
    println!("  peak RSS       : {:.1} MB", peak as f64 / 1e6);
}

fn print_synthetic(kb: u64) {
    let para = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen \
liquor jugs. How vexingly quick daft zebras jump! Sphinx of black quartz, judge my vow. \
Sed ut perspiciatis unde omnis iste natus error sit voluptatem accusantium doloremque.";
    let mut out = String::new();
    out.push_str("# Synthetic Markdown Bench\n\n");
    let mut section = 0u64;
    while out.len() < kb as usize * 1024 {
        section += 1;
        out.push_str(&format!("## Section {section}\n\n"));
        out.push_str(para);
        out.push_str(
            "\n\n- first item with `inline code`\n- second item\n  - nested item\n- [x] done\n\n",
        );
        out.push_str(&format!("> A quoted line for section {section}.\n\n"));
        out.push_str("```rust\nfn main() {\n    println!(\"hello, fastmark\");\n}\n```\n\n");
        out.push_str("| a | b | c |\n|---|:-:|--:|\n| 1 | 2 | 3 |\n| x | y | z |\n\n");
        out.push_str(para);
        out.push_str("\n\n");
    }
    print!("{out}");
}
