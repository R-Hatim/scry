//! Windows resources for both programs: the icon (assets\scry.ico, see
//! assets\icon.ps1) and a version block (what Explorer's Details tab, Task
//! Manager and notifications show), written as a compiled .res that the
//! linker takes as it is: no resource compiler needed.

use std::path::PathBuf;

fn main() {
    // The search bar's edit box needs Common Controls 6 (its placeholder
    // text): a manifest saying so, embedded in scryd.exe.
    println!("cargo:rustc-link-arg-bin=scryd=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=scryd=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/scry.ico");
    let ico = std::fs::read("assets/scry.ico").expect("assets/scry.ico");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    for (bin, description) in [("scry", "Scry command line"), ("scryd", "Scry")] {
        let mut res = Res::new();
        icon(&mut res, &ico);
        res.add(RT_VERSION, 1, version_info(&version, bin, description));
        let path = out.join(format!("{bin}.res"));
        std::fs::write(&path, res.0).unwrap();
        println!("cargo:rustc-link-arg-bin={bin}={}", path.display());
    }
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
/// English (United States), Unicode: what the version block is written in.
const LANG: u16 = 0x0409;
const CODEPAGE: u16 = 1200;

/// A .res file: resources, each a header (type and name by number) and its
/// data, 32-bit aligned.
struct Res(Vec<u8>);

impl Res {
    /// Starts with the empty entry that marks a 32-bit .res.
    fn new() -> Res {
        let mut r = Res(Vec::new());
        r.entry(0, 0, 0, &[]);
        r
    }

    fn add(&mut self, kind: u16, id: u16, data: Vec<u8>) {
        self.entry(kind, id, LANG, &data);
    }

    fn entry(&mut self, kind: u16, id: u16, lang: u16, data: &[u8]) {
        let b = &mut self.0;
        put32(b, data.len() as u32);
        put32(b, 32); // header size
        for v in [0xFFFF, kind, 0xFFFF, id] {
            put16(b, v);
        }
        put32(b, 0); // data version
        put16(b, if kind == 0 { 0 } else { 0x1030 }); // moveable, pure, discardable
        put16(b, lang);
        put32(b, 0); // version
        put32(b, 0); // characteristics
        b.extend_from_slice(data);
        pad(b);
    }
}

/// An .ico's images as icon resources 1..n, and the group that lists them
/// as icon group 1 (the program's icon).
fn icon(res: &mut Res, ico: &[u8]) {
    let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([ico[i], ico[i + 1], ico[i + 2], ico[i + 3]]);
    assert!(u16_at(2) == 1, "assets/scry.ico is not an icon");
    let n = u16_at(4);
    let mut group = Vec::new();
    for v in [0, 1, n] {
        put16(&mut group, v);
    }
    for i in 0..n {
        let e = 6 + 16 * i as usize;
        let (size, offset) = (u32_at(e + 8), u32_at(e + 12) as usize);
        res.add(RT_ICON, i + 1, ico[offset..offset + size as usize].to_vec());
        // The directory entry as it is, but the image's resource number
        // where the file has its offset.
        group.extend_from_slice(&ico[e..e + 12]);
        put16(&mut group, i + 1);
    }
    res.add(RT_GROUP_ICON, 1, group);
}

/// VS_VERSIONINFO: the fixed part, the strings, the language.
fn version_info(version: &str, bin: &str, description: &str) -> Vec<u8> {
    let v: Vec<u16> = version.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    let (ms, ls) = ((v[0] as u32) << 16 | v[1] as u32, (v[2] as u32) << 16);
    let mut fixed = Vec::new();
    for x in [
        0xFEEF04BD, // signature
        0x0001_0000,
        ms,
        ls,
        ms,
        ls,
        0x3F,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,
        0,
        0,
    ] {
        put32(&mut fixed, x);
    }
    let exe = format!("{bin}.exe");
    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "R-Hatim"),
        ("FileDescription", description),
        ("FileVersion", version),
        ("InternalName", bin),
        ("LegalCopyright", "Copyright (c) 2026 Noah Dunnagan, R-Hatim. MIT License."),
        ("OriginalFilename", &exe),
        ("ProductName", "Scry"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(k, s)| {
        let text = utf16z(s);
        block(k, (text.len() / 2) as u16, true, &text, &[])
    })
    .collect();
    let table = block(&format!("{LANG:04x}{CODEPAGE:04x}"), 0, true, &[], &strings);
    let string_info = block("StringFileInfo", 0, true, &[], &[table]);
    let translation = [LANG.to_le_bytes(), CODEPAGE.to_le_bytes()].concat();
    let var = block("Translation", 4, false, &translation, &[]);
    let var_info = block("VarFileInfo", 0, true, &[], &[var]);
    block("VS_VERSION_INFO", fixed.len() as u16, false, &fixed, &[string_info, var_info])
}

/// One node of a version block: its length, its value's length (in
/// characters for text, else bytes), whether the value is text, its key, the
/// value and the children, each starting on a 32-bit boundary.
fn block(key: &str, value_len: u16, text: bool, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0; 6];
    b.extend_from_slice(&utf16z(key));
    pad(&mut b);
    b.extend_from_slice(value);
    for c in children {
        pad(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b[2..4].copy_from_slice(&value_len.to_le_bytes());
    b[4..6].copy_from_slice(&(text as u16).to_le_bytes());
    b
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn pad(b: &mut Vec<u8>) {
    b.resize(b.len().next_multiple_of(4), 0);
}
