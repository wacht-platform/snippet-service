use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::config::SnippetConfig;
use crate::harness::{HarnessEvent, HarnessState};
use super::app::*;
use super::render::*;
use super::keybindings::handle_question_key;
use super::*;


    /// Build an `App` for rendering without a daemon.
    ///
    /// `App::new` is pure construction — no file, network or process IO — so it
    /// is safe to call directly under `cargo test`. It does arm the connecting
    /// overlay unconditionally and pick a random uuid conversation, so both are
    /// normalised: the transcript is what a normal launch shows, and a fixed id
    /// keeps the snapshot stable across runs.
    fn tui_app() -> App {
        let mut app = App::new(TuiOptions {
            config_path: PathBuf::from("/nonexistent/config.toml"),
            config: SnippetConfig::default(),
            resume: None,
        });
        app.connecting_phase = None;
        app.active_conversation = "tui-snapshot".to_string();
        app
    }

    /// Render one frame and return it as rows of text.
    ///
    /// This is the TUI's equivalent of the Flutter golden harness: `TestBackend`
    /// composites a real frame into a buffer, so the assertions below read what
    /// the terminal would actually show rather than what the code intends.
    fn snapshot(width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = tui_app();
        terminal
            .draw(|f| render(f, &mut app))
            .expect("draw a frame");
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol())
                            .unwrap_or(" ")
                    })
                    .collect::<String>()
            })
            .collect()
    }

    /// The whole frame as one string, for "does it contain X" assertions.
    fn frame_text(width: u16, height: u16) -> String {
        snapshot(width, height).join("\n")
    }

    /// Print a frame so a human can read it: `cargo test -- --nocapture`.
    #[test]
    fn dump_frame_for_review() {
        for row in snapshot(90, 24) {
            println!("|{row}|");
        }
    }

    /// The empty state is the first thing anyone sees, and it is composed almost
    /// entirely of one long literal in `transcript.rs` — so a mangled or dropped
    /// block of art is invisible to every other kind of test.
    #[test]
    fn empty_state_renders_its_artwork_and_status() {
        let rows = snapshot(90, 24);
        let frame = rows.join("\n");
        for expected in [
            "(  o.o  )",          // the mascot's eyes
            "SNIPPET",            // the nameplate inside the mascot's box
            "MATRIX PET",
            "███████",            // the block-letter title
            "workspace",          // the footer names the working folder
            "gpt-4o",             // ...and the model
        ] {
            assert!(frame.contains(expected), "empty state is missing {expected:?}");
        }
        for stray in [
            "t                                          T",
            "G",
            "g                                          g",
            "t",
        ] {
            assert!(
                rows.iter().all(|row| row.trim() != stray),
                "empty state contains an orphan artwork row {stray:?}"
            );
        }
    }

    /// A model that is not connected is the one case the footer must not stay
    /// quiet about: without it the screen is a silent prompt.
    #[test]
    fn the_disconnected_hint_is_shown() {
        let frame = frame_text(90, 24);
        assert!(
            frame.contains("No model connected yet"),
            "a disconnected TUI must say so; frame was:\n{frame}"
        );
    }

    /// The header divider is what separates chrome from the transcript.
    #[test]
    fn the_header_divider_is_drawn() {
        let rows = snapshot(90, 24);
        assert!(
            rows[1].contains('─'),
            "row 2 should be the header rule, got {:?}",
            rows[1]
        );
    }

    /// Layout maths is where a TUI panics, and it panics only at the sizes nobody
    /// runs by hand. Every one of these must render without unwinding.
    #[test]
    fn renders_at_many_sizes_without_panicking() {
        for (w, h) in [
            (40u16, 12u16),
            (50, 20),
            (80, 24),
            (90, 30),
            (120, 40),
            (200, 60),
        ] {
            let rows = snapshot(w, h);
            assert_eq!(rows.len(), h as usize, "height mismatch at {w}x{h}");
            for row in &rows {
                assert_eq!(
                    row.chars().count(),
                    w as usize,
                    "width mismatch at {w}x{h}"
                );
            }
        }
    }

    /// A resumed conversation renders the transcript instead of the welcome art,
    /// so the two states are distinguishable by content.
    #[test]
    fn a_non_empty_conversation_drops_the_welcome_art() {
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = App::new(TuiOptions {
            config_path: PathBuf::from("/nonexistent/config.toml"),
            config: SnippetConfig::default(),
            resume: Some("existing".to_string()),
        });
        app.connecting_phase = None;
        // A real state with a message in it, so `empty` is false. Built by
        // mutation, not struct-update syntax: `HarnessState` carries a private
        // field, so `..Default::default()` is not allowed from out here.
        let mut st = HarnessState::default();
        st.events = vec![HarnessEvent::UserInput {
            text: "hello from the snapshot test".to_string(),
        }];
        st.title = Some("Snapshot".to_string());
        app.state = Some(st);
        terminal.draw(|f| render(f, &mut app)).expect("draw");

        let buf = terminal.backend().buffer();
        let frame: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !frame.contains("MATRIX PET"),
            "welcome art should not draw over a real conversation"
        );
    }

/// Render one frame of an already-prepared app, as text rows.
fn snapshot_app(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| render(f, app)).expect("draw a frame");
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ")).collect())
        .collect()
}

fn board_app() -> App {
    let mut app = tui_app();
    app.shell.width = 170;
    app.shell.pane = Some(true);
    app.shell.focus = Focus::Pane;
    app.boards.seed(
        "agents",
        serde_json::json!([
            {"id": "snippet", "display_name": "Snippet", "role": "coding", "status": "idle", "kind": "general",
             "capabilities": ["code", "review"]},
            {"id": "designer", "display_name": "Designer", "role": "ui", "status": "busy"},
        ]),
    );
    app.boards.seed(
        "board:snippet",
        serde_json::json!({"entries": [
            {"kind": "dispatched", "summary": "Wire the traffic path panel", "workspace": "/home/u/ext"},
            {"kind": "reported", "summary": "31 taxonomy assertions pass; combination gating verified.", "workspace": "/home/u/ext"},
        ]}),
    );
    app
}

#[test]
fn agents_panel_lists_agents_and_opens_a_board() {
    let mut app = board_app();
    app.shell.tab = PaneTab::Agents;
    let list = snapshot_app(&mut app, 170, 30).join("\n");
    for row in list.lines() { println!("|{row}|"); }
    assert!(list.contains("Snippet") && list.contains("Designer"));
    assert!(list.contains("idle") && list.contains("busy"));

    app.shell.pane_detail = Some(0);
    let detail = snapshot_app(&mut app, 170, 30).join("\n");
    for row in detail.lines() { println!("|{row}|"); }
    assert!(detail.contains("Board"));
    assert!(detail.contains("Wire the traffic path panel"));
    assert!(detail.contains("reported"));
}

#[test]
fn tasks_and_jobs_panels_render() {
    let mut app = board_app();
    app.boards.seed(
        "tasks",
        serde_json::json!([
            {"id": "eaa98084-fee3", "title": "Check Secure Access extension", "status": "in_progress",
             "description": "Confirm whether all Secure Access work is done.", "summary": "31 assertions pass."},
            {"id": "9f8e7d6c5b4a", "title": "Package the extension", "status": "blocked"},
        ]),
    );
    app.boards.seed(
        "jobs",
        serde_json::json!([
            {"id": "j1", "title": "Nightly dependency audit", "enabled": true,
             "schedule": {"kind": "daily", "hour": 2, "minute": 30}, "next_run_at": 0},
            {"id": "j2", "title": "Check CI on open PRs", "enabled": false,
             "schedule": {"kind": "interval", "every_secs": 3600}, "last_error": "session not found"},
        ]),
    );
    app.shell.tab = PaneTab::Tasks;
    let tasks = snapshot_app(&mut app, 170, 30).join("\n");
    for row in tasks.lines() { println!("|{row}|"); }
    assert!(tasks.contains("Check Secure Access extension") && tasks.contains("in progress"));
    assert!(tasks.contains("blocked") && tasks.contains("eaa98084"));

    app.shell.pane_detail = Some(0);
    let detail = snapshot_app(&mut app, 170, 30).join("\n");
    assert!(detail.contains("Briefing") && detail.contains("Confirm whether all Secure Access"));
    assert!(detail.contains("31 assertions pass."));

    app.shell.pane_detail = None;
    app.shell.tab = PaneTab::Jobs;
    app.board_confirm = Some(super::boards::Confirm {
        prompt: "Delete Nightly dependency audit? y to confirm".into(),
        action: super::boards::ConfirmAction::DeleteJob("j1".into(), "Nightly dependency audit".into()),
    });
    let jobs = snapshot_app(&mut app, 170, 30).join("\n");
    for row in jobs.lines() { println!("|{row}|"); }
    assert!(jobs.contains("daily 02:30") && jobs.contains("every 1h"));
    assert!(jobs.contains("paused") && jobs.contains("session not found"));
    assert!(jobs.contains("y to confirm"));
}

#[test]
fn usage_and_vault_panels_render_without_exposing_secrets() {
    let mut app = board_app();
    app.boards.seed(
        "usage:all",
        serde_json::json!({"providers": [
            {"provider": "chatgpt", "sessions": 3, "calls": 42, "total_tokens": 1_234_567,
             "prompt_tokens": 1_000_000, "cache_read_tokens": 800_000, "completion_tokens": 234_567,
             "models": [{"model": "gpt-5-codex", "total_tokens": 1_234_567, "calls": 42}],
             "rate_limits": [{"window_minutes": 300, "used_percent": 30.0, "resets_at": 9_999_999_999i64}]},
            {"provider": "xai", "sessions": 1, "calls": 2, "total_tokens": 0, "rate_limits": [],
             "rate_limits_supported": false},
        ]}),
    );
    app.shell.tab = PaneTab::Usage;
    let usage = snapshot_app(&mut app, 170, 30).join("\n");
    for row in usage.lines() { println!("|{row}|"); }
    assert!(usage.contains("All time") && usage.contains("chatgpt"));
    assert!(usage.contains("1.2M") && usage.contains("gpt-5-codex"));
    assert!(usage.contains("5-hour window") && usage.contains("70% left"));
    assert!(usage.contains("aren't exposed"));

    app.boards.seed("vault", serde_json::json!({"names": ["GITHUB_TOKEN", "OPENAI_KEY"]}));
    app.shell.tab = PaneTab::Vault;
    app.vault_input = Some(super::boards::VaultInput {
        name: "NEW_SECRET".into(),
        value: "hunter2-super-secret".into(),
        on_value: true,
    });
    let vault = snapshot_app(&mut app, 170, 30).join("\n");
    for row in vault.lines() { println!("|{row}|"); }
    assert!(vault.contains("GITHUB_TOKEN") && vault.contains("NEW_SECRET"));
    assert!(!vault.contains("hunter2"), "a secret value must never render");
}

#[test]
fn ctrl_f_opens_sessions_and_ctrl_g_still_steers() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = tui_app();
    app.shell.width = 170;
    assert!(handle_shell_key(&mut app, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)));
    assert_eq!(app.shell.focus, Focus::Sidebar);
    // Ctrl-G is steer-now; the shell must leave it to the global handler.
    assert!(!handle_shell_key(&mut app, KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)));
}

fn question_app(pending: serde_json::Value) -> App {
    let mut app = tui_app();
    let mut st = HarnessState::default();
    st.status = crate::harness::HarnessStatus::WaitingForInput;
    st.pending_question = Some(pending.clone());
    st.events.push(HarnessEvent::UserQuestion { questions: pending });
    app.state = Some(st);
    app
}

#[test]
fn ask_user_picker_shows_recommended_multi_choice_tabs_and_review() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
    let mut app = question_app(serde_json::json!({"questions": [
        {"id": "targets", "header": "Targets", "text": "Which builds should I make?",
         "answer_kind": {"kind": "multi_choice", "choices": [
            {"value": "macos", "label": "macOS", "description": "Universal app bundle"},
            {"value": "android", "label": "Android", "description": "ARM64 release APK", "recommended": true}]}},
        {"id": "ship", "header": "Ship", "text": "Publish the release now?",
         "answer_kind": {"kind": "confirm", "confirm_label": "Publish", "cancel_label": "Hold"}}
    ]}));
    ensure_q_init(&mut app); // what tick() does when the question arrives

    let first = snapshot_app(&mut app, 100, 40).join("\n");
    for row in first.lines() { println!("|{row}|"); }
    assert!(first.contains("Targets") && first.contains("Ship"), "question tabs");
    assert!(first.contains("recommended") && first.contains("ARM64 release APK"));
    // Recommended leads and is already ticked.
    let android = first.lines().position(|l| l.contains("Android")).unwrap();
    let macos = first.lines().position(|l| l.contains("macOS")).unwrap();
    assert!(android < macos);
    assert!(first.lines().nth(android).unwrap().contains("[x]"));

    // 2 ticks macOS too, Enter moves on to the confirm with its own labels.
    assert!(handle_question_key(&mut app, key(KeyCode::Char('2'))));
    assert!(handle_question_key(&mut app, key(KeyCode::Enter)));
    let second = snapshot_app(&mut app, 100, 40).join("\n");
    assert!(second.contains("Publish") && second.contains("Hold") && second.contains("✓ Targets"));

    // Back returns to the first question; forward again, then pick Publish.
    assert!(handle_question_key(&mut app, key(KeyCode::Left)));
    assert_eq!(app.q_index, 0);
    assert!(handle_question_key(&mut app, key(KeyCode::Char('2'))));
    assert!(handle_question_key(&mut app, key(KeyCode::Enter)));
    assert!(handle_question_key(&mut app, key(KeyCode::Char('1'))));

    // Every question answered: a review before anything is sent.
    assert!(app.q_review);
    let review = snapshot_app(&mut app, 100, 40).join("\n");
    for row in review.lines() { println!("|{row}|"); }
    assert!(review.contains("Check your answers"));
    assert!(review.contains("Android (android), macOS (macos)"));
    assert!(review.contains("Publish (confirm)"));
}

#[test]
fn ask_user_choice_question_takes_a_typed_answer() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = question_app(serde_json::json!({"questions": [
        {"text": "Which branch?", "answer_kind": {"kind": "single_choice", "choices": [
            {"value": "main", "label": "main"}]}}
    ]}));
    ensure_q_init(&mut app);
    app.input = "release/1.2".into();
    // Typing isn't swallowed by the picker any more; Enter sends the typed text.
    assert!(!handle_question_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    app.answer_current_question();
    assert!(app.q_answers.is_empty() && app.input.is_empty(), "sent and reset");
}
