use skyhook::{identity::AgentId, provider::protocol::Usage};
pub fn agent_label(agent: &AgentId) -> String {
    if agent.path().is_empty() {
        return "root".into();
    }
    agent
        .path()
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(":")
}
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
    format!(
        "{} · {}({}) · {context}",
        number(usage.output_tokens),
        number(usage.input_tokens.saturating_add(usage.cached_input_tokens)),
        number(usage.input_tokens)
    )
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

    fn original_brief(value: &str, limit: usize) -> String {
        let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut chars = normalized.chars();
        let mut text: String = chars.by_ref().take(limit).collect();
        if chars.next().is_some() {
            text.push('…');
        }
        text
    }

    #[test]
    fn brief_preserves_normalization_and_character_limits() {
        for value in [
            "",
            " \t\n",
            "abc",
            "  a  b ",
            "a\t\nb",
            "界 👩‍💻",
            "a\u{a0}\u{2003}b",
            "a   ",
        ] {
            for limit in 0..20 {
                assert_eq!(
                    brief(value, limit),
                    original_brief(value, limit),
                    "{value:?}, {limit}"
                );
            }
        }
        // Exercise combinations of whitespace runs and multibyte characters.
        let alphabet = ['a', '界', ' ', '\t', '\n', '\u{2003}'];
        for mut seed in 0..1296 {
            let mut value = String::new();
            for _ in 0..4 {
                value.push(alphabet[seed % alphabet.len()]);
                seed /= alphabet.len();
            }
            for limit in 0..6 {
                assert_eq!(brief(&value, limit), original_brief(&value, limit));
            }
        }
    }

    #[test]
    fn brief_large_unbroken_source_needs_only_a_preview_allocation() {
        let value = "界".repeat(1024 * 1024);
        let preview = brief(&value, 40);
        assert_eq!(preview, format!("{}…", "界".repeat(40)));
        assert!(preview.capacity() < 1024);
        assert_eq!(brief("abc     ", 3), "abc");
        assert_eq!(brief("abc     d", 3), "abc…");
    }
}
