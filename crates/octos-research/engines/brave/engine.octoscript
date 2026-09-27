// Brave Search API: web search, or news search for the news category.
// Written from https://api-dashboard.search.brave.com/app/documentation/web-search/query
// and .../news-search/query. The host adds X-Subscription-Token.
use mod.markup
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object

fn freshness(opts) {
    if opts.since == nil {
        return nil
    }
    let h = opts.since.hours
    if h <= 24 {
        return "pd"
    } elif h <= 168 {
        return "pw"
    } elif h <= 744 {
        return "pm"
    } elif h <= 8784 {
        return "py"
    }
    return opts.since.date + "to" + opts.now.date
}

fn build_request(query, opts) {
    let news = opts.category == "news"
    let path = if news {
        "news"
    } else {
        "web"
    }
    let max = if news {
        50
    } else {
        20
    }
    let params = [["q", query], ["count", math.min(opts.count, max)], ["offset", math.min(opts.page - 1, 9)]]
    if opts.lang != nil {
        array.push(params, ["search_lang", opts.lang.primary])
    }
    if opts.region != nil {
        array.push(params, ["country", opts.region])
    }
    let f = freshness(opts)
    if f != nil {
        array.push(params, ["freshness", f])
    }
    let u = net.url({base: "https://api.search.brave.com/res/v1/" + path + "/search", query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn item(r) {
    let profile = object.get(r, "profile", nil)
    let meta = object.get(r, "meta_url", nil)
    let source = if profile != nil {
        object.get(profile, "name", nil)
    } elif meta != nil {
        object.get(meta, "hostname", nil)
    } else {
        nil
    }
    return {
        url: object.get(r, "url", ""),
        title: markup.text({html: object.get(r, "title", "")}).text,
        snippet: markup.text({html: object.get(r, "description", "")}).text,
        source: source,
        lang: object.get(r, "language", nil),
        published: object.get(r, "page_age", nil)
    }
}

fn parse_response(response, opts) {
    let data = response.json
    let web = object.get(data, "web", nil)
    let results = if web != nil {
        object.get(web, "results", [])
    } else {
        object.get(data, "results", [])
    }
    let out = []
    for r in results {
        array.push(out, item(r))
    }
    return out
}
