use super::*;

#[tokio::test]
async fn tool_persistence_updates_store_without_activity_signal() {
    let dir = tempfile::tempdir().unwrap();
    let harness = CodingHarness::new(
        HarnessConfig {
            state_path: Some(dir.path().join("state.json")),
            memory_enabled: false,
            ..HarnessConfig::default()
        },
        ToolRegistry::new(),
        ToolContext::new(dir.path())
            .unwrap()
            .with_store_path(dir.path().join("store.db")),
    );
    let mut events = crate::session::subscribe_device_events();
    let mut state = harness
        .load_or_initialize_state(Some("test".into()))
        .await
        .unwrap();
    let id = harness.session_id().unwrap();
    let store = harness.store().unwrap();
    store.set_session_last_active(&id, 123).unwrap();
    let before = state.updated_at.clone();
    state.events.push(HarnessEvent::ToolResult {
        tool_name: "bash".into(),
        result: serde_json::json!({"stdout": "ok"}),
    });
    harness.persist_state(&mut state).await.unwrap();
    let row = store.get_session_row(&id).unwrap().unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.updated_at, state.updated_at);
    assert_ne!(state.updated_at, before);
    assert_eq!(row.last_active, Some(123));
    assert_eq!(
        store.load_conversation_events(&id).unwrap(),
        state.events
    );
    state.status = HarnessStatus::Completed;
    harness.persist_state(&mut state).await.unwrap();
    let mut kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event["session"] == id {
            kinds.push(event["kind"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(kinds, ["running", "done"]);
    assert_eq!(
        store.get_session_row(&id).unwrap().unwrap().status,
        "completed"
    );
}

#[cfg(test)]
mod state_migration_tests {
    use super::*;

    fn legacy_state(title: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "status": "idle",
            "created_at": "now",
            "updated_at": "now",
            "workspace": "/tmp/workspace",
            "user_request": "Restore the old session title",
            "title": title,
            "messages": [],
            "events": [],
            "iterations": 0
        }))
        .expect("legacy state JSON")
    }

    #[test]
    fn legacy_request_migrates_to_title_and_is_not_reserialized() {
        let state = deserialize_state(&legacy_state(None)).expect("legacy state should load");
        assert_eq!(
            state.title.as_deref(),
            Some("Restore the old session title")
        );
        assert_eq!(
            state.initial_request(),
            Some("Restore the old session title")
        );

        let encoded = serialize_state(&state).expect("migrated state should serialize");
        let mut decoder = flate2::read::GzDecoder::new(encoded.as_slice());
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut raw).expect("decompress state");
        let value: serde_json::Value =
            rmp_serde::from_slice(&raw).expect("decode serialized state map");
        assert_eq!(
            value.get("title").and_then(|v| v.as_str()),
            Some("Restore the old session title")
        );
        assert!(value.get("user_request").is_none());
    }

    #[test]
    fn explicit_title_wins_over_legacy_request() {
        let state =
            deserialize_state(&legacy_state(Some("Saved title"))).expect("state should load");
        assert_eq!(state.title.as_deref(), Some("Saved title"));
        assert_eq!(
            state.initial_request(),
            Some("Restore the old session title")
        );
    }
}

#[cfg(test)]
mod assistant_dedup_tests {
    use super::*;
    use crate::llm::ToolCallRecord;

    fn empty_state() -> HarnessState {
        HarnessState {
            version: 1,
            status: HarnessStatus::Idle,
            created_at: "t".into(),
            updated_at: "t".into(),
            workspace: "/tmp".into(),
            title: None,
            legacy_request: String::new(),
            goal: None,
            compacting: false,
            turn_started_at: None,
            compacting_started_at: None,
            watches: Vec::new(),
            messages: Vec::new(),
            events: Vec::new(),
            iterations: 0,
            final_text: None,
            lanes: Vec::new(),
            pending_question: None,
            approval_mode: ApprovalMode::Auto,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            last_prompt_tokens: 0,
            cache_read_tokens: 0,
            checkpoints: Vec::new(),
            rate_limit: None,
            context_window: 10_000,
            queued_inputs: Vec::new(),
            plan: Vec::new(),
            history_rewritten: false,
            events_rewritten: false,
            compactions: 0,
            reflected_events: None,
        }
    }

    fn assistant_texts(state: &HarnessState) -> Vec<&str> {
        state
            .events
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::AssistantText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn restated_status_is_not_shown_twice() {
        let mut state = empty_state();
        let first = "I'll inspect the hydrate path, keep loading until the first snapshot, then format worker reports.";
        // Narration (with tool calls) goes through the redundancy filter.
        let calls = || {
            Some(vec![ToolCallRecord {
                id: "c".into(),
                name: "read_file".into(),
                arguments: json!({}),
                signature: None,
                origin_model: None,
            }])
        };
        record_assistant_text(&mut state, first.into(), calls());
        record_assistant_text(
            &mut state,
            "I'll inspect the hydrate path, keep loading until the first snapshot.".into(),
            calls(),
        );
        record_assistant_text(
            &mut state,
            "I'll keep going on the MC chat: hide the terminal, make the list row distinct, add a dispatched-task list.".into(),
            calls(),
        );
        record_assistant_text(
            &mut state,
            "I'll keep going on the MC chat: distinct list row, hide terminal, dispatched-task list, and tool/handoff rows that only expand when they have something to show.".into(),
            calls(),
        );
        record_assistant_text(&mut state, "Fresh direction now.".into(), calls());
        assert_eq!(
            assistant_texts(&state),
            vec![
                first,
                "I'll keep going on the MC chat: hide the terminal, make the list row distinct, add a dispatched-task list.",
                "Fresh direction now.",
            ]
        );
        // The model keeps its own words in its history even when the UI skips
        // a near-duplicate; only the visible event is deduplicated.
        assert_eq!(state.messages.len(), 5);
        let empty = state
            .messages
            .iter()
            .filter_map(|m| match m {
                HarnessMessage::Assistant { content, .. } => Some(content.is_empty()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(empty, vec![false; 5]);
    }

    #[test]
    fn terminal_text_always_records_even_if_near_duplicate() {
        // A turn-final reply (no tool calls) is the actual answer — it must
        // land in events and messages even when it repeats prior narration.
        let mut state = empty_state();
        let calls = || {
            Some(vec![ToolCallRecord {
                id: "c".into(),
                name: "bash".into(),
                arguments: json!({}),
                signature: None,
                origin_model: None,
            }])
        };
        record_assistant_text(
            &mut state,
            "Scheduled goal finished: nightly review complete.".into(),
            calls(),
        );
        // Terminal reply, near-duplicate of the narration above.
        record_assistant_text(
            &mut state,
            "Scheduled goal finished: nightly review complete.".into(),
            None,
        );
        assert_eq!(
            assistant_texts(&state),
            vec![
                "Scheduled goal finished: nightly review complete.",
                "Scheduled goal finished: nightly review complete.",
            ]
        );
        assert_eq!(state.messages.len(), 2);
        match &state.messages[1] {
            HarnessMessage::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content, "Scheduled goal finished: nightly review complete.");
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected terminal assistant turn, got {other:?}"),
        }
    }

    #[test]
    fn redundant_prose_still_stores_tool_calls() {
        let mut state = empty_state();
        record_assistant_text(&mut state, "Checking the hydrate path now.".into(), None);
        record_assistant_text(
            &mut state,
            "Checking the hydrate path now.".into(),
            Some(vec![ToolCallRecord {
                id: "c1".into(),
                name: "list_sessions".into(),
                arguments: json!({}),
                signature: None,
                origin_model: None,
            }]),
        );
        assert_eq!(
            assistant_texts(&state),
            vec!["Checking the hydrate path now."]
        );
        match &state.messages[1] {
            HarnessMessage::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content, "Checking the hydrate path now.");
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "list_sessions");
            }
            other => panic!("expected assistant tool turn, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    fn harness() -> CodingHarness {
        CodingHarness::new(
            HarnessConfig::default(),
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        )
    }

    fn dispatched(body: &str) -> HarnessEvent {
        HarnessEvent::TaskDispatched {
            task_id: "t1".into(),
            title: body.into(),
            session_id: "s1".into(),
            by: "You".into(),
        }
    }

    /// A notice also enters the model's context, so a resumed loop knows what
    /// happened while it was not looking.
    #[test]
    fn a_recorded_notice_enters_the_transcript_and_the_context() {
        let harness = harness();
        let mut state = HarnessState::blank("/tmp", None);

        harness.record_notice(&mut state, dispatched("do the thing"));

        assert_eq!(state.events.len(), 1);
        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            HarnessMessage::User { content } => {
                assert!(content.contains("dispatched by You"), "got {content:?}");
                assert!(content.contains("do the thing"), "got {content:?}");
                assert!(
                    content.contains("Do not dispatch it again"),
                    "a resumed loop must know not to re-dispatch: {content:?}"
                );
            }
            other => panic!("expected a user notice, got {other:?}"),
        }
    }

    #[test]
    fn unanswered_tool_events_are_repaired_with_interrupted_status() {
        let mut events = vec![
            HarnessEvent::AssistantText {
                text: "Running a shell command to check status".to_string(),
            },
            HarnessEvent::ToolCall {
                tool_name: "bash".to_string(),
                arguments: serde_json::json!({"command": "sleep 10"}),
            },
        ];

        repair_unanswered_tool_events(&mut events, 0);

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            HarnessEvent::AssistantText { text } if text == "Running a shell command to check status"
        ));
        assert!(matches!(
            &events[1],
            HarnessEvent::ToolCall { tool_name, .. } if tool_name == "bash"
        ));
        match &events[2] {
            HarnessEvent::ToolResult { tool_name, result } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("error"));
                let err_code = result
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str());
                assert_eq!(err_code, Some("interrupted"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        // Running repair again should be a no-op since it's now answered.
        repair_unanswered_tool_events(&mut events, 0);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn already_answered_tool_events_are_not_duplicated() {
        let mut events = vec![
            HarnessEvent::ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "file.txt"}),
            },
            HarnessEvent::ToolResult {
                tool_name: "read_file".to_string(),
                result: serde_json::json!({"status": "ok"}),
            },
            HarnessEvent::ToolCall {
                tool_name: "bash".to_string(),
                arguments: serde_json::json!({"command": "cargo test"}),
            },
        ];

        repair_unanswered_tool_events(&mut events, 0);

        // read_file was already answered; only bash should receive a synthetic result.
        assert_eq!(events.len(), 4);
        match &events[3] {
            HarnessEvent::ToolResult { tool_name, result } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(
                    result
                        .get("error")
                        .and_then(|e| e.get("code"))
                        .and_then(|c| c.as_str()),
                    Some("interrupted")
                );
            }
            other => panic!("expected ToolResult for bash, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod dedup_and_stuck_edit_tests {
    use super::*;

    #[test]
    fn test_mutating_tool_set() {
        assert!(MUTATING_TOOLS.contains(&"change_files"));
        assert!(MUTATING_TOOLS.contains(&"bash"));
    }

    #[test]
    fn test_stuck_edit_signal_rendering() {
        let signal = RuntimeSignal::StuckEdit {
            path: "src/main.rs".to_string(),
            count: 2,
        };
        let rendered = signal.message();
        assert!(rendered.contains("Your changes to `src/main.rs` have failed 2 times in a row"));
        assert!(rendered.contains("rg -n"));
    }

    #[test]
    fn test_tool_context_is_file_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let file_path = temp.path().join("code.rs");

        // Untracked file
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        assert!(!ctx.is_file_unchanged(&file_path));

        // Marked read
        ctx.mark_read(&file_path);
        assert!(ctx.is_file_unchanged(&file_path));

        // Modified externally / by shell
        std::fs::write(&file_path, "fn main() { println!(\"modified\"); }\n").unwrap();
        assert!(!ctx.is_file_unchanged(&file_path));

        // Marked change
        ctx.record_change(&file_path);
        assert!(ctx.is_file_unchanged(&file_path));
    }
}

#[cfg(test)]
mod empty_reply_tests {
    use super::*;

    fn input(text: &str) -> HarnessEvent {
        HarnessEvent::UserInput { text: text.into() }
    }

    fn speak(status: &str) -> HarnessEvent {
        HarnessEvent::ToolResult {
            tool_name: "speak".into(),
            result: serde_json::json!({"schema_version": 1, "status": status}),
        }
    }

    #[test]
    fn silent_mission_control_ambient_turn_is_not_reprompted() {
        let events = [input(
            "<ambient_turn mode=\"active\" ids=\"x\" parts=\"1\">Okay.</ambient_turn>",
        )];
        assert!(!empty_turn_needs_reply(&events, true));
        let board = [input("[coordination_board_message]\nthread_id: task:1\nbody: claimed")];
        assert!(!empty_turn_needs_reply(&board, true));
        let steered = [HarnessEvent::Steer {
            text: steer_message("[mission_task_report]\ntask_id: 1"),
        }];
        assert!(!empty_turn_needs_reply(&steered, true));
    }

    #[test]
    fn a_successful_speak_counts_as_a_reply() {
        let events = [input("can you check the build?"), speak("success")];
        assert!(!empty_turn_needs_reply(&events, true));
        let failed = [input("can you check the build?"), speak("error")];
        assert!(empty_turn_needs_reply(&failed, true));
        let stale = [speak("success"), input("and the tests?")];
        assert!(empty_turn_needs_reply(&stale, true));
    }

    #[test]
    fn a_normal_chat_empty_turn_still_reprompts() {
        let typed = [input("what changed?")];
        assert!(empty_turn_needs_reply(&typed, false));
        assert!(empty_turn_needs_reply(&typed, true));
        let ambient_elsewhere = [input("<ambient_turn mode=\"active\">Okay.</ambient_turn>"), speak("success")];
        assert!(empty_turn_needs_reply(&ambient_elsewhere, false));
        let replied = [input("what changed?"), HarnessEvent::AssistantText { text: "Nothing.".into() }];
        assert!(!empty_turn_needs_reply(&replied, false));
    }
}
