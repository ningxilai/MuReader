//! Small single-line text stack for the outline panel.
//!
//! Font discovery and fallback come from the shared `FontCatalog` (backed by
//! `fontdb`), shaping is done by native HarfBuzz through `harfbuzz_rs`, and
//! glyphs are rasterized with `ab_glyph`.
//! This deliberately avoids a full text-layout engine: outline rows are one
//! line, do not wrap, and only need a baseline plus a sprite cache.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use ab_glyph::{Font as _, FontRef, GlyphId};
use fontdb::{Database, Weight, ID};
use harfbuzz_rs::{shape, Direction, Face, Font as HarfBuzzFont, UnicodeBuffer};
use unicode_bidi::ParagraphBidiInfo;
use unicode_bidi_mirroring::get_mirrored;

use crate::fonts::FontCatalog;

pub struct TextTarget<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    clip_y0: i32,
    clip_y1: i32,
}

impl<'a> TextTarget<'a> {
    pub fn new(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        row_top: i32,
        row_height: u32,
    ) -> Self {
        debug_assert_eq!(pixels.len(), width as usize * height as usize);
        let clip_y0 = row_top.max(0);
        let clip_y1 = row_top.saturating_add(row_height as i32).min(height as i32);
        Self {
            pixels,
            width,
            height,
            clip_y0,
            clip_y1,
        }
    }
}

pub struct TextEngine {
    catalog: Arc<FontCatalog>,
    regular: ID,
    bold: ID,
    regular_chain: Vec<ID>,
    bold_chain: Vec<ID>,
    char_faces: HashMap<(Weight, char), ID>,
    sprites: HashMap<SpriteKey, Option<CachedSprite>>,
    metrics: Metrics,
}

#[derive(Clone, Copy)]
struct Metrics {
    ascent: f32,
    descent: f32,
    line_gap: f32,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct SpriteKey {
    face: ID,
    glyph: u16,
    scale: u32,
}

struct CachedSprite {
    offset_x: i32,
    offset_y: i32,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

struct FaceRun<'a> {
    text: &'a str,
    face: ID,
    rtl: bool,
}

impl TextEngine {
    pub fn new(catalog: Arc<FontCatalog>) -> Result<Self, String> {
        let regular = catalog.ui_regular();
        let bold = catalog.ui_bold();
        let metrics = load_metrics(catalog.db(), regular)?;
        let regular_chain = catalog.ui_regular_chain().to_vec();
        let bold_chain = catalog.ui_bold_chain().to_vec();

        Ok(Self {
            catalog,
            regular,
            bold,
            regular_chain,
            bold_chain,
            char_faces: HashMap::new(),
            sprites: HashMap::new(),
            metrics,
        })
    }

    /// Draw one outline row. `origin_x` is the left edge after indentation and
    /// `row_top` is the top of the row in the target bitmap.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_line(
        &mut self,
        text: &str,
        size: f32,
        weight: Weight,
        target: &mut TextTarget<'_>,
        origin_x: i32,
        row_top: i32,
        row_height: u32,
        color: (u8, u8, u8),
    ) {
        if text.is_empty() {
            return;
        }

        let baseline = row_top as f32 + self.baseline(size, row_height);
        let runs = self.face_runs(text, weight);
        let db = self.catalog.db();
        let sprites = &mut self.sprites;
        let mut pen_x = origin_x as f32;

        for run in runs {
            db.with_face_data(run.face, |data, index| {
                let Ok(font) = FontRef::try_from_slice_and_index(data, index) else {
                    return;
                };
                let face = Face::from_bytes(data, index);
                let upem = face.upem().max(1) as f32;
                let hb_font = HarfBuzzFont::new(face);
                let shaped_text = if run.rtl {
                    Cow::Owned(
                        run.text
                            .chars()
                            .map(|ch| get_mirrored(ch).unwrap_or(ch))
                            .collect::<String>(),
                    )
                } else {
                    Cow::Borrowed(run.text)
                };
                let buffer = UnicodeBuffer::new().add_str(&shaped_text);
                let buffer = buffer
                    .set_direction(if run.rtl {
                        Direction::Rtl
                    } else {
                        Direction::Ltr
                    })
                    .guess_segment_properties();
                let output = shape(&hb_font, buffer, &[]);
                let infos = output.get_glyph_infos();
                let positions = output.get_glyph_positions();
                if infos.is_empty() {
                    return;
                }

                let unit = size / upem;
                let px_scale = size * font.height_unscaled() / font.units_per_em().unwrap_or(upem);
                let advances: Vec<f32> = positions
                    .iter()
                    .map(|position| {
                        let advance = position.x_advance as f32 * unit;
                        if run.rtl {
                            advance.abs()
                        } else {
                            advance
                        }
                    })
                    .collect();
                let width: f32 = advances.iter().sum();
                let mut pen = pen_x;

                for ((info, position), advance) in
                    infos.iter().zip(positions).zip(advances.iter().copied())
                {
                    let Ok(glyph_id) = u16::try_from(info.codepoint) else {
                        continue;
                    };
                    let glyph_x = pen + position.x_offset as f32 * unit;
                    let glyph_y = baseline + position.y_offset as f32 * unit;
                    let key = SpriteKey {
                        face: run.face,
                        glyph: glyph_id,
                        scale: px_scale.to_bits(),
                    };
                    let sprite = sprites
                        .entry(key)
                        .or_insert_with(|| rasterize(&font, glyph_id, px_scale));
                    if let Some(sprite) = sprite.as_ref() {
                        draw_sprite(
                            target,
                            glyph_x.floor() as i32 + sprite.offset_x,
                            glyph_y.floor() as i32 + sprite.offset_y,
                            sprite,
                            color,
                        );
                    }
                    pen += advance;
                }

                pen_x += width;
            });
        }
    }

    fn face_runs<'a>(&mut self, text: &'a str, weight: Weight) -> Vec<FaceRun<'a>> {
        let bidi = ParagraphBidiInfo::new(text, None);
        let (levels, visual_runs) = bidi.visual_runs(0..text.len());
        let mut runs = Vec::new();

        for run in visual_runs {
            let rtl = levels[run.start].is_rtl();
            let slice = &text[run.clone()];
            let first = runs.len();
            let mut current_face = None;
            let mut run_start = run.start;

            for (offset, ch) in slice.char_indices() {
                let face = if continues_run(ch) {
                    current_face.unwrap_or_else(|| self.face_for_char(weight, ch))
                } else {
                    self.face_for_char(weight, ch)
                };
                if let Some(previous) = current_face {
                    if face != previous {
                        runs.push(FaceRun {
                            text: &text[run_start..run.start + offset],
                            face: previous,
                            rtl,
                        });
                        run_start = run.start + offset;
                    }
                }
                current_face = Some(face);
            }

            if let Some(face) = current_face {
                runs.push(FaceRun {
                    text: &text[run_start..run.end],
                    face,
                    rtl,
                });
            }

            if rtl {
                runs[first..].reverse();
            }
        }

        runs
    }

    fn face_for_char(&mut self, weight: Weight, ch: char) -> ID {
        if let Some(face) = self.char_faces.get(&(weight, ch)) {
            return *face;
        }

        let primary = if weight >= Weight::SEMIBOLD {
            self.bold
        } else {
            self.regular
        };
        let chain = if weight >= Weight::SEMIBOLD {
            &self.bold_chain
        } else {
            &self.regular_chain
        };
        let face = chain
            .iter()
            .copied()
            .find(|face| self.supports(*face, ch))
            .unwrap_or(primary);
        self.char_faces.insert((weight, ch), face);
        face
    }

    fn supports(&self, face: ID, ch: char) -> bool {
        self.catalog
            .db()
            .with_face_data(face, |data, index| {
                let face = Face::from_bytes(data, index);
                let font = HarfBuzzFont::new(face);
                font.get_nominal_glyph(ch).is_some()
            })
            .unwrap_or(false)
    }

    fn baseline(&self, size: f32, row_height: u32) -> f32 {
        let ascent = self.metrics.ascent * size;
        let descent = self.metrics.descent * size;
        let gap = self.metrics.line_gap * size;
        let line_height = ascent - descent + gap;
        let top = ((row_height as f32 - line_height) * 0.5).max(0.0);
        top + ascent + gap * 0.5
    }
}

fn continues_run(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{200D}' | '\u{FE0E}' | '\u{FE0F}')
}

fn load_metrics(db: &Database, face: ID) -> Result<Metrics, String> {
    db.with_face_data(face, |data, index| {
        let font = FontRef::try_from_slice_and_index(data, index).ok()?;
        let upem = font.units_per_em()?;
        Some(Metrics {
            ascent: font.ascent_unscaled() / upem,
            descent: font.descent_unscaled() / upem,
            line_gap: font.line_gap_unscaled() / upem,
        })
    })
    .flatten()
    .ok_or_else(|| "unable to read outline font metrics".to_string())
}

fn rasterize(font: &FontRef<'_>, glyph: u16, px_scale: f32) -> Option<CachedSprite> {
    let outline = font.outline_glyph(GlyphId(glyph).with_scale(px_scale))?;
    let bounds = outline.px_bounds();
    let width = (bounds.max.x - bounds.min.x).ceil().max(0.0) as u32;
    let height = (bounds.max.y - bounds.min.y).ceil().max(0.0) as u32;
    if width == 0 || height == 0 {
        return None;
    }

    let mut data = vec![0u8; width as usize * height as usize];
    outline.draw(|x, y, coverage| {
        let index = y as usize * width as usize + x as usize;
        if let Some(pixel) = data.get_mut(index) {
            *pixel = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    });

    Some(CachedSprite {
        offset_x: bounds.min.x.floor() as i32,
        offset_y: bounds.min.y.floor() as i32,
        width,
        height,
        data,
    })
}

fn draw_sprite(
    target: &mut TextTarget<'_>,
    x: i32,
    y: i32,
    sprite: &CachedSprite,
    color: (u8, u8, u8),
) {
    debug_assert_eq!(
        sprite.data.len(),
        sprite.width as usize * sprite.height as usize
    );
    let (cr, cg, cb) = color;
    let width = target.width as i32;
    let height = target.height as i32;
    let x0 = x.max(0);
    let x1 = x.saturating_add(sprite.width as i32).min(width);
    let y0 = y.max(target.clip_y0).max(0);
    let y1 = y
        .saturating_add(sprite.height as i32)
        .min(target.clip_y1)
        .min(height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let sprite_width = sprite.width as usize;
    let src_x = (x0 - x) as usize;
    let src_y = (y0 - y) as usize;
    for py in y0..y1 {
        let dst_row = py as usize * target.width as usize;
        let src_row = (src_y + (py - y0) as usize) * sprite_width;
        for px in x0..x1 {
            let coverage = sprite.data[src_row + src_x + (px - x0) as usize] as u32;
            if coverage == 0 {
                continue;
            }
            let index = dst_row + px as usize;
            if coverage == 255 {
                target.pixels[index] = ((cr as u32) << 16) | ((cg as u32) << 8) | cb as u32;
            } else {
                let dst = target.pixels[index];
                let dr = (dst >> 16) & 0xff;
                let dg = (dst >> 8) & 0xff;
                let db = dst & 0xff;
                let r = (dr * (255 - coverage) + cr as u32 * coverage) / 255;
                let g = (dg * (255 - coverage) + cg as u32 * coverage) / 255;
                let b = (db * (255 - coverage) + cb as u32 * coverage) / 255;
                target.pixels[index] = (r << 16) | (g << 8) | b;
            }
        }
    }
}
