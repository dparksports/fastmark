// SPDX-License-Identifier: Apache-2.0
//! Color themes. Colors are stored as 8-bit sRGB triplets and converted for
//! both glyphon text and wgpu clears.

/// Command-line theme choice.
#[derive(Clone, Copy)]
pub enum ViewTheme {
    Dark,
    Light,
    System,
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub bg: [u8; 3],
    pub sidebar_bg: [u8; 3],
    pub overlay_bg: [u8; 3],
    pub fg: [u8; 3],
    pub dim: [u8; 3],
    pub faint: [u8; 3],
    pub accent: [u8; 3],
    pub code_bg: [u8; 3],
    pub quote_bar: [u8; 3],
    pub rule: [u8; 3],
    pub thumb: [u8; 3],
}

impl Theme {
    pub fn dark() -> Theme {
        Theme {
            dark: true,
            // Layered-navy design system: depth from background lightness
            // steps, never borders.
            bg: [0x0d, 0x11, 0x17],
            sidebar_bg: [0x0f, 0x14, 0x20],
            overlay_bg: [0x1e, 0x22, 0x2b],
            fg: [0xe8, 0xea, 0xed],
            dim: [0xa6, 0xb0, 0xc0],
            faint: [0x7c, 0x87, 0x96],
            accent: [0x3b, 0x82, 0xf6],
            code_bg: [0x14, 0x18, 0x21],
            quote_bar: [0x2a, 0x30, 0x3d],
            rule: [0x24, 0x2a, 0x35],
            thumb: [0x33, 0x3a, 0x47],
        }
    }

    pub fn light() -> Theme {
        Theme {
            dark: false,
            bg: [0xf7, 0xf8, 0xfa],
            sidebar_bg: [0xf0, 0xf2, 0xf5],
            overlay_bg: [0xff, 0xff, 0xff],
            fg: [0x1a, 0x1d, 0x24],
            dim: [0x4a, 0x53, 0x61],
            faint: [0x6e, 0x78, 0x87],
            accent: [0x25, 0x63, 0xeb],
            code_bg: [0xec, 0xef, 0xf4],
            quote_bar: [0xc3, 0xc9, 0xd4],
            rule: [0xdf, 0xe3, 0xe9],
            thumb: [0xc5, 0xca, 0xd3],
        }
    }

    /// Follow the Windows personalization setting (dark by default
    /// elsewhere).
    pub fn from_system() -> Theme {
        if platform::system_prefers_light() {
            Theme::light()
        } else {
            Theme::dark()
        }
    }
}

impl Theme {
    pub fn glyphon(&self, c: [u8; 3]) -> glyphon::Color {
        glyphon::Color::rgb(c[0], c[1], c[2])
    }
    /// Linear blend of two sRGB triplets (0=a, 1=b).
    pub fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
        let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
    }

    /// Row-hover tint: a subtle lift from the panel tone.
    pub fn hover(&self) -> [u8; 3] {
        self.overlay_bg
    }

    /// Selected-row tint: accent washed into the background.
    pub fn sel_tint(&self) -> [u8; 3] {
        Theme::mix(self.bg, self.accent, 0.22)
    }

    /// Pill background: slightly lifted panel.
    pub fn pill_bg(&self) -> [u8; 3] {
        Theme::mix(self.overlay_bg, self.fg, 0.06)
    }

    pub fn clear(&self) -> wgpu::Color {
        let c = self.bg;
        wgpu::Color {
            r: c[0] as f64 / 255.0,
            g: c[1] as f64 / 255.0,
            b: c[2] as f64 / 255.0,
            a: 1.0,
        }
    }
}

pub mod platform {
    //! Small platform integration shims.

    #[cfg(windows)]
    pub mod imp {
        pub fn system_prefers_light() -> bool {
            use winreg::enums::HKEY_CURRENT_USER;
            use winreg::RegKey;
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
                .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
                .map(|v| v != 0)
                .unwrap_or(false)
        }

        #[allow(unsafe_code)]
        pub fn apply_titlebar_theme(window: &winit::window::Window, dark: bool) {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return;
            };
            let hwnd = win32.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            let value: i32 = if dark { 1 } else { 0 };
            // SAFETY: valid HWND from winit; `value` outlives the call.
            unsafe {
                windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
                    hwnd,
                    windows_sys::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                    &value as *const i32 as *const core::ffi::c_void,
                    core::mem::size_of::<i32>() as u32,
                );
            }
        }

        /// Native folder picker (SHBrowseForFolder, modern dialog style),
        /// parented to the app window. Returns None if cancelled.
        #[allow(unsafe_code)]
        pub fn pick_folder(parent: &winit::window::Window) -> Option<std::path::PathBuf> {
            use std::ffi::c_void;
            use windows_sys::Win32::Foundation::{BOOL, HWND};
            use windows_sys::Win32::System::Com::CoTaskMemFree;
            use windows_sys::Win32::UI::Shell::{
                SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
                BROWSEINFOW,
            };
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

            let Ok(handle) = parent.window_handle() else {
                return None;
            };
            let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return None;
            };
            let hwnd = win32.hwnd.get() as HWND;

            let title: Vec<u16> = "Choose a folder of Markdown files\0"
                .encode_utf16()
                .collect();
            let mut display = [0u16; 260];
            let bi = BROWSEINFOW {
                hwndOwner: hwnd,
                pidlRoot: std::ptr::null_mut(),
                pszDisplayName: display.as_mut_ptr(),
                lpszTitle: title.as_ptr(),
                ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
                lpfn: None,
                lParam: 0,
                iImage: 0,
            };
            // SAFETY: bi and its buffers live for the duration of the call;
            // the returned PIDL is freed via CoTaskMemFree on all paths.
            unsafe {
                let pidl = SHBrowseForFolderW(&bi);
                if pidl.is_null() {
                    return None;
                }
                let mut path = [0u16; 260];
                let ok: BOOL = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
                CoTaskMemFree(pidl as *mut c_void);
                if ok == 0 {
                    return None;
                }
                let len = path.iter().position(|&c| c == 0).unwrap_or(0);
                use std::os::windows::ffi::OsStringExt;
                Some(std::ffi::OsString::from_wide(&path[..len]).into())
            }
        }

        /// Peak working set in bytes, for the bench mode.
        #[allow(unsafe_code)]
        pub fn peak_rss() -> Option<u64> {
            pmc().map(|p| p.PeakWorkingSetSize as u64)
        }

        /// Current working set in bytes, for the bench mode.
        #[allow(unsafe_code)]
        pub fn current_rss() -> Option<u64> {
            pmc().map(|p| p.WorkingSetSize as u64)
        }

        #[allow(unsafe_code)]
        fn pmc() -> Option<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS> {
            use windows_sys::Win32::System::ProcessStatus::{
                GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
            };
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            // SAFETY: PROCESS_MEMORY_COUNTERS is a plain C struct; cb is set
            // before the call as required.
            let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { core::mem::zeroed() };
            counters.cb = core::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let ok =
                unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
            (ok != 0).then_some(counters)
        }
    }

    #[cfg(not(windows))]
    pub mod imp {
        pub fn system_prefers_light() -> bool {
            false
        }
        pub fn apply_titlebar_theme(_window: &winit::window::Window, _dark: bool) {}
        pub fn pick_folder(_parent: &winit::window::Window) -> Option<std::path::PathBuf> {
            None
        }
        pub fn peak_rss() -> Option<u64> {
            rss_field("VmHWM:")
        }
        pub fn current_rss() -> Option<u64> {
            rss_field("VmRSS:")
        }
        fn rss_field(field: &str) -> Option<u64> {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with(field))
                        .and_then(|l| {
                            l.split_whitespace()
                                .nth(1)
                                .and_then(|kb| kb.parse::<u64>().ok())
                        })
                        .map(|kb| kb * 1024)
                })
        }
    }

    pub use imp::*;
}
