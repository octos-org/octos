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

/// Return a localized cancel response matching the trigger language.
pub fn abort_response(trigger: &str) -> &'static str {
    let t = trigger.trim().to_lowercase();
    let t = t.as_str();
    match t {
        "停" | "停止" | "取消" | "停下" | "别说了" => "🛑 已取消。",
        "やめて" | "止めて" | "ストップ" => "🛑 キャンセルしました。",
        "стоп" | "отмена" | "хватит" => "🛑 Отменено.",
        "arrête" | "annuler" => "🛑 Annulé.",
        "detente" | "cancelar" => "🛑 Cancelado.",
        "रुको" | "बंद करो" => "🛑 रद्द किया गया।",
        "توقف" | "قف" => "🛑 تم الإلغاء.",
        "멈춰" | "중지" => "🛑 취소되었습니다.",
        _ => "🛑 Cancelled.",
    }
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
    fn test_abort_response_language() {
        assert_eq!(abort_response("stop"), "🛑 Cancelled.");
        assert_eq!(abort_response("cancel"), "🛑 Cancelled.");
        assert_eq!(abort_response("停止"), "🛑 已取消。");
        assert_eq!(abort_response("取消"), "🛑 已取消。");
        assert_eq!(abort_response("やめて"), "🛑 キャンセルしました。");
        assert_eq!(abort_response("멈춰"), "🛑 취소되었습니다.");
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
