use std::path::PathBuf;
use std::sync::Arc;

use pirs_agent::AgentTool;

pub mod bash;
pub mod sandbox;
pub mod edit;
pub mod job_tools;
pub mod filelock;
pub mod find;
pub mod grep;
pub mod ls;
pub mod paths;
pub mod read;
pub mod truncate;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;

pub fn default_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(BashTool::new(cwd.clone())),
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(EditTool::new(cwd.clone())),
        Arc::new(WriteTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd)),
    ];
    for t in job_tools::tools() {
        tools.push(std::sync::Arc::from(t));
    }
    tools
}
