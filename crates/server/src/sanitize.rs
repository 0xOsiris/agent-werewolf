//! Cleaning of untrusted player-supplied strings.

/// Strips all control characters, trims, and truncates to `max` chars.
pub fn line(input: &str, max: usize) -> String {
    clean(input, max, |c| !c.is_control())
}

/// Like [`line`], but keeps `\n` so speech can span paragraphs.
pub fn text(input: &str, max: usize) -> String {
    clean(input, max, |c| c == '\n' || !c.is_control())
}

fn clean(input: &str, max: usize, keep: impl Fn(char) -> bool) -> String {
    let filtered: String = input
        .chars()
        .filter(|&c| keep(c) && !is_bidi_control(c))
        .collect();
    let truncated: String = filtered.trim().chars().take(max).collect();
    truncated.trim_end().to_owned()
}

/// Bidi overrides are format chars, not controls, but they can garble the projector.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_strips_controls_and_trims() {
        assert_eq!(line("  al\u{0}ice\n\t ", 32), "alice");
        assert_eq!(line("\u{202E}evil", 32), "evil");
    }

    #[test]
    fn text_keeps_newlines() {
        assert_eq!(text(" hi\r\nthere\u{7}  ", 600), "hi\nthere");
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(line("ééééé", 3), "ééé");
        assert_eq!(line("ab   cd", 4), "ab");
    }
}
