//! The daemon: one `Engine`, answering JSON lines over a named pipe.
//! `scry stdio` and the CLI are thin clients; so can any app be.

use crate::engine::log;
use crate::walk::{KIND_DIR, KIND_FILE, KIND_LINK};
use crate::{Engine, GrepMode, Options, Query, paths, sys};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The daemon's pipe for an index dir: one per user and index.
pub fn pipe_name(dir: &Path) -> String {
    let h = dir.to_string_lossy().to_lowercase().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    format!(r"\\.\pipe\scry-{h:016x}")
}

/// `config.json` in the index dir, all optional:
/// `{"roots": ["~", "C:\\dev"], "skip": ["D:\\Backups"], "hotkey": "Alt+Shift+Space"}`.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Your folders (see `Options::roots`); `~` is home.
    pub roots: Vec<String>,
    /// Folders never to open.
    pub skip: Vec<String>,
    /// What brings up the search bar (`scryd`).
    pub hotkey: Option<String>,
}

/// Text as Windows editors save it: UTF-8, with or without a BOM, or
/// UTF-16 with one (PowerShell 5's `>`).
fn decode(b: &[u8]) -> Option<String> {
    let utf16 = |b: &[u8], f: fn([u8; 2]) -> u16| String::from_utf16(&b.as_chunks::<2>().0.iter().map(|&c| f(c)).collect::<Vec<_>>()).ok();
    match b {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8(b.to_vec()).ok(),
    }
}

/// The index dir's `config.json` (defaults if there is none, or it is
/// unreadable: that is logged).
pub fn config(dir: &Path) -> Config {
    let parsed = match std::fs::read(dir.join("config.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.to_string()),
        Ok(b) => decode(&b).ok_or_else(|| "not UTF-8 or UTF-16 text".to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())),
    };
    parsed.unwrap_or_else(|e| {
        log(format!("config.json ignored: {e}"));
        Config::default()
    })
}

/// Engine options for an index dir, from its `config.json`.
pub fn options(dir: &Path, home: &str) -> Options {
    let cfg = config(dir);
    let real = |s: &String| {
        let p = paths::from_user(s, home).map(|p| paths::to_native(&p));
        if p.is_none() {
            log(format!("config.json: {s} is not a folder on a drive, ignored"));
        }
        p
    };
    Options {
        dir: dir.to_path_buf(),
        home: home.to_string(),
        roots: cfg.roots.iter().filter_map(real).collect(),
        skip: cfg.skip.iter().filter_map(real).collect(),
    }
}

/// Start `daemon` (the windowless `scryd.exe`) at login, or stop doing
/// so (None).
pub fn login_item(daemon: Option<&Path>) -> bool {
    sys::login_item("scry", daemon.map(|d| format!("\"{}\"", d.display())).as_deref())
}

/// Whether the daemon starts at login.
pub fn has_login_item() -> bool {
    sys::has_login_item("scry")
}

static ON_QUIT: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Call `f` just before the daemon exits because a client asked it to
/// (`scry stop`): to take its icon out of the notification area, say.
pub fn on_quit(f: fn()) {
    let _ = ON_QUIT.set(f);
}

/// Log (stderr) to `f` from now on: a process without a console has none.
pub fn log_to(f: std::fs::File) {
    sys::set_stderr(f);
}

/// The daemon's log, set aside once it passes 8 MB.
pub fn open_log(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("daemon.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 8 << 20) {
        let _ = std::fs::rename(&path, dir.join("daemon.log.old"));
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// Become the daemon for an index dir: one per index. Holding the returned
/// lock is being it; the lock dies with the process. None if another
/// daemon still runs after `wait` (one restarting itself takes a moment to
/// leave). (The engine's own lock decides who writes the index: an app
/// embedding scry may own it while the daemon follows.)
pub fn claim(dir: &Path, wait: Duration) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok();
    let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("socket.lock")).ok()?;
    let t = Instant::now();
    while lock.try_lock().is_err() {
        if t.elapsed() >= wait {
            log("another scry daemon is running");
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(lock)
}

/// Your folders and the skipped ones as the engine takes them from
/// config.json: paths on disk, your user folder when no folder is listed.
pub fn folders(dir: &Path, home: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let o = options(dir, home);
    let roots = if o.roots.is_empty() { vec![PathBuf::from(home)] } else { o.roots };
    (roots, o.skip)
}

/// Write `roots` and `skip` into config.json, keeping its other settings
/// (your user folder as `~`). The daemon reads them when it next starts.
/// Refuses to replace a config.json it cannot read rather than lose it.
pub fn save_folders(dir: &Path, home: &str, roots: &[PathBuf], skip: &[PathBuf]) -> Result<(), String> {
    let path = dir.join("config.json");
    let mut v: Value = match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(b) => decode(&b)
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .ok_or_else(|| format!("{} is not a JSON object; fix or delete it first", path.display()))?,
    };
    let list = |ps: &[PathBuf]| -> Value {
        ps.iter().map(|p| if p == Path::new(home) { "~".to_string() } else { p.display().to_string() }).collect::<Vec<_>>().into()
    };
    v["roots"] = list(roots);
    v["skip"] = list(skip);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap() + "\n")
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The daemon: claim the index dir, start an engine, answer on its pipe.
pub fn serve(dir: PathBuf, home: String) {
    let Some(_lock) = claim(&dir, Duration::ZERO) else { return };
    match Engine::start(options(&dir, &home)) {
        Ok(engine) => listen(&dir, &engine),
        Err(e) => log(e),
    }
}

/// Answer clients on the index dir's pipe with `engine`, until the process
/// ends. Call while holding `claim`'s lock.
pub fn listen(dir: &Path, engine: &Engine) {
    let name = pipe_name(dir);
    let mut next = match sys::pipe_listen(&name, true) {
        Ok(h) => Some(h),
        Err(e) => return log(format!("cannot listen on {name}: {e}")),
    };
    loop {
        let h = match next.take().map_or_else(|| sys::pipe_listen(&name, false), Ok) {
            Ok(h) => h,
            Err(e) => {
                log(format!("cannot listen on {name}: {e}"));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let conn = sys::pipe_accept(h);
        // Listening again before this one is handled: a client never finds
        // the pipe without a free instance.
        next = sys::pipe_listen(&name, false).ok();
        match conn {
            Ok(conn) => {
                let e = engine.clone();
                std::thread::spawn(move || handle(conn, &e));
            }
            // A client that hung up at once; anything else, don't spin.
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn handle(conn: std::fs::File, engine: &Engine) {
    let Ok(r) = conn.try_clone() else { return };
    let mut w = std::io::BufWriter::new(conn);
    for line in BufReader::new(r).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let resp = respond(&line, engine);
        if writeln!(w, "{resp}").and_then(|_| w.flush()).is_err() {
            return;
        }
        if resp["quitting"] == true {
            log("asked to quit");
            engine.checkpoint();
            if let Some(f) = ON_QUIT.get() {
                f();
            }
            std::process::exit(0);
        }
    }
}

fn respond(line: &str, engine: &Engine) -> Value {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad json: {e}")}),
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let mut out = match run(&v, engine) {
        Ok(r) => r,
        Err(e) => json!({"ok": false, "error": e}),
    };
    out["id"] = id;
    out
}

fn run(v: &Value, engine: &Engine) -> Result<Value, String> {
    let op = v.get("op").and_then(Value::as_str).unwrap_or("search");
    let is_grep = op == "grep"
        || (op == "search"
            && v.get("q").and_then(Value::as_str).is_some_and(|q| ["grep:", "regex:", "sym:", "content:", "symbol:"].iter().any(|k| q.contains(k))));
    match op {
        "ping" => Ok(json!({"ok": true})),
        "save" => {
            engine.save();
            Ok(json!({"ok": true, "scheduled": true}))
        }
        // The process exits once this answer is written (`scry install`
        // replacing the binary, say). The index is crash-safe: every file is
        // written aside and renamed into place.
        "quit" => Ok(json!({"ok": true, "quitting": true})),
        _ if is_grep => grep(v, engine),
        "status" => {
            let s = engine.status();
            if !s.ready {
                return Err("indexing (the first run lists every fixed drive, about a minute)".into());
            }
            let mut v = serde_json::to_value(s).map_err(|e| e.to_string())?;
            v["ok"] = true.into();
            Ok(v)
        }
        "search" => {
            let q = parse_request(v, engine.home())?;
            let t = Instant::now();
            let found = engine.search(&q)?;
            let took = t.elapsed().as_micros() as u64;
            let hits: Vec<Value> = found
                .iter()
                .map(|f| {
                    json!({
                        "path": f.path.to_string_lossy(),
                        "kind": kind_name(f.kind),
                        "size": f.size,
                        "mtime": f.mtime,
                        "score": f.score,
                    })
                })
                .collect();
            Ok(json!({"ok": true, "took_us": took, "hits": hits}))
        }
        _ => Err(format!("unknown op {op}")),
    }
}

/// Content search. The pattern comes from `pattern` (+ `mode`) or from a
/// `grep:`/`regex:`/`sym:` filter in `q`; the rest of the query narrows
/// which files are read.
fn grep(v: &Value, engine: &Engine) -> Result<Value, String> {
    let mut q = parse_request(v, engine.home())?;
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("regex") => GrepMode::Regex,
        Some("symbol") => GrepMode::Symbol,
        Some("literal") => GrepMode::Literal,
        Some(m) => return Err(format!("unknown mode {m}")),
        None => q.grep_mode,
    };
    let pattern = v.get("pattern").and_then(Value::as_str).map(str::to_string).or(q.grep.take()).ok_or("grep needs a pattern")?;
    let mut g = crate::Grep::new(&pattern, mode)?;
    if let Some(n) = v.get("per_file").and_then(Value::as_u64) {
        g.max_per_file = n as usize;
    }
    if let Some(ms) = v.get("budget_ms").and_then(Value::as_u64) {
        g.budget = (ms > 0).then(|| Duration::from_millis(ms));
    }
    let t = Instant::now();
    let (r, indexed) = engine.grep(&q, &g)?;
    let files: Vec<Value> = r
        .files
        .iter()
        .map(|f| {
            json!({
                "path": paths::display(&f.path),
                "matches": f.lines.iter().map(|(n, t)| json!({"line": n, "text": t})).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "took_us": t.elapsed().as_micros() as u64,
        "source": if indexed { "index" } else { "scan" },
        "candidates": r.candidates,
        "read": r.read,
        "complete": r.complete,
        "indexing": engine.status().content_pending,
        "files": files,
    }))
}

/// `q` is the query language; any filter key may also be given as its own
/// JSON field (`{"q": "main", "ext": "rs", "in": "~/Developer"}`).
fn parse_request(v: &Value, home: &str) -> Result<Query, String> {
    let mut q = Query::parse(v.get("q").and_then(Value::as_str).unwrap_or(""), home)?;
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k == "limit" {
                q.limit = val.as_u64().ok_or("limit must be a number")? as usize;
            } else {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                q.filter(k, &s, home)?;
            }
        }
    }
    Ok(q)
}

fn kind_name(k: u8) -> &'static str {
    match k & 3 {
        KIND_FILE => "file",
        KIND_DIR => "dir",
        KIND_LINK => "link",
        _ => "other",
    }
}

/// Connect to the daemon if it is running.
pub fn connect_running(dir: &Path) -> std::io::Result<std::fs::File> {
    sys::pipe_connect(&pipe_name(dir))
}

/// Connect to the daemon, starting it if it isn't running.
pub fn connect(dir: &Path) -> std::io::Result<std::fs::File> {
    let name = pipe_name(dir);
    match sys::pipe_connect(&name) {
        Ok(s) => return Ok(s),
        // It runs, but not for us: starting another would get nowhere.
        Err(e) if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32) => {
            return Err(std::io::Error::new(e.kind(), "the daemon refuses this process (is it running as another user, or elevated?)"));
        }
        Err(_) => {}
    }
    start_daemon(dir, &std::env::current_exe()?, &[])?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(30));
        if let Ok(s) = sys::pipe_connect(&name) {
            return Ok(s);
        }
    }
    sys::pipe_connect(&name)
}

/// Start the daemon in the background: the windowless `scryd.exe` next
/// to `exe` (with `args`) if there is one, else `exe serve`. Detached: no console, its
/// own process group and, where allowed, out of this terminal's job, so
/// closing the terminal doesn't take it down.
pub fn start_daemon(dir: &Path, exe: &Path, args: &[std::ffi::OsString]) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    let daemon = exe.with_file_name("scryd.exe");
    let mut cmd = std::process::Command::new(if daemon.is_file() { &daemon } else { exe });
    if !daemon.is_file() {
        cmd.arg("serve");
    } else {
        cmd.args(args);
    }
    let log = open_log(dir)?;
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // Not the caller's folder: a long-lived process there would keep it
    // from being renamed or deleted.
    cmd.current_dir(dir);
    sys::keep_std_handles();
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn().or_else(|_| cmd.creation_flags(flags).spawn()).map(drop)
}
