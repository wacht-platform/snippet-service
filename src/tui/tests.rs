use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::config::SnippetConfig;
use crate::harness::{HarnessEvent, HarnessState};
use super::app::*;
use super::render::*;
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
