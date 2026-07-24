//! Runtime feature + session-state awareness for humans and the LLM.
//!
//! Tools alone are not enough: autonomy, packs, strategies, hybrid models,
//! MCP/LSP/graph, and mid-session dials are first-class product surface.
//! This module builds one inspectable snapshot and formats it for:
//! - system prompt (LLM, concise)
//! - `/status` · doctor (human)
//! - `session_state` tool (live re-dump)

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use pirs_agent::AgentTool;

/// Process-wide live snapshot handle (updated when autonomy/model change).
fn live_slot() -> &'static Mutex<Option<RuntimeFeatures>> {
    static SLOT: OnceLock<Mutex<Option<RuntimeFeatures>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Publish the current snapshot so tools / slash commands see live state.
pub fn publish(features: RuntimeFeatures) {
    *live_slot().lock().unwrap() = Some(features);
}

/// Best-effort live copy (re-syncs autonomy fields from env/live permission).
pub fn live() -> Option<RuntimeFeatures> {
    let mut guard = live_slot().lock().unwrap();
    let mut snap = guard.clone()?;
    snap.refresh_live_dials();
    *guard = Some(snap.clone());
    Some(snap)
}

#[derive(Debug, Clone)]
pub struct Capability {
    pub id: String,
    pub available: bool,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeFeatures {
    pub cwd: String,
    pub mode: String,
    pub model: String,
    pub plan_model: Option<String>,
    pub strategy: Option<String>,
    pub profile: Option<String>,
    pub autonomy: String,
    pub permission: String,
    pub approval: String,
    pub agent_profile: String,
    pub weak: bool,
    pub packs: Vec<String>,
    pub slash_commands: Vec<(String, String)>,
    pub tools: Vec<(String, String)>,
    pub capabilities: Vec<Capability>,
    pub notes: Vec<String>,
}

impl RuntimeFeatures {
    /// Re-read dials that change mid-session without rebuilding the agent.
    pub fn refresh_live_dials(&mut self) {
        let perm = pirs_tools::live_permission_mode();
        self.permission = perm.name().to_string();
        self.autonomy = match perm {
            pirs_tools::PermissionMode::ReadOnly => "plan",
            pirs_tools::PermissionMode::WorkspaceWrite => "edit",
            pirs_tools::PermissionMode::DangerFullAccess => "full",
        }
        .to_string();
        if let Ok(p) = std::env::var("PIRS_AGENT_PROFILE") {
            if !p.is_empty() {
                self.agent_profile = p;
            }
        }
        if let Ok(a) = std::env::var("PIRS_AUTONOMY") {
            if !a.is_empty() {
                self.autonomy = a;
            }
        }
    }

    /// Compact block for the system prompt — feature awareness, not tool dump.
    pub fn format_llm(&self) -> String {
        let mut s = String::from(
            "\n## pirs session runtime (inspectable)\n\
             You run inside the pirs harness. Tools are listed above; this section is the \
             **rest of the product surface** — use it. Call the `session_state` tool anytime \
             for a full live dump (autonomy/packs may change mid-session via /plan /act /yolo).\n\n",
        );
        s.push_str(&format!(
            "### Control dials\n\
             - **autonomy**: `{auto}` → permission `{perm}`, profile `{prof}`, approval prompts `{appr}`\n\
             - ladder: `plan` (read-only) · `edit` (writes, no shell) · `full`/`yolo` (all tools)\n\
             - change mid-session: user may run /plan, /edit, /act, /yolo, /autonomy\n",
            auto = self.autonomy,
            perm = self.permission,
            prof = self.agent_profile,
            appr = self.approval,
        ));
        s.push_str(&format!(
            "\n### Models & strategy\n\
             - exec model: `{model}`\n",
            model = self.model
        ));
        if let Some(pm) = &self.plan_model {
            s.push_str(&format!(
                "- **plan-model**: `{pm}` (strong plan / weak exec — planning phases use this)\n"
            ));
        }
        if let Some(st) = &self.strategy {
            s.push_str(&format!(
                "- **strategy**: `{st}` (multi-phase loop; plan phases are read-only tools)\n"
            ));
        } else {
            s.push_str("- strategy: monolithic (single growing loop on exec model)\n");
        }
        if self.weak {
            s.push_str("- **weak mode** on: small steps, verify after edits, no vacuous success\n");
        }

        s.push_str("\n### Capabilities (beyond raw tools)\n");
        for c in &self.capabilities {
            let mark = if c.available { "yes" } else { "no" };
            s.push_str(&format!("- **{}**: {mark}", c.id));
            if !c.note.is_empty() {
                s.push_str(&format!(" — {}", c.note));
            }
            s.push('\n');
        }

        if !self.packs.is_empty() {
            s.push_str(&format!(
                "\n### Extension packs ({n} loaded)\n\
                 Behavior packs can add tools, slash commands, denials, and context pins. \
                 Notable: {preview}{more}\n",
                n = self.packs.len(),
                preview = self
                    .packs
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                more = if self.packs.len() > 8 {
                    format!(", … +{} more (see session_state)", self.packs.len() - 8)
                } else {
                    String::new()
                },
            ));
        }

        if !self.slash_commands.is_empty() {
            s.push_str("\n### User slash commands (not for you to invoke as tools)\n");
            for (name, desc) in self.slash_commands.iter().take(12) {
                s.push_str(&format!("- /{name}: {desc}\n"));
            }
            if self.slash_commands.len() > 12 {
                s.push_str(&format!(
                    "- … +{} more\n",
                    self.slash_commands.len() - 12
                ));
            }
        }

        s.push_str(&format!(
            "\n### Workspace\n- cwd: {}\n- mode: {}\n",
            self.cwd, self.mode
        ));
        for n in &self.notes {
            s.push_str(&format!("- note: {n}\n"));
        }
        s.push_str(
            "\nWhen choosing how to work: prefer specialized tools (code_search, project, \
             edit_block) over bash archaeology; respect autonomy (no bash under plan/edit); \
             use plan-model phases when a strategy is active.\n",
        );
        s
    }

    /// Multi-line dump for humans (`/status`, doctor, CLI).
    pub fn format_human(&self) -> String {
        let mut lines = Vec::new();
        lines.push("── pirs runtime ─────────────────────────────".into());
        lines.push(format!("  cwd            {}", self.cwd));
        lines.push(format!("  mode           {}", self.mode));
        lines.push(format!(
            "  autonomy       {}  (permission={}  profile={}  approval={})",
            self.autonomy, self.permission, self.agent_profile, self.approval
        ));
        lines.push(format!("  model          {}", self.model));
        if let Some(pm) = &self.plan_model {
            lines.push(format!("  plan-model     {pm}"));
        }
        if let Some(st) = &self.strategy {
            lines.push(format!("  strategy       {st}"));
        }
        if let Some(p) = &self.profile {
            lines.push(format!("  cli-profile    {p}"));
        }
        if self.weak {
            lines.push("  weak           true".into());
        }
        lines.push(format!("  tools          {}", self.tools.len()));
        lines.push(format!("  packs          {}", self.packs.len()));
        if !self.packs.is_empty() {
            for p in &self.packs {
                lines.push(format!("    · {p}"));
            }
        }
        lines.push("  capabilities".into());
        for c in &self.capabilities {
            let mark = if c.available { "✓" } else { "·" };
            if c.note.is_empty() {
                lines.push(format!("    {mark} {}", c.id));
            } else {
                lines.push(format!("    {mark} {} — {}", c.id, c.note));
            }
        }
        if !self.slash_commands.is_empty() {
            lines.push(format!("  slash cmds     {}", self.slash_commands.len()));
            for (n, d) in self.slash_commands.iter().take(20) {
                lines.push(format!("    /{n:<16} {d}"));
            }
        }
        for n in &self.notes {
            lines.push(format!("  note           {n}"));
        }
        lines.push("──────────────────────────────────────────────".into());
        lines.join("\n")
    }

    pub fn format_json_pretty(&self) -> String {
        // Hand-rolled light JSON to avoid adding serde derives on every field path.
        let tools: Vec<String> = self
            .tools
            .iter()
            .map(|(n, d)| {
                format!(
                    "{{\"name\":{},\"desc\":{}}}",
                    json_str(n),
                    json_str(d)
                )
            })
            .collect();
        let caps: Vec<String> = self
            .capabilities
            .iter()
            .map(|c| {
                format!(
                    "{{\"id\":{},\"available\":{},\"note\":{}}}",
                    json_str(&c.id),
                    c.available,
                    json_str(&c.note)
                )
            })
            .collect();
        let packs: Vec<String> = self.packs.iter().map(|p| json_str(p)).collect();
        format!(
            "{{\n  \"autonomy\": {},\n  \"permission\": {},\n  \"approval\": {},\n  \
             \"agent_profile\": {},\n  \"model\": {},\n  \"plan_model\": {},\n  \
             \"strategy\": {},\n  \"mode\": {},\n  \"cwd\": {},\n  \"weak\": {},\n  \
             \"packs\": [{}],\n  \"tools\": [{}],\n  \"capabilities\": [{}]\n}}",
            json_str(&self.autonomy),
            json_str(&self.permission),
            json_str(&self.approval),
            json_str(&self.agent_profile),
            json_str(&self.model),
            self.plan_model
                .as_deref()
                .map(json_str)
                .unwrap_or_else(|| "null".into()),
            self.strategy
                .as_deref()
                .map(json_str)
                .unwrap_or_else(|| "null".into()),
            json_str(&self.mode),
            json_str(&self.cwd),
            self.weak,
            packs.join(","),
            tools.join(","),
            caps.join(","),
        )
    }
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn cap(id: &str, available: bool, note: impl Into<String>) -> Capability {
    Capability {
        id: id.into(),
        available,
        note: note.into(),
    }
}

/// Build a snapshot from the live session wiring.
#[allow(clippy::too_many_arguments)]
pub fn collect(
    cwd: &Path,
    mode: &str,
    model: &str,
    plan_model: Option<&str>,
    strategy: Option<&str>,
    profile: Option<&str>,
    approval: &str,
    weak: bool,
    tools: &[Arc<dyn AgentTool>],
    pack_names: &[String],
    slash_commands: &[(String, String)],
    has_graph: bool,
    has_mcp: bool,
    has_lsp: bool,
) -> RuntimeFeatures {
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    let has = |n: &str| tool_names.iter().any(|t| *t == n);

    let perm = pirs_tools::live_permission_mode();
    let autonomy = std::env::var("PIRS_AUTONOMY").unwrap_or_else(|_| match perm {
        pirs_tools::PermissionMode::ReadOnly => "plan".into(),
        pirs_tools::PermissionMode::WorkspaceWrite => "edit".into(),
        pirs_tools::PermissionMode::DangerFullAccess => "full".into(),
    });
    let agent_profile =
        std::env::var("PIRS_AGENT_PROFILE").unwrap_or_else(|_| "default".into());

    let mut capabilities = vec![
        cap(
            "autonomy_ladder",
            true,
            "plan|edit|full via --autonomy / /plan /edit /yolo",
        ),
        cap(
            "code_search",
            has("code_search"),
            "ranked symbol/error/behavior → file:line",
        ),
        cap("code_map", has("code_map"), "symbol/callers/callees/blast"),
        cap(
            "semantic_search",
            has("semantic_search"),
            "embedding search when --semantic + embed model",
        ),
        cap("edit_block", has("edit_block"), "SEARCH/REPLACE blocks"),
        cap(
            "project",
            has("project"),
            "native test/lint/typecheck/build/format",
        ),
        cap("graph", has_graph, "code graph + repo_map sketch"),
        cap("lsp", has_lsp, "language servers on PATH / attached"),
        cap("mcp", has_mcp, "MCP tools from .mcp.json"),
        cap(
            "browser",
            tool_names.iter().any(|t| t.starts_with("browser_")),
            "browser_* tools",
        ),
        cap(
            "computer_use",
            tool_names.iter().any(|t| t.starts_with("computer_")),
            "computer_* tools (PIRS_COMPUTER_USE)",
        ),
        cap(
            "hybrid_plan_model",
            plan_model.is_some(),
            "strong plan / weak exec when strategy multi-phase",
        ),
        cap(
            "strategy_phases",
            strategy.is_some(),
            "plan-exec / plan-critic-exec multi-phase loops",
        ),
        cap("skills", has("skill_list") || has("skill_view"), "progressive skills"),
        cap("memory", has("recall"), "session memory recall"),
        cap("audit", pirs_agent::audit_enabled(), "action audit log"),
        cap(
            "sandbox",
            pack_names.iter().any(|p| p.contains("sandbox")),
            "sandbox pack / docker fallback for bash",
        ),
        cap(
            "multi_root",
            pirs_tools::current_work_context().roots.len() > 1,
            "//name/path work context",
        ),
    ];

    // Sort available first for LLM scanability.
    capabilities.sort_by(|a, b| b.available.cmp(&a.available).then(a.id.cmp(&b.id)));

    let tools_list: Vec<(String, String)> = tools
        .iter()
        .map(|t| {
            let desc = t.description();
            let short: String = desc.chars().take(80).collect();
            (t.name().to_string(), short)
        })
        .collect();

    let mut notes = Vec::new();
    if plan_model.is_some() && strategy.is_none() {
        notes.push(
            "plan-model is set but no multi-phase strategy — pin --strategy plan-exec to use it"
                .into(),
        );
    }
    if autonomy == "edit" {
        notes.push("bash/shell blocked under autonomy edit — need full/yolo for shell".into());
    }
    if autonomy == "plan" {
        notes.push("read-only autonomy — no writes or shell".into());
    }

    RuntimeFeatures {
        cwd: cwd.display().to_string(),
        mode: mode.into(),
        model: model.into(),
        plan_model: plan_model.map(|s| s.to_string()),
        strategy: strategy.map(|s| s.to_string()),
        profile: profile.map(|s| s.to_string()),
        autonomy,
        permission: perm.name().to_string(),
        approval: approval.into(),
        agent_profile,
        weak,
        packs: pack_names.to_vec(),
        slash_commands: slash_commands.to_vec(),
        tools: tools_list,
        capabilities,
        notes,
    }
}

/// Agent tool: dump live session/runtime features.
pub struct SessionStateTool;

impl SessionStateTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SessionStateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AgentTool for SessionStateTool {
    fn name(&self) -> &str {
        "session_state"
    }

    fn description(&self) -> &str {
        "Inspect pirs runtime: autonomy dial, models, strategy, packs, capabilities, tools. \
         Prefer this over guessing harness features. Args: format=text|json (default text)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "format": {
                    "type": "string",
                    "enum": ["text", "json"],
                    "description": "Output format (default text)"
                }
            },
            "additionalProperties": false
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "session_state: dump live autonomy, models, strategy, packs, capabilities \
             (call when unsure what the harness can do)",
        )
    }

    async fn execute(
        &self,
        ctx: pirs_agent::ToolExecContext,
    ) -> anyhow::Result<pirs_agent::ToolOutput> {
        let fmt = ctx
            .args
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("text");
        let Some(mut snap) = live() else {
            return Ok(pirs_agent::ToolOutput::text(
                "session_state: snapshot not published yet (host still starting)",
            ));
        };
        snap.refresh_live_dials();
        let body = if fmt.eq_ignore_ascii_case("json") {
            snap.format_json_pretty()
        } else {
            snap.format_human()
        };
        Ok(pirs_agent::ToolOutput::text(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct DummyTool;
    #[async_trait::async_trait]
    impl AgentTool for DummyTool {
        fn name(&self) -> &str {
            "code_search"
        }
        fn description(&self) -> &str {
            "find code"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn execute(
            &self,
            _: pirs_agent::ToolExecContext,
        ) -> anyhow::Result<pirs_agent::ToolOutput> {
            Ok(pirs_agent::ToolOutput::text("ok"))
        }
    }

    #[test]
    fn llm_section_mentions_autonomy_and_capabilities() {
        pirs_tools::set_live_permission_mode(pirs_tools::PermissionMode::DangerFullAccess);
        std::env::set_var("PIRS_AUTONOMY", "full");
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(DummyTool)];
        let snap = collect(
            Path::new("/tmp/proj"),
            "tui",
            "qwen-plus",
            Some("deepseek-v4-pro"),
            Some("plan-exec"),
            None,
            "yolo",
            false,
            &tools,
            &["bundled:weak-model.rhai".into()],
            &[("stats".into(), "session stats".into())],
            true,
            false,
            true,
        );
        let llm = snap.format_llm();
        assert!(llm.contains("autonomy"), "{llm}");
        assert!(llm.contains("code_search"), "{llm}");
        assert!(llm.contains("plan-model"), "{llm}");
        assert!(llm.contains("session_state"), "{llm}");
        let human = snap.format_human();
        assert!(human.contains("pirs runtime"), "{human}");
        assert!(human.contains("plan-exec"), "{human}");
    }

    #[test]
    fn publish_and_live_roundtrip() {
        pirs_tools::set_live_permission_mode(pirs_tools::PermissionMode::WorkspaceWrite);
        let snap = collect(
            Path::new("."),
            "repl",
            "m",
            None,
            None,
            None,
            "auto",
            false,
            &[],
            &[],
            &[],
            false,
            false,
            false,
        );
        publish(snap);
        let live = live().expect("published");
        assert_eq!(live.mode, "repl");
        assert!(live.format_json_pretty().contains("\"mode\""));
    }
}
