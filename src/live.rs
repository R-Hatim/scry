//! The index as it is right now: the immutable base, a bitset of base
//! entries that are gone, and a small overlay of entries added since.
//!
//! Every change notification names a folder and is handled the same way:
//! list that one folder and diff it against what we have. The diff is
//! idempotent, so catch-up relists, duplicate events, and events that race
//! a compaction are all harmless.

use crate::index::{Index, enc_size};
use crate::walk::{self, FLAG_MOUNT, KIND_DIR, Listing, NONE, RawEnt};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub struct OEnt {
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    /// `index::name_mask` of the entry's name, so a search rejects most of
    /// the overlay with one AND.
    pub mask: u64,
    /// Location prior of the nearest folder the base knows (set on insert).
    pub prior: i8,
}

impl OEnt {
    /// `path` may be the whole path or just the name.
    pub fn new(path: &[u8], kind: u8, size: u64, mtime: u32) -> OEnt {
        let name = &path[path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1)..];
        OEnt { kind, size, mtime, mask: crate::index::name_mask(name), prior: 0 }
    }
}

/// Folder sizes by dir id, and the (dead, overlay, changes) they were taken at.
type DirSizes = Option<((usize, usize, u64), Arc<Vec<u64>>)>;

pub struct Live {
    pub base: Index,
    dead: Vec<u64>,
    pub dead_count: usize,
    pub over: BTreeMap<Vec<u8>, OEnt>,
    /// Last event id fully applied.
    pub event_id: u64,
    /// Paths of directories added or removed since last drained; the
    /// content index re-syncs these whole subtrees.
    pub trees: Vec<Vec<u8>>,
    /// The last name search's scored names, reused while you type.
    pub names_cache: crate::query::NameCache,
    /// Wall-clock second up to which every change is known applied.
    pub synced_at: u32,
    /// Overlay folder -> prior of its nearest base folder.
    priors: HashMap<Vec<u8>, i8>,
    /// Bumped by every overlay write, so cached folder sizes know they are stale.
    changes: u64,
    /// `dir_sizes`, and the (dead, overlay, changes) it was computed at.
    dir_sizes: Mutex<DirSizes>,
    /// Folder sizes of the base alone, computed once.
    base_sizes: std::sync::OnceLock<Arc<Vec<u64>>>,
}

/// What `Live::fetch` read from disk for one folder update.
pub struct Fetched {
    path: Vec<u8>,
    recursive: bool,
    blocked: bool,
    attrs: Option<OEnt>,
    listing: Option<Listing>,
    scans: HashMap<Vec<u8>, Vec<Listing>>,
}

/// What makes an entry a different thing: its kind, and for a folder
/// whether we descend into it (a real folder) or not (a junction).
const SHAPE: u8 = 3 | FLAG_MOUNT;

pub enum Applied {
    Done,
    /// The whole disk needs rescanning (history lost at the root).
    Rebuild,
}

impl Live {
    pub fn new(base: Index) -> Live {
        base.prefault();
        let words = base.n.div_ceil(64);
        let event_id = base.event_id;
        let synced_at = base.synced_at;
        Live {
            base,
            dead: vec![0; words],
            dead_count: 0,
            over: BTreeMap::new(),
            event_id,
            trees: Vec::new(),
            names_cache: Default::default(),
            synced_at,
            priors: HashMap::new(),
            changes: 0,
            dir_sizes: Mutex::new(None),
            base_sizes: std::sync::OnceLock::new(),
        }
    }

    /// Put an entry in the overlay, stamping the prior it ranks with.
    fn put(&mut self, path: Vec<u8>, mut e: OEnt) {
        let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
        e.prior = match self.priors.get(&path[..cut]) {
            Some(&p) => p,
            None => {
                let mut up = &path[..cut];
                let mut prior = 0;
                while !up.is_empty() {
                    if let Some(d) = self.base.lookup(up).and_then(|e| self.base.dir_of(e)) {
                        prior = self.base.dir_prior()[d as usize];
                        break;
                    }
                    up = &up[..up.iter().rposition(|&b| b == b'/').unwrap_or(0)];
                }
                self.priors.insert(path[..cut].to_vec(), prior);
                prior
            }
        };
        self.over.insert(path, e);
        self.changes += 1;
    }

    #[inline]
    pub fn is_dead(&self, i: u32) -> bool {
        self.dead[i as usize >> 6] & (1 << (i & 63)) != 0
    }

    fn kill(&mut self, i: u32) {
        let w = &mut self.dead[i as usize >> 6];
        if *w & (1 << (i & 63)) == 0 {
            *w |= 1 << (i & 63);
            self.dead_count += 1;
        }
    }

    fn kill_subtree(&mut self, e: u32) {
        self.kill(e);
        if let Some(d) = self.base.dir_of(e) {
            let (a, b) = (self.base.dir_start()[d as usize], self.base.dir_end()[d as usize]);
            for i in a..b {
                self.kill(i);
            }
        }
    }

    fn drop_over_subtree(&mut self, path: &[u8]) {
        self.over.remove(path);
        let (lo, hi) = subtree_bounds(path);
        let keys: Vec<Vec<u8>> = self.over.range(lo..hi).map(|(k, _)| k.clone()).collect();
        for k in keys {
            self.over.remove(&k);
        }
    }

    /// Alive base entry for a path, if the base has one.
    fn base_alive(&self, path: &[u8]) -> Option<u32> {
        self.base.lookup(path).filter(|&e| !self.is_dead(e))
    }

    fn remove_path(&mut self, path: &[u8]) {
        self.trees.push(path.to_vec());
        if let Some(e) = self.base_alive(path) {
            self.kill_subtree(e);
        }
        self.drop_over_subtree(path);
    }

    /// Every base folder's size (by dir id): the files under it, leaving out
    /// the ones gone since the base was saved and counting the ones added.
    /// The base's own sizes are added up once; each change since is then
    /// carried up its folders (a few thousand steps, not millions). Cached
    /// until the index changes.
    pub fn dir_sizes(&self) -> Arc<Vec<u64>> {
        let base = self.base_sizes.get_or_init(|| Arc::new(self.add_up_base()));
        if self.dead_count == 0 && self.over.is_empty() {
            return base.clone();
        }
        let at = (self.dead_count, self.over.len(), self.changes);
        let mut cache = self.dir_sizes.lock().unwrap();
        if let Some((when, sizes)) = cache.as_ref()
            && *when == at
        {
            return sizes.clone();
        }
        let idx = &self.base;
        let (kind, parent, de) = (idx.kind(), idx.parent(), idx.dir_entry());
        let mut t = (**base).clone();
        // `d` and every folder above it, up to the root (ids only go down).
        let carry = |t: &mut Vec<u64>, mut d: usize, size: u64, add: bool| loop {
            t[d] = if add { t[d] + size } else { t[d].saturating_sub(size) };
            let p = parent[de[d] as usize] as usize;
            if p >= d {
                break;
            }
            d = p;
        };
        for (w, &bits) in self.dead.iter().enumerate() {
            let mut m = bits;
            while m != 0 {
                let i = w * 64 + m.trailing_zeros() as usize;
                m &= m - 1;
                if i < idx.n && kind[i] & 3 == walk::KIND_FILE {
                    carry(&mut t, parent[i] as usize, idx.size_of(i), false);
                }
            }
        }
        // Files added since: into the nearest folder the base has.
        let mut nearest: HashMap<&[u8], Option<u32>> = HashMap::new();
        for (path, o) in &self.over {
            if o.kind & 3 != walk::KIND_FILE {
                continue;
            }
            let dir = &path[..path.iter().rposition(|&b| b == b'/').unwrap_or(0)];
            let d = *nearest.entry(dir).or_insert_with(|| {
                let mut up = dir;
                loop {
                    if let Some(d) = self.base_alive(up).and_then(|e| idx.dir_of(e)) {
                        return Some(d);
                    }
                    if up.is_empty() {
                        return None;
                    }
                    up = &up[..up.iter().rposition(|&b| b == b'/').unwrap_or(0)];
                }
            });
            if let Some(d) = d {
                carry(&mut t, d as usize, o.size, true);
            }
        }
        let sizes = Arc::new(t);
        *cache = Some((at, sizes.clone()));
        sizes
    }

    /// The base's folder sizes: one pass over the entries, then one up the
    /// folders (a folder's id always comes after its parent's).
    fn add_up_base(&self) -> Vec<u64> {
        let idx = &self.base;
        let (kind, parent, de) = (idx.kind(), idx.parent(), idx.dir_entry());
        let mut t = vec![0u64; idx.d];
        for i in 0..idx.n {
            if kind[i] & 3 == walk::KIND_FILE {
                t[parent[i] as usize] += idx.size_of(i);
            }
        }
        for d in (1..idx.d).rev() {
            let p = parent[de[d] as usize] as usize;
            if p < d {
                t[p] += t[d];
            }
        }
        t
    }

    /// The size of a folder added since the base was saved: the files under
    /// it, all in the overlay.
    pub fn over_size(&self, dir: &[u8]) -> u64 {
        let (lo, hi) = subtree_bounds(dir);
        self.over.range(lo..hi).filter(|(_, o)| o.kind & 3 == walk::KIND_FILE).map(|(_, o)| o.size).sum()
    }

    /// What changed since the base was saved, as bytes for
    /// `restore_checkpoint`: which base entries are gone, the overlay, and
    /// how far it is in sync. A bit per base entry plus the overlay, so it
    /// can be written every few minutes where the base can't: a restart then
    /// catches up on minutes of changes, which the change journal still
    /// holds, rather than on everything since the last save.
    pub fn checkpoint(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.dead.len() * 8 + self.over.len() * 64);
        b.extend_from_slice(CHECKPOINT);
        for v in [self.base.n as u64, self.base.event_id, self.event_id, self.dead.len() as u64, self.dead_count as u64, self.over.len() as u64] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.base.synced_at.to_le_bytes());
        b.extend_from_slice(&self.synced_at.to_le_bytes());
        for w in &self.dead {
            b.extend_from_slice(&w.to_le_bytes());
        }
        for (p, e) in &self.over {
            b.extend_from_slice(&(p.len() as u32).to_le_bytes());
            b.extend_from_slice(p);
            b.push(e.kind);
            b.extend_from_slice(&e.size.to_le_bytes());
            b.extend_from_slice(&e.mtime.to_le_bytes());
            b.extend_from_slice(&e.mask.to_le_bytes());
            b.push(e.prior as u8);
        }
        b
    }

    /// Take up where a checkpoint of this same base left off. False, and
    /// nothing changed, if it was taken of another base or does not read.
    pub fn restore_checkpoint(&mut self, b: &[u8]) -> bool {
        self.read_checkpoint(b).is_some()
    }

    fn read_checkpoint(&mut self, b: &[u8]) -> Option<()> {
        let mut r = Bytes(b.strip_prefix(CHECKPOINT)?);
        let [n, base_event, event_id, words, dead_count, over] = [r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?];
        let (base_synced, synced_at) = (r.u32()?, r.u32()?);
        let ours = (self.base.n as u64, self.base.event_id, self.base.synced_at, self.dead.len() as u64);
        if (n, base_event, base_synced, words) != ours {
            return None;
        }
        let dead = (0..words).map(|_| r.u64()).collect::<Option<Vec<u64>>>()?;
        let mut map = BTreeMap::new();
        for _ in 0..over {
            let len = r.u32()? as usize;
            let path = r.take(len)?.to_vec();
            let e = OEnt { kind: r.u8()?, size: r.u64()?, mtime: r.u32()?, mask: r.u64()?, prior: r.u8()? as i8 };
            map.insert(path, e);
        }
        if !r.0.is_empty() {
            return None;
        }
        (self.dead, self.dead_count, self.over, self.event_id, self.synced_at) = (dead, dead_count as usize, map, event_id, synced_at);
        self.changes += 1;
        Some(())
    }

    /// Drop what is indexed under `path` but keep the folder itself, as a
    /// crawl leaves a folder it must not open: for one that is skipped now
    /// (`Options::skip`) but was indexed before. How many entries went.
    pub fn forget(&mut self, path: &[u8]) -> usize {
        let dead = self.dead_count;
        if let Some(d) = self.base_alive(path).and_then(|e| self.base.dir_of(e)) {
            let (a, b) = (self.base.dir_start()[d as usize], self.base.dir_end()[d as usize]);
            for i in a..b {
                self.kill(i);
            }
        }
        let (lo, hi) = subtree_bounds(path);
        let keys: Vec<Vec<u8>> = self.over.range(lo..hi).map(|(k, _)| k.clone()).collect();
        for k in &keys {
            self.over.remove(k);
        }
        if self.dead_count > dead || !keys.is_empty() {
            self.trees.push(path.to_vec());
        }
        self.dead_count - dead + keys.len()
    }

    /// Bring one directory (or, with `recursive`, its whole subtree) in line
    /// with the disk.
    pub fn apply_dir(&mut self, path: &[u8], recursive: bool) -> Applied {
        let f = self.fetch(path, recursive);
        self.apply(f)
    }

    /// The disk half of `apply_dir`: list the folder (or stat it) and scan
    /// any folder that will be new to the index. Needs only `&self`, so the
    /// caller does this under a read lock and searches keep answering.
    pub fn fetch(&self, path: &[u8], recursive: bool) -> Fetched {
        let p = normalize(path);
        let mut f = Fetched { path: p, recursive, blocked: false, attrs: None, listing: None, scans: HashMap::new() };
        // Not even an lstat inside folders we may not touch.
        if walk::blocked(&f.path) {
            f.blocked = true;
            return f;
        }
        if recursive {
            if f.path != b"/" {
                f.attrs = lstat(&f.path);
                if f.attrs.is_some_and(|a| a.kind & 3 == KIND_DIR && a.kind & FLAG_MOUNT == 0) {
                    f.scans.insert(f.path.clone(), walk::scan(&f.path, 4));
                }
            }
            return f;
        }
        let Some(listing) = walk::list_one(&f.path) else {
            f.attrs = lstat(&f.path);
            return f;
        };
        // Children that will be added as folders get their subtree scanned now.
        let cur = self.current_children(&f.path);
        for r in &listing.ents {
            if r.kind & 3 != KIND_DIR || r.kind & FLAG_MOUNT != 0 {
                continue;
            }
            let name = &listing.names[r.name_off as usize..][..r.name_len as usize];
            // A junction turned back into a real folder is new too.
            let was_dir = match cur.get(name) {
                Some(Some(c)) => self.base.kind()[*c as usize] & SHAPE == KIND_DIR,
                Some(None) => self.over.get(&join(&f.path, name)).is_some_and(|o| o.kind & SHAPE == KIND_DIR),
                None => false,
            };
            if !was_dir {
                let child = join(&f.path, name);
                let ls = walk::scan(&child, 4);
                f.scans.insert(child, ls);
            }
        }
        f.listing = Some(listing);
        f
    }

    /// Children the index holds for `p`: name -> base entry, or None for an
    /// overlay entry.
    fn current_children(&self, p: &[u8]) -> HashMap<Vec<u8>, Option<u32>> {
        let mut cur: HashMap<Vec<u8>, Option<u32>> = HashMap::new();
        if let Some(d) = self.base_alive(p).and_then(|e| self.base.dir_of(e)) {
            for c in self.base.children(d) {
                if !self.is_dead(c as u32) {
                    cur.insert(self.base.name(c).to_vec(), Some(c as u32));
                }
            }
        }
        let (lo, hi) = subtree_bounds(p);
        let plen = lo.len();
        for k in self.over.range(lo..hi).map(|(k, _)| k) {
            if !k[plen..].contains(&b'/') {
                cur.insert(k[plen..].to_vec(), None);
            }
        }
        cur
    }

    /// The memory half of `apply_dir`: diff what `fetch` read into the index.
    pub fn apply(&mut self, mut f: Fetched) -> Applied {
        if f.blocked {
            return Applied::Done;
        }
        let p = std::mem::take(&mut f.path);
        if f.recursive {
            if p == b"/" {
                return Applied::Rebuild;
            }
            self.remove_path(&p);
            if let Some(attrs) = f.attrs {
                let scan = f.scans.remove(&p);
                self.add_new(p, attrs, scan);
            }
            return Applied::Done;
        }
        let Some(listing) = f.listing else {
            if f.attrs.is_none() {
                self.remove_path(&p);
            }
            return Applied::Done;
        };
        let mut cur = self.current_children(&p);
        for r in &listing.ents {
            let name = &listing.names[r.name_off as usize..][..r.name_len as usize];
            let child = join(&p, name);
            let now = OEnt::new(name, r.kind, r.size, r.mtime);
            match cur.remove(name) {
                None => {
                    let scan = f.scans.remove(&child);
                    self.add_new(child, now, scan)
                }
                Some(Some(c)) => {
                    let c = c as usize;
                    let was_kind = self.base.kind()[c];
                    if was_kind & SHAPE != now.kind & SHAPE {
                        self.kill_subtree(c as u32);
                        // Entries added under it since the base was built.
                        self.drop_over_subtree(&child);
                        let scan = f.scans.remove(&child);
                        self.add_new(child, now, scan);
                    } else if now.kind & 3 != KIND_DIR && (self.base.size_raw()[c] != enc_size(now.size) || self.base.mtime()[c] != now.mtime) {
                        self.kill(c as u32);
                        self.put(child, now);
                    }
                }
                Some(None) => {
                    let old = self.over[&child];
                    if old.kind & SHAPE != now.kind & SHAPE {
                        self.drop_over_subtree(&child);
                        let scan = f.scans.remove(&child);
                        self.add_new(child, now, scan);
                    } else if (old.kind, old.size, old.mtime) != (now.kind, now.size, now.mtime) {
                        self.put(child, now);
                    }
                }
            }
        }
        for (name, c) in cur {
            let child = join(&p, &name);
            match c {
                Some(c) => {
                    if self.base.kind()[c as usize] & 3 == KIND_DIR {
                        // Entries added under it since the base was built
                        // go with it.
                        self.drop_over_subtree(&child);
                        self.trees.push(child);
                    }
                    self.kill_subtree(c)
                }
                None => {
                    if self.over.get(&child).is_some_and(|o| o.kind & 3 == KIND_DIR) {
                        self.trees.push(child.clone());
                    }
                    self.drop_over_subtree(&child)
                }
            }
        }
        Applied::Done
    }

    /// Add an entry and, if it is a directory, its subtree (`scan`, read
    /// ahead by `fetch`; scanned here if missing).
    fn add_new(&mut self, path: Vec<u8>, e: OEnt, scan: Option<Vec<Listing>>) {
        let is_dir = e.kind & 3 == KIND_DIR && e.kind & FLAG_MOUNT == 0;
        self.put(path.clone(), e);
        if !is_dir {
            return;
        }
        self.trees.push(path.clone());
        let ls = scan.unwrap_or_else(|| walk::scan(&path, 4));
        for_each_path(&ls, &path, |p, r| {
            let e = OEnt::new(&p, r.kind, r.size, r.mtime);
            self.put(p, e);
        });
    }

    /// Folders whose listing may have changed since `since` (a wall-clock
    /// second): adding, removing or renaming an entry bumps its folder's
    /// mtime. One lstat per folder, in parallel, under a read lock;
    /// relisting the few that changed is `apply_dir`'s job. This is how
    /// changes made while nobody watched (or lost to an overflow) are found
    /// without crawling the whole disk.
    /// Only folders `keep` accepts are checked.
    pub fn changed_dirs(&self, since: u32, keep: &(dyn Fn(&[u8]) -> bool + Sync)) -> Vec<Vec<u8>> {
        use rayon::prelude::*;
        let idx = &self.base;
        let de = idx.dir_entry();
        let check = |p: &[u8]| keep(p) && !walk::blocked(p) && lstat(p).is_some_and(|o| o.mtime >= since);
        let pool = stat_pool();
        let mut out: Vec<Vec<u8>> = pool.install(|| {
            (1..idx.d)
                .into_par_iter()
                .with_min_len(1024)
                .filter(|&k| !self.is_dead(de[k]))
                .map_init(Vec::new, |buf, k| {
                    idx.path(de[k] as usize, buf);
                    check(buf).then(|| buf.clone())
                })
                .flatten()
                .collect()
        });
        out.extend(self.over.iter().filter(|(p, o)| o.kind & 3 == KIND_DIR && o.kind & FLAG_MOUNT == 0 && check(p)).map(|(p, _)| p.clone()));
        if lstat(b"/").is_some_and(|o| o.mtime >= since) {
            out.push(b"/".to_vec());
        }
        out.sort();
        out
    }

    /// Everything alive, as listings for Index::build.
    pub fn to_listings(&self) -> Vec<Listing> {
        let mut by_parent: HashMap<&[u8], Vec<(&[u8], OEnt)>> = HashMap::new();
        for (k, v) in &self.over {
            let cut = k.iter().rposition(|&b| b == b'/').unwrap_or(0);
            let parent: &[u8] = if cut == 0 { b"/" } else { &k[..cut] };
            by_parent.entry(parent).or_default().push((&k[cut + 1..], *v));
        }
        let mut out = Vec::new();
        // (base dir id if alive in base, path, listing id)
        let mut stack: Vec<(Option<u32>, Vec<u8>, u32)> = vec![(Some(0), b"/".to_vec(), 0)];
        let mut next = 1u32;
        while let Some((bd, path, id)) = stack.pop() {
            let mut l = Listing { id, names: Vec::new(), ents: Vec::new() };
            let mut push = |l: &mut Listing, name: &[u8], kind: u8, size: u64, mtime: u32, child_path: Option<Vec<u8>>, bdir: Option<u32>| {
                let child = match child_path {
                    Some(cp) => {
                        let c = next;
                        next += 1;
                        stack.push((bdir, cp, c));
                        c
                    }
                    None => NONE,
                };
                l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child });
                l.names.extend_from_slice(name);
            };
            if let Some(d) = bd {
                for c in self.base.children(d) {
                    if self.is_dead(c as u32) {
                        continue;
                    }
                    let name = self.base.name(c);
                    let k = self.base.kind()[c];
                    let sub = self.base.dir_of(c as u32);
                    let cp = sub.map(|_| join(&path, name));
                    push(&mut l, name, k, self.base.size_of(c), self.base.mtime()[c], cp, sub);
                }
            }
            if let Some(kids) = by_parent.get(path.as_slice()) {
                for &(name, o) in kids {
                    let descend = o.kind & 3 == KIND_DIR && o.kind & FLAG_MOUNT == 0;
                    push(&mut l, name, o.kind, o.size, o.mtime, descend.then(|| join(&path, name)), None);
                }
            }
            out.push(l);
        }
        out
    }
}

/// Threads for bulk lstat: path lookups scale further than opens do.
pub fn stat_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new().num_threads(12).build().unwrap()
}

/// Visit every entry of a scan with its full path.
pub fn for_each_path(ls: &[Listing], root: &[u8], mut f: impl FnMut(Vec<u8>, &RawEnt)) {
    let mut by_id: HashMap<u32, &Listing> = HashMap::with_capacity(ls.len());
    for l in ls {
        by_id.insert(l.id, l);
    }
    let mut stack = vec![(0u32, root.to_vec())];
    while let Some((id, path)) = stack.pop() {
        let Some(l) = by_id.get(&id) else { continue };
        for r in &l.ents {
            let p = join(&path, &l.names[r.name_off as usize..][..r.name_len as usize]);
            if r.child != NONE {
                stack.push((r.child, p.clone()));
            }
            f(p, r);
        }
    }
}

pub(crate) fn normalize(path: &[u8]) -> Vec<u8> {
    let mut p = path.to_vec();
    while p.len() > 1 && p.last() == Some(&b'/') {
        p.pop();
    }
    p
}

pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
    p.extend_from_slice(dir);
    if dir != b"/" {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// Key range holding everything strictly under `path`.
fn subtree_bounds(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lo = join(path, b"");
    let mut hi = lo.clone();
    *hi.last_mut().unwrap() += 1; // '/' + 1 == '0'
    (lo, hi)
}

/// What a path is now, not following it. The virtual root "/" is a folder.
pub fn lstat(path: &[u8]) -> Option<OEnt> {
    if path == b"/" {
        return Some(OEnt::new(path, KIND_DIR, 0, 0));
    }
    let s = crate::sys::stat(path)?;
    Some(OEnt::new(path, walk::kind_of(s.attrs, s.tag), s.size, s.mtime))
}

/// A checkpoint file's first bytes, with the format's version.
const CHECKPOINT: &[u8; 8] = b"SCRYCKP1";

/// Little-endian values off the front of a byte slice; None past its end.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}
