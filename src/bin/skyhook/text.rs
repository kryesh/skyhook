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

/// A span for people: tenths of a second under a minute, whole seconds above.
/// A negative span, from clocks disagreeing, reads as zero.
pub fn duration(span: chrono::TimeDelta) -> String {
    let tenths = span.num_milliseconds().max(0) / 100;
    let seconds = tenths / 10;
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}.{}s", tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn durations_format_for_people() {
        for (millis, expected) in [
            (-5, "0.0s"),
            (99, "0.0s"),
            (5_260, "5.2s"),
            (65_900, "1m 05s"),
            (3_725_000, "1h 02m 05s"),
        ] {
            assert_eq!(duration(TimeDelta::milliseconds(millis)), expected);
        }
    }
}
