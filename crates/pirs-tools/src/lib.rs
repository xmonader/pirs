use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use pirs_agent::AgentTool;

/// A process-private scratch directory (mode 0700, unpredictable name) for
/// transient job and command-output logs. Writing these under a private dir
/// instead of directly in a world-writable `/tmp` defeats two attacks a
/// predictable path enables on a multi-user host: symlink pre-creation (an
/// attacker cannot enter or pre-seed a 0700 dir owned by us, so `File::create`
/// can't be redirected onto a victim file) and info-leak of command output
/// (which may contain secrets) to other local users.
pub fn scratch_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        // tempfile creates the directory with 0700 and a random name, failing
        // rather than reusing an existing path. keep() persists it for the
        // process lifetime. Fall back to temp_dir only if creation fails, to
        // preserve the never-panic contract of the callers.
        tempfile::Builder::new()
            .prefix("pirs-")
            .tempdir()
            .map(|d| {
                let path = d.keep();
                // tempfile's default dir mode honors the umask (often 0755);
                // force owner-only so other local users cannot read the logs
                // or plant symlinks inside.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
                }
                path
            })
            .unwrap_or_else(|_| std::env::temp_dir())
    })
}

pub mod ask_user;
pub mod audit_tool;
pub mod bash;
pub mod browser;
#[cfg(feature = "cdp")]
pub mod browser_cdp;
pub mod computer;
pub mod doctor;
pub mod edit;
pub mod edit_block;
pub mod filelock;
pub mod find;
pub mod fleet;
pub mod grep;
pub mod job_tools;
pub mod ls;
pub mod paths;
pub mod pr_tools;
pub mod project;
pub mod read;
pub mod recall;
pub mod research;
pub mod run_tests;
pub mod safety_profile;
pub mod sandbox;
pub mod session_rewind;
pub mod todo_tool;
pub mod truncate;
pub mod vision;
pub mod web;
pub mod worktree;
pub mod write;

pub use ask_user::{
    env_or_stdin_answer_source, queue_answer_source, resolve_answer, AskUserArgs, AskUserTool,
    ResolvedAnswer,
};
pub use bash::BashTool;
pub use edit::EditTool;
pub use edit_block::EditBlockTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use recall::RecallTool;
pub use project::{
    detect_native_checks, detect_profile, detect_toolchain_label, discover_packages,
    ProjectProfile, ProjectTool,
};
pub use run_tests::RunTestsTool;
pub use safety_profile::{
    profile_deny_reason, profile_hook, profile_skips_approval, SafetyProfile,
};
pub use todo_tool::{TodoStore, TodoTool};
pub use browser::browser_tools;
#[cfg(feature = "cdp")]
pub use browser_cdp::cdp_tools;
pub use computer::computer_tools;
pub use vision::vision_tools;
pub use doctor::{doctor_report, DoctorTool};
pub use fleet::fleet_tools;
pub use pr_tools::pr_tools;
pub use research::research_tools;
pub use session_rewind::{host_undo, snapshot as rewind_snapshot, RewindTool};
pub use web::life_tools;
pub use worktree::{
    bind_session_worktree, ensure_worktree, git_repo_root, sanitize_worktree_name,
    worktree_path_for, WorktreeSession,
};
pub use write::WriteTool;

/// Session-aware tools (ask_user, todo) plus coding defaults.
///
/// `session_dir` holds durable todo state (defaults to `{cwd}/.pirs` when None).
pub fn default_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    default_tools_with_session(cwd, None)
}

pub fn default_tools_with_session(
    cwd: PathBuf,
    session_dir: Option<PathBuf>,
) -> Vec<Arc<dyn AgentTool>> {
    let sess = session_dir.unwrap_or_else(|| cwd.join(".pirs"));
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(BashTool::new(cwd.clone())),
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(EditTool::new(cwd.clone())),
        Arc::new(EditBlockTool::new(cwd.clone())),
        Arc::new(WriteTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd.clone())),
        Arc::new(ProjectTool::new(cwd.clone())),
        Arc::new(RunTestsTool::new(cwd.clone())),
        Arc::new(RecallTool::default()),
        Arc::new(AskUserTool::default_interactive()),
    ];
    if let Ok(todo) = TodoTool::open_at(&sess) {
        tools.push(Arc::new(todo));
    }
    tools.push(Arc::new(audit_tool::AuditTailTool));
    tools.push(Arc::new(DoctorTool::new(cwd.clone())));
    tools.push(Arc::new(RewindTool));
    tools.extend(pr_tools(cwd.clone()));
    tools.extend(research_tools(cwd.clone()));
    tools.extend(fleet_tools());
    // Shared life tools (harness + claw): web_fetch / web_search.
    tools.extend(web::life_tools(false));
    // Browser + vision (always available; computer-use opt-in via env).
    tools.extend(browser::browser_tools(cwd.clone()));
    #[cfg(feature = "cdp")]
    tools.extend(browser_cdp::cdp_tools(cwd.clone()));
    tools.extend(vision::vision_tools(cwd.clone()));
    tools.extend(computer::computer_tools(cwd));
    for t in job_tools::tools() {
        tools.push(std::sync::Arc::from(t));
    }
    tools
}

#[cfg(all(test, unix))]
mod scratch_tests {
    use super::scratch_dir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn scratch_dir_is_private_and_hosts_job_logs() {
        let dir = scratch_dir();
        assert!(dir.is_dir(), "scratch dir must exist");
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "scratch dir must be owner-only, got {mode:o}");
        // Job output paths must live inside the private dir, not loose in /tmp.
        let job = crate::job_tools::bash_job_output_path(1);
        assert_eq!(job.parent(), Some(dir), "job log must be under scratch dir");
    }
}
