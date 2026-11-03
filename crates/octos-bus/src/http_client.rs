//! Shared HTTP client construction for channel integrations.
//!
//! Every channel previously built bare `reqwest::Client::new()` instances with
//! no connect/read/total deadline, so one half-open connection (e.g. Slack)
//! could park an outbound send forever — and because the outbound dispatcher
//! is serial, that stalls every channel (R-14). All channel HTTP now goes
//! through the values here; bump them deliberately, not ad hoc.

// Consumers are the feature-gated channel modules; in a no-channel build this
// module is only exercised by its tests.
#![allow(dead_code)]

use std::time::Duration;

use reqwest::Client;

/// TCP connect deadline: a reachable host answers in milliseconds; 10s
/// tolerates genuinely bad networks without masking dead ones.
pub(crate) const CHANNEL_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle-read deadline: no response bytes for this long means the peer is
/// stalled, not slow.
pub(crate) const CHANNEL_HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Whole-request deadline for channel API calls (postMessage-style JSON
/// endpoints and media uploads/downloads). Two minutes covers large media on
/// slow links while guaranteeing an end to a stuck request.
pub(crate) const CHANNEL_HTTP_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Build the standard channel HTTP client with connect/read/total deadlines.
pub(crate) fn channel_http_client() -> Client {
    Client::builder()
        .connect_timeout(CHANNEL_HTTP_CONNECT_TIMEOUT)
        .read_timeout(CHANNEL_HTTP_READ_TIMEOUT)
        .timeout(CHANNEL_HTTP_TOTAL_TIMEOUT)
        .build()
        .expect("failed to build channel HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared client must enforce its total deadline against a peer that
    /// accepts the connection and then never sends a byte.
    #[tokio::test(start_paused = true)]
    async fn channel_http_client_times_out_stalled_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let client = channel_http_client();
        let url = format!("http://{addr}/hang");
        let fut = client.get(&url).send();
        tokio::pin!(fut);
        let advance = tokio::spawn(tokio::time::advance(CHANNEL_HTTP_TOTAL_TIMEOUT * 2));
        let result = tokio::time::timeout(CHANNEL_HTTP_TOTAL_TIMEOUT * 2, &mut fut).await;
        advance.await.unwrap();
        server.abort();

        let err = result
            .expect("request must produce an outcome before twice the total deadline")
            .expect_err("a stalled peer must fail, not hang");
        assert!(err.is_timeout(), "expected a timeout error, got: {err}");
    }
}
