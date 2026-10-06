// Brave Search's results page (search.brave.com/search), key-less.
// Written from the page itself and Brave's search-operators help page
// (docs_url): `q` is the query, `source=web`, `tf` the time filter
// (pd, pw, pm, py). Each web result is a `div.snippet[data-type="web"]`
// whose first link is the destination. A challenge page is reported, not
// worked around.
use mod.markup
use mod.net
use mod.std.array
use mod.std.text

fn build_request(query, opts) {
    let params = [["q", query], ["source", "web"]]
    if opts.since != nil {
        let h = opts.since.hours
        let tf = if h <= 24 {
            "pd"
        } elif h <= 168 {
            "pw"
        } elif h <= 744 {
            "pm"
        } else {
            "py"
        }
        array.push(params, ["tf", tf])
    }
    let u = net.url({base: "https://search.brave.com/search", query: params})
    return net.request({url: u.url, headers: {accept: "text/html"}})
}

fn parse_response(response, opts) {
    let body = response.body
    let found = markup.select({item: "div.snippet[data-type=\"web\"]", fields: {title: ".search-snippet-title", url: "a@href", snippet: ".generic-snippet .content", site: ".site-name-content .desktop-small-semibold"}})
    if array.len(found.items) == 0 && text.contains(body, "pow-captcha") {
        return {challenge: "Brave Search asked to confirm a person is searching"}
    }
    let out = []
    for r in found.items {
        if r.url == nil || r.title == nil || !text.starts_with(r.url, "http") {
            continue
        }
        array.push(out, {url: r.url, title: r.title, snippet: r.snippet, source: r.site})
    }
    return out
}
