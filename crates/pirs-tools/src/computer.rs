//! Thin computer-use tools (Linux): screenshot + xdotool click/type.
//! Fail closed with clear errors when host tools are missing.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn cu_enabled() -> bool {
    matches!(
        std::env::var("PIRS_COMPUTER_USE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

#[derive(Deserialize, JsonSchema)]
struct ShotArgs {
    /// Output path under cwd (default .pirs/screen.png).
    #[serde(default)]
    path: Option<String>,
}

pub struct ComputerScreenshotTool {
    cwd: PathBuf,
}

impl ComputerScreenshotTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for ComputerScreenshotTool {
    fn name(&self) -> &str {
        "computer_screenshot"
    }

    fn description(&self) -> &str {
        "Capture the local desktop screenshot (requires PIRS_COMPUTER_USE=1 and scrot/import/gnome-screenshot). \
         Analyze with vision_describe."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ShotArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("computer_screenshot: grab the desktop (opt-in)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !cu_enabled() {
            anyhow::bail!(
                "computer use disabled — set PIRS_COMPUTER_USE=1 to allow desktop control"
            );
        }
        let args: ShotArgs = serde_json::from_value(ctx.args)?;
        let rel = args.path.unwrap_or_else(|| ".pirs/screen.png".into());
        let out = crate::paths::resolve_contained(&self.cwd, &rel)?;
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        let out_s = out.to_string_lossy().to_string();
        let ok = if let Some(bin) = which("scrot") {
            Command::new(bin).args(["-o", &out_s]).status()?.success()
        } else if let Some(bin) = which("gnome-screenshot") {
            Command::new(bin)
                .args(["-f", &out_s])
                .status()?
                .success()
        } else if let Some(bin) = which("import") {
            // ImageMagick
            Command::new(bin)
                .args(["-window", "root", &out_s])
                .status()?
                .success()
        } else {
            anyhow::bail!("no screenshot tool (install scrot, gnome-screenshot, or imagemagick)");
        };
        if !ok || !out.is_file() {
            anyhow::bail!("screenshot command failed");
        }
        Ok(ToolOutput::text(format!(
            "Desktop screenshot saved to {}",
            out.display()
        )))
    }
}

#[derive(Deserialize, JsonSchema)]
struct ClickArgs {
    x: i32,
    y: i32,
    /// left | right | middle
    #[serde(default)]
    button: Option<String>,
}

pub struct ComputerClickTool;

#[async_trait]
impl AgentTool for ComputerClickTool {
    fn name(&self) -> &str {
        "computer_click"
    }

    fn description(&self) -> &str {
        "Click at screen coordinates via xdotool (requires PIRS_COMPUTER_USE=1)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ClickArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("computer_click: click x,y on desktop")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !cu_enabled() {
            anyhow::bail!("computer use disabled — set PIRS_COMPUTER_USE=1");
        }
        let Some(xdotool) = which("xdotool") else {
            anyhow::bail!("xdotool not installed");
        };
        let args: ClickArgs = serde_json::from_value(ctx.args)?;
        let btn = match args.button.as_deref().unwrap_or("left") {
            "right" => "3",
            "middle" => "2",
            _ => "1",
        };
        let status = Command::new(xdotool)
            .args([
                "mousemove",
                &args.x.to_string(),
                &args.y.to_string(),
                "click",
                btn,
            ])
            .status()?;
        if !status.success() {
            anyhow::bail!("xdotool click failed");
        }
        Ok(ToolOutput::text(format!(
            "clicked ({}, {}) button={btn}",
            args.x, args.y
        )))
    }
}

#[derive(Deserialize, JsonSchema)]
struct TypeArgs {
    text: String,
}

pub struct ComputerTypeTool;

#[async_trait]
impl AgentTool for ComputerTypeTool {
    fn name(&self) -> &str {
        "computer_type"
    }

    fn description(&self) -> &str {
        "Type text via xdotool (requires PIRS_COMPUTER_USE=1). Prefer short strings."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TypeArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("computer_type: type text on desktop")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !cu_enabled() {
            anyhow::bail!("computer use disabled — set PIRS_COMPUTER_USE=1");
        }
        let Some(xdotool) = which("xdotool") else {
            anyhow::bail!("xdotool not installed");
        };
        let args: TypeArgs = serde_json::from_value(ctx.args)?;
        if args.text.len() > 500 {
            anyhow::bail!("text too long (max 500 chars)");
        }
        let status = Command::new(xdotool)
            .args(["type", "--clearmodifiers", "--", &args.text])
            .status()?;
        if !status.success() {
            anyhow::bail!("xdotool type failed");
        }
        Ok(ToolOutput::text(format!("typed {} chars", args.text.len())))
    }
}

#[derive(Deserialize, JsonSchema)]
struct KeyArgs {
    /// Key name for xdotool (e.g. Return, Escape, ctrl+c, Tab)
    key: String,
}

pub struct ComputerKeyTool;

#[async_trait]
impl AgentTool for ComputerKeyTool {
    fn name(&self) -> &str {
        "computer_key"
    }

    fn description(&self) -> &str {
        "Press a key or key-chord via xdotool (requires PIRS_COMPUTER_USE=1). \
         Examples: Return, Escape, Tab, ctrl+s, alt+Tab."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(KeyArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("computer_key: press key/chord on desktop")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !cu_enabled() {
            anyhow::bail!("computer use disabled — set PIRS_COMPUTER_USE=1");
        }
        let Some(xdotool) = which("xdotool") else {
            anyhow::bail!("xdotool not installed");
        };
        let args: KeyArgs = serde_json::from_value(ctx.args)?;
        if args.key.len() > 64 || args.key.chars().any(|c| c.is_control()) {
            anyhow::bail!("invalid key");
        }
        let status = Command::new(xdotool)
            .args(["key", "--clearmodifiers", &args.key])
            .status()?;
        if !status.success() {
            anyhow::bail!("xdotool key failed");
        }
        Ok(ToolOutput::text(format!("pressed key {}", args.key)))
    }
}

#[derive(Deserialize, JsonSchema)]
struct MoveArgs {
    x: i32,
    y: i32,
}

pub struct ComputerMoveTool;

#[async_trait]
impl AgentTool for ComputerMoveTool {
    fn name(&self) -> &str {
        "computer_move"
    }

    fn description(&self) -> &str {
        "Move mouse pointer via xdotool (requires PIRS_COMPUTER_USE=1)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(MoveArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("computer_move: move mouse to x,y")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !cu_enabled() {
            anyhow::bail!("computer use disabled — set PIRS_COMPUTER_USE=1");
        }
        let Some(xdotool) = which("xdotool") else {
            anyhow::bail!("xdotool not installed");
        };
        let args: MoveArgs = serde_json::from_value(ctx.args)?;
        let status = Command::new(xdotool)
            .args(["mousemove", &args.x.to_string(), &args.y.to_string()])
            .status()?;
        if !status.success() {
            anyhow::bail!("xdotool mousemove failed");
        }
        Ok(ToolOutput::text(format!("moved to ({}, {})", args.x, args.y)))
    }
}

pub fn computer_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ComputerScreenshotTool::new(cwd)),
        Arc::new(ComputerClickTool),
        Arc::new(ComputerTypeTool),
        Arc::new(ComputerKeyTool),
        Arc::new(ComputerMoveTool),
    ]
}
