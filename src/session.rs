//! The single seam between a frontend and the agent: build the model + tools +
//! harness for a config and spawn the resident `run_interactive` loop. Drive it by
//! sending `LoopInput` on `input_tx`; observe it via the persisted `HarnessState`
//! (and, optionally, a live `StreamHandle`). Shared by the TUI and headless `serve`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::builtins::coding_tools;
use crate::config::SnippetConfig;
use crate::harness::{CodingHarness, HarnessConfig, HarnessState, LoopInput};
use crate::lanes::ModelFactory;
use crate::llm::StreamHandle;
use crate::prompts::conversation_system_prompt;
use crate::tools::ToolContext;

pub struct SessionHandle {
    pub input_tx: mpsc::UnboundedSender<LoopInput>,
    pub join: tokio::task::JoinHandle<Result<HarnessState, String>>,
    pub state_path: PathBuf,
}

/// Spawn a resident conversation session for `config`, persisting to `state_path`.
/// `stream` carries live text deltas to a UI sink; pass `None` for headless callers
/// that only read committed `HarnessState`.
pub fn start_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
) -> SessionHandle {
    let (input_tx, rx) = mpsc::unbounded_channel();

    let workspace = config.workspace.clone();
    let model_config = config.model.clone();
    let exa_api_key = config.exa_api_key.clone();
    let manual_approval = config.manual_approval;
    let context_window_tokens = model_config.context_window;
    let compact_at_pct = model_config.compact_at_pct;
    let factory: ModelFactory = {
        let mc = model_config.clone();
        Arc::new(move || mc.build_model())
    };
    let sp = state_path.clone();

    let join = tokio::spawn(async move {
        let mut model = model_config.build_model();
        let context = ToolContext::new(workspace).map_err(|e| e.to_string())?;
        let harness = CodingHarness::new(
            HarnessConfig {
                system_prompt: conversation_system_prompt(),
                state_path: Some(sp),
                resume,
                exa_api_key: exa_api_key.clone(),
                context_window_tokens,
                compact_at_pct,
                manual_approval,
                ..HarnessConfig::default()
            },
            coding_tools(exa_api_key),
            context,
        );
        harness
            .run_interactive(&mut model, initial, rx, Some(factory), stream)
            .await
            .map_err(|e| e.to_string())
    });

    SessionHandle { input_tx, join, state_path }
}
