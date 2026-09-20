use skyhook::provider::protocol::Usage;
pub use skyhook::session::stats::agent_label;
/// Normalize only as much text as the preview needs, without allocating a full
/// normalized copy. Whitespace-only tails still have to be inspected to preserve
/// the distinction between an exact fit and a truncated preview.
pub fn brief(value: &str, limit: usize) -> String {
    let mut text = String::with_capacity(value.len().min(limit.saturating_add(3)));
    let mut remaining = limit;
    let mut whitespace = false;
    let mut started = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            whitespace |= started;
            continue;
        }
        if whitespace {
            if remaining == 0 {
                text.push('…');
                return text;
            }
            text.push(' ');
            remaining -= 1;
            whitespace = false;
        }
        if remaining == 0 {
            text.push('…');
            return text;
        }
        text.push(ch);
        remaining -= 1;
        started = true;
    }
    text
}

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
pub fn push_clean(output: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '\t' => output.push_str("    "),
            '\n' => output.push(ch),
            ch if !ch.is_control() => output.push(ch),
            _ => {}
        }
    }
}

pub fn clean(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    push_clean(&mut output, text);
    output
}

pub fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_cleaning_preserves_unicode_lines_and_expands_tabs() {
        for text in ["", "plain 界👩‍💻", "a\tb\n", "\u{1b}[31mred\u{1b}[0m\r\0"] {
            let expected = text
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect::<String>()
                .replace('\t', "    ");
            assert_eq!(clean(text), expected);
            let mut prefixed = "prefix:".to_owned();
            push_clean(&mut prefixed, text);
            assert_eq!(prefixed, format!("prefix:{expected}"));
        }
    }
}
