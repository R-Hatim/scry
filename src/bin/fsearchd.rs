//! The fsearch daemon without a console window: what `fsearch install
//! --login` starts at login, and what the CLI starts when no daemon runs.
//! Logs go to `daemon.log` in the index dir.
#![windows_subsystem = "windows"]

fn main() {
    let home = fsearch::default_home();
    let dir = fsearch::default_dir(&home);
    if let Ok(log) = fsearch::server::open_log(&dir) {
        fsearch::server::log_to(log);
    }
    fsearch::server::serve(dir, home);
}
