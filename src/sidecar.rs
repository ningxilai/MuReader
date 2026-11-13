// SPDX-License-Identifier: AGPL-3.0-only
//! Auto-discovered sidecar stylesheets.
//!
//! A sidecar is a stylesheet named after the SHA-256 of the document bytes:
//! `$XDG_CACHE_HOME/mureader/css/<sha256>.css`. Nothing in the config names
//! it, so a book and its replacement stylesheet travel together by content
//! rather than by path. `MUREADER_CSS_CACHE` overrides the directory.
//!
//! Only reflowable formats are looked up. Fixed-layout documents keep their
//! own CSS and are never affected by a stale sidecar.

use std::env;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Refuse to load an unbounded stylesheet. MuPDF parses user CSS eagerly, so a
/// huge file would stall the worker before the first page is rendered.
const MAX_SIDECAR_BYTES: u64 = 4 * 1024 * 1024;

/// Extensions MuPDF renders as reflowable HTML or text.
const REFLOWABLE_EXTENSIONS: &[&str] = &[
    "epub", "htm", "html", "xhtml", "fb2", "mobi", "docx", "xlsx", "pptx", "hwpx", "txt", "text",
    "log",
];

/// The directory that holds sidecar stylesheets.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("MUREADER_CSS_CACHE") {
        return Some(PathBuf::from(dir));
    }
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("mureader").join("css"))
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

/// Load the sidecar for a document from the default cache directory.
pub fn load(document: &Path) -> Option<String> {
    let dir = cache_dir()?;
    load_from_dir(document, &dir)
}

/// Load the sidecar for a document from a specific cache directory. A missing
/// sidecar is normal and silent; an unreadable or oversized one falls back to
/// the configured CSS.
pub fn load_from_dir(document: &Path, dir: &Path) -> Option<String> {
    if !is_reflowable_path(document) {
        return None;
    }
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return None,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(error) => {
            eprintln!(
                "MuReader: cannot inspect sidecar directory {}: {error}",
                dir.display()
            );
            return None;
        }
    }
    let path = match file_for(document, dir) {
        Ok(Some(path)) => path,
        Ok(None) => return None,
        Err(error) => {
            eprintln!(
                "MuReader: cannot hash {} for sidecar CSS: {error}",
                document.display()
            );
            return None;
        }
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(error) => {
            eprintln!(
                "MuReader: cannot inspect sidecar CSS {}: {error}",
                path.display()
            );
            return None;
        }
    };
    if !metadata.is_file() {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: not a regular file",
            path.display()
        );
        return None;
    }
    if metadata.len() > MAX_SIDECAR_BYTES {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: {} bytes exceeds the {} byte limit",
            path.display(),
            metadata.len(),
            MAX_SIDECAR_BYTES
        );
        return None;
    }
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "MuReader: cannot read sidecar CSS {}: {error}",
                path.display()
            );
            return None;
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
        return None;
    }
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: exceeds the {} byte limit",
            path.display(),
            MAX_SIDECAR_BYTES
        );
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(css) => Some(css),
        Err(error) => {
            eprintln!(
                "MuReader: ignoring sidecar CSS {}: not UTF-8: {error}",
                path.display()
            );
            None
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
            load_from_dir(&document, &dir).as_deref(),
            Some("body { color: #123456; }")
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
        assert_eq!(load_from_dir(&document, &dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sidecar_is_silent() {
        let dir = test_dir("sidecar-missing");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        assert_eq!(load_from_dir(&document, &dir), None);
        assert_eq!(load_from_dir(&document, &dir.join("absent")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_sidecar_is_ignored() {
        let dir = test_dir("sidecar-large");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let hash = document_hash(&document).unwrap();
        let sidecar = dir.join(format!("{hash}.css"));
        let file = File::create(&sidecar).unwrap();
        file.set_len(MAX_SIDECAR_BYTES + 1).unwrap();
        drop(file);

        assert_eq!(load_from_dir(&document, &dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
