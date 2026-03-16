use std::time::Duration;

#[tokio::test]
#[ignore] // Network-dependent: fetches live RSS/API data from external services
async fn test_new_sources() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0")
        .build()
        .unwrap();

    let feeds = &[
        (
            "Substack (Pragmatic Engineer)",
            "https://newsletter.pragmaticengineer.com/feed",
        ),
        (
            "Substack (One Useful Thing)",
            "https://www.oneusefulthing.org/feed",
        ),
        (
            "Medium (technology)",
            "https://medium.com/feed/tag/technology",
        ),
    ];

    for (name, url) in feeds {
        print!("{name}: ");
        match client.get(*url).send().await {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                // Count items
                let items = body.matches("<item>").count() + body.matches("<entry>").count();
                println!("{status} — {} bytes, {items} items", body.len());
                // Show first 2 titles
                let mut count = 0;
                let split = if body.contains("<item>") {
                    "<item>"
                } else {
                    "<entry>"
                };
                for chunk in body.split(split).skip(1) {
                    if let Some(start) = chunk.find("<title>") {
                        let s = start + 7;
                        if let Some(end) = chunk[s..].find("</title>") {
                            let title = &chunk[s..s + end];
                            println!("  - {title}");
                            count += 1;
                            if count >= 3 {
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => println!("FAIL: {e}"),
        }
    }

    // Test deep fetch on a known article
    println!("\n=== Deep fetch test (HN article) ===");
    let resp = client
        .get("https://blog.cloudflare.com/a-better-web-streams-api/")
        .send()
        .await
        .unwrap();
    let html = resp.text().await.unwrap();
    let text = htmd::convert(&html).unwrap_or_default();
    println!("Cloudflare blog: {} chars of markdown", text.len());
    let preview: String = text.chars().take(500).collect();
    println!("Preview: {preview}...");
}
