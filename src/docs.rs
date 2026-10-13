//! Text out of documents, for search inside files: Word, Excel and
//! PowerPoint files (Office Open XML) and OpenDocument files are ZIP archives
//! of XML, read here directly, Office or not; PDFs go through the PDF filter
//! Windows indexes them with itself (Windows.Data.Pdf, an IFilter). The text
//! comes out as UTF-8, a line per paragraph, capped like a large text file.

use std::ffi::c_void;
use std::io::Read;
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Storage::IndexServer::*;
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSIDFromString, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::UI::Shell::SHCreateStreamOnFileEx;
use windows_sys::core::{GUID, HRESULT};

/// Documents larger than this are left out.
pub const MAX_DOC: u64 = 64 << 20;
/// Text kept per document.
const MAX_TEXT: usize = 2 << 20;
/// What one XML part may inflate to.
const MAX_PART: usize = 32 << 20;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Word,
    Excel,
    PowerPoint,
    OpenDocument,
    Pdf,
}

/// A document, by the extension of its name.
pub fn kind(name: &[u8]) -> Option<Kind> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    Some(match name[dot + 1..].to_ascii_lowercase().as_slice() {
        b"docx" | b"docm" | b"dotx" => Kind::Word,
        b"xlsx" | b"xlsm" => Kind::Excel,
        b"pptx" | b"pptm" | b"ppsx" => Kind::PowerPoint,
        b"odt" | b"ods" | b"odp" => Kind::OpenDocument,
        b"pdf" => Kind::Pdf,
        _ => return None,
    })
}

/// The text of the document at `path` (opened as `file`) into `out`. False
/// when there is none to be had: damaged, encrypted, scanned images.
pub fn text(path: &[u8], kind: Kind, file: std::fs::File, out: &mut Vec<u8>) -> bool {
    out.clear();
    if kind == Kind::Pdf {
        drop(file);
        pdf(path, out);
    } else {
        let mut zip = Vec::new();
        if file.take(MAX_DOC + 1).read_to_end(&mut zip).ok().is_some_and(|n| n as u64 <= MAX_DOC) {
            office(&zip, kind, out);
        }
    }
    out.truncate(MAX_TEXT);
    !out.is_empty()
}

fn u16_at(b: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize)
}

fn u32_at(b: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize)
}

/// A ZIP entry: name, method, packed and unpacked sizes, where its local
/// header is.
struct Entry<'a> {
    name: &'a [u8],
    method: usize,
    packed: usize,
    size: usize,
    at: usize,
}

/// The entries the central directory lists (not ZIP64: documents are small).
fn entries(zip: &[u8]) -> Option<Vec<Entry<'_>>> {
    let last = zip.len().checked_sub(22)?;
    let eocd = (last.saturating_sub(65_535)..=last).rev().find(|&i| zip[i..i + 4] == *b"PK\x05\x06")?;
    let (count, mut at) = (u16_at(zip, eocd + 10)?, u32_at(zip, eocd + 16)?);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if zip.get(at..at + 4)? != b"PK\x01\x02" {
            return None;
        }
        let (n, extra, comment) = (u16_at(zip, at + 28)?, u16_at(zip, at + 30)?, u16_at(zip, at + 32)?);
        out.push(Entry {
            name: zip.get(at + 46..at + 46 + n)?,
            method: u16_at(zip, at + 10)?,
            packed: u32_at(zip, at + 20)?,
            size: u32_at(zip, at + 24)?,
            at: u32_at(zip, at + 42)?,
        });
        at += 46 + n + extra + comment;
    }
    Some(out)
}

/// An entry's bytes, stored or deflated.
fn unpack(zip: &[u8], e: &Entry) -> Option<Vec<u8>> {
    if zip.get(e.at..e.at + 4)? != b"PK\x03\x04" {
        return None;
    }
    let start = e.at + 30 + u16_at(zip, e.at + 26)? + u16_at(zip, e.at + 28)?;
    let data = zip.get(start..start + e.packed)?;
    match e.method {
        0 => Some(data.to_vec()),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, e.size.min(MAX_PART)).ok(),
        _ => None,
    }
}

/// The text parts of an Office or OpenDocument file, in reading order.
fn office(zip: &[u8], kind: Kind, out: &mut Vec<u8>) {
    let Some(entries) = entries(zip) else { return };
    let part = |name: &[u8]| -> bool {
        let under = |dir: &[u8]| name.starts_with(dir) && name.ends_with(b".xml");
        match kind {
            Kind::Word => {
                name == b"word/document.xml" || [&b"word/header"[..], b"word/footer", b"word/footnotes", b"word/endnotes"].iter().any(|p| under(p))
            }
            // The text of every cell is in the shared strings.
            Kind::Excel => name == b"xl/sharedStrings.xml",
            Kind::PowerPoint => under(b"ppt/slides/slide") || under(b"ppt/notesSlides/notesSlide"),
            Kind::OpenDocument => name == b"content.xml",
            Kind::Pdf => false,
        }
    };
    let mut parts: Vec<&Entry> = entries.iter().filter(|e| part(e.name)).collect();
    // slide2 before slide10.
    parts.sort_by_key(|e| {
        let digits = e.name.iter().rev().skip(4).take_while(|b| b.is_ascii_digit()).count();
        let number: usize = std::str::from_utf8(&e.name[e.name.len() - 4 - digits..e.name.len() - 4]).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        (e.name[..e.name.len() - 4 - digits].to_vec(), number)
    });
    for e in parts {
        if let Some(xml) = unpack(zip, e) {
            xml_text(&xml, out);
        }
        if out.len() >= MAX_TEXT {
            break;
        }
    }
}

/// The character data of an XML part, a line per paragraph: tags go, a
/// paragraph's end (`</w:p>`, `</a:p>`, `</text:p>`, a shared string) makes a
/// line break, a tab or a cell boundary a tab. Field codes are left out.
pub fn xml_text(xml: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    let mut skip = 0usize;
    let newline = |out: &mut Vec<u8>| {
        if out.last().is_some_and(|&b| b != b'\n') {
            out.push(b'\n');
        }
    };
    while i < xml.len() {
        if xml[i] != b'<' {
            let end = memchr::memchr(b'<', &xml[i..]).map_or(xml.len(), |p| i + p);
            if skip == 0 {
                unescape(&xml[i..end], out);
            }
            i = end;
            continue;
        }
        let end = memchr::memchr(b'>', &xml[i..]).map_or(xml.len(), |p| i + p);
        let tag = &xml[i + 1..end];
        i = end + 1;
        let (closing, selfclosing) = (tag.first() == Some(&b'/'), tag.last() == Some(&b'/'));
        let name = tag.trim_ascii_start().strip_prefix(b"/").unwrap_or(tag);
        let name = &name[..name.iter().position(|&b| b == b' ' || b == b'/' || b == b'\t' || b == b'\n').unwrap_or(name.len())];
        let local = &name[name.iter().rposition(|&b| b == b':').map_or(0, |p| p + 1)..];
        match local {
            b"instrText" | b"delText" if !selfclosing => skip = if closing { skip.saturating_sub(1) } else { skip + 1 },
            b"p" | b"h" | b"si" | b"tr" | b"table-row" if closing => newline(out),
            b"br" | b"line-break" | b"cr" if selfclosing => newline(out),
            b"tab" if selfclosing => out.push(b'\t'),
            b"tc" | b"table-cell" if closing => out.push(b'\t'),
            b"s" if selfclosing && name.starts_with(b"text:") => out.push(b' '),
            _ => {}
        }
    }
}

/// XML character data, entities decoded.
fn unescape(s: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < s.len() {
        let Some(p) = memchr::memchr(b'&', &s[i..]) else {
            out.extend_from_slice(&s[i..]);
            return;
        };
        out.extend_from_slice(&s[i..i + p]);
        i += p;
        let Some(semi) = memchr::memchr(b';', &s[i..(i + 12).min(s.len())]) else {
            out.push(b'&');
            i += 1;
            continue;
        };
        let ent = &s[i + 1..i + semi];
        let c = match ent {
            b"amp" => Some('&'),
            b"lt" => Some('<'),
            b"gt" => Some('>'),
            b"quot" => Some('"'),
            b"apos" => Some('\''),
            _ => {
                let n = std::str::from_utf8(ent.strip_prefix(b"#").unwrap_or(b"")).ok();
                n.and_then(|n| n.strip_prefix(['x', 'X']).map_or_else(|| n.parse().ok(), |h| u32::from_str_radix(h, 16).ok()))
                    .and_then(char::from_u32)
            }
        };
        match c {
            Some(c) => {
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                i += semi + 1;
            }
            None => {
                out.push(b'&');
                i += 1;
            }
        }
    }
}

/// A COM object, released on drop, called by vtable slot (windows-sys has
/// no COM interfaces).
struct Com(*mut c_void);

impl Com {
    unsafe fn method<F: Copy>(&self, i: usize) -> F {
        unsafe {
            let vtbl = *(self.0 as *const *const usize);
            std::mem::transmute_copy(&*vtbl.add(i))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { self.method::<unsafe extern "system" fn(*mut c_void) -> u32>(2)(self.0) };
    }
}

impl Com {
    /// Another interface of the same object, if it has it.
    fn query(&self, iid: &GUID) -> Option<Com> {
        type QueryInterface = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
        let mut obj = null_mut();
        (unsafe { self.method::<QueryInterface>(0)(self.0, iid, &mut obj) } >= 0 && !obj.is_null()).then(|| Com(obj))
    }
}

const IID_IFILTER: GUID = GUID::from_u128(0x89bcb740_6119_101a_bcb7_00dd010655af);
const IID_INITIALIZE_WITH_STREAM: GUID = GUID::from_u128(0xb824b49d_22ac_4161_ac8a_9916e8fa3f7f);
const IID_PERSIST_STREAM: GUID = GUID::from_u128(0x00000109_0000_0000_c000_000000000046);
const IID_PERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);

/// The filter class Windows search uses for .pdf: the extension's
/// persistent handler, then the IFilter that handler registers.
fn pdf_filter() -> Option<GUID> {
    static CLSID: std::sync::OnceLock<Option<GUID>> = std::sync::OnceLock::new();
    *CLSID.get_or_init(|| {
        let handler = class_default(r".pdf\PersistentHandler")?;
        let filter = class_default(&format!(r"CLSID\{handler}\PersistentAddinsRegistered\{{89BCB740-6119-101A-BCB7-00DD010655AF}}"))?;
        let w: Vec<u16> = filter.encode_utf16().chain([0]).collect();
        let mut clsid: GUID = unsafe { zeroed() };
        (unsafe { CLSIDFromString(w.as_ptr(), &mut clsid) } >= 0).then_some(clsid)
    })
}

/// The default value of an HKEY_CLASSES_ROOT key.
fn class_default(key: &str) -> Option<String> {
    let k: Vec<u16> = key.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 128];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe { RegGetValueW(HKEY_CLASSES_ROOT, k.as_ptr(), null(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr().cast(), &mut size) } == 0;
    ok.then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
}

/// The PDF filter, loaded with the file: as a stream (how Windows search
/// hands it files, and all that Windows.Data.Pdf takes), else by name.
fn open_filter(path: &[u8]) -> Option<Com> {
    type InitializeWithStream = unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> HRESULT;
    type LoadStream = unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT;
    type LoadFile = unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT;
    let clsid = pdf_filter()?;
    let mut obj = null_mut();
    if unsafe { CoCreateInstance(&clsid, null_mut(), CLSCTX_INPROC_SERVER, &IID_IFILTER, &mut obj) } < 0 || obj.is_null() {
        return None;
    }
    let f = Com(obj);
    let w = crate::paths::to_wide(path);
    let mut stream = null_mut();
    let opened = unsafe { SHCreateStreamOnFileEx(w.as_ptr(), STGM_READ | STGM_SHARE_DENY_NONE, 0, 0, null_mut(), &mut stream) } >= 0;
    let stream = opened.then(|| Com(stream));
    let loaded = unsafe {
        match (&stream, f.query(&IID_INITIALIZE_WITH_STREAM), f.query(&IID_PERSIST_STREAM), f.query(&IID_PERSIST_FILE)) {
            (Some(s), Some(i), _, _) => i.method::<InitializeWithStream>(3)(i.0, s.0, STGM_READ),
            (Some(s), None, Some(p), _) => p.method::<LoadStream>(5)(p.0, s.0),
            (_, _, _, Some(p)) => p.method::<LoadFile>(5)(p.0, w.as_ptr(), STGM_READ),
            _ => return None,
        }
    };
    (loaded >= 0).then_some(f)
}

thread_local! {
    /// COM, once per thread, for the filters.
    static COM: () = unsafe {
        CoInitializeEx(null(), COINIT_MULTITHREADED as _);
    };
}

/// A PDF's text through the filter Windows registers for .pdf.
fn pdf(path: &[u8], out: &mut Vec<u8>) {
    // IFilter: Init 3, GetChunk 4, GetText 5.
    type Init = unsafe extern "system" fn(*mut c_void, u32, u32, *const FULLPROPSPEC, *mut u32) -> HRESULT;
    type GetChunk = unsafe extern "system" fn(*mut c_void, *mut STAT_CHUNK) -> HRESULT;
    type GetText = unsafe extern "system" fn(*mut c_void, *mut u32, *mut u16) -> HRESULT;
    COM.with(|_| {});
    let Some(f) = open_filter(path) else { return };
    let flags = IFILTER_INIT_CANON_PARAGRAPHS | IFILTER_INIT_HARD_LINE_BREAKS | IFILTER_INIT_CANON_SPACES | IFILTER_INIT_APPLY_INDEX_ATTRIBUTES;
    let mut got = 0;
    if unsafe { f.method::<Init>(3)(f.0, flags as u32, 0, null(), &mut got) } < 0 {
        return;
    }
    let mut buf = vec![0u16; 8192];
    // A damaged file must not keep a filter going for ever.
    for _ in 0..1_000_000 {
        let mut chunk: STAT_CHUNK = unsafe { zeroed() };
        let hr = unsafe { f.method::<GetChunk>(4)(f.0, &mut chunk) };
        if hr == FILTER_E_END_OF_CHUNKS {
            break;
        }
        if hr < 0 {
            if hr == FILTER_E_EMBEDDING_UNAVAILABLE || hr == FILTER_E_LINK_UNAVAILABLE {
                continue;
            }
            break;
        }
        if chunk.flags != CHUNK_TEXT {
            continue;
        }
        if chunk.breakType != CHUNK_NO_BREAK && out.last().is_some_and(|&b| b != b'\n') {
            out.push(b'\n');
        }
        loop {
            let mut n = buf.len() as u32;
            let hr = unsafe { f.method::<GetText>(5)(f.0, &mut n, buf.as_mut_ptr()) };
            if hr < 0 {
                break;
            }
            let text = String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]).replace("\r\n", "\n").replace('\r', "\n");
            out.extend_from_slice(text.as_bytes());
            if out.len() >= MAX_TEXT || hr == FILTER_S_LAST_TEXT {
                break;
            }
        }
        if out.len() >= MAX_TEXT {
            break;
        }
    }
}
