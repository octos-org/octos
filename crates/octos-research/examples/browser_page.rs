//! Load a page in the person's browser (the octos Chrome profile).
//!
//! ```text
//! cargo run -p octos-research --features browser --example browser_page -- <url>          # print the rendered HTML
//! cargo run -p octos-research --features browser --example browser_page -- --open <url>   # show it and wait for Enter
//! ```
//!
//! `--open` is how a person signs in to the profile or clears a challenge
//! by hand: the page stays open until Enter is pressed.

use std::time::Duration;

use octos_research::browser::PersonBrowser;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (open, url) = match args.as_slice() {
        [flag, url] if flag == "--open" => (true, url.clone()),
        [url] => (false, url.clone()),
        _ => {
            eprintln!("usage: browser_page [--open] <url>");
            std::process::exit(2);
        }
    };
    let Some(browser) = PersonBrowser::from_env() else {
        eprintln!("the person's browser is off (OCTOS_BROWSER) or has no profile location");
        std::process::exit(1);
    };
    if open {
        match browser.show(&url).await {
            Ok(true) => {
                eprintln!("open in the octos browser; press Enter when done");
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                browser.close().await;
            }
            Ok(false) => eprintln!("no window to show it in ({:?} mode)", browser.mode()),
            Err(e) => eprintln!("{e}"),
        }
        return;
    }
    let rendered = browser.render(&url, Duration::from_secs(30)).await;
    browser.close().await;
    match rendered {
        Ok(r) => {
            eprintln!(
                "final url: {}",
                r.header("x-octos-final-url").unwrap_or("?")
            );
            println!("{}", r.body);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
