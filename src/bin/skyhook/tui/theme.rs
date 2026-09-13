//! Shared content colours, independent of Skyhook's neutral UI surfaces.
//!
//! Semantic text and syntax roles share a blue/cyan palette.
//! OKLCH was converted to sRGB, reducing chroma
//! at fixed hue/lightness until in gamut. Secondary L is raised from .62 to .70;
//! other lightnesses retain their original values. We deliberately
//! keep Skyhook's existing neutral foreground/muted colours and all backgrounds.
use ratatui::style::Color;
use syntect::highlighting::{
    Color as SyntaxColor, FontStyle, StyleModifier, Theme, ThemeItem, ThemeSettings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentTheme {
    pub fg: Color,
    pub muted: Color,
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub info: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub heading: Color,
    pub strong: Color,
    pub quote: Color,
    pub inline_code: Color,
    /// Markdown code-block surface; syntax spans and tool output stay foreground-only.
    pub code_bg: Color,
}

impl ContentTheme {
    pub const fn new() -> Self {
        let (fg, muted, primary, secondary, accent, info, success, warning, error) = (
            Color::Rgb(222, 225, 230),
            Color::Rgb(146, 153, 163),
            Color::Rgb(0, 185, 250),
            Color::Rgb(97, 157, 255),
            Color::Rgb(0, 206, 233),
            Color::Rgb(0, 185, 250),
            Color::Rgb(0, 208, 147),
            Color::Rgb(246, 185, 1),
            Color::Rgb(255, 102, 127),
        );
        Self {
            fg,
            muted,
            primary,
            secondary,
            accent,
            info,
            success,
            warning,
            error,
            heading: primary,
            strong: secondary,
            quote: muted,
            inline_code: primary,
            code_bg: Color::Rgb(16, 16, 16),
        }
    }

    /// Foreground-only syntax theme. Consumers must leave UI backgrounds alone.
    pub fn syntax_theme(self) -> Theme {
        let mut theme = Theme {
            name: Some("Skyhook dark".into()),
            settings: ThemeSettings {
                foreground: Some(syntax_color(self.fg)),
                ..ThemeSettings::default()
            },
            ..Theme::default()
        };
        // More specific selectors override their containing string/keyword scope.
        for (scope, color, font_style) in [
            ("variable", self.fg, FontStyle::empty()),
            ("comment", self.muted, FontStyle::empty()),
            (
                "keyword, storage, entity.name, support, constant, variable.function",
                self.secondary,
                FontStyle::empty(),
            ),
            ("constant.language", self.primary, FontStyle::empty()),
            ("constant.numeric", self.accent, FontStyle::empty()),
            ("string", self.success, FontStyle::empty()),
            ("entity.name.label", self.info, FontStyle::empty()),
            ("string.regexp", self.warning, FontStyle::empty()),
            ("keyword.operator", self.error, FontStyle::empty()),
            ("invalid", self.error, FontStyle::empty()),
            ("markup.heading", self.heading, FontStyle::BOLD),
            ("markup.bold, markup.italic", self.success, FontStyle::BOLD),
            ("markup.inserted", self.success, FontStyle::empty()),
            ("markup.deleted", self.error, FontStyle::empty()),
            ("markup.changed", self.warning, FontStyle::empty()),
        ] {
            theme.scopes.push(ThemeItem {
                scope: scope.parse().expect("valid built-in syntax selector"),
                style: StyleModifier {
                    foreground: Some(syntax_color(color)),
                    font_style: Some(font_style),
                    ..StyleModifier::default()
                },
            });
        }
        theme
    }
}

fn syntax_color(color: Color) -> SyntaxColor {
    let Color::Rgb(r, g, b) = color else {
        unreachable!("content colours are always explicit RGB")
    };
    SyntaxColor { r, g, b, a: 255 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntect::{highlighting::Highlighter, parsing::Scope};

    #[test]
    fn content_roles_preserve_neutrals() {
        let theme = ContentTheme::new();
        assert_eq!(theme.fg, Color::Rgb(222, 225, 230));
        assert_eq!(theme.heading, theme.primary);
        assert_eq!(theme.strong, theme.secondary);
        assert_eq!(theme.quote, theme.muted);
        assert_eq!(theme.inline_code, theme.primary);
        assert_ne!(theme.primary, theme.secondary);
        assert_ne!(theme.accent, theme.success);
    }

    #[test]
    fn syntax_scopes_follow_semantic_roles() {
        let colors = ContentTheme::new();
        let theme = colors.syntax_theme();
        assert_eq!(theme.settings.background, None);
        assert!(
            theme
                .scopes
                .iter()
                .all(|item| item.style.background.is_none())
        );
        let highlighter = Highlighter::new(&theme);
        for (scope, expected) in [
            ("variable.other", colors.fg),
            ("comment.line", colors.muted),
            ("keyword.control", colors.secondary),
            ("storage.type", colors.secondary),
            ("entity.name.function", colors.secondary),
            ("variable.function", colors.secondary),
            ("constant.language.boolean", colors.primary),
            ("constant.numeric", colors.accent),
            ("string.quoted.double", colors.success),
            ("entity.name.label", colors.info),
            ("string.regexp", colors.warning),
            ("keyword.operator.arithmetic", colors.error),
            ("markup.inserted", colors.success),
            ("markup.deleted", colors.error),
        ] {
            let style = highlighter.style_for_stack(&[Scope::new(scope).unwrap()]);
            assert_eq!(style.foreground, syntax_color(expected), "{scope}");
        }
    }

    #[test]
    fn semantic_colours_have_text_contrast_on_existing_surfaces() {
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(r, g, b) = color else {
                unreachable!()
            };
            let linear = |byte: u8| {
                let c = f64::from(byte) / 255.;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        }
        let theme = ContentTheme::new();
        // Brightest surface, including selection.
        let bg = Color::Rgb(64, 64, 64);
        for color in [
            theme.fg,
            theme.muted,
            theme.primary,
            theme.secondary,
            theme.accent,
            theme.info,
            theme.success,
            theme.warning,
            theme.error,
        ] {
            let a = luminance(color);
            let b = luminance(bg);
            let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(contrast >= 3., "{color:?} contrast {contrast}");
            // Normal content is on user/agent backgrounds, not selection.
            let bg = Color::Rgb(44, 44, 44);
            let b = luminance(bg);
            let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(contrast >= 4.5, "{color:?} contrast {contrast}");
        }
    }
}
