// GitHub REST API: search repositories.
// Written from https://docs.github.com/en/rest/search/search#search-repositories
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn build_request(query, opts) {
    let q = query
    if opts.since != nil {
        q = q + " pushed:>" + opts.since.date
    }
    let params = [["q", q], ["per_page", math.min(opts.count, 100)], ["page", opts.page]]
    let u = net.url({base: "https://api.github.com/search/repositories", query: params})
    return net.request({url: u.url, headers: {accept: "application/vnd.github+json", x_github_api_version: "2022-11-28"}})
}

fn parse_response(response, opts) {
    let data = response.json
    let out = []
    for r in object.get(data, "items", []) {
        let lang = object.get(r, "language", nil)
        let stars = object.get(r, "stargazers_count", 0)
        let src = if lang == nil {
            "GitHub · " + stars + " stars"
        } else {
            "GitHub · " + lang + " · " + stars + " stars"
        }
        array.push(out, {
            url: object.get(r, "html_url", ""),
            title: object.get(r, "full_name", ""),
            snippet: object.get(r, "description", ""),
            source: src,
            published: object.get(r, "pushed_at", nil)
        })
    }
    return out
}
