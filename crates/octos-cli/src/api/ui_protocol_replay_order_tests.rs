use super::*;

#[tokio::test]
async fn send_file_delivery_stays_in_order_in_the_main_transcript() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("ui_protocol_replay_order_fixture.json")).unwrap();
    let tenant = tempfile::tempdir().unwrap();
    let workspace = tenant.path().join("work");
    let store_dir = workspace.join(".octos").join("coding");
    std::fs::create_dir_all(&store_dir).unwrap();
    let sessions = Arc::new(TokioMutex::new(
        octos_bus::SessionManager::open(&store_dir).unwrap(),
    ));
    let key = SessionKey(fixture["session_id"].as_str().unwrap().to_owned());
    let thread_id = fixture["thread_id"].as_str().unwrap().to_owned();
    let main_path = sessions.lock().await.session_path(&key);
    let sidecar_path = store_dir
        .join("users")
        .join(octos_bus::session::encode_path_component(key.base_key()))
        .join("sessions")
        .join("default.jsonl");

    let mut prompt = Message::user(fixture["events"][0]["text"].as_str().unwrap());
    prompt.thread_id = Some(thread_id.clone());
    let prompt_seq = sessions
        .lock()
        .await
        .add_message_with_seq(&key, prompt)
        .await
        .unwrap();
    assert_eq!(prompt_seq, 0);

    let media_name = fixture["events"][1]["media_name"].as_str().unwrap();
    let media = workspace.join(media_name);
    std::fs::write(
        &media,
        fixture["events"][1]["file_content"].as_str().unwrap(),
    )
    .unwrap();
    let sent = octos_core::OutboundMessage {
        channel: "api".to_owned(),
        chat_id: key.0.clone(),
        content: fixture["events"][1]["content"].as_str().unwrap().to_owned(),
        reply_to: None,
        media: vec![media.to_string_lossy().into_owned()],
        metadata: serde_json::json!({}),
    };
    let (_, delivery_seq) = persist_send_file_delivery(
        &sessions,
        tenant.path(),
        &key,
        &thread_id,
        Default::default(),
        sent,
    )
    .await
    .expect("delivery must persist");
    assert_eq!(delivery_seq, 1, "delivery follows the existing prompt");

    let final_response = Message::assistant_with_thread(
        fixture["events"][2]["text"].as_str().unwrap().to_owned(),
        octos_core::ThreadId::new(thread_id),
    );
    let final_seq = sessions
        .lock()
        .await
        .add_message_with_seq(&key, final_response)
        .await
        .unwrap();
    assert_eq!(final_seq, 2);

    assert!(main_path.is_file());
    assert!(
        !sidecar_path.exists(),
        "delivery must not create a seq-0 side transcript"
    );
    let reread = octos_bus::SessionManager::open(&store_dir)
        .unwrap()
        .load(&key)
        .await
        .expect("main transcript exists");
    assert_eq!(
        reread
            .messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Assistant
        ]
    );
    assert_eq!(reread.messages[1].media.len(), 1);
    assert_eq!(reread.messages[2].content, "RECOVERED");
}
