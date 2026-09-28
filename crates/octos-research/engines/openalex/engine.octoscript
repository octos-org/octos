// OpenAlex works search.
// Written from https://help.openalex.org/api/searching/ and
// https://help.openalex.org/api/ (filters, paging, work fields).
use mod.net
use mod.std.array
use mod.std.math
use mod.std.object
use mod.std.text

fn build_request(query, opts) {
    let filters = []
    if opts.since != nil {
        array.push(filters, "from_publication_date:" + opts.since.date)
    }
    if array.len(opts.langs) > 0 {
        let codes = []
        for l in opts.langs {
            array.push(codes, l.primary)
        }
        array.push(filters, "language:" + text.join(codes, "|"))
    }
    // `select` returns only the fields read below.
    let fields = "id,doi,display_name,publication_date,language,primary_location,abstract_inverted_index"
    let params = [["search", query], ["per-page", math.min(opts.count, 100)], ["page", opts.page], ["select", fields]]
    if array.len(filters) > 0 {
        array.push(params, ["filter", text.join(filters, ",")])
    }
    let u = net.url({base: "https://api.openalex.org/works", query: params})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

// Abstracts come as an inverted index {word: [positions]}; rebuild the
// first words in order.
fn abstract_text(inv) {
    if inv == nil {
        return ""
    }
    let words = array.range(0, 80)
    let seen = 0
    for entry in object.entries(inv) {
        for p in entry[1] {
            if p < 80 {
                words[p] = entry[0]
                seen = math.max(seen, p + 1)
            }
        }
    }
    let out = []
    for i in array.range(0, seen) {
        if words[i] != i {
            array.push(out, words[i])
        }
    }
    return text.join(out, " ")
}

fn parse_response(response, opts) {
    let data = response.json
    let out = []
    for w in object.get(data, "results", []) {
        let loc = object.get(w, "primary_location", nil)
        let landing = if loc == nil {
            nil
        } else {
            object.get(loc, "landing_page_url", nil)
        }
        let src = if loc == nil {
            nil
        } else {
            object.get(loc, "source", nil)
        }
        let venue = if src == nil {
            nil
        } else {
            object.get(src, "display_name", nil)
        }
        let url = if landing != nil {
            landing
        } elif object.get(w, "doi", nil) != nil {
            w.doi
        } else {
            object.get(w, "id", "")
        }
        array.push(out, {
            url: url,
            title: object.get(w, "display_name", ""),
            snippet: abstract_text(object.get(w, "abstract_inverted_index", nil)),
            source: venue,
            lang: object.get(w, "language", nil),
            published: object.get(w, "publication_date", nil)
        })
    }
    return out
}
