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
            bg: [0x18, 0x18, 0x25],
            sidebar_bg: [0x13, 0x13, 0x1e],
            overlay_bg: [0x22, 0x22, 0x33],
            fg: [0xcd, 0xd6, 0xf4],
            dim: [0x93, 0x99, 0xb8],
            faint: [0x5c, 0x61, 0x84],
            accent: [0x89, 0xb4, 0xfa],
            code_bg: [0x11, 0x11, 0x1b],
            quote_bar: [0x45, 0x47, 0x5a],
            rule: [0x3a, 0x3d, 0x55],
            thumb: [0x4a, 0x4d, 0x68],
        }
    }

    pub fn light() -> Theme {
        Theme {
            dark: false,
            bg: [0xfa, 0xfa, 0xfc],
            sidebar_bg: [0xf0, 0xf0, 0xf4],
            overlay_bg: [0xff, 0xff, 0xff],
            fg: [0x22, 0x22, 0x30],
            dim: [0x60, 0x62, 0x74],
            faint: [0x9a, 0x9c, 0xac],
            accent: [0x1a, 0x5f, 0xb4],
            code_bg: [0xe9, 0xe9, 0xf2],
            quote_bar: [0xb9, 0xbb, 0xca],
            rule: [0xd4, 0xd6, 0xe2],
            thumb: [0xc0, 0xc2, 0xd2],
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
