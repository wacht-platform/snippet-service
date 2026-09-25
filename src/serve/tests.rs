use super::*;
    use super::*;
    use tempfile::tempdir;

    fn test_daemon() -> Daemon {
        Daemon {
            config: std::sync::Mutex::new(SnippetConfig::default()),
            config_path: PathBuf::new(),
            token: String::new(),
            hostname: String::from("test"),
            sessions: Mutex::new(HashMap::new()),
            shells: crate::term::SessionTerms::new(global_shell_cwd()),
            git_write: Mutex::new(()),
            browser: BrowserManager::default(),
            seen_nonces: std::sync::Mutex::new(HashMap::new()),
            queue_hidden: std::sync::Mutex::new(HashMap::new()),
            queue_revision: AtomicU64::new(0),
            mission_control_root: tempfile::tempdir().expect("temporary directory").keep(),
            store: crate::store::Store::open_in_memory().unwrap(),
            coordination_events: tokio::sync::broadcast::channel(256).0,
            recurring_root: tempfile::tempdir().expect("temporary directory").keep(),
        }
    }

    #[test]
    fn session_event_page_returns_bounded_backward_cursor() {
        let events = vec![
            crate::harness::HarnessEvent::UserInput { text: "one".into() },
            crate::harness::HarnessEvent::AssistantText { text: "two".into() },
            crate::harness::HarnessEvent::UserInput {
                text: "three".into(),
            },
        ];
        assert_eq!(session_event_page(&events, None, None), (0, 3, false));
        assert_eq!(session_event_page(&events, Some(3), Some(2)), (1, 3, true));
        assert_eq!(
            session_event_page(&events, Some(1), Some(50)),
            (0, 1, false)
        );
        assert_eq!(
            session_event_page(&events, Some(999), Some(0)),
            (2, 3, true)
        );
    }

    /// A nonce sidecar is ignored: the store is the only record now.
    ///
    /// Retirement guard. The fallback used to read a session's `.nonces.json`
    /// and treat anything in it as already-sent. If that read ever came back,
    /// a stale file would silently suppress a legitimate request — the failure
    /// would look like "my message vanished", with nothing in the logs.
    #[test]
    fn a_legacy_nonces_file_is_ignored() {
        let dir = tempdir().expect("temporary directory");
        let session_id = "ws-2-def456";
        let session_dir = dir.path().join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("state.nonces.json"),
            r#"["already-sent"]"#,
        )
        .unwrap();

        let mut daemon = test_daemon();
        daemon.store = crate::store::Store::open_in_memory().unwrap();

        assert!(
            daemon.accept_nonce(session_id, "already-sent"),
            "a nonce present only in a legacy file must be ACCEPTED — the file is retired"
        );
        assert!(
            !daemon.accept_nonce(session_id, "already-sent"),
            "but the store still rejects a genuine replay within the same run"
        );
    }

    #[test]
    fn nonce_is_rejected_after_daemon_state_is_recreated() {
        let mut first = test_daemon();
        first.store = crate::store::Store::open_in_memory().unwrap();
        let store = first.store.clone();

        assert!(first.accept_nonce("session", "nonce-1"));
        assert!(!first.accept_nonce("session", "nonce-1"));

        let mut restarted = test_daemon();
        restarted.store = store;
        assert!(!restarted.accept_nonce("session", "nonce-1"));
        assert!(restarted.accept_nonce("session", "nonce-2"));
    }

    // -- Coordination route handlers -----------------------------------------
    // These drive the real axum handlers (with their extractors) so auth,
    // validation, and store wiring are covered, not just the DB beneath them.

    fn authed_daemon() -> Shared {
        let mut daemon = test_daemon();
        daemon.token = "test-token".into();
        Arc::new(daemon)
    }

    fn with_token() -> Auth {
        Auth {
            token: Some("test-token".into()),
        }
    }

    fn create_agent_req(id: &str) -> AgentReq {
        AgentReq {
            id: id.into(),
            display_name: "Web Research Specialist".into(),
            handle: format!("handle-{id}"),
            kind: crate::coordination::types::AgentKind::Worker,
            status: crate::coordination::types::AgentStatus::Active,
            role: crate::coordination::types::AgentRole::Researcher,
            capabilities: vec!["web_search".into()],
        }
    }

    fn agents_query(token: Option<&str>) -> AgentsQuery {
        AgentsQuery {
            token: token.map(str::to_string),
            after_name: None,
            after_id: None,
            limit: default_agent_page_limit(),
        }
    }

    #[tokio::test]
    async fn agents_route_rejects_an_unauthenticated_request() {
        let d = authed_daemon();
        let response = list_agents(State(d), Query(agents_query(None))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn agents_route_requires_id_and_handle() {
        let d = authed_daemon();
        let mut req = create_agent_req("researcher");
        req.handle = "  ".into();
        let response = create_agent(State(d), Query(with_token()), Json(req)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn agents_route_creates_then_lists() {
        let d = authed_daemon();
        let created = create_agent(
            State(d.clone()),
            Query(with_token()),
            Json(create_agent_req("researcher")),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);

        // A duplicate id is a conflict, not a silent overwrite.
        let duplicate = create_agent(
            State(d.clone()),
            Query(with_token()),
            Json(create_agent_req("researcher")),
        )
        .await;
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);

        let listed = list_agents(State(d), Query(agents_query(Some("test-token")))).await;
        assert_eq!(listed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn message_route_requires_a_body() {
        let d = authed_daemon();
        let response = coordination::post_message(
            State(d),
            Query(with_token()),
            axum::extract::Path("t1".to_string()),
            Json(coordination::CoordinationMessageReq {
                body: "   ".into(),
                idempotency_key: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// A human post must wake Mission Control (that is what makes the board
    /// interactive); Mission Control's own reply must not, or it would loop.
    #[test]
    fn board_posts_wake_mission_control_except_its_own() {
        assert!(coordination::should_wake_mission_control("human"));
        assert!(coordination::should_wake_mission_control("rust-pr-reviewer"));
        assert!(!coordination::should_wake_mission_control(
            crate::mission_control::SESSION_ID
        ));
    }

    #[test]
    fn board_message_envelope_carries_sender_thread_and_reply_rule() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e1".into(),
            thread_id: coordination::COORDINATION_THREAD.into(),
            partition_key: format!("thread:{}", coordination::COORDINATION_THREAD),
            sequence: 7,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "please investigate X"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let envelope = coordination::board_message_envelope(&event, &[]);
        assert!(envelope.starts_with("[coordination_board_message]"));
        assert!(envelope.contains("thread_id: system"));
        assert!(envelope.contains("from_id: human"));
        assert!(envelope.contains("from_kind: human"));
        assert!(envelope.contains("body: please investigate X"));
        // The reply contract is what stops a silent no-op.
        assert!(envelope.contains("post_coordination_message"));
        assert!(envelope.contains("[/coordination_board_message]"));
    }

    /// The body is last so a client can read everything up to the closing tag as
    /// the message; this pins that ordering so it can't silently regress.
    #[test]
    fn board_message_body_is_the_final_field_before_the_closing_tag() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e2".into(),
            thread_id: coordination::COORDINATION_THREAD.into(),
            partition_key: format!("thread:{}", coordination::COORDINATION_THREAD),
            sequence: 8,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "line one\nline two"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k2".into(),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let envelope = coordination::board_message_envelope(&event, &[]);
        let after_body = envelope.split_once("body: ").expect("body field present").1;
        assert!(after_body.starts_with("line one\nline two"));
        assert!(
            after_body
                .trim_end()
                .ends_with("[/coordination_board_message]")
        );
    }

    /// The wake must read as a group room: recent messages are included, and the
    /// digest can't be confused with the new message or the field layout.
    #[test]
    fn board_message_envelope_includes_bounded_history() {
        let prior =
            |seq: u64, who: &str, body: &str| crate::coordination::types::CoordinationEvent {
                event_id: format!("e{seq}"),
                thread_id: coordination::COORDINATION_THREAD.into(),
                partition_key: format!("thread:{}", coordination::COORDINATION_THREAD),
                sequence: seq,
                event_type: "message.posted".into(),
                actor_kind: "agent".into(),
                actor_id: who.into(),
                payload_version: 1,
                payload: serde_json::json!({"body": body}),
                causation_id: None,
                correlation_id: None,
                idempotency_key: format!("k{seq}"),
                created_at: "2020-01-01T00:00:00Z".into(),
            };
        let current = prior(5, "human", "what is the status?");
        let history = [
            prior(3, "mission-control", "started the review"),
            prior(4, "reviewer", "found two issues"),
        ];

        let envelope = coordination::board_message_envelope(&current, &history);
        // Both prior turns appear, attributed.
        assert!(envelope.contains("mission-control: started the review"));
        assert!(envelope.contains("reviewer: found two issues"));
        assert!(envelope.contains("history: last 2 message(s)"));
        // The new message is still the final body, and history precedes it.
        let body_at = envelope.find("body: what is the status?").unwrap();
        let history_at = envelope.find("history:").unwrap();
        assert!(history_at < body_at);
        // A multi-line prior body is collapsed so it can't break the layout.
        let wrapped = coordination::board_message_envelope(&current, &[prior(1, "a", "line one\nline two")]);
        assert!(wrapped.contains("a: line one line two"));
    }

    /// With no history the room is honestly reported as just starting.
    #[test]
    fn board_message_envelope_marks_an_empty_room() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e1".into(),
            thread_id: coordination::COORDINATION_THREAD.into(),
            partition_key: format!("thread:{}", coordination::COORDINATION_THREAD),
            sequence: 1,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "first ever message"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let envelope = coordination::board_message_envelope(&event, &[]);
        assert!(envelope.contains("history: (start of the room)"));
    }

    #[tokio::test]
    async fn message_route_publishes_to_the_live_event_feed() {
        let d = authed_daemon();
        // The websocket handler forwards everything from this broadcast channel,
        // so receiving here proves a posted event reaches live subscribers.
        let mut feed = d.coordination_events.subscribe();

        coordination::post_message(
            State(d.clone()),
            Query(with_token()),
            axum::extract::Path("live".to_string()),
            Json(coordination::CoordinationMessageReq {
                body: "hello live".into(),
                idempotency_key: None,
            }),
        )
        .await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), feed.recv())
            .await
            .expect("event delivered to live subscribers")
            .expect("channel open");
        assert_eq!(event.thread_id, "live");
        assert_eq!(event.payload["body"], "hello live");
    }

    #[tokio::test]
    async fn message_route_posts_and_replays_by_cursor() {
        let d = authed_daemon();
        for body in ["first", "second"] {
            let posted = coordination::post_message(
                State(d.clone()),
                Query(with_token()),
                axum::extract::Path("t1".to_string()),
                Json(coordination::CoordinationMessageReq {
                    body: body.into(),
                    idempotency_key: None,
                }),
            )
            .await;
            assert_eq!(posted.status(), StatusCode::OK);
        }

        // Full replay returns both; a cursor past the first returns only the tail.
        let all = d.store.events_for_thread("t1", 0, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].sequence, 1);
        assert_eq!(all[1].sequence, 2);
        let tail = d.store.events_for_thread("t1", 1, 10).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].sequence, 2);
    }

    // -- Coordination visibility routes -------------------------------------





