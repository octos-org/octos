//! Opt-in S5 reproduction: append cost must not grow with the active JSONL size.
//! Run with `cargo test -p octos-bus --test session_seq_bench -- --ignored --nocapture`.

use std::time::Instant;

use octos_bus::session::SessionHandle;
use octos_core::{Message, MessageRole, SessionKey};

fn message(index: usize) -> Message {
    Message {
        role: MessageRole::User,
        content: format!("message-{index}:{}", "x".repeat(2048)),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
        source: None,
        participant: None,
    }
}

#[cfg(target_os = "linux")]
fn read_rchar() -> Option<u64> {
    std::fs::read_to_string("/proc/self/io")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("rchar: ")?.parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn read_rchar() -> Option<u64> {
    None
}

#[tokio::test]
#[ignore = "opt-in performance reproduction; not a timing assertion in CI"]
async fn append_seq_scaling() {
    for messages in [128usize, 256, 512] {
        let tmp = tempfile::tempdir().unwrap();
        let key = SessionKey::new("cli", &format!("s5-bench-{messages}"));
        let mut handle = SessionHandle::open(tmp.path(), &key);
        let started = Instant::now();
        let rchar_before = read_rchar();
        for index in 0..messages {
            let seq = handle.add_message_with_seq(message(index)).await.unwrap();
            assert_eq!(seq, index);
        }
        let wall = started.elapsed();
        let rchar_delta = match (read_rchar(), rchar_before) {
            (Some(after), Some(before)) => after.saturating_sub(before),
            _ => 0,
        };
        println!(
            "{{\"messages\":{messages},\"wall_ms\":{},\"rchar_mib\":{:.3}}}",
            wall.as_secs_f64() * 1000.0,
            rchar_delta as f64 / (1024.0 * 1024.0),
        );
        let reloaded = SessionHandle::open_full(tmp.path(), &key);
        assert_eq!(reloaded.session().messages.len(), messages);
    }
}
