//! Tests against the real file system, in throwaway folders under %TEMP%:
//! the crawl, the live diff, change notifications, the change journal and
//! content search, the parts that talk to Windows.

use crate::content::{self, Content, Grep};
use crate::index::Index;
use crate::live::{Live, join};
use crate::query::{GrepMode, Query, Searcher, now_secs};
use crate::walk::{self, FLAG_HIDDEN, FLAG_MOUNT, KIND_DIR, KIND_FILE, Listing, NONE, RawEnt};
use crate::{paths, watch};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A folder of its own under %TEMP%, removed on drop.
struct Tree {
    root: PathBuf,
    /// `root` as an index path.
    ip: Vec<u8>,
}

impl Tree {
    fn new(name: &str) -> Tree {
        let root = std::env::temp_dir().join(format!("fsearch-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // %TEMP% may be spelled with 8.3 names; the index holds real ones.
        let ip = paths::from_user(&root.to_string_lossy(), "").unwrap();
        Tree { root: paths::to_native(&ip), ip }
    }

    fn write(&self, rel: &str, data: &str) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, data).unwrap();
        p
    }

    fn at(&self, rel: &str) -> Vec<u8> {
        rel.split('/').fold(self.ip.clone(), |p, c| join(&p, c.as_bytes()))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The disk as far as a test can tell: the chain of folders from "/" down
/// to `root`, then a scan of `root`. Index paths come out absolute.
fn listings(root: &[u8]) -> Vec<Listing> {
    let mut ls = walk::scan(root, 2);
    let comps: Vec<&[u8]> = root.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let shift = comps.len() as u32;
    for l in &mut ls {
        l.id += shift;
        for e in &mut l.ents {
            if e.child != NONE {
                e.child += shift;
            }
        }
    }
    for (k, c) in comps.iter().enumerate() {
        let ent = RawEnt { name_off: 0, name_len: c.len() as u16, kind: KIND_DIR, size: 0, mtime: 0, child: k as u32 + 1 };
        ls.push(Listing { id: k as u32, names: c.to_vec(), ents: vec![ent] });
    }
    ls
}

fn live_of(t: &Tree) -> Live {
    Live::new(Index::build(listings(&t.ip), 1, now_secs(), std::slice::from_ref(&t.ip)))
}

/// Native paths of the hits for `q`, best first.
fn find(live: &Live, q: &str) -> Vec<String> {
    let q = Query::parse(q, r"C:\").unwrap();
    Searcher { live }
        .search(&q)
        .into_iter()
        .map(|h| {
            let p = h.over.unwrap_or_else(|| {
                let mut p = Vec::new();
                live.base.path(h.idx as usize, &mut p);
                p
            });
            paths::display(&p)
        })
        .collect()
}

fn ent<'a>(l: &'a Listing, name: &[u8]) -> &'a RawEnt {
    l.ents
        .iter()
        .find(|e| &l.names[e.name_off as usize..][..e.name_len as usize] == name)
        .unwrap_or_else(|| panic!("no {}", String::from_utf8_lossy(name)))
}

#[test]
fn crawl_sees_kinds_flags_and_names() {
    use std::os::windows::fs::OpenOptionsExt;
    let t = Tree::new("walk");
    t.write(r"sub\five.txt", "12345");
    std::fs::OpenOptions::new().write(true).create_new(true).attributes(0x2).open(t.root.join("secret.txt")).unwrap();
    t.write("café 中文.txt", "");
    // A lone surrogate is a legal Windows name.
    std::fs::write(t.root.join(std::ffi::OsString::from_wide(&[0x6C, 0xD800, 0x2E, 0x74])), "").unwrap();
    junction(&t.root.join("jn"), &t.root.join("sub"));

    let ls = walk::scan(&t.ip, 2);
    let root = ls.iter().find(|l| l.id == 0).unwrap();
    let sub = ent(root, b"sub");
    assert_eq!(sub.kind, KIND_DIR);
    assert_ne!(sub.child, NONE);
    let jn = ent(root, b"jn");
    assert_eq!(jn.kind, KIND_DIR | FLAG_MOUNT, "a junction is a folder we don't enter");
    assert_eq!(jn.child, NONE);
    assert_eq!(ent(root, b"secret.txt").kind, KIND_FILE | FLAG_HIDDEN);
    ent(root, "café 中文.txt".as_bytes());
    ent(root, &[0x6C, 0xED, 0xA0, 0x80, 0x2E, 0x74]);
    let five = ent(ls.iter().find(|l| l.id == sub.child).unwrap(), b"five.txt");
    assert_eq!((five.kind, five.size), (KIND_FILE, 5));
    assert!(five.mtime >= now_secs() - 60);
    assert_eq!(ls.len(), 2, "the junction's target is listed once, as sub");

    let one = walk::list_one(&t.at("sub")).unwrap();
    ent(&one, b"five.txt");
    assert_eq!(crate::live::lstat(&t.at("jn")).unwrap().kind, KIND_DIR | FLAG_MOUNT);
    assert_eq!(crate::live::lstat(&t.at("sub/five.txt")).unwrap().size, 5);
    assert!(crate::live::lstat(&t.at("nope")).is_none());
}

#[test]
fn index_finds_by_name_folder_and_filters() {
    let t = Tree::new("index");
    t.write(r"proj\src\main.rs", "fn main() {}");
    t.write(r"proj\notes\zebra stripes.md", "");
    t.write(r"other\main.rs", "");
    let live = live_of(&t);
    let main = t.root.join(r"proj\src\main.rs").to_string_lossy().into_owned();
    assert_eq!(find(&live, "proj main").first(), Some(&main), "folder tokens match ancestors");
    assert_eq!(find(&live, "mian.rs").len(), 2, "one typo is forgiven");
    let scoped = format!("main in:{}", t.root.join("other").display());
    assert_eq!(find(&live, &scoped), [t.root.join(r"other\main.rs").to_string_lossy()]);
    assert_eq!(find(&live, r"src\main").first(), Some(&main), "a backslash splits tokens too");
    assert_eq!(find(&live, "ext:md zebra").len(), 1);
    assert_eq!(find(&live, "kind:dir notes").len(), 1);
    let re = format!("path:{}/proj/src main", t.ip.strip_prefix(b"/").map(|p| String::from_utf8_lossy(p).replace('.', r"\.")).unwrap());
    assert_eq!(find(&live, &re), std::slice::from_ref(&main), "path: sees C:/... with forward slashes");
}

#[test]
fn live_diff_follows_create_rename_delete() {
    let t = Tree::new("live");
    t.write(r"a\one.txt", "1");
    let mut live = live_of(&t);
    assert_eq!(find(&live, "'one.txt").len(), 1);

    t.write(r"a\two_qux.txt", "2");
    live.apply_dir(&t.at("a"), false);
    assert_eq!(find(&live, "two_qux"), [t.root.join(r"a\two_qux.txt").to_string_lossy()]);

    std::fs::rename(t.root.join("a"), t.root.join("b")).unwrap();
    live.apply_dir(&t.ip, false);
    assert_eq!(find(&live, "two_qux"), [t.root.join(r"b\two_qux.txt").to_string_lossy()]);
    assert_eq!(find(&live, "'one.txt"), [t.root.join(r"b\one.txt").to_string_lossy()], "base entries move with their folder");

    std::fs::remove_dir_all(t.root.join("b")).unwrap();
    live.apply_dir(&t.ip, false);
    assert!(find(&live, "two_qux").is_empty());
    assert!(find(&live, "'one.txt").is_empty());
}

fn junction(link: &Path, target: &Path) {
    let ok = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
    assert!(ok.status.success(), "mklink /J failed");
}

#[test]
fn junction_swaps_never_index_a_target_twice() {
    let t = Tree::new("swap");
    t.write(r"x\a.txt", "");
    t.write(r"elsewhere\b.txt", "");
    let mut live = live_of(&t);
    // A folder moved away, a junction left in its place.
    std::fs::remove_dir_all(t.root.join("x")).unwrap();
    junction(&t.root.join("x"), &t.root.join("elsewhere"));
    // A change queued for x itself must not list it through the junction.
    live.apply_dir(&t.at("x"), false);
    live.apply_dir(&t.ip, false);
    assert!(find(&live, "'a.txt").is_empty());
    assert_eq!(find(&live, "'b.txt"), [t.root.join(r"elsewhere\b.txt").to_string_lossy()]);
    // And back: the junction replaced by a real folder with files in it.
    std::fs::remove_dir(t.root.join("x")).unwrap();
    t.write(r"x\real.txt", "");
    live.apply_dir(&t.ip, false);
    assert_eq!(find(&live, "'real.txt"), [t.root.join(r"x\real.txt").to_string_lossy()]);
}

#[test]
fn names_windows_would_trim_open_themselves() {
    use std::io::Read;
    let t = Tree::new("trim");
    let verbatim = |name: &str| PathBuf::from(format!(r"\\?\{}\{name}", t.root.display()));
    std::fs::write(verbatim("trail."), "dot").unwrap();
    std::fs::write(verbatim("trail"), "plain").unwrap();
    let mut s = String::new();
    content::open_regular(&t.at("trail.")).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "dot");
}

#[test]
fn config_reads_with_any_bom() {
    let t = Tree::new("config");
    let json = format!(r#"{{"roots": ["{}"]}}"#, t.root.display().to_string().replace('\\', r"\\"));
    let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(json.encode_utf16().flat_map(u16::to_le_bytes)).collect();
    for bytes in [json.as_bytes().to_vec(), [&[0xEF, 0xBB, 0xBF][..], json.as_bytes()].concat(), utf16] {
        std::fs::write(t.root.join("config.json"), bytes).unwrap();
        assert_eq!(crate::server::options(&t.root, r"C:\").roots, std::slice::from_ref(&t.root));
    }
}

#[test]
fn pipe_serves_this_user() {
    use std::io::{BufRead, BufReader, Write};
    let name = format!(r"\\.\pipe\fsearch-test-{}", std::process::id());
    let listening = crate::sys::pipe_listen(&name, true).unwrap();
    let server = std::thread::spawn(move || {
        let conn = crate::sys::pipe_accept(listening).unwrap();
        let mut line = String::new();
        BufReader::new(&conn).read_line(&mut line).unwrap();
        writeln!(&conn, "echo {}", line.trim()).unwrap();
    });
    let mut c = crate::sys::pipe_connect(&name).unwrap();
    writeln!(c, "hi").unwrap();
    let mut line = String::new();
    BufReader::new(&c).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "echo hi");
    server.join().unwrap();
}

#[test]
fn notifications_name_the_folder() {
    let t = Tree::new("watch");
    let (tx, rx) = std::sync::mpsc::channel();
    let _stream = watch::watch(watch::current_event_id(), 0.02, tx);
    std::thread::sleep(Duration::from_millis(300));
    t.write(r"deep\er\x.txt", "x");
    let want = t.at("deep/er");
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if let Ok(batch) = rx.recv_timeout(Duration::from_millis(200))
            && batch.iter().any(|e| e.path == want)
        {
            return;
        }
    }
    panic!("no event for {}", paths::display(&want));
}

#[test]
fn journal_names_changed_folders() {
    let t = Tree::new("journal");
    let since = now_secs() - 5;
    t.write(r"j\changed.txt", "x");
    let drive = t.ip[1];
    match watch::journal_dirs(drive, since) {
        // FAT, exFAT, ReFS, or a journal younger than 5 seconds.
        None => eprintln!("no change journal on {}: skipped", drive as char),
        Some(dirs) => assert!(dirs.contains(&t.at("j")), "{} not among {} folders", paths::display(&t.at("j")), dirs.len()),
    }
}

#[test]
fn content_search_reads_crlf_text() {
    let t = Tree::new("content");
    t.write(r"src\lib.rs", "pub fn apply_dir() {}\r\n// Zebra crossing here\r\n");
    t.write("notes.md", "nothing to see\n");
    t.write(r"node_modules\dep\index.js", "zebra");
    let idx = Tree::new("content-idx");
    let live = live_of(&t);
    let roots = std::slice::from_ref(&t.ip);
    let mut c = Content::open(idx.root.join("content"));
    let todo = c.diff(content::wants(&live, roots, &[], roots));
    assert_eq!(todo.len(), 2, "node_modules is skipped");
    for b in todo.batches() {
        let (dir, id) = (c.dir.clone(), c.alloc_id());
        c.push(content::build_segment(&dir, id, &todo, b).unwrap());
    }
    let q = Query::parse("", r"C:\").unwrap();
    let hits = |pattern: &str, mode| c.search(&Grep::new(pattern, mode).unwrap(), &q).files;
    let r = hits("zebra", GrepMode::Literal);
    assert_eq!(r.len(), 1, "smart case: lowercase matches Zebra");
    assert_eq!((r[0].lines[0].0, r[0].lines[0].1.as_str()), (2, "// Zebra crossing here"));
    assert_eq!(hits("Zebra", GrepMode::Literal).len(), 1);
    assert!(hits("ZEBRA", GrepMode::Literal).is_empty(), "uppercase is case-sensitive");
    assert_eq!(hits(r"here$", GrepMode::Regex).len(), 1, "$ before \\r\\n");
    assert_eq!(paths::display(&hits("apply_dir", GrepMode::Symbol)[0].path), t.root.join(r"src\lib.rs").to_string_lossy());
    assert!(content::open_regular(&t.at("src")).is_none(), "a folder is not a regular file");
}

#[test]
fn query_reads_windows_paths() {
    let q = Query::parse(r#"in:"C:\Windows\System32" drivers\etc"#, r"C:\Users\me").unwrap();
    assert_eq!(q.scope.as_deref(), Some(&b"/C:/Windows/System32"[..]));
    assert_eq!(q.tokens.iter().map(|t| String::from_utf8_lossy(&t.text).into_owned()).collect::<Vec<_>>(), ["drivers", "etc"]);
    let q = Query::parse("in:c:/windows", "").unwrap();
    assert_eq!(q.scope.as_deref(), Some(&b"/C:/Windows"[..]), "the real spelling of an existing folder");
    assert_eq!(paths::from_user("~", r"C:\Windows"), Some(b"/C:/Windows".to_vec()));
    assert!(Path::new(&paths::display(b"/C:/Windows")).is_dir());
}
