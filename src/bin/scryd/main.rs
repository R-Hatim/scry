//! The Scry daemon, without a console window: the search bar and the tray
//! icon. What `scry install --login` (or the installer) starts at login,
//! and what the CLI starts when no daemon runs. One engine answers the pipe
//! (the CLI, other apps), the bar and the icon. Logs go to `daemon.log` in
//! the index dir.
//!
//! `--restart` (the tray, after the folders changed): the daemon before is
//! on its way out; wait for it rather than leave. `--rescan <folder>`: list
//! that folder whole once started (it was skipped until now).
#![windows_subsystem = "windows"]

mod bar;
mod folders;
mod tray;

use scry::server;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let home = scry::default_home();
    let dir = scry::default_dir(&home);
    if let Ok(log) = server::open_log(&dir) {
        server::log_to(log);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wait = if args.iter().any(|a| a == "--restart") { Duration::from_secs(15) } else { Duration::ZERO };
    let rescan: Vec<PathBuf> = args.windows(2).filter(|w| w[0] == "--rescan").map(|w| PathBuf::from(&w[1])).collect();
    // One per user: when the login item and the CLI both start one, the
    // second leaves.
    let Some(_lock) = server::claim(&dir, wait) else { return };
    let engine = match scry::Engine::start(server::options(&dir, &home)) {
        Ok(e) => e,
        Err(e) => return scry::log(e),
    };
    engine.rescan(&rescan);
    let (e, d) = (engine.clone(), dir.clone());
    std::thread::Builder::new().name("scry-pipe".into()).spawn(move || server::listen(&d, &e)).expect("spawn");
    let hotkeys = match server::config(&dir).hotkey {
        Some(k) => vec![k],
        None => bar::DEFAULT_HOTKEYS.iter().map(|k| k.to_string()).collect(),
    };
    let hotkey = match bar::create(engine.clone(), home, &hotkeys) {
        Ok(k) => k,
        Err(e) => return scry::log(e),
    };
    tray::create(engine, hotkey, dir);
    bar::pump();
}
