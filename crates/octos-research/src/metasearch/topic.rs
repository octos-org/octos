//! Whether a headline (title and snippet) is about the query.
//!
//! Engines that cannot search (publisher feeds) list every entry and keep
//! the ones that match the query. A match is either the query as a phrase,
//! or every significant term of it:
//!
//! - Words match whole words ("ai" is not in "said"; "act" matches "acts"
//!   but not "action"). Letters and digits form words; any other character
//!   separates them, so "AI能力" holds the word "ai".
//! - CJK text has no spaces: a CJK term matches anywhere in the CJK text,
//!   with punctuation and spaces between CJK characters ignored
//!   (`台风桦加沙` matches `台风“桦加沙”`).
//! - Stop-words ("the", "of", "news", "的", "最新"…) and one-letter or
//!   one-character terms are not significant, unless nothing else is left.
//! - Matching some terms is never enough: "EU AI Act" does not match "UK
//!   police arrest 5 over a terrorist act" or "AI in schools".

use std::collections::HashSet;

use crate::text::is_cjk;

/// Words that say nothing about the topic. Lowercase.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "is", "latest",
    "new", "news", "of", "on", "or", "the", "to", "today", "update", "updates", "vs", "versus",
    "was", "were", "what", "who", "why", "with", "about", "de", "la", "le", "les", "des", "du",
    "et", "der", "die", "das", "und", "el", "los", "las", "y", "的", "和", "与", "與", "及",
    "最新", "新闻", "新聞", "消息", "报道", "報導",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Token {
    /// Letters and digits (lowercase).
    Word(String),
    /// A run of CJK characters.
    Cjk(String),
}

impl Token {
    fn text(&self) -> &str {
        match self {
            Token::Word(s) | Token::Cjk(s) => s,
        }
    }

    fn chars(&self) -> usize {
        self.text().chars().count()
    }
}

fn tokens(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if is_cjk(c) {
            if !word.is_empty() {
                out.push(Token::Word(std::mem::take(&mut word)));
            }
            cjk.push(c);
        } else if c.is_alphanumeric() {
            if !cjk.is_empty() {
                out.push(Token::Cjk(std::mem::take(&mut cjk)));
            }
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push(Token::Word(std::mem::take(&mut word)));
            }
            if !cjk.is_empty() {
                out.push(Token::Cjk(std::mem::take(&mut cjk)));
            }
        }
    }
    if !word.is_empty() {
        out.push(Token::Word(word));
    }
    if !cjk.is_empty() {
        out.push(Token::Cjk(cjk));
    }
    out
}

/// A query, ready to test headlines against.
#[derive(Debug, Clone)]
pub struct QueryMatcher {
    /// The query's tokens without one-letter words, for the phrase test.
    phrase: Vec<String>,
    /// Terms every one of which must match.
    required: Vec<Token>,
}

impl QueryMatcher {
    pub fn new(query: &str) -> Self {
        let toks = tokens(query);
        let phrase: Vec<String> = toks
            .iter()
            .filter(|t| t.chars() >= 2 || matches!(t, Token::Cjk(_)))
            .map(|t| t.text().to_string())
            .collect();
        let mut terms: Vec<Token> = Vec::new();
        for t in toks {
            if t.chars() >= 2 && !terms.contains(&t) {
                terms.push(t);
            }
        }
        let significant: Vec<Token> = terms
            .iter()
            .filter(|t| !STOP_WORDS.contains(&t.text()))
            .cloned()
            .collect();
        let required = if significant.is_empty() {
            terms
        } else {
            significant
        };
        Self { phrase, required }
    }

    /// Whether the query has anything to match (a query of only
    /// one-letter words has not; every text then matches).
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }

    /// Whether `text` (a title and snippet) is about the query.
    pub fn matches(&self, text: &str) -> bool {
        if self.required.is_empty() {
            return true;
        }
        let toks = tokens(text);
        // The phrase: the query's tokens in order, one-letter words aside
        // ("EU's AI Act" holds "eu ai act").
        let seq: Vec<&str> = toks
            .iter()
            .filter(|t| t.chars() >= 2 || matches!(t, Token::Cjk(_)))
            .map(Token::text)
            .collect();
        if !self.phrase.is_empty()
            && seq
                .windows(self.phrase.len())
                .any(|w| w.iter().zip(&self.phrase).all(|(a, b)| a == b))
        {
            return true;
        }
        let words: HashSet<&str> = toks
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w.as_str()),
                Token::Cjk(_) => None,
            })
            .collect();
        // CJK runs joined across punctuation and spaces, split at words.
        let mut cjk = String::new();
        for t in &toks {
            match t {
                Token::Cjk(s) => cjk.push_str(s),
                Token::Word(_) => cjk.push('|'),
            }
        }
        self.required.iter().all(|term| match term {
            Token::Word(w) => {
                words.contains(w.as_str())
                    || (w.chars().count() >= 3
                        && (words.contains(format!("{w}s").as_str())
                            || words.contains(format!("{w}es").as_str())))
            }
            Token::Cjk(c) => cjk.contains(c.as_str()),
        })
    }
}

/// Whether `text` matches `query` (see the module docs).
pub fn matches_query(query: &str, text: &str) -> bool {
    QueryMatcher::new(query).matches(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_not_match_when_a_headline_has_only_some_query_terms() {
        // publisher_feeds ranked these 2nd and 3rd for "EU AI Act" in
        // validation 3.
        let q = QueryMatcher::new("EU AI Act");
        for off in [
            "'The last thing you expect': Locals near UK airbase react to 'terrorist act' arrests",
            "Welcome to the 'Wild West' of AI in schools: Research is scarce, but experiments abound",
            "EU debates need for NATO-style response to Russia 'hybrid attacks'",
            "Pope Leo to speak on EU integration from French border town of Metz",
            "Trump confirms meeting with Anthropic's Amodei, repeats dismissal of AI fears",
        ] {
            assert!(!q.matches(off), "{off}");
        }
    }

    #[test]
    fn should_match_when_a_headline_has_the_phrase_or_every_significant_term() {
        let q = QueryMatcher::new("EU AI Act");
        for on in [
            "Deployment of AI Recruitment Tools in the EU: Employer Obligations Under GDPR and EU AI Act",
            "Brussels delays parts of the EU's AI Act",
            "AI Act: what the EU's new rules mean for chatbots",
            "The EU AI Acts' high-risk rules",
        ] {
            assert!(q.matches(on), "{on}");
        }
        // Stop-words are not required.
        let q = QueryMatcher::new("latest news on the Strait of Hormuz");
        assert!(q.matches("Iran says it will reopen Hormuz strait once conditions are met"));
        assert!(!q.matches("Tankers wait outside the strait"));
    }

    #[test]
    fn should_match_whole_words_only_when_terms_are_short() {
        let q = QueryMatcher::new("AI");
        assert!(!q.matches("Officials said the plan was fair"));
        assert!(q.matches("Is A.I. coming for your job? AI tools spread"));
        assert!(q.matches("AI能力強大 比爾蓋茲：落入惡人手上可殺害10億人"));
        assert!(q.matches("多方预测美中峰会聚焦AI”护栏”及稀土供应"));
        let q = QueryMatcher::new("act");
        assert!(!q.matches("A factory fire in the capital"));
        assert!(!q.matches("Lawmakers take action on pensions"));
        assert!(q.matches("New acts of defiance in the capital"));
        // Hong Kong's "euro" bond is not about the EU.
        assert!(
            !QueryMatcher::new("EU").matches("Hong Kong launches 4-year euro digital green bond")
        );
    }

    #[test]
    fn should_match_cjk_terms_across_punctuation_and_require_all_of_them() {
        let q = QueryMatcher::new("人工智能 对话");
        assert!(q.matches("北京称中美达成涉及300亿美元关税减让安排 将启动人工智能对话"));
        assert!(!q.matches("中国和美国如同“罐子里的蝎子” 人工智能正在搅动“罐子”迫使二者合作？"));
        let q = QueryMatcher::new("台风桦加沙");
        assert!(q.matches("超强台风“桦加沙”今日登陆广东"));
        assert!(!q.matches("台风季来临"));
        // CJK stop-words and one-character terms are not required.
        let q = QueryMatcher::new("最新 台风 的 消息");
        assert!(q.matches("台风“舒力基”强度将减弱"));
        // Mixed scripts: the Latin and CJK parts are separate terms.
        let q = QueryMatcher::new("AI法案");
        assert!(q.matches("欧盟AI法案本周生效"));
        assert!(!q.matches("欧盟法案本周生效"));
    }

    #[test]
    fn should_match_everything_when_the_query_has_no_terms() {
        assert!(QueryMatcher::new("a").is_empty());
        assert!(matches_query("a", "anything at all"));
        // Only stop-words: they are required then.
        let q = QueryMatcher::new("The Who");
        assert!(q.matches("The Who announce farewell tour"));
        assert!(!q.matches("The band announced a tour"));
    }
}
