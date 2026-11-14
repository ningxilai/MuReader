// SPDX-License-Identifier: AGPL-3.0-only
//! Auto-generated sidecar stylesheets.
//!
//! A sidecar is a stylesheet named after the SHA-256 of the document bytes:
//! `$XDG_DATA_HOME/mureader/css/<sha256>.css`. Nothing in the config names
//! it, so a book and its stylesheet travel together by content rather than by
//! path. `MUREADER_CSS_DIR` overrides the directory; the old
//! `MUREADER_CSS_CACHE` name is still accepted for migration.
//!
//! A missing sidecar is created on the first open of a reflowable document
//! from the legacy config CSS and palette. After that the file belongs to the
//! user: it is authoritative even when it is empty or contains only comments.
//!
//! Only reflowable formats are looked up. Fixed-layout documents keep their
//! own CSS and are never affected by a stale sidecar.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Refuse to load an unbounded stylesheet. MuPDF parses user CSS eagerly, so a
/// huge file would stall the worker before the first page is rendered.
const MAX_SIDECAR_BYTES: u64 = 4 * 1024 * 1024;

/// The first line that makes a sidecar replace the document's own stylesheet
/// instead of overlaying it.
pub const REPLACE_DOCUMENT_CSS_MARKER: &str = "/* mureader: replace-document-css */";

/// Extensions MuPDF renders as reflowable HTML or text.
const REFLOWABLE_EXTENSIONS: &[&str] = &[
    "epub", "htm", "html", "xhtml", "fb2", "mobi", "docx", "xlsx", "pptx", "hwpx", "txt", "text",
    "log",
];

/// What a sidecar lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// The file exists and was read successfully.
    Present(String),
    /// No sidecar exists yet; the caller may seed one.
    Missing,
    /// A sidecar exists but cannot be used. The document's own CSS is the
    /// fallback, never the legacy config CSS.
    Failed,
}

/// The directory that holds sidecar stylesheets.
pub fn css_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("MUREADER_CSS_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    // Legacy name from the cache-based sidecar model.
    if let Some(dir) = env::var_os("MUREADER_CSS_CACHE").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    xdg_data_home(env::var_os("XDG_DATA_HOME"), env::var_os("HOME"))
        .map(|base| base.join("mureader").join("css"))
}

/// Resolve the XDG data root. A relative `XDG_DATA_HOME` is invalid per the
/// specification, so it is ignored in favour of the home fallback.
fn xdg_data_home(value: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local").join("share")))
}

/// Whether MuPDF treats this path as reflowable. Pretty-text `.log` files are
/// rejected later by the engine; the extension list still describes what the
/// text handler would claim.
pub fn is_reflowable_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            REFLOWABLE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Lowercase SHA-256 of the file contents.
pub fn document_hash(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The sidecar path for a document in `dir`, whether or not the file exists.
pub fn file_for(document: &Path, dir: &Path) -> io::Result<Option<PathBuf>> {
    if !is_reflowable_path(document) {
        return Ok(None);
    }
    Ok(Some(dir.join(format!("{}.css", document_hash(document)?))))
}

/// Whether the sidecar asks MuPDF to ignore the document's own stylesheet.
/// Only the first line counts; leading whitespace and a UTF-8 BOM are
/// tolerated so the marker survives hand editing.
pub fn replaces_document_css(css: &str) -> bool {
    let css = css.strip_prefix('\u{feff}').unwrap_or(css);
    css.lines()
        .next()
        .is_some_and(|line| line.trim() == REPLACE_DOCUMENT_CSS_MARKER)
}

/// Load the sidecar for a document from the default directory. Returns `None`
/// for formats that never use a sidecar.
pub fn load(document: &Path) -> Option<Loaded> {
    if !is_reflowable_path(document) {
        return None;
    }
    Some(match css_dir() {
        Some(dir) => load_from_dir(document, &dir),
        None => Loaded::Missing,
    })
}

/// Load the sidecar for a document from a specific directory. A missing
/// sidecar is normal and silent; an unreadable or oversized one is reported
/// and reported as [`Loaded::Failed`].
pub fn load_from_dir(document: &Path, dir: &Path) -> Loaded {
    if !is_reflowable_path(document) {
        return Loaded::Missing;
    }
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            eprintln!(
                "MuReader: cannot use sidecar directory {}: not a directory",
                dir.display()
            );
            return Loaded::Failed;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Loaded::Missing,
        Err(error) => {
            eprintln!(
                "MuReader: cannot inspect sidecar directory {}: {error}",
                dir.display()
            );
            return Loaded::Failed;
        }
    }
    let path = match file_for(document, dir) {
        Ok(Some(path)) => path,
        Ok(None) => return Loaded::Missing,
        Err(error) => {
            eprintln!(
                "MuReader: cannot hash {} for sidecar CSS: {error}",
                document.display()
            );
            return Loaded::Failed;
        }
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Loaded::Missing,
        Err(error) => {
            eprintln!(
                "MuReader: cannot inspect sidecar CSS {}: {error}",
                path.display()
            );
            return Loaded::Failed;
        }
    };
    if !metadata.is_file() {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: not a regular file",
            path.display()
        );
        return Loaded::Failed;
    }
    if metadata.len() > MAX_SIDECAR_BYTES {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: {} bytes exceeds the {} byte limit",
            path.display(),
            metadata.len(),
            MAX_SIDECAR_BYTES
        );
        return Loaded::Failed;
    }
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "MuReader: cannot read sidecar CSS {}: {error}",
                path.display()
            );
            return Loaded::Failed;
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file
        .by_ref()
        .take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        eprintln!(
            "MuReader: cannot read sidecar CSS {}: {error}",
            path.display()
        );
        return Loaded::Failed;
    }
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: exceeds the {} byte limit",
            path.display(),
            MAX_SIDECAR_BYTES
        );
        return Loaded::Failed;
    }
    match String::from_utf8(bytes) {
        Ok(css) => Loaded::Present(css),
        Err(error) => {
            eprintln!(
                "MuReader: ignoring sidecar CSS {}: not UTF-8: {error}",
                path.display()
            );
            Loaded::Failed
        }
    }
}

/// Write the first sidecar for a document. Returns `None` for formats that
/// never use a sidecar. An existing file is never overwritten, so a sidecar
/// created by another process (or by the user) stays authoritative.
pub fn write(document: &Path, css: &str) -> io::Result<Option<PathBuf>> {
    let Some(dir) = css_dir() else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "no data directory for sidecar CSS",
        ));
    };
    write_to_dir(document, &dir, css)
}

/// Write a sidecar into a specific directory. The file is written to a
/// temporary neighbour and linked into place, so a crash cannot leave a
/// partial stylesheet under the real name.
pub fn write_to_dir(document: &Path, dir: &Path, css: &str) -> io::Result<Option<PathBuf>> {
    let Some(path) = file_for(document, dir)? else {
        return Ok(None);
    };
    if css.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("sidecar CSS exceeds the {MAX_SIDECAR_BYTES} byte limit"),
        ));
    }
    fs::create_dir_all(dir)?;
    let temp = temp_path(&path);
    let _ = fs::remove_file(&temp);
    fs::write(&temp, css)?;
    let result = match fs::hard_link(&temp, &path) {
        Ok(()) => Ok(()),
        // Another process won the race. Its file is authoritative.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        // Filesystems without hard links still get an atomic rename; the
        // missing-file check above already covered the ordinary case.
        Err(error) if error.kind() == ErrorKind::Unsupported => fs::rename(&temp, &path),
        Err(error) => Err(error),
    };
    let _ = fs::remove_file(&temp);
    result.map(|()| Some(path))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
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
    fn hash_is_lowercase_sha256_of_the_file() {
        let dir = test_dir("sidecar-hash");
        let document = dir.join("book.epub");
        fs::write(&document, b"hello").unwrap();
        assert_eq!(
            document_hash(&document).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_sidecar_named_after_the_document() {
        let dir = test_dir("sidecar-find");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let hash = document_hash(&document).unwrap();
        fs::write(dir.join(format!("{hash}.css")), "body { color: #123456; }").unwrap();

        assert_eq!(
            load_from_dir(&document, &dir),
            Loaded::Present("body { color: #123456; }".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fixed_layout_documents_never_look_up_a_sidecar() {
        let dir = test_dir("sidecar-fixed");
        let document = dir.join("book.pdf");
        fs::write(&document, b"pdf bytes").unwrap();
        let hash = document_hash(&document).unwrap();
        fs::write(dir.join(format!("{hash}.css")), "body { color: red; }").unwrap();

        assert!(!is_reflowable_path(&document));
        assert_eq!(load(&document), None);
        assert_eq!(load_from_dir(&document, &dir), Loaded::Missing);
        assert_eq!(
            write_to_dir(&document, &dir, "body { color: red; }").unwrap(),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sidecar_is_reported_as_missing() {
        let dir = test_dir("sidecar-missing");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        assert_eq!(load_from_dir(&document, &dir), Loaded::Missing);
        assert_eq!(
            load_from_dir(&document, &dir.join("absent")),
            Loaded::Missing
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_sidecar_is_rejected() {
        let dir = test_dir("sidecar-large");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let hash = document_hash(&document).unwrap();
        let sidecar = dir.join(format!("{hash}.css"));
        let file = File::create(&sidecar).unwrap();
        file.set_len(MAX_SIDECAR_BYTES + 1).unwrap();
        drop(file);

        assert_eq!(load_from_dir(&document, &dir), Loaded::Failed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_utf8_sidecar_is_rejected() {
        let dir = test_dir("sidecar-utf8");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let hash = document_hash(&document).unwrap();
        fs::write(dir.join(format!("{hash}.css")), [0xff, 0xfe, 0x00]).unwrap();

        assert_eq!(load_from_dir(&document, &dir), Loaded::Failed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_a_sidecar_once_and_keeps_an_existing_file() {
        let dir = test_dir("sidecar-write");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();

        let path = write_to_dir(&document, &dir, "body { color: #123456; }")
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "body { color: #123456; }"
        );

        // A second seed must not overwrite what the user now owns.
        write_to_dir(&document, &dir, "body { color: #abcdef; }").unwrap();
        assert_eq!(
            load_from_dir(&document, &dir),
            Loaded::Present("body { color: #123456; }".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_marker_must_be_the_first_line() {
        assert!(replaces_document_css(REPLACE_DOCUMENT_CSS_MARKER));
        assert!(replaces_document_css(&format!(
            "  {REPLACE_DOCUMENT_CSS_MARKER}  \r\nbody {{ color: red; }}"
        )));
        assert!(replaces_document_css(&format!(
            "\u{feff}{REPLACE_DOCUMENT_CSS_MARKER}\nbody {{ color: red; }}"
        )));
        assert!(!replaces_document_css(&format!(
            "\n{REPLACE_DOCUMENT_CSS_MARKER}\nbody {{ color: red; }}"
        )));
        assert!(!replaces_document_css(&format!(
            "body {{ color: red; }}\n{REPLACE_DOCUMENT_CSS_MARKER}"
        )));
        assert!(!replaces_document_css("body { color: red; }"));
    }

    #[test]
    fn relative_xdg_data_home_is_ignored() {
        assert_eq!(
            xdg_data_home(
                Some(OsString::from("relative/share")),
                Some(OsString::from("/home/user"))
            ),
            Some(PathBuf::from("/home/user/.local/share"))
        );
        assert_eq!(
            xdg_data_home(
                Some(OsString::from("/data")),
                Some(OsString::from("/home/user"))
            ),
            Some(PathBuf::from("/data"))
        );
        assert_eq!(xdg_data_home(None, None), None);
    }
}
