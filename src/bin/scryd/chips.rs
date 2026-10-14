//! The filter chips under the search box: Type, Modified, Size, In folder,
//! Sort. A chip shows the filter of its kind the query holds (or its name),
//! and choosing from it writes that filter into the query text, replacing
//! any of the same kind: the text stays what is searched, and typing a
//! filter works as before.

use std::path::Path;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Chip {
    Type,
    Modified,
    Size,
    Folder,
    Sort,
}

pub const CHIPS: [Chip; 5] = [Chip::Type, Chip::Modified, Chip::Size, Chip::Folder, Chip::Sort];

impl Chip {
    /// Its name, when the query has no filter of its kind.
    pub fn name(self) -> &'static str {
        match self {
            Chip::Type => "Type",
            Chip::Modified => "Modified",
            Chip::Size => "Size",
            Chip::Folder => "In folder",
            Chip::Sort => "Sort",
        }
    }

    /// The filter keys it stands for: choosing replaces all of them.
    fn keys(self) -> &'static [&'static str] {
        match self {
            Chip::Type => &["type", "kind"],
            Chip::Modified => &["mtime", "modified"],
            Chip::Size => &["size"],
            Chip::Folder => &["in"],
            Chip::Sort => &["sort", "order"],
        }
    }

    /// What it offers: a label and the filter it writes. Empty for the
    /// folder chip, which opens the folder picker instead.
    pub fn choices(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Chip::Type => &[
                ("Documents", "type:doc"),
                ("Images", "type:image"),
                ("Videos", "type:video"),
                ("Audio", "type:audio"),
                ("Code", "type:code"),
                ("Archives", "type:archive"),
                ("Apps", "type:app"),
                ("Fonts", "type:font"),
                ("Folders", "kind:dir"),
            ],
            Chip::Modified => &[
                ("Today", "mtime:<1d"),
                ("Last 7 days", "mtime:<7d"),
                ("Last 30 days", "mtime:<30d"),
                ("Last year", "mtime:<365d"),
                ("Over a year ago", "mtime:>365d"),
            ],
            Chip::Size => &[("Under 100 KB", "size:<100kb"), ("Over 1 MB", "size:>1mb"), ("Over 100 MB", "size:>100mb"), ("Over 1 GB", "size:>1gb")],
            Chip::Folder => &[],
            Chip::Sort => &[("Largest first", "sort:size"), ("Newest first", "sort:date")],
        }
    }
}

/// The words of a query as byte ranges: split on spaces, a quoted part
/// (`in:"C:\Program Files"`) staying whole.
fn words(text: &str) -> Vec<(usize, usize)> {
    let (mut out, mut start, mut quoted) = (Vec::new(), None, false);
    for (i, c) in text.char_indices() {
        match c {
            '"' => {
                quoted = !quoted;
                start.get_or_insert(i);
            }
            c if c.is_whitespace() && !quoted => {
                if let Some(s) = start.take() {
                    out.push((s, i));
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

/// The filter of `chip`'s kind in `text`, as written (the last one wins,
/// as in the query).
fn filter_of(text: &str, chip: Chip) -> Option<&str> {
    words(text).into_iter().map(|(a, b)| &text[a..b]).rfind(|w| w.split_once(':').is_some_and(|(k, _)| chip.keys().iter().any(|x| x.eq_ignore_ascii_case(k))))
}

/// What the chip shows: the label of the choice the query holds, a folder's
/// name, the filter as typed, or None (then its name).
pub fn shown(text: &str, chip: Chip) -> Option<String> {
    let f = filter_of(text, chip)?;
    if let Some((label, _)) = chip.choices().iter().find(|(_, w)| w.eq_ignore_ascii_case(f)) {
        return Some(label.to_string());
    }
    let v = f.split_once(':').map_or("", |(_, v)| v).trim_matches('"');
    Some(match chip {
        Chip::Folder => Path::new(v).file_name().map_or(v.to_string(), |n| n.to_string_lossy().into_owned()),
        _ => v.to_string(),
    })
}

/// `text` with `chip`'s filters taken out and `filter` (if any) added at the
/// end; the rest as it was.
pub fn set(text: &str, chip: Chip, filter: Option<&str>) -> String {
    let keep: Vec<&str> = words(text)
        .into_iter()
        .map(|(a, b)| &text[a..b])
        .filter(|w| !w.split_once(':').is_some_and(|(k, _)| chip.keys().iter().any(|x| x.eq_ignore_ascii_case(k))))
        .collect();
    let mut out = keep.join(" ");
    if let Some(f) = filter {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(f);
    }
    out
}

/// A folder as an `in:` value: quoted when it has spaces.
pub fn in_value(folder: &Path) -> String {
    let s = folder.display().to_string();
    if s.contains(' ') { format!("\"{s}\"") } else { s }
}

/// A folder as a `path:` value: a regex over `C:/...` paths (forward
/// slashes), matching the folder and what is under it.
pub fn path_value(folder: &Path) -> String {
    let s = folder.display().to_string().replace('\\', "/");
    let s = regex::escape(s.trim_end_matches('/'));
    if s.contains(' ') { format!("\"{s}/\"") } else { format!("{s}/") }
}

/// The folder filter key just before the caret (`in:` or `path:`, nothing
/// typed after it yet), whose value the folder picker can fill in.
pub fn folder_key_before(text: &str, caret: usize) -> Option<&'static str> {
    let before = text.get(..caret)?;
    let word = &before[before.rfind(char::is_whitespace).map_or(0, |i| i + 1)..];
    ["in:", "path:"].into_iter().find(|k| word.eq_ignore_ascii_case(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_read_and_write_the_query() {
        let q = r#"invoice in:"C:\My Docs" ext:pdf Type:image"#;
        assert_eq!(shown(q, Chip::Type).as_deref(), Some("Images"), "keys ignore case");
        assert_eq!(shown(q, Chip::Folder).as_deref(), Some("My Docs"));
        assert_eq!(shown(q, Chip::Size), None);
        assert_eq!(shown("size:>5mb", Chip::Size).as_deref(), Some(">5mb"), "a typed value shows as typed");
        assert_eq!(set(q, Chip::Type, Some("kind:dir")), r#"invoice in:"C:\My Docs" ext:pdf kind:dir"#);
        assert_eq!(set(q, Chip::Folder, None), "invoice ext:pdf Type:image");
        assert_eq!(set("", Chip::Sort, Some("sort:size")), "sort:size");
        assert_eq!(set("report sort:date", Chip::Sort, Some("sort:size")), "report sort:size");
    }

    #[test]
    fn every_choice_is_a_filter_the_query_takes() {
        for chip in CHIPS {
            for (label, filter) in chip.choices() {
                assert!(scry::Query::parse(filter, r"C:\").is_ok(), "{label}: {filter}");
            }
        }
    }

    #[test]
    fn folders_as_filter_values() {
        assert_eq!(in_value(Path::new(r"C:\Work")), r"C:\Work");
        assert_eq!(in_value(Path::new(r"C:\Program Files")), r#""C:\Program Files""#);
        assert_eq!(path_value(Path::new(r"C:\Work\a.b")), r"C:/Work/a\.b/");
        assert_eq!(folder_key_before("report in:", 10), Some("in:"));
        assert_eq!(folder_key_before("PATH:", 5), Some("path:"));
        assert_eq!(folder_key_before("report in:C", 11), None);
        assert_eq!(folder_key_before("main", 4), None);
    }
}
