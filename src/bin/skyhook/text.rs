//! Text shaping shared by every host.

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
