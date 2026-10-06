// Publishers' own RSS/Atom feeds, filtered by the query.
// Feeds cannot be searched, so this engine reads the feeds for the
// requested languages (each cached for 15 minutes by the core) and keeps
// the entries whose headline or summary match the query (the phrase, or
// every significant term; the core checks the same again, see
// `query_match` in the manifest).
// Written from each publisher's feed and its RSS/terms pages (docs_url in
// the manifest); publishers that forbid AI or automated use are left out.
use mod.markup
use mod.net
use mod.std.array
use mod.std.object
use mod.std.text

fn feeds() {
    let f = []
    array.push(f, {lang: "en", source: "NPR", url: "https://feeds.npr.org/1001/rss.xml"})
    array.push(f, {lang: "en", source: "France 24", url: "https://www.france24.com/en/rss"})
    array.push(f, {lang: "en", source: "CNA", url: "https://www.channelnewsasia.com/api/v1/rss-outbound-feed?_format=xml"})
    array.push(f, {lang: "zh-CN", source: "RFI 中文", url: "https://www.rfi.fr/cn/rss"})
    array.push(f, {lang: "zh-CN", source: "自由亚洲电台", url: "https://www.rfa.org/arc/outboundfeeds/mandarin/rss/"})
    array.push(f, {lang: "zh-TW", source: "中央通訊社", url: "https://feeds.feedburner.com/rsscna/intworld"})
    array.push(f, {lang: "zh-TW", source: "中央通訊社", url: "https://feeds.feedburner.com/rsscna/mainland"})
    array.push(f, {lang: "zh-TW", source: "中央通訊社", url: "https://feeds.feedburner.com/rsscna/technology"})
    array.push(f, {lang: "zh-TW", source: "端傳媒", url: "https://theinitium.com/rss/"})
    return f
}

fn primary(tag) {
    let i = text.index_of(tag, "-")
    if i < 0 {
        return text.lower(tag)
    }
    return text.lower(text.slice(tag, 0, i))
}

fn build_request(query, opts) {
    let wanted = []
    for l in opts.langs {
        array.push(wanted, l.primary)
    }
    if array.len(wanted) == 0 {
        wanted = ["en"]
    }
    let out = []
    for f in feeds() {
        if array.contains(wanted, primary(f.lang)) {
            array.push(out, net.request({url: f.url, headers: {accept: "application/rss+xml, application/atom+xml, application/xml"}}))
        }
    }
    return out
}

fn parse_response(response, opts) {
    let feed = nil
    for f in feeds() {
        if f.url == response.url {
            feed = f
        }
    }
    if feed == nil {
        return []
    }
    let out = []
    for e in markup.feed({lang: feed.lang}).entries {
        let title = object.get(e, "title", "")
        let snippet = object.get(e, "snippet", "")
        // The host's matcher: the query as a phrase, or every significant
        // term as a whole word (CJK terms anywhere in the CJK text).
        // Stop-words and one-letter terms are not significant, and some
        // terms are never enough ("EU AI Act" is not "terrorist act").
        if !markup.matches({query: opts.query, text: title + " " + snippet}).matched {
            continue
        }
        array.push(out, {
            url: e.url,
            title: title,
            snippet: snippet,
            source: feed.source,
            lang: feed.lang,
            published: object.get(e, "published", nil)
        })
    }
    return out
}
