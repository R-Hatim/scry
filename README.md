# Scry

Whole-disk file search for Windows: to scry is to see what is hidden. Finds
any file on your fixed drives by name in about a millisecond, forgives
typos, and searches inside your text files with an index. Use it from a
search bar on a hotkey, as a CLI (with a small daemon), or as a Rust crate.

A Windows port of [noahdunnagan/fsearch](https://github.com/noahdunnagan/fsearch),
which is macOS-only.

## Install

Download `scry-<version>-setup-x64.exe` from the
[latest release](https://github.com/R-Hatim/scry/releases/latest) and run
it. It needs no administrator rights: it installs for you only
(`%LOCALAPPDATA%\Programs\Scry`), starts Scry when you sign in and puts
the `scry` command on your PATH; both can be unticked. The first run
indexes your drives (about a minute) and a notification says when it is
ready. Uninstall from Settings > Apps.

The builds are not code-signed yet (see the
[code signing policy](#code-signing-policy)), so Windows may say it
protected your PC: **More info**, then **Run anyway**. `SHA256SUMS.txt` in
each release has the checksums.

From source:

```
cargo build --release
target\release\scry install --login   # -> %USERPROFILE%\.local\bin, started at login
scry scry main                        # find files by name
scry "ext:rs grep:apply_dir"          # search inside files
```

## Search bar

**Alt+Shift+Space** brings up a search box in the middle of the screen.
Results come as you type; Up/Down picks one, **Enter** opens it,
**Ctrl+Enter** shows it in Explorer, **Esc** (or a click elsewhere) puts
the bar away. The whole query language works in it (`ext:pdf invoice`,
`grep:TODO in:C:\Workspace`), and when no file name matches, it looks for
the words inside your files instead (filters still apply: `ext:cs
SendInvoiceMail`).

**Ctrl+C** copies the selected file (paste it into Explorer, a mail, a
chat), **Ctrl+Shift+C** its path. What you open or copy ranks higher the
next times, more for what you use often and recently. **Up** in an empty
box brings back your past searches, one by one.

The bar lives in the daemon, `scryd.exe`: it is there whenever the
daemon runs (at login with `install --login`, or from the first search).
Another key: `"hotkey": "Win+Shift+F"` in `config.json`. Without one, the
first free of Alt+Shift+Space, Win+Shift+F and Ctrl+Alt+F is used;
`daemon.log` says which.

## Tray icon

The daemon has an icon in the notification area. Its tooltip says how far
the index is: indexing, how many files and folders it holds, how many text
files are still being read for content search. A click brings up the bar.
The right-click menu shows the same counts and has **Folders…** (see
below), **Start at login** (the same login item as `install --login`) and
**Quit**. After the very first crawl, a notification says when it is ready.

## Speed

i7-11850H laptop, NVMe, Defender on, 5.08M files and folders on C:, a
busy development machine. Searches by name are `scry bench` medians over 20
runs; the rest was measured through the daemon.

| | |
|---|---|
| find a file by name, one word (`invoice`) | 0.5 ms; up to ~8 ms for a word with very many matches to rank |
| with a typo (`mian.rs` finds `main.rs`) | 1.2 ms |
| two words (`src main`) | 7-11 ms |
| with a filter (`ext:pdf invoice`) | 0.07 ms |
| search inside files, warm | 2-10 ms for words, ~60 ms for a regex |
| search inside files, cold disk cache | up to ~1.5 s (matches are read fresh from disk) |
| the largest folders on the whole disk (`kind:dir sort:size`) | 8-20 ms (60 ms the first time) |
| a new, renamed or deleted file shows up | ~0.1 s |
| first crawl of the disk | 50-80 s, once |
| start after a reboot | searchable at once, caught up 0.05-0.5 s later |
| daemon memory | ~22 MB, plus the index mapped from disk (190 MB of names, 71 MB of content for 36k text files; evictable) |

## Queries

```
scry "readme in:C:\Workspace"         # inside a folder
scry "type:image size:>5mb mtime:<7d"
scry "type:app code"                  # programs and Start Menu shortcuts
scry "ext:rs regex:fn\s+\w+_dir"      # regex inside files
scry "sym:apply_dir"                  # where it's defined
scry "kind:dir sort:size in:~"         # the biggest folders in your user folder
scry "kind:file sort:size"             # the largest files on every drive
scry "ext:log sort:date"               # the newest logs
```

Words are fuzzy, and 5+ letter words forgive one typo (`mian.rs` finds
`main.rs`). A `\` or `/` splits a word into folder tokens (`src\main`).
Also `'exact`, `^prefix`, `suffix$` and `!exclude`. Filters: `ext:` `type:`
`kind:` `in:` `size:` `mtime:` `re:` `path:` `grep:` `regex:` `sym:`
`limit:` `sort:`. `sort:size` puts the largest first, a folder counting
everything under it (sizes as files report them: a hard-linked file counts
each time); `sort:date` the most recently modified. Quote paths with spaces: `in:"C:\Program Files"`. `path:` regexes
see `C:/Users/...`, with forward slashes. Content search is smart-case.

## Your folders

Content search covers the text files in your folders, and what is in them
ranks first. By default that is your user folder. Choose them, and the
folders to leave out entirely, in **Folders…** on the tray icon (Scry
restarts to apply them), or in `%LOCALAPPDATA%\Scry\config.json`, then
`scry stop` (the next search restarts the daemon):

```json
{"roots": ["~", "C:\\Workspace"], "skip": ["D:\\Backups"], "hotkey": "Alt+Shift+Space"}
```

`skip` lists folders never to open: what is in them is not found, by name
or by content. Folders outside your roots are still searched by name, and
`grep:` with an `in:` there reads the files directly.
Build output, dependencies and app data (`node_modules`, `.git`, `obj`,
`AppData`, ...) are left out of the content index, and OneDrive files that
are online-only are never downloaded by a search.

## Login item and elevation

`scry install --login` also starts the daemon at login (a `Run` entry
for `scryd.exe`, which has no console window, only the search bar and
the tray icon); `scry uninstall` removes it, `scry stop` ends the
running one. The daemon indexes what your account can read. Run it
elevated and it turns on the backup privilege and indexes everything; set
`SCRY_RESTRICT=1` to keep it to what you can read anyway.

## API

JSON lines over a named pipe (`scry::server::pipe_name`), or `scry stdio`:

```json
{"q": "scry main", "limit": 20}
{"op": "grep", "pattern": "apply_dir", "in": "~\\Developer"}
{"op": "status"}
```

Or link the crate:

```rust
let home = scry::default_home();
let opts = scry::server::options(&scry::default_dir(&home), &home);
let engine = scry::Engine::start(opts)?;
let hits = engine.search(&scry::Query::parse("scry main", &home)?)?;
```

An app and the CLI share one index: the first process owns it and the
others follow along.

## How it works

- Crawls every fixed drive once. Each folder is opened relative to its
  parent's handle (`NtOpenFile`, NT's `openat`) and listed in bulk with
  `GetFileInformationByHandleEx`: names, sizes and times in a few calls per
  folder, no per-file stat. Junctions, symlinks and mount points are kept
  as entries, never entered.
- Stays current from `ReadDirectoryChangesW` on every drive root. Each
  change names a folder, which is relisted and diffed against the index.
- After a restart, or when more changed at once than the change buffer
  holds, the NTFS change journal (readable without admin) names the folders
  that changed since the index was last in sync. Drives without one are
  checked folder by folder.
- A busy disk's change journal may hold only a few hours, so what changed
  since the last full save of the index is checkpointed every five minutes
  and at sign-out (a few MB): a restart needs only the last minutes.
- Names live in one mmap'd file, laid out folder by folder so `in:` is a
  range. Each distinct name is scored once.
- Content search uses a trigram index of your text files. Matches are read
  fresh from disk, so they're never stale.

## Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io/),
certificate by [SignPath Foundation](https://signpath.org/). (Applied for;
v0.1.0 is not signed.)

Releases are built from this repository by GitHub Actions
([release.yml](.github/workflows/release.yml)) from a version tag; nothing
released is built on a personal machine.

- Committers and reviewers: [@R-Hatim](https://github.com/R-Hatim)
- Approvers: [@R-Hatim](https://github.com/R-Hatim)

Changes from anyone else come in as pull requests, reviewed by a committer
before they are merged. Every signing request is approved by hand.

## Privacy

Scry does not transfer any information to other networked systems unless
specifically requested by the user or the person installing or operating
it. The index and settings stay on your PC, in `%LOCALAPPDATA%\Scry`, and
the daemon answers the `scry` command over a local named pipe that only
your account can open.

## Building

Rust 1.89+ with the MSVC toolchain, Windows 10 1809 or later. `cargo test`
runs against throwaway folders in `%TEMP%`. The icon is `assets\scry.ico`,
drawn by `assets\icon.ps1`; `build.rs` embeds it and the version (from
`Cargo.toml`) in both programs, without a resource compiler.

`installer\scry.iss` is the Inno Setup script for the installer (`iscc
/DAppVersion=0.1.0 installer\scry.iss` after a release build). A tag like
`v0.1.0`, matching `Cargo.toml`, makes GitHub Actions build the installer
and a zip and attach them to a draft release.