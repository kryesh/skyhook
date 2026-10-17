//! Skyhook's colours: neutral UI surfaces and the semantic content palette.
//!
//! Semantic text and syntax roles share a blue/cyan palette.
//! OKLCH was converted to sRGB, reducing chroma
//! at fixed hue/lightness until in gamut. Secondary L is raised from .62 to .70;
//! other lightnesses retain their original values. We deliberately
//! keep Skyhook's existing neutral foreground/muted colours and all backgrounds.
use ratatui::style::Color;
use syntect::highlighting::{
    Color as SyntaxColor, FontStyle, StyleModifier, Theme as SyntaxTheme, ThemeItem, ThemeSettings,
};

pub struct Theme {
    pub base: Color,
    pub panel: Color,
    pub input: Color,
    pub agent: Color,
    pub user: Color,
    pub selected: Color,
    /// Markdown code-block surface; syntax spans and tool output stay foreground-only.
    pub code_bg: Color,
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
}

/// Text colours keep WCAG contrast of at least 4.5:1 on `user` and 3:1 on `selected`.
pub const THEME: Theme = {
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
    Theme {
        base: Color::Rgb(0, 0, 0),
        panel: Color::Rgb(28, 28, 28),
        input: Color::Rgb(40, 40, 40),
        agent: Color::Rgb(34, 34, 34),
        user: Color::Rgb(44, 44, 44),
        selected: Color::Rgb(64, 64, 64),
        code_bg: Color::Rgb(16, 16, 16),
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
    }
};

impl Theme {
    /// Foreground-only syntax theme. Consumers must leave UI backgrounds alone.
    pub fn syntax_theme(&self) -> SyntaxTheme {
        let mut theme = SyntaxTheme {
            name: Some("Skyhook dark".into()),
            settings: ThemeSettings {
                foreground: Some(syntax_color(self.fg)),
                ..ThemeSettings::default()
            },
            ..SyntaxTheme::default()
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
