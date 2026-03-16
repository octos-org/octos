//! Multilingual abort trigger detection.
//!
//! Recognizes 30+ trigger words across 9 languages for cancelling
//! in-flight agent operations via chat messages.

/// Check if a message is an abort trigger.
///
/// Matches against known stop/cancel words in English, Chinese, Japanese,
/// Russian, French, Spanish, Hindi, Arabic, and Korean.
pub fn is_abort_trigger(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    ABORT_TRIGGERS.iter().any(|t| normalized == *t)
}

static ABORT_TRIGGERS: &[&str] = &[
    // English — only unambiguous abort words (avoid "wait", "exit", "para"
    // which are too common in normal conversation)
    "stop",
    "abort",
    "cancel",
    "halt",
    "interrupt",
    "quit",
    "enough",
    // Chinese
    "停",
    "停止",
    "取消",
    "停下",
    "别说了",
    // Japanese
    "やめて",
    "止めて",
    "ストップ",
    // Russian
    "стоп",
    "отмена",
    "хватит",
    // French
    "arrête",
    "annuler",
    // Spanish ("para" excluded — too ambiguous across languages)
    "detente",
    "cancelar",
    // Hindi
    "रुको",
    "बंद करो",
    // Arabic
    "توقف",
    "قف",
    // Korean
    "멈춰",
    "중지",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_english_triggers() {
        assert!(is_abort_trigger("stop"));
        assert!(is_abort_trigger("STOP"));
        assert!(is_abort_trigger("  Stop  "));
        assert!(is_abort_trigger("cancel"));
        assert!(is_abort_trigger("abort"));
    }

    #[test]
    fn test_chinese_triggers() {
        assert!(is_abort_trigger("停止"));
        assert!(is_abort_trigger("取消"));
        assert!(is_abort_trigger("别说了"));
    }

    #[test]
    fn test_japanese_triggers() {
        assert!(is_abort_trigger("やめて"));
        assert!(is_abort_trigger("ストップ"));
    }

    #[test]
    fn test_non_triggers() {
        assert!(!is_abort_trigger("hello"));
        assert!(!is_abort_trigger("please stop talking about cats"));
        assert!(!is_abort_trigger("stopping point"));
        assert!(!is_abort_trigger(""));
        // Ambiguous words removed to avoid false positives
        assert!(!is_abort_trigger("wait"));
        assert!(!is_abort_trigger("exit"));
        assert!(!is_abort_trigger("para"));
    }
}
