//! Thin Win32 layer for the overlay: global hotkeys, focus-free clicks, and hit-region clipping.
//! Other platforms get no-op fallbacks so the crate still builds for development.
#![cfg_attr(not(windows), allow(dead_code))]

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hotkey {
    Toggle,
    Calibrate,
}

/// `RegisterHotKey` modifier and virtual-key codes.
const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_SHIFT: u32 = 0x4;
const MOD_WIN: u32 = 0x8;

/// Parses "F8", "Shift+F8", "Ctrl+Alt+K", "Insert" into `(modifiers, virtual key)`.
pub fn parse_hotkey(spec: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut key = None;
    for part in spec.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "alt" => mods |= MOD_ALT,
            "win" => mods |= MOD_WIN,
            k if key.is_none() => key = Some(virtual_key(k)?),
            _ => return None, // two non-modifier keys
        }
    }
    Some((mods, key?))
}

fn virtual_key(name: &str) -> Option<u32> {
    let named = match name {
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "pause" => 0x13,
        "scrolllock" => 0x91,
        "tab" => 0x09,
        "`" | "backquote" => 0xC0,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then_some(0x70 + n - 1);
    }
    if let Some(n) = name
        .strip_prefix("numpad")
        .and_then(|n| n.parse::<u32>().ok())
    {
        return (n <= 9).then_some(0x60 + n);
    }
    match name.as_bytes() {
        [c @ (b'a'..=b'z' | b'0'..=b'9')] => Some(c.to_ascii_uppercase() as u32),
        _ => None,
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;

    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        CombineRgn, CreateRectRgn, DeleteObject, RGN_OR, SetWindowRgn,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GWL_EXSTYLE, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, MSG,
        SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
        WM_HOTKEY, WS_EX_NOACTIVATE,
    };

    use super::{Hotkey, parse_hotkey};

    /// Registers both hotkeys on a dedicated thread that sleeps in `GetMessageW` until one fires.
    /// Returns an error naming any key that could not be registered (bad spec or already taken).
    pub fn spawn_hotkeys(
        toggle: &str,
        calibrate: &str,
        on_press: impl Fn(Hotkey) + Send + 'static,
    ) -> Result<(), String> {
        let keys = [
            (Hotkey::Toggle, toggle.to_owned()),
            (Hotkey::Calibrate, calibrate.to_owned()),
        ];
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("vltracker-hotkeys".into())
            .spawn(move || {
                let mut failed = Vec::new();
                for (id, (_, spec)) in keys.iter().enumerate() {
                    let ok = parse_hotkey(spec).is_some_and(|(mods, vk)| unsafe {
                        // Hotkeys bound to a null HWND post WM_HOTKEY to this thread's queue.
                        RegisterHotKey(
                            std::ptr::null_mut(),
                            id as i32,
                            mods as HOT_KEY_MODIFIERS | MOD_NOREPEAT,
                            vk,
                        ) != 0
                    });
                    if !ok {
                        failed.push(spec.clone());
                    }
                }
                let _ = ready_tx.send(failed);
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                    if msg.message == WM_HOTKEY
                        && let Some((hotkey, _)) = keys.get(msg.wParam)
                    {
                        on_press(*hotkey);
                    }
                }
            })
            .map_err(|e| format!("hotkey thread: {e}"))?;
        match ready_rx.recv() {
            Ok(failed) if failed.is_empty() => Ok(()),
            Ok(failed) => Err(format!(
                "could not register hotkey {} (invalid or used by another app)",
                failed.join(", ")
            )),
            Err(_) => Err("hotkey thread exited".into()),
        }
    }

    /// Primary monitor size in physical pixels.
    pub fn primary_monitor_px() -> Option<[f32; 2]> {
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        (w > 0 && h > 0).then_some([w as f32, h as f32])
    }

    /// Top-level window of this process with the given title, as a plain integer handle.
    pub fn find_window(title: &str) -> Option<isize> {
        let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
        (!hwnd.is_null()).then_some(hwnd as isize)
    }

    /// Clicks reach the window without activating it, so the game keeps keyboard and mouse focus.
    pub fn set_no_activate(hwnd: isize) {
        let hwnd = hwnd as HWND;
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
        }
    }

    /// Put the undecorated overlay on the primary display in native pixels, independently of
    /// the main window's DPI or location on another monitor. Primary origin is always (0, 0).
    pub fn cover_primary_monitor(hwnd: isize) {
        if let Some([w, h]) = primary_monitor_px() {
            unsafe {
                SetWindowPos(
                    hwnd as HWND,
                    std::ptr::null_mut(),
                    0,
                    0,
                    w as i32,
                    h as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
    }

    /// Restricts painting and hit-testing to `rects` (client pixels: x0, y0, x1, y1). Clicks outside
    /// fall through to the game. `None` restores the full window.
    pub fn set_region(hwnd: isize, rects: Option<&[[i32; 4]]>) {
        let hwnd = hwnd as HWND;
        unsafe {
            let Some(rects) = rects else {
                SetWindowRgn(hwnd, std::ptr::null_mut(), 1);
                return;
            };
            let region = CreateRectRgn(0, 0, 0, 0);
            for [x0, y0, x1, y1] in rects {
                let part = CreateRectRgn(*x0, *y0, *x1, *y1);
                CombineRgn(region, region, part, RGN_OR);
                DeleteObject(part);
            }
            // On success the system owns `region`; only free it on failure.
            if SetWindowRgn(hwnd, region, 1) == 0 {
                DeleteObject(region);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Hotkey;

    pub fn spawn_hotkeys(
        _: &str,
        _: &str,
        _: impl Fn(Hotkey) + Send + 'static,
    ) -> Result<(), String> {
        Err("global hotkeys are only supported on Windows".into())
    }
    pub fn primary_monitor_px() -> Option<[f32; 2]> {
        None
    }
    pub fn find_window(_: &str) -> Option<isize> {
        None
    }
    pub fn set_no_activate(_: isize) {}
    pub fn cover_primary_monitor(_: isize) {}
    pub fn set_region(_: isize, _: Option<&[[i32; 4]]>) {}
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_specs() {
        assert_eq!(parse_hotkey("F8"), Some((0, 0x77)));
        assert_eq!(parse_hotkey("shift + f8"), Some((MOD_SHIFT, 0x77)));
        assert_eq!(
            parse_hotkey("Ctrl+Alt+K"),
            Some((MOD_CONTROL | MOD_ALT, b'K' as u32))
        );
        assert_eq!(parse_hotkey("Insert"), Some((0, 0x2D)));
        assert_eq!(parse_hotkey("Numpad5"), Some((0, 0x65)));
        assert_eq!(parse_hotkey("F25"), None);
        assert_eq!(parse_hotkey("Shift"), None);
        assert_eq!(parse_hotkey("A+B"), None);
    }
}
