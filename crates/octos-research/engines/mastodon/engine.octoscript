// Mastodon public hashtag timeline: GET /api/v1/timelines/tag/:hashtag.
// Written from https://docs.joinmastodon.org/methods/timelines/#tag
// Query words become hashtags: the first is the path tag, up to three more
// go in `any[]`, so posts tagged with any of them match.
use mod.markup
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn tags(query) {
    let q = text.lower(query)
    for p in [".", ",", "#", "!", "?", "'", "\"", ":", ";", "(", ")", "/", "&", "+", "，", "。", "、"] {
        q = text.replace_all(q, p, " ")
    }
    let out = []
    for w in text.split(q, " ") {
        if w != "" && array.len(out) < 4 && !array.contains(out, w) {
            array.push(out, w)
        }
    }
    return out
}

fn build_request(query, opts) {
    let t = tags(query)
    if array.len(t) == 0 {
        t = ["news"]
    }
    let params = [["limit", math.min(opts.count, 40)]]
    for extra in array.slice(t, 1, array.len(t)) {
        array.push(params, ["any[]", extra])
    }
    let u = net.url({base: "https://" + opts.settings.instance + "/api/v1/timelines/tag/" + t[0], query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn parse_response(response, opts) {
    let out = []
    for s in response.json {
        // Skip posts their authors marked sensitive or put behind a content
        // warning (Status.sensitive, Status.spoiler_text).
        if object.get(s, "sensitive", false) || object.get(s, "spoiler_text", "") != "" {
            continue
        }
        let content = markup.text({html: object.get(s, "content", "")}).text
        if content == "" {
            continue
        }
        let url = object.get(s, "url", nil)
        if url == nil {
            url = object.get(s, "uri", "")
        }
        let account = object.get(s, "account", {})
        let title = if text.len(content) > 120 {
            text.slice(content, 0, 117) + "..."
        } else {
            content
        }
        array.push(out, {
            url: url,
            title: title,
            snippet: content,
            source: "@" + object.get(account, "acct", "") + " on " + opts.settings.instance,
            lang: object.get(s, "language", nil),
            published: object.get(s, "created_at", nil)
        })
    }
    return out
}
