// SPDX-License-Identifier: AGPL-3.0-only
//! Persistent reading positions.
//!
//! The progress file lives at `$XDG_STATE_HOME/mureader/progress` (usually
//! `~/.local/state/mureader/progress`). `MUREADER_PROGRESS` overrides that
//! path. It follows the XDG Base Directory specification: a relative
//! `XDG_STATE_HOME` is ignored.
//!
//! The format is deliberately close to Vim's viminfo file marks: comment
//! lines start with `#`, and each mark starts with `'` followed by the page
//! number and the document path. Marks are stored newest first.
//!
//! ```text
//! # MuReader reading progress.
//! '42 /home/user/Documents/book.pdf
//! '7 /home/user/Documents/notes.epub
//! ```
//!
//! Paths are the last field, so spaces need no quoting. Backslashes, tabs,
//! and line breaks are escaped as `\\`, `\t`, `\n`, and `\r`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

const ENV_PROGRESS: &str = "MUREADER_PROGRESS";
/// Prefix for paths that are not valid UTF-8. The rest of the mark is the
/// path's bytes in lowercase hexadecimal.
const BYTES_PREFIX: &str = "@bytes:";
/// Keep the history bounded. Vim keeps ten file marks; a reader can
/// reasonably remember more, but the file should not grow without limit.
const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    page: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ProgressStore {
    file: Option<PathBuf>,
    entries: Vec<Entry>,
}

impl ProgressStore {
    pub fn load() -> Self {
        Self::from_file(progress_path())
    }

    fn from_file(file: Option<PathBuf>) -> Self {
        let entries = file
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|text| parse(&text))
            .unwrap_or_default();
        Self { file, entries }
    }

    /// The last recorded page for a document, if any.
    pub fn page_for(&self, document: &Path) -> Option<usize> {
        let key = canonicalize(document);
        self.entries
            .iter()
            .find(|entry| entry.path == key)
            .map(|entry| entry.page)
    }

    /// Record a document's current page. The mark is moved to the front and
    /// the file is rewritten immediately so an abrupt exit cannot lose it.
    pub fn record(&mut self, document: &Path, page: usize) {
        if page == 0 {
            return;
        }
        let key = canonicalize(document);
        if let Some(index) = self.entries.iter().position(|entry| entry.path == key) {
            if index == 0 && self.entries[index].page == page {
                return;
            }
            self.entries.remove(index);
        }
        self.entries.insert(0, Entry { path: key, page });
        self.entries.truncate(MAX_ENTRIES);
        self.save();
    }

    fn save(&self) {
        let Some(path) = self.file.as_ref() else {
            return;
        };
        if let Err(error) = write_atomic(path, &render(&self.entries)) {
            eprintln!(
                "MuReader: cannot write progress {}: {error}",
                path.display()
            );
        }
    }
}

fn progress_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os(ENV_PROGRESS) {
        return Some(PathBuf::from(path));
    }

    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".local").join("state"))
        })?;
    Some(base.join("mureader").join("progress"))
}

fn canonicalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn parse(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(mark) = line.strip_prefix('\'') else {
            continue;
        };
        let Some((page, path)) = mark.split_once(' ') else {
            continue;
        };
        let Ok(page) = page.parse::<usize>() else {
            continue;
        };
        if page == 0 {
            continue;
        }
        let path = parse_path(path);
        if path.as_os_str().is_empty() || entries.iter().any(|entry| entry.path == path) {
            continue;
        }
        entries.push(Entry { path, page });
        if entries.len() == MAX_ENTRIES {
            break;
        }
    }
    entries
}

fn render(entries: &[Entry]) -> String {
    let mut text = String::from(
        "# MuReader reading progress.\n\
         # Written automatically; lines starting with ' are file marks stored\n\
         # newest first: '<page> <path>.\n",
    );
    for entry in entries {
        text.push('\'');
        text.push_str(&entry.page.to_string());
        text.push(' ');
        text.push_str(&format_path(&entry.path));
        text.push('\n');
    }
    text
}

fn format_path(path: &Path) -> String {
    if let Some(text) = path.to_str() {
        return escape_text(text);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut encoded = String::from(BYTES_PREFIX);
        for byte in path.as_os_str().as_bytes() {
            let _ = write!(encoded, "{byte:02x}");
        }
        encoded
    }
    #[cfg(not(unix))]
    {
        escape_text(&path.to_string_lossy())
    }
}

fn parse_path(value: &str) -> PathBuf {
    #[cfg(unix)]
    if let Some(hex) = value.strip_prefix(BYTES_PREFIX) {
        use std::os::unix::ffi::OsStringExt;
        if let Some(bytes) = decode_hex(hex) {
            return PathBuf::from(std::ffi::OsString::from_vec(bytes));
        }
    }
    unescape_path(value)
}

#[cfg(unix)]
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let pair = std::str::from_utf8(pair).ok()?;
        decoded.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(decoded)
}

fn escape_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn unescape_path(value: &str) -> PathBuf {
    let mut path = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            path.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => path.push('\n'),
            Some('r') => path.push('\r'),
            Some('t') => path.push('\t'),
            Some('\\') => path.push('\\'),
            Some(other) => {
                path.push('\\');
                path.push(other);
            }
            None => path.push('\\'),
        }
    }
    PathBuf::from(path)
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_dir(name: &str) -> PathBuf {
        let serial = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mureader-{name}-{}-{serial}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_viminfo_style_file_marks() {
        let entries = parse(
            "# MuReader reading progress.\n\
             \n\
             '12 /home/user/books/one.pdf\n\
             '3 /home/user/books/two words.epub\n\
             'not-a-page /ignored\n\
             '0 /ignored\n\
             random noise\n\
             '4 /home/user/books/one.pdf\n",
        );
        assert_eq!(
            entries,
            vec![
                Entry {
                    path: PathBuf::from("/home/user/books/one.pdf"),
                    page: 12,
                },
                Entry {
                    path: PathBuf::from("/home/user/books/two words.epub"),
                    page: 3,
                },
            ]
        );
    }

    #[test]
    fn round_trips_paths_with_spaces_and_escapes() {
        let dir = test_dir("progress");
        let file = dir.join("progress");
        let mut store = ProgressStore::from_file(Some(file.clone()));
        let title = "a book\\with\todd\nname.pdf";
        let document = dir.join(title);
        fs::write(&document, b"x").unwrap();

        store.record(&document, 42);
        let loaded = ProgressStore::from_file(Some(file));
        assert_eq!(loaded.page_for(&document), Some(42));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn round_trips_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = test_dir("progress-bytes");
        let file = dir.join("progress");
        let name = OsString::from_vec(b"book-\xff.pdf".to_vec());
        let document = dir.join(name);
        fs::write(&document, b"x").unwrap();

        let mut store = ProgressStore::from_file(Some(file.clone()));
        store.record(&document, 5);
        let loaded = ProgressStore::from_file(Some(file));
        assert_eq!(loaded.page_for(&document), Some(5));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_document_comes_first_and_repeats_move_to_front() {
        let entries = parse("'1 /a\n'2 /b\n");
        assert_eq!(entries[0].path, PathBuf::from("/a"));
        assert_eq!(entries[1].path, PathBuf::from("/b"));

        let parsed = parse(&render(&[
            Entry {
                path: PathBuf::from("/b"),
                page: 9,
            },
            Entry {
                path: PathBuf::from("/a"),
                page: 1,
            },
        ]));
        assert_eq!(parsed[0].path, PathBuf::from("/b"));
        assert_eq!(parsed[0].page, 9);
    }
}
