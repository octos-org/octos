// Stack Exchange API 2.3: /search/advanced.
// Written from https://api.stackexchange.com/docs/advanced-search and
// https://api.stackexchange.com/docs/throttle (the `backoff` field).
use mod.markup
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn build_request(query, opts) {
    let params = [["q", query], ["site", opts.settings.site], ["pagesize", math.min(opts.count, 100)], ["page", opts.page], ["order", "desc"], ["sort", "relevance"]]
    if opts.since != nil {
        array.push(params, ["fromdate", opts.since.unix])
    }
    let u = net.url({base: "https://api.stackexchange.com/2.3/search/advanced", query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn parse_response(response, opts) {
    let data = response.json
    let backoff = object.get(data, "backoff", nil)
    let err = object.get(data, "error_message", nil)
    if err != nil {
        return {error: "Stack Exchange: " + err, backoff: backoff}
    }
    let out = []
    for q in object.get(data, "items", []) {
        let owner = object.get(q, "owner", {})
        let answered = if object.get(q, "is_answered", false) {
            "answered"
        } else {
            "unanswered"
        }
        let tags = text.join(object.get(q, "tags", []), ", ")
        array.push(out, {
            url: object.get(q, "link", ""),
            title: markup.text({html: object.get(q, "title", "")}).text,
            snippet: "Score " + object.get(q, "score", 0) + ", " + answered + ". Tags: " + tags,
            source: "Stack Overflow · " + markup.text({html: object.get(owner, "display_name", "")}).text,
            lang: "en",
            published: object.get(q, "creation_date", nil)
        })
    }
    return {items: out, backoff: backoff}
}
