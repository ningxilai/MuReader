//! A deliberately small plain-text reflow pass.
//!
//! MuPDF's HTML/EPUB layout owns its own greedy line breaker and exposes no
//! hook for a different one. For plain text, which has no images, links, or
//! document CSS to preserve, we can do the line breaking ourselves and then
//! hand MuPDF a page-shaped HTML document to render.
//!
//! Break opportunities come from the Unicode line-breaking algorithm
//! (`unicode-linebreak`). A small total-fit pass minimises raggedness, with a
//! gentler last line, which is the same idea as `text-wrap: pretty` rather
//! than a full Knuth-Plass engine with glue and hyphenation.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use hypher::{hyphenate, Lang};

use crate::fonts::FontCatalog;

const PAGE_WIDTH: f32 = 612.0;
const PAGE_HEIGHT: f32 = 792.0;
const MARGIN: f32 = 54.0;
const CONTENT_WIDTH: f32 = PAGE_WIDTH - 2.0 * MARGIN;
const CONTENT_HEIGHT: f32 = PAGE_HEIGHT - 2.0 * MARGIN;
/// Leave a few points for shaping differences between the measuring pass
/// and MuPDF's real layout, so an exactly-full line never wraps again.
const BREAK_WIDTH: f32 = CONTENT_WIDTH - 6.0;
const FONT_SIZE: f32 = 12.0;
const LINE_HEIGHT: f32 = 18.0;
const LINES_PER_PAGE: usize = (CONTENT_HEIGHT / LINE_HEIGHT) as usize;

// Small total-fit demerit parameters, in the spirit of the Knuth-Plass
// reference implementation in ekp.el, without its hyphenation and glue
// redistribution machinery. The line stays left-aligned, so badness measures
// how much of the measure a line leaves unused rather than how far glue is
// stretched.
const LINE_PENALTY: f32 = 10.0;
const FITNESS_PENALTY: f32 = 100.0;
const LAST_LINE_SHORT_PENALTY: f32 = 50.0;
const LAST_LINE_MIN_RATIO: f32 = 0.5;
const BADNESS_CAP: f32 = 10_000.0;
const HYPHEN_PENALTY: f32 = 50.0;
const CONSECUTIVE_HYPHEN_PENALTY: f32 = 100.0;
/// Ragged-right flexibility, roughly 2em, as in ekp's default
/// `ekp-ragged-stretch-pixel` (8x the Latin word space).
const RAGGED_STRETCH: f32 = 2.0 * FONT_SIZE;
/// The emergency pass may over-stretch further; a final fallback accepts
/// any underfull line so text is never lost.
const EMERGENCY_STRETCH: f32 = 3.0 * FONT_SIZE;

#[derive(Debug, Clone)]
struct Unit {
    text: String,
    width: f32,
    space: f32,
    /// A discretionary hyphen may be printed after this box.
    hyphen_after: bool,
}

/// Styleless source formats this module can read as plain text. Markup such
/// as Markdown, reST, Org, AsciiDoc and LaTeX is kept verbatim; it is not
/// rendered as a rich document.
const PLAIN_TEXT_EXTENSIONS: &[&str] = &[
    "txt", "text", "md", "markdown", "mdown", "mkd", "rst", "org", "adoc", "asciidoc", "tex", "log",
];

/// Whether this path is read as unstyled plain text.
pub fn is_plain_text(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            PLAIN_TEXT_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Build a paginated HTML document for a plain-text file.
pub fn html_for_path(path: &Path, fonts: &FontCatalog) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        format!(
            "{} is not UTF-8; leaving plain-text layout to MuPDF",
            path.display()
        )
    })?;
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    Ok(html_for_text(text, fonts))
}

/// Build a paginated HTML document from text.
pub fn html_for_text(text: &str, fonts: &FontCatalog) -> String {
    let paragraphs = split_paragraphs(text);
    let mut pages: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for paragraph in paragraphs {
        let lines = layout_paragraph(&paragraph, |piece| fonts.measure_text(piece, FONT_SIZE));
        let remaining = LINES_PER_PAGE.saturating_sub(current.len());
        if remaining < 2 && lines.len() > remaining {
            pages.push(std::mem::take(&mut current));
        }
        for line in lines {
            if current.len() >= LINES_PER_PAGE {
                pages.push(std::mem::take(&mut current));
            }
            current.push(line);
        }
        if current.len() < LINES_PER_PAGE {
            current.push(String::new());
        }
    }
    pages.push(current);

    for page in &mut pages {
        while page.last().is_some_and(String::is_empty) {
            page.pop();
        }
    }
    pages.retain(|page| !page.is_empty());
    if pages.is_empty() {
        pages.push(Vec::new());
    }

    render_html(&pages)
}

fn split_paragraphs(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut paragraphs = Vec::new();
    let mut current = String::new();

    for line in normalized.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            let previous = current.chars().last().unwrap_or(' ');
            let next = line.chars().next().unwrap_or(' ');
            if previous == '-' && next.is_lowercase() {
                current.pop();
            } else if !(is_cjk(previous) && is_cjk(next)) {
                current.push(' ');
            }
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs
}

fn layout_paragraph(text: &str, measure: impl Fn(&str) -> f32) -> Vec<String> {
    let hyphen_width = measure("-");
    let units = units(text, measure);
    if units.is_empty() {
        return Vec::new();
    }

    // prefix[k] is the sum of boxes [0, k) and the glue between them.
    // A line [i, k) removes the glue immediately before box i.
    let mut prefix = vec![0.0f32; units.len() + 1];
    if !units.is_empty() {
        prefix[1] = units[0].width;
    }
    for j in 2..=units.len() {
        prefix[j] = prefix[j - 1] + units[j - 2].space + units[j - 1].width;
    }

    let breaks = dp_breaks(&units, &prefix, RAGGED_STRETCH, hyphen_width)
        .or_else(|| {
            dp_breaks(
                &units,
                &prefix,
                RAGGED_STRETCH + EMERGENCY_STRETCH,
                hyphen_width,
            )
        })
        // A guaranteed fallback: any line short of the measure is allowed.
        .or_else(|| dp_breaks(&units, &prefix, RAGGED_STRETCH + BREAK_WIDTH, hyphen_width))
        // A guaranteed fallback: any line short of the measure is allowed.
        .or_else(|| dp_breaks(&units, &prefix, RAGGED_STRETCH + BREAK_WIDTH, hyphen_width))
        .unwrap_or_else(|| vec![units.len()]);

    let mut starts = vec![0usize];
    starts.extend(breaks);
    starts
        .windows(2)
        .map(|window| {
            let (start, end) = (window[0], window[1]);
            let mut line = String::new();
            for (index, unit) in units[start..end].iter().enumerate() {
                line.push_str(&unit.text);
                if index + 1 < end - start && unit.space > 0.0 {
                    line.push(' ');
                }
            }
            if end < units.len() && units[end - 1].hyphen_after {
                line.push('-');
            }
            line
        })
        .collect()
}

fn dp_breaks(
    units: &[Unit],
    prefix: &[f32],
    stretch: f32,
    hyphen_width: f32,
) -> Option<Vec<usize>> {
    let n = units.len();
    let mut best = vec![f32::INFINITY; n + 1];
    let mut previous = vec![usize::MAX; n + 1];
    let mut fitness_at = vec![1u8; n + 1];
    let mut hyphen_run = vec![0u32; n + 1];
    best[0] = 0.0;

    for start in 0..n {
        if !best[start].is_finite() {
            continue;
        }
        let lead_glue = if start == 0 {
            0.0
        } else {
            units[start - 1].space
        };
        for end in start + 1..=n {
            let end_hyphen = end < n && units[end - 1].hyphen_after;
            let hyphen = if end_hyphen { hyphen_width } else { 0.0 };
            let ideal = prefix[end] - prefix[start] - lead_glue + hyphen;
            if ideal > BREAK_WIDTH {
                break;
            }
            let last = end == n;

            // A non-last line may be short only within this pass's ragged
            // stretch. The last line is allowed to be short.
            if !last && BREAK_WIDTH > ideal + stretch {
                continue;
            }

            let adjustment = BREAK_WIDTH - ideal;
            let single = end == start + 1;
            let mut cost = if single && !last {
                let flexibility = if stretch > RAGGED_STRETCH {
                    stretch
                } else {
                    1.0
                };
                let badness = badness(adjustment, flexibility);
                (LINE_PENALTY + badness).powi(2)
                    + 200.0
                    + hyphen_demerits(end_hyphen, hyphen_run[start] + 1)
            } else if last {
                let fill = ideal / BREAK_WIDTH;
                let short = (LAST_LINE_MIN_RATIO - fill).max(0.0) * LAST_LINE_SHORT_PENALTY;
                (LINE_PENALTY + short).powi(2)
            } else {
                let badness = badness(adjustment, stretch);
                let mut demerits = (LINE_PENALTY + badness).powi(2);
                let fitness = fitness_class(adjustment, stretch);
                if previous_fitness(&fitness_at, start).abs_diff(fitness) > 1 {
                    demerits += FITNESS_PENALTY;
                }
                if end_hyphen {
                    let run = hyphen_run[start] + 1;
                    demerits +=
                        HYPHEN_PENALTY.powi(2) + CONSECUTIVE_HYPHEN_PENALTY * (run * run) as f32;
                }
                demerits
            };

            if last && n > 1 && ideal < BREAK_WIDTH * 0.25 {
                cost += 60.0;
            }

            let candidate = best[start] + cost;
            if candidate < best[end] {
                best[end] = candidate;
                previous[end] = start;
                fitness_at[end] = if last {
                    1
                } else {
                    fitness_class(adjustment, stretch)
                };
                hyphen_run[end] = if end_hyphen { hyphen_run[start] + 1 } else { 0 };
            }
        }
    }

    if !best[n].is_finite() {
        return None;
    }
    let mut breaks = Vec::new();
    let mut at = n;
    while at > 0 {
        breaks.push(at);
        at = previous[at];
        if at == usize::MAX {
            return None;
        }
    }
    breaks.reverse();
    Some(breaks)
}
fn hyphen_demerits(end_hyphen: bool, run: u32) -> f32 {
    if end_hyphen {
        HYPHEN_PENALTY.powi(2) + CONSECUTIVE_HYPHEN_PENALTY * (run * run) as f32
    } else {
        0.0
    }
}

fn previous_fitness(fitness_at: &[u8], start: usize) -> u8 {
    fitness_at.get(start).copied().unwrap_or(1)
}

fn badness(adjustment: f32, flexibility: f32) -> f32 {
    if adjustment <= 0.0 {
        return 0.0;
    }
    if flexibility <= 0.0 {
        return BADNESS_CAP;
    }
    let ratio = adjustment / flexibility;
    (100.0 * ratio.powi(3)).min(BADNESS_CAP)
}
fn fitness_class(adjustment: f32, flexibility: f32) -> u8 {
    if flexibility <= 0.0 {
        return 1;
    }
    let ratio = adjustment / flexibility;
    if ratio < -0.5 {
        0
    } else if ratio < 0.5 {
        1
    } else if ratio < 1.0 {
        2
    } else {
        3
    }
}

fn units(text: &str, measure: impl Fn(&str) -> f32) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut start = 0;
    for (offset, _) in unicode_linebreak::linebreaks(text) {
        let raw = &text[start..offset];
        let trimmed = raw.trim_end_matches(char::is_whitespace);
        let trailing = &raw[trimmed.len()..];
        push_piece(&mut units, trimmed, measure(trailing), &measure);
        start = offset;
    }
    if start < text.len() {
        let raw = &text[start..];
        let trimmed = raw.trim_end_matches(char::is_whitespace);
        push_piece(&mut units, trimmed, 0.0, &measure);
    }
    units
}

fn push_piece(
    units: &mut Vec<Unit>,
    text: &str,
    trailing_space: f32,
    measure: &impl Fn(&str) -> f32,
) {
    if text.is_empty() {
        return;
    }
    if let Some(syllables) = hyphenate_english(text) {
        let count = syllables.len();
        for (index, syllable) in syllables.into_iter().enumerate() {
            units.push(Unit {
                width: measure(&syllable),
                text: syllable,
                space: if index + 1 == count {
                    trailing_space
                } else {
                    0.0
                },
                hyphen_after: index + 1 < count,
            });
        }
        return;
    }

    let width = measure(text);
    if width > BREAK_WIDTH && text.chars().count() > 1 {
        // A single word wider than the measure. Split it by character so
        // the page can still be filled; no hyphen is invented here.
        for ch in text.chars() {
            let value = ch.to_string();
            units.push(Unit {
                width: measure(&value),
                text: value,
                space: 0.0,
                hyphen_after: false,
            });
        }
        if trailing_space > 0.0 {
            if let Some(last) = units.last_mut() {
                last.space = trailing_space;
            }
        }
    } else {
        units.push(Unit {
            text: text.to_string(),
            width,
            space: trailing_space,
            hyphen_after: false,
        });
    }
}

fn hyphenate_english(text: &str) -> Option<Vec<String>> {
    if text.chars().any(is_cjk)
        || !text.chars().any(|ch| ch.is_ascii_alphabetic())
        || !text
            .chars()
            .all(|ch| ch.is_alphabetic() || matches!(ch, '\'' | '-'))
    {
        return None;
    }
    let syllables: Vec<String> = hyphenate(text, Lang::English).map(str::to_string).collect();
    (syllables.len() > 1).then_some(syllables)
}
fn render_html(pages: &[Vec<String>]) -> String {
    let mut html = String::new();
    html.push_str(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>\n\
         @page { size: 612pt 792pt; margin: 0; }\n\
         html, body { margin: 0; padding: 0; }\n\
         .page { width: 504pt; height: 684pt; margin: 54pt; }\n\
         .line { margin: 0; font-family: serif; font-size: 12pt; \
                 line-height: 18pt; white-space: pre; }\n\
         </style></head><body>\n",
    );
    for (index, page) in pages.iter().enumerate() {
        if index + 1 == pages.len() {
            html.push_str("<div class=\"page\">\n");
        } else {
            html.push_str(
                "<div class=\"page\" style=\"page-break-after: always; break-after: page\">\n",
            );
        }
        for line in page {
            html.push_str("<div class=\"line\">");
            if line.is_empty() {
                html.push_str("&#160;");
            } else {
                escape_html(&mut html, line);
            }
            html.push_str("</div>\n");
        }
        html.push_str("</div>\n");
    }
    html.push_str("</body></html>\n");
    html
}

fn escape_html(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{00A0}' => out.push_str("&#160;"),
            _ => {
                let _ = out.write_char(ch);
            }
        }
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x2FFF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFFEF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width(text: &str) -> f32 {
        text.chars().count() as f32 * 10.0
    }

    #[test]
    fn recognizes_common_plain_text_source_formats() {
        for name in [
            "a.txt",
            "a.text",
            "a.md",
            "a.markdown",
            "a.mdown",
            "a.mkd",
            "a.rst",
            "a.org",
            "a.adoc",
            "a.asciidoc",
            "a.tex",
            "a.log",
            "a.MD",
        ] {
            assert!(is_plain_text(Path::new(name)), "{name}");
        }
        for name in ["a.epub", "a.html", "a.pdf", "a.fb2", "a.docx"] {
            assert!(!is_plain_text(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn joins_soft_wrapped_lines_and_cjk_without_spaces() {
        let paragraphs = split_paragraphs("hello\nworld\n\n你\n好");
        assert_eq!(paragraphs, vec!["hello world", "你好"]);
    }

    #[test]
    fn every_line_fits_the_measure() {
        let text = "one two three four five six seven eight nine ten eleven twelve thirteen";
        let lines = layout_paragraph(text, width);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| width(line) <= BREAK_WIDTH));
    }

    #[test]
    fn hyphenates_a_long_latin_word() {
        let text = "extraordinary characteristics";
        let lines = layout_paragraph(text, |value| value.chars().count() as f32 * 50.0);
        assert!(lines.len() > 1);
        assert!(
            lines.iter().any(|line| line.ends_with('-')),
            "no discretionary hyphen was used: {lines:?}"
        );
    }

    #[test]
    fn cjk_punctuation_does_not_start_a_line() {
        let text = "你好，世界。这是一个用来测试换行的句子，标点不应该出现在行首。\
                    我们继续添加一些文字，让这一段必须跨过好几行，才能检查断行位置。\
                    标点符号，包括逗号和句号，都应当跟随在前一个字符之后。";
        let lines = layout_paragraph(text, width);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| !line.starts_with('，') && !line.starts_with('。')),
            "punctuation moved to line start: {lines:?}"
        );
    }

    #[test]
    fn a_long_unbreakable_word_is_split() {
        let text = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz";
        let lines = layout_paragraph(text, width);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| width(line) <= BREAK_WIDTH));
    }

    #[test]
    fn generated_html_last_page_has_lines() {
        let text = (0..120)
            .map(|index| format!("paragraph {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let fonts = FontCatalog::new(None).unwrap();
        let html = html_for_text(&text, &fonts);
        let pages = html.matches("class=\"page").count();
        assert!(pages > 1);
        let last = html.rfind("class=\"page").unwrap();
        let tail = &html[last..];
        assert!(tail.matches("class=\"line\"").count() > 0);
    }

    #[test]
    fn html_pages_have_fixed_line_counts() {
        let text = (0..200)
            .map(|index| format!("paragraph {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let fonts = FontCatalog::new(None).unwrap();
        let html = html_for_text(&text, &fonts);
        assert!(html.contains("class=\"page\""));
        assert!(html.contains("class=\"line\""));
        assert!(html.matches("class=\"line\"").count() > LINES_PER_PAGE);
    }
}
