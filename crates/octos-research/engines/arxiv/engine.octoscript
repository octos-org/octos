// arXiv API query interface (Atom 1.0 response).
// Written from https://info.arxiv.org/help/api/user-manual.html
use mod.markup
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn build_request(query, opts) {
    let terms = []
    for w in text.split(text.replace_all(query, "\"", " "), " ") {
        if w != "" {
            array.push(terms, "all:" + w)
        }
    }
    let sq = text.join(terms, " AND ")
    if opts.since != nil {
        // submittedDate:[YYYYMMDDTTTT TO YYYYMMDDTTTT], GMT.
        let from = text.slice(opts.since.compact, 0, 12)
        let to = text.slice(opts.now.compact, 0, 12)
        sq = "(" + sq + ") AND submittedDate:[" + from + " TO " + to + "]"
    }
    let recent = opts.since != nil || opts.category == "news"
    let sort = if recent {
        "submittedDate"
    } else {
        "relevance"
    }
    let params = [["search_query", sq], ["start", (opts.page - 1) * math.min(opts.count, 100)], ["max_results", math.min(opts.count, 100)], ["sortBy", sort], ["sortOrder", "descending"]]
    let u = net.url({base: "https://export.arxiv.org/api/query", query: params})
    return net.request({url: u.url, headers: {accept: "application/atom+xml"}})
}

fn parse_response(response, opts) {
    let feed = markup.feed({lang: "en"})
    let out = []
    for e in feed.entries {
        let authors = e.authors
        let by = if array.len(authors) > 3 {
            text.join(array.slice(authors, 0, 3), ", ") + " et al."
        } else {
            text.join(authors, ", ")
        }
        array.push(out, {
            url: e.url,
            title: e.title,
            snippet: e.snippet,
            source: "arXiv · " + by,
            lang: "en",
            published: e.published
        })
    }
    return out
}
