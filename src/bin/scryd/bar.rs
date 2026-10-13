//! The search bar: a hotkey (Alt+Shift+Space, or `hotkey` in config.json)
//! or a click on the tray icon brings up a box in the middle of the screen. Results come as you type;
//! Up/Down picks one, Enter opens it, Ctrl+Enter shows it in Explorer, Esc
//! or a click elsewhere puts the bar away. The whole query language works
//! (`ext:pdf invoice`, `in:~\Documents report`, `grep:TODO`), and when no
//! file name matches, the words are looked for inside files.
//!
//! Searches run on a worker thread, which also looks up the icons (shell
//! calls that may block), so typing never waits on either.

use crate::usage::{History, Opened};
use scry::{Engine, Found, Grep, GrepMode, Query, paths, walk};
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Condvar, Mutex};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Tried in turn when config.json names none: the first that no other
/// program holds. (Taken often enough: Ctrl+Space, autocomplete in IDEs;
/// Alt+Space and Win+Alt+Space, PowerToys; Win+Space, the keyboard layout.)
pub const DEFAULT_HOTKEYS: &[&str] = &["Alt+Shift+Space", "Win+Shift+F", "Ctrl+Alt+F"];

const ID_EDIT: usize = 1;
const ID_LIST: usize = 2;
/// The worker has results.
const WM_RESULTS: u32 = WM_APP + 1;
/// Rows shown at once; the rest scroll.
const VISIBLE: usize = 8;
const LIMIT: usize = 50;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// One result row.
struct Row {
    /// What Enter opens; None for a message ("No results").
    path: Option<PathBuf>,
    name: String,
    /// The folder it is in (or, for a content match, that and the line).
    detail: String,
    /// In the system image list; -1 for none.
    icon: i32,
}

/// Colors, as COLORREF (0x00BBGGRR).
#[derive(Clone, Copy, PartialEq)]
struct Theme {
    bg: u32,
    text: u32,
    dim: u32,
    sel: u32,
    line: u32,
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

const DARK: Theme = Theme {
    bg: rgb(0x20, 0x20, 0x20),
    text: rgb(0xF2, 0xF2, 0xF2),
    dim: rgb(0x9C, 0x9C, 0x9C),
    sel: rgb(0x2E, 0x45, 0x66),
    line: rgb(0x3A, 0x3A, 0x3A),
};
const LIGHT: Theme = Theme {
    bg: rgb(0xFF, 0xFF, 0xFF),
    text: rgb(0x1B, 0x1B, 0x1B),
    dim: rgb(0x6B, 0x6B, 0x6B),
    sel: rgb(0xD6, 0xE8, 0xFA),
    line: rgb(0xE3, 0xE3, 0xE3),
};

/// Apps' light or dark setting (Settings > Personalization > Colors).
fn system_theme() -> Theme {
    let (key, value) = (wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"), wide("AppsUseLightTheme"));
    let (mut light, mut size) = (1u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut light).cast(), &mut size) };
    if light == 0 { DARK } else { LIGHT }
}

/// Windows 11 draws a thin border round a window with rounded corners: in
/// the theme's line color.
fn border(hwnd: HWND, t: Theme) {
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as _, (&raw const t.line).cast(), 4) };
}

/// Sizes at a DPI, in physical pixels.
struct Metrics {
    width: i32,
    pad: i32,
    /// Room for the magnifier left of the query.
    glyph: i32,
    edit_h: i32,
    top_h: i32,
    row_h: i32,
}

fn metrics(dpi: u32) -> Metrics {
    let px = |v: f32| (v * dpi as f32 / 96.0).round() as i32;
    let (pad, edit_h) = (px(14.0), px(30.0));
    Metrics { width: px(660.0), pad, glyph: px(30.0), edit_h, top_h: 2 * pad + edit_h, row_h: px(48.0) }
}

fn font(px: f32, dpi: u32, weight: i32, face: &str) -> HFONT {
    let face = wide(face);
    let h = -(px * dpi as f32 / 96.0).round() as i32;
    unsafe {
        CreateFontW(
            h,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            0,
            face.as_ptr(),
        )
    }
}

struct Ui {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    dpi: u32,
    theme: Theme,
    bg: HBRUSH,
    sel: HBRUSH,
    query_font: HFONT,
    glyph_font: HFONT,
    name_font: HFONT,
    detail_font: HFONT,
    images: HIMAGELIST,
    icon_px: i32,
    rows: Vec<Row>,
    /// Top-center of the bar, on the screen it was brought up on.
    anchor: (i32, i32),
    /// Which query the rows answer.
    seq: u64,
    work: Arc<Work>,
    /// Your past searches; which one Up brought back (0 the latest) while
    /// the box still holds it; and that the box is being set to one.
    history: History,
    recalled: Option<usize>,
    recalling: bool,
}

impl Ui {
    fn set_theme(&mut self, t: Theme) {
        unsafe {
            DeleteObject(self.bg);
            DeleteObject(self.sel);
            self.bg = CreateSolidBrush(t.bg);
            self.sel = CreateSolidBrush(t.sel);
        }
        self.theme = t;
    }

    /// Fonts for `dpi`; the controls are told by `apply_fonts`.
    fn set_dpi(&mut self, dpi: u32) {
        unsafe {
            for f in [self.query_font, self.glyph_font, self.name_font, self.detail_font] {
                DeleteObject(f);
            }
        }
        self.dpi = dpi;
        self.query_font = font(20.0, dpi, FW_NORMAL as i32, "Segoe UI");
        self.glyph_font = font(17.0, dpi, FW_NORMAL as i32, "Segoe MDL2 Assets");
        self.name_font = font(15.0, dpi, FW_SEMIBOLD as i32, "Segoe UI");
        self.detail_font = font(12.0, dpi, FW_NORMAL as i32, "Segoe UI");
    }
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// The UI state, briefly. Never send a window message from inside `f`: the
/// answer may come back through `wndproc` while it is borrowed.
fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with_borrow_mut(|u| u.as_mut().map(f))
}

/// The query waiting for the worker (only the latest), and its answer.
struct Work {
    want: Mutex<Option<(u64, String)>>,
    wake: Condvar,
    done: Mutex<Option<(u64, Vec<Row>)>>,
    /// What you opened before, which ranks higher.
    opened: Mutex<Opened>,
}

/// "Ctrl+Alt+Space", "Win+Shift+F" or "Alt+F1" as RegisterHotKey wants it.
fn parse_hotkey(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let (mut mods, mut vk) = (0, None);
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            "space" => vk = Some(VK_SPACE as u32),
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => vk = Some(k.as_bytes()[0].to_ascii_uppercase() as u32),
            k => {
                let n: u32 = k.strip_prefix('f')?.parse().ok().filter(|n| (1..=24).contains(n))?;
                vk = Some(VK_F1 as u32 + n - 1);
            }
        }
    }
    Some((mods, vk?))
}

/// Make the bar (hidden) on this thread, and take the first of `hotkeys`
/// no other program holds: which one, or None. Err if there is no window.
pub fn create(engine: Engine, home: String, dir: &Path, hotkeys: &[String]) -> Result<Option<String>, String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as _);
        let inst = GetModuleHandleW(null());
        let class = wide("scry-bar");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassExW(&wc);
        // A tool window: no taskbar button, not in Alt+Tab.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class.as_ptr(),
            wide("Scry").as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if hwnd.is_null() {
            return Err(format!("search bar: no window ({})", std::io::Error::last_os_error()));
        }
        let edit = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
            0,
            0,
            10,
            10,
            hwnd,
            ID_EDIT as HMENU,
            inst,
            null(),
        );
        // No data of its own (the rows live in Ui), drawn by us, no scroll bar.
        let style = WS_CHILD | WS_VISIBLE | (LBS_NODATA | LBS_OWNERDRAWFIXED | LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32;
        let list = CreateWindowExW(0, wide("LISTBOX").as_ptr(), null(), style, 0, 0, 10, 10, hwnd, ID_LIST as HMENU, inst, null());
        SendMessageW(edit, EM_SETCUEBANNER, 1, wide("Search files").as_ptr() as LPARAM);
        let round = DWMWCP_ROUND;
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, (&raw const round).cast(), 4);
        // The system's big icons, by index: the same list in every thread.
        let mut sfi: SHFILEINFOW = zeroed();
        let probe = wide("x.txt");
        let images = SHGetFileInfoW(
            probe.as_ptr(),
            FILE_ATTRIBUTE_NORMAL,
            &mut sfi,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        ) as HIMAGELIST;
        let (mut cx, mut cy) = (32, 32);
        ImageList_GetIconSize(images, &mut cx, &mut cy);
        let work = Arc::new(Work { want: Mutex::new(None), wake: Condvar::new(), done: Mutex::new(None), opened: Mutex::new(Opened::load(dir)) });
        let mut ui = Ui {
            hwnd,
            edit,
            list,
            dpi: GetDpiForSystem(),
            theme: LIGHT,
            bg: null_mut(),
            sel: null_mut(),
            query_font: null_mut(),
            glyph_font: null_mut(),
            name_font: null_mut(),
            detail_font: null_mut(),
            images,
            icon_px: cy,
            rows: Vec::new(),
            anchor: (0, 0),
            seq: 0,
            work: work.clone(),
            history: History::load(dir),
            recalled: None,
            recalling: false,
        };
        let theme = system_theme();
        ui.set_theme(theme);
        ui.set_dpi(ui.dpi);
        UI.with_borrow_mut(|u| *u = Some(ui));
        border(hwnd, theme);
        apply_fonts();

        let got = hotkeys.iter().find(|k| match parse_hotkey(k) {
            Some((mods, vk)) if RegisterHotKey(hwnd, 1, mods | MOD_NOREPEAT, vk) != 0 => true,
            Some(_) => {
                scry::log(format!("search bar: cannot have {k} ({})", std::io::Error::last_os_error()));
                false
            }
            None => {
                scry::log(format!("search bar: cannot read the hotkey {k:?} (like \"Alt+Shift+Space\")"));
                false
            }
        });
        match got {
            Some(k) => scry::log(format!("search bar on {k}")),
            None => scry::log("search bar: no hotkey; set a free one as \"hotkey\" in config.json"),
        }
        let target = hwnd as isize;
        std::thread::Builder::new().name("scry-bar-search".into()).spawn(move || worker(&work, &engine, &home, target)).expect("spawn");
        Ok(got.cloned())
    }
}

/// Run this thread's windows (the bar's, the tray's, the folders') until
/// the process ends.
pub fn pump() {
    let edit = handles().map(|(_, edit, _)| edit);
    unsafe {
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_KEYDOWN && Some(msg.hwnd) == edit && key(msg.wParam as u16) {
                continue;
            }
            // Tab, Enter and Esc in the folders window, as in a dialog.
            if let Some(w) = crate::folders::window()
                && IsDialogMessageW(w, &msg) != 0
            {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Keys the bar answers rather than the edit box. True if handled.
fn key(vk: u16) -> bool {
    let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
    match vk {
        VK_DOWN => step(1),
        VK_UP => {
            if !back_in_history() {
                step(-1)
            }
        }
        VK_NEXT => step(VISIBLE as i32),
        VK_PRIOR => step(-(VISIBLE as i32)),
        VK_RETURN => open(ctrl),
        // The selected file rather than the query's text (when one is).
        VK_C if ctrl => return copy(unsafe { GetKeyState(VK_SHIFT as i32) } < 0),
        VK_ESCAPE => hide(),
        // The edit box would type a box character.
        VK_BACK if ctrl => delete_word(),
        _ => return false,
    }
    true
}

fn handles() -> Option<(HWND, HWND, HWND)> {
    with(|u| (u.hwnd, u.edit, u.list))
}

/// Move the selection by `by` rows, staying on the list.
fn step(by: i32) {
    let Some((_, _, list)) = handles() else { return };
    let openable = with(|u| u.rows.iter().any(|r| r.path.is_some())).unwrap_or(false);
    let n = unsafe { SendMessageW(list, LB_GETCOUNT, 0, 0) } as i32;
    if !openable || n == 0 {
        return;
    }
    let cur = unsafe { SendMessageW(list, LB_GETCURSEL, 0, 0) } as i32;
    let next = (cur.max(0) + by).clamp(0, n - 1);
    unsafe { SendMessageW(list, LB_SETCURSEL, next as WPARAM, 0) };
}

/// Up in an empty box, or at the top of the list after a past search came
/// back, brings back the search before it. True if it did.
fn back_in_history() -> bool {
    let Some((_, edit, list)) = handles() else { return false };
    let text = String::from_utf16_lossy(&edit_text(edit));
    let text = text.trim();
    let at_top = unsafe { SendMessageW(list, LB_GETCURSEL, 0, 0) } <= 0;
    let Some(Some((back, query))) = with(|u| {
        let back = match u.recalled {
            Some(i) if at_top => i + 1,
            Some(_) => return None,
            None if text.is_empty() => 0,
            // The bar came back with your last search in the box.
            None if at_top => (0..).map_while(|i| u.history.get(i)).position(|q| q == text)? + 1,
            None => return None,
        };
        u.history.get(back).map(|q| (back, q.to_string()))
    }) else {
        return false;
    };
    with(|u| (u.recalled, u.recalling) = (Some(back), true));
    let w = wide(&query);
    let end = query.encode_utf16().count();
    unsafe {
        // Sends EN_CHANGE: the search runs as if typed.
        SetWindowTextW(edit, w.as_ptr());
        SendMessageW(edit, EM_SETSEL, end, end as LPARAM);
    }
    with(|u| u.recalling = false);
    true
}

/// Open the selected row (or, with `reveal`, show it selected in Explorer).
fn open(reveal: bool) {
    let Some((_, edit, list)) = handles() else { return };
    let sel = unsafe { SendMessageW(list, LB_GETCURSEL, 0, 0) };
    let Some(path) = with(|u| usize::try_from(sel).ok().and_then(|i| u.rows.get(i)).and_then(|r| r.path.clone())).flatten() else { return };
    // Remembered: it ranks higher next time, and Up brings the search back.
    remember(edit, &path);
    hide();
    let verb = wide("open");
    unsafe {
        if reveal {
            let (exe, args) = (wide("explorer.exe"), wide(&format!("/select,\"{}\"", path.display())));
            ShellExecuteW(null_mut(), verb.as_ptr(), exe.as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL);
        } else {
            let file = wide(&path.display().to_string());
            let dir = path.parent().map(|d| wide(&d.display().to_string()));
            ShellExecuteW(null_mut(), verb.as_ptr(), file.as_ptr(), null(), dir.as_ref().map_or(null(), |d| d.as_ptr()), SW_SHOWNORMAL);
        }
    }
}

/// A result you opened or copied: it ranks higher next time, and Up brings
/// the search back.
fn remember(edit: HWND, path: &Path) {
    let query = String::from_utf16_lossy(&edit_text(edit));
    with(|u| {
        u.history.push(&query);
        u.work.opened.lock().unwrap().record(path);
    });
}

/// Ctrl+C: the selected file to the clipboard, to paste into Explorer, a
/// mail or a chat. Ctrl+Shift+C: its path in quotes, as Explorer's "Copy as
/// path". False when no file is selected: the box copies its text then.
fn copy(as_path: bool) -> bool {
    let Some((hwnd, edit, list)) = handles() else { return false };
    let sel = unsafe { SendMessageW(list, LB_GETCURSEL, 0, 0) };
    let Some(path) = with(|u| usize::try_from(sel).ok().and_then(|i| u.rows.get(i)).and_then(|r| r.path.clone())).flatten() else { return false };
    let copied = if as_path {
        let text: Vec<u8> = format!("\"{}\"", path.display()).encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        to_clipboard(hwnd, CF_UNICODETEXT, &text)
    } else {
        to_clipboard(hwnd, CF_HDROP, &drop_files(&path))
    };
    if copied {
        remember(edit, &path);
        hide();
    }
    true
}

const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

/// One file as the clipboard's CF_HDROP: a DROPFILES header, then the path
/// in UTF-16, ended by two NULs.
fn drop_files(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let header = size_of::<DROPFILES>();
    let mut b = vec![0u8; header];
    b[..4].copy_from_slice(&(header as u32).to_le_bytes());
    // fWide: the names are UTF-16.
    b[header - 4..].copy_from_slice(&1u32.to_le_bytes());
    b.extend(path.as_os_str().encode_wide().chain([0, 0]).flat_map(u16::to_le_bytes));
    b
}

/// Put `data` on the clipboard as `format`, replacing what was there.
fn to_clipboard(owner: HWND, format: u32, data: &[u8]) -> bool {
    unsafe {
        if OpenClipboard(owner) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len());
        let p = GlobalLock(h);
        let ok = !p.is_null() && {
            std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast(), data.len());
            GlobalUnlock(h);
            // The clipboard owns the memory from here on.
            !SetClipboardData(format, h).is_null()
        };
        if !ok && !h.is_null() {
            GlobalFree(h);
        }
        CloseClipboard();
        ok
    }
}

/// Ctrl+Backspace: the word before the caret goes (a path segment counts
/// as a word).
fn delete_word() {
    let Some((_, edit, _)) = handles() else { return };
    let text = edit_text(edit);
    let (mut start, mut end) = (0u32, 0u32);
    unsafe { SendMessageW(edit, EM_GETSEL, (&raw mut start) as WPARAM, (&raw mut end) as LPARAM) };
    let mut i = (start as usize).min(text.len());
    let sep = |c: u16| c == b' ' as u16 || c == b'\\' as u16 || c == b'/' as u16;
    while i > 0 && sep(text[i - 1]) {
        i -= 1;
    }
    while i > 0 && !sep(text[i - 1]) {
        i -= 1;
    }
    let empty = [0u16];
    unsafe {
        SendMessageW(edit, EM_SETSEL, i, end as LPARAM);
        SendMessageW(edit, EM_REPLACESEL, 1, empty.as_ptr() as LPARAM);
    }
}

fn edit_text(edit: HWND) -> Vec<u16> {
    let n = unsafe { GetWindowTextLengthW(edit) } as usize;
    let mut buf = vec![0u16; n + 1];
    let got = unsafe { GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    buf.truncate(got);
    buf
}

/// Bring the bar up on the screen with the mouse, a quarter of the way
/// down, the last query selected (typing replaces it).
pub fn show() {
    let Some((hwnd, edit, _)) = handles() else { return };
    unsafe {
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let wa = mi.rcWork;
        let (mut dx, mut dy) = (96, 96);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let theme = system_theme();
        let Some((theme_changed, dpi_changed)) = with(|u| {
            u.anchor = ((wa.left + wa.right) / 2, wa.top + (wa.bottom - wa.top) / 4);
            let changed = (u.theme != theme, u.dpi != dx);
            if changed.0 {
                u.set_theme(theme);
            }
            if changed.1 {
                u.set_dpi(dx);
            }
            changed
        }) else {
            return;
        };
        if theme_changed {
            border(hwnd, theme);
        }
        if dpi_changed {
            apply_fonts();
        }
        place();
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(edit);
        SendMessageW(edit, EM_SETSEL, 0, -1);
        InvalidateRect(hwnd, null(), 1);
    }
}

fn hide() {
    if let Some((hwnd, _, _)) = handles() {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

/// Fonts and row height to the controls.
fn apply_fonts() {
    let Some((edit, list, query_font, row_h)) = with(|u| (u.edit, u.list, u.query_font, metrics(u.dpi).row_h)) else { return };
    unsafe {
        SendMessageW(edit, WM_SETFONT, query_font as WPARAM, 1);
        SendMessageW(list, LB_SETITEMHEIGHT, 0, row_h as LPARAM);
    }
}

/// Size and place the bar for its rows: the query on top, up to VISIBLE
/// rows under it.
fn place() {
    let Some((hwnd, edit, list, m, n, anchor)) = with(|u| (u.hwnd, u.edit, u.list, metrics(u.dpi), u.rows.len().min(VISIBLE) as i32, u.anchor))
    else {
        return;
    };
    let list_h = n * m.row_h;
    let h = m.top_h + if n > 0 { list_h + m.pad / 2 } else { 0 };
    unsafe {
        SetWindowPos(hwnd, HWND_TOPMOST, anchor.0 - m.width / 2, anchor.1, m.width, h, SWP_NOACTIVATE);
        MoveWindow(edit, m.pad + m.glyph, m.pad, m.width - 2 * m.pad - m.glyph, m.edit_h, 1);
        MoveWindow(list, 0, m.top_h, m.width, list_h, 1);
        InvalidateRect(hwnd, null(), 1);
    }
}

/// The query changed: hand it to the worker.
fn query_changed() {
    let Some((_, edit, list)) = handles() else { return };
    // Typed into: no longer a past search brought back.
    with(|u| {
        if !u.recalling {
            u.recalled = None;
        }
    });
    let text = String::from_utf16_lossy(&edit_text(edit));
    let empty = text.trim().is_empty();
    let Some(work) = with(|u| {
        u.seq += 1;
        if empty {
            u.rows.clear();
        }
        (u.seq, u.work.clone())
    }) else {
        return;
    };
    if empty {
        unsafe { SendMessageW(list, LB_SETCOUNT, 0, 0) };
        return place();
    }
    *work.1.want.lock().unwrap() = Some((work.0, text));
    work.1.wake.notify_one();
}

/// The worker answered: show its rows if they are for the current query.
fn results() {
    let Some((_, _, list)) = handles() else { return };
    let Some(work) = with(|u| u.work.clone()) else { return };
    let Some((seq, rows)) = work.done.lock().unwrap().take() else { return };
    let Some(Some((n, openable))) = with(|u| {
        (seq == u.seq).then(|| {
            u.rows = rows;
            (u.rows.len(), u.rows.iter().any(|r| r.path.is_some()))
        })
    }) else {
        return;
    };
    unsafe {
        SendMessageW(list, LB_SETCOUNT, n, 0);
        SendMessageW(list, LB_SETCURSEL, if openable { 0 } else { usize::MAX }, 0);
    }
    place();
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if unsafe { IsWindowVisible(hwnd) } != 0 {
                hide()
            } else {
                show()
            }
            0
        }
        WM_COMMAND => {
            let (id, code) = (wp & 0xFFFF, (wp >> 16) as u32);
            match (id, code) {
                (ID_EDIT, EN_CHANGE) => query_changed(),
                (ID_LIST, LBN_DBLCLK) => open(false),
                // A click picks a row; typing still goes to the query.
                (ID_LIST, LBN_SELCHANGE) => {
                    if let Some((_, edit, _)) = handles() {
                        unsafe { SetFocus(edit) };
                    }
                }
                _ => {}
            }
            0
        }
        WM_RESULTS => {
            results();
            0
        }
        WM_ACTIVATE => {
            if wp & 0xFFFF == WA_INACTIVE as usize {
                hide();
            }
            0
        }
        WM_DPICHANGED => {
            // We place ourselves; the suggested rect is for dragged windows.
            if with(|u| u.set_dpi((wp & 0xFFFF) as u32)).is_some() {
                apply_fonts();
                place();
            }
            0
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let hdc = wp as HDC;
            with(|u| unsafe {
                SetTextColor(hdc, u.theme.text);
                SetBkColor(hdc, u.theme.bg);
                u.bg as LRESULT
            })
            .unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wp, lp) })
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = unsafe { zeroed() };
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            with(|u| unsafe { paint(u, hdc) });
            unsafe { EndPaint(hwnd, &ps) };
            0
        }
        WM_DRAWITEM => {
            let dis = unsafe { &*(lp as *const DRAWITEMSTRUCT) };
            with(|u| unsafe { draw_row(u, dis) });
            1
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// The bar behind the controls: background, magnifier, the line under the
/// query.
unsafe fn paint(u: &Ui, hdc: HDC) {
    let m = metrics(u.dpi);
    let mut rc: RECT = unsafe { zeroed() };
    unsafe {
        GetClientRect(u.hwnd, &mut rc);
        FillRect(hdc, &rc, u.bg);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, u.theme.dim);
        SelectObject(hdc, u.glyph_font);
        // Segoe MDL2 Assets "Search".
        let mut glyph = [0xE721u16];
        let mut g = RECT { left: m.pad, top: m.pad, right: m.pad + m.glyph, bottom: m.pad + m.edit_h };
        DrawTextW(hdc, glyph.as_mut_ptr(), 1, &mut g, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);
        if !u.rows.is_empty() {
            let line = CreateSolidBrush(u.theme.line);
            let r = RECT { left: 0, top: m.top_h - 1, right: rc.right, bottom: m.top_h };
            FillRect(hdc, &r, line);
            DeleteObject(line);
        }
    }
}

/// One result: icon, name, and under it the folder (or the matching line).
unsafe fn draw_row(u: &Ui, dis: &DRAWITEMSTRUCT) {
    let Some(row) = u.rows.get(dis.itemID as usize) else { return };
    let (hdc, rc, m) = (dis.hDC, dis.rcItem, metrics(u.dpi));
    let selected = dis.itemState & ODS_SELECTED != 0 && row.path.is_some();
    unsafe {
        FillRect(hdc, &rc, if selected { u.sel } else { u.bg });
        SetBkMode(hdc, TRANSPARENT as i32);
        let x = rc.left + m.pad + m.glyph / 2 - u.icon_px / 2;
        if row.icon >= 0 {
            ImageList_Draw(u.images, row.icon, hdc, x, rc.top + (m.row_h - u.icon_px) / 2, ILD_TRANSPARENT);
        }
        let left = rc.left + m.pad + m.glyph;
        let mid = rc.top + m.row_h / 2;
        let text = |s: &str, font: HFONT, color: u32, mut r: RECT, flags: DRAW_TEXT_FORMAT| {
            let mut w: Vec<u16> = s.encode_utf16().collect();
            SelectObject(hdc, font);
            SetTextColor(hdc, color);
            DrawTextW(hdc, w.as_mut_ptr(), w.len() as i32, &mut r, flags | DT_SINGLELINE | DT_NOPREFIX);
        };
        let right = rc.right - m.pad;
        if row.path.is_none() {
            // A message: "No results", "indexing...".
            text(&row.name, u.name_font, u.theme.dim, RECT { left, top: rc.top, right, bottom: rc.bottom }, DT_VCENTER | DT_END_ELLIPSIS);
            return;
        }
        text(&row.name, u.name_font, u.theme.text, RECT { left, top: rc.top, right, bottom: mid + 1 }, DT_BOTTOM | DT_END_ELLIPSIS);
        let fit = if row.detail.contains("  ·  ") { DT_END_ELLIPSIS } else { DT_PATH_ELLIPSIS };
        text(&row.detail, u.detail_font, u.theme.dim, RECT { left, top: mid + 2, right, bottom: rc.bottom }, DT_TOP | fit);
    }
}

/// Runs the queries the bar hands it, latest first, and posts the rows back.
fn worker(work: &Work, engine: &Engine, home: &str, target: isize) {
    // Shell calls (icons) want COM.
    unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as _) };
    let mut icons = Icons::default();
    loop {
        let (seq, text) = {
            let mut want = work.want.lock().unwrap();
            loop {
                if let Some(w) = want.take() {
                    break w;
                }
                want = work.wake.wait(want).unwrap();
            }
        };
        let rows = search(engine, home, &text, &work.opened, &mut icons);
        *work.done.lock().unwrap() = Some((seq, rows));
        unsafe { PostMessageW(target as HWND, WM_RESULTS, 0, 0) };
    }
}

/// Shortest text looked for inside files when no name matches: below a
/// trigram, the content index can't narrow anything down.
const FALLBACK_MIN: usize = 3;

fn search(engine: &Engine, home: &str, text: &str, opened: &Mutex<Opened>, icons: &mut Icons) -> Vec<Row> {
    let message = |m: String| vec![Row { path: None, name: m, detail: String::new(), icon: -1 }];
    let mut q = match Query::parse(text, home) {
        Ok(q) => q,
        Err(e) => return message(e),
    };
    q.limit = LIMIT;
    let rows = match q.grep.take() {
        Some(pattern) => content(engine, &q, &pattern, q.grep_mode, icons),
        None => match engine.search(&Query { limit: LIMIT * 4, ..q.clone() }) {
            Ok(found) if found.is_empty() => {
                // No file is called that: look for it inside files (the
                // filters still choose which), as if typed with grep:.
                let words = Query::plain_words(text, home);
                let words = words.trim();
                if words.chars().count() < FALLBACK_MIN {
                    return message("No results".into());
                }
                q.tokens.clear();
                content(engine, &q, words, GrepMode::Literal, icons)
            }
            Ok(found) => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                // Not held while icons are looked up: opening waits on it.
                let ranked = rerank(found, &opened.lock().unwrap(), now);
                Ok(ranked
                    .into_iter()
                    .map(|f| {
                        let dir = f.kind & 3 == walk::KIND_DIR;
                        Row { name: name_of(&f.path), detail: parent_of(&f.path), icon: icons.of(&f.path, dir), path: Some(f.path) }
                    })
                    .collect())
            }
            Err(e) => Err(e),
        },
    };
    match rows {
        Ok(rows) if rows.is_empty() => message("No results".into()),
        Ok(rows) => rows,
        Err(e) => message(e),
    }
}

/// What you opened before ranks higher (`Opened::boost`); the rest keep
/// their order. The best LIMIT.
fn rerank(mut found: Vec<Found>, opened: &Opened, now: u64) -> Vec<Found> {
    found.sort_by_cached_key(|f| std::cmp::Reverse(f.score + opened.boost(&f.path, now)));
    found.truncate(LIMIT);
    found
}

/// Files containing `pattern`, among those `q` picks: a row per file, with
/// its folder and the first matching line.
fn content(engine: &Engine, q: &Query, pattern: &str, mode: GrepMode, icons: &mut Icons) -> Result<Vec<Row>, String> {
    let (r, _) = Grep::new(pattern, mode).and_then(|g| engine.grep(q, &g))?;
    Ok(r.files
        .iter()
        .map(|f| {
            let path = paths::to_native(&f.path);
            let (n, line) = f.lines.first().map_or((0, ""), |(n, l)| (*n, l.trim()));
            Row { name: name_of(&path), detail: format!("{}  ·  {n}: {line}", parent_of(&path)), icon: icons.of(&path, false), path: Some(path) }
        })
        .collect())
}

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn parent_of(p: &Path) -> String {
    p.parent().map_or_else(String::new, |d| d.display().to_string())
}

/// System image list indices, looked up once per kind of file. Programs
/// and shortcuts get their own icon, read from the file; anything else the
/// icon of its type, which needs no disk access.
#[derive(Default)]
struct Icons(HashMap<String, i32>);

impl Icons {
    fn of(&mut self, path: &Path, dir: bool) -> i32 {
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let own = !dir && matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url" | "appref-ms");
        let key = match (own, dir) {
            (true, _) => path.display().to_string(),
            (_, true) => "<folder>".into(),
            _ => format!(".{ext}"),
        };
        if let Some(&i) = self.0.get(&key) {
            return i;
        }
        let probe = wide(&if own || dir { path.display().to_string() } else { format!("x.{ext}") });
        let mut sfi: SHFILEINFOW = unsafe { zeroed() };
        let flags = SHGFI_SYSICONINDEX | SHGFI_LARGEICON | if own { 0 } else { SHGFI_USEFILEATTRIBUTES };
        let attrs = if dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
        let ok = unsafe { SHGetFileInfoW(probe.as_ptr(), attrs, &mut sfi, size_of::<SHFILEINFOW>() as u32, flags) } != 0;
        let i = if ok { sfi.iIcon } else { -1 };
        self.0.insert(key, i);
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_files_rank_higher() {
        let dir = std::env::temp_dir().join(format!("scry-rerank-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut opened = Opened::load(&dir);
        let f = |name: &str, score| Found { path: Path::new(r"C:\probe").join(name), kind: 0, size: 0, mtime: 0, score };
        let names = |v: Vec<Found>| v.iter().map(|f| name_of(&f.path)).collect::<Vec<_>>();
        let found = || vec![f("exact.txt", 400), f("alpha.txt", 100), f("beta.txt", 100), f("gamma.txt", 100)];
        assert_eq!(names(rerank(found(), &opened, 0)), ["exact.txt", "alpha.txt", "beta.txt", "gamma.txt"]);
        for _ in 0..5 {
            opened.record(Path::new(r"C:\probe\gamma.txt"));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(names(rerank(found(), &opened, now)), ["exact.txt", "gamma.txt", "alpha.txt", "beta.txt"], "above equals, not above a far better match");
        let many: Vec<Found> = (0..300).map(|i| f(&format!("{i}.txt"), 1000 - i)).collect();
        assert_eq!(rerank(many, &opened, now).len(), LIMIT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_for_the_clipboard() {
        let b = drop_files(Path::new(r"C:\Docs\é.pdf"));
        assert_eq!(&b[..4], &20u32.to_le_bytes(), "the names start after the 20-byte header");
        assert_eq!(&b[16..20], &1u32.to_le_bytes(), "in UTF-16");
        let names: Vec<u16> = b[20..].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        let want: Vec<u16> = r"C:\Docs\é.pdf".encode_utf16().chain([0, 0]).collect();
        assert_eq!(names, want, "one name, then the empty one that ends the list");
    }

    #[test]
    fn hotkeys_parse() {
        assert_eq!(parse_hotkey("Ctrl+Alt+Space"), Some((MOD_CONTROL | MOD_ALT, VK_SPACE as u32)));
        assert_eq!(parse_hotkey("win + shift + f"), Some((MOD_WIN | MOD_SHIFT, b'F' as u32)));
        assert_eq!(parse_hotkey("Alt+F12"), Some((MOD_ALT, VK_F12 as u32)));
        assert_eq!(parse_hotkey("Ctrl+Alt"), None);
        assert_eq!(parse_hotkey("Ctrl+Hyper+K"), None);
    }
}
