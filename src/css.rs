// SPDX-License-Identifier: AGPL-3.0-only
//! CSS parsing shared by the reading pipeline and the UI theme.
//!
//! MuPDF implements a small CSS 2.1 subset. Its parser is a hand-written C
//! lexer that recovers from syntax errors by skipping to the next declaration
//! or block, but it is not a general-purpose syntax parser. This module parses
//! CSS with `cssparser`, which implements the CSS Syntax Level 3 tokenizer and
//! rule parser. CSS 2.1 is a subset of that grammar, so every valid CSS 2.1
//! stylesheet is parsed without a lossy custom scanner.
//!
//! Parsing and sanitizing are deliberately separate:
//!
//! * [`StyleSheet::parse`] is lenient. It preserves every declaration it can
//!   and is used for the user's own config CSS and for theme/font extraction.
//! * [`StyleSheet::safe_css`] is the sidecar path. It keeps only the
//!   at-rules and properties MuPDF understands, rejects values that MuPDF
//!   cannot apply safely, and serializes a deterministic stylesheet.
//!
//! Document CSS is never parsed or rewritten here; MuPDF remains responsible
//! for the book's own stylesheet.

use std::collections::BTreeMap;

use cssparser::{
    serialize_identifier, AtRuleParser, BasicParseErrorKind, CowRcStr, DeclarationParser,
    ParseError, Parser, ParserInput, ParserState, QualifiedRuleParser, RuleBodyItemParser,
    RuleBodyParser, StyleSheetParser, ToCss, Token,
};

/// Largest absolute number accepted in a sidecar value. A reading layout is
/// measured in points; anything larger is a mistake or an attempt to make the
/// C layout engine allocate or overflow.
const MAX_VALUE: f32 = 10_000.0;
/// Bound parser recursion so a hostile stylesheet cannot exhaust the stack.
const MAX_DEPTH: usize = 32;

/// Properties MuPDF's CSS engine actually stores. This is the list from
/// `mupdf/source/html/css-properties.h`; properties outside it are ignored by
/// MuPDF, so a sidecar should not carry them.
const SUPPORTED_PROPERTIES: &[&str] = &[
    "background",
    "background-color",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-style",
    "border-top-width",
    "border-width",
    "bottom",
    "clear",
    "color",
    "columns",
    "direction",
    "display",
    "float",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-variant",
    "font-weight",
    "height",
    "hyphens",
    "inset",
    "left",
    "letter-spacing",
    "line-height",
    "list-style",
    "list-style-image",
    "list-style-position",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "-mupdf-leading",
    "orphans",
    "overflow-wrap",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "page-break-after",
    "page-break-before",
    "position",
    "quotes",
    "right",
    "src",
    "text-align",
    "text-decoration",
    "text-indent",
    "text-transform",
    "top",
    "vertical-align",
    "visibility",
    "-webkit-text-fill-color",
    "-webkit-text-stroke-color",
    "-webkit-text-stroke-width",
    "white-space",
    "widows",
    "width",
    "word-spacing",
];

/// Descriptors MuPDF reads from `@font-face`.
const FONT_FACE_PROPERTIES: &[&str] = &[
    "font-family",
    "font-weight",
    "font-style",
    "font-variant",
    "src",
];

/// Properties whose value may contain `url()`. MuPDF resolves the URL against
/// the document archive, so the sidecar can point at fonts and images that
/// already live inside the book.
fn property_allows_url(name: &str) -> bool {
    matches!(name, "background" | "list-style" | "list-style-image")
}

/// One `property: value` pair, with `!important` separated so the caller can
/// decide whether to keep it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

/// A style rule. `selectors` is the serialized selector list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub selectors: String,
    pub declarations: Vec<Declaration>,
}

/// A parsed stylesheet. At-rules are kept separately because MuPDF treats
/// `@font-face` and `@page` as special rule kinds rather than style rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleSheet {
    pub rules: Vec<Rule>,
    pub font_faces: Vec<Vec<Declaration>>,
    pub page: Vec<Vec<Declaration>>,
}

impl StyleSheet {
    /// Parse a stylesheet leniently. Invalid rules and declarations are
    /// skipped; a malformed construct never aborts the whole sheet.
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut input = ParserInput::new(text);
        let mut parser = Parser::new(&mut input);
        let mut rules = RuleParser::default();
        for _ in StyleSheetParser::new(&mut parser, &mut rules) {
            // `RuleParser` records every valid rule itself. Errors are already
            // isolated to the rule that caused them by the parser.
        }
        rules.sheet
    }

    /// Serialize the stylesheet for MuPDF: only supported at-rules and
    /// properties, only values MuPDF can apply safely, and no comments.
    pub fn safe_css(&self) -> String {
        let mut out = String::new();

        for declarations in &self.font_faces {
            let kept: Vec<Declaration> = declarations
                .iter()
                .filter(|declaration| {
                    FONT_FACE_PROPERTIES.contains(&declaration.name.as_str())
                        && if declaration.name == "src" {
                            src_is_safe(&declaration.value)
                        } else {
                            value_is_safe(&declaration.value, false)
                        }
                })
                .cloned()
                .collect();
            if kept.iter().any(|declaration| declaration.name == "src")
                && kept
                    .iter()
                    .any(|declaration| declaration.name == "font-family")
            {
                write_block(&mut out, "@font-face", &kept);
            }
        }

        for declarations in &self.page {
            let kept: Vec<Declaration> = declarations
                .iter()
                .filter(|declaration| {
                    supported_property(&declaration.name)
                        && value_is_safe(&declaration.value, property_allows_url(&declaration.name))
                })
                .cloned()
                .collect();
            if !kept.is_empty() {
                write_block(&mut out, "@page", &kept);
            }
        }

        for rule in &self.rules {
            if !selectors_are_safe(&rule.selectors) {
                continue;
            }
            let kept: Vec<Declaration> = rule
                .declarations
                .iter()
                .filter(|declaration| {
                    if declaration.name.starts_with("--") {
                        // Custom properties are for MuReader's UI theme, not
                        // MuPDF. Keep the documented tokens only, and require
                        // a literal value because MuPDF cannot resolve `var()`.
                        declaration.name.starts_with("--mureader-")
                            && value_is_safe(&declaration.value, false)
                    } else {
                        supported_property(&declaration.name)
                            && value_is_safe(
                                &declaration.value,
                                property_allows_url(&declaration.name),
                            )
                    }
                })
                .cloned()
                .collect();
            if !kept.is_empty() {
                write_block(&mut out, &rule.selectors, &kept);
            }
        }

        out
    }

    /// The first `font-family` in a style rule, ignoring `@font-face` (which
    /// names a source font, not the face the document asks for) and `var()`
    /// indirections. UI text follows this family.
    pub fn font_family(&self) -> Option<String> {
        for rule in &self.rules {
            for declaration in &rule.declarations {
                if declaration.name != "font-family" {
                    continue;
                }
                if let Some(family) = first_font_family(&declaration.value) {
                    return Some(family);
                }
            }
        }
        None
    }

    /// Last definition wins, matching CSS's cascade for custom properties.
    pub fn custom_properties(&self) -> BTreeMap<String, String> {
        let mut variables = BTreeMap::new();
        for rule in &self.rules {
            for declaration in &rule.declarations {
                if declaration.name.starts_with("--") {
                    variables.insert(declaration.name.clone(), declaration.value.clone());
                }
            }
        }
        variables
    }
}

fn supported_property(name: &str) -> bool {
    SUPPORTED_PROPERTIES.contains(&name)
}

/// `@font-face` takes exactly one source. MuPDF stores the whole descriptor
/// value as a path, so a list or `format()` suffix would be treated as part of
/// the filename.
fn src_is_safe(value: &str) -> bool {
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    let mut url = None;
    loop {
        let token = match parser.next_including_whitespace_and_comments() {
            Ok(token) => token,
            Err(_) => break,
        };
        match token {
            Token::WhiteSpace(_) | Token::Comment(_) => {}
            Token::UnquotedUrl(value) if url.is_none() => url = Some(value.to_string()),
            Token::Function(name) if name.eq_ignore_ascii_case("url") && url.is_none() => {
                match parser.parse_nested_block(parse_quoted_url) {
                    Ok(value) => url = Some(value),
                    Err(_) => return false,
                }
            }
            _ => return false,
        }
    }
    url.as_deref().is_some_and(url_is_safe)
}

/// The first family in a `font-family` value. A `var()` or other function is
/// not a literal family, so it yields `None` and the caller keeps looking.
fn first_font_family(value: &str) -> Option<String> {
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    loop {
        match parser.next_including_whitespace_and_comments() {
            Ok(Token::Ident(name)) => return Some(name.to_string()),
            Ok(Token::QuotedString(name)) => return Some(name.to_string()),
            Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
            Ok(_) | Err(_) => return None,
        }
    }
}

fn write_block(out: &mut String, prelude: &str, declarations: &[Declaration]) {
    out.push_str(prelude);
    out.push_str(" {\n");
    for declaration in declarations {
        out.push_str("    ");
        out.push_str(&declaration.name);
        out.push_str(": ");
        out.push_str(&declaration.value);
        if declaration.important {
            out.push_str(" !important");
        }
        out.push_str(";\n");
    }
    out.push_str("}\n");
}

/// A token tree used while parsing. Functions and blocks keep their children
/// so nested values such as `rgb(1 2 3 / 50%)`, `var(--paper)` and
/// `:nth-child(2n + 1)` survive serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Whitespace,
    Token(String),
    Function(String, Vec<Node>),
    Block(char, Vec<Node>),
}

#[derive(Default)]
struct RuleParser {
    sheet: StyleSheet,
}

impl<'i> QualifiedRuleParser<'i> for RuleParser {
    type Prelude = String;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        let nodes = parse_nodes(input, 0)?;
        let prelude = serialize(&nodes);
        if prelude.is_empty() {
            return Err(input.new_custom_error(()));
        }
        Ok(prelude)
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Self::Prelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, Self::Error>> {
        let declarations = parse_declarations(input)?;
        self.sheet.rules.push(Rule {
            selectors: prelude,
            declarations,
        });
        Ok(())
    }
}

impl<'i> AtRuleParser<'i> for RuleParser {
    type Prelude = AtPrelude;
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        let kind = if name.eq_ignore_ascii_case("font-face") {
            AtKind::FontFace
        } else if name.eq_ignore_ascii_case("page") {
            AtKind::Page
        } else {
            // MuPDF ignores every other at-rule, including `@media`,
            // `@import`, `@supports` and `@layer`.
            return Err(input.new_custom_error(()));
        };
        // Consume the prelude even though both supported at-rules ignore it.
        // `@font-face` has no prelude; `@page` may carry `:first` or `:left`,
        // which MuPDF ignores when it matches the page box.
        let _ = parse_nodes(input, 0)?;
        Ok(AtPrelude { kind })
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Self::Prelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::AtRule, ParseError<'i, Self::Error>> {
        let declarations = parse_declarations(input)?;
        match prelude.kind {
            AtKind::FontFace => self.sheet.font_faces.push(declarations),
            AtKind::Page => self.sheet.page.push(declarations),
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum AtKind {
    FontFace,
    Page,
}

struct AtPrelude {
    kind: AtKind,
}

fn parse_declarations<'i, 't>(
    input: &mut Parser<'i, 't>,
) -> Result<Vec<Declaration>, ParseError<'i, ()>> {
    let mut declarations = Vec::new();
    let mut parser = DeclarationList;
    for declaration in RuleBodyParser::new(input, &mut parser).flatten() {
        declarations.push(declaration);
    }
    Ok(declarations)
}

/// A declaration-only body. At-rules and nested qualified rules are rejected
/// by the default trait methods; `RuleBodyParser` then consumes them.
struct DeclarationList;

impl<'i> DeclarationParser<'i> for DeclarationList {
    type Declaration = Declaration;
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _declaration_start: &ParserState,
    ) -> Result<Self::Declaration, ParseError<'i, Self::Error>> {
        let nodes = parse_nodes(input, 0)?;
        let (nodes, important) = strip_important(nodes);
        let value = serialize(&nodes);
        if value.is_empty() {
            return Err(input.new_custom_error(()));
        }
        let name = if name.starts_with("--") {
            name.to_string()
        } else {
            name.to_ascii_lowercase()
        };
        Ok(Declaration {
            name,
            value,
            important,
        })
    }
}

impl<'i> AtRuleParser<'i> for DeclarationList {
    type Prelude = ();
    type AtRule = Declaration;
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for DeclarationList {
    type Prelude = ();
    type QualifiedRule = Declaration;
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, Declaration, ()> for DeclarationList {
    fn parse_declarations(&self) -> bool {
        true
    }

    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Consume a component-value list, descending into functions and blocks.
fn parse_nodes<'i, 't>(
    input: &mut Parser<'i, 't>,
    depth: usize,
) -> Result<Vec<Node>, ParseError<'i, ()>> {
    if depth > MAX_DEPTH {
        return Err(input.new_custom_error(()));
    }
    let mut nodes = Vec::new();
    loop {
        let token = match input.next_including_whitespace_and_comments() {
            Ok(token) => token.clone(),
            Err(error) => match error.kind {
                BasicParseErrorKind::EndOfInput => break,
                _ => return Err(input.new_custom_error(())),
            },
        };
        match token {
            Token::WhiteSpace(_) => nodes.push(Node::Whitespace),
            Token::Comment(_) => {}
            Token::Function(name) => {
                let children = input.parse_nested_block(|nested| parse_nodes(nested, depth + 1))?;
                let mut serialized = String::new();
                if serialize_identifier(&name, &mut serialized).is_err() {
                    return Err(input.new_custom_error(()));
                }
                nodes.push(Node::Function(serialized, children));
            }
            Token::ParenthesisBlock => {
                let children = input.parse_nested_block(|nested| parse_nodes(nested, depth + 1))?;
                nodes.push(Node::Block('(', children));
            }
            Token::SquareBracketBlock => {
                let children = input.parse_nested_block(|nested| parse_nodes(nested, depth + 1))?;
                nodes.push(Node::Block('[', children));
            }
            Token::CurlyBracketBlock => {
                let children = input.parse_nested_block(|nested| parse_nodes(nested, depth + 1))?;
                nodes.push(Node::Block('{', children));
            }
            Token::BadUrl(_) | Token::BadString(_) => return Err(input.new_custom_error(())),
            Token::CloseParenthesis | Token::CloseSquareBracket | Token::CloseCurlyBracket => {
                return Err(input.new_custom_error(()))
            }
            Token::CDO | Token::CDC => return Err(input.new_custom_error(())),
            token => {
                let mut text = String::new();
                let _ = token.to_css(&mut text);
                nodes.push(Node::Token(text));
            }
        }
    }
    Ok(nodes)
}

fn strip_important(mut nodes: Vec<Node>) -> (Vec<Node>, bool) {
    let mut end = nodes.len();
    while end > 0 && matches!(nodes[end - 1], Node::Whitespace) {
        end -= 1;
    }
    if end == 0 {
        return (nodes, false);
    }
    let important =
        matches!(&nodes[end - 1], Node::Token(text) if text.eq_ignore_ascii_case("important"));
    if !important {
        return (nodes, false);
    }
    let mut bang = end - 1;
    while bang > 0 && matches!(nodes[bang - 1], Node::Whitespace) {
        bang -= 1;
    }
    if bang == 0 || !matches!(&nodes[bang - 1], Node::Token(text) if text == "!") {
        return (nodes, false);
    }
    nodes.truncate(bang - 1);
    while matches!(nodes.last(), Some(Node::Whitespace)) {
        nodes.pop();
    }
    (nodes, true)
}

fn serialize(nodes: &[Node]) -> String {
    let mut out = String::new();
    serialize_into(nodes, &mut out);
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn serialize_into(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Whitespace => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            Node::Token(text) => out.push_str(text),
            Node::Function(name, children) => {
                out.push_str(name);
                out.push('(');
                let start = out.len();
                serialize_into(children, out);
                if out.len() > start && out.ends_with(' ') {
                    out.pop();
                }
                out.push(')');
            }
            Node::Block(open, children) => {
                out.push(*open);
                serialize_into(children, out);
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push(match open {
                    '(' => ')',
                    '[' => ']',
                    _ => '}',
                });
            }
        }
    }
}

/// Reject selectors that are not a balanced, single selector list. Balanced
/// component values are guaranteed by `cssparser`; this only removes stray
/// top-level delimiters that would make MuPDF's recovery skip a whole block.
fn selectors_are_safe(selectors: &str) -> bool {
    if selectors.trim().is_empty() {
        return false;
    }
    let mut input = ParserInput::new(selectors);
    let mut parser = Parser::new(&mut input);
    loop {
        match parser.next_including_whitespace_and_comments() {
            Ok(Token::Semicolon)
            | Ok(Token::AtKeyword(_))
            | Ok(Token::CurlyBracketBlock)
            | Ok(Token::BadUrl(_))
            | Ok(Token::BadString(_))
            | Ok(Token::CloseParenthesis)
            | Ok(Token::CloseSquareBracket)
            | Ok(Token::CloseCurlyBracket) => return false,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    true
}

/// Validate one serialized declaration value. This is intentionally a
/// token-level check, not a per-property value grammar: MuPDF ignores values it
/// does not understand, while malformed or unbounded tokens are the real
/// robustness risk.
fn value_is_safe(value: &str, allow_url: bool) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    validate_value(&mut parser, allow_url, 0).is_ok()
}

fn validate_value<'i, 't>(
    input: &mut Parser<'i, 't>,
    allow_url: bool,
    depth: usize,
) -> Result<(), ()> {
    if depth > MAX_DEPTH {
        return Err(());
    }
    loop {
        let token = match input.next_including_whitespace_and_comments() {
            Ok(token) => token,
            Err(error) => match error.kind {
                BasicParseErrorKind::EndOfInput => break,
                _ => return Err(()),
            },
        };
        match token {
            Token::WhiteSpace(_) | Token::Comment(_) => {}
            Token::Ident(_)
            | Token::Hash(_)
            | Token::IDHash(_)
            | Token::QuotedString(_)
            | Token::Comma => {}
            Token::Delim('/') => {}
            Token::Number { value, .. } | Token::Dimension { value, .. } => {
                if !value.is_finite() || value.abs() > MAX_VALUE {
                    return Err(());
                }
            }
            Token::Percentage { unit_value, .. } => {
                let value = unit_value * 100.0;
                if !value.is_finite() || value.abs() > MAX_VALUE {
                    return Err(());
                }
            }
            Token::UnquotedUrl(url) => {
                if !allow_url || !url_is_safe(url) {
                    return Err(());
                }
            }
            Token::Function(name) => {
                if name.eq_ignore_ascii_case("url") {
                    if !allow_url {
                        return Err(());
                    }
                    let url = input.parse_nested_block(parse_quoted_url).map_err(|_| ())?;
                    if !url_is_safe(&url) {
                        return Err(());
                    }
                } else if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "rgb" | "rgba" | "hsl" | "hsla"
                ) {
                    input
                        .parse_nested_block(|nested| validate_nested(nested, depth + 1))
                        .map_err(|_| ())?;
                } else {
                    return Err(());
                }
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

fn validate_nested<'i, 't>(
    input: &mut Parser<'i, 't>,
    depth: usize,
) -> Result<(), ParseError<'i, ()>> {
    validate_value(input, false, depth).map_err(|()| input.new_custom_error(()))
}

fn parse_quoted_url<'i, 't>(input: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, ()>> {
    let mut url = None;
    loop {
        let token = match input.next_including_whitespace_and_comments() {
            Ok(token) => token,
            Err(error) => match error.kind {
                BasicParseErrorKind::EndOfInput => break,
                _ => return Err(input.new_custom_error(())),
            },
        };
        match token {
            Token::WhiteSpace(_) | Token::Comment(_) => {}
            Token::QuotedString(value) if url.is_none() => url = Some(value.to_string()),
            _ => return Err(input.new_custom_error(())),
        }
    }
    url.ok_or_else(|| input.new_custom_error(()))
}

/// Keep URLs inside the document archive. Absolute paths and schemes could
/// read arbitrary local files or reach the network; a parent component could
/// escape the archive root. MuPDF percent-decodes archive paths, so the
/// decoded form is checked too.
fn url_is_safe(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.len() > 1024 || !path_is_safe(url) {
        return false;
    }
    if url.contains('%') {
        return percent_decode(url).is_some_and(|decoded| path_is_safe(&decoded));
    }
    true
}

fn path_is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(':')
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && !path.split(['/', '\\']).any(|part| part == "..")
        && !path.chars().any(char::is_control)
}

/// Decode `%XX` escapes the way MuPDF does. Invalid escapes stay literal.
fn percent_decode(value: &str) -> Option<String> {
    if !value.contains('%') {
        return None;
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_style_rules_and_at_rules() {
        let sheet = StyleSheet::parse(
            r#"
            /* a comment */
            @font-face {
                font-family: "Embedded Face";
                src: url(fonts/face.ttf);
            }
            @page :first { margin: 2em; }
            body, html {
                background: #f5e6c8 url("paper.png") no-repeat;
                color: rgb(30 30 30);
                font-family: "Noto Serif", serif !important;
            }
            @media print { body { color: red; } }
            "#,
        );
        assert_eq!(sheet.font_faces.len(), 1);
        assert_eq!(sheet.page.len(), 1);
        assert_eq!(sheet.rules.len(), 1);
        let rule = &sheet.rules[0];
        assert_eq!(rule.selectors, "body, html");
        assert_eq!(rule.declarations.len(), 3);
        assert_eq!(rule.declarations[2].name, "font-family");
        assert_eq!(rule.declarations[2].value, "\"Noto Serif\", serif");
        assert!(rule.declarations[2].important);
        assert_eq!(sheet.font_family().as_deref(), Some("Noto Serif"));
    }

    #[test]
    fn safe_css_keeps_supported_declarations_and_drops_the_rest() {
        let sheet = StyleSheet::parse(
            r#"
            @font-face {
                font-family: "Face";
                src: url(fonts/face.ttf);
                unicode-range: U+0000-00FF;
            }
            @page { size: 612pt 792pt; margin: 1em; }
            body {
                color: #111111;
                font-family: "Face", serif;
                display: flex;
                transform: rotate(90deg);
                opacity: 0.5;
                --mureader-accent: #d7a45e;
                --internal: red;
            }
            a { color: blue; }
            @media print { body { color: red; } }
            "#,
        );
        let css = sheet.safe_css();
        assert!(css.contains("@font-face"));
        assert!(css.contains("src: url(fonts/face.ttf);"));
        assert!(!css.contains("unicode-range"));
        assert!(css.contains("@page"));
        assert!(css.contains("margin: 1em;"));
        assert!(!css.contains("size:"));
        assert!(css.contains("color: #111111;"));
        assert!(css.contains("display: flex;"));
        assert!(!css.contains("transform"));
        assert!(!css.contains("opacity"));
        assert!(css.contains("--mureader-accent: #d7a45e;"));
        assert!(!css.contains("--internal"));
        assert!(css.contains("a {\n    color: blue;\n}"));
        assert!(!css.contains("@media"));
    }

    #[test]
    fn safe_css_keeps_only_a_single_relative_font_source() {
        let sheet = StyleSheet::parse(
            r#"
            @font-face { font-family: "One"; src: url(fonts/one.ttf); }
            @font-face { font-family: "Space"; src: url(fonts/my%20font.ttf); }
            @font-face { font-family: "List"; src: url(a.ttf), url(b.ttf); }
            @font-face { font-family: "Format"; src: url(c.woff2) format("woff2"); }
            @font-face { src: url(d.ttf); }
            "#,
        );
        let css = sheet.safe_css();
        assert!(css.contains("font-family: \"One\";"));
        assert!(css.contains("font-family: \"Space\";"));
        assert!(!css.contains("font-family: \"List\";"));
        assert!(!css.contains("font-family: \"Format\";"));
        assert!(!css.contains("d.ttf"));
    }

    #[test]
    fn safe_css_rejects_unbounded_values_and_unsafe_urls() {
        let sheet = StyleSheet::parse(
            r#"
            body {
                margin: 1e9px;
                font-size: 12pt;
                background: url(../../etc/passwd);
                list-style-image: url("https://example.com/x.png");
                border: 1px solid #000;
                background: url(%2e%2e/%2e%2e/etc/passwd);
                list-style-image: url(%2Fetc/passwd);
            }
            "#,
        );
        let css = sheet.safe_css();
        assert!(!css.contains("1e9"));
        assert!(!css.contains("passwd"));
        assert!(!css.contains("example.com"));
        assert!(css.contains("font-size: 12pt;"));
        assert!(css.contains("border: 1px solid #000;"));
    }

    #[test]
    fn safe_css_rejects_var_and_calc_because_mupdf_cannot_resolve_them() {
        let sheet = StyleSheet::parse(
            r#"
            :root { --paper: #faf3e0; }
            body {
                background: var(--paper);
                width: calc(100% - 2em);
                color: #222222;
            }
            "#,
        );
        let css = sheet.safe_css();
        assert!(!css.contains("var("));
        assert!(!css.contains("calc("));
        assert!(css.contains("color: #222222;"));
    }

    #[test]
    fn custom_properties_are_case_sensitive_and_last_wins() {
        let sheet = StyleSheet::parse(
            r#"
            :root { --mureader-accent: #111111; --mureader-Accent: #222222; }
            body { --mureader-accent: #333333; }
            "#,
        );
        let variables = sheet.custom_properties();
        assert_eq!(
            variables.get("--mureader-accent").map(String::as_str),
            Some("#333333")
        );
        assert_eq!(
            variables.get("--mureader-Accent").map(String::as_str),
            Some("#222222")
        );
    }

    #[test]
    fn font_family_ignores_font_face_and_var() {
        let sheet = StyleSheet::parse(
            r#"
            @font-face { font-family: "Embedded"; src: url(x); }
            body { font-family: var(--body-face); }
            p { font-family: "Body Face", serif; }
            "#,
        );
        assert_eq!(sheet.font_family().as_deref(), Some("Body Face"));

        let sheet = StyleSheet::parse(r#"body { font-family: "Foo, Bar", serif; }"#);
        assert_eq!(sheet.font_family().as_deref(), Some("Foo, Bar"));
    }

    #[test]
    fn malformed_css_is_skipped_without_losing_later_rules() {
        let sheet = StyleSheet::parse(
            r#"
            body { color: red; broken }
            @garbage nope;
            p { color: #222222; }
            "#,
        );
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors, "body");
        assert_eq!(sheet.rules[1].selectors, "p");
        assert_eq!(sheet.rules[1].declarations[0].value, "#222222");
    }

    #[test]
    fn at_rule_selector_is_not_passed_to_mupdf() {
        let sheet = StyleSheet::parse("@page :left { margin: 1em; }");
        assert_eq!(sheet.safe_css(), "@page {\n    margin: 1em;\n}\n");
    }
}
