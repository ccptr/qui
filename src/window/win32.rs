//! The Windows backend: Win32 windows, input and clipboard, and Vulkan's
//! `VK_KHR_win32_surface`. See the parent module for what differs from
//! Wayland.
//!
//! The window procedures only record what happened (in an [`Inbox`]); the
//! loop handles it after dispatching, as on X11. Windows' own modal loops
//! (moving or resizing a window, the window menu) don't return to the
//! loop until they end, so while one runs, the procedures run the loop's
//! step themselves ([`pump`]): the content follows a resize live.

use super::{App, Cx, Fetched, Keyboard, Layer, Painter, Spec, Waker, char_keysym, named, place, stat};
use crate::clipboard::{Pasted, dib_to_bmp};
use crate::gfx::{Gfx, Native};
use crate::input::{Button, Cursor, Event, Key, KeyEvent, Mods};
use std::cell::{Cell, RefCell};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, ValidateRect};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId, INFINITE};
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForSystem,
    GetDpiForWindow, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Posted to the helper window by a [`Waker`].
const WM_WAKE: u32 = WM_APP + 1;
/// `WM_MOUSELEAVE` (in another windows-sys module).
const WM_MOUSELEAVE: u32 = 0x02a3;

/// A [`Waker`]'s side: the helper window to post to (0 once the loop
/// ended), and whether a wake-up is posted and not yet seen.
pub(crate) struct Wake {
    hwnd: AtomicUsize,
    pending: AtomicBool,
}

impl Waker {
    pub fn wake(&self) {
        if !self.0.pending.swap(true, Ordering::AcqRel) {
            let hwnd = self.0.hwnd.load(Ordering::Acquire);
            if hwnd != 0 {
                unsafe { PostMessageW(hwnd as HWND, WM_WAKE, 0, 0) };
            }
        }
    }
}

/// Milliseconds on the performance counter (comparable across processes).
pub(crate) fn monotonic_ms() -> f64 {
    let (mut count, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut count);
        QueryPerformanceFrequency(&mut freq);
    }
    count as f64 * 1000.0 / freq.max(1) as f64
}

/// Whether apps should be light (Settings > Personalization > Colors);
/// light where the setting is missing, as Windows is by default.
pub(crate) fn light_theme() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let (mut data, mut size) = (1u32, 4u32);
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    status != 0 || data != 0
}

/// A NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- what the window procedures record --------------------------------------------

struct Inbox {
    /// The content window (null without one): other windows' messages to
    /// its procedure (a destroyed window's last ones) are not recorded.
    main: HWND,
    /// It is a layer window (it is placed by qui, not by the DPI change).
    layer: bool,
    wake: Option<Arc<Wake>>,

    events: Vec<Event>,
    fetched: Vec<Fetched>,
    woken: bool,
    focus_changed: Option<bool>,
    outside_click: bool,
    closed: bool,
    /// Painted, restored or otherwise needing a fresh frame.
    redraw: bool,
    /// The client size changed (physical pixels).
    size: Option<(u32, u32)>,
    /// The window's DPI changed.
    dpi: Option<u32>,

    focused: bool,
    minimized: bool,
    pointer_inside: bool,
    /// Mouse buttons held (the pointer is captured while any is).
    buttons: u8,
    /// The cursor in the client area.
    cursor: HCURSOR,
    /// The smallest outer size (resizable toplevels), or zero.
    min_track: (i32, i32),
    /// Losing the focus is a click outside (an exclusive layer window).
    exclusive: bool,
    /// Inside one of Windows' modal loops: the procedures pump.
    modal: bool,
    /// The last key press, waiting for the text `WM_CHAR` brings: its
    /// index in `events`, and whether its keysym is its character's.
    typing: Option<(usize, bool)>,
    /// The high half of a surrogate pair from `WM_CHAR`.
    surrogate: Option<u16>,
}

/// What a step takes from the inbox.
struct Taken {
    events: Vec<Event>,
    fetched: Vec<Fetched>,
    woken: bool,
    focus_changed: Option<bool>,
    outside_click: bool,
    closed: bool,
    redraw: bool,
    size: Option<(u32, u32)>,
    dpi: Option<u32>,
}

/// What the window procedure does after recording a message, with the
/// inbox released (these calls send messages to the procedure).
enum After {
    Nothing,
    Capture,
    Release,
    /// Move to the rectangle a DPI change suggests.
    Move(RECT),
}

impl Inbox {
    const fn new() -> Inbox {
        Inbox {
            main: null_mut(),
            layer: false,
            wake: None,
            events: Vec::new(),
            fetched: Vec::new(),
            woken: false,
            focus_changed: None,
            outside_click: false,
            closed: false,
            redraw: false,
            size: None,
            dpi: None,
            focused: false,
            minimized: false,
            pointer_inside: false,
            buttons: 0,
            cursor: null_mut(),
            min_track: (0, 0),
            exclusive: false,
            modal: false,
            typing: None,
            surrogate: None,
        }
    }

    fn pending(&self) -> bool {
        !self.events.is_empty()
            || !self.fetched.is_empty()
            || self.woken
            || self.focus_changed.is_some()
            || self.outside_click
            || self.closed
            || self.redraw
            || self.size.is_some()
            || self.dpi.is_some()
    }

    fn take(&mut self) -> Taken {
        self.typing = None;
        Taken {
            events: std::mem::take(&mut self.events),
            fetched: std::mem::take(&mut self.fetched),
            woken: std::mem::take(&mut self.woken),
            focus_changed: self.focus_changed.take(),
            outside_click: std::mem::take(&mut self.outside_click),
            closed: std::mem::take(&mut self.closed),
            redraw: std::mem::take(&mut self.redraw),
            size: self.size.take(),
            dpi: self.dpi.take(),
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.focus_changed = Some(focused);
        self.typing = None;
        self.events.push(Event::Focus(focused));
        self.events.push(Event::Modifiers(if focused { mods() } else { Mods::NONE }));
    }

    /// The window stops getting input (hidden or gone).
    fn lose_input(&mut self) {
        self.exclusive = false;
        self.pointer_inside = false;
        self.buttons = 0;
        self.set_focus(false);
    }

    /// Records a message to the content window: its result (`None`: the
    /// default procedure's) and what to do next.
    fn handle(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> (Option<LRESULT>, After) {
        let mut after = After::Nothing;
        let xy = || ((lp & 0xffff) as i16 as f32, ((lp >> 16) & 0xffff) as i16 as f32);
        let result = match msg {
            WM_CLOSE => {
                self.closed = true;
                Some(0)
            }
            WM_PAINT => {
                unsafe { ValidateRect(hwnd, null()) };
                self.redraw = true;
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_SIZE => {
                if wp as u32 == SIZE_MINIMIZED {
                    self.minimized = true;
                } else {
                    if std::mem::take(&mut self.minimized) {
                        self.redraw = true;
                    }
                    let (w, h) = ((lp & 0xffff) as u32, ((lp >> 16) & 0xffff) as u32);
                    self.size = Some((w.max(1), h.max(1)));
                }
                Some(0)
            }
            WM_DPICHANGED => {
                self.dpi = Some(((wp & 0xffff) as u32).max(1));
                if !self.layer {
                    after = After::Move(unsafe { *(lp as *const RECT) });
                }
                Some(0)
            }
            WM_GETMINMAXINFO => {
                if self.min_track != (0, 0) {
                    let info = unsafe { &mut *(lp as *mut MINMAXINFO) };
                    info.ptMinTrackSize = POINT { x: self.min_track.0, y: self.min_track.1 };
                }
                Some(0)
            }
            WM_SETFOCUS => {
                self.set_focus(true);
                Some(0)
            }
            WM_KILLFOCUS => {
                self.set_focus(false);
                Some(0)
            }
            WM_ACTIVATE => {
                if (wp & 0xffff) as u32 == WA_INACTIVE && self.exclusive {
                    self.outside_click = true;
                }
                // The default procedure gives the focus.
                None
            }
            WM_ENTERSIZEMOVE | WM_ENTERMENULOOP => {
                self.modal = true;
                None
            }
            WM_EXITSIZEMOVE | WM_EXITMENULOOP => {
                self.modal = false;
                None
            }
            WM_MOUSEMOVE => {
                if !std::mem::replace(&mut self.pointer_inside, true) {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe { TrackMouseEvent(&mut track) };
                }
                let (x, y) = xy();
                self.events.push(Event::PointerMoved(x, y));
                Some(0)
            }
            WM_MOUSELEAVE => {
                if std::mem::take(&mut self.pointer_inside) {
                    self.events.push(Event::PointerLeft);
                }
                Some(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP
            | WM_MBUTTONUP | WM_XBUTTONUP => {
                let (button, bit) = match msg {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => (Button::Left, 1),
                    WM_RBUTTONDOWN | WM_RBUTTONUP => (Button::Right, 2),
                    WM_MBUTTONDOWN | WM_MBUTTONUP => (Button::Middle, 4),
                    _ => (Button::Other, 8),
                };
                let pressed = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN);
                let held = self.buttons;
                if pressed {
                    self.buttons |= bit;
                    if held == 0 {
                        after = After::Capture;
                    }
                } else {
                    self.buttons &= !bit;
                    if held != 0 && self.buttons == 0 {
                        after = After::Release;
                    }
                }
                self.events.push(Event::Button { button, pressed });
                Some(if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) { 1 } else { 0 })
            }
            WM_CAPTURECHANGED => {
                self.buttons = 0;
                Some(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp >> 16) & 0xffff) as i16 as f32;
                self.events.push(Event::WheelLines(delta / WHEEL_DELTA as f32));
                Some(0)
            }
            WM_SETCURSOR if (lp & 0xffff) as u32 == HTCLIENT && !self.cursor.is_null() => {
                unsafe { SetCursor(self.cursor) };
                Some(1)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                self.key_down(wp as u16, lp);
                // Alt+F4 and Alt+Space are the default procedure's.
                (msg == WM_KEYDOWN).then_some(0)
            }
            WM_KEYUP | WM_SYSKEYUP => {
                self.typing = None;
                if is_modifier(wp as u16) {
                    self.events.push(Event::Modifiers(mods()));
                }
                (msg == WM_KEYUP).then_some(0)
            }
            WM_CHAR => {
                self.char(wp as u16);
                Some(0)
            }
            // Alt+letter isn't text (and would beep); Alt+Space opens the
            // window menu.
            WM_SYSCHAR => (wp != ' ' as usize).then_some(0),
            // A bare Alt or F10 would enter the (missing) menu bar.
            WM_SYSCOMMAND if (wp as u32 & 0xfff0) == SC_KEYMENU && lp == 0 => Some(0),
            WM_DROPFILES => {
                let hdrop = wp as HDROP;
                let paths = unsafe { drop_paths(hdrop) };
                unsafe { DragFinish(hdrop) };
                self.fetched.push(Fetched::Dropped(paths));
                Some(0)
            }
            _ => None,
        };
        (result, after)
    }

    fn key_down(&mut self, vk: u16, lp: LPARAM) {
        // The input method takes it; what it commits comes as WM_CHAR.
        if vk == VK_PROCESSKEY {
            self.typing = None;
            return;
        }
        let scan = ((lp >> 16) & 0xff) as u32;
        let extended = lp & (1 << 24) != 0;
        let repeat = lp & (1 << 30) != 0;
        let mods = mods();
        if is_modifier(vk) {
            self.events.push(Event::Modifiers(mods));
            if repeat {
                return;
            }
        }
        let base = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_CHAR) } & 0xffff;
        let base = char::from_u32(base)
            .filter(|c| *c != '\0' && !c.is_control())
            .map(|c| c.to_lowercase().next().unwrap_or(c));
        let (key, keysym, from_char) = match vk_keysym(vk, scan, extended, mods.shift) {
            Some(sym) => {
                let key = named(sym).map(Key::Named).or_else(|| keypad_char(sym).map(Key::Char)).unwrap_or(Key::Other);
                (key, sym, false)
            }
            None => match base {
                Some(c) => {
                    let c = if mods.shift { c.to_uppercase().next().unwrap_or(c) } else { c };
                    (Key::Char(c), char_keysym(c), true)
                }
                None => (Key::Other, 0, false),
            },
        };
        let event = KeyEvent { key, text: None, keysym, base, code: evdev(vk, scan, extended), repeat, mods };
        self.typing = Some((self.events.len(), from_char));
        self.events.push(Event::Key(event));
    }

    /// A UTF-16 unit of typed text: the text of the key just pressed, or
    /// (from an input method, or Alt and keypad digits) text by itself.
    fn char(&mut self, unit: u16) {
        let c = match unit {
            0xd800..=0xdbff => {
                self.surrogate = Some(unit);
                return;
            }
            0xdc00..=0xdfff => {
                let Some(high) = self.surrogate.take() else { return };
                char::decode_utf16([high, unit]).next().and_then(Result::ok)
            }
            _ => char::from_u32(unit as u32),
        };
        let Some(c) = c.filter(|c| !c.is_control()) else { return };
        if let Some((i, from_char)) = self.typing
            && let Some(Event::Key(key)) = self.events.get_mut(i)
        {
            match &mut key.text {
                Some(text) => text.push(c),
                None => {
                    key.text = Some(c.to_string());
                    if !matches!(key.key, Key::Named(_)) {
                        key.key = Key::Char(c);
                    }
                    if from_char {
                        key.keysym = char_keysym(c);
                    }
                    if key.mods.ctrl && key.mods.alt {
                        // AltGr (Windows reports it as Ctrl+Alt).
                        key.mods.ctrl = false;
                        key.mods.alt = false;
                    }
                }
            }
            return;
        }
        match self.events.last_mut() {
            Some(Event::Commit(text)) => text.push(c),
            _ => self.events.push(Event::Commit(c.to_string())),
        }
    }
}

thread_local! {
    static INBOX: RefCell<Inbox> = const { RefCell::new(Inbox::new()) };
    /// The running loop, for the procedures to step while Windows runs a
    /// modal loop.
    static PUMP: Cell<Option<*mut dyn Pump>> = const { Cell::new(None) };
    /// A step is running (the procedures then only record).
    static PUMPING: Cell<bool> = const { Cell::new(false) };
}

fn inbox<R>(f: impl FnOnce(&mut Inbox) -> R) -> R {
    INBOX.with_borrow_mut(f)
}

/// Runs a step of the loop, unless one is running. Returns whether
/// there is more to do right away.
fn pump() -> bool {
    let Some(step) = PUMP.get() else { return false };
    if PUMPING.replace(true) {
        return false;
    }
    // SAFETY: the loop lives until `run` clears PUMP, and PUMPING keeps
    // this the only reference while the step runs.
    let busy = unsafe { (*step).step() };
    PUMPING.set(false);
    busy
}

/// Inside a modal loop, handles what was recorded.
fn modal_pump() {
    if INBOX.with(|i| i.try_borrow().is_ok_and(|i| i.modal && i.pending())) {
        for _ in 0..8 {
            if !pump() {
                break;
            }
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (result, after) = INBOX.with(|cell| match cell.try_borrow_mut() {
        Ok(mut i) if i.main == hwnd => i.handle(hwnd, msg, wp, lp),
        _ => (None, After::Nothing),
    });
    unsafe {
        match after {
            After::Nothing => {}
            After::Capture => {
                SetCapture(hwnd);
            }
            After::Release => {
                ReleaseCapture();
            }
            After::Move(r) => {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
    modal_pump();
    match result {
        Some(r) => r,
        None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn helper_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_WAKE {
        INBOX.with(|cell| {
            if let Ok(mut i) = cell.try_borrow_mut() {
                if let Some(wake) = &i.wake {
                    wake.pending.store(false, Ordering::Release);
                }
                i.woken = true;
            }
        });
        modal_pump();
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---- keys ----------------------------------------------------------------------------

fn mods() -> Mods {
    let down = |vk: u16| unsafe { GetKeyState(vk as i32) } < 0;
    Mods { ctrl: down(VK_CONTROL), shift: down(VK_SHIFT), alt: down(VK_MENU), logo: down(VK_LWIN) || down(VK_RWIN) }
}

fn is_modifier(vk: u16) -> bool {
    matches!(
        vk,
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL | VK_LMENU
            | VK_RMENU
    )
}

/// The XKB keysym of a key that isn't a character key (those get their
/// character's).
fn vk_keysym(vk: u16, scan: u32, extended: bool, shift: bool) -> Option<u32> {
    // Without NumLock, the keypad's keys are the navigation keys, not
    // extended.
    let pad = |main: u32, keypad: u32| if extended { main } else { keypad };
    Some(match vk {
        VK_BACK => 0xff08,
        VK_TAB if shift => 0xfe20,
        VK_TAB => 0xff09,
        VK_RETURN if extended => 0xff8d,
        VK_RETURN => 0xff0d,
        VK_PAUSE => 0xff13,
        VK_SCROLL => 0xff14,
        VK_ESCAPE => 0xff1b,
        VK_DELETE => pad(0xffff, 0xff9f),
        VK_HOME => pad(0xff50, 0xff95),
        VK_LEFT => pad(0xff51, 0xff96),
        VK_UP => pad(0xff52, 0xff97),
        VK_RIGHT => pad(0xff53, 0xff98),
        VK_DOWN => pad(0xff54, 0xff99),
        VK_PRIOR => pad(0xff55, 0xff9a),
        VK_NEXT => pad(0xff56, 0xff9b),
        VK_END => pad(0xff57, 0xff9c),
        VK_CLEAR => 0xff9d,
        VK_INSERT => pad(0xff63, 0xff9e),
        VK_SNAPSHOT => 0xff61,
        VK_APPS => 0xff67,
        VK_NUMLOCK => 0xff7f,
        VK_NUMPAD0..=VK_NUMPAD9 => 0xffb0 + (vk - VK_NUMPAD0) as u32,
        VK_MULTIPLY => 0xffaa,
        VK_ADD => 0xffab,
        VK_SUBTRACT => 0xffad,
        VK_DECIMAL => 0xffae,
        VK_DIVIDE => 0xffaf,
        VK_F1..=VK_F24 => 0xffbe + (vk - VK_F1) as u32,
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => {
            if vk == VK_RSHIFT || scan == 0x36 {
                0xffe2
            } else {
                0xffe1
            }
        }
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => {
            if vk == VK_RCONTROL || extended {
                0xffe4
            } else {
                0xffe3
            }
        }
        VK_MENU | VK_LMENU | VK_RMENU => {
            if vk == VK_RMENU || extended {
                0xffea
            } else {
                0xffe9
            }
        }
        VK_CAPITAL => 0xffe5,
        VK_LWIN => 0xffeb,
        VK_RWIN => 0xffec,
        _ => return None,
    })
}

/// The character of a keypad keysym, as libxkbcommon gives it.
fn keypad_char(sym: u32) -> Option<char> {
    match sym {
        0xffb0..=0xffb9 => char::from_digit(sym - 0xffb0, 10),
        0xffaa => Some('*'),
        0xffab => Some('+'),
        0xffad => Some('-'),
        0xffae => Some('.'),
        0xffaf => Some('/'),
        _ => None,
    }
}

/// The Linux evdev code of a key from its (set 1) scan code: the same
/// number for most keys, others for the extended ones.
fn evdev(vk: u16, scan: u32, extended: bool) -> u32 {
    if vk == VK_PAUSE {
        return 119;
    }
    if !extended {
        return scan;
    }
    match scan {
        0x1c => 96,  // keypad Enter
        0x1d => 97,  // right Ctrl
        0x35 => 98,  // keypad /
        0x37 => 99,  // Print Screen
        0x38 => 100, // right Alt
        0x45 => 69,  // Num Lock
        0x47 => 102, // Home
        0x48 => 103, // Up
        0x49 => 104, // Page Up
        0x4b => 105, // Left
        0x4d => 106, // Right
        0x4f => 107, // End
        0x50 => 108, // Down
        0x51 => 109, // Page Down
        0x52 => 110, // Insert
        0x53 => 111, // Delete
        0x5b => 125, // left Windows key
        0x5c => 126, // right Windows key
        0x5d => 127, // Menu
        _ => 0,
    }
}

// ---- the clipboard and drops ---------------------------------------------------------

/// The clipboard, open (closed when dropped).
struct OpenClip;

impl OpenClip {
    /// Opens the clipboard; another program may hold it for a moment.
    fn open(owner: HWND) -> Option<OpenClip> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(owner) } != 0 {
                return Some(OpenClip);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn bytes(&self, format: u32) -> Option<Vec<u8>> {
        unsafe {
            if IsClipboardFormatAvailable(format) == 0 {
                return None;
            }
            let handle: HANDLE = GetClipboardData(format);
            if handle.is_null() {
                return None;
            }
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                return None;
            }
            let data = std::slice::from_raw_parts(ptr as *const u8, GlobalSize(handle)).to_vec();
            GlobalUnlock(handle);
            Some(data)
        }
    }

    fn text(&self) -> Option<String> {
        let data = self.bytes(CF_UNICODETEXT as u32)?;
        let units: Vec<u16> = data.as_chunks::<2>().0.iter().map(|&b| u16::from_ne_bytes(b)).take_while(|&u| u != 0).collect();
        Some(String::from_utf16_lossy(&units).replace("\r\n", "\n"))
    }

    fn files(&self) -> Vec<PathBuf> {
        unsafe {
            if IsClipboardFormatAvailable(CF_HDROP as u32) == 0 {
                return Vec::new();
            }
            let handle = GetClipboardData(CF_HDROP as u32);
            if handle.is_null() { Vec::new() } else { drop_paths(handle as HDROP) }
        }
    }

    /// An image: PNG where a program offers it, else the bitmap as a BMP
    /// file.
    fn image(&self, png: &[u32]) -> Option<(Vec<u8>, &'static str)> {
        if let Some(data) = png.iter().filter(|&&f| f != 0).find_map(|&f| self.bytes(f)) {
            return Some((data, "png"));
        }
        Some((dib_to_bmp(&self.bytes(CF_DIB as u32)?)?, "bmp"))
    }
}

impl Drop for OpenClip {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

/// The paths of a drop (or of files on the clipboard).
unsafe fn drop_paths(hdrop: HDROP) -> Vec<PathBuf> {
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, null_mut(), 0) };
    (0..count)
        .filter_map(|i| {
            let len = unsafe { DragQueryFileW(hdrop, i, null_mut(), 0) } as usize;
            let mut buf = vec![0u16; len + 1];
            let n = unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32) } as usize;
            (n > 0).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
        })
        .collect()
}

// ---- windows -------------------------------------------------------------------------

struct Win {
    hwnd: HWND,
    spec: Spec,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    /// Shown; false while parked.
    shown: bool,
    /// Client size, physical.
    size: (u32, u32),
    scale: f32,
    needs_attach: bool,
    /// The Vulkan surface must be created for it (new window).
    fresh: bool,
}

struct W {
    hinstance: HINSTANCE,
    class: Vec<u16>,
    /// A message-only window: wake-ups, the clipboard's owner.
    helper: HWND,
    win: Option<Win>,
    /// Arrow, hand, I-beam.
    cursors: [HCURSOR; 3],
    cursor_dirty: bool,
    /// The registered "PNG" and "image/png" clipboard formats.
    png: [u32; 2],
    system_scale: f32,
}

fn werr(what: &str) -> String {
    format!("Win32: {what} failed (error {})", unsafe { GetLastError() })
}

fn register(hinstance: HINSTANCE, name: &[u16], proc: WNDPROC) -> Result<(), String> {
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: proc,
        hInstance: hinstance,
        lpszClassName: name.as_ptr(),
        ..WNDCLASSEXW::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
        return Err(werr("RegisterClassExW"));
    }
    Ok(())
}

/// The window's styles for a spec.
fn styles(spec: &Spec) -> (WINDOW_STYLE, WINDOW_EX_STYLE) {
    if spec.layer_shell {
        let mut ex = WS_EX_TOOLWINDOW;
        if matches!(spec.layer, Layer::Top | Layer::Overlay) {
            ex |= WS_EX_TOPMOST;
        }
        if spec.keyboard == Keyboard::None {
            ex |= WS_EX_NOACTIVATE;
        }
        (WS_POPUP, ex)
    } else if spec.resizable {
        (WS_OVERLAPPEDWINDOW, 0)
    } else {
        (WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX), 0)
    }
}

fn physical((w, h): (u32, u32), scale: f32) -> (u32, u32) {
    let p = |v: u32| ((v as f32 * scale).round() as u32).max(1);
    (p(w), p(h))
}

/// The outer size of a window with this client size.
fn outer((w, h): (u32, u32), style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, dpi: u32) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    unsafe { AdjustWindowRectExForDpi(&mut r, style, 0, ex, dpi) };
    (r.right - r.left, r.bottom - r.top)
}

fn window_dpi(hwnd: HWND) -> u32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        dpi => dpi,
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(hwnd, &mut r) };
    (((r.right - r.left) as u32).max(1), ((r.bottom - r.top) as u32).max(1))
}

/// The work area (x, y, width, height) and DPI of the monitor under the
/// pointer.
fn monitor_under_pointer() -> ((i32, i32, i32, i32), u32) {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let r = info.rcWork;
        let (mut dpi, mut dpi_y) = (0, 0);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) != 0 || dpi == 0 {
            dpi = GetDpiForSystem();
        }
        ((r.left, r.top, r.right - r.left, r.bottom - r.top), dpi)
    }
}

/// Brings a window to the front and gives it the keyboard. Windows lets
/// only the foreground process do that, so this borrows the foreground
/// thread's input state for the moment.
fn foreground(hwnd: HWND) {
    unsafe {
        let fg = GetForegroundWindow();
        let me = GetCurrentThreadId();
        let other = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, null_mut()) };
        let attach = other != 0 && other != me && fg != hwnd;
        if attach {
            AttachThreadInput(me, other, 1);
        }
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        SetFocus(hwnd);
        if attach {
            AttachThreadInput(me, other, 0);
        }
    }
}

impl W {
    fn new() -> Result<W, String> {
        unsafe {
            // Fails when a manifest already chose; that choice stands.
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let hinstance = GetModuleHandleW(null());
            let class = wide("qui");
            let helper_class = wide("qui-helper");
            register(hinstance, &class, Some(main_proc))?;
            register(hinstance, &helper_class, Some(helper_proc))?;
            let helper = CreateWindowExW(
                0,
                helper_class.as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                hinstance,
                null(),
            );
            if helper.is_null() {
                return Err(werr("CreateWindowExW"));
            }
            let png = ["PNG", "image/png"].map(|name| RegisterClipboardFormatW(wide(name).as_ptr()));
            let system_scale = GetDpiForSystem() as f32 / 96.0;
            stat(format_args!("Win32, system scale {system_scale}"));
            Ok(W {
                hinstance,
                class,
                helper,
                win: None,
                cursors: [IDC_ARROW, IDC_HAND, IDC_IBEAM].map(|c| LoadCursorW(null_mut(), c)),
                cursor_dirty: false,
                png,
                system_scale,
            })
        }
    }

    fn visible(&self) -> bool {
        self.win.as_ref().is_some_and(|w| w.shown)
    }

    fn scale(&self) -> f32 {
        self.win.as_ref().map_or(self.system_scale, |w| w.scale)
    }

    fn drawable(&self) -> bool {
        self.visible() && !inbox(|i| i.minimized)
    }

    /// The smallest outer size of a resizable toplevel, or zero.
    fn min_track(spec: &Spec, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, scale: f32) -> (i32, i32) {
        if spec.layer_shell || !spec.resizable || spec.min_size == (0, 0) {
            return (0, 0);
        }
        let min = physical((spec.min_size.0.max(1), spec.min_size.1.max(1)), scale);
        outer(min, style, ex, (scale * 96.0).round() as u32)
    }

    /// Creates the window for `spec`, shown if `show` (else parked, as
    /// [`Cx::prepare`] asks). `dark`: the title bar should be dark.
    fn create(&mut self, spec: Spec, show: bool, dark: bool) -> Result<(), String> {
        let (style, ex) = styles(&spec);
        let title = wide(&spec.title);
        let (hwnd, scale) = unsafe {
            if spec.layer_shell {
                let (monitor, dpi) = monitor_under_pointer();
                let scale = dpi as f32 / 96.0;
                let size = physical((spec.width, spec.height), scale);
                let (x, y) = place(&spec, scale, size, monitor);
                let hwnd = CreateWindowExW(
                    ex,
                    self.class.as_ptr(),
                    title.as_ptr(),
                    style,
                    x,
                    y,
                    size.0 as i32,
                    size.1 as i32,
                    null_mut(),
                    null_mut(),
                    self.hinstance,
                    null(),
                );
                (hwnd, scale)
            } else {
                let hwnd = CreateWindowExW(
                    ex,
                    self.class.as_ptr(),
                    title.as_ptr(),
                    style,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    null_mut(),
                    null_mut(),
                    self.hinstance,
                    null(),
                );
                if hwnd.is_null() {
                    return Err(werr("CreateWindowExW"));
                }
                // Sized at the scale of the monitor it was put on.
                let dpi = window_dpi(hwnd);
                let scale = dpi as f32 / 96.0;
                let (w, h) = outer(physical((spec.width, spec.height), scale), style, ex, dpi);
                SetWindowPos(hwnd, null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                (hwnd, scale)
            }
        };
        if hwnd.is_null() {
            return Err(werr("CreateWindowExW"));
        }
        unsafe {
            DragAcceptFiles(hwnd, 1);
            if !spec.layer_shell && dark {
                let on: i32 = 1;
                DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, (&on as *const i32).cast(), 4);
            }
        }
        let size = client_size(hwnd);
        stat(format_args!(
            "window {}x{}, scale {scale}{}",
            size.0,
            size.1,
            if spec.layer_shell { ", popup" } else { "" }
        ));
        let min_track = Self::min_track(&spec, style, ex, scale);
        let layer = spec.layer_shell;
        let arrow = self.cursors[0];
        inbox(|i| {
            i.main = hwnd;
            i.layer = layer;
            i.min_track = min_track;
            i.cursor = arrow;
            i.minimized = false;
            i.size = None;
            i.dpi = None;
        });
        self.win = Some(Win {
            hwnd,
            spec,
            style,
            ex_style: ex,
            shown: false,
            size,
            scale,
            needs_attach: true,
            fresh: true,
        });
        self.cursor_dirty = true;
        if show {
            self.map();
        }
        Ok(())
    }

    fn map(&mut self) {
        let Some(w) = &mut self.win else { return };
        w.shown = true;
        let (hwnd, layer_shell, layer, keyboard) = (w.hwnd, w.spec.layer_shell, w.spec.layer, w.spec.keyboard);
        unsafe {
            if layer_shell {
                let after = match layer {
                    Layer::Top | Layer::Overlay => HWND_TOPMOST,
                    Layer::Background | Layer::Bottom => HWND_BOTTOM,
                };
                SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                ShowWindow(hwnd, if keyboard == Keyboard::None { SW_SHOWNOACTIVATE } else { SW_SHOW });
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
        }
        if !layer_shell || keyboard != Keyboard::None {
            foreground(hwnd);
        }
        let exclusive = layer_shell && keyboard == Keyboard::Exclusive;
        inbox(|i| {
            i.exclusive = exclusive;
            i.redraw = true;
        });
    }

    /// Shows `spec`: a new window, or the existing one (parked or shown)
    /// updated.
    fn show(&mut self, spec: Spec, dark: bool) -> Result<(), String> {
        let Some(w) = &self.win else { return self.create(spec, true, dark) };
        let same_kind = w.spec.layer_shell == spec.layer_shell
            && if spec.layer_shell {
                (w.spec.layer, w.spec.keyboard) == (spec.layer, spec.keyboard)
            } else {
                w.spec.resizable == spec.resizable
            };
        if !same_kind {
            self.destroy_window();
            return self.create(spec, true, dark);
        }
        if w.shown && w.spec == spec {
            return Ok(());
        }
        let Some(w) = &mut self.win else { return Ok(()) };
        let hwnd = w.hwnd;
        unsafe {
            if w.spec.title != spec.title {
                SetWindowTextW(hwnd, wide(&spec.title).as_ptr());
            }
            if spec.layer_shell {
                // Placed anew, on the monitor under the pointer, at its scale.
                let (monitor, dpi) = monitor_under_pointer();
                let scale = dpi as f32 / 96.0;
                let size = physical((spec.width, spec.height), scale);
                let (x, y) = place(&spec, scale, size, monitor);
                SetWindowPos(hwnd, null_mut(), x, y, size.0 as i32, size.1 as i32, SWP_NOZORDER | SWP_NOACTIVATE);
                if scale != w.scale {
                    w.scale = scale;
                    inbox(|i| i.redraw = true);
                }
            } else if (w.spec.width, w.spec.height) != (spec.width, spec.height) {
                let size = physical((spec.width, spec.height), w.scale);
                let (ow, oh) = outer(size, w.style, w.ex_style, window_dpi(hwnd));
                SetWindowPos(hwnd, null_mut(), 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        let size = client_size(hwnd);
        if size != w.size {
            w.size = size;
            w.needs_attach = true;
        }
        let min_track = Self::min_track(&spec, w.style, w.ex_style, w.scale);
        inbox(|i| i.min_track = min_track);
        w.spec = spec;
        if !w.shown {
            self.map();
        }
        Ok(())
    }

    /// Takes in what the procedures recorded about the window's size and
    /// scale. Returns whether the scale changed (all must be rebuilt).
    fn update(&mut self, t: &Taken) -> bool {
        let Some(w) = &mut self.win else { return false };
        let mut rescaled = false;
        if let Some(dpi) = t.dpi {
            let scale = dpi as f32 / 96.0;
            if scale != w.scale {
                w.scale = scale;
                rescaled = true;
                if w.spec.layer_shell {
                    // Keep the logical size (a toplevel was resized by
                    // the procedure, as Windows suggested).
                    let (pw, ph) = physical((w.spec.width, w.spec.height), scale);
                    unsafe {
                        SetWindowPos(
                            w.hwnd,
                            null_mut(),
                            0,
                            0,
                            pw as i32,
                            ph as i32,
                            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                        )
                    };
                }
                let min_track = Self::min_track(&w.spec, w.style, w.ex_style, scale);
                inbox(|i| i.min_track = min_track);
            }
        }
        if t.size.is_some() || rescaled {
            let size = client_size(w.hwnd);
            if size != w.size && !inbox(|i| i.minimized) {
                w.size = size;
                w.needs_attach = true;
            }
        }
        rescaled
    }

    /// Hides the window but keeps it (and its swapchain).
    fn park(&mut self) {
        inbox(Inbox::lose_input);
        if let Some(w) = &mut self.win {
            w.shown = false;
            unsafe { ShowWindow(w.hwnd, SW_HIDE) };
        }
    }

    fn destroy_window(&mut self) {
        inbox(Inbox::lose_input);
        if let Some(w) = self.win.take() {
            inbox(|i| {
                i.main = null_mut();
                i.minimized = false;
            });
            unsafe { DestroyWindow(w.hwnd) };
        }
    }

    fn set_cursor(&self, cursor: Cursor) {
        let handle = self.cursors[match cursor {
            Cursor::Default => 0,
            Cursor::Pointer => 1,
            Cursor::Text => 2,
        }];
        if inbox(|i| {
            i.cursor = handle;
            i.pointer_inside
        }) {
            unsafe { SetCursor(handle) };
        }
    }

    fn copy(&self, text: &str) {
        let units: Vec<u16> = wide(&text.replace("\r\n", "\n").replace('\n', "\r\n"));
        let Some(_clip) = OpenClip::open(self.helper) else {
            eprintln!("qui: cannot open the clipboard");
            return;
        };
        unsafe {
            EmptyClipboard();
            let handle = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
            if handle.is_null() {
                return;
            }
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                GlobalFree(handle);
                return;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr.cast::<u16>(), units.len());
            GlobalUnlock(handle);
            if SetClipboardData(CF_UNICODETEXT as u32, handle).is_null() {
                GlobalFree(handle);
            }
        }
    }

    /// Reads the clipboard: the text for the focused field, or everything.
    fn paste(&self, field: bool) {
        let fetched = match OpenClip::open(self.helper) {
            None => Fetched::Error("Cannot open the clipboard (another program holds it)".into()),
            Some(clip) if field => match clip.text() {
                Some(text) => Fetched::Field(text),
                None => return,
            },
            Some(clip) => {
                let mut pasted = Pasted {
                    files: clip.files(),
                    image: clip.image(&self.png),
                    text: clip.text().filter(|t| !t.trim().is_empty()),
                };
                drop(clip);
                pasted.drop_repeats(&[]);
                Fetched::Pasted(pasted)
            }
        };
        inbox(|i| i.fetched.push(fetched));
    }
}

impl Drop for W {
    fn drop(&mut self) {
        self.destroy_window();
        unsafe { DestroyWindow(self.helper) };
    }
}

// ---- the loop ------------------------------------------------------------------------

/// The loop's state, stepped by [`pump`].
trait Pump {
    /// Handles what was recorded. Returns whether there is more to do
    /// right away.
    fn step(&mut self) -> bool;
    fn outcome(&mut self) -> Option<Result<(), String>>;
    fn done(&self) -> bool;
}

struct Loop<A: App> {
    runner: Runner<A>,
    w: W,
    started: bool,
    outcome: Option<Result<(), String>>,
}

impl<A: App> Pump for Loop<A> {
    fn step(&mut self) -> bool {
        if self.outcome.is_some() {
            return false;
        }
        let result = if std::mem::replace(&mut self.started, true) {
            self.runner.process(&mut self.w)
        } else {
            let mut cx = Cx { scale: self.w.scale(), ..Cx::default() };
            self.runner.app.start(&mut cx);
            self.runner.apply(&mut self.w, cx)
        };
        match result {
            Ok(false) => {}
            Ok(true) => self.outcome = Some(Ok(())),
            Err(e) => self.outcome = Some(Err(e)),
        }
        self.outcome.is_none() && (self.w.win.as_ref().is_some_and(|w| w.needs_attach) || inbox(|i| i.pending()))
    }

    fn outcome(&mut self) -> Option<Result<(), String>> {
        self.outcome.take()
    }

    fn done(&self) -> bool {
        self.outcome.is_some()
    }
}

/// Runs the app on Windows until [`Cx::exit`].
pub(crate) fn run<A: App>(make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    if PUMP.get().is_some() {
        return Err("qui is already running on this thread".into());
    }
    let w = W::new()?;
    let wake = Arc::new(Wake { hwnd: AtomicUsize::new(w.helper as usize), pending: AtomicBool::new(false) });
    inbox(|i| {
        *i = Inbox::new();
        i.wake = Some(wake.clone());
    });
    let app = make(Waker(wake.clone()));
    let state: Box<dyn Pump> = Box::new(Loop { runner: Runner { app, p: Painter::new() }, w, started: false, outcome: None });
    let state = Box::into_raw(state);
    PUMP.set(Some(state));
    // SAFETY (the derefs of `state` below): it is only used through this
    // pointer, and not while a step runs (steps only run from `pump`).
    let done = || unsafe { (*state).done() };
    let mut busy = pump();
    let mut msg = MSG::default();
    'run: while !done() {
        let mut got = false;
        while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                break 'run;
            }
            got = true;
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if done() {
                break 'run;
            }
        }
        // Messages sent from other threads (another app's activation
        // deactivating ours, say) run inside PeekMessageW without it
        // returning one, so `got` misses what they recorded.
        if got || busy || inbox(|i| i.pending()) {
            busy = pump();
            continue;
        }
        unsafe { MsgWaitForMultipleObjectsEx(0, null(), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
    }
    PUMP.set(None);
    wake.hwnd.store(0, Ordering::Release);
    let mut state = unsafe { Box::from_raw(state) };
    let outcome = state.outcome().unwrap_or(Ok(()));
    // The renderer goes before the windows.
    drop(state);
    inbox(|i| *i = Inbox::new());
    outcome
}

struct Runner<A: App> {
    app: A,
    p: Painter,
}

impl<A: App> Runner<A> {
    /// Handles what the procedures recorded. Returns true to exit.
    fn process(&mut self, w: &mut W) -> Result<bool, String> {
        let t = inbox(Inbox::take);
        let mut force = t.redraw;
        force |= w.update(&t);
        let mut rebuild = false;
        let mut cx = Cx { visible: w.visible(), scale: w.scale(), ..Cx::default() };
        for f in t.fetched {
            rebuild = true;
            self.p.fetched(&mut self.app, f);
        }
        if t.woken && self.app.woken(&mut cx) {
            rebuild = true;
        }
        if let Some(focused) = t.focus_changed {
            self.app.focus(focused, &mut cx);
            rebuild = true;
        }
        if t.outside_click {
            self.app.clicked_outside(&mut cx);
            rebuild = true;
        }
        if t.closed {
            self.teardown(w);
            self.app.closed(&mut cx);
        }
        let (changed, paste) = self.p.events(&t.events);
        rebuild |= changed;
        if let Some(field) = paste {
            w.paste(field);
        }
        // A new or resized window: (re)attach the swapchain.
        if let Some(win) = &mut w.win
            && win.needs_attach
        {
            win.needs_attach = false;
            let native = Native::Win32 { hinstance: w.hinstance, hwnd: win.hwnd };
            match &mut self.p.gfx {
                Some(gfx) if win.fresh => unsafe { gfx.attach_native(native, win.size)? },
                Some(gfx) => gfx.resize(win.size.0, win.size.1),
                None => {
                    // Opaque: Windows' drivers don't composite alpha.
                    let gfx = unsafe { Gfx::new(native, win.size, false) }?;
                    if self.p.stats {
                        eprintln!("qui: rendering on {} (Win32)", gfx.device_name);
                    }
                    self.p.gfx = Some(gfx);
                }
            }
            win.fresh = false;
            if !win.shown
                && let Some(gfx) = &mut self.p.gfx
            {
                // Prepared: the swapchain now, the first frame on show.
                gfx.ensure_swapchain()?;
            }
            stat(format_args!("swapchain attached"));
            self.p.ensure_ui(&self.app)?;
            force = true;
        }
        let drawable = w.drawable();
        if drawable && (force || rebuild || cx.again) && self.build(w, &mut cx, force) {
            self.p.dirty = true;
            stat(format_args!("built"));
        }
        if self.p.dirty && drawable {
            self.p.draw(false)?;
        }
        self.apply(w, cx)
    }

    fn build(&mut self, w: &mut W, cx: &mut Cx, force: bool) -> bool {
        let Some(win) = &w.win else { return false };
        let logical = (win.size.0 as f32 / win.scale, win.size.1 as f32 / win.scale);
        let changed = self.p.build(&mut self.app, cx, logical, win.scale, force);
        if let Some(cursor) = self.p.cursor_change(&mut w.cursor_dirty) {
            w.set_cursor(cursor);
        }
        changed
    }

    /// Whether the UI's theme is dark (for the title bar).
    fn dark(&self) -> bool {
        let bg = self.p.ui.as_ref().map_or_else(|| self.app.theme().bg, |ui| ui.theme.bg);
        let [r, g, b, _] = bg.floats();
        0.2126 * r + 0.7152 * g + 0.0722 * b < 0.5
    }

    /// Applies the app's requests. Returns true to exit.
    fn apply(&mut self, w: &mut W, cx: Cx) -> Result<bool, String> {
        if let Some(text) = &cx.copy {
            w.copy(text);
        }
        if cx.paste {
            w.paste(false);
        }
        if cx.exit {
            self.teardown(w);
            return Ok(true);
        }
        if cx.hide && w.visible() {
            if !cx.release_gpu && w.win.as_ref().is_some_and(|w| w.spec.keep_when_hidden) {
                w.park();
                self.p.dirty = false;
            } else {
                self.teardown(w);
            }
        }
        if cx.release_gpu && !w.visible() {
            self.teardown(w);
            self.p.release_gpu();
        }
        if let Some(spec) = cx.prepare
            && w.win.is_none()
            && spec.keep_when_hidden
        {
            let dark = self.dark();
            w.create(spec, false, dark)?;
        }
        if let Some(spec) = cx.show {
            if !w.visible() {
                self.p.shown_at = Some(Instant::now());
                stat(format_args!("show"));
            }
            let dark = self.dark();
            w.show(spec, dark)?;
        }
        Ok(false)
    }

    fn teardown(&mut self, w: &mut W) {
        self.p.detach();
        w.destroy_window();
    }
}
