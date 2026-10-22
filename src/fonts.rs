//! Shared font discovery for the outline panel and MuPDF.
//!
//! One `fontdb::Database` serves both the native-HarfBuzz UI text stack and the
//! [`mupdf::FontLoader`] implementation used for EPUB/reflowable text. Bundled
//! Noto/Droid fonts are added as a last-resort fallback so Unicode coverage does
//! not depend on what happens to be installed on the host.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ab_glyph::{Font as _, FontRef, GlyphId};
use fontdb::{Database, Family, Query, Source, Stretch, Style, Weight, ID};
use harfbuzz_rs::Face;
use mupdf::{CjkFontOrdering, Font as MuPdfFont, FontHints, FontLoader};

const FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans CJK SC",
    "Noto Sans CJK TC",
    "Noto Sans SC",
    "Noto Sans TC",
    "Source Han Sans SC",
    "Source Han Sans TC",
    "WenQuanYi Micro Hei",
    "Droid Sans Fallback",
];

pub struct FontCatalog {
    db: Database,
    ui_regular: ID,
    ui_bold: ID,
    ui_regular_chain: Vec<ID>,
    ui_bold_chain: Vec<ID>,
    epub: Option<EpubFaces>,
    epub_family: Option<String>,
    epub_emoji: Option<ID>,
    /// Character -> chosen face and normalized advance (font units / upem).
    measure_cache: Mutex<HashMap<char, Option<(ID, f32)>>>,
}

struct EpubFaces {
    regular: ID,
    bold: ID,
    italic: ID,
    bold_italic: ID,
}

impl FontCatalog {
    pub fn new(family: Option<&str>) -> Result<Self, String> {
        let mut db = Database::new();
        db.load_system_fonts();

        let bundled_sans = load_bundled(
            &mut db,
            mupdf_fonts_noto::find_by_name("Noto Sans", false, false),
        );
        let bundled_serif = load_bundled(
            &mut db,
            mupdf_fonts_noto::find_by_name("Noto Serif", false, false),
        );
        let bundled_emoji = load_bundled(
            &mut db,
            mupdf_fonts_noto::find_by_name("Noto Emoji", false, false),
        );
        let bundled_cjk = load_bundled(
            &mut db,
            mupdf_fonts_droid::find_by_name("Droid Sans Fallback Full", false, false),
        );

        // The CSS-selected family is the primary face. Emoji uses that face
        // when it has outline emoji glyphs, otherwise the same shared
        // Symbola/Noto/bundled fallback as before.
        let requested =
            family.and_then(|spec| query_named(&db, spec, Weight::NORMAL, Style::Normal));
        let requested_emoji = family.and_then(|spec| {
            let id = query_named(&db, spec, Weight::NORMAL, Style::Normal)?;
            face_has_outline(&db, id, '🥳').then_some(id)
        });
        let ui_emoji = requested_emoji
            .or_else(|| find_outline_emoji(&db))
            .or_else(|| bundled_emoji.first().copied());
        let epub_emoji = ui_emoji;

        let system_regular = query_face(&db, Family::SansSerif, Weight::NORMAL, Style::Normal)
            .or_else(|| bundled_sans.first().copied())
            .ok_or_else(|| "no usable sans-serif font found".to_string())?;

        let (ui_regular, ui_bold) = if let Some(regular) = requested {
            let bold = family
                .and_then(|spec| query_named(&db, spec, Weight::BOLD, Style::Normal))
                .unwrap_or(regular);
            (regular, bold)
        } else {
            let bold = query_face(&db, Family::SansSerif, Weight::BOLD, Style::Normal)
                .unwrap_or(system_regular);
            (system_regular, bold)
        };

        let mut bundled = Vec::new();
        bundled.extend(bundled_cjk.iter().copied());
        bundled.extend(ui_emoji);
        bundled.extend(bundled_emoji.iter().copied());
        bundled.extend(bundled_sans.iter().copied());
        bundled.extend(bundled_serif.iter().copied());

        let ui_regular_chain = build_chain(&db, ui_regular, Weight::NORMAL, &bundled);
        let ui_bold_chain = build_chain(&db, ui_bold, Weight::BOLD, &bundled);

        let (epub, epub_family) = resolve_epub_faces(&mut db, family).unzip();

        Ok(Self {
            db,
            ui_regular,
            ui_bold,
            ui_regular_chain,
            ui_bold_chain,
            epub,
            epub_family,
            epub_emoji,
            measure_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn ui_regular(&self) -> ID {
        self.ui_regular
    }

    pub fn ui_bold(&self) -> ID {
        self.ui_bold
    }

    pub fn ui_regular_chain(&self) -> &[ID] {
        &self.ui_regular_chain
    }

    pub fn ui_bold_chain(&self) -> &[ID] {
        &self.ui_bold_chain
    }

    fn epub_font(&self, name: &str, hints: FontHints) -> Option<MuPdfFont> {
        if name.to_ascii_lowercase().contains("emoji") {
            if let Some(emoji) = self.epub_emoji {
                return self.mupdf_font(emoji, name);
            }
        }

        let faces = self.epub.as_ref()?;
        if !should_override_epub_font(name, self.epub_family.as_deref()) {
            return None;
        }
        let id = match (hints.bold, hints.italic) {
            (false, false) => faces.regular,
            (true, false) => faces.bold,
            (false, true) => faces.italic,
            (true, true) => faces.bold_italic,
        };
        self.mupdf_font(id, name)
    }

    fn epub_cjk_font(&self, _ordering: CjkFontOrdering, _serif: bool) -> Option<MuPdfFont> {
        let faces = self.epub.as_ref()?;
        if self.face_has_char(faces.regular, '中') {
            self.mupdf_font(faces.regular, "epub-cjk")
        } else {
            None
        }
    }

    fn mupdf_font(&self, id: ID, name: &str) -> Option<MuPdfFont> {
        self.db
            .with_face_data(id, |data, index| {
                MuPdfFont::from_bytes_with_index(name, index as i32, data).ok()
            })
            .flatten()
    }

    /// Approximate advance width of `text` at `size` points, using the
    /// configured reflowable body face and the shared fallback chain.
    ///
    /// This is deliberately a nominal-advance sum: it is used for line
    /// breaking a plain-text subset before MuPDF lays the generated HTML
    /// out, not for painting glyphs. Complex-script shaping differences are
    /// left to MuPDF's real layout.
    pub fn measure_text(&self, text: &str, size: f32) -> f32 {
        if !size.is_finite() || size <= 0.0 {
            return 0.0;
        }
        let primary = self
            .epub
            .as_ref()
            .map_or(self.ui_regular, |faces| faces.regular);
        let mut total = 0.0;
        for ch in text.chars() {
            let cached = {
                let cache = self
                    .measure_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.get(&ch).copied()
            };
            let measured = cached.unwrap_or_else(|| {
                let face = std::iter::once(primary)
                    .chain(self.ui_regular_chain.iter().copied())
                    .find(|face| self.face_has_char(*face, ch))
                    .unwrap_or(primary);
                let advance = self
                    .db
                    .with_face_data(face, |data, index| {
                        let face = Face::from_bytes(data, index);
                        let upem = face.upem().max(1) as f32;
                        let font = harfbuzz_rs::Font::new(face);
                        let glyph = font.get_nominal_glyph(ch)?;
                        Some(font.get_glyph_h_advance(glyph) as f32 / upem)
                    })
                    .flatten();
                let value = advance.map(|advance| (face, advance));
                let mut cache = self
                    .measure_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.insert(ch, value);
                value
            });
            if let Some((_, advance)) = measured {
                total += advance * size;
            }
        }
        total
    }

    fn face_has_char(&self, id: ID, ch: char) -> bool {
        self.db
            .with_face_data(id, |data, index| {
                let face = Face::from_bytes(data, index);
                let font = harfbuzz_rs::Font::new(face);
                font.get_nominal_glyph(ch).is_some()
            })
            .unwrap_or(false)
    }
}

/// MuPDF font loader backed by the same font database as the UI.
pub struct EpubFontLoader {
    catalog: Arc<FontCatalog>,
}

impl EpubFontLoader {
    pub fn new(catalog: Arc<FontCatalog>) -> Self {
        Self { catalog }
    }
}

impl FontLoader for EpubFontLoader {
    fn load_font(&self, name: &str, hints: FontHints) -> Option<MuPdfFont> {
        self.catalog.epub_font(name, hints)
    }

    fn load_cjk_font(
        &self,
        _name: &str,
        ordering: CjkFontOrdering,
        serif: bool,
    ) -> Option<MuPdfFont> {
        self.catalog.epub_cjk_font(ordering, serif)
    }
}

const DEFAULT_EPUB_FAMILIES: &[&str] = &[
    "Noto Serif CJK SC",
    "Noto Serif",
    "Droid Sans Fallback Full",
];

fn resolve_epub_faces(db: &mut Database, spec: Option<&str>) -> Option<(EpubFaces, String)> {
    if let Some(spec) = spec {
        if is_font_path(spec) {
            if let Some(id) = load_font_path(db, spec) {
                return Some((
                    EpubFaces {
                        regular: id,
                        bold: id,
                        italic: id,
                        bold_italic: id,
                    },
                    spec.to_string(),
                ));
            }
            eprintln!("MuReader: cannot load EPUB font {spec:?}");
        } else if let Some(faces) = epub_faces_for_family(db, spec) {
            return Some((faces, spec.to_string()));
        } else {
            eprintln!("MuReader: EPUB font family {spec:?} not found");
        }
    }

    DEFAULT_EPUB_FAMILIES.iter().find_map(|family| {
        epub_faces_for_family(db, family).map(|faces| (faces, family.to_string()))
    })
}

fn epub_faces_for_family(db: &Database, family: &str) -> Option<EpubFaces> {
    let regular = query_named(db, family, Weight::NORMAL, Style::Normal)?;
    Some(EpubFaces {
        regular,
        bold: query_named(db, family, Weight::BOLD, Style::Normal).unwrap_or(regular),
        italic: query_named(db, family, Weight::NORMAL, Style::Italic).unwrap_or(regular),
        bold_italic: query_named(db, family, Weight::BOLD, Style::Italic).unwrap_or(regular),
    })
}

fn should_override_epub_font(name: &str, family: Option<&str>) -> bool {
    if name.is_empty() {
        return true;
    }

    let lower = name.to_ascii_lowercase();
    if family.is_some_and(|family| lower == family.to_ascii_lowercase()) {
        return true;
    }
    if matches!(
        lower.as_str(),
        "serif" | "sans" | "sans-serif" | "mono" | "monospace" | "cursive" | "fantasy"
    ) {
        return true;
    }

    // MuPDF asks for these by name while filling missing glyphs. Let the
    // bundled/system loaders answer those requests instead of replacing the
    // emoji/math/music/symbol fonts with the body font.
    !(lower.starts_with("noto ")
        || lower.starts_with("droid ")
        || lower.contains("emoji")
        || lower.contains("symbol")
        || lower.contains("math")
        || lower.contains("music"))
}

fn find_outline_emoji(db: &Database) -> Option<ID> {
    [
        "Symbola",
        "Noto Emoji",
        "Noto Sans Symbols 2",
        "DejaVu Sans",
    ]
    .into_iter()
    .filter_map(|name| query_named(db, name, Weight::NORMAL, Style::Normal))
    .find(|id| face_has_outline(db, *id, '🥳'))
}

fn face_has_outline(db: &Database, id: ID, ch: char) -> bool {
    db.with_face_data(id, |data, index| {
        let Some(font) = FontRef::try_from_slice_and_index(data, index).ok() else {
            return false;
        };
        let glyph = font.glyph_id(ch);
        font.outline_glyph(GlyphId(glyph.0).with_scale(14.0))
            .is_some()
    })
    .unwrap_or(false)
}

fn query_face(db: &Database, family: Family<'_>, weight: Weight, style: Style) -> Option<ID> {
    let families = [family];
    db.query(&Query {
        families: &families,
        weight,
        stretch: Stretch::Normal,
        style,
    })
}

fn query_named(db: &Database, spec: &str, weight: Weight, style: Style) -> Option<ID> {
    let lower = spec.to_ascii_lowercase();
    let family = match lower.as_str() {
        "serif" => Family::Serif,
        "sans" | "sans-serif" => Family::SansSerif,
        "mono" | "monospace" => Family::Monospace,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        _ => Family::Name(spec),
    };
    query_face(db, family, weight, style)
}

fn build_chain(db: &Database, primary: ID, weight: Weight, bundled: &[ID]) -> Vec<ID> {
    let mut chain = Vec::new();
    push_unique(&mut chain, primary);

    for name in FALLBACK_FAMILIES {
        if let Some(face) = query_named(db, name, weight, Style::Normal) {
            push_unique(&mut chain, face);
        }
    }
    for face in bundled {
        push_unique(&mut chain, *face);
    }
    for face in db.faces() {
        if face.style == Style::Normal && face.stretch == Stretch::Normal {
            push_unique(&mut chain, face.id);
        }
    }

    chain
}

fn push_unique(chain: &mut Vec<ID>, face: ID) {
    if !chain.contains(&face) {
        chain.push(face);
    }
}

fn is_font_path(spec: &str) -> bool {
    let path = Path::new(spec);
    path.is_file()
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "ttf" | "otf" | "ttc" | "otc"
                )
            })
}

fn load_font_path(db: &mut Database, spec: &str) -> Option<ID> {
    let data = std::fs::read(spec).ok()?;
    let source = Source::Binary(Arc::new(data));
    db.load_font_source(source).first().copied()
}

fn load_bundled(db: &mut Database, font: Option<impl BundledFont>) -> Vec<ID> {
    let Some(font) = font else {
        return Vec::new();
    };
    let data: &'static [u8] = font.data();
    let source = Source::Binary(Arc::new(data));
    db.load_font_source(source).into_iter().collect()
}

trait BundledFont {
    fn data(&self) -> &'static [u8];
}

impl BundledFont for mupdf_fonts_noto::Font {
    fn data(&self) -> &'static [u8] {
        self.data
    }
}

impl BundledFont for mupdf_fonts_droid::Font {
    fn data(&self) -> &'static [u8] {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::should_override_epub_font;

    #[test]
    fn epub_body_font_does_not_replace_fallback_special_fonts() {
        assert!(should_override_epub_font("serif", Some("Noto Serif")));
        assert!(should_override_epub_font("Noto Serif", Some("Noto Serif")));
        assert!(!should_override_epub_font("Noto Emoji", Some("Noto Serif")));
        assert!(!should_override_epub_font(
            "Noto Sans Symbols 2",
            Some("Noto Serif")
        ));
        assert!(!should_override_epub_font(
            "Droid Sans Fallback",
            Some("Noto Serif")
        ));
    }
}
