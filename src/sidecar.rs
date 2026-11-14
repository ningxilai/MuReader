// SPDX-License-Identifier: AGPL-3.0-only
//! Auto-generated sidecar stylesheets.
//!
//! A sidecar is a stylesheet named after the document's embedded name and the
//! SHA-256 of its bytes:
//! `$XDG_DATA_HOME/mureader/css/<name>-<sha256>.css`. For EPUB, `name` is the
//! title stored in the book's own metadata, not the file name. Other formats
//! use the file stem. Nothing in the config names the sidecar, so a book and
//! its stylesheet travel together by content rather than by path.
//! `MUREADER_CSS_DIR` overrides the directory.
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

/// Keep the readable part of a sidecar name well inside the usual 255-byte
/// filename limit, leaving room for the hyphen, 64-hex hash, and `.css`.
const MAX_NAME_BYTES: usize = 120;

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
    /// No sidecar exists yet; the caller may seed one at this path.
    Missing(PathBuf),
    /// A sidecar exists but cannot be used. The document's own CSS is the
    /// fallback, never the legacy config CSS.
    Failed,
}

/// The directory that holds sidecar stylesheets.
pub fn css_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("MUREADER_CSS_DIR").filter(|dir| !dir.is_empty()) {
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
    let hash = document_hash(document)?;
    Ok(Some(
        dir.join(format!("{}-{hash}.css", document_name(document))),
    ))
}

/// The readable part of a sidecar name. EPUB uses the title from the book's
/// own metadata; every other format falls back to the file stem.
fn document_name(document: &Path) -> String {
    let raw = embedded_title(document)
        .or_else(|| {
            document
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    sanitize_name(&raw)
}

/// Read the EPUB title through MuPDF. Opening the book is deferred to a short
/// thread so the UI thread never touches MuPDF. A file that is not a ZIP EPUB
/// is skipped before opening.
fn embedded_title(document: &Path) -> Option<String> {
    let is_epub = document
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"));
    if !is_epub || !is_zip(document) {
        return None;
    }
    let path = document.to_path_buf();
    std::thread::spawn(move || {
        let document = mupdf::Document::open(path.as_path()).ok()?;
        document.metadata(mupdf::MetadataName::Title).ok()
    })
    .join()
    .ok()
    .flatten()
    .map(|title| title.trim().to_string())
    .filter(|title| !title.is_empty())
}

fn is_zip(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == *b"PK\x03\x04"
}

/// Turn a title into one path component. Separators and control characters
/// become dashes, empty names fall back to `document`, and long titles are cut
/// on a UTF-8 boundary.
fn sanitize_name(raw: &str) -> String {
    let mut name = String::new();
    let mut pending_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '\0') {
            pending_dash = !name.is_empty();
            continue;
        }
        if pending_dash && !name.ends_with('-') {
            name.push('-');
        }
        pending_dash = false;
        name.push(ch);
    }
    let name = truncate_name(name.trim_matches(is_name_edge));
    if name.is_empty() || name == "." || name == ".." {
        return "document".to_string();
    }
    if name.starts_with('.') {
        return format!("_{name}");
    }
    name.to_string()
}

fn is_name_edge(ch: char) -> bool {
    ch == '-' || ch == '.' || ch.is_whitespace()
}

fn truncate_name(name: &str) -> &str {
    if name.len() <= MAX_NAME_BYTES {
        return name;
    }
    let mut end = MAX_NAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].trim_end_matches(is_name_edge)
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
        Some(dir) => load_from_dir(document, &dir).unwrap_or(Loaded::Failed),
        None => Loaded::Failed,
    })
}

/// Load the sidecar for a document from a specific directory. Returns `None`
/// for formats that never use a sidecar. A missing sidecar is normal and
/// silent; an unreadable or oversized one is reported as [`Loaded::Failed`].
pub fn load_from_dir(document: &Path, dir: &Path) -> Option<Loaded> {
    if !is_reflowable_path(document) {
        return None;
    }
    let path = match file_for(document, dir) {
        Ok(Some(path)) => path,
        Ok(None) => return None,
        Err(error) => {
            eprintln!(
                "MuReader: cannot hash {} for sidecar CSS: {error}",
                document.display()
            );
            return Some(Loaded::Failed);
        }
    };
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            eprintln!(
                "MuReader: cannot use sidecar directory {}: not a directory",
                dir.display()
            );
            return Some(Loaded::Failed);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Some(Loaded::Missing(path)),
        Err(error) => {
            eprintln!(
                "MuReader: cannot inspect sidecar directory {}: {error}",
                dir.display()
            );
            return Some(Loaded::Failed);
        }
    }
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Some(Loaded::Missing(path)),
        Err(error) => {
            eprintln!(
                "MuReader: cannot inspect sidecar CSS {}: {error}",
                path.display()
            );
            return Some(Loaded::Failed);
        }
    };
    if !metadata.is_file() {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: not a regular file",
            path.display()
        );
        return Some(Loaded::Failed);
    }
    if metadata.len() > MAX_SIDECAR_BYTES {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: {} bytes exceeds the {} byte limit",
            path.display(),
            metadata.len(),
            MAX_SIDECAR_BYTES
        );
        return Some(Loaded::Failed);
    }
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "MuReader: cannot read sidecar CSS {}: {error}",
                path.display()
            );
            return Some(Loaded::Failed);
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
        return Some(Loaded::Failed);
    }
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        eprintln!(
            "MuReader: ignoring sidecar CSS {}: exceeds the {} byte limit",
            path.display(),
            MAX_SIDECAR_BYTES
        );
        return Some(Loaded::Failed);
    }
    Some(match String::from_utf8(bytes) {
        Ok(css) => Loaded::Present(css),
        Err(error) => {
            eprintln!(
                "MuReader: ignoring sidecar CSS {}: not UTF-8: {error}",
                path.display()
            );
            Loaded::Failed
        }
    })
}

/// Write a sidecar to a path returned by [`file_for`]. The file is written to
/// a temporary neighbour and linked into place, so a crash cannot leave a
/// partial stylesheet under the real name. An existing file is never
/// overwritten, so a sidecar created by another process (or by the user) stays
/// authoritative.
pub fn write_path(path: &Path, css: &str) -> io::Result<()> {
    if css.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("sidecar CSS exceeds the {MAX_SIDECAR_BYTES} byte limit"),
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = temp_path(path);
    let _ = fs::remove_file(&temp);
    fs::write(&temp, css)?;
    let result = match fs::hard_link(&temp, path) {
        Ok(()) => Ok(()),
        // Another process won the race. Its file is authoritative.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        // Filesystems without hard links still get an atomic rename; the
        // missing-file check above already covered the ordinary case.
        Err(error) if error.kind() == ErrorKind::Unsupported => fs::rename(&temp, path),
        Err(error) => Err(error),
    };
    let _ = fs::remove_file(&temp);
    result
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
        let path = file_for(&document, &dir).unwrap().unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("book-{hash}.css"),
            "a non-ZIP EPUB falls back to the file stem"
        );
        fs::write(&path, "body { color: #123456; }").unwrap();

        assert_eq!(
            load_from_dir(&document, &dir),
            Some(Loaded::Present("body { color: #123456; }".to_string()))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fixed_layout_documents_never_look_up_a_sidecar() {
        let dir = test_dir("sidecar-fixed");
        let document = dir.join("book.pdf");
        fs::write(&document, b"pdf bytes").unwrap();

        assert!(!is_reflowable_path(&document));
        assert_eq!(load(&document), None);
        assert_eq!(load_from_dir(&document, &dir), None);
        assert_eq!(file_for(&document, &dir).unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sidecar_is_reported_as_missing() {
        let dir = test_dir("sidecar-missing");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        assert!(matches!(
            load_from_dir(&document, &dir),
            Some(Loaded::Missing(_))
        ));
        assert!(matches!(
            load_from_dir(&document, &dir.join("absent")),
            Some(Loaded::Missing(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_sidecar_is_rejected() {
        let dir = test_dir("sidecar-large");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let sidecar = file_for(&document, &dir).unwrap().unwrap();
        let file = File::create(&sidecar).unwrap();
        file.set_len(MAX_SIDECAR_BYTES + 1).unwrap();
        drop(file);

        assert_eq!(load_from_dir(&document, &dir), Some(Loaded::Failed));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_utf8_sidecar_is_rejected() {
        let dir = test_dir("sidecar-utf8");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let sidecar = file_for(&document, &dir).unwrap().unwrap();
        fs::write(&sidecar, [0xff, 0xfe, 0x00]).unwrap();

        assert_eq!(load_from_dir(&document, &dir), Some(Loaded::Failed));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_a_sidecar_once_and_keeps_an_existing_file() {
        let dir = test_dir("sidecar-write");
        let document = dir.join("book.epub");
        fs::write(&document, b"book bytes").unwrap();
        let path = file_for(&document, &dir).unwrap().unwrap();

        write_path(&path, "body { color: #123456; }").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "body { color: #123456; }"
        );

        // A second seed must not overwrite what the user now owns.
        write_path(&path, "body { color: #abcdef; }").unwrap();
        assert_eq!(
            load_from_dir(&document, &dir),
            Some(Loaded::Present("body { color: #123456; }".to_string()))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_name_is_readable_and_safe() {
        assert_eq!(sanitize_name("The Little Prince"), "The Little Prince");
        assert_eq!(sanitize_name("  a/b\\c:d\n"), "a-b-c-d");
        assert_eq!(sanitize_name("   "), "document");
        assert_eq!(sanitize_name("."), "document");
        assert_eq!(sanitize_name(".."), "document");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(sanitize_name("小王子"), "小王子");
    }

    #[test]
    fn long_names_are_cut_on_a_utf8_boundary() {
        let name = sanitize_name(&"小".repeat(100));
        assert!(name.len() <= MAX_NAME_BYTES);
        assert!(name.is_char_boundary(name.len()));
        assert!(!name.is_empty());
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

    /// A minimal, valid EPUB, used to check that the embedded `dc:title` and
    /// not the file name names the sidecar.
    fn write_epub(path: &Path, title: &str) {
        let container = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
        let opf = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="id">urn:uuid:mureader-test</dc:identifier>
<dc:title>{title}</dc:title>
<dc:language>en</dc:language>
</metadata>
<manifest>
<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
<item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine toc="ncx"><itemref idref="chapter"/></spine>
</package>"#
        );
        let ncx = r#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
<head/><docTitle><text>Test</text></docTitle>
<navMap><navPoint id="n1" playOrder="1"><navLabel><text>Chapter</text></navLabel><content src="chapter.xhtml"/></navPoint></navMap>
</ncx>"#;
        let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter</title></head><body><p>Hello.</p></body></html>"#;
        let files: &[(&str, &[u8])] = &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", container.as_bytes()),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/toc.ncx", ncx.as_bytes()),
            ("OEBPS/chapter.xhtml", chapter.as_bytes()),
        ];
        fs::write(path, zip_stored(files)).unwrap();
    }

    /// Build a ZIP archive with every entry stored uncompressed. EPUB requires
    /// the `mimetype` entry first and uncompressed, which this ordering keeps.
    fn zip_stored(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, data) in files {
            let offset = out.len() as u32;
            let crc = crc32(data);
            let size = data.len() as u32;
            push_u32(&mut out, 0x0403_4b50);
            push_u16(&mut out, 20);
            push_u16(&mut out, 0);
            push_u16(&mut out, 0); // stored
            push_u16(&mut out, 0);
            push_u16(&mut out, 0);
            push_u32(&mut out, crc);
            push_u32(&mut out, size);
            push_u32(&mut out, size);
            push_u16(&mut out, name.len() as u16);
            push_u16(&mut out, 0);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            push_u32(&mut directory, 0x0201_4b50);
            push_u16(&mut directory, 20);
            push_u16(&mut directory, 20);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u32(&mut directory, crc);
            push_u32(&mut directory, size);
            push_u32(&mut directory, size);
            push_u16(&mut directory, name.len() as u16);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u32(&mut directory, 0);
            push_u32(&mut directory, offset);
            directory.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let cd_size = directory.len() as u32;
        out.extend_from_slice(&directory);
        push_u32(&mut out, 0x0605_4b50);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, files.len() as u16);
        push_u16(&mut out, files.len() as u16);
        push_u32(&mut out, cd_size);
        push_u32(&mut out, cd_offset);
        push_u16(&mut out, 0);
        out
    }

    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    #[test]
    fn epub_sidecar_uses_the_embedded_title() {
        let dir = test_dir("sidecar-epub-title");
        let document = dir.join("book.epub");
        write_epub(&document, "The Little Prince");

        assert_eq!(
            embedded_title(&document).as_deref(),
            Some("The Little Prince")
        );
        let hash = document_hash(&document).unwrap();
        let path = file_for(&document, &dir).unwrap().unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("The Little Prince-{hash}.css")
        );
        let _ = fs::remove_dir_all(&dir);
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
