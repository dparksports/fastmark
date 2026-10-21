// SPDX-License-Identifier: Apache-2.0
//! fastmark-core: the platform-independent document pipeline.
//!
//! Design goals, in priority order:
//! 1. Memory proportional to what the user is looking at, not to the file.
//! 2. Zero-copy: rendered text is sliced straight out of a memory map.
//! 3. Fast: streaming pull parse, `memchr`-based search, O(1) reload.
//!
//! The only owned text is what decoding legacy encodings (UTF-16, ANSI)
//! forces us to allocate; the UTF-8 fast path never copies the file.

// Deny (rather than forbid) so the two audited FFI/mmap calls in `loader`
// can carry explicit `allow` attributes with safety rationales.
#![deny(unsafe_code)]

pub mod blocks;
pub mod index;
pub mod loader;
pub mod search;

pub use blocks::{parse, Block, Line, Parsed, Piece, PieceText, Style, TableAlign, TocEntry};
pub use index::{HeightEstimator, PositionIndex};
pub use loader::Document;
