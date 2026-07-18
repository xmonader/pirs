use std::collections::BTreeMap;

use pirs_ai::{Message, StopReason, Usage};

#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub model: String,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Default)]
pub struct UsageReport {
    pub calls: Vec<UsageRecord>,
    pub main_usage: Usage,
    pub delegate_usage: Usage,
    pub compaction_usage: Usage,
    pub by_model: BTreeMap<String, Usage>,
}

impl UsageReport {
    pub fn grand_total(&self) -> Usage {
        let mut total =
            self.main_usage.clone() + self.delegate_usage.clone() + self.compaction_usage.clone();
        // total_tokens accumulated per-message can drift from input+output
        // (providers report it independently); recompute from the parts.
        total.total_tokens = total.input + total.output + total.cache_read + total.cache_write;
        total
    }

    pub fn delegate_calls(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| c.model.starts_with("delegate:"))
            .count()
    }
}

/// Aggregate token usage from a message list: assistant API calls,
/// delegate sub-agent usage (from tool result details), plus any
/// out-of-band usage (compaction summaries) tracked separately.
pub fn usage_report(messages: &[Message], compaction_usage: Usage) -> UsageReport {
    let mut report = UsageReport {
        compaction_usage,
        ..Default::default()
    };

    for msg in messages {
        match msg {
            Message::Assistant(a) => {
                if a.usage.total_tokens == 0 && a.usage.input == 0 && a.usage.output == 0 {
                    continue;
                }
                report.calls.push(UsageRecord {
                    model: a.model.clone(),
                    usage: a.usage.clone(),
                    stop_reason: a.stop_reason,
                    timestamp: a.timestamp,
                });
                report.main_usage += a.usage.clone();
                *report.by_model.entry(a.model.clone()).or_default() += a.usage.clone();
            }
            Message::ToolResult(tr) if tr.tool_name == "delegate" => {
                let Some(details) = &tr.details else { continue };
                let usage = parse_usage(details.get("subAgentUsage"));
                if usage.total_tokens == 0 && usage.input == 0 {
                    continue;
                }
                let model = details
                    .get("subAgentModel")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                report.calls.push(UsageRecord {
                    model: format!("delegate:{model}"),
                    usage: usage.clone(),
                    stop_reason: StopReason::Stop,
                    timestamp: tr.timestamp,
                });
                report.delegate_usage += usage.clone();
                *report.by_model.entry(model).or_default() += usage;
            }
            _ => {}
        }
    }
    report
}

fn parse_usage(v: Option<&serde_json::Value>) -> Usage {
    let Some(v) = v else { return Usage::default() };
    serde_json::from_value(v.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::{AssistantMessage, ContentBlock, ToolResultMessage};

    fn assistant(model: &str, input: u64, cache: u64, output: u64) -> Message {
        Message::Assistant(AssistantMessage {
            model: model.into(),
            content: vec![ContentBlock::text("x")],
            usage: Usage {
                input,
                cache_read: cache,
                output,
                total_tokens: input + output,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    #[test]
    fn aggregates_main_delegate_and_extra() {
        let delegate_result = Message::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "delegate".into(),
            content: vec![ContentBlock::text("42")],
            details: Some(serde_json::json!({
                "subAgentModel": "weak-model",
                "subAgentUsage": {"input": 100, "cacheRead": 50, "output": 10, "totalTokens": 110}
            })),
            is_error: false,
            terminate: false,
            timestamp: 0,
        });
        let messages = vec![
            assistant("strong", 1000, 400, 100),
            delegate_result,
            assistant("strong", 2000, 100, 50),
        ];
        let extra = Usage {
            input: 500,
            output: 100,
            total_tokens: 600,
            ..Default::default()
        };
        let r = usage_report(&messages, extra);
        assert_eq!(r.main_usage.input, 3000);
        assert_eq!(r.main_usage.cache_read, 500);
        assert_eq!(r.delegate_usage.input, 100);
        assert_eq!(r.delegate_usage.cache_read, 50);
        assert_eq!(r.compaction_usage.input, 500);
        let total = r.grand_total();
        assert_eq!(total.input, 3600);
        assert_eq!(total.output, 260);
        assert_eq!(r.by_model["strong"].input, 3000);
        assert_eq!(r.by_model["weak-model"].input, 100);
        assert_eq!(r.delegate_calls(), 1);
    }

    #[test]
    fn skips_zero_usage_and_non_delegate_results() {
        let messages = vec![
            Message::user("hi"),
            Message::Assistant(AssistantMessage::default()),
        ];
        let r = usage_report(&messages, Usage::default());
        assert!(r.calls.is_empty());
        assert_eq!(r.grand_total().input, 0);
    }
}
