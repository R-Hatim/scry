//! Paths inside fsearch are bytes: WTF-8 (what Windows names decode to,
//! unpaired surrogates included), '/'-separated, under a virtual root "/"
//! whose children are the drives. `C:\Users\me\a.txt` is `/C:/Users/me/a.txt`
//! here and `C:\` is `/C:`. Everything that splits, joins and compares paths
//! works on that one form; only the edges (the OS, the user) convert.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Append a UTF-16 name as WTF-8.
pub fn push_wide(out: &mut Vec<u8>, w: &[u16]) {
    if w.iter().all(|&c| c < 0x80) {
        out.extend(w.iter().map(|&c| c as u8));
        return;
    }
    for r in char::decode_utf16(w.iter().copied()) {
        match r {
            Ok(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            // A lone surrogate is legal in a Windows name; WTF-8 keeps it.
            Err(e) => {
                let s = e.unpaired_surrogate() as u32;
                out.extend_from_slice(&[0xE0 | (s >> 12) as u8, 0x80 | (s >> 6 & 0x3F) as u8, 0x80 | (s & 0x3F) as u8]);
            }
        }
    }
}

/// Append WTF-8 as UTF-16: the inverse of `push_wide`. Bytes that are not
/// WTF-8 become U+FFFD.
pub fn push_utf16(out: &mut Vec<u16>, b: &[u8]) {
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            out.push(c as u16);
            i += 1;
            continue;
        }
        let (n, init) = match c {
            0xC2..=0xDF => (2, (c & 0x1F) as u32),
            0xE0..=0xEF => (3, (c & 0x0F) as u32),
            0xF0..=0xF4 => (4, (c & 0x07) as u32),
            _ => (0, 0),
        };
        if n == 0 || i + n > b.len() || !b[i + 1..i + n].iter().all(|&x| x & 0xC0 == 0x80) {
            out.push(0xFFFD);
            i += 1;
            continue;
        }
        let v = b[i + 1..i + n].iter().fold(init, |v, &x| v << 6 | (x & 0x3F) as u32);
        // Overlong or out-of-range sequences are not WTF-8 either.
        if v < [0, 0, 0x80, 0x800, 0x1_0000][n] || v > 0x10_FFFF {
            out.push(0xFFFD);
            i += 1;
            continue;
        }
        if v >= 0x10000 {
            let v = v - 0x10000;
            out.extend_from_slice(&[0xD800 | (v >> 10) as u16, 0xDC00 | (v & 0x3FF) as u16]);
        } else {
            out.push(v as u16);
        }
        i += n;
    }
}

/// UTF-16 of the native path, backslashes, a drive alone being its root.
fn native_wide(p: &[u8], verbatim: bool) -> Vec<u16> {
    let rest = p.strip_prefix(b"/").unwrap_or(p);
    let mut w = Vec::with_capacity(rest.len() + 5);
    if verbatim && !rest.is_empty() {
        w.extend(r"\\?\".encode_utf16());
    }
    let start = w.len();
    push_utf16(&mut w, rest);
    for c in &mut w[start..] {
        if *c == b'/' as u16 {
            *c = b'\\' as u16;
        }
    }
    if rest.len() == 2 && rest[1] == b':' {
        w.push(b'\\' as u16);
    }
    w
}

/// The native path: `/C:/x` is `C:\x`, `/C:` is `C:\`.
pub fn to_native(p: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_wide(&native_wide(p, false)))
}

/// The native path as text, for people and JSON (lossy only for names with
/// unpaired surrogates).
pub fn display(p: &[u8]) -> String {
    to_native(p).to_string_lossy().into_owned()
}

/// NUL-terminated UTF-16 for Win32 calls, verbatim (`\\?\C:\x`): no
/// MAX_PATH limit, and names with trailing dots or spaces stay as they are.
pub fn to_wide(p: &[u8]) -> Vec<u16> {
    let mut w = native_wide(p, true);
    w.push(0);
    w
}

/// The index path of an absolute native path (`C:\x`, `c:/x\`, `\\?\C:\x`).
/// None for anything not on a drive letter (shares, relative paths).
pub fn from_native(p: &Path) -> Option<Vec<u8>> {
    let w: Vec<u16> = p.as_os_str().encode_wide().collect();
    let mut b = Vec::with_capacity(w.len() + 1);
    push_wide(&mut b, &w);
    from_native_bytes(&b)
}

fn from_native_bytes(b: &[u8]) -> Option<Vec<u8>> {
    let b = b.strip_prefix(br"\\?\").or_else(|| b.strip_prefix(br"\??\")).unwrap_or(b);
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b.get(2).is_some_and(|&c| c != b'\\' && c != b'/') {
        return None;
    }
    let mut out = vec![b'/', b[0].to_ascii_uppercase(), b':'];
    for comp in b[2..].split(|&c| c == b'\\' || c == b'/').filter(|c| !c.is_empty()) {
        out.push(b'/');
        out.extend_from_slice(comp);
    }
    Some(out)
}

/// A folder the user named (`in:`, config): `~` is home; made absolute,
/// then real (true case, long names, junctions resolved) if it exists.
pub fn from_user(s: &str, home: &str) -> Option<Vec<u8>> {
    let s = match s.strip_prefix('~') {
        Some(r) => format!("{home}{r}"),
        None => s.to_string(),
    };
    let p = std::fs::canonicalize(&s).or_else(|_| std::path::absolute(&s)).ok()?;
    from_native(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_round_trip() {
        assert_eq!(from_native(Path::new(r"C:\Users\me\a.txt")).unwrap(), b"/C:/Users/me/a.txt");
        assert_eq!(from_native(Path::new(r"\\?\c:\Users\")).unwrap(), b"/C:/Users");
        assert_eq!(from_native(Path::new(r"d:/x//y")).unwrap(), b"/D:/x/y");
        assert_eq!(from_native(Path::new(r"C:\")).unwrap(), b"/C:");
        assert_eq!(from_native(Path::new(r"\\server\share\x")), None);
        assert_eq!(from_native(Path::new(r"relative\x")), None);
        assert_eq!(to_native(b"/C:/Users/me"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(to_native(b"/C:"), PathBuf::from(r"C:\"));
        assert_eq!(to_wide(b"/C:/a"), r"\\?\C:\a".encode_utf16().chain([0]).collect::<Vec<_>>());
    }

    #[test]
    fn wtf8_round_trip() {
        for s in [&[0x61u16, 0x62][..], &[0xE9, 0x4E2D], &[0xD83D, 0xDE00], &[0xD800, 0x61], &[0x61, 0xDC00]] {
            let mut b = Vec::new();
            push_wide(&mut b, s);
            let mut w = Vec::new();
            push_utf16(&mut w, &b);
            assert_eq!(w, s);
            // Matches what std itself makes of the name.
            assert_eq!(b, OsString::from_wide(s).as_encoded_bytes());
        }
    }
}
