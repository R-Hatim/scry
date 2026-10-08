//! Parallel whole-disk enumeration.
//!
//! Each folder is opened relative to its parent's handle (NT's openat, so
//! no path is rebuilt or re-walked from the drive root) and read with
//! GetFileInformationByHandleEx: one call returns hundreds of entries with
//! name, attributes, size and mtime already attached, so there is no
//! per-file stat. Folders fan out over a rayon pool. The root "/" is
//! virtual: its children are the fixed drives. Reparse points that stand
//! for another place (symlinks, junctions, volume mount points) are kept as
//! entries but never entered, so every folder is listed once, under its real
//! path.

use crate::{paths, sys};
use rayon::Scope;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT};
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

pub const NONE: u32 = u32::MAX;

/// Folders never to open (`Options::skip`).
pub static SKIP: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();

pub fn blocked(path: &[u8]) -> bool {
    SKIP.get().is_some_and(|v| v.iter().any(|s| path.starts_with(s) && (path.len() == s.len() || path[s.len()] == b'/')))
}

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_LINK: u8 = 2;
pub const KIND_OTHER: u8 = 3;

/// Entry flag bit: the hidden attribute.
pub const FLAG_HIDDEN: u8 = 1 << 2;
/// A folder we do not descend into: a junction or a volume mount point.
pub const FLAG_MOUNT: u8 = 1 << 3;

/// An entry's kind and flags, from its attributes and reparse tag. Only
/// reparse points that name another place count as links; the rest (cloud
/// placeholders, dedup, app execution aliases) are what they look like.
pub fn kind_of(attrs: u32, tag: u32) -> u8 {
    let dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    let k = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && sys::elsewhere(tag) {
        // A folder whose tag we can't tell is kept as one, never entered.
        if dir && matches!(tag, IO_REPARSE_TAG_MOUNT_POINT | 0) { KIND_DIR | FLAG_MOUNT } else { KIND_LINK }
    } else if dir {
        KIND_DIR
    } else if attrs & FILE_ATTRIBUTE_DEVICE != 0 {
        KIND_OTHER
    } else {
        KIND_FILE
    };
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 { k | FLAG_HIDDEN } else { k }
}

#[derive(Clone, Copy)]
pub struct RawEnt {
    pub name_off: u32,
    pub name_len: u16,
    /// kind in the low 2 bits, FLAG_* above.
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    /// Temp id of the listing for this directory, or NONE.
    pub child: u32,
}

pub struct Listing {
    pub id: u32,
    pub names: Vec<u8>,
    pub ents: Vec<RawEnt>,
}

struct Ctx {
    next_id: AtomicU32,
    out: Vec<Mutex<Vec<Listing>>>,
}

thread_local! {
    static BUF: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Scan `root` recursively. Listing id 0 is `root` itself.
pub fn scan(root: &[u8], threads: usize) -> Vec<Listing> {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
    let ctx = Ctx { next_id: AtomicU32::new(1), out: (0..threads + 1).map(|_| Mutex::new(Vec::new())).collect() };
    // Paths are only tracked when there is something to skip.
    let track = SKIP.get().is_some_and(|v| !v.is_empty());
    pool.scope(|s| {
        if root != b"/" {
            let h = if blocked(root) { None } else { sys::open_dir(root, false) };
            return finish_dir(s, h, track.then(|| root.to_vec()), 0, &ctx);
        }
        let mut l = drives();
        let mut kids = Vec::new();
        for e in l.ents.iter_mut() {
            let path = crate::live::join(b"/", &l.names[e.name_off as usize..][..e.name_len as usize]);
            if !blocked(&path) {
                e.child = ctx.next_id.fetch_add(1, Ordering::Relaxed);
                kids.push((path, e.child));
            }
        }
        push(l, &ctx);
        let ctx = &ctx;
        for (path, id) in kids {
            s.spawn(move |s| finish_dir(s, sys::open_dir(&path, false), track.then_some(path), id, ctx));
        }
    });
    ctx.out.into_iter().flat_map(|m| m.into_inner().unwrap()).collect()
}

/// The virtual root's listing: a folder per fixed drive.
fn drives() -> Listing {
    let mut l = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
    for d in sys::fixed_drives() {
        let mtime = sys::stat(&[b'/', d, b':']).map_or(0, |s| s.mtime);
        l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: 2, kind: KIND_DIR, size: 0, mtime, child: NONE });
        l.names.extend_from_slice(&[d, b':']);
    }
    l
}

/// List a single directory (no recursion). Subdirectories come back with
/// `child == NONE`. Used by the live updater.
pub fn list_one(path: &[u8]) -> Option<Listing> {
    if blocked(path) {
        return None;
    }
    if path == b"/" {
        return Some(drives());
    }
    let h = sys::open_dir(path, false)?;
    let mut l = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
    // Half a listing would read as half the folder deleted.
    list_handle(&h, &mut l).then_some(l)
}

// Children are opened relative to the parent's handle, which stays open
// until the last of them is.
fn finish_dir<'s>(s: &Scope<'s>, h: Option<sys::Handle>, path: Option<Vec<u8>>, id: u32, ctx: &'s Ctx) {
    let mut l = Listing { id, names: Vec::new(), ents: Vec::new() };
    let Some(h) = h else {
        push(l, ctx);
        return;
    };
    list_handle(&h, &mut l);
    let me = Arc::new(h);
    let mut kids = Vec::new();
    for e in l.ents.iter_mut() {
        if e.kind & 3 == KIND_DIR && e.kind & FLAG_MOUNT == 0 {
            let name = &l.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            let child_path = path.as_ref().map(|p| crate::live::join(p, name));
            if child_path.as_deref().is_some_and(blocked) {
                continue;
            }
            e.child = ctx.next_id.fetch_add(1, Ordering::Relaxed);
            let mut w = Vec::with_capacity(name.len());
            paths::push_utf16(&mut w, name);
            kids.push((w, e.child, child_path));
        }
    }
    push(l, ctx);
    for (name, cid, child_path) in kids {
        let parent = me.clone();
        s.spawn(move |s| {
            let h = sys::open_subdir(&parent, &name);
            drop(parent);
            finish_dir(s, h, child_path, cid, ctx);
        });
    }
}

fn push(l: Listing, ctx: &Ctx) {
    let slot = rayon::current_thread_index().unwrap_or(ctx.out.len() - 1);
    ctx.out[slot].lock().unwrap().push(l);
}

/// False if the listing broke off before the end.
fn list_handle(h: &sys::Handle, l: &mut Listing) -> bool {
    BUF.with_borrow_mut(|buf| {
        sys::read_dir(h, buf, |e| {
            let off = l.names.len();
            paths::push_wide(&mut l.names, e.name);
            let len = l.names.len() - off;
            if len == 0 || len > u16::MAX as usize {
                l.names.truncate(off);
                return;
            }
            l.ents.push(RawEnt {
                name_off: off as u32,
                name_len: len as u16,
                kind: kind_of(e.attrs, e.tag),
                size: e.size,
                mtime: e.mtime,
                child: NONE,
            });
        })
    })
}
