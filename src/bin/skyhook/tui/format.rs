use skyhook::provider::protocol::Usage;
pub use skyhook::session::stats::agent_label;
pub fn number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.)
    } else if n >= 1000 {
        let value = format!("{:.1}", n as f64 / 1000.);
        format!("{}k", value.trim_end_matches(".0"))
    } else {
        n.to_string()
    }
}
pub fn footer(usage: Usage, context: Option<(u64, u64)>) -> String {
    footer_stats(usage, context).join(" · ")
}

pub fn footer_stats(usage: Usage, context: Option<(u64, u64)>) -> [String; 3] {
    let context =
        context
            .filter(|(_, capacity)| *capacity > 0)
            .map_or("—".into(), |(current, total)| {
                format!(
                    "{:.0}% ({}/{})",
                    current as f64 / total as f64 * 100.,
                    number(current),
                    number(total)
                )
            });
    [
        number(usage.output_tokens),
        format!(
            "{}({})",
            number(usage.input_tokens.saturating_add(usage.cached_input_tokens)),
            number(usage.input_tokens)
        ),
        context,
    ]
}

/// Remove terminal controls while preserving line boundaries and expanding tabs.
pub fn clean(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => output.push_str("    "),
            '\n' => output.push(ch),
            ch if !ch.is_control() => output.push(ch),
            _ => {}
        }
    }
    output
}

/// Text that is safe to paint: `clean` has been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clean(String);

impl From<String> for Clean {
    fn from(text: String) -> Self {
        if text.contains(|ch: char| ch.is_control() && ch != '\n') {
            Self(clean(&text))
        } else {
            Self(text)
        }
    }
}

impl From<&str> for Clean {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl std::ops::Deref for Clean {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

pub fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_cleaning_preserves_unicode_lines_and_expands_tabs() {
        for (text, expected) in [
            ("plain 界👩‍💻", "plain 界👩‍💻"),
            ("a\tb\n", "a    b\n"),
            ("\u{1b}[31mred\u{1b}[0m\r\0", "[31mred[0m"),
        ] {
            assert_eq!(clean(text), expected);
            assert_eq!(*Clean::from(text), *expected);
        }
    }
}
