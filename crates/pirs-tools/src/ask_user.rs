//! Mid-turn clarification tool (Vibe `ask_user_question` class).
//!
//! The agent poses a structured question with options; the user (or a test
//! injector) answers; the tool result content includes the selected option
//! label(s) so the model can continue.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One choice shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct AskOption {
    /// Stable id (e.g. "a", "yes").
    pub id: String,
    /// Human-readable label.
    pub label: String,
}

/// Arguments for `ask_user`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct AskUserArgs {
    /// Question text.
    pub question: String,
    /// Options to choose from (at least one).
    pub options: Vec<AskOption>,
    /// Allow multiple selections (comma-separated ids).
    #[serde(default)]
    pub multi_select: bool,
}

/// Pure parse of a user answer string against options.
///
/// Accepts option ids and (case-insensitive) full labels. Multi-select:
/// comma/space separated ids.
pub fn resolve_answer(
    args: &AskUserArgs,
    answer: &str,
) -> Result<ResolvedAnswer, String> {
    if args.options.is_empty() {
        return Err("ask_user requires at least one option".into());
    }
    let answer = answer.trim();
    if answer.is_empty() {
        return Err("empty answer".into());
    }
    if args.multi_select {
        let parts: Vec<&str> = answer
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err("empty multi-select answer".into());
        }
        let mut selected = Vec::new();
        for p in parts {
            selected.push(match_one(args, p)?);
        }
        Ok(ResolvedAnswer { selected })
    } else {
        Ok(ResolvedAnswer {
            selected: vec![match_one(args, answer)?],
        })
    }
}

fn match_one(args: &AskUserArgs, token: &str) -> Result<AskOption, String> {
    let t = token.trim();
    if let Some(o) = args.options.iter().find(|o| o.id == t) {
        return Ok(o.clone());
    }
    let tl = t.to_ascii_lowercase();
    if let Some(o) = args
        .options
        .iter()
        .find(|o| o.label.to_ascii_lowercase() == tl)
    {
        return Ok(o.clone());
    }
    // Numeric 1-based index
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 && n <= args.options.len() {
            return Ok(args.options[n - 1].clone());
        }
    }
    Err(format!(
        "unknown option {t:?}; valid ids: {}",
        args.options
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAnswer {
    pub selected: Vec<AskOption>,
}

impl ResolvedAnswer {
    /// Model-facing tool result body (must include labels for continuation).
    pub fn tool_content(&self, question: &str) -> String {
        let labels: Vec<&str> = self.selected.iter().map(|o| o.label.as_str()).collect();
        let ids: Vec<&str> = self.selected.iter().map(|o| o.id.as_str()).collect();
        format!(
            "User answered question: {question}\n\
             selected_ids: {}\n\
             selected_labels: {}\n\
             choice: {}",
            ids.join(", "),
            labels.join(", "),
            labels.join("; ")
        )
    }
}

/// How answers are obtained (stdin in CLI; queue in tests).
pub type AnswerSource = Arc<dyn Fn(&AskUserArgs) -> Result<String, String> + Send + Sync>;

/// Default interactive source: print options, read a line from stdin.
pub fn stdin_answer_source() -> AnswerSource {
    Arc::new(|args: &AskUserArgs| {
        eprintln!("\n[ask_user] {}", args.question);
        for (i, o) in args.options.iter().enumerate() {
            eprintln!("  {}. [{}] {}", i + 1, o.id, o.label);
        }
        if args.multi_select {
            eprint!("select one or more ids (comma-separated): ");
        } else {
            eprint!("select option id: ");
        }
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        Ok(line.trim().to_string())
    })
}

/// Fixed answer queue for tests / non-interactive injection.
pub fn queue_answer_source(answers: Vec<String>) -> AnswerSource {
    let q = Arc::new(Mutex::new(VecDeque::from(answers)));
    Arc::new(move |_args: &AskUserArgs| {
        let mut g = q.lock().map_err(|e| e.to_string())?;
        g.pop_front()
            .ok_or_else(|| "ask_user: no more injected answers".into())
    })
}

/// Env override for one-shot automation: `PIRS_ASK_USER_ANSWER=id`.
pub fn env_or_stdin_answer_source() -> AnswerSource {
    if let Ok(a) = std::env::var("PIRS_ASK_USER_ANSWER") {
        if !a.trim().is_empty() {
            return queue_answer_source(vec![a]);
        }
    }
    stdin_answer_source()
}

pub struct AskUserTool {
    source: AnswerSource,
}

impl AskUserTool {
    pub fn new(source: AnswerSource) -> Self {
        Self { source }
    }

    pub fn default_interactive() -> Self {
        Self::new(env_or_stdin_answer_source())
    }
}

#[async_trait]
impl AgentTool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a structured multiple-choice question and wait for their answer. \
         Use when you need clarification before proceeding. Options must have id + label."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AskUserArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("ask_user: mid-turn multiple-choice clarification")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: AskUserArgs = serde_json::from_value(ctx.args)
            .map_err(|e| anyhow::anyhow!("invalid ask_user args: {e}"))?;
        if args.options.is_empty() {
            anyhow::bail!("ask_user requires options");
        }
        let raw = (self.source)(&args).map_err(|e| anyhow::anyhow!(e))?;
        let resolved =
            resolve_answer(&args, &raw).map_err(|e| anyhow::anyhow!("ask_user: {e}"))?;
        Ok(ToolOutput::text(resolved.tool_content(&args.question)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_agent::ToolExecContext;
    use tokio_util::sync::CancellationToken;

    fn sample_args() -> AskUserArgs {
        AskUserArgs {
            question: "Which approach?".into(),
            options: vec![
                AskOption {
                    id: "a".into(),
                    label: "Refactor module".into(),
                },
                AskOption {
                    id: "b".into(),
                    label: "Rewrite from scratch".into(),
                },
            ],
            multi_select: false,
        }
    }

    #[test]
    fn resolve_by_id_includes_label_in_content() {
        let args = sample_args();
        let r = resolve_answer(&args, "a").unwrap();
        let content = r.tool_content(&args.question);
        assert!(content.contains("Refactor module"), "{content}");
        assert!(content.contains("selected_ids: a"), "{content}");
        assert!(content.contains("selected_labels: Refactor module"), "{content}");
    }

    #[test]
    fn resolve_by_label_and_index() {
        let args = sample_args();
        assert_eq!(
            resolve_answer(&args, "Rewrite from scratch")
                .unwrap()
                .selected[0]
                .id,
            "b"
        );
        assert_eq!(resolve_answer(&args, "2").unwrap().selected[0].id, "b");
    }

    #[test]
    fn multi_select() {
        let mut args = sample_args();
        args.multi_select = true;
        let r = resolve_answer(&args, "a,b").unwrap();
        assert_eq!(r.selected.len(), 2);
        let c = r.tool_content(&args.question);
        assert!(c.contains("Refactor module"));
        assert!(c.contains("Rewrite from scratch"));
    }

    #[tokio::test]
    async fn tool_execute_uses_injected_answer() {
        let tool = AskUserTool::new(queue_answer_source(vec!["b".into()]));
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "1".into(),
                args: serde_json::to_value(sample_args()).unwrap(),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.model_text().unwrap_or("");
        assert!(text.contains("Rewrite from scratch"), "{text}");
        assert!(text.contains("selected_ids: b"), "{text}");
    }
}
