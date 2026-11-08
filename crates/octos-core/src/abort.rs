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
