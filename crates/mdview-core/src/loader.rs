// SPDX-License-Identifier: Apache-2.0
//! File loading: memory mapping plus Windows-aware encoding detection.
//!
//! - UTF-8 files (the overwhelmingly common case) are mapped read-only and
//!   never copied; block text is sliced out of the mapping on demand, so the
//!   OS page cache owns the memory and can evict it under pressure.
//! - Files saved by legacy Windows editors (UTF-16LE/BE with BOM, or ANSI /
//!   Windows-1252 without BOM) are transcoded once into an owned string.
//! - Files on network shares skip the mapping: mapped sections over SMB can
//!   behave badly, so those are read into a buffer instead.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// Files below this size are simply read; mapping overhead is not worth it.
const SMALL_FILE: u64 = 64 * 1024;

/// A loaded Markdown document.
///
/// `slice_str` is the hot path: it is called once per visible block per
/// layout-cache miss, on small slices only. Whole-document `text()` is used
/// once per load/reload (parse + search), where a full validation pass is
/// acceptable.
pub struct Document {
    storage: Storage,
    /// Bytes skipped at the start of the original file (a UTF-8 BOM).
    offset: usize,
    /// Encoding that was detected and used, for display and bench output.
    pub encoding: &'static str,
}

enum Storage {
    /// Validated UTF-8 straight over a read-only mapping. The `Arc` keeps the
    /// mapping alive while buffers built from its slices are cached.
    Mapped(Arc<memmap2::Mmap>),
    /// Transcoded or buffered bytes (UTF-16/ANSI/remote/small files).
    Owned(Arc<Vec<u8>>),
}

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    /// The file could not be interpreted as UTF-8, UTF-16 or Windows-1252.
    Undecodable,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::Undecodable => f.write_str("file could not be decoded"),
        }
    }
}

impl std::error::Error for LoadError {}

impl Document {
    pub fn load(path: &Path) -> Result<Document, LoadError> {
        let file = File::open(path).map_err(LoadError::Io)?;
        let meta = file.metadata().map_err(LoadError::Io)?;
        let len = meta.len();

        let bytes: Arc<Vec<u8>>; // used for read/buffered paths
        let map: Option<Arc<memmap2::Mmap>>;
        let remote = is_remote_path(path);
        if remote || len < SMALL_FILE {
            let mut buf = Vec::with_capacity(len as usize);
            file.take(len)
                .read_to_end(&mut buf)
                .map_err(LoadError::Io)?;
            bytes = Arc::new(buf);
            map = None;
        } else {
            #[allow(unsafe_code)]
            // SAFETY: mapping a read-only file handle opened above; the
            // mapping is only ever read, and the file is not truncated by
            // anyone while mapped (viewers only read).
            let m = unsafe { memmap2::Mmap::map(&file) }.map_err(LoadError::Io)?;
            map = Some(Arc::new(m));
            bytes = Arc::new(Vec::new());
        }

        let raw: &[u8] = match (&map, &bytes) {
            (Some(m), _) => &m[..],
            (None, b) => &b[..],
        };

        // BOM sniffing: UTF-8, UTF-16LE, UTF-16BE.
        let (skip, encoding, decoded): (usize, &'static str, Option<String>) =
            if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
                (3, "UTF-8", None)
            } else if len >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
                (2, "UTF-16LE", Some(decode_utf16(&raw[2..], false)))
            } else if len >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
                (2, "UTF-16BE", Some(decode_utf16(&raw[2..], true)))
            } else {
                // No BOM: validate UTF-8; fall back to Windows-1252.
                match std::str::from_utf8(raw) {
                    Ok(_) => (0, "UTF-8", None),
                    Err(_) => (0, "Windows-1252", Some(decode_1252(raw))),
                }
            };

        // Decoded text has the BOM removed by the decoder itself; only the
        // raw-bytes paths carry a BOM `skip`.
        let (storage, offset) = match decoded {
            Some(text) => (Storage::Owned(Arc::new(text.into_bytes())), 0),
            None => (
                match map {
                    Some(m) => {
                        debug_assert!(std::str::from_utf8(&m[skip..]).is_ok());
                        Storage::Mapped(m)
                    }
                    None => Storage::Owned(bytes),
                },
                skip,
            ),
        };

        Ok(Document {
            storage,
            offset,
            encoding,
        })
    }

    /// Total decodable bytes (BOM already excluded).
    pub fn byte_len(&self) -> usize {
        self.bytes().len()
    }

    /// Raw bytes of the decodable text (BOM already skipped).
    pub fn bytes(&self) -> &[u8] {
        &self.storage.bytes()[self.offset..]
    }

    /// Whole-document text, validated. Called once per load/reload.
    pub fn text(&self) -> Result<&str, LoadError> {
        std::str::from_utf8(self.bytes()).map_err(|_| LoadError::Undecodable)
    }

    /// Text for a byte range known to sit on char boundaries (any range
    /// produced by the parser). Defensive lossy fallback keeps rendering
    /// alive instead of panicking on malformed input.
    pub fn slice_str(&self, range: std::ops::Range<usize>) -> &str {
        let bytes = self.bytes();
        let clamped = range.start.min(bytes.len())..range.end.min(bytes.len());
        match std::str::from_utf8(&bytes[clamped.clone()]) {
            Ok(s) => s,
            Err(_) => String::from_utf8_lossy(&bytes[clamped]).into_owned().leak(),
        }
    }
}

impl Storage {
    fn bytes(&self) -> &[u8] {
        match self {
            Storage::Mapped(m) => &m[..],
            Storage::Owned(v) => &v[..],
        }
    }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let unit = |i: usize| -> u16 {
        if big_endian {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        }
    };
    let units: Vec<u16> = (0..bytes.len() / 2).map(|i| unit(i * 2)).collect();
    String::from_utf16_lossy(&units)
}

fn decode_1252(bytes: &[u8]) -> String {
    let (text, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    text.into_owned()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn is_remote_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    // UNC paths (`\\server\share\...`) are always remote.
    if path.as_os_str().to_string_lossy().starts_with("\\\\") {
        return true;
    }
    let Some(root) = path.ancestors().last() else {
        return false;
    };
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is a valid NUL-terminated string; GetDriveTypeW only
    // reads it. DRIVE_REMOTE = 4.
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) == 4 }
}

#[cfg(not(windows))]
fn is_remote_path(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("mdview-test-{name}"));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn utf8_no_bom() {
        let p = tmp("utf8.md", b"# hello *world*");
        let d = Document::load(&p).unwrap();
        assert_eq!(d.encoding, "UTF-8");
        assert_eq!(d.text().unwrap(), "# hello *world*");
    }

    #[test]
    fn utf8_bom_skipped() {
        let p = tmp("bom.md", &[0xEF, 0xBB, 0xBF, b'#', b' ', b'h', b'i']);
        let d = Document::load(&p).unwrap();
        assert_eq!(d.encoding, "UTF-8");
        assert_eq!(d.text().unwrap(), "# hi");
    }

    #[test]
    fn utf16le_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "# café".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let p = tmp("u16.md", &bytes);
        let d = Document::load(&p).unwrap();
        assert_eq!(d.encoding, "UTF-16LE");
        assert_eq!(d.text().unwrap(), "# café");
    }

    #[test]
    fn ansi_fallback() {
        // 0xE9 is 'é' in Windows-1252 but invalid UTF-8.
        let p = tmp("ansi.md", b"# caf\xE9");
        let d = Document::load(&p).unwrap();
        assert_eq!(d.encoding, "Windows-1252");
        assert_eq!(d.text().unwrap(), "# café");
    }

    #[test]
    fn slice_str_roundtrip() {
        let p = tmp("slice.md", b"alpha beta gamma");
        let d = Document::load(&p).unwrap();
        assert_eq!(d.slice_str(6..10), "beta");
    }
}
