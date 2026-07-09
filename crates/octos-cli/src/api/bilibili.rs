use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use reqwest::header::{ACCEPT, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};

use super::AppState;

const BILIBILI_SEARCH_API: &str = "https://api.bilibili.com/x/web-interface/search/type";
const BILIBILI_REFERER: &str = "https://search.bilibili.com/";
const BILIBILI_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/125.0 Safari/537.36";

#[derive(Debug, Deserialize)]
pub struct FirstVideoQuery {
    keyword: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FirstVideoResponse {
    pub title: String,
    pub url: String,
    pub bvid: String,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    code: i64,
    data: Option<SearchData>,
}

#[derive(Debug, Deserialize)]
struct SearchData {
    #[serde(default)]
    result: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    title: Option<String>,
    bvid: Option<String>,
    arcurl: Option<String>,
}

pub async fn first_video(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FirstVideoQuery>,
) -> Result<Json<FirstVideoResponse>, (StatusCode, String)> {
    let keyword = query.keyword.trim();
    if keyword.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "keyword is required".into()));
    }

    let response = state
        .http_client
        .get(BILIBILI_SEARCH_API)
        .header(USER_AGENT, BILIBILI_USER_AGENT)
        .header(REFERER, BILIBILI_REFERER)
        .header(ACCEPT, "application/json,text/plain,*/*")
        .query(&[
            ("search_type", "video"),
            ("keyword", keyword),
            ("page", "1"),
            ("pagesize", "1"),
        ])
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    if !response.status().is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("bilibili search returned HTTP {}", response.status()),
        ));
    }

    let body = response
        .json::<SearchResponse>()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    first_result_from_search_response(body)
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no bilibili video result".into()))
}

fn first_result_from_search_response(body: SearchResponse) -> Option<FirstVideoResponse> {
    if body.code != 0 {
        return None;
    }

    body.data?
        .result
        .into_iter()
        .filter_map(|item| {
            let bvid = item.bvid?.trim().to_string();
            if bvid.is_empty() {
                return None;
            }
            let title = clean_bilibili_title(item.title.as_deref().unwrap_or(&bvid));
            let url = item
                .arcurl
                .filter(|url| url.starts_with("https://www.bilibili.com/video/"))
                .unwrap_or_else(|| format!("https://www.bilibili.com/video/{bvid}/"));
            Some(FirstVideoResponse { title, url, bvid })
        })
        .next()
}

fn clean_bilibili_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut in_tag = false;
    for ch in title.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_video_result() {
        let result = first_result_from_search_response(SearchResponse {
            code: 0,
            data: Some(SearchData {
                result: vec![SearchResult {
                    title: Some("40分钟<em class=\"keyword\">做饭</em>神曲合集".into()),
                    bvid: Some("BV1cTcbzNE9p".into()),
                    arcurl: None,
                }],
            }),
        });

        assert_eq!(
            result,
            Some(FirstVideoResponse {
                title: "40分钟做饭神曲合集".into(),
                url: "https://www.bilibili.com/video/BV1cTcbzNE9p/".into(),
                bvid: "BV1cTcbzNE9p".into(),
            }),
        );
    }

    #[test]
    fn rejects_non_success_search_response() {
        let result = first_result_from_search_response(SearchResponse {
            code: -412,
            data: None,
        });

        assert_eq!(result, None);
    }
}
