//! Integration-style tests exercising the public library surface end-to-end
//! with canned (mock) Synology responses — no live NAS required.

use nas::{
    download_body_is_error, format_listing, parse_file_list, parse_login, parse_search_list,
    parse_search_start, parse_share_list, text_from_bytes,
};

#[test]
fn login_then_list_share_flow_parses() {
    let login = r#"{"success":true,"data":{"sid":"xJoGn-S-K_Cx0zF"}}"#;
    let sid = parse_login(login).expect("login parses");
    assert_eq!(sid, "xJoGn-S-K_Cx0zF");

    let shares = r#"{"success":true,"data":{"total":2,"offset":0,"shares":[
        {"isdir":true,"name":"video","path":"/video"},
        {"isdir":true,"name":"photo","path":"/photo"}]}}"#;
    let entries = parse_share_list(shares).expect("shares parse");
    let listing = format_listing(&entries, "Shared folders:");
    assert!(listing.contains("[DIR]  video"));
    assert!(listing.contains("[DIR]  photo"));
}

#[test]
fn folder_list_with_metadata_renders_sizes() {
    let body = r#"{"success":true,"data":{"total":1,"offset":0,"files":[
        {"path":"/video/clip.mp4","name":"clip.mp4","isdir":false,
         "additional":{"size":1048576,"time":{"mtime":1369964408},"type":"MP4"}}]}}"#;
    let files = parse_file_list(body).expect("files parse");
    let listing = format_listing(&files, "Listing of /video:");
    assert!(listing.contains("clip.mp4"));
    assert!(listing.contains("1.0 MB"));
}

#[test]
fn session_failure_maps_to_friendly_error() {
    let body = r#"{"success":false,"error":{"code":106}}"#;
    let err = parse_share_list(body).unwrap_err();
    assert!(err.contains("106"));
    assert!(err.contains("timeout"));
}

#[test]
fn search_two_phase_flow_parses() {
    let start = r#"{"success":true,"data":{"taskid":"51CE617CF57B24E5"}}"#;
    let taskid = parse_search_start(start).expect("start parses");
    assert_eq!(taskid, "51CE617CF57B24E5");

    let running = r#"{"success":true,"data":{"finished":false,"files":[]}}"#;
    let (_files, finished) = parse_search_list(running).unwrap();
    assert!(!finished);

    let done = r#"{"success":true,"data":{"finished":true,"files":[
        {"path":"/video/report.txt","name":"report.txt","isdir":false}]}}"#;
    let (files, finished) = parse_search_list(done).unwrap();
    assert!(finished);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "/video/report.txt");
}

#[test]
fn download_text_vs_binary_vs_error() {
    // valid text
    assert_eq!(text_from_bytes(b"plain text").unwrap(), "plain text");
    // binary
    assert!(text_from_bytes(&[0x00, 0x01]).is_err());
    // JSON error body in place of file bytes
    let err = download_body_is_error(
        Some("application/json"),
        br#"{"success":false,"error":{"code":408}}"#,
    )
    .expect("error detected");
    assert!(err.contains("408"));
}
