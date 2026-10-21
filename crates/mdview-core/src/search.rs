// SPDX-License-Identifier: Apache-2.0
//! Incremental substring search over the document text.
//!
//! Memory discipline: the query is matched line-by-line (split on `\n` via
//! `memchr`), so the only allocation besides the hit list is one lowercased
//! line at a time - never a second copy of the document. Case folding is
//! ASCII-only, which covers code and prose overwhelmingly.

use memchr::{memchr, memmem};

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    /// Byte offset of the match start in document text.
    pub byte: usize,
    pub len: usize,
}

/// Find up to `limit` occurrences of `needle`.
///
/// Returns the hits plus whether the limit truncated the search.
pub fn find(text: &str, needle: &str, case_insensitive: bool, limit: usize) -> (Vec<Hit>, bool) {
    if needle.is_empty() {
        return (Vec::new(), false);
    }
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut truncated = false;

    // Scratch buffers, reused across lines.
    let mut lower_line: Vec<u8> = Vec::new();
    let lower_needle: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let matcher = memmem::Finder::new(needle.as_bytes());

    let mut line_start = 0usize;
    'lines: while line_start <= bytes.len() {
        let line_end = match memchr(b'\n', &bytes[line_start..]) {
            Some(n) => line_start + n,
            None => bytes.len(),
        };
        let line = &bytes[line_start..line_end];

        // Positions of matches within this line.
        let positions: Vec<usize> = if case_insensitive {
            lower_line.clear();
            lower_line.extend_from_slice(line);
            lower_line.make_ascii_lowercase();
            let mut at = 0usize;
            let mut out = Vec::new();
            while let Some(pos) = memmem::find(&lower_line[at..], &lower_needle) {
                out.push(at + pos);
                at += pos + lower_needle.len();
            }
            out
        } else {
            let mut at = 0usize;
            let mut out = Vec::new();
            while let Some(pos) = matcher.find(&line[at..]) {
                out.push(at + pos);
                at += pos + needle.len();
            }
            out
        };

        for pos in positions {
            if hits.len() >= limit {
                truncated = true;
                break 'lines;
            }
            hits.push(Hit {
                byte: line_start + pos,
                len: needle.len(),
            });
        }

        if line_end == bytes.len() {
            break;
        }
        line_start = line_end + 1;
    }
    (hits, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_sensitive() {
        let (hits, tr) = find("foo bar foo", "foo", false, 100);
        assert!(!tr);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].byte, 0);
        assert_eq!(hits[1].byte, 8);
    }

    #[test]
    fn case_insensitive_ascii() {
        let (hits, _) = find("Foo bar FOO", "foo", true, 100);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn across_lines_and_limit() {
        let text = "a\nb\na\na\n";
        let (hits, tr) = find(text, "a", false, 2);
        assert!(tr);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].byte, 4);
    }

    #[test]
    fn empty_needle_no_hits() {
        let (hits, _) = find("anything", "", true, 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn no_needle_inside_line_overlap_guard() {
        // "aa" with needle "aa" must find one match, not loop.
        let (hits, _) = find("aa", "aa", false, 10);
        assert_eq!(hits.len(), 1);
    }
}
