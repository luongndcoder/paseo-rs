//! Timeline recording: record-then-dispatch of rows, submitted prompts and echoes, and the
//! system error row. Rows are recorded BEFORE the `agent_stream` event is dispatched, so a
//! gap-recovering client can always fetch the row.

use std::sync::OnceLock;

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, ToolCallDetail, ToolCallStatus};
use paseo_provider_api::PromptInput;
use paseo_timeline::limits::limit_item_content;
use paseo_timeline::store::AppendOptions;
use paseo_timeline::{project_agent_message, ProjectedEntry, Row};
use regex::Regex;

use super::emit::StreamMeta;
use super::AgentActor;

pub(crate) const SYSTEM_ERROR_PREFIX: &str = "[System Error]";

#[derive(Debug, Clone, Default)]
pub(crate) struct RecordOptions {
    pub timestamp: Option<String>,
    pub provider_message_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptOutcome {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptRecordOptions {
    pub message_id: Option<String>,
    pub provider_message_id: Option<String>,
    pub turn_id: Option<String>,
}

/// `submittedPromptText`: strings as-is; blocks keep plain text blocks joined and trimmed.
pub(crate) fn submitted_prompt_text(prompt: &PromptInput) -> String {
    match prompt {
        PromptInput::Text(text) => text.clone(),
        PromptInput::Blocks(blocks) => blocks
            .iter()
            .filter(|block| {
                block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && block.get("mimeType").is_none()
            })
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
    }
}

/// `commandMayHaveChangedExternalState`: remote git state that no local watcher sees.
pub fn command_may_have_changed_external_state(command: &str) -> bool {
    static PATTERNS: OnceLock<[Regex; 3]> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"\bgh\s+pr\s+(merge|close|create|edit|comment|review)\b",
            r"\bgit\s+push\b",
            r"\bgit\s+fetch\b",
        ]
        .map(|p| Regex::new(p).expect("static pattern"))
    });
    let normalized = command.to_lowercase();
    patterns.iter().any(|p| p.is_match(&normalized))
}

impl AgentActor {
    /// `recordTimeline`: limit, then append at the next seq.
    pub(crate) fn record_timeline(
        &mut self,
        item: AgentTimelineItem,
        options: RecordOptions,
    ) -> Row {
        let item = limit_item_content(item);
        let timestamp = options.timestamp.unwrap_or_else(|| self.now_iso());
        self.timeline.append(
            item,
            AppendOptions {
                timestamp: Some(timestamp),
                provider_message_id: options.provider_message_id,
                turn_id: options.turn_id,
            },
        )
    }

    /// Record a row and dispatch it with `{seq, epoch, timestamp}`.
    pub(crate) fn dispatch_recorded_row(&mut self, event: AgentStreamEvent, row: &Row) {
        self.dispatch_stream(
            event,
            StreamMeta {
                seq: Some(row.seq),
                epoch: Some(self.timeline.epoch().to_owned()),
                timestamp: Some(row.timestamp.clone()),
            },
        );
    }

    /// `recordAndDispatchTimelineItem`. Returns the event as recorded (what coalesced waiters
    /// receive).
    pub(crate) fn record_and_dispatch_timeline_item(
        &mut self,
        item: AgentTimelineItem,
        provider: &str,
        turn_id: Option<String>,
        provider_message_id: Option<String>,
    ) -> AgentStreamEvent {
        let row = self.record_timeline(
            item.clone(),
            RecordOptions {
                timestamp: None,
                provider_message_id,
                turn_id: turn_id.clone(),
            },
        );
        let event = AgentStreamEvent::Timeline {
            item: Box::new(item.clone()),
            provider: provider.to_owned(),
            turn_id,
            timestamp: None,
        };
        self.dispatch_recorded_row(event.clone(), &row);
        if let AgentTimelineItem::ToolCall(call) = &item {
            if let (ToolCallStatus::Completed, ToolCallDetail::Shell { command, .. }) =
                (call.status, &call.detail)
            {
                if command_may_have_changed_external_state(command) {
                    self.ctx
                        .hooks
                        .on_workspace_state_may_have_changed(&self.view.cwd);
                }
            }
        }
        event
    }

    /// `recordSubmittedPrompt`.
    pub(crate) fn record_submitted_prompt(
        &mut self,
        prompt: &PromptInput,
        client_message_id: &str,
        outcome: PromptOutcome,
        options: PromptRecordOptions,
    ) {
        let candidate = AgentTimelineItem::UserMessage {
            text: submitted_prompt_text(prompt),
            message_id: options.message_id.clone(),
            client_message_id: Some(client_message_id.to_owned()),
        };
        let Some(item) = project_agent_message(&candidate) else {
            return;
        };
        // A human attempt stays in history on rejection; delivery notices need acceptance.
        if outcome == PromptOutcome::Rejected
            && !matches!(item, AgentTimelineItem::UserMessage { .. })
        {
            return;
        }
        self.record_submitted_prompt_item(item, options);
    }

    /// `recordSubmittedPromptItem`: one canonical `user_message` per `clientMessageId`.
    fn record_submitted_prompt_item(
        &mut self,
        item: AgentTimelineItem,
        options: PromptRecordOptions,
    ) {
        if let AgentTimelineItem::UserMessage {
            client_message_id: Some(client_message_id),
            ..
        } = &item
        {
            if self
                .timeline
                .submitted_user_message(client_message_id)
                .is_some()
            {
                return;
            }
        }
        self.touch_updated_at();
        if matches!(item, AgentTimelineItem::UserMessage { .. }) {
            self.view.last_user_message_at_ms = Some(self.now_ms());
        }
        let provider = self.view.provider.clone();
        self.record_and_dispatch_timeline_item(
            item,
            &provider,
            options.turn_id,
            options.provider_message_id,
        );
    }

    /// `reconcileSubmittedPromptEcho`: the provider echo of a submitted prompt only enriches
    /// `providerMessageId`; it is never a second row. `None` means "not an echo".
    pub(crate) fn reconcile_submitted_prompt_echo(
        &mut self,
        item: &AgentTimelineItem,
        turn_id: Option<&str>,
    ) -> Option<ProjectedEntry> {
        let AgentTimelineItem::UserMessage {
            text,
            message_id,
            client_message_id: Some(client_message_id),
        } = item
        else {
            return None;
        };
        let mut existing = self.timeline.submitted_user_message(client_message_id);
        if existing.is_none() {
            self.record_submitted_prompt_item(
                AgentTimelineItem::UserMessage {
                    text: text.clone(),
                    message_id: Some(client_message_id.clone()),
                    client_message_id: Some(client_message_id.clone()),
                },
                PromptRecordOptions {
                    message_id: None,
                    provider_message_id: message_id.clone().filter(|id| !id.is_empty()),
                    turn_id: turn_id.filter(|id| !id.is_empty()).map(str::to_owned),
                },
            );
            existing = self.timeline.submitted_user_message(client_message_id);
        }
        let existing = existing?;
        if !matches!(existing.item, AgentTimelineItem::UserMessage { .. }) {
            return None;
        }
        if let Some(provider_message_id) = message_id.as_deref().filter(|id| !id.is_empty()) {
            self.timeline
                .enrich_submitted_user_message(client_message_id, provider_message_id);
        }
        Some(existing)
    }

    /// `appendSystemErrorTimelineMessage`: `[System Error] <message>` unless it repeats the
    /// last row.
    pub(crate) fn append_system_error_timeline_message(&mut self, provider: &str, message: &str) {
        let normalized = message.trim();
        if normalized.is_empty() {
            return;
        }
        let text = format!("{SYSTEM_ERROR_PREFIX} {normalized}");
        if matches!(
            self.timeline.last_item(),
            Some(AgentTimelineItem::AssistantMessage { text: last, .. }) if *last == text
        ) {
            return;
        }
        let item = AgentTimelineItem::AssistantMessage {
            text,
            message_id: None,
        };
        let row = self.record_timeline(item.clone(), RecordOptions::default());
        let event = AgentStreamEvent::Timeline {
            item: Box::new(item),
            provider: provider.to_owned(),
            turn_id: None,
            timestamp: None,
        };
        self.dispatch_recorded_row(event, &row);
    }

    /// `appendTimelineItem` (the API form): limit, touch, record, dispatch. Returns `(seq, epoch)`.
    pub(crate) fn append_timeline_item(&mut self, item: AgentTimelineItem) -> (i64, String) {
        let item = limit_item_content(item);
        self.touch_updated_at();
        let row = self.record_timeline(item.clone(), RecordOptions::default());
        let event = AgentStreamEvent::Timeline {
            item: Box::new(item),
            provider: self.view.provider.clone(),
            turn_id: None,
            timestamp: None,
        };
        self.dispatch_recorded_row(event, &row);
        (row.seq, self.timeline.epoch().to_owned())
    }

    /// `emitLiveTimelineItem`: broadcast only, nothing recorded.
    pub(crate) fn emit_live_timeline_item(&mut self, item: AgentTimelineItem) {
        self.touch_updated_at();
        let event = AgentStreamEvent::Timeline {
            item: Box::new(item),
            provider: self.view.provider.clone(),
            turn_id: None,
            timestamp: None,
        };
        self.dispatch_stream(event, StreamMeta::default());
    }
}

/// `formatTurnFailedMessage`: base (or `Provider run failed`), `code: <code>`, diagnostic when it
/// differs, joined by a blank line.
pub(crate) fn format_turn_failed_message(
    error: &str,
    code: Option<&str>,
    diagnostic: Option<&str>,
) -> String {
    let base = error.trim();
    let mut parts = vec![if base.is_empty() {
        "Provider run failed".to_owned()
    } else {
        base.to_owned()
    }];
    if let Some(code) = code.map(str::trim).filter(|c| !c.is_empty()) {
        parts.push(format!("code: {code}"));
    }
    if let Some(diagnostic) = diagnostic.map(str::trim) {
        if !diagnostic.is_empty() && diagnostic != base {
            parts.push(diagnostic.to_owned());
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_state_commands() {
        for command in [
            "gh pr merge 12",
            "GH  PR create --fill",
            "git push origin main",
            "git fetch",
        ] {
            assert!(
                command_may_have_changed_external_state(command),
                "{command}"
            );
        }
        for command in [
            "git commit -m x",
            "gh pr view 1",
            "echo git pushing",
            "git pull",
        ] {
            assert!(
                !command_may_have_changed_external_state(command),
                "{command}"
            );
        }
    }

    #[test]
    fn turn_failed_message_format() {
        assert_eq!(
            format_turn_failed_message("  ", None, None),
            "Provider run failed"
        );
        assert_eq!(
            format_turn_failed_message("boom", Some(" E1 "), Some("details")),
            "boom\n\ncode: E1\n\ndetails"
        );
        assert_eq!(
            format_turn_failed_message("boom", None, Some("boom")),
            "boom"
        );
    }

    #[test]
    fn prompt_text_joins_plain_text_blocks_only() {
        let blocks = PromptInput::Blocks(vec![
            serde_json::json!({"type": "text", "text": " one "}),
            serde_json::json!({"type": "image", "mimeType": "image/png", "data": "x"}),
            serde_json::json!({"type": "text", "text": "two", "mimeType": "text/plain"}),
            serde_json::json!({"type": "text", "text": "three "}),
        ]);
        assert_eq!(submitted_prompt_text(&blocks), "one \nthree");
        assert_eq!(
            submitted_prompt_text(&PromptInput::Text(" keep ".into())),
            " keep "
        );
    }
}
