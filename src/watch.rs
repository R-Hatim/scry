//! Live changes: ReadDirectoryChangesW on the root of every fixed drive,
//! whole subtree. A change is reported as the folder it happened in, which
//! is all the live index needs: it relists that folder and diffs.
//!
//! These notifications have no history to replay, so a stream started for
//! an older index first asks for "/" to be resynced, and so does a
//! notification buffer that overflowed before we read it. The engine then
//! asks the NTFS change journal (`journal_dirs`) which folders changed
//! since its last known-good second, and checks folder by folder where
//! there is no journal.

use crate::{paths, sys};
use rayon::prelude::*;
use std::collections::HashSet;
use std::mem::{offset_of, size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::*;

/// The whole subtree may have changed (only ever sent for "/").
pub const MUST_SCAN_SUBDIRS: u32 = 0x1;
/// Changes were lost: more happened than the buffer held.
pub const DROPPED: u32 = 0x4;
/// Everything before this was catch-up; live changes follow.
pub const HISTORY_DONE: u32 = 0x10;

pub struct Event {
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
}

/// Event ids are wall-clock FILETIME ticks: they order events across
/// processes, which is what a follower checks a saved index against.
pub fn current_event_id() -> u64 {
    sys::filetime_now()
}

/// The running watches; dropping it stops them.
pub struct Stream {
    watches: Vec<Arc<Watch>>,
    /// The drives watched (`sys::fixed_drives` when it started).
    pub drives: Vec<u8>,
}

/// One drive's watch: (stopping, handle). The handle lives under the lock,
/// so cancelling its pending read never races the thread closing it.
struct Watch(Mutex<(bool, Option<sys::Handle>)>);

impl Drop for Stream {
    fn drop(&mut self) {
        for w in &self.watches {
            let mut g = w.0.lock().unwrap();
            g.0 = true;
            if let Some(h) = &g.1 {
                unsafe { CancelIoEx(h.0, std::ptr::null()) };
            }
        }
    }
}

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE;
/// What the kernel holds for us between reads; past this, changes are lost
/// and the disk is resynced instead.
const BUF: usize = 1 << 20;

/// Watch every fixed drive. Batches of folders arrive on `tx` until the
/// returned stream is dropped; `since` is the event id the caller's index
/// is current as of.
pub fn watch(since: u64, latency: f64, tx: Sender<Vec<Event>>) -> Stream {
    let now = current_event_id();
    let mut first = Vec::new();
    // Anything older than a moment ago may have changed unseen.
    if since.saturating_add(10_000_000) < now {
        first.push(Event { path: b"/".to_vec(), flags: MUST_SCAN_SUBDIRS, id: now });
    }
    first.push(Event { path: Vec::new(), flags: HISTORY_DONE, id: now });
    let _ = tx.send(first);
    let drives = sys::fixed_drives();
    let watches = drives
        .iter()
        .filter_map(|&d| {
            let root = vec![b'/', d, b':'];
            // A drive that doesn't open now is retried by its thread.
            let w = Arc::new(Watch(Mutex::new((false, sys::open_dir(&root, true)))));
            let (me, tx) = (w.clone(), tx.clone());
            std::thread::Builder::new().name(format!("scry-watch-{}", d as char)).spawn(move || run(&me, &root, latency, &tx)).ok()?;
            Some(w)
        })
        .collect();
    Stream { watches, drives }
}

fn run(w: &Watch, root: &[u8], latency: f64, tx: &Sender<Vec<Event>>) {
    let mut buf = vec![0u64; BUF / 8];
    let Some(ev) = sys::event() else { return };
    // Whether changes may have gone unseen (the watch broke, or never
    // started), and how long to wait before trying again.
    let (mut lost, mut backoff) = (false, Duration::from_secs(1));
    loop {
        let mut ov: OVERLAPPED = unsafe { zeroed() };
        ov.hEvent = ev.0;
        let armed = {
            let mut g = w.0.lock().unwrap();
            if g.0 {
                break;
            }
            if g.1.is_none() {
                g.1 = sys::open_dir(root, true);
            }
            match &g.1 {
                None => Err(std::io::Error::other("the drive does not open")),
                Some(h) => {
                    match unsafe { ReadDirectoryChangesW(h.0, buf.as_mut_ptr().cast(), BUF as u32, 1, FILTER, std::ptr::null_mut(), &mut ov, None) } {
                        0 => Err(std::io::Error::last_os_error()),
                        _ => Ok(h.0),
                    }
                }
            }
        };
        let h = match armed {
            Ok(h) => h,
            Err(e) => {
                if !pause(w, root, e, &mut lost, &mut backoff) {
                    break;
                }
                continue;
            }
        };
        if lost {
            // Watching again: what changed meanwhile gets resynced.
            crate::engine::log(format!("watching {} again", paths::display(root)));
            (lost, backoff) = (false, Duration::from_secs(1));
            if tx.send(vec![Event { path: b"/".to_vec(), flags: MUST_SCAN_SUBDIRS | DROPPED, id: current_event_id() }]).is_err() {
                break;
            }
        }
        let mut n = 0u32;
        let err = match unsafe { GetOverlappedResult(h, &ov, &mut n, 1) } {
            0 => Some(unsafe { GetLastError() }),
            _ => None,
        };
        let id = current_event_id();
        let batch = match err {
            None if n > 0 => folders(&buf, n as usize, root, id),
            // More changed than the buffer held.
            None | Some(ERROR_NOTIFY_ENUM_DIR) => vec![Event { path: b"/".to_vec(), flags: MUST_SCAN_SUBDIRS | DROPPED, id }],
            Some(e) => {
                // Cancelled because we are stopping, or the drive went away
                // (ejected, locked, dismounted) or the system ran short.
                if w.0.lock().unwrap().0 || !pause(w, root, std::io::Error::from_raw_os_error(e as i32), &mut lost, &mut backoff) {
                    break;
                }
                continue;
            }
        };
        if tx.send(batch).is_err() {
            break;
        }
        // The kernel keeps collecting meanwhile: one read then picks up a
        // whole burst.
        std::thread::sleep(Duration::from_secs_f64(latency));
    }
    w.0.lock().unwrap().1 = None;
}

/// The watch broke: close it and wait before the loop reopens it, longer
/// each time (up to 30 s). False once stopping.
fn pause(w: &Watch, root: &[u8], err: std::io::Error, lost: &mut bool, backoff: &mut Duration) -> bool {
    if !*lost {
        crate::engine::log(format!("watching {} stopped: {err}; retrying", paths::display(root)));
    }
    *lost = true;
    w.0.lock().unwrap().1 = None;
    let until = std::time::Instant::now() + *backoff;
    *backoff = (*backoff * 2).min(Duration::from_secs(30));
    while std::time::Instant::now() < until {
        if w.0.lock().unwrap().0 {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// What a folder's listing shows: names, sizes, mtimes.
const LISTED: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_DATA_OVERWRITE
    | USN_REASON_DATA_EXTEND
    | USN_REASON_DATA_TRUNCATION
    | USN_REASON_BASIC_INFO_CHANGE;

/// The folders on `drive` where something changed since `since` (Unix
/// seconds), from the NTFS change journal. An unprivileged process may read
/// it (Windows 10 1709+): names come back blank, but each record keeps the
/// id of the folder it happened in, which is all a relist needs. A folder
/// that is gone by now resolves to nothing; its own removal is recorded in
/// its parent. Err (why) when the journal doesn't reach back to `since`, or
/// there is none (FAT, exFAT; ReFS's 128-bit ids).
pub fn journal_dirs(drive: u8, since: u32) -> Result<Vec<Vec<u8>>, String> {
    let h = sys::open_dir(&[b'/', drive, b':'], false).ok_or("the drive does not open")?;
    let mut jd: USN_JOURNAL_DATA_V0 = unsafe { zeroed() };
    let mut n = 0u32;
    let size = size_of::<USN_JOURNAL_DATA_V0>() as u32;
    if unsafe { DeviceIoControl(h.0, FSCTL_QUERY_USN_JOURNAL, std::ptr::null(), 0, (&raw mut jd).cast(), size, &mut n, std::ptr::null_mut()) } == 0 {
        return Err(format!("no change journal ({})", std::io::Error::last_os_error()));
    }
    let cutoff = (since as i64 + 11_644_473_600) * 10_000_000;
    let mut rd = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: jd.FirstUsn,
        // Every record: the oldest of any kind says how far back it reaches.
        ReasonMask: u32::MAX,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: jd.UsnJournalID,
    };
    let mut buf = vec![0u64; 1 << 17];
    let (mut parents, mut covered) = (HashSet::new(), false);
    // Up to where the journal was when we asked; later changes come by watch.
    while rd.StartUsn < jd.NextUsn {
        let size = size_of::<READ_USN_JOURNAL_DATA_V0>() as u32;
        let ok = unsafe {
            DeviceIoControl(
                h.0,
                FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
                (&raw const rd).cast(),
                size,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n < 8 {
            return Err(format!("the change journal does not read ({})", std::io::Error::last_os_error()));
        }
        let base = buf.as_ptr() as *const u8;
        let mut off = 8usize;
        while off < n as usize {
            // Records are 8-byte aligned and lie inside what the read filled.
            if off + size_of::<USN_RECORD_V2>() > n as usize {
                return Err("a change journal record runs past the read".into());
            }
            let r = unsafe { base.add(off) } as *const USN_RECORD_V2;
            let (len, major, parent, time, reason) =
                unsafe { ((*r).RecordLength, (*r).MajorVersion, (*r).ParentFileReferenceNumber, (*r).TimeStamp, (*r).Reason) };
            if major != 2 || len == 0 || off + len as usize > n as usize {
                return Err(format!("a change journal record of version {major} is not understood"));
            }
            // The oldest record must predate `since`, or changes may be lost.
            covered |= time <= cutoff;
            if !covered {
                let oldest = (time / 10_000_000 - 11_644_473_600).max(0);
                return Err(format!("the change journal reaches back {} only, not the {} needed", ago(oldest), ago(since as i64)));
            }
            if time >= cutoff && reason & LISTED != 0 {
                parents.insert(parent);
            }
            off += len as usize;
        }
        let next = unsafe { *(base as *const i64) };
        if next <= rd.StartUsn {
            break;
        }
        rd.StartUsn = next;
    }
    // An empty journal proves nothing either.
    if !covered {
        return Err("the change journal is empty".into());
    }
    let ids: Vec<u64> = parents.into_iter().collect();
    Ok(ids.par_iter().filter_map(|&id| folder_by_id(&h, id)).collect())
}

/// How long ago Unix second `t` was, roughly: "3h 20m", "45m", "2d 4h".
fn ago(t: i64) -> String {
    let s = (crate::query::now_secs() as i64 - t).max(0);
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// The path of the folder with file id `id` on `vol`'s drive.
fn folder_by_id(vol: &sys::Handle, id: u64) -> Option<Vec<u8>> {
    let desc = FILE_ID_DESCRIPTOR {
        dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id as i64 },
    };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let h = unsafe { OpenFileById(vol.0, &desc, 0, share, std::ptr::null(), FILE_FLAG_BACKUP_SEMANTICS) };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = sys::Handle(h);
    let mut out = vec![0u16; 512];
    loop {
        let n = unsafe { GetFinalPathNameByHandleW(h.0, out.as_mut_ptr(), out.len() as u32, 0) } as usize;
        if n == 0 {
            return None;
        }
        if n < out.len() {
            out.truncate(n);
            return paths::from_native(Path::new(&std::ffi::OsString::from_wide(&out)));
        }
        out.resize(n, 0);
    }
}

/// The folders the changes in the first `n` bytes of `buf` happened in,
/// once each.
fn folders(buf: &[u64], n: usize, root: &[u8], id: u64) -> Vec<Event> {
    let base = buf.as_ptr() as *const u8;
    let name_at = offset_of!(FILE_NOTIFY_INFORMATION, FileName);
    let mut dirs = HashSet::new();
    let mut path = Vec::new();
    let mut off = 0usize;
    // Records are 4-byte aligned and lie inside what the read filled.
    while off + name_at <= n {
        let e = unsafe { base.add(off) } as *const FILE_NOTIFY_INFORMATION;
        let (next, len) = unsafe { ((*e).NextEntryOffset, (*e).FileNameLength) };
        if off + name_at + len as usize > n {
            break;
        }
        let name = unsafe { std::slice::from_raw_parts(base.add(off + name_at) as *const u16, len as usize / 2) };
        path.clear();
        path.extend_from_slice(root);
        path.push(b'/');
        let at = path.len();
        paths::push_wide(&mut path, name);
        for b in &mut path[at..] {
            if *b == b'\\' {
                *b = b'/';
            }
        }
        let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0).max(root.len());
        dirs.insert(path[..cut].to_vec());
        if next == 0 {
            break;
        }
        off += next as usize;
    }
    dirs.into_iter()
        .map(|d| {
            // A change made through an 8.3 name is reported under it.
            let path = if d.contains(&b'~') { sys::long_name(&d).unwrap_or(d) } else { d };
            Event { path, flags: 0, id }
        })
        .collect()
}
