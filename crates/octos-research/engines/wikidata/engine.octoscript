// Wikidata entity search (action=wbsearchentities).
// Written from https://www.wikidata.org/w/api.php?action=help&modules=wbsearchentities
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn lang_code(opts) {
    if opts.lang == nil {
        return "en"
    }
    return opts.lang.primary
}

fn build_request(query, opts) {
    let l = lang_code(opts)
    let params = [["action", "wbsearchentities"], ["search", query], ["language", l], ["uselang", l], ["type", "item"], ["limit", math.min(opts.count, 50)], ["continue", (opts.page - 1) * math.min(opts.count, 50)], ["format", "json"]]
    let u = net.url({base: "https://www.wikidata.org/w/api.php", query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn parse_response(response, opts) {
    let data = response.json
    let out = []
    for r in object.get(data, "search", []) {
        let url = object.get(r, "url", "")
        if text.starts_with(url, "//") {
            url = "https:" + url
        }
        if url == "" {
            url = object.get(r, "concepturi", "")
        }
        let label = object.get(r, "label", object.get(r, "id", ""))
        array.push(out, {
            url: url,
            title: label + " (" + object.get(r, "id", "") + ")",
            snippet: object.get(r, "description", ""),
            source: "Wikidata",
            lang: lang_code(opts)
        })
    }
    return out
}
