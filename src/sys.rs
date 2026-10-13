//! The Windows calls everything else stands on: handles, directory reads,
//! attributes, drives, privileges, thread priorities, named pipes and the
//! login item. Paths come in as index paths (see `paths`).

use crate::paths;
use std::ffi::OsString;
use std::mem::{offset_of, size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{FromRawHandle, IntoRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtOpenFile,
};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

/// An owned kernel handle, closed on drop.
pub struct Handle(pub HANDLE);

// A handle is just an index into the process's handle table.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Never stand in the way of anyone renaming or deleting what we read.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A FILETIME (100 ns ticks since 1601) as Unix seconds.
pub fn unix_secs(ft: i64) -> u32 {
    (ft / 10_000_000 - 11_644_473_600).clamp(0, u32::MAX as i64) as u32
}

fn ft(t: FILETIME) -> i64 {
    ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as i64
}

/// Now, as a FILETIME.
pub fn filetime_now() -> u64 {
    let mut t: FILETIME = unsafe { zeroed() };
    unsafe { GetSystemTimePreciseAsFileTime(&mut t) };
    ft(t) as u64
}

/// Reparse tags with this bit stand for another file or folder (symlinks,
/// junctions, mount points).
pub const NAME_SURROGATE: u32 = 0x2000_0000;

/// Does a reparse point with this tag stand for another place? An unknown
/// tag (0: a file system that doesn't report it) counts as one: never
/// entered rather than maybe entered twice.
pub fn elsewhere(tag: u32) -> bool {
    tag & NAME_SURROGATE != 0 || tag == 0
}

/// An open folder, unless it is a reparse point standing for another place:
/// such a folder is listed where it really is, never through the link.
/// Other reparse points (cloud folders) are opened again without
/// FILE_FLAG_OPEN_REPARSE_POINT, so their file system filter serves them.
fn real_dir(h: Handle, reopen: impl FnOnce() -> Option<Handle>) -> Option<Handle> {
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
    let size = size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32;
    let ok = unsafe { GetFileInformationByHandleEx(h.0, FileAttributeTagInfo, (&raw mut info).cast(), size) } != 0;
    match ok && info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        false => Some(h),
        true if elsewhere(info.ReparseTag) => None,
        true => {
            drop(h);
            reopen()
        }
    }
}

/// Open a folder for listing (or watching, `overlapped`). Backup semantics
/// is what lets CreateFile open a folder at all; with SeBackupPrivilege on,
/// it also gets past the folder's permissions.
pub fn open_dir(path: &[u8], overlapped: bool) -> Option<Handle> {
    let w = paths::to_wide(path);
    let open = |extra: u32| {
        let flags = FILE_FLAG_BACKUP_SEMANTICS | extra | if overlapped { FILE_FLAG_OVERLAPPED } else { 0 };
        let h = unsafe { CreateFileW(w.as_ptr(), FILE_LIST_DIRECTORY | SYNCHRONIZE, SHARE_ALL, null(), OPEN_EXISTING, flags, null_mut()) };
        (h != INVALID_HANDLE_VALUE).then_some(Handle(h))
    };
    real_dir(open(FILE_FLAG_OPEN_REPARSE_POINT)?, || open(0))
}

/// Open the folder `name` inside `parent`: NT's openat, so no path is
/// rebuilt and re-walked from the drive root for every folder.
pub fn open_subdir(parent: &Handle, name: &[u16]) -> Option<Handle> {
    let bytes = u16::try_from(name.len() * 2).ok()?;
    let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: name.as_ptr() as *mut u16 };
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.0,
        ObjectName: &us,
        Attributes: 0,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let open = |extra: u32| {
        let mut iosb: IO_STATUS_BLOCK = unsafe { zeroed() };
        let mut h = null_mut();
        let opts = FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_FOR_BACKUP_INTENT | extra;
        let st = unsafe { NtOpenFile(&mut h, FILE_LIST_DIRECTORY | SYNCHRONIZE, &oa, &mut iosb, SHARE_ALL, opts) };
        (st >= 0).then_some(Handle(h))
    };
    real_dir(open(FILE_OPEN_REPARSE_POINT)?, || open(0))
}

/// One entry of a folder listing.
pub struct Ent<'a> {
    pub name: &'a [u16],
    pub attrs: u32,
    /// Reparse tag, if `attrs` has FILE_ATTRIBUTE_REPARSE_POINT.
    pub tag: u32,
    pub size: u64,
    pub mtime: u32,
}

const DIR_BUF: usize = 128 << 10;

/// List an open folder in a few big reads, every entry arriving with its
/// attributes, size and mtime. `buf` is reusable scratch. False if the
/// listing broke off (an I/O error, the folder going away): what arrived is
/// then not the whole folder.
pub fn read_dir(h: &Handle, buf: &mut Vec<u64>, mut f: impl FnMut(Ent)) -> bool {
    if buf.len() < DIR_BUF / 8 {
        buf.resize(DIR_BUF / 8, 0);
    }
    let name_at = offset_of!(FILE_FULL_DIR_INFO, FileName);
    loop {
        let ok = unsafe { GetFileInformationByHandleEx(h.0, FileFullDirectoryInfo, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) };
        if ok == 0 {
            // FILE_NOT_FOUND: a folder without even "." (an empty volume root).
            return matches!(unsafe { GetLastError() }, ERROR_NO_MORE_FILES | ERROR_FILE_NOT_FOUND);
        }
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            // Entries are 8-byte aligned and lie inside the buffer the call filled.
            let e = unsafe { base.add(off) } as *const FILE_FULL_DIR_INFO;
            let (next, attrs, len, ea, size, mtime) =
                unsafe { ((*e).NextEntryOffset, (*e).FileAttributes, (*e).FileNameLength, (*e).EaSize, (*e).EndOfFile, (*e).LastWriteTime) };
            let name = unsafe { std::slice::from_raw_parts(base.add(off + name_at) as *const u16, len as usize / 2) };
            if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                // For a reparse point the EA size field holds its tag instead.
                let tag = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 { ea } else { 0 };
                f(Ent { name, attrs, tag, size: size.max(0) as u64, mtime: unix_secs(mtime) });
            }
            if next == 0 {
                break;
            }
            off += next as usize;
        }
    }
}

/// What `stat` reports.
pub struct Stat {
    pub attrs: u32,
    pub tag: u32,
    pub size: u64,
    pub mtime: u32,
}

/// Attributes of a path, not following it: from the entry itself, or where
/// that is refused (no access, in use), from its folder's listing.
pub fn stat(path: &[u8]) -> Option<Stat> {
    let w = paths::to_wide(path);
    let mut d: WIN32_FILE_ATTRIBUTE_DATA = unsafe { zeroed() };
    if unsafe { GetFileAttributesExW(w.as_ptr(), GetFileExInfoStandard, (&raw mut d).cast()) } != 0 {
        // A reparse point's tag (is it a link?) is only in the listing.
        if d.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            let size = (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64;
            return Some(Stat { attrs: d.dwFileAttributes, tag: 0, size, mtime: unix_secs(ft(d.ftLastWriteTime)) });
        }
    } else if matches!(unsafe { GetLastError() }, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_INVALID_NAME) {
        return None;
    }
    let mut d: WIN32_FIND_DATAW = unsafe { zeroed() };
    let h = unsafe { FindFirstFileExW(w.as_ptr(), FindExInfoBasic, (&raw mut d).cast(), FindExSearchNameMatch, null(), 0) };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe { FindClose(h) };
    let tag = if d.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 { d.dwReserved0 } else { 0 };
    let size = (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64;
    Some(Stat { attrs: d.dwFileAttributes, tag, size, mtime: unix_secs(ft(d.ftLastWriteTime)) })
}

/// A path with its 8.3 short names (`PROGRA~1`) spelled out, if it exists.
pub fn long_name(path: &[u8]) -> Option<Vec<u8>> {
    let w = paths::to_wide(path);
    let mut out = vec![0u16; 512];
    loop {
        let n = unsafe { GetLongPathNameW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) } as usize;
        if n == 0 {
            return None;
        }
        if n < out.len() {
            out.truncate(n);
            return paths::from_native(Path::new(&OsString::from_wide(&out)));
        }
        out.resize(n, 0);
    }
}

/// Letters of the fixed drives: no removable, network or optical ones, and
/// no `subst` letters (a folder of another drive, which would be indexed
/// twice).
pub fn fixed_drives() -> Vec<u8> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| b'A' + i)
        .filter(|&d| {
            let (letter, root) = ([d as u16, b':' as u16, 0], [d as u16, b':' as u16, b'\\' as u16, 0]);
            let mut target = [0u16; 512];
            let n = unsafe { QueryDosDeviceW(letter.as_ptr(), target.as_mut_ptr(), 512) };
            // A subst letter maps to `\??\C:\some\folder`, a volume to `\Device\...`.
            let subst = n >= 4 && target[..4] == [b'\\' as u16, b'?' as u16, b'?' as u16, b'\\' as u16];
            !subst && unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
        })
        .collect()
}

/// Turn on SeBackupPrivilege, so folder opens get past folder permissions
/// (what backup tools do). Only an elevated process holds it; anyone else
/// gets false and indexes what their account can read.
pub fn enable_backup_privilege() -> bool {
    unsafe {
        let mut tok = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let tok = Handle(tok);
        let mut tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: zeroed(), Attributes: SE_PRIVILEGE_ENABLED }] };
        let name = wide("SeBackupPrivilege");
        if LookupPrivilegeValueW(null(), name.as_ptr(), &mut tp.Privileges[0].Luid) == 0 {
            return false;
        }
        // Succeeds even when the token lacks the privilege; that is
        // ERROR_NOT_ALL_ASSIGNED.
        AdjustTokenPrivileges(tok.0, 0, &tp, 0, null_mut(), null_mut()) != 0 && GetLastError() == ERROR_SUCCESS
    }
}

/// Someone is waiting on this thread: opt it out of EcoQoS, which Windows
/// may otherwise apply to a process without a window.
pub fn interactive_thread() {
    let s = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    unsafe {
        SetThreadInformation(GetCurrentThread(), ThreadPowerThrottling, (&raw const s).cast(), size_of::<THREAD_POWER_THROTTLING_STATE>() as u32)
    };
}

/// Background work: low CPU, I/O and memory priority for this thread.
pub fn background_thread() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// A manual-reset event, for overlapped I/O.
pub fn event() -> Option<Handle> {
    let h = unsafe { CreateEventW(null(), 1, 0, null()) };
    (!h.is_null()).then_some(Handle(h))
}

/// This process's user, as an SID string (`S-1-5-21-...`).
fn user_sid() -> Option<String> {
    unsafe {
        let mut tok = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return None;
        }
        let tok = Handle(tok);
        let mut buf = vec![0u64; 64];
        let mut n = 0;
        if GetTokenInformation(tok.0, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut n) == 0 {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *s.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s.cast());
        Some(sid)
    }
}

/// A listening instance of the named pipe `name`. The first one also claims
/// the name, and fails if another process already holds it. Only this user
/// (and SYSTEM) may open it: by default every local account, anonymous
/// logons included, could open it for reading.
pub fn pipe_listen(name: &str, first: bool) -> std::io::Result<Handle> {
    let w = wide(name);
    let sid = user_sid().ok_or_else(std::io::Error::last_os_error)?;
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
    let mut sd = null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let open = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let h = unsafe { CreateNamedPipeW(w.as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, &sa) };
    let err = std::io::Error::last_os_error();
    unsafe { LocalFree(sd) };
    if h == INVALID_HANDLE_VALUE {
        return Err(err);
    }
    Ok(Handle(h))
}

/// Wait for a client on a listening instance; the connection is then a file.
pub fn pipe_accept(h: Handle) -> std::io::Result<std::fs::File> {
    if unsafe { ConnectNamedPipe(h.0, null_mut()) } == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_handle(h.into_raw()) })
}

/// Connect to the named pipe `name`, waiting while every instance is busy.
pub fn pipe_connect(name: &str) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let w = wide(name);
    for _ in 0..50 {
        // Identification only: whoever answers on this name may learn who
        // we are, never act as us.
        match std::fs::OpenOptions::new().read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION).open(name) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => unsafe {
                WaitNamedPipeW(w.as_ptr(), 100);
            },
            r => return r,
        }
    }
    Err(std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY as i32))
}

/// Where processes started at login are listed, per user.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Start `command` at login (the user's Run key), or stop doing so (None).
pub fn login_item(name: &str, command: Option<&str>) -> bool {
    let (key, val) = (wide(RUN_KEY), wide(name));
    unsafe {
        match command {
            Some(c) => {
                let data = wide(c);
                RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) == 0
            }
            None => matches!(RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr()), 0 | ERROR_FILE_NOT_FOUND),
        }
    }
}

/// Whether something is started at login as `name`.
pub fn has_login_item(name: &str) -> bool {
    let (key, val) = (wide(RUN_KEY), wide(name));
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size) == 0 }
}

/// Send this process's stderr (where the engine logs) to `f`.
pub fn set_stderr(f: std::fs::File) {
    unsafe { SetStdHandle(STD_ERROR_HANDLE, f.into_raw_handle()) };
}

/// Keep this process's own stdin/stdout/stderr out of programs it starts.
/// A child inherits every inheritable handle, so a daemon started from
/// `scry status | more` would hold the pipe open and `more` would never
/// see its end.
pub fn keep_std_handles() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
