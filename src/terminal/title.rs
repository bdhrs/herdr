/// Leading glyphs agents use as an activity indicator in their terminal title.
/// Claude cycles the stars; pi prefixes everything with `π`.
const AGENT_ACTIVITY_GLYPHS: &str = "·✢✳✶✻✽◐◓◑◒π";

pub(crate) fn stripped_terminal_title(title: &str) -> Option<String> {
    let title = crate::platform::terminal_title_for_presentation(title).trim();
    if title.is_empty() {
        return None;
    }

    let mut chars = title.char_indices();
    let (_, first) = chars.next()?;
    let after_first = &title[first.len_utf8()..];
    let recognized =
        matches!(first, '\u{2800}'..='\u{28ff}') || AGENT_ACTIVITY_GLYPHS.contains(first);
    let stripped = if recognized
        && (after_first.is_empty() || after_first.chars().next().is_some_and(char::is_whitespace))
    {
        after_first.trim()
    } else {
        title
    };

    (!stripped.is_empty()).then(|| stripped.to_string())
}

#[cfg(test)]
mod tests {
    use super::stripped_terminal_title;

    #[test]
    fn strips_one_recognized_leading_activity_glyph() {
        for title in [
            "⠋ task",
            "✳ task",
            "  ⠙   task  ",
            "✢ task",
            "✻ task",
            "◐ task",
            "◓ task",
            "◑ task",
            "◒ task",
        ] {
            assert_eq!(stripped_terminal_title(title).as_deref(), Some("task"));
        }
        assert_eq!(
            stripped_terminal_title("⠋ ⠙ task").as_deref(),
            Some("⠙ task")
        );
    }

    #[test]
    fn strips_pis_leading_glyph_so_a_renamed_session_shows_only_its_name() {
        assert_eq!(
            stripped_terminal_title("π pi test").as_deref(),
            Some("pi test")
        );
        // An unnamed pi session is nothing but the glyph, which leaves no name at all —
        // callers then fall back to the agent's own label.
        assert_eq!(stripped_terminal_title("π"), None);
        assert_eq!(stripped_terminal_title("π   "), None);
        // Mid-title it is ordinary text, and π-words must survive.
        assert_eq!(stripped_terminal_title("πλάτων").as_deref(), Some("πλάτων"));
        assert_eq!(
            stripped_terminal_title("build π stage").as_deref(),
            Some("build π stage")
        );
    }

    #[test]
    fn preserves_unrecognized_or_unbounded_symbols() {
        for (title, expected) in [
            ("★task", "★task"),
            ("★ production", "★ production"),
            ("✨ task", "✨ task"),
            ("☼ status", "☼ status"),
            ("@ task", "@ task"),
            ("task ⠋ detail", "task ⠋ detail"),
            ("[prod] task", "[prod] task"),
        ] {
            assert_eq!(stripped_terminal_title(title).as_deref(), Some(expected));
        }
    }

    #[test]
    fn preserves_unicode_text_and_elides_empty_results() {
        assert_eq!(
            stripped_terminal_title(" ⠋ 修复🙂标题 ").as_deref(),
            Some("修复🙂标题")
        );
        assert_eq!(stripped_terminal_title("  "), None);
        assert_eq!(stripped_terminal_title("⠋   "), None);
    }

    #[cfg(windows)]
    #[test]
    fn strips_one_windows_elevation_decoration_before_activity_glyph() {
        assert_eq!(
            stripped_terminal_title("Administrator:   ⠋ task").as_deref(),
            Some("task")
        );
        assert_eq!(
            stripped_terminal_title("Administrator: Administrator: task").as_deref(),
            Some("Administrator: task")
        );
        assert_eq!(stripped_terminal_title("Administrator: "), None);
    }
}
