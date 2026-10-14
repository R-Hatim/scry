//! The Folders window, from the tray menu: the folders Scry reads the text
//! files of (your folders: search inside files, and they rank first), and
//! the folders it leaves out entirely. Every drive is searched by name
//! either way. Save writes config.json and restarts the daemon, which takes
//! the lists from there; folders taken off the left-out list are listed
//! whole once it is back.

use crate::tray;
use scry::{paths, server};
use std::cell::RefCell;
use std::ffi::{OsString, c_void};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{SS_LEFT, SS_NOPREFIX};
use windows_sys::Win32::UI::Controls::{LIM_LARGE, LIM_SMALL, LoadIconMetric};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR};

const ID_ROOTS: i32 = 10;
const ID_ROOT_ADD: i32 = 11;
const ID_ROOT_REMOVE: i32 = 12;
const ID_SKIP: i32 = 20;
const ID_SKIP_ADD: i32 = 21;
const ID_SKIP_REMOVE: i32 = 22;
/// Notes (dim text) are 30 and up, headings 40 and up.
const ID_NOTE: i32 = 30;
const ID_HEADING: i32 = 40;

/// The client area at 96 DPI, and its margin and button size.
const WIDTH: i32 = 540;
const HEIGHT: i32 = 420;
const M: i32 = 16;
const BW: i32 = 96;
const BH: i32 = 28;
const LIST_W: i32 = WIDTH - 2 * M - BW - 8;
const BX: i32 = WIDTH - M - BW;

/// A control: class, text, id, style, and where it goes at 96 DPI.
struct Spec(&'static str, &'static str, i32, u32, (i32, i32, i32, i32));

const LIST: u32 = WS_TABSTOP | WS_VSCROLL | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32;
const TEXT: u32 = SS_LEFT | SS_NOPREFIX;
const BUTTON: u32 = WS_TABSTOP | BS_PUSHBUTTON as u32;

const SPECS: &[Spec] = &[
    Spec("STATIC", "Search inside files in these folders", ID_HEADING, TEXT, (M, 16, WIDTH - 2 * M, 20)),
    Spec(
        "STATIC",
        "Scry reads their text files so you can search what is in them, and what is in them ranks first. Every drive is still searched by file name.",
        ID_NOTE,
        TEXT,
        (M, 38, WIDTH - 2 * M, 34),
    ),
    Spec("LISTBOX", "", ID_ROOTS, LIST, (M, 78, LIST_W, 120)),
    Spec("BUTTON", "Add…", ID_ROOT_ADD, BUTTON, (BX, 78, BW, BH)),
    Spec("BUTTON", "Remove", ID_ROOT_REMOVE, BUTTON, (BX, 112, BW, BH)),
    Spec("STATIC", "Leave these folders out", ID_HEADING + 1, TEXT, (M, 216, WIDTH - 2 * M, 20)),
    Spec("STATIC", "Nothing in them is indexed, by name or by content.", ID_NOTE + 1, TEXT, (M, 238, WIDTH - 2 * M, 18)),
    Spec("LISTBOX", "", ID_SKIP, LIST, (M, 262, LIST_W, 96)),
    Spec("BUTTON", "Add…", ID_SKIP_ADD, BUTTON, (BX, 262, BW, BH)),
    Spec("BUTTON", "Remove", ID_SKIP_REMOVE, BUTTON, (BX, 296, BW, BH)),
    Spec("STATIC", "Scry restarts to apply changes.", ID_NOTE + 2, TEXT, (M, 382, 280, 18)),
    Spec("BUTTON", "Save", IDOK, WS_TABSTOP | BS_DEFPUSHBUTTON as u32, (WIDTH - M - 2 * BW - 8, 376, BW, BH)),
    Spec("BUTTON", "Cancel", IDCANCEL, BUTTON, (BX, 376, BW, BH)),
];

const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

struct Win {
    hwnd: HWND,
    dir: PathBuf,
    home: String,
    roots: Vec<PathBuf>,
    skip: Vec<PathBuf>,
    /// The lists as they were: Save acts on what changed.
    was: (Vec<PathBuf>, Vec<PathBuf>),
    font: HFONT,
    bold: HFONT,
}

thread_local! {
    static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
}

/// The state, briefly; never send a window message from inside `f`.
fn with<R>(f: impl FnOnce(&mut Win) -> R) -> Option<R> {
    WIN.with_borrow_mut(|w| w.as_mut().map(f))
}

/// The window, while it is open (for `IsDialogMessage`: Tab, Enter, Esc).
pub fn window() -> Option<HWND> {
    with(|w| w.hwnd)
}

/// Windows paths ignore case.
fn same(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

fn px(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

/// The window's outer size for the client area at `dpi`.
fn outer(dpi: u32) -> (i32, i32) {
    let mut rc = RECT { left: 0, top: 0, right: px(WIDTH, dpi), bottom: px(HEIGHT, dpi) };
    unsafe { AdjustWindowRectExForDpi(&mut rc, STYLE, 0, 0, dpi) };
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// Open the window (or bring it forward), with the lists from config.json.
pub fn open(dir: PathBuf, home: String) {
    if let Some(h) = window() {
        unsafe {
            ShowWindow(h, SW_RESTORE);
            SetForegroundWindow(h);
        }
        return;
    }
    let (roots, skip) = server::folders(&dir, &home);
    unsafe {
        let inst = GetModuleHandleW(null());
        let class = wide("scry-folders");
        let (mut icon, mut small) = (null_mut(), null_mut());
        LoadIconMetric(inst, 1 as _, LIM_LARGE, &mut icon);
        LoadIconMetric(inst, 1 as _, LIM_SMALL, &mut small);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            hIcon: icon,
            hIconSm: small,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        // Fails harmlessly the second time.
        RegisterClassExW(&wc);
        // In the middle of the screen with the mouse.
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dpi, mut dy) = (96, 96);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut dy);
        let (w, h) = outer(dpi);
        let wa = mi.rcWork;
        let title = wide("Scry folders");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            STYLE,
            (wa.left + wa.right - w) / 2,
            (wa.top + wa.bottom - h) / 2,
            w,
            h,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if hwnd.is_null() {
            return scry::log(format!("folders: no window ({})", std::io::Error::last_os_error()));
        }
        for Spec(class, text, id, style, _) in SPECS {
            let ex = if *class == "LISTBOX" { WS_EX_CLIENTEDGE } else { 0 };
            CreateWindowExW(
                ex,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                hwnd,
                *id as isize as HMENU,
                inst,
                null(),
            );
        }
        let was = (roots.clone(), skip.clone());
        WIN.with_borrow_mut(|w| *w = Some(Win { hwnd, dir, home, roots, skip, was, font: null_mut(), bold: null_mut() }));
        set_dpi(hwnd, GetDpiForWindow(hwnd));
        fill();
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

/// Fonts (the system's message font) and places for `dpi`.
fn set_dpi(hwnd: HWND, dpi: u32) {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, (&raw mut ncm).cast(), 0, dpi);
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        ncm.lfMessageFont.lfWeight = FW_SEMIBOLD as i32;
        let bold = CreateFontIndirectW(&ncm.lfMessageFont);
        let old = with(|w| (std::mem::replace(&mut w.font, font), std::mem::replace(&mut w.bold, bold)));
        for Spec(_, _, id, _, (x, y, w, h)) in SPECS {
            let c = GetDlgItem(hwnd, *id);
            MoveWindow(c, px(*x, dpi), px(*y, dpi), px(*w, dpi), px(*h, dpi), 1);
            let f = if (ID_HEADING..ID_HEADING + 10).contains(id) { bold } else { font };
            SendMessageW(c, WM_SETFONT, f as WPARAM, 1);
        }
        if let Some((f, b)) = old {
            DeleteObject(f);
            DeleteObject(b);
        }
    }
}

/// Both lists from the state, and the Remove buttons only with a selection.
fn fill() {
    let Some((hwnd, roots, skip)) = with(|w| (w.hwnd, w.roots.clone(), w.skip.clone())) else { return };
    for (id, list) in [(ID_ROOTS, roots), (ID_SKIP, skip)] {
        unsafe {
            let c = GetDlgItem(hwnd, id);
            SendMessageW(c, LB_RESETCONTENT, 0, 0);
            for p in list {
                SendMessageW(c, LB_ADDSTRING, 0, wide(&p.display().to_string()).as_ptr() as LPARAM);
            }
        }
    }
    buttons(hwnd);
}

fn buttons(hwnd: HWND) {
    for (list, remove) in [(ID_ROOTS, ID_ROOT_REMOVE), (ID_SKIP, ID_SKIP_REMOVE)] {
        unsafe {
            let sel = SendMessageW(GetDlgItem(hwnd, list), LB_GETCURSEL, 0, 0);
            EnableWindow(GetDlgItem(hwnd, remove), (sel >= 0) as i32);
        }
    }
}

fn message(hwnd: HWND, text: &str) {
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide("Scry").as_ptr(), MB_OK | MB_ICONWARNING) };
}

/// Add…: a folder from the system's picker, to your folders or the left-out ones.
fn add(roots: bool) {
    let Some(hwnd) = window() else { return };
    let title = if roots { "Add a folder to search inside" } else { "Add a folder to leave out" };
    let Some(p) = pick_folder(hwnd, title) else { return };
    if paths::from_native(&p).is_none() {
        return message(hwnd, &format!("{} is not on a drive of this PC; only those can be indexed.", p.display()));
    }
    with(|w| {
        let list = if roots { &mut w.roots } else { &mut w.skip };
        if !list.iter().any(|q| same(q, &p)) {
            list.push(p);
            list.sort();
        }
    });
    fill();
}

/// Remove: the selected folder goes from its list.
fn remove(roots: bool) {
    let Some(hwnd) = window() else { return };
    let c = unsafe { GetDlgItem(hwnd, if roots { ID_ROOTS } else { ID_SKIP }) };
    let sel = unsafe { SendMessageW(c, LB_GETCURSEL, 0, 0) };
    let Ok(i) = usize::try_from(sel) else { return };
    let left = with(|w| {
        let list = if roots { &mut w.roots } else { &mut w.skip };
        if i < list.len() {
            list.remove(i);
        }
        list.len()
    });
    fill();
    // Keep a selection where the removed one was, for removing several.
    if let Some(n) = left.filter(|&n| n > 0) {
        unsafe { SendMessageW(c, LB_SETCURSEL, i.min(n - 1), 0) };
        buttons(hwnd);
    }
}

/// Save: config.json, then a restart that picks it up. Nothing changed:
/// just close.
fn save() {
    let Some((hwnd, dir, home, roots, skip, was)) = with(|w| (w.hwnd, w.dir.clone(), w.home.clone(), w.roots.clone(), w.skip.clone(), w.was.clone()))
    else {
        return;
    };
    if (&roots, &skip) == (&was.0, &was.1) {
        unsafe { DestroyWindow(hwnd) };
        return;
    }
    if let Err(e) = server::save_folders(&dir, &home, &roots, &skip) {
        return message(hwnd, &format!("Could not save the folders: {e}"));
    }
    let list = |ps: &[PathBuf]| ps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("; ");
    scry::log(format!("folders: yours {}; left out {}", list(&roots), list(&skip)));
    // Left out until now: the index never saw what is in them.
    let rescan: Vec<PathBuf> = was.1.into_iter().filter(|p| !skip.iter().any(|q| same(p, q))).collect();
    unsafe { DestroyWindow(hwnd) };
    tray::restart(&rescan);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let (id, code) = ((wp & 0xFFFF) as i32, (wp >> 16) as u32);
            match (id, code) {
                (ID_ROOT_ADD, BN_CLICKED) => add(true),
                (ID_SKIP_ADD, BN_CLICKED) => add(false),
                (ID_ROOT_REMOVE, BN_CLICKED) => remove(true),
                (ID_SKIP_REMOVE, BN_CLICKED) => remove(false),
                (ID_ROOTS | ID_SKIP, LBN_SELCHANGE) => buttons(hwnd),
                (IDOK, _) => save(),
                (IDCANCEL, _) => unsafe {
                    DestroyWindow(hwnd);
                },
                _ => {}
            }
            0
        }
        WM_CTLCOLORSTATIC => {
            // Labels on the window's background; the notes dimmer.
            let hdc = wp as HDC;
            let id = unsafe { GetDlgCtrlID(lp as HWND) };
            unsafe {
                SetBkMode(hdc, TRANSPARENT as i32);
                SetTextColor(hdc, GetSysColor(if (ID_NOTE..ID_NOTE + 10).contains(&id) { COLOR_GRAYTEXT } else { COLOR_WINDOWTEXT }));
                GetSysColorBrush(COLOR_WINDOW) as LRESULT
            }
        }
        WM_DPICHANGED => {
            // Keep the suggested place; the size is ours.
            let r = unsafe { &*(lp as *const RECT) };
            let dpi = (wp & 0xFFFF) as u32;
            set_dpi(hwnd, dpi);
            let (w, h) = outer(dpi);
            unsafe { SetWindowPos(hwnd, null_mut(), r.left, r.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
            0
        }
        WM_DESTROY => {
            if let Some(w) = WIN.with_borrow_mut(Option::take) {
                unsafe {
                    DeleteObject(w.font);
                    DeleteObject(w.bold);
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// A COM object, released on drop, whose methods are called by their place
/// in its vtable (windows-sys has no COM interfaces).
struct Com(*mut c_void);

impl Com {
    /// The vtable's `i`th method, as `F` (an `extern "system" fn` taking the
    /// object first).
    unsafe fn method<F: Copy>(&self, i: usize) -> F {
        unsafe {
            let vtbl = *(self.0 as *const *const usize);
            std::mem::transmute_copy(&*vtbl.add(i))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { self.method::<unsafe extern "system" fn(*mut c_void) -> u32>(2)(self.0) };
    }
}

const CLSID_FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xdc1c5a9c_e88a_4dde_a5a1_60f82a20aef7);
const IID_IFILE_OPEN_DIALOG: GUID = GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
const FOS_PICKFOLDERS: u32 = 0x20;
const FOS_FORCEFILESYSTEM: u32 = 0x40;
const FOS_PATHMUSTEXIST: u32 = 0x800;
const SIGDN_FILESYSPATH: i32 = 0x8005_8000_u32 as i32;

/// The system's folder picker (IFileOpenDialog); None if cancelled.
pub fn pick_folder(owner: HWND, title: &str) -> Option<PathBuf> {
    // IFileDialog: Show 3, SetOptions 9, GetOptions 10, SetTitle 17,
    // GetResult 20. IShellItem: GetDisplayName 5.
    type Show = unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT;
    type SetOptions = unsafe extern "system" fn(*mut c_void, u32) -> HRESULT;
    type GetOptions = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;
    type SetTitle = unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT;
    type GetResult = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
    type GetDisplayName = unsafe extern "system" fn(*mut c_void, i32, *mut PWSTR) -> HRESULT;
    unsafe {
        let mut obj = null_mut();
        if CoCreateInstance(&CLSID_FILE_OPEN_DIALOG, null_mut(), CLSCTX_INPROC_SERVER, &IID_IFILE_OPEN_DIALOG, &mut obj) < 0 {
            return None;
        }
        let dlg = Com(obj);
        let mut opts = 0;
        dlg.method::<GetOptions>(10)(dlg.0, &mut opts);
        dlg.method::<SetOptions>(9)(dlg.0, opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST);
        let t = wide(title);
        dlg.method::<SetTitle>(17)(dlg.0, t.as_ptr());
        if dlg.method::<Show>(3)(dlg.0, owner) < 0 {
            return None;
        }
        let mut obj = null_mut();
        if dlg.method::<GetResult>(20)(dlg.0, &mut obj) < 0 {
            return None;
        }
        let item = Com(obj);
        let mut name: PWSTR = null_mut();
        if item.method::<GetDisplayName>(5)(item.0, SIGDN_FILESYSPATH, &mut name) < 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(name, len)));
        CoTaskMemFree(name.cast());
        Some(path)
    }
}
