//! Colours: neutral UI surfaces, a semantic content palette, and the code palette.
//!
//! A product installs its theme once at startup; `Palette::DEFAULT` is Skyhook's
//! blue/cyan palette (OKLCH converted to sRGB, chroma reduced at fixed hue and
//! lightness until in gamut) and `SyntaxPalette::DEFAULT` colours code the same way
//! in every product.
pub use ratatui::style::Color;
use std::sync::OnceLock;
use syntect::highlighting::{
    Color as SyntaxColor, FontStyle, StyleModifier, Theme as SyntaxTheme, ThemeItem, ThemeSettings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub ui: Palette,
    pub syntax: SyntaxPalette,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ui: Palette::DEFAULT,
            syntax: SyntaxPalette::DEFAULT,
        }
    }
}

/// Semantic UI colours. Text colours keep WCAG contrast of at least 4.5:1 on
/// `user` and 3:1 on `selected`; `emphasis` and `error` must stay distinguishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
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
    /// Headings, inline code, markers, the selected tab, in-flight status.
    pub emphasis: Color,
    /// Bold text and entry titles.
    pub strong: Color,
    /// Links, targets and numbers.
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub caution: Color,
    pub error: Color,
}

impl Palette {
    pub const DEFAULT: Self = Self {
        base: Color::Rgb(0, 0, 0),
        panel: Color::Rgb(28, 28, 28),
        input: Color::Rgb(40, 40, 40),
        agent: Color::Rgb(34, 34, 34),
        user: Color::Rgb(44, 44, 44),
        selected: Color::Rgb(64, 64, 64),
        code_bg: Color::Rgb(16, 16, 16),
        fg: Color::Rgb(222, 225, 230),
        muted: Color::Rgb(146, 153, 163),
        emphasis: Color::Rgb(0, 185, 250),
        strong: Color::Rgb(97, 157, 255),
        accent: Color::Rgb(0, 206, 233),
        success: Color::Rgb(0, 208, 147),
        warning: Color::Rgb(246, 185, 1),
        caution: Color::Rgb(255, 140, 40),
        error: Color::Rgb(255, 102, 127),
    };
}

impl Default for Palette {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// An explicit colour: code highlighting takes no terminal-defined or indexed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl From<Rgb> for Color {
    fn from(Rgb(r, g, b): Rgb) -> Self {
        Self::Rgb(r, g, b)
    }
}

/// Foreground colours for highlighted code, by token class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxPalette {
    pub text: Rgb,
    pub comment: Rgb,
    pub keyword: Rgb,
    pub literal: Rgb,
    pub number: Rgb,
    pub string: Rgb,
    pub label: Rgb,
    pub regexp: Rgb,
    pub operator: Rgb,
    pub heading: Rgb,
}

impl SyntaxPalette {
    pub const DEFAULT: Self = Self {
        text: Rgb(222, 225, 230),
        comment: Rgb(146, 153, 163),
        keyword: Rgb(97, 157, 255),
        literal: Rgb(0, 185, 250),
        number: Rgb(0, 206, 233),
        string: Rgb(0, 208, 147),
        label: Rgb(0, 185, 250),
        regexp: Rgb(246, 185, 1),
        operator: Rgb(255, 102, 127),
        heading: Rgb(0, 185, 250),
    };

    /// Foreground-only syntax theme. Consumers must leave UI backgrounds alone.
    pub fn theme(&self) -> SyntaxTheme {
        let mut theme = SyntaxTheme {
            settings: ThemeSettings {
                foreground: Some(syntax_color(self.text)),
                ..ThemeSettings::default()
            },
            ..SyntaxTheme::default()
        };
        // More specific selectors override their containing string/keyword scope.
        for (scope, color, font_style) in [
            ("variable", self.text, FontStyle::empty()),
            ("comment", self.comment, FontStyle::empty()),
            (
                "keyword, storage, entity.name, support, constant, variable.function",
                self.keyword,
                FontStyle::empty(),
            ),
            ("constant.language", self.literal, FontStyle::empty()),
            ("constant.numeric", self.number, FontStyle::empty()),
            ("string", self.string, FontStyle::empty()),
            ("entity.name.label", self.label, FontStyle::empty()),
            ("string.regexp", self.regexp, FontStyle::empty()),
            ("keyword.operator", self.operator, FontStyle::empty()),
            ("invalid", self.operator, FontStyle::empty()),
            ("markup.heading", self.heading, FontStyle::BOLD),
            ("markup.bold, markup.italic", self.string, FontStyle::BOLD),
            ("markup.inserted", self.string, FontStyle::empty()),
            ("markup.deleted", self.operator, FontStyle::empty()),
            ("markup.changed", self.regexp, FontStyle::empty()),
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

impl Default for SyntaxPalette {
    fn default() -> Self {
        Self::DEFAULT
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Install the product's theme before the first frame. Code highlighting caches
/// its syntax theme on first use, so a later call changes nothing.
pub fn install(theme: Theme) {
    let _ = THEME.set(theme);
}

pub fn theme() -> &'static Theme {
    THEME.get_or_init(Theme::default)
}

fn syntax_color(Rgb(r, g, b): Rgb) -> SyntaxColor {
    SyntaxColor { r, g, b, a: 255 }
}
