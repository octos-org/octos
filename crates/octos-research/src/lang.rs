//! BCP-47 language handling: normalization, matching, and the per-provider
//! locale parameters (GDELT `sourcelang`, Google News `hl`/`gl`/`ceid`).

/// Normalize a BCP-47 tag: `ZH_cn` → `zh-CN`, `zh-hant` → `zh-Hant`.
/// Returns `None` for empty/invalid input.
pub fn normalize(tag: &str) -> Option<String> {
    let mut parts = tag
        .trim()
        .split(['-', '_'])
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()));
    let primary = parts.next()?.to_ascii_lowercase();
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut out = primary;
    for sub in parts {
        out.push('-');
        match sub.len() {
            2 => out.push_str(&sub.to_ascii_uppercase()),
            4 => {
                let mut chars = sub.chars();
                if let Some(first) = chars.next() {
                    out.push(first.to_ascii_uppercase());
                    out.push_str(&chars.as_str().to_ascii_lowercase());
                }
            }
            _ => out.push_str(&sub.to_ascii_lowercase()),
        }
    }
    Some(out)
}

/// Primary subtag, lowercase (`zh-TW` → `zh`).
pub fn primary(tag: &str) -> String {
    tag.trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Whether `lang` (from a page or provider) matches any requested language,
/// comparing primary subtags. An empty request list matches everything.
pub fn matches_any(lang: &str, wanted: &[String]) -> bool {
    if wanted.is_empty() {
        return true;
    }
    let p = primary(lang);
    wanted.iter().any(|w| primary(w) == p)
}

/// (BCP-47 primary subtag, GDELT `sourcelang` name).
const GDELT_LANGS: &[(&str, &str)] = &[
    ("af", "afrikaans"),
    ("ar", "arabic"),
    ("bg", "bulgarian"),
    ("bn", "bengali"),
    ("ca", "catalan"),
    ("cs", "czech"),
    ("da", "danish"),
    ("de", "german"),
    ("el", "greek"),
    ("en", "english"),
    ("es", "spanish"),
    ("et", "estonian"),
    ("fa", "persian"),
    ("fi", "finnish"),
    ("fr", "french"),
    ("he", "hebrew"),
    ("hi", "hindi"),
    ("hr", "croatian"),
    ("hu", "hungarian"),
    ("id", "indonesian"),
    ("it", "italian"),
    ("ja", "japanese"),
    ("ko", "korean"),
    ("lt", "lithuanian"),
    ("lv", "latvian"),
    ("ms", "malay"),
    ("nl", "dutch"),
    ("no", "norwegian"),
    ("pl", "polish"),
    ("pt", "portuguese"),
    ("ro", "romanian"),
    ("ru", "russian"),
    ("sk", "slovak"),
    ("sl", "slovenian"),
    ("sr", "serbian"),
    ("sv", "swedish"),
    ("th", "thai"),
    ("tr", "turkish"),
    ("uk", "ukrainian"),
    ("ur", "urdu"),
    ("vi", "vietnamese"),
    ("zh", "chinese"),
];

/// GDELT `sourcelang:` value for a BCP-47 tag.
pub fn gdelt_sourcelang(tag: &str) -> Option<&'static str> {
    let p = primary(tag);
    GDELT_LANGS
        .iter()
        .find(|(code, _)| *code == p)
        .map(|(_, name)| *name)
}

/// BCP-47 primary subtag for a GDELT language name (`"Spanish"` → `es`).
pub fn from_gdelt_name(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_ascii_lowercase();
    GDELT_LANGS
        .iter()
        .find(|(_, n)| *n == lower)
        .map(|(code, _)| *code)
}

/// Google News locale triple `(hl, gl, ceid)` for a language and optional
/// region (ISO 3166-1 alpha-2). Defaults to US English.
pub fn google_news_locale(lang: Option<&str>, region: Option<&str>) -> (String, String, String) {
    let tag = lang.and_then(normalize).unwrap_or_else(|| "en".to_string());
    let p = primary(&tag);
    let explicit_region = tag
        .split('-')
        .skip(1)
        .find(|s| s.len() == 2)
        .map(|s| s.to_string());
    let region = region
        .map(|r| r.trim().to_ascii_uppercase())
        .filter(|r| r.len() == 2)
        .or(explicit_region);

    // Chinese needs a script-specific edition.
    if p == "zh" {
        let traditional = tag.contains("Hant")
            || matches!(region.as_deref(), Some("TW") | Some("HK") | Some("MO"));
        let gl = region.unwrap_or_else(|| if traditional { "TW" } else { "CN" }.to_string());
        return if traditional {
            ("zh-TW".into(), gl.clone(), format!("{gl}:zh-Hant"))
        } else {
            ("zh-CN".into(), gl.clone(), format!("{gl}:zh-Hans"))
        };
    }

    let default_region = match p.as_str() {
        "en" => "US",
        "ja" => "JP",
        "ko" => "KR",
        "de" => "DE",
        "fr" => "FR",
        "es" => "ES",
        "pt" => "BR",
        "it" => "IT",
        "ru" => "RU",
        "ar" => "EG",
        "hi" => "IN",
        "nl" => "NL",
        "sv" => "SE",
        "pl" => "PL",
        "tr" => "TR",
        "uk" => "UA",
        "vi" => "VN",
        "th" => "TH",
        "id" => "ID",
        "he" => "IL",
        _ => "US",
    };
    let gl = region.unwrap_or_else(|| default_region.to_string());
    let hl = if p == "en" || p == "pt" || p == "es" {
        format!("{p}-{gl}")
    } else {
        p.clone()
    };
    let ceid = format!("{gl}:{p}");
    (hl, gl, ceid)
}

/// Best-effort language guess from the query's script, used as a default
/// when the caller gives no `lang`: Hangul → ko, kana → ja, CJK → zh.
pub fn guess_from_script(query: &str) -> Option<&'static str> {
    let mut cjk = false;
    let mut kana = false;
    let mut hangul = false;
    for ch in query.chars() {
        let c = ch as u32;
        if (0xAC00..=0xD7AF).contains(&c) {
            hangul = true;
        } else if (0x3040..=0x30FF).contains(&c) {
            kana = true;
        } else if (0x4E00..=0x9FFF).contains(&c) {
            cjk = true;
        }
    }
    if hangul {
        Some("ko")
    } else if kana {
        Some("ja")
    } else if cjk {
        Some("zh")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_normalize_bcp47_tags() {
        assert_eq!(normalize("ZH_cn").as_deref(), Some("zh-CN"));
        assert_eq!(normalize("zh-hant").as_deref(), Some("zh-Hant"));
        assert_eq!(normalize("en").as_deref(), Some("en"));
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("english"), None);
    }

    #[test]
    fn should_match_on_primary_subtag() {
        let wanted = vec!["en".to_string(), "zh-CN".to_string()];
        assert!(matches_any("en-GB", &wanted));
        assert!(matches_any("zh-TW", &wanted));
        assert!(!matches_any("de", &wanted));
        assert!(matches_any("de", &[]));
    }

    #[test]
    fn should_map_gdelt_language_names_both_ways() {
        assert_eq!(gdelt_sourcelang("es-MX"), Some("spanish"));
        assert_eq!(from_gdelt_name("Spanish"), Some("es"));
        assert_eq!(gdelt_sourcelang("xx"), None);
    }

    #[test]
    fn should_build_google_news_locales() {
        assert_eq!(
            google_news_locale(None, None),
            ("en-US".into(), "US".into(), "US:en".into())
        );
        assert_eq!(
            google_news_locale(Some("zh-TW"), None),
            ("zh-TW".into(), "TW".into(), "TW:zh-Hant".into())
        );
        assert_eq!(
            google_news_locale(Some("zh"), None),
            ("zh-CN".into(), "CN".into(), "CN:zh-Hans".into())
        );
        assert_eq!(
            google_news_locale(Some("de"), Some("at")),
            ("de".into(), "AT".into(), "AT:de".into())
        );
        assert_eq!(
            google_news_locale(Some("en"), Some("GB")),
            ("en-GB".into(), "GB".into(), "GB:en".into())
        );
    }

    #[test]
    fn should_guess_language_from_script() {
        assert_eq!(guess_from_script("美国和伊朗和谈"), Some("zh"));
        assert_eq!(guess_from_script("アメリカとイラン"), Some("ja"));
        assert_eq!(guess_from_script("미국과 이란"), Some("ko"));
        assert_eq!(guess_from_script("peace talks"), None);
    }
}
