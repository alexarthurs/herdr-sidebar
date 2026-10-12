//! Syntax themes compiled into the binary on top of syntect's bundled set:
//! JetBrains' Darcula and IntelliJ (new UI) Dark palettes. They are plain
//! Rust tables rather than `.tmTheme` files so loading them needs no plist
//! parser (syntect's `plist-load`) and the repository carries no binary
//! theme dumps. The rules color only foregrounds and font styles, like every
//! other preview theme; the global colors feed the opt-in themed background.
//!
//! Beyond the usual scopes the tables cover Markdown quirks of two-face's
//! grammar: list/quote MARKERS are colored instead of the whole item, a lone
//! `~` (which opens a never-closed `markup.strikethrough`) stays at the plain
//! foreground, and `herdr-sidebar.shortcut-reference.markdown` colors the
//! `[LABEL]` shortcut references `syntax::shortcut_references` finds.

use std::str::FromStr;

use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};

/// One theme: global colors as `0xRRGGBB`, then `(scope selector,
/// foreground, font style)` rules in tmTheme precedence order. An empty font
/// style means "not set" (inherit), like a tmTheme rule without `fontStyle`.
pub struct ThemeSpec {
    pub name: &'static str,
    pub background: u32,
    pub foreground: u32,
    pub gutter: u32,
    pub caret: u32,
    pub selection: u32,
    pub line_highlight: u32,
    pub rules: &'static [(&'static str, Option<u32>, &'static str)],
}

fn rgb(hex: u32) -> Color {
    let [_, r, g, b] = hex.to_be_bytes();
    Color { r, g, b, a: 0xFF }
}

fn font_style(spec: &str) -> FontStyle {
    spec.split_whitespace()
        .fold(FontStyle::empty(), |style, word| match word {
            "bold" => style | FontStyle::BOLD,
            "italic" => style | FontStyle::ITALIC,
            "underline" => style | FontStyle::UNDERLINE,
            _ => style,
        })
}

impl ThemeSpec {
    /// The syntect theme. A selector that fails to parse is skipped rather
    /// than failing the whole theme (the tables are covered by a test).
    pub fn build(&self) -> Theme {
        Theme {
            name: Some(self.name.to_string()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(rgb(self.foreground)),
                background: Some(rgb(self.background)),
                caret: Some(rgb(self.caret)),
                line_highlight: Some(rgb(self.line_highlight)),
                selection: Some(rgb(self.selection)),
                gutter_foreground: Some(rgb(self.gutter)),
                ..ThemeSettings::default()
            },
            scopes: self
                .rules
                .iter()
                .filter_map(|(scope, foreground, font)| {
                    Some(ThemeItem {
                        scope: ScopeSelectors::from_str(scope).ok()?,
                        style: StyleModifier {
                            foreground: foreground.map(rgb),
                            background: None,
                            font_style: (!font.is_empty()).then(|| font_style(font)),
                        },
                    })
                })
                .collect(),
        }
    }
}

pub const DARCULA: ThemeSpec = ThemeSpec {
    name: "Darcula",
    background: 0x2B2B2B,
    foreground: 0xA9B7C6,
    gutter: 0x606366,
    caret: 0xBBBBBB,
    selection: 0x214283,
    line_highlight: 0x323232,
    rules: &[
        (
            "comment, punctuation.definition.comment",
            Some(0x808080),
            "italic",
        ),
        (
            "comment.block.documentation, comment.line.documentation",
            Some(0x629755),
            "italic",
        ),
        (
            "keyword, storage, storage.type, storage.modifier",
            Some(0xCC7832),
            "",
        ),
        ("keyword.operator", Some(0xA9B7C6), ""),
        (
            "punctuation.separator, punctuation.terminator",
            Some(0xCC7832),
            "",
        ),
        ("string", Some(0x6A8759), ""),
        (
            "constant.character.escape, constant.other.placeholder",
            Some(0xCC7832),
            "",
        ),
        ("constant.numeric", Some(0x6897BB), ""),
        ("constant.language", Some(0xCC7832), ""),
        (
            "constant.other, variable.other.constant, entity.name.constant",
            Some(0x9876AA),
            "italic",
        ),
        ("entity.name.function", Some(0xFFC66D), ""),
        ("variable.function, support.function", Some(0xA9B7C6), ""),
        (
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class",
            Some(0xA9B7C6),
            "",
        ),
        (
            "entity.name.type.parameter, variable.parameter.type",
            Some(0x20999D),
            "",
        ),
        (
            "variable.other.member, variable.other.property, meta.property-name, support.type.property-name",
            Some(0x9876AA),
            "",
        ),
        ("variable.language", Some(0x94558D), ""),
        ("variable.parameter", Some(0xA9B7C6), ""),
        (
            "meta.annotation, meta.decorator, entity.name.function.decorator, meta.attribute, punctuation.definition.annotation",
            Some(0xBBB529),
            "",
        ),
        (
            "entity.name.function.macro, support.macro",
            Some(0x908B25),
            "",
        ),
        (
            "storage.modifier.lifetime, entity.name.lifetime",
            Some(0x20999D),
            "",
        ),
        (
            "entity.name.tag, punctuation.definition.tag",
            Some(0xE8BF6A),
            "",
        ),
        ("entity.other.attribute-name", Some(0xBABABA), ""),
        (
            "meta.attribute-with-value string, string.quoted.double.html, string.quoted.single.html",
            Some(0xA5C261),
            "",
        ),
        (
            "support.type.property-name.json, entity.name.tag.yaml, meta.mapping.key, entity.name.tag.toml, support.type.property-name.toml",
            Some(0x9876AA),
            "",
        ),
        (
            "entity.name.section.toml, meta.tag.table.toml",
            Some(0xCC7832),
            "",
        ),
        ("invalid", Some(0xBC3F3C), ""),
        ("markup.inserted", Some(0x6A8759), ""),
        ("markup.deleted", Some(0xBC3F3C), ""),
        ("markup.changed", Some(0x6897BB), ""),
        (
            "markup.heading, markup.heading entity.name.section, markup.heading punctuation.definition.heading",
            Some(0xC77DBB),
            "bold italic",
        ),
        ("markup.heading.setext punctuation", Some(0xC77DBB), ""),
        ("markup.bold", Some(0xA9B7C6), "bold"),
        ("markup.italic", Some(0xA9B7C6), "italic"),
        ("markup.strikethrough", Some(0xA9B7C6), ""),
        ("markup.list, meta.paragraph.list", Some(0xA9B7C6), ""),
        (
            "markup.list.unnumbered.bullet, markup.list.numbered.bullet, punctuation.definition.list_item, punctuation.definition.list.begin",
            Some(0xCC7832),
            "",
        ),
        ("markup.quote", Some(0x629755), ""),
        ("punctuation.definition.blockquote", Some(0xCC7832), ""),
        ("markup.raw.inline, markup.raw", Some(0x6A8759), ""),
        (
            "punctuation.definition.raw.code-fence, meta.code-fence.definition, punctuation.definition.raw",
            Some(0x808080),
            "",
        ),
        ("constant.other.language-name", Some(0xCC7832), ""),
        (
            "punctuation.definition.link, punctuation.definition.metadata, punctuation.definition.image, punctuation.definition.constant.markdown, punctuation.separator.key-value.markdown",
            Some(0xCC7832),
            "",
        ),
        (
            "meta.link.inline.description, meta.link.reference.description, meta.image.inline.description, string.other.link.title, string.other.link.description",
            Some(0xCC7832),
            "",
        ),
        (
            "herdr-sidebar.shortcut-reference.markdown",
            Some(0xCC7832),
            "",
        ),
        ("markup.underline.link", Some(0x287BDE), "underline"),
        ("constant.other.reference.link", Some(0x9876AA), ""),
        (
            "meta.table punctuation.separator.table-cell, punctuation.section.table-header, meta.table.header-separator",
            Some(0xCC7832),
            "",
        ),
        (
            "meta.separator.thematic-break, meta.separator.markdown",
            Some(0x808080),
            "",
        ),
        (
            "text.html.markdown meta.tag punctuation.definition.tag, text.html.markdown entity.name.tag",
            Some(0xE8BF6A),
            "",
        ),
    ],
};

pub const INTELLIJ_DARK: ThemeSpec = ThemeSpec {
    name: "IntelliJ Dark",
    background: 0x1E1F22,
    foreground: 0xBCBEC4,
    gutter: 0x4B5059,
    caret: 0xCED0D6,
    selection: 0x214283,
    line_highlight: 0x26282E,
    rules: &[
        (
            "comment, punctuation.definition.comment",
            Some(0x7A7E85),
            "",
        ),
        (
            "comment.block.documentation, comment.line.documentation",
            Some(0x5F826B),
            "italic",
        ),
        (
            "keyword, storage, storage.type, storage.modifier",
            Some(0xCF8E6D),
            "",
        ),
        ("keyword.operator", Some(0xBCBEC4), ""),
        (
            "punctuation.separator, punctuation.terminator",
            Some(0xCF8E6D),
            "",
        ),
        ("string", Some(0x6AAB73), ""),
        (
            "constant.character.escape, constant.other.placeholder",
            Some(0xCF8E6D),
            "",
        ),
        ("constant.numeric", Some(0x2AACB8), ""),
        ("constant.language", Some(0xCF8E6D), ""),
        (
            "constant.other, variable.other.constant, entity.name.constant",
            Some(0xC77DBB),
            "italic",
        ),
        ("entity.name.function", Some(0x56A8F5), ""),
        ("variable.function, support.function", Some(0xBCBEC4), ""),
        (
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class",
            Some(0xBCBEC4),
            "",
        ),
        (
            "entity.name.type.parameter, variable.parameter.type",
            Some(0x16BAAC),
            "",
        ),
        (
            "variable.other.member, variable.other.property, meta.property-name, support.type.property-name",
            Some(0xC77DBB),
            "",
        ),
        ("variable.language", Some(0xCF8E6D), ""),
        ("variable.parameter", Some(0xBCBEC4), ""),
        (
            "meta.annotation, meta.decorator, entity.name.function.decorator, meta.attribute, punctuation.definition.annotation",
            Some(0xB3AE60),
            "",
        ),
        (
            "entity.name.function.macro, support.macro",
            Some(0x56A8F5),
            "",
        ),
        (
            "storage.modifier.lifetime, entity.name.lifetime",
            Some(0x20999D),
            "",
        ),
        (
            "entity.name.tag, punctuation.definition.tag",
            Some(0xD5B778),
            "",
        ),
        ("entity.other.attribute-name", Some(0xBABABA), ""),
        (
            "meta.attribute-with-value string, string.quoted.double.html, string.quoted.single.html",
            Some(0x6AAB73),
            "",
        ),
        (
            "support.type.property-name.json, entity.name.tag.yaml, meta.mapping.key, entity.name.tag.toml, support.type.property-name.toml",
            Some(0xC77DBB),
            "",
        ),
        (
            "entity.name.section.toml, meta.tag.table.toml",
            Some(0xCF8E6D),
            "",
        ),
        ("invalid", Some(0xF75464), ""),
        ("markup.inserted", Some(0x6AAB73), ""),
        ("markup.deleted", Some(0xF75464), ""),
        ("markup.changed", Some(0x56A8F5), ""),
        (
            "markup.heading, markup.heading entity.name.section, markup.heading punctuation.definition.heading",
            Some(0xC77DBB),
            "bold italic",
        ),
        ("markup.heading.setext punctuation", Some(0xC77DBB), ""),
        ("markup.bold", Some(0xBCBEC4), "bold"),
        ("markup.italic", Some(0xBCBEC4), "italic"),
        ("markup.strikethrough", Some(0xBCBEC4), ""),
        ("markup.list, meta.paragraph.list", Some(0xBCBEC4), ""),
        (
            "markup.list.unnumbered.bullet, markup.list.numbered.bullet, punctuation.definition.list_item, punctuation.definition.list.begin",
            Some(0xCF8E6D),
            "",
        ),
        ("markup.quote", Some(0x6AAB73), ""),
        ("punctuation.definition.blockquote", Some(0xCF8E6D), ""),
        ("markup.raw.inline, markup.raw", Some(0x6AAB73), ""),
        (
            "punctuation.definition.raw.code-fence, meta.code-fence.definition, punctuation.definition.raw",
            Some(0x7A7E85),
            "",
        ),
        ("constant.other.language-name", Some(0xCF8E6D), ""),
        (
            "punctuation.definition.link, punctuation.definition.metadata, punctuation.definition.image, punctuation.definition.constant.markdown, punctuation.separator.key-value.markdown",
            Some(0xCF8E6D),
            "",
        ),
        (
            "meta.link.inline.description, meta.link.reference.description, meta.image.inline.description, string.other.link.title, string.other.link.description",
            Some(0xCF8E6D),
            "",
        ),
        (
            "herdr-sidebar.shortcut-reference.markdown",
            Some(0xCF8E6D),
            "",
        ),
        ("markup.underline.link", Some(0x548AF7), "underline"),
        ("constant.other.reference.link", Some(0xC77DBB), ""),
        (
            "meta.table punctuation.separator.table-cell, punctuation.section.table-header, meta.table.header-separator",
            Some(0xCF8E6D),
            "",
        ),
        (
            "meta.separator.thematic-break, meta.separator.markdown",
            Some(0x7A7E85),
            "",
        ),
        (
            "text.html.markdown meta.tag punctuation.definition.tag, text.html.markdown entity.name.tag",
            Some(0xD5B778),
            "",
        ),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_parses() {
        for spec in [&DARCULA, &INTELLIJ_DARK] {
            assert_eq!(spec.build().scopes.len(), spec.rules.len(), "{}", spec.name);
            for (scope, _, font) in spec.rules {
                assert!(ScopeSelectors::from_str(scope).is_ok(), "{scope}");
                assert!(
                    font.split_whitespace()
                        .all(|w| matches!(w, "bold" | "italic" | "underline")),
                    "{font}"
                );
            }
        }
    }
}
