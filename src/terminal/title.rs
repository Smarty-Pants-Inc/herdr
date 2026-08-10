const CLAUDE_ACTIVITY_GLYPHS: &str = "·✢✳✶✻✽";

pub(crate) fn stripped_terminal_title(title: &str) -> Option<String> {
    let title = crate::platform::terminal_title_for_presentation(title).trim();
    if title.is_empty() {
        return None;
    }

    if let Some(stripped) = strip_leading_activity_glyph(title) {
        return (!stripped.is_empty()).then(|| stripped.to_string());
    }

    let mut chars = title.chars();
    let first = chars.next()?;
    let after_first = chars.as_str();
    if after_first.chars().next().is_some_and(char::is_whitespace) {
        let after_prefix = after_first.trim_start();
        if let Some(stripped) = strip_leading_activity_glyph(after_prefix) {
            return Some(if stripped.is_empty() {
                first.to_string()
            } else {
                format!("{first} {stripped}")
            });
        }
    }

    Some(title.to_string())
}
pub(crate) fn terminal_title_has_activity_glyph(title: &str) -> bool {
    let title = title.trim();
    if strip_leading_activity_glyph(title).is_some() {
        return true;
    }

    let mut chars = title.chars();
    let Some(_) = chars.next() else {
        return false;
    };
    let after_first = chars.as_str();
    after_first.chars().next().is_some_and(char::is_whitespace)
        && strip_leading_activity_glyph(after_first.trim_start()).is_some()
}

fn strip_leading_activity_glyph(title: &str) -> Option<&str> {
    let mut chars = title.chars();
    let first = chars.next()?;
    let after_first = chars.as_str();
    let recognized =
        matches!(first, '\u{2800}'..='\u{28ff}') || CLAUDE_ACTIVITY_GLYPHS.contains(first);
    (recognized
        && (after_first.is_empty() || after_first.chars().next().is_some_and(char::is_whitespace)))
    .then(|| after_first.trim())
}

#[cfg(test)]
mod tests {
    use super::{stripped_terminal_title, terminal_title_has_activity_glyph};

    #[test]
    fn strips_one_recognized_leading_activity_glyph() {
        for title in ["⠋ task", "✳ task", "  ⠙   task  ", "✢ task", "✻ task"] {
            assert_eq!(stripped_terminal_title(title).as_deref(), Some("task"));
        }
        assert_eq!(
            stripped_terminal_title("⠋ ⠙ task").as_deref(),
            Some("⠙ task")
        );
    }

    #[test]
    fn strips_activity_glyph_after_single_character_prefix() {
        for (title, expected) in [
            ("π ⠋ task", "π task"),
            ("  π   ⠙   task  ", "π task"),
            ("π ✳ task", "π task"),
            ("A ⠋ task", "A task"),
        ] {
            assert_eq!(stripped_terminal_title(title).as_deref(), Some(expected));
        }
        assert_eq!(
            stripped_terminal_title("π ⠋ ⠙ task").as_deref(),
            Some("π ⠙ task")
        );
        assert_eq!(stripped_terminal_title("π ⠋").as_deref(), Some("π"));
        for title in ["π⠋ task", "OMP ⠋ task"] {
            assert_eq!(stripped_terminal_title(title).as_deref(), Some(title));
        }
    }
    #[test]
    fn recognizes_activity_glyphs_in_supported_title_positions() {
        for title in ["⠋ task", "π ⠙ task", "  π   ✳ task  "] {
            assert!(terminal_title_has_activity_glyph(title));
        }
        for title in ["π > task", "OMP ⠋ task", "task ⠋ detail"] {
            assert!(!terminal_title_has_activity_glyph(title));
        }
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
