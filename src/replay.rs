use async_trait::async_trait;

use crate::llm::{AgentModel, HarnessMessage, ModelOutput, NativeToolDefinition, StreamHandle};
use crate::tools::ToolError;

pub struct ReplayModel {
    outputs: std::collections::VecDeque<ModelOutput>,
    error: Option<String>,
}

impl ReplayModel {
    pub fn new(outputs: Vec<ModelOutput>) -> Self {
        Self {
            outputs: outputs.into(),
            error: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            outputs: Vec::new().into(),
            error: Some(message.into()),
        }
    }
}

#[async_trait]
impl AgentModel for ReplayModel {
    async fn generate(
        &mut self,
        _messages: &[HarnessMessage],
        _tools: &[NativeToolDefinition],
        _force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        if let Some(message) = &self.error {
            return Err(ToolError::msg(message.clone()));
        }
        // Replay the recorded text through the sink so a streamed UI sees it too.
        if let (Some(sink), Some(output)) = (&sink, self.outputs.front()) {
            if let Some(text) = &output.content_text {
                crate::llm::StreamBuffer::append(sink, text);
            }
        }
        self.outputs
            .pop_front()
            .ok_or_else(|| ToolError::msg("replay model has no outputs left"))
    }
}
