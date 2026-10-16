//! Out-of-band prompts (`tryRunOutOfBand`, `agent-manager.ts:2473-2511`).
//!
//! DEVIATION(out-of-band-via-stream): TS gets a handler object and its `emit` callback records
//! rows and dispatches straight away. The provider API's `try_handle_out_of_band` is one async
//! call returning whether the provider handled the prompt, with its events arriving on the
//! normal event stream. To keep the submitted prompt row ahead of the output, a barrier holds
//! the stream while the provider call is in flight; the prompt is recorded, then the held
//! events go through the regular pipeline (so assistant text coalesces like any live output).

use paseo_provider_api::PromptInput;

use crate::agent_actor::timeline_record::{PromptOutcome, PromptRecordOptions};
use crate::error::Result;
use crate::manager::AgentManager;
use crate::model::RunOpts;
use paseo_agent_types::AgentTimelineItem;

impl AgentManager {
    /// `tryRunOutOfBand`: `true` when the session accepted the prompt as a side-effect command.
    /// A provider failure becomes an `[Error] <message>` assistant row and still returns `true`.
    pub async fn try_run_out_of_band(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        options: RunOpts,
    ) -> Result<bool> {
        let handle = self.require_agent(agent_id)?;
        let barrier = handle.ask(|a| a.gate.install_barrier()).await?;
        let handled = handle.session.try_handle_out_of_band(prompt.clone()).await;
        let accepted = !matches!(handled, Ok(false));
        let client_message_id = options.client_message_id.clone();
        let failure = handled.as_ref().err().map(ToString::to_string);
        handle
            .ask(move |a| {
                if accepted {
                    if let Some(id) = &client_message_id {
                        a.record_submitted_prompt(
                            &prompt,
                            id,
                            PromptOutcome::Accepted,
                            PromptRecordOptions::default(),
                        );
                        a.emit_state(true);
                    }
                }
                if let Some(message) = failure {
                    let text = if message.is_empty() {
                        "Out-of-band command failed".to_owned()
                    } else {
                        message
                    };
                    a.append_timeline_item(AgentTimelineItem::AssistantMessage {
                        text: format!("[Error] {text}"),
                        message_id: None,
                    });
                }
                a.end_steer_admission(barrier);
            })
            .await?;
        Ok(accepted)
    }
}
