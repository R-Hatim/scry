//! The fsearch daemon, without a console window, and its search bar: what
//! `fsearch install --login` starts at login, and what the CLI starts when
//! no daemon runs. One engine answers both the pipe (the CLI, other apps)
//! and the bar. Logs go to `daemon.log` in the index dir.
#![windows_subsystem = "windows"]

mod bar;

use fsearch::server;

fn main() {
    let home = fsearch::default_home();
    let dir = fsearch::default_dir(&home);
    if let Ok(log) = server::open_log(&dir) {
        server::log_to(log);
    }
    // One per user: when the login item and the CLI both start one, the
    // second leaves.
    let Some(_lock) = server::claim(&dir) else { return };
    let engine = match fsearch::Engine::start(server::options(&dir, &home)) {
        Ok(e) => e,
        Err(e) => return fsearch::log(e),
    };
    let (e, d) = (engine.clone(), dir.clone());
    std::thread::Builder::new().name("fsearch-pipe".into()).spawn(move || server::listen(&d, &e)).expect("spawn");
    let hotkeys = match server::config(&dir).hotkey {
        Some(k) => vec![k],
        None => bar::DEFAULT_HOTKEYS.iter().map(|k| k.to_string()).collect(),
    };
    bar::run(engine, home, &hotkeys);
}
