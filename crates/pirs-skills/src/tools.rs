//! Agent tools for progressive skills (list / view / manage).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use serde_json::{json, Value};

use crate::skill::{
    default_skills_dir, find_skill, read_skill_resource, record_usage, write_skill, Skill,
};

/// Env: set to `0`/`false` to deny skill_manage writes (gateway default can set this).
pub const SKILL_WRITE_ENV: &str = "PIRS_SKILL_WRITE";

pub fn skill_write_allowed() -> bool {
    for key in [SKILL_WRITE_ENV, "PIRS_CLAW_SKILL_WRITE"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_ascii_lowercase();
            return !(v == "0" || v == "false" || v == "no" || v == "off");
        }
    }
    // Default allow on CLI; gateway should set PIRS_SKILL_WRITE=0.
    true
}

pub fn skill_tools(skills: Arc<Vec<Skill>>, allow_manage: bool) -> Vec<Arc<dyn AgentTool>> {
    let mut out: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(SkillListTool {
            skills: skills.clone(),
        }),
        Arc::new(SkillViewTool {
            skills: skills.clone(),
        }),
    ];
    if allow_manage && skill_write_allowed() {
        out.push(Arc::new(SkillManageTool { skills }));
    }
    out
}

struct SkillListTool {
    skills: Arc<Vec<Skill>>,
}

#[async_trait]
impl AgentTool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }

    fn description(&self) -> &str {
        "List installed skills (name + description). Use skill_view to load full instructions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if self.skills.is_empty() {
            return Ok(ToolOutput::text("(no skills installed under ~/.pirs/skills)"));
        }
        let mut s = String::new();
        for sk in self.skills.iter() {
            s.push_str(&format!("{} — {}\n", sk.name, sk.description));
        }
        Ok(ToolOutput::text(s))
    }
}

struct SkillViewTool {
    skills: Arc<Vec<Skill>>,
}

#[async_trait]
impl AgentTool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load full skill instructions (SKILL.md body) or a relative resource under the skill \
         (e.g. references/FOO.md). Prefer this over guessing procedures."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name from skill_list"
                },
                "path": {
                    "type": "string",
                    "description": "Optional relative path under the skill root (default: full SKILL.md body)"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let name = ctx
            .args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("name required"))?;
        let sk = find_skill(&self.skills, name)
            .ok_or_else(|| anyhow::anyhow!("unknown skill {name:?}"))?;
        record_usage(&sk.name);
        if let Some(rel) = ctx.args.get("path").and_then(|v| v.as_str()) {
            if !rel.is_empty() {
                let text = read_skill_resource(sk, rel)?;
                return Ok(ToolOutput::text(text));
            }
        }
        Ok(ToolOutput::text(format!(
            "# {}\n\n{}\n\n{}",
            sk.name, sk.description, sk.body
        )))
    }
}

struct SkillManageTool {
    skills: Arc<Vec<Skill>>,
}

#[async_trait]
impl AgentTool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Create or update a skill under ~/.pirs/skills (agentskills.io SKILL.md). \
         Use after learning a reusable procedure. Requires name (kebab-case), description, content body."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update"],
                    "description": "create new or update existing"
                },
                "name": {
                    "type": "string",
                    "description": "kebab-case skill name (agentskills.io)"
                },
                "description": {
                    "type": "string",
                    "description": "When to use this skill (1–1024 chars)"
                },
                "content": {
                    "type": "string",
                    "description": "Markdown body (procedure, pitfalls)"
                }
            },
            "required": ["action", "name", "description", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !skill_write_allowed() {
            anyhow::bail!("skill writes disabled (PIRS_CLAW_SKILL_WRITE=0)");
        }
        let action = ctx
            .args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("create");
        let name = ctx
            .args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("name required"))?;
        let description = ctx
            .args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = ctx
            .args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if action == "update" && find_skill(&self.skills, name).is_none() {
            // Still allow write (create-on-update) but note it.
        }
        let dest = write_skill(&default_skills_dir(), name, description, content)?;
        Ok(ToolOutput::text(format!(
            "skill {action}d → {}",
            dest.display()
        )))
    }
}

/// Root for tests.
#[allow(dead_code)]
pub fn skills_dir_override() -> Option<PathBuf> {
    None
}
