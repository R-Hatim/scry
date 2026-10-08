//! The `fsearch` command: searches through the daemon (starting it when
//! needed), runs it, installs it.

use fsearch::{index, live, query, server};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = r"usage:
  fsearch <query...> [--json]   search (starts the daemon if needed)
  fsearch stdio                 JSON lines on stdin/stdout
  fsearch serve                 run the daemon in the foreground
  fsearch status
  fsearch stop                  stop the daemon (the next search starts it again)
  fsearch install [--login]     copy to ~\.local\bin; --login also starts the daemon at login
  fsearch uninstall             remove the login item and stop the daemon (keeps the index)
  fsearch bench <query...>      time a query in-process against the saved index";

fn home() -> String {
    fsearch::default_home()
}

fn data_dir() -> PathBuf {
    let d = fsearch::default_dir(&home());
    std::fs::create_dir_all(&d).ok();
    d
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => eprintln!("{USAGE}"),
        Some("serve") => server::serve(data_dir(), home()),
        Some("stdio") => stdio(),
        Some("status") => print_one(&json!({"op": "status"}), true),
        Some("stop") => stop(true),
        Some("bench") => bench(&args[1..].join(" ")),
        Some("install") => install(args.iter().any(|a| a == "--login")),
        Some("uninstall") => uninstall(),
        Some(_) => {
            let json = args.iter().any(|a| a == "--json");
            let q: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
            print_one(&json!({"q": q.join(" ")}), json);
        }
    }
}

fn print_one(req: &serde_json::Value, raw: bool) {
    let mut s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    writeln!(s, "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).unwrap();
    if raw {
        print!("{line}");
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"] != true {
        die(v["error"].as_str().unwrap_or("error"));
    }
    let mut out = std::io::stdout().lock();
    for h in v["hits"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", h["path"].as_str().unwrap_or(""));
    }
    for f in v["files"].as_array().into_iter().flatten() {
        for m in f["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{}:{}: {}", f["path"].as_str().unwrap_or(""), m["line"], m["text"].as_str().unwrap_or("").trim());
        }
    }
}

/// One request, one answer, in turn: a pipe handle does one read or write
/// at a time, so a reader blocked on it would stall a writer.
fn stdio() {
    let s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    let mut out = std::io::stdout().lock();
    let mut resp = String::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        // The daemon answers nothing to an empty line.
        if line.trim().is_empty() {
            continue;
        }
        resp.clear();
        if writeln!(w, "{line}").is_err() || r.read_line(&mut resp).unwrap_or(0) == 0 {
            break;
        }
        if out.write_all(resp.as_bytes()).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn bench(qs: &str) {
    let idx = index::Index::load(&data_dir().join("index.bin")).unwrap_or_else(|| die("no index yet; run fsearch serve"));
    let live = live::Live::new(idx);
    let q = query::Query::parse(qs, &home()).unwrap_or_else(|e| die(&e));
    let s = query::Searcher { live: &live };
    let mut times = Vec::new();
    let mut hits = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        hits = s.search(&q);
        times.push(t.elapsed());
    }
    let mut p = Vec::new();
    for h in hits.iter().take(10) {
        live.base.path(h.idx as usize, &mut p);
        println!("{:5} {}", h.score, fsearch::paths::display(&p));
    }
    let first = times[0];
    times.sort();
    eprintln!("first {first:.2?}  median {:.2?}  min {:.2?}", times[times.len() / 2], times[0]);
}

/// Ask the daemon to exit and wait until it has.
fn stop(verbose: bool) {
    let dir = data_dir();
    let Ok(mut s) = server::connect_running(&dir) else {
        if verbose {
            println!("the daemon is not running");
        }
        return;
    };
    let _ = writeln!(s, "{}", json!({"op": "quit"}));
    let _ = BufReader::new(&s).read_line(&mut String::new());
    drop(s);
    for _ in 0..100 {
        if server::connect_running(&dir).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if verbose {
        println!("stopped the daemon");
    }
}

fn install(login: bool) {
    let bin = PathBuf::from(home()).join(r".local\bin");
    std::fs::create_dir_all(&bin).unwrap_or_else(|e| die(&format!("{}: {e}", bin.display())));
    let exe = std::env::current_exe().unwrap();
    // The next search starts the new daemon.
    stop(false);
    for name in ["fsearch.exe", "fsearchd.exe"] {
        let (src, dst) = (exe.with_file_name(name), bin.join(name));
        if !src.is_file() || src == dst {
            continue;
        }
        // A running program can't be overwritten, but it can be renamed:
        // set the old one aside, and remove it unless it is still running
        // (then the next install does).
        let old = dst.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&dst, &old);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| die(&format!("copy {name}: {e}")));
        let _ = std::fs::remove_file(&old);
    }
    let daemon = bin.join("fsearchd.exe");
    if login {
        if !daemon.is_file() || !server::login_item(Some(&daemon)) {
            die("could not add the login item");
        }
        server::start_daemon(&data_dir(), &bin.join("fsearch.exe")).unwrap_or_else(|e| die(&format!("start the daemon: {e}")));
        println!("installed {} (starts at login, running now)", bin.display());
    } else {
        println!("installed {}; the daemon starts on first use", bin.display());
    }
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == bin));
    if !on_path {
        println!("add {} to your PATH to run fsearch from anywhere", bin.display());
    }
}

fn uninstall() {
    server::login_item(None);
    stop(false);
    println!("removed the login item and stopped the daemon; index kept in {}", data_dir().display());
}

fn die(msg: &str) -> ! {
    eprintln!("fsearch: {msg}");
    std::process::exit(1)
}
