//! The engine: owns the live name index, follows file system changes, keeps
//! the content index current, and answers searches. The daemon runs one; so
//! can any app that links this crate.

use crate::content::{self, Content, Grep, GrepResult};
use crate::index::Index;
use crate::live::{Applied, Live};
use crate::query::{Query, Searcher};
use crate::watch::{self, DROPPED, HISTORY_DONE, MUST_SCAN_SUBDIRS};
use crate::{paths, sys, walk};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

// Every search scans the whole overlay (~1 ms per 100k entries), and a
// follower starting up replays everything since the save, so fold it into
// the base once it grows: ~1 s of CPU and a few hundred MB written, about
// hourly on a busy disk. Otherwise only twice a day.
const COMPACT_PENDING: usize = 50_000;
const COMPACT_EVERY: Duration = Duration::from_secs(12 * 3600);
/// The first crawl waits on cold folder records more than on the CPU, so
/// it keeps more reads in flight than there are cores.
const SCAN_THREADS: usize = 16;
const CONTENT_QUIET: Duration = Duration::from_secs(2);
const CONTENT_MAX_WAIT: Duration = Duration::from_secs(300);
/// How often a follower checks whether it can take over or reload.
const FOLLOW_EVERY: Duration = Duration::from_secs(10);
/// Seconds before the last known-good moment that a relist also covers.
const SYNC_MARGIN: u32 = 120;
/// How often what changed since the last full save is checkpointed
/// (`Live::checkpoint`), so a restart resumes from minutes ago: the NTFS
/// change journal may hold only a few hours of a busy disk.
const CHECKPOINT_EVERY: Duration = Duration::from_secs(5 * 60);
const CHECKPOINT: &str = "checkpoint.bin";

pub struct Options {
    /// Where the index lives (`index.bin`, `content/`).
    pub dir: PathBuf,
    /// Your user folder (`C:\Users\you`): what `~` means in queries.
    pub home: String,
    /// Your folders: their text files are indexed for content search, and
    /// what is in them ranks first. Empty means just `home`.
    pub roots: Vec<PathBuf>,
    /// Folders never to open.
    pub skip: Vec<PathBuf>,
}

/// One name-search result.
pub struct Found {
    pub path: PathBuf,
    /// `walk::KIND_*` in the low 2 bits, `walk::FLAG_*` above.
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    pub score: i32,
}

#[derive(serde::Serialize)]
pub struct Status {
    #[serde(skip)]
    pub ready: bool,
    pub entries: usize,
    pub dirs: usize,
    pub overlay: usize,
    pub removed: usize,
    pub event_id: u64,
    pub index_bytes: usize,
    pub content_docs: usize,
    pub content_segments: usize,
    pub content_bytes: usize,
    pub content_pending: usize,
    /// Your folders (content search scope).
    pub roots: Vec<String>,
    /// Running elevated, with SeBackupPrivilege: folders this account
    /// cannot read are indexed too.
    pub elevated: bool,
    /// Writes the index files (false: following another process's).
    pub owner: bool,
}

#[derive(Clone)]
pub struct Engine {
    s: Arc<Shared>,
}

struct Shared {
    live: RwLock<Option<Live>>,
    content: RwLock<Content>,
    home: String,
    /// Your folders, as index paths.
    roots: Vec<Vec<u8>>,
    dir: PathBuf,
    /// Wakes the apply loop; an empty batch is a no-op wake-up.
    wake: Sender<Vec<watch::Event>>,
    save_requested: AtomicBool,
    /// (dirs, trees) for the content worker to re-sync.
    content_tx: Sender<(Vec<Vec<u8>>, Vec<Vec<u8>>)>,
    content_rx: Mutex<Option<Receiver<(Vec<Vec<u8>>, Vec<Vec<u8>>)>>>,
    content_pending: AtomicUsize,
    /// Holding `lock`: this engine writes the index files. Another process
    /// may own them (the daemon, an app); then this one follows: it reads
    /// the saved index, keeps it live in memory, and takes over when the
    /// owner goes away.
    owner: AtomicBool,
    lock: std::fs::File,
    stream: Mutex<Option<watch::Stream>>,
    /// The stream is still catching up (until HISTORY_DONE).
    replaying: AtomicBool,
    /// Follower: content dir mtime when its segments were last opened.
    content_seen: Mutex<Option<std::time::SystemTime>>,
    /// One checkpoint written at a time.
    checkpointing: Mutex<()>,
}

/// One line to stderr (the daemon's log); a full disk must not take the
/// caller down with it.
pub fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{} {}", crate::query::now_secs(), msg.as_ref());
}

/// Whether this process indexes past folder permissions: true when it runs
/// elevated (SeBackupPrivilege could be turned on), unless SCRY_RESTRICT
/// is set.
pub fn elevated() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCRY_RESTRICT").is_none() && sys::enable_backup_privilege())
}

/// Searches run here, opted out of EcoQoS: a windowless process's threads
/// may otherwise be put on efficiency cores.
fn search_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new().thread_name(|i| format!("scry-search-{i}")).start_handler(|_| sys::interactive_thread()).build().unwrap()
    })
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn");
}

impl Engine {
    /// Start indexing in the background and return at once; searches answer
    /// `Err` until the index is loaded (or, on the very first run, built).
    pub fn start(opts: Options) -> Result<Engine, String> {
        std::fs::create_dir_all(&opts.dir).map_err(|e| e.to_string())?;
        // One writer per index: a second one would race index writes. The
        // lock dies with the process.
        let lock =
            std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(opts.dir.join("daemon.lock")).map_err(|e| e.to_string())?;
        let owner = lock.try_lock().is_ok();
        if elevated() {
            log("elevated: indexing folders this account cannot read too");
        }
        let skip: Vec<Vec<u8>> = opts.skip.iter().filter_map(|p| paths::from_native(p)).collect();
        if !skip.is_empty() {
            let _ = walk::SKIP.set(skip);
        }
        let home = paths::from_native(Path::new(&opts.home)).ok_or_else(|| format!("home {} is not on a drive", opts.home))?;
        let mut roots: Vec<Vec<u8>> = opts.roots.iter().filter_map(|p| paths::from_native(p)).collect();
        if roots.is_empty() {
            roots.push(home);
        }
        roots.sort();
        roots.dedup();
        let dir = opts.dir;
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctx, crx) = std::sync::mpsc::channel();
        let content = if owner { Content::open(dir.join("content")) } else { Content::open_shared(dir.join("content")) };
        let shared = Arc::new(Shared {
            live: RwLock::new(None),
            content: RwLock::new(content),
            home: opts.home,
            roots,
            dir,
            wake: tx,
            save_requested: AtomicBool::new(false),
            content_tx: ctx,
            content_rx: Mutex::new(Some(crx)),
            content_pending: AtomicUsize::new(0),
            owner: AtomicBool::new(owner),
            lock,
            stream: Mutex::new(None),
            replaying: AtomicBool::new(true),
            content_seen: Mutex::new(None),
            checkpointing: Mutex::new(()),
        });
        let base = Index::load(&shared.dir.join("index.bin"));
        let since = match &base {
            Some(b) if b.event_id != 0 => b.event_id,
            _ => watch::current_event_id(),
        };
        if owner {
            // Watch before scanning so nothing that changes mid-scan is
            // missed; applying it afterwards is harmless (diffs are idempotent).
            shared.watch(since);
        }
        let s = shared.clone();
        spawn("scry-apply", move || {
            let base = match base {
                Some(b) => {
                    log(format!("loaded {} entries, catching up on changes since {}", b.n, b.synced_at));
                    if !owner {
                        s.watch(b.event_id);
                    }
                    b
                }
                None if owner => full_build(&s, since),
                None => {
                    // The owner is building it; follow once it exists.
                    let b = wait_for_index(&s.dir);
                    s.watch(b.event_id);
                    b
                }
            };
            let mut live = Live::new(base);
            if owner {
                resume(&s, &mut live);
            }
            *s.live.write().unwrap() = Some(live);
            if owner {
                forget_skipped(&s);
                start_content(&s);
            }
            apply_loop(&s, rx);
        });
        Ok(Engine { s: shared })
    }

    pub fn home(&self) -> &str {
        &self.s.home
    }

    /// Name search.
    pub fn search(&self, q: &Query) -> Result<Vec<Found>, String> {
        let g = self.s.live.read().unwrap();
        let Some(live) = g.as_ref() else { return Err(INDEXING.into()) };
        let mut p = Vec::new();
        Ok(search_pool()
            .install(|| Searcher { live }.search(q))
            .into_iter()
            .map(|h| {
                let (kind, size, mtime) = match &h.over {
                    Some(path) => {
                        let o = live.over[path];
                        p = path.clone();
                        (o.kind, o.size, o.mtime)
                    }
                    None => {
                        let i = h.idx as usize;
                        live.base.path(i, &mut p);
                        (live.base.kind()[i], live.base.size_of(i), live.base.mtime()[i])
                    }
                };
                Found { path: paths::to_native(&p), kind, size, mtime, score: h.score }
            })
            .collect())
    }

    /// Content search: `g` is the pattern, `q` narrows which files are read.
    /// The bool says whether the content index answered (false: files were
    /// picked from the name index and read, for folders it doesn't cover).
    pub fn grep(&self, q: &Query, g: &Grep) -> Result<(GrepResult, bool), String> {
        let indexed = q.scope.as_ref().is_none_or(|s| content::in_scope(s, &self.s.roots));
        if indexed {
            return Ok((search_pool().install(|| self.s.content.read().unwrap().search(g, q)), true));
        }
        // Pick files under the lock, read them after releasing it: reading can
        // be slow and a waiting writer would stall every other query.
        let paths = {
            let l = self.s.live.read().unwrap();
            let Some(live) = l.as_ref() else { return Err(INDEXING.into()) };
            search_pool().install(|| content::scan_paths(live, q.clone_for_scan()))
        };
        Ok((content::verify(g, &paths, q.limit), false))
    }

    pub fn status(&self) -> Status {
        let l = self.s.live.read().unwrap();
        let c = self.s.content.read().unwrap();
        Status {
            ready: l.is_some(),
            entries: l.as_ref().map_or(0, |l| l.base.n),
            dirs: l.as_ref().map_or(0, |l| l.base.d),
            overlay: l.as_ref().map_or(0, |l| l.over.len()),
            removed: l.as_ref().map_or(0, |l| l.dead_count),
            event_id: l.as_ref().map_or(0, |l| l.event_id),
            index_bytes: l.as_ref().map_or(0, |l| l.base.bytes()),
            content_docs: c.docs(),
            content_segments: c.segs.len(),
            content_bytes: c.bytes(),
            content_pending: self.s.content_pending.load(Ordering::Relaxed),
            roots: self.s.roots.iter().map(|r| paths::display(r)).collect(),
            elevated: elevated(),
            owner: self.s.owner(),
        }
    }

    /// Save what changed since the last full save (a few MB at most), now:
    /// before the process ends, so the next start resumes from here.
    pub fn checkpoint(&self) {
        self.s.checkpoint();
    }

    /// Compact and save the name index soon (on the background thread).
    pub fn save(&self) {
        self.s.save_requested.store(true, Ordering::Relaxed);
        let _ = self.s.wake.send(Vec::new());
    }

    /// Bring these folders' whole subtrees in line with the disk soon (on
    /// the background thread): folders that were skipped and are not any
    /// more, whose contents the index never saw.
    pub fn rescan(&self, dirs: &[PathBuf]) {
        let id = watch::current_event_id();
        let events: Vec<watch::Event> =
            dirs.iter().filter_map(|d| paths::from_native(d)).map(|path| watch::Event { path, flags: MUST_SCAN_SUBDIRS, id }).collect();
        if !events.is_empty() {
            let _ = self.s.wake.send(events);
        }
    }
}

/// Folders skipped now that were indexed before (`skip` grew since the
/// last crawl): what is under them goes.
fn forget_skipped(s: &Shared) {
    let Some(skip) = walk::SKIP.get() else { return };
    let n: usize = {
        let mut g = s.live.write().unwrap();
        let live = g.as_mut().unwrap();
        skip.iter().map(|p| live.forget(p)).sum()
    };
    if n > 0 {
        log(format!("left out {n} entries under skipped folders"));
    }
}

const INDEXING: &str = "indexing (the first run lists every fixed drive, about a minute)";

impl Shared {
    fn owner(&self) -> bool {
        self.owner.load(Ordering::Relaxed)
    }

    /// Write the checkpoint (`Live::checkpoint`) aside and rename it into
    /// place. Only the owner: it is of the index the owner saved.
    fn checkpoint(&self) {
        if !self.owner() {
            return;
        }
        let _one = self.checkpointing.lock().unwrap();
        let Some(bytes) = self.live.read().unwrap().as_ref().map(Live::checkpoint) else { return };
        let path = self.dir.join(CHECKPOINT);
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path)) {
            log(format!("checkpoint failed: {e}"));
        }
    }

    /// (Re)start watching the drives for an index current as of `since`,
    /// replacing any old watch.
    fn watch(&self, since: u64) {
        self.replaying.store(true, Ordering::Relaxed);
        let new = watch::watch(since, 0.1, self.wake.clone());
        *self.stream.lock().unwrap() = Some(new);
    }

    /// A follower picks up what the owner wrote: a newer name-index save
    /// (catching up from it) and content segment changes.
    fn follow(&self) {
        let path = self.dir.join("index.bin");
        let saved = Index::saved_event_id(&path).unwrap_or(0);
        let ours = self.live.read().unwrap().as_ref().map_or(0, |l| l.base.event_id);
        if saved > ours
            && let Some(base) = Index::load(&path)
        {
            log(format!("following the owner's save: {} entries, catching up since {}", base.n, base.synced_at));
            self.watch(base.event_id);
            *self.live.write().unwrap() = Some(Live::new(base));
        }
        let cdir = self.dir.join("content");
        let changed = std::fs::metadata(&cdir).and_then(|m| m.modified()).ok();
        let mut seen = self.content_seen.lock().unwrap();
        if changed != *seen {
            *seen = changed;
            *self.content.write().unwrap() = Content::open_shared(cdir);
        }
    }
}

fn start_content(s: &Arc<Shared>) {
    let Some(rx) = s.content_rx.lock().unwrap().take() else { return };
    // A folder that is no longer one of yours: what was read from it goes.
    let gone = s.content.write().unwrap().forget_outside(&s.roots);
    if gone > 0 {
        log(format!("content: dropped {gone} files no longer in your folders"));
    }
    // Reconcile your folders once (cheap when nothing changed), then follow
    // along with the name index's changes.
    let _ = s.content_tx.send((Vec::new(), s.roots.clone()));
    let s = s.clone();
    spawn("scry-content", move || content_loop(&s, rx));
}

/// A follower takes over the index files once their owner is gone.
fn try_upgrade(s: &Arc<Shared>) -> bool {
    if s.owner() {
        return true;
    }
    if s.lock.try_lock().is_err() {
        return false;
    }
    s.owner.store(true, Ordering::Relaxed);
    log("took over the index from a previous owner");
    *s.content.write().unwrap() = Content::open(s.dir.join("content"));
    start_content(s);
    true
}

fn wait_for_index(dir: &Path) -> Index {
    loop {
        if let Some(b) = Index::load(&dir.join("index.bin")) {
            return b;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn content_loop(shared: &Shared, rx: Receiver<(Vec<Vec<u8>>, Vec<Vec<u8>>)>) {
    // Indexing file contents is background work: low CPU, I/O and memory
    // priority keeps it out of the user's way.
    sys::background_thread();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).start_handler(|_| sys::background_thread()).build().unwrap();
    let roots = shared.roots.clone();
    // Per-folder debounce: a folder is processed 2s after its last change,
    // or 5 min after its first pending one if it never goes quiet. A file you
    // save lands in ~2s; files apps rewrite every second (state, logs) cost
    // one reindex per 5 min instead of one per event batch.
    let mut pending: HashMap<(Vec<u8>, bool), (Instant, Instant)> = HashMap::new();
    loop {
        let wait = if pending.is_empty() { Duration::from_secs(3600) } else { Duration::from_millis(250) };
        match rx.recv_timeout(wait) {
            Ok(first) => {
                let now = Instant::now();
                for (d, t) in std::iter::once(first).chain(rx.try_iter()) {
                    for key in d.into_iter().map(|p| (p, false)).chain(t.into_iter().map(|p| (p, true))) {
                        // Most of the disk's churn (AppData, Windows) is outside the indexed area.
                        if content::in_scope(&key.0, &roots) || (key.1 && content::holds_root(&key.0, &roots)) {
                            pending.entry(key).and_modify(|e| e.1 = now).or_insert((now, now));
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let ripe: Vec<(Vec<u8>, bool)> = pending
            .iter()
            .filter(|(_, (first, last))| last.elapsed() >= CONTENT_QUIET || first.elapsed() >= CONTENT_MAX_WAIT)
            .map(|(k, _)| k.clone())
            .collect();
        if ripe.is_empty() {
            continue;
        }
        let (mut dirs, mut trees) = (Vec::<Vec<u8>>::new(), Vec::<Vec<u8>>::new());
        for k in ripe {
            pending.remove(&k);
            if k.1 { trees.push(k.0) } else { dirs.push(k.0) }
        }
        let t = Instant::now();
        let wants = {
            let g = shared.live.read().unwrap();
            let Some(live) = g.as_ref() else { continue };
            content::wants(live, &roots, &dirs, &trees)
        };
        let todo = shared.content.write().unwrap().diff(wants);
        if todo.len() == 0 {
            continue;
        }
        let n = todo.len();
        shared.content_pending.store(n, Ordering::Relaxed);
        for batch in todo.batches() {
            let (dir, id) = {
                let mut c = shared.content.write().unwrap();
                (c.dir.clone(), c.alloc_id())
            };
            let len = batch.len();
            if let Some(seg) = pool.install(|| content::build_segment(&dir, id, &todo, batch)) {
                shared.content.write().unwrap().push(seg);
            }
            shared.content_pending.fetch_sub(len, Ordering::Relaxed);
        }
        drop(todo);
        // Keep the segment count small: merge size tiers of 8.
        loop {
            let plan = shared.content.read().unwrap().merge_plan();
            let Some(ids) = plan else { break };
            let (dir, id) = {
                let mut c = shared.content.write().unwrap();
                (c.dir.clone(), c.alloc_id())
            };
            let merged = {
                let c = shared.content.read().unwrap();
                pool.install(|| content::merge(&dir, id, &c.segments(&ids)))
            };
            match merged {
                Some(seg) => shared.content.write().unwrap().replace(&ids, seg),
                None => break,
            }
        }
        if n > 100 {
            log(format!("content: indexed {n} files in {:.2?}", t.elapsed()));
        }
    }
}

fn full_build(shared: &Shared, event_id: u64) -> Index {
    let t = Instant::now();
    let started = crate::query::now_secs();
    let ls = walk::scan(b"/", SCAN_THREADS);
    let idx = Index::build(ls, event_id, started, &shared.roots);
    let path = shared.dir.join("index.bin");
    if let Err(e) = idx.save(&path) {
        log(format!("save failed: {e}"));
    }
    log(format!("indexed {} entries in {:.2?}", idx.n, t.elapsed()));
    // Re-map from the file so the index is clean, evictable page cache
    // rather than anonymous memory.
    Index::load(&path).unwrap_or(idx)
}

fn compact(shared: &Shared) {
    let t = Instant::now();
    let (ls, eid, synced) = {
        let g = shared.live.read().unwrap();
        let live = g.as_ref().unwrap();
        (live.to_listings(), live.event_id, live.synced_at)
    };
    let idx = Index::build(ls, eid, synced, &shared.roots);
    let path = shared.dir.join("index.bin");
    if let Err(e) = idx.save(&path) {
        log(format!("save failed: {e}"));
    }
    let idx = Index::load(&path).unwrap_or(idx);
    let n = idx.n;
    *shared.live.write().unwrap() = Some(Live::new(idx));
    // The saved index holds everything the checkpoint did.
    let _ = std::fs::remove_file(shared.dir.join(CHECKPOINT));
    log(format!("compacted to {n} entries in {:.2?}", t.elapsed()));
}

/// The changes the last checkpoint holds, on top of the saved index: a
/// restart then catches up from minutes ago.
fn resume(s: &Shared, live: &mut Live) {
    let Ok(b) = std::fs::read(s.dir.join(CHECKPOINT)) else { return };
    if live.restore_checkpoint(&b) {
        log(format!("resumed from the checkpoint: {} changes since the save, in sync as of {}", live.over.len() + live.dead_count, live.synced_at));
    }
}

/// Changes went unseen (made while nothing watched, or more at once than
/// the change buffer holds): relist the folders where something changed
/// since we were last in sync. The NTFS change journal names them; on a
/// drive whose journal doesn't reach back that far (or that has none),
/// every folder modified since is relisted, plus the folders of indexed
/// text files edited since (an edit in place doesn't touch its folder).
/// Seconds at worst, instead of recrawling every drive.
fn relist_changed(shared: &Shared, why: &str) {
    let t = Instant::now();
    let started = crate::query::now_secs();
    let synced = shared.live.read().unwrap().as_ref().unwrap().synced_at;
    let from = synced.saturating_sub(SYNC_MARGIN);
    // synced_at 0 (unknown) checks every folder: a full crawl, done in place.
    let (mut dirs, mut unjournaled) = (Vec::new(), Vec::new());
    for d in sys::fixed_drives() {
        let journal = if synced == 0 { Err(String::new()) } else { watch::journal_dirs(d, from) };
        match journal {
            Ok(v) => dirs.extend(v),
            Err(why) => {
                if !why.is_empty() {
                    log(format!("{}: {why}; checking its folders instead", d as char));
                }
                unjournaled.push(vec![b'/', d, b':']);
            }
        }
    }
    let journaled = dirs.len();
    if !unjournaled.is_empty() {
        // A FAT or exFAT root has no mtime to compare: list it anyway.
        dirs.extend(unjournaled.iter().cloned());
        let keep = |p: &[u8]| unjournaled.iter().any(|u| p.starts_with(u) && p.get(u.len()).is_none_or(|&b| b == b'/'));
        dirs.extend(shared.live.read().unwrap().as_ref().unwrap().changed_dirs(from, &keep));
        dirs.extend(shared.content.read().unwrap().changed_dirs(from, &keep));
    }
    dirs.sort();
    dirs.dedup();
    let stat_time = t.elapsed();
    // Disk reads under the read lock, one folder per write, so searches keep
    // answering meanwhile.
    for d in &dirs {
        let f = shared.live.read().unwrap().as_ref().unwrap().fetch(d, false);
        shared.live.write().unwrap().as_mut().unwrap().apply(f);
    }
    let trees = {
        let mut g = shared.live.write().unwrap();
        let live = g.as_mut().unwrap();
        live.synced_at = started;
        std::mem::take(&mut live.trees)
    };
    let n = dirs.len();
    let _ = shared.content_tx.send((dirs, trees));
    let how = match unjournaled.len() {
        0 => format!("{journaled} from the change journal"),
        _ => format!(
            "{journaled} from the change journal, the rest by checking {}",
            unjournaled.iter().map(|u| paths::display(u)).collect::<Vec<_>>().join(" ")
        ),
    };
    log(format!("resynced ({why}): relisted {n} folders changed since {from} ({how}) in {:.2?} ({stat_time:.2?} finding them)", t.elapsed()));
}

/// A drive came or went (a disk attached, a VHD mounted): watch the
/// drives there are now, and relist "/" so the index gains or drops them.
fn check_drives(shared: &Shared) {
    let now = sys::fixed_drives();
    if shared.stream.lock().unwrap().as_ref().is_none_or(|s| s.drives == now) {
        return;
    }
    log(format!("fixed drives are now {}", now.iter().map(|&d| format!("{}:", d as char)).collect::<Vec<_>>().join(" ")));
    shared.watch(watch::current_event_id());
    let _ = shared.wake.send(vec![watch::Event { path: b"/".to_vec(), flags: 0, id: watch::current_event_id() }]);
}

fn apply_loop(shared: &Arc<Shared>, rx: Receiver<Vec<watch::Event>>) {
    let mut last_save = Instant::now();
    let (mut last_checkpoint, mut checkpointed) = (Instant::now(), 0u32);
    let mut last_follow = Instant::now();
    let mut last_drives = Instant::now();
    let ours = paths::from_native(&shared.dir);
    loop {
        let mut events = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        events.extend(rx.try_iter().flatten());
        // The owner wrote the index files: a follower picks that up now
        // rather than at its next periodic check.
        let owner_wrote = events.iter().any(|e| ours.as_ref().is_some_and(|o| e.path.starts_with(o)));
        if !events.is_empty() {
            let mut dirs: HashMap<Vec<u8>, bool> = HashMap::new();
            let (mut max_id, mut root_flags) = (0, 0);
            for e in events {
                max_id = max_id.max(e.id);
                if e.path == b"/" && e.flags & MUST_SCAN_SUBDIRS != 0 {
                    root_flags |= e.flags;
                }
                if e.flags & HISTORY_DONE != 0 {
                    shared.replaying.store(false, Ordering::Relaxed);
                    continue;
                }
                *dirs.entry(crate::live::normalize(&e.path)).or_default() |= e.flags & MUST_SCAN_SUBDIRS != 0;
            }
            let mut rebuild = false;
            let mut trees = Vec::new();
            // Read the disk under the read lock, then apply in memory: a
            // search never waits on a folder listing or a new subtree's scan.
            let fetched: Vec<_> = {
                let g = shared.live.read().unwrap();
                let live = g.as_ref().unwrap();
                dirs.iter().map(|(p, recursive)| live.fetch(p, *recursive)).collect()
            };
            {
                let mut g = shared.live.write().unwrap();
                let live = g.as_mut().unwrap();
                for f in fetched {
                    if let Applied::Rebuild = live.apply(f) {
                        rebuild = true;
                    }
                }
                live.event_id = live.event_id.max(max_id);
                trees.append(&mut live.trees);
            }
            let (rec, flat): (Vec<_>, Vec<_>) = dirs.into_iter().partition(|(_, r)| *r);
            trees.extend(rec.into_iter().map(|(p, _)| p));
            let _ = shared.content_tx.send((flat.into_iter().map(|(p, _)| p).collect(), trees));
            if rebuild {
                let why = if root_flags & DROPPED != 0 {
                    "more changed at once than the change buffer holds"
                } else {
                    "changes made while nothing watched"
                };
                relist_changed(shared, why);
            } else if !shared.replaying.load(Ordering::Relaxed) {
                // Everything up to this batch is applied (a change's event can
                // trail it by the watch latency; relisting keeps a margin).
                shared.live.write().unwrap().as_mut().unwrap().synced_at = crate::query::now_secs();
            }
        }
        if let Some(l) = shared.live.read().unwrap().as_ref() {
            l.names_cache.trim_if_idle(Duration::from_secs(60));
        }
        if last_drives.elapsed() > Duration::from_secs(60) {
            last_drives = Instant::now();
            check_drives(shared);
        }
        if !shared.owner() && (owner_wrote || last_follow.elapsed() > FOLLOW_EVERY) {
            last_follow = Instant::now();
            if !try_upgrade(shared) {
                shared.follow();
            }
        }
        if !shared.owner() {
            continue;
        }
        let (pending, stale, synced) = {
            let g = shared.live.read().unwrap();
            let live = g.as_ref().unwrap();
            (live.over.len() + live.dead_count, live.event_id != live.base.event_id, live.synced_at)
        };
        let asked = shared.save_requested.swap(false, Ordering::Relaxed);
        if asked || pending > COMPACT_PENDING || (stale && last_save.elapsed() > COMPACT_EVERY) {
            compact(shared);
            (last_save, last_checkpoint, checkpointed) = (Instant::now(), Instant::now(), synced);
        } else if synced != checkpointed && last_checkpoint.elapsed() > CHECKPOINT_EVERY {
            shared.checkpoint();
            (last_checkpoint, checkpointed) = (Instant::now(), synced);
        }
    }
}

/// Your user folder (`%USERPROFILE%`), spelled as the file system spells it.
pub fn default_home() -> String {
    let h = std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\".into());
    paths::from_user(&h, &h).map_or(h, |p| paths::display(&p))
}

/// Default data dir: `%LOCALAPPDATA%\Scry`.
pub fn default_dir(home: &str) -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(|| Path::new(home).join(r"AppData\Local"), PathBuf::from).join("Scry")
}
