//! The icon in the notification area. Its tooltip says how far the index
//! is; a click brings up the search bar; its menu has the search, the
//! counts, the folders window, start at login and quit. After a first crawl
//! (the daemon started with nothing to search yet), a notification says it
//! is ready.
//!
//! The counts come from a thread of their own, since a writer may hold the
//! engine's locks for a while; the icon lives on the bar's thread.

use crate::{bar, folders};
use scry::{Engine, server};
use std::cell::RefCell;
use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{LIM_LARGE, LIM_SMALL, LoadIconMetric};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Explorer has news about the icon: a click, the menu.
const WM_TRAY: u32 = WM_APP + 10;
/// The counts changed.
const WM_STATUS: u32 = WM_APP + 11;
const ID_SEARCH: usize = 1;
const ID_LOGIN: usize = 2;
const ID_QUIT: usize = 3;
const ID_FOLDERS: usize = 4;
/// NIN_SELECT from the keyboard (Enter or Space on the focused icon).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
/// Indexing that takes longer than this is a crawl worth announcing the end
/// of (loading a saved index takes a fraction of it).
const CRAWL: Duration = Duration::from_secs(5);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// What the icon tells, as the status thread last saw it.
#[derive(Clone, Copy, PartialEq, Default)]
struct Counts {
    ready: bool,
    /// Files and folders.
    entries: usize,
    /// Files whose content is indexed, and those still to read.
    docs: usize,
    pending: usize,
}

struct Tray {
    hwnd: HWND,
    /// For a checkpoint before the process ends.
    engine: Engine,
    /// The index dir (config.json, the log) and your user folder.
    dir: PathBuf,
    home: String,
    small: HICON,
    large: HICON,
    hotkey: Option<String>,
    counts: Counts,
    latest: Arc<Mutex<Counts>>,
    started: Instant,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Tray) -> R) -> Option<R> {
    TRAY.with_borrow_mut(|t| t.as_mut().map(f))
}

/// The icon's window, for `remove` from the pipe's thread.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Sent to every top-level window when Explorer (re)starts: the icon has
/// to be added again.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Put the icon up, on this thread (the bar's: its messages come through
/// `bar::pump`). `hotkey` is the one the bar got.
pub fn create(engine: Engine, hotkey: Option<String>, dir: PathBuf) {
    unsafe {
        let inst = GetModuleHandleW(null());
        let class = wide("scry-tray");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassExW(&wc);
        // Never shown, but a top-level window: the menu needs one in the
        // foreground to close when you click elsewhere.
        let hwnd =
            CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), wide("Scry").as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), inst, null());
        if hwnd.is_null() {
            return scry::log(format!("tray: no window ({})", std::io::Error::last_os_error()));
        }
        let created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        TASKBAR_CREATED.store(created, Ordering::Relaxed);
        // An elevated daemon still hears from Explorer.
        for m in [created, WM_TRAY] {
            ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, null_mut());
        }
        let (mut small, mut large) = (null_mut(), null_mut());
        // The program's icon (build.rs), at the sizes the shell uses.
        LoadIconMetric(inst, 1 as _, LIM_SMALL, &mut small);
        LoadIconMetric(inst, 1 as _, LIM_LARGE, &mut large);
        let latest = Arc::new(Mutex::new(Counts::default()));
        let home = engine.home().to_string();
        let tray = Tray {
            hwnd,
            engine: engine.clone(),
            dir,
            home,
            small,
            large,
            hotkey,
            counts: Counts::default(),
            latest: latest.clone(),
            started: Instant::now(),
        };
        TRAY.with_borrow_mut(|t| *t = Some(tray));
        WINDOW.store(hwnd as isize, Ordering::Relaxed);
        add();
        server::on_quit(remove);
        let target = hwnd as isize;
        std::thread::Builder::new().name("scry-tray".into()).spawn(move || watch(&engine, &latest, target)).expect("spawn");
    }
}

/// Polls the engine, every second while it indexes and every few once it
/// is done, and tells the icon when the counts change.
fn watch(engine: &Engine, latest: &Mutex<Counts>, target: isize) {
    let mut last = None;
    loop {
        let s = engine.status();
        let c =
            Counts { ready: s.ready, entries: (s.entries + s.overlay).saturating_sub(s.removed), docs: s.content_docs, pending: s.content_pending };
        if last != Some(c) {
            *latest.lock().unwrap() = c;
            unsafe { PostMessageW(target as HWND, WM_STATUS, 0, 0) };
            last = Some(c);
        }
        let busy = !c.ready || c.pending > 0;
        std::thread::sleep(Duration::from_secs(if busy { 1 } else { 5 }));
    }
}

/// 5041233 as "5,041,233".
fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What the index holds, or that it is being built.
fn index_line(c: Counts) -> String {
    if c.ready { format!("{} files and folders", thousands(c.entries)) } else { "Indexing your drives (the first time, about a minute)".into() }
}

/// What content search covers, while it is being read.
fn reading_line(c: Counts) -> Option<String> {
    (c.ready && c.pending > 0).then(|| format!("Reading {} files for content search", thousands(c.pending)))
}

const NO_HOTKEY: &str = "No hotkey: set one in config.json";

/// The tooltip: the name, the index, then what is going on or what to press.
fn tip(t: &Tray) -> String {
    let c = t.counts;
    let last = reading_line(c).or_else(|| c.ready.then(|| t.hotkey.as_ref().map_or(NO_HOTKEY.into(), |k| format!("{k} to search"))));
    let mut s = format!("Scry\n{}", index_line(c));
    if let Some(l) = last {
        s.push('\n');
        s.push_str(&l);
    }
    s
}

/// The fixed-size text fields of NOTIFYICONDATAW, cut to fit.
fn copy(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut d: NOTIFYICONDATAW = unsafe { zeroed() };
    d.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = 1;
    d
}

fn add() {
    let Some((hwnd, icon, tip)) = with(|t| (t.hwnd, t.small, tip(t))) else { return };
    let mut d = icon_data(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = icon;
    copy(&mut d.szTip, &tip);
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, &d) == 0 {
            // No taskbar yet (early at login): TaskbarCreated comes when it is.
            return scry::log("tray: no icon yet, waiting for the taskbar");
        }
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &d);
    }
}

/// Take the icon down (any thread): before the process ends, or it lingers
/// until the mouse passes over it.
fn remove() {
    let hwnd = WINDOW.load(Ordering::Relaxed) as HWND;
    if !hwnd.is_null() {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd)) };
    }
}

/// New counts: the tooltip, and the notification after a first crawl.
fn refresh() {
    let Some((hwnd, tip, ready)) = with(|t| {
        let c = *t.latest.lock().unwrap();
        let crawled = c.ready && !t.counts.ready && t.started.elapsed() > CRAWL;
        t.counts = c;
        let ready = crawled.then(|| {
            let how = t.hotkey.as_ref().map_or("Click the icon".into(), |k| format!("Press {k}"));
            (format!("{} files and folders indexed. {how} to search.", thousands(c.entries)), t.large)
        });
        (t.hwnd, tip(t), ready)
    }) else {
        return;
    };
    let mut d = icon_data(hwnd);
    d.uFlags = NIF_TIP | NIF_SHOWTIP;
    copy(&mut d.szTip, &tip);
    if let Some((text, large)) = ready {
        d.uFlags |= NIF_INFO;
        copy(&mut d.szInfoTitle, "Scry is ready");
        copy(&mut d.szInfo, &text);
        d.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        d.hBalloonIcon = large;
    }
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
}

/// The menu, at `(x, y)` on the screen.
fn menu(hwnd: HWND, x: i32, y: i32) {
    let Some((lines, hotkey)) = with(|t| {
        let c = t.counts;
        let mut lines = vec![index_line(c)];
        lines.extend(reading_line(c).or_else(|| (c.docs > 0).then(|| format!("Content search in {} files", thousands(c.docs)))));
        if t.hotkey.is_none() {
            lines.push(NO_HOTKEY.into());
        }
        (lines, t.hotkey.clone())
    }) else {
        return;
    };
    let login = server::has_login_item();
    let cmd = unsafe {
        let m = CreatePopupMenu();
        let search = hotkey.map_or("Search".into(), |k| format!("Search\t{k}"));
        AppendMenuW(m, MF_STRING, ID_SEARCH, wide(&search).as_ptr());
        SetMenuDefaultItem(m, ID_SEARCH as u32, 0);
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        for l in &lines {
            AppendMenuW(m, MF_STRING | MF_GRAYED, 0, wide(l).as_ptr());
        }
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, ID_FOLDERS, wide("Folders…").as_ptr());
        AppendMenuW(m, MF_STRING | if login { MF_CHECKED } else { MF_UNCHECKED }, ID_LOGIN, wide("Start at login").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, ID_QUIT, wide("Quit Scry").as_ptr());
        // Without this the menu stays up when you click elsewhere.
        SetForegroundWindow(hwnd);
        let side = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 { TPM_RIGHTALIGN } else { TPM_LEFTALIGN };
        let cmd = TrackPopupMenuEx(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | side, x, y, hwnd, null());
        DestroyMenu(m);
        cmd as usize
    };
    command(cmd);
}

/// What a menu item does (also as WM_COMMAND, so it can be driven from
/// outside).
fn command(id: usize) {
    match id {
        ID_SEARCH => bar::show(),
        ID_FOLDERS => {
            if let Some((dir, home)) = with(|t| (t.dir.clone(), t.home.clone())) {
                folders::open(dir, home);
            }
        }
        ID_LOGIN => {
            let ok = if server::has_login_item() {
                server::login_item(None)
            } else {
                std::env::current_exe().is_ok_and(|exe| server::login_item(Some(&exe)))
            };
            if !ok {
                scry::log("tray: could not change the login item");
            }
        }
        ID_QUIT => {
            scry::log("quit from the tray");
            checkpoint();
            remove();
            std::process::exit(0);
        }
        _ => {}
    }
}

/// Save what changed since the last full save, so the next start (after a
/// quit, a restart, signing out) resumes from here.
fn checkpoint() {
    if let Some(engine) = with(|t| t.engine.clone()) {
        engine.checkpoint();
    }
}

/// Hand over to a new daemon, which reads config.json afresh, and leave:
/// it waits for this one to be gone. `rescan`: folders it should list whole.
pub fn restart(rescan: &[PathBuf]) {
    let Some(dir) = with(|t| t.dir.clone()) else { return };
    // The new daemon resumes from here.
    checkpoint();
    let args: Vec<OsString> = std::iter::once("--restart".into()).chain(rescan.iter().flat_map(|d| ["--rescan".into(), d.into()])).collect();
    match std::env::current_exe().and_then(|exe| server::start_daemon(&dir, &exe, &args)) {
        Ok(()) => {
            scry::log("restarting for the new folders");
            remove();
            std::process::exit(0);
        }
        Err(e) => scry::log(format!("tray: cannot restart ({e}); the folders apply at the next start")),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            // Version 4: the event in the low word of lp, where it happened
            // (on the screen) in wp.
            let (x, y) = ((wp & 0xFFFF) as i16 as i32, ((wp >> 16) & 0xFFFF) as i16 as i32);
            match (lp & 0xFFFF) as u32 {
                NIN_SELECT | NIN_KEYSELECT => bar::show(),
                WM_CONTEXTMENU => menu(hwnd, x, y),
                _ => {}
            }
            0
        }
        WM_STATUS => {
            refresh();
            0
        }
        WM_COMMAND => {
            command(wp & 0xFFFF);
            0
        }
        // Signing out or shutting down: Windows ends the process right after.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wp != 0 {
                checkpoint();
            }
            0
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            add();
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_well() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(5_041_233), "5,041,233");
    }
}
