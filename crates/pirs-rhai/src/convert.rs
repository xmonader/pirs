//! Rhai Dynamic / JSON ↔ agent messages and events.
use rhai::Dynamic;
use serde_json::Value;


pub(crate) fn dynamic_to_messages(d: &Dynamic) -> Vec<pirs_ai::Message> {
    if d.is_unit() {
        return vec![];
    }
    if d.is::<String>() {
        return vec![pirs_ai::Message::user(d.clone().cast::<String>())];
    }
    let value: Value = match rhai::serde::from_dynamic(d) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    match value {
        Value::Array(items) => items.into_iter().filter_map(value_to_message).collect(),
        single => value_to_message(single).into_iter().collect(),
    }
}

pub(crate) fn value_to_message(v: Value) -> Option<pirs_ai::Message> {
    match &v {
        Value::String(s) => Some(pirs_ai::Message::user(s.clone())),
        _ => serde_json::from_value(v).ok(),
    }
}

pub(crate) fn event_to_rhai(event: &pirs_agent::AgentEvent) -> (String, Dynamic) {
    use pirs_agent::AgentEvent as E;
    let mut map = rhai::Map::new();
    let ty = match event {
        E::AgentStart => "agent_start",
        E::AgentEnd { messages } => {
            map.insert("numMessages".into(), (messages.len() as i64).into());
            let report = pirs_agent::usage::usage_report(messages, pirs_ai::Usage::default());
            let total = report.grand_total();
            map.insert("inputTokens".into(), (total.input as i64).into());
            map.insert("cacheReadTokens".into(), (total.cache_read as i64).into());
            map.insert("outputTokens".into(), (total.output as i64).into());
            map.insert("totalTokens".into(), (total.total_tokens as i64).into());
            "agent_end"
        }
        E::TurnStart => "turn_start",
        E::TurnEnd {
            message,
            tool_results,
        } => {
            map.insert("text".into(), message.text().into());
            map.insert("model".into(), message.model.clone().into());
            map.insert(
                "stopReason".into(),
                format!("{:?}", message.stop_reason).into(),
            );
            map.insert("numToolResults".into(), (tool_results.len() as i64).into());
            map.insert("inputTokens".into(), (message.usage.input as i64).into());
            map.insert(
                "cacheReadTokens".into(),
                (message.usage.cache_read as i64).into(),
            );
            map.insert("outputTokens".into(), (message.usage.output as i64).into());
            "turn_end"
        }
        E::MessageStart { message } => {
            map.insert("role".into(), message_role(message).into());
            "message_start"
        }
        E::MessageUpdate { message } => {
            map.insert("text".into(), message.text().into());
            "message_update"
        }
        E::MessageEnd { message } => {
            map.insert("role".into(), message_role(message).into());
            "message_end"
        }
        E::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            map.insert("id".into(), tool_call_id.clone().into());
            map.insert("name".into(), tool_name.clone().into());
            map.insert(
                "args".into(),
                rhai::serde::to_dynamic(args).unwrap_or(Dynamic::UNIT),
            );
            "tool_execution_start"
        }
        E::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            partial,
        } => {
            map.insert("id".into(), tool_call_id.clone().into());
            map.insert("name".into(), tool_name.clone().into());
            map.insert("partial".into(), partial.clone().into());
            "tool_execution_update"
        }
        E::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
        } => {
            map.insert("id".into(), tool_call_id.clone().into());
            map.insert("name".into(), tool_name.clone().into());
            map.insert("isError".into(), result.is_error.into());
            let text: String = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            map.insert("text".into(), text.into());
            "tool_execution_end"
        }
        E::CompactionStart { reason } => {
            map.insert("reason".into(), reason.clone().into());
            "compaction_start"
        }
        E::CompactionEnd {
            reason,
            aborted,
            error_message,
        } => {
            map.insert("reason".into(), reason.clone().into());
            map.insert("aborted".into(), (*aborted).into());
            if let Some(e) = error_message {
                map.insert("errorMessage".into(), e.clone().into());
            }
            "compaction_end"
        }
    };
    (ty.to_string(), Dynamic::from_map(map))
}

pub(crate) fn message_role(m: &pirs_ai::Message) -> &'static str {
    match m {
        pirs_ai::Message::User(_) => "user",
        pirs_ai::Message::Assistant(_) => "assistant",
        pirs_ai::Message::ToolResult(_) => "toolResult",
    }
}
