//! Push → pull round-trip smoke test against the live backend.
//!
//! Run with: `cargo test -p xai-grok-shell -- pull_smoke --ignored --nocapture`

#[cfg(test)]
mod tests {
    use crate::auth::GrokAuth;
    use crate::remote::client::BackendClient;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn load_prod_auth() -> Option<GrokAuth> {
        let path = crate::util::grok_home::grok_home().join("auth.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        let store: BTreeMap<String, GrokAuth> = serde_json::from_str(&contents).ok()?;
        let scope = crate::auth::GrokComConfig::default().auth_scope();
        crate::auth::lookup_auth(&store, &scope)
    }

    /// Full round-trip using the real RemoteSync production code path:
    /// create RemoteSync → queue ACP notifications → flush → verify on
    /// backend → pull back → verify local hydration + storage adapter load.
    #[tokio::test]
    #[ignore]
    async fn smoke_push_pull_round_trip() {
        use crate::remote::sync::RemoteSync;
        use crate::session::export::ExportedMetadata;
        use agent_client_protocol::{
            ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
        };

        let auth = load_prod_auth().expect("No auth.json — run `grok login`");
        let am = Arc::new(crate::auth::AuthManager::new(
            &crate::util::grok_home::grok_home(),
            crate::auth::GrokComConfig::default(),
        ));
        am.hot_swap(auth);
        let client = BackendClient::new().with_auth_manager(am.clone());

        let session_id = format!("test-rt-{}", uuid::Uuid::new_v4());
        let test_cwd = "/tmp/smoke-test".to_string();
        let test_title = "Push-Pull Round Trip Test";

        // PUSH via RemoteSync (real production path)
        let metadata = ExportedMetadata {
            title: Some(test_title.into()),
            cwd: test_cwd.clone(),
            model_id: Some("grok-3".into()),
            catalog_identity: None,
            agent_name: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            total_messages: None,
            parent_session_id: None,
            session_kind: None,
            subagent_type: None,
            subagent_persona: None,
            subagent_role: None,
            fork_context_source: None,
            subagent_depth: None,
            title_is_manual: None,
        };

        let sync = RemoteSync::new(
            session_id.clone(),
            metadata,
            BackendClient::new().with_auth_manager(am.clone()),
        );

        let sid = agent_client_protocol::SessionId::new(Arc::from(session_id.as_str()));
        sync.queue(SessionNotification::new(
            sid.clone(),
            SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("Hello from smoke test — user".to_string()),
            ))),
        ));
        sync.queue(SessionNotification::new(
            sid.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("Hello from smoke test — agent".to_string()),
            ))),
        ));
        sync.flush();
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        // Verify backend has cwd, title, messages
        let loaded = client
            .load_session_data(&session_id)
            .await
            .expect("load after push failed");
        let remote = loaded.session.as_ref().expect("no session row");
        assert_eq!(remote.cwd.as_deref(), Some(test_cwd.as_str()));
        assert_eq!(remote.title.as_deref(), Some(test_title));
        assert!(loaded.messages.as_ref().map_or(0, |m| m.len()) >= 2);

        // PULL back through the production load-on-miss path. This path fetches
        // without filesystem I/O, hydrates behind the canonical exclusive
        // publication claim, then hands the actor a lifetime shared lease.
        let requested = crate::session::info::Info {
            id: sid,
            cwd: test_cwd.clone(),
        };
        let sampling_client =
            crate::sampling::Client::new(xai_grok_sampler::SamplerConfig::default())
                .expect("sampling client");
        let (persisted, persistence) = crate::session::persistence::load_light(
            &requested,
            sampling_client,
            crate::config::StorageMode::Local,
            None,
            Some(&client),
            None,
            None,
            String::new(),
            None,
        )
        .await
        .expect("pull through load-on-miss failed");
        let pulled = persisted.summary.info.clone();
        assert_eq!(pulled.cwd, test_cwd);

        // Verify local storage loads
        let local_dir = crate::session::persistence::session_dir(&pulled);
        assert!(local_dir.join("summary.json").exists());
        assert!(local_dir.join("updates.jsonl").exists());

        assert_eq!(persisted.summary.session_summary, test_title);

        // Verify chat_history has both turns
        let chat =
            std::fs::read_to_string(local_dir.join("chat_history.jsonl")).unwrap_or_default();
        assert!(chat.contains("user"), "chat_history missing user turn");
        assert!(chat.contains("agent"), "chat_history missing agent turn");

        // Cleanup
        drop(persistence);
        drop(sync);
        let _ = client.delete_session_data(&session_id).await;
        let _ = std::fs::remove_dir_all(&local_dir);
    }
}
