// GDELT DOC 2.0 API, ArtList mode, JSON output.
// Written from https://blog.gdeltproject.org/gdelt-doc-2-0-api-debuts/
// Languages become `sourcelang:` operators; several are ORed in one
// parenthesised group, so one request covers every requested language.
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn build_request(query, opts) {
    let q = query
    let names = []
    for l in opts.langs {
        if l.name != nil {
            array.push(names, "sourcelang:" + l.name)
        }
    }
    if array.len(names) == 1 {
        q = q + " " + names[0]
    } elif array.len(names) > 1 {
        q = q + " (" + text.join(names, " OR ") + ")"
    }
    let count = math.min(opts.count * math.max(array.len(names), 1), 250)
    let params = [["query", q], ["mode", "ArtList"], ["format", "json"], ["maxrecords", count], ["sort", "DateDesc"]]
    if opts.since != nil {
        if opts.since.relative {
            if opts.since.hours < 72 {
                array.push(params, ["timespan", opts.since.hours + "h"])
            } else {
                array.push(params, ["timespan", math.min(math.floor(opts.since.hours / 24), 90) + "d"])
            }
        } elif opts.since.hours < 2160 {
            array.push(params, ["startdatetime", opts.since.compact])
        }
    }
    let u = net.url({base: "https://api.gdeltproject.org/api/v2/doc/doc", query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn parse_response(response, opts) {
    let data = response.json
    if data == nil {
        // Errors and rate-limit notices come back as plain text.
        let t = text.trim(response.body)
        if t == "" {
            return []
        }
        return {error: "GDELT: " + text.slice(t, 0, math.min(text.len(t), 160)), backoff: 60}
    }
    let out = []
    for a in object.get(data, "articles", []) {
        let url = object.get(a, "url", "")
        if url == "" {
            continue
        }
        array.push(out, {
            url: url,
            title: object.get(a, "title", ""),
            source: object.get(a, "domain", nil),
            lang: object.get(a, "language", nil),
            published: object.get(a, "seendate", nil)
        })
    }
    return out
}
