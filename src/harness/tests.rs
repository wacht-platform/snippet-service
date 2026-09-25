use super::*;

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
        assert_eq!(state.tool_payloads_pruned, false);
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
            tool_payloads_pruned: false,
            queued_inputs: Vec::new(),
            history_rewritten: false,
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
    fn restated_status_is_not_stored() {
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
        // Every turn persists (tool calls must keep pairing valid); redundant
        // narration turns carry empty content.
        assert_eq!(state.messages.len(), 5);
        let non_empty = state
            .messages
            .iter()
            .filter_map(|m| match m {
                HarnessMessage::Assistant { content, .. } => Some(content.is_empty()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(non_empty, vec![false, true, false, true, false]);
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
                assert!(content.is_empty());
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "list_sessions");
            }
            other => panic!("expected assistant tool turn, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tool_prune_tests {
    use super::*;
    use crate::llm::ToolCallRecord;

    fn bulky_messages(n_tools: usize) -> Vec<HarnessMessage> {
        let mut msgs = vec![HarnessMessage::System {
            content: "sys".into(),
        }];
        for i in 0..n_tools {
            let id = format!("c{i}");
            let blob = "x".repeat(4_000);
            msgs.push(HarnessMessage::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCallRecord {
                    id: id.clone(),
                    name: "bash".into(),
                    arguments: json!({"command": blob.clone()}),
                    signature: None,
                    origin_model: None,
                }],
            });
            msgs.push(HarnessMessage::ToolResult {
                tool_call_id: id,
                tool_name: "bash".into(),
                content: json!({"stdout": blob, "status": "ok"}),
            });
        }
        // Live tail the pruner must leave alone.
        msgs.push(HarnessMessage::User {
            content: "keep me".into(),
        });
        msgs.push(HarnessMessage::Assistant {
            content: "recent".into(),
            tool_calls: vec![ToolCallRecord {
                id: "live".into(),
                name: "bash".into(),
                arguments: json!({"command": "echo live-tail-must-stay"}),
                signature: None,
                origin_model: None,
            }],
        });
        msgs.push(HarnessMessage::ToolResult {
            tool_call_id: "live".into(),
            tool_name: "bash".into(),
            content: json!({"stdout": "live-tail-must-stay", "status": "ok"}),
        });
        msgs
    }

    fn test_state(messages: Vec<HarnessMessage>, last_prompt_tokens: u64) -> HarnessState {
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
            messages,
            events: Vec::new(),
            iterations: 0,
            final_text: None,
            lanes: Vec::new(),
            pending_question: None,
            approval_mode: ApprovalMode::Auto,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            last_prompt_tokens,
            cache_read_tokens: 0,
            checkpoints: Vec::new(),
            rate_limit: None,
            context_window: 10_000,
            tool_payloads_pruned: false,
            queued_inputs: Vec::new(),
            history_rewritten: false,
        }
    }

    #[test]
    fn prunes_old_tool_bodies_keeps_recent_and_pairing() {
        // window 10k, fire at 50% (5k), prefix = first 40% (4k) of window.
        // Each bulky tool pair is ~2k est tokens, so several early pairs fall
        // inside the prefix and must be stubbed; the live tail must not.
        let harness = CodingHarness::new(
            HarnessConfig {
                context_window_tokens: 10_000,
                tool_prune_at_pct: 50,
                tool_prune_prefix_pct: 40,
                ..HarnessConfig::default()
            },
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        );
        let mut state = test_state(bulky_messages(12), 8_000); // above 50% of 10k
        let prefix_budget = 10_000u64 * 40 / 100;
        let mut cum = 0u64;
        let mut cut = 0usize;
        for (i, msg) in state.messages.iter().enumerate() {
            let t = estimate_message_tokens(msg);
            if cum + t > prefix_budget {
                break;
            }
            cum += t;
            cut = i + 1;
        }
        assert!(
            cut > 1,
            "prefix cut should cover more than system alone, cut={cut}"
        );

        let before = estimate_prompt_tokens(&state.messages);
        assert!(harness.prune_old_tool_payloads(&mut state));
        assert!(state.tool_payloads_pruned);
        let after = estimate_prompt_tokens(&state.messages);
        assert!(
            after < before,
            "expected prune to shrink tokens {before} → {after}"
        );

        // Everything in the prefix with tools is stubbed.
        for i in 0..cut {
            match &state.messages[i] {
                HarnessMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    assert!(
                        tool_args_are_stub(&tool_calls[0].arguments),
                        "prefix msg {i} tool args should be stubbed"
                    );
                    assert!(!tool_calls[0].id.is_empty());
                    assert_eq!(tool_calls[0].name, "bash");
                }
                HarnessMessage::ToolResult { content, .. } => {
                    assert!(
                        tool_result_is_stub(content),
                        "prefix msg {i} tool result should be stubbed"
                    );
                }
                _ => {}
            }
        }

        // Past the prefix: tool payloads stay intact (including live tail).
        for i in cut..state.messages.len() {
            match &state.messages[i] {
                HarnessMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    assert!(
                        !tool_args_are_stub(&tool_calls[0].arguments),
                        "post-prefix msg {i} tool args must stay"
                    );
                }
                HarnessMessage::ToolResult { content, .. } => {
                    assert!(
                        !tool_result_is_stub(content),
                        "post-prefix msg {i} tool result must stay"
                    );
                }
                _ => {}
            }
        }

        let last = state.messages.last().expect("tail");
        match last {
            HarnessMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "live");
                assert!(!tool_result_is_stub(content));
                assert_eq!(
                    content.get("stdout").and_then(Value::as_str),
                    Some("live-tail-must-stay")
                );
            }
            other => panic!("expected live tool result, got {other:?}"),
        }
    }

    #[test]
    fn pruning_is_once_per_history_epoch() {
        let harness = CodingHarness::new(
            HarnessConfig {
                context_window_tokens: 10_000,
                tool_prune_at_pct: 50,
                tool_prune_prefix_pct: 40,
                ..HarnessConfig::default()
            },
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        );
        let mut state = test_state(bulky_messages(12), 8_000);
        assert!(harness.prune_old_tool_payloads(&mut state));

        state.messages.insert(
            1,
            HarnessMessage::ToolResult {
                tool_call_id: "new-old".into(),
                tool_name: "bash".into(),
                content: json!({"stdout": "z".repeat(4_000)}),
            },
        );
        let messages = state.messages.clone();
        assert!(!harness.prune_old_tool_payloads(&mut state));
        assert_eq!(state.messages, messages);
    }

    #[test]
    fn insignificant_savings_do_not_mutate_or_mark_epoch() {
        let harness = CodingHarness::new(
            HarnessConfig {
                context_window_tokens: 1_000,
                tool_prune_at_pct: 50,
                tool_prune_prefix_pct: 100,
                ..HarnessConfig::default()
            },
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        );
        let messages = vec![
            HarnessMessage::System {
                content: "sys".into(),
            },
            HarnessMessage::ToolResult {
                tool_call_id: "small".into(),
                tool_name: "bash".into(),
                content: json!({"stdout": "tiny"}),
            },
        ];
        let mut state = test_state(messages.clone(), 900);
        assert!(!harness.prune_old_tool_payloads(&mut state));
        assert_eq!(state.messages, messages);
        assert!(!state.tool_payloads_pruned);
    }

    #[tokio::test]
    async fn successful_compaction_starts_a_new_prune_epoch() {
        let harness = CodingHarness::new(
            HarnessConfig::default(),
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        );
        let mut state = test_state(bulky_messages(12), 100_000);
        state.tool_payloads_pruned = true;

        harness
            .compact_history(&mut state, true)
            .await
            .expect("compaction");

        assert!(!state.tool_payloads_pruned);
        assert_eq!(state.last_prompt_tokens, 0);
        assert!(state.events.iter().any(
            |event| matches!(event, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted")
        ));
    }

    #[test]
    fn skips_prune_below_threshold() {
        let harness = CodingHarness::new(
            HarnessConfig {
                context_window_tokens: 10_000,
                tool_prune_at_pct: 75,
                ..HarnessConfig::default()
            },
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        );
        let mut state = test_state(bulky_messages(12), 1_000); // well under 75%
        assert!(!harness.prune_old_tool_payloads(&mut state));
        match &state.messages[1] {
            HarnessMessage::Assistant { tool_calls, .. } => {
                assert!(!tool_args_are_stub(&tool_calls[0].arguments));
            }
            _ => panic!("expected assistant"),
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

    /// A notice buffered during a step must survive an interrupt.
    ///
    /// The interrupt paths discard the in-flight turn with a truncate, and a
    /// notice is not part of that turn — it records something the sender already
    /// had accepted. `pending_inputs` is dropped when the loop breaks, so without
    /// draining notices at the interrupt, a direct message sent to a session that
    /// was mid-run disappeared from its transcript even though the send had
    /// succeeded. That is the regression this pins.
    #[test]
    fn a_buffered_notice_survives_an_interrupt() {
        let harness = harness();
        let mut state = HarnessState::blank("/tmp", None);
        let mut pending = vec![
            LoopInput::Notice(dispatched("keep me")),
            LoopInput::Interrupt,
        ];

        harness.record_pending_notices(&mut state, &mut pending);

        assert!(
            state.events.iter().any(|e| matches!(
                e,
                HarnessEvent::TaskDispatched { title, .. } if title == "keep me"
            )),
            "the notice must reach the transcript"
        );
        assert_eq!(pending.len(), 1, "only notices are consumed");
        assert!(
            matches!(pending[0], LoopInput::Interrupt),
            "an interrupt is left for the loop to act on"
        );
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
    fn test_dedup_tools_includes_read_file() {
        assert!(DEDUP_TOOLS.contains(&"read_file"));
        assert!(DEDUP_TOOLS.contains(&"search_content"));
        assert!(MUTATING_TOOLS.contains(&"edit_file"));
        assert!(MUTATING_TOOLS.contains(&"bash"));
    }

    #[test]
    fn test_stuck_edit_signal_rendering() {
        let signal = RuntimeSignal::StuckEdit {
            path: "src/main.rs".to_string(),
            count: 2,
        };
        let rendered = signal.render();
        assert!(rendered.starts_with("stuck_edit = \""));
        assert!(rendered.contains("your edits on `src/main.rs` have failed 2 times consecutively"));
        assert!(rendered.contains("read_file"));
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
