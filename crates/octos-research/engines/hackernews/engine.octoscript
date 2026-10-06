// Hacker News Search API (Algolia).
// Written from https://hn.algolia.com/api
use mod.markup
use mod.net
use mod.std.array
use mod.std.object
use mod.std.text

fn build_request(query, opts) {
    // Relevance ranking; `since` becomes a creation-time filter (newest-first
    // ordering let loosely matching stories crowd out relevant ones).
    let params = [["query", query], ["tags", "story"], ["hitsPerPage", opts.count], ["page", opts.page - 1]]
    if opts.since != nil {
        array.push(params, ["numericFilters", "created_at_i>" + opts.since.unix])
    }
    let u = net.url({base: "https://hn.algolia.com/api/v1/search", query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn parse_response(response, opts) {
    let data = response.json
    let out = []
    for h in object.get(data, "hits", []) {
        let id = object.get(h, "objectID", "")
        let discussion = "https://news.ycombinator.com/item?id=" + id
        // A story links to an article; a text post (Ask HN, …) is only
        // its discussion thread.
        let url = object.get(h, "url", nil)
        let kind = "article"
        if url == nil || url == "" {
            url = discussion
            kind = "post"
        }
        let story = object.get(h, "story_text", nil)
        let snippet = if story == nil {
            ""
        } else {
            markup.text({html: story}).text
        }
        let meta = object.get(h, "points", 0) + " points, " + object.get(h, "num_comments", 0) + " comments: " + discussion
        array.push(out, {
            url: url,
            title: object.get(h, "title", ""),
            snippet: if snippet == "" {
                meta
            } else {
                snippet + " (" + meta + ")"
            },
            source: "Hacker News",
            lang: "en",
            published: object.get(h, "created_at", nil),
            kind: kind
        })
    }
    return out
}
