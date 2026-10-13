//! What you did in the bar, kept in the index dir so it lasts:
//!
//! - the files you opened (`opened.txt`), which rank higher the more often
//!   and the more recently you opened them;
//! - the searches you opened something from (`history.txt`), which Up
//!   brings back.
//!
//! Plain text, one entry per line, rewritten whole (a few KB) on each change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Entries kept; past this, the ones worth least go.
const KEEP: usize = 2000;
/// The most a file gains from being opened before: a few letters' worth of
/// match, enough to beat names that match as well, not ones that match
/// clearly better.
const BOOST_PER_OPEN: i32 = 30;
const OPENS_COUNTED: u32 = 5;
const HISTORY: usize = 100;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Windows paths ignore case.
fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Files opened from the bar: how often, and when last.
pub struct Opened {
    file: PathBuf,
    /// Lowercased path -> (path as opened, times, last time in Unix seconds).
    map: HashMap<String, (String, u32, u64)>,
}

impl Opened {
    pub fn load(dir: &Path) -> Opened {
        let file = dir.join("opened.txt");
        let mut map = HashMap::new();
        for line in std::fs::read_to_string(&file).unwrap_or_default().lines() {
            let mut f = line.splitn(3, '\t');
            if let (Some(n), Some(t), Some(p)) = (f.next(), f.next(), f.next())
                && let (Ok(n), Ok(t)) = (n.parse(), t.parse())
            {
                map.insert(p.to_lowercase(), (p.to_string(), n, t));
            }
        }
        Opened { file, map }
    }

    pub fn record(&mut self, path: &Path) {
        let e = self.map.entry(key(path)).or_insert_with(|| (path.to_string_lossy().into_owned(), 0, 0));
        e.1 += 1;
        e.2 = now();
        if self.map.len() > KEEP {
            let t = now();
            let mut worth: Vec<(i32, String)> = self.map.iter().map(|(k, v)| (worth(v.1, v.2, t), k.clone())).collect();
            worth.sort();
            for (_, k) in worth.into_iter().take(self.map.len() - KEEP) {
                self.map.remove(&k);
            }
        }
        let text: String = self.map.values().map(|(p, n, t)| format!("{n}\t{t}\t{p}\n")).collect();
        save(&self.file, &text);
    }

    /// What `path` gains in the ranking from having been opened.
    pub fn boost(&self, path: &Path, now: u64) -> i32 {
        self.map.get(&key(path)).map_or(0, |&(_, n, t)| worth(n, t, now))
    }
}

/// Opened `n` times, last at `t`: more for more opens, less as it ages.
fn worth(n: u32, t: u64, now: u64) -> i32 {
    let days = now.saturating_sub(t) / 86_400;
    let fade = match days {
        0..=1 => 10,
        2..=13 => 6,
        14..=59 => 3,
        _ => 1,
    };
    BOOST_PER_OPEN * n.min(OPENS_COUNTED) as i32 * fade / 10
}

/// The searches you opened something from, oldest first.
pub struct History {
    file: PathBuf,
    items: Vec<String>,
}

impl History {
    pub fn load(dir: &Path) -> History {
        let file = dir.join("history.txt");
        let items = std::fs::read_to_string(&file).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        History { file, items }
    }

    pub fn push(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() || q.contains('\n') {
            return;
        }
        self.items.retain(|i| i != q);
        self.items.push(q.to_string());
        let over = self.items.len().saturating_sub(HISTORY);
        self.items.drain(..over);
        let text: String = self.items.iter().map(|i| format!("{i}\n")).collect();
        save(&self.file, &text);
    }

    /// The `back`th most recent search (0 is the last one).
    pub fn get(&self, back: usize) -> Option<&str> {
        self.items.iter().rev().nth(back).map(String::as_str)
    }
}

/// Write aside and rename into place; losing this to a full disk is fine.
fn save(file: &Path, text: &str) {
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, file)).is_err() {
        scry::log(format!("could not save {}", file.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scry-usage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn opens_count_fade_and_last() {
        let d = dir("opened");
        let mut o = Opened::load(&d);
        let p = Path::new(r"C:\Work\Invoices");
        assert_eq!(o.boost(p, now()), 0);
        o.record(p);
        o.record(Path::new(r"c:\work\INVOICES"));
        assert_eq!(o.boost(p, now()), 2 * BOOST_PER_OPEN, "case does not matter");
        for _ in 0..10 {
            o.record(p);
        }
        assert_eq!(o.boost(p, now()), OPENS_COUNTED as i32 * BOOST_PER_OPEN, "capped");
        assert_eq!(o.boost(p, now() + 30 * 86_400), OPENS_COUNTED as i32 * BOOST_PER_OPEN * 3 / 10, "fades");
        assert_eq!(Opened::load(&d).boost(p, now()), OPENS_COUNTED as i32 * BOOST_PER_OPEN, "kept on disk");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn history_recalls_latest_first_without_repeats() {
        let d = dir("history");
        let mut h = History::load(&d);
        for q in ["invoice", "ext:pdf tax", "invoice ", "  "] {
            h.push(q);
        }
        assert_eq!((h.get(0), h.get(1), h.get(2)), (Some("invoice"), Some("ext:pdf tax"), None));
        assert_eq!(History::load(&d).get(0), Some("invoice"), "kept on disk");
        let _ = std::fs::remove_dir_all(&d);
    }
}
