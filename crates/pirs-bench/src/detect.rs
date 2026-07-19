//! Runner discovery via Rhai detectors.
//!
//! Detectors are per-ecosystem heuristics — exactly the code that changes often
//! and is safe to get wrong (a bad `RunnerSpec` fails its probe). So they live
//! in Rhai, not Rust. The host exposes only **read-only, root-relative** file
//! primitives; a detector can inspect the repo to produce candidate specs but
//! cannot execute anything. Actual probing/running happens Rust-side afterward,
//! under the sandbox and timeout.
//!
//! **Bench-mode isolation is structural:** the host loads scripts only from a
//! trusted directory handed to it, never from the repo under test, and the
//! registered functions cannot run commands or write files. There is no path by
//! which a task repo's own `.rhai` influences detection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rhai::{Array, Engine, Scope, AST};

use crate::types::RunnerSpec;

/// A sandbox of loaded detector scripts over a read-only file API.
pub struct DetectorHost {
    engine: Engine,
    detectors: Vec<(String, AST)>,
    /// The repo root the read-only primitives resolve against, set per `detect`.
    root: Arc<Mutex<PathBuf>>,
}

impl Default for DetectorHost {
    fn default() -> Self {
        Self::new()
    }
}

impl DetectorHost {
    pub fn new() -> Self {
        let root = Arc::new(Mutex::new(PathBuf::new()));
        let mut engine = Engine::new();

        // file_read(rel) -> String — "" if missing or outside root.
        {
            let root = Arc::clone(&root);
            engine.register_fn("file_read", move |rel: &str| -> String {
                resolve(&root, rel)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or_default()
            });
        }
        // path_exists(rel) -> bool
        {
            let root = Arc::clone(&root);
            engine.register_fn("path_exists", move |rel: &str| -> bool {
                resolve(&root, rel).map(|p| p.exists()).unwrap_or(false)
            });
        }
        // read_dir(rel) -> [names] — entry file names, "" list if missing.
        {
            let root = Arc::clone(&root);
            engine.register_fn("read_dir", move |rel: &str| -> Array {
                resolve(&root, rel)
                    .and_then(|p| std::fs::read_dir(p).ok())
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned().into())
                            .collect()
                    })
                    .unwrap_or_default()
            });
        }

        DetectorHost { engine, detectors: Vec::new(), root }
    }

    /// Compile and register a detector script (which must define `fn detect()`
    /// returning an array of spec maps). `name` is used only in diagnostics.
    pub fn load_detector(&mut self, name: &str, source: &str) -> anyhow::Result<()> {
        let ast = self
            .engine
            .compile(source)
            .with_context(|| format!("compile detector '{name}'"))?;
        self.detectors.push((name.to_string(), ast));
        Ok(())
    }

    /// Load every `*.rhai` in a **trusted** directory. Never point this at the
    /// repo under test — bundled/home detectors only.
    pub fn load_dir(&mut self, dir: &Path) -> anyhow::Result<usize> {
        let mut n = 0;
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("read detector dir {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rhai") {
                let src = std::fs::read_to_string(&path)?;
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                self.load_detector(name, &src)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Run every detector against `repo_root` and collect the candidate specs
    /// they emit, in load order (trust order). A detector that errors or emits a
    /// malformed spec is logged and skipped — never fatal.
    pub fn detect(&self, repo_root: &Path) -> Vec<RunnerSpec> {
        *self.root.lock().unwrap() = repo_root.to_path_buf();
        let mut specs = Vec::new();
        for (name, ast) in &self.detectors {
            let mut scope = Scope::new();
            let arr: Array = match self.engine.call_fn(&mut scope, ast, "detect", ()) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("detector '{name}' failed: {e}");
                    continue;
                }
            };
            for item in arr {
                match rhai::serde::from_dynamic::<RunnerSpec>(&item) {
                    Ok(spec) => specs.push(spec),
                    Err(e) => tracing::warn!("detector '{name}' produced an invalid spec: {e}"),
                }
            }
        }
        specs
    }
}

/// Resolve a detector-supplied relative path against the current root, rejecting
/// absolute paths and `..` escapes — defense in depth even for trusted scripts.
fn resolve(root: &Arc<Mutex<PathBuf>>, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    Some(root.lock().unwrap().join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO_DETECTOR: &str = r#"
        fn detect() {
            let specs = [];
            if path_exists("go.mod") {
                specs.push(#{
                    framework: "go",
                    install: [],
                    list_cmd: "go test -list \".*\" ./...",
                    test_cmd: "gotestsum --junitfile {junit} -- -run {tests} ./...",
                    timeout_secs: 600,
                    parallel: true,
                });
            }
            specs
        }
    "#;

    #[test]
    fn detector_emits_spec_when_marker_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        let mut host = DetectorHost::new();
        host.load_detector("go", GO_DETECTOR).unwrap();
        let specs = host.detect(dir.path());
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].framework, "go");
        assert!(specs[0].test_cmd.contains("{junit}"));
        assert_eq!(specs[0].timeout_secs, 600);
    }

    #[test]
    fn detector_emits_nothing_without_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = DetectorHost::new();
        host.load_detector("go", GO_DETECTOR).unwrap();
        assert!(host.detect(dir.path()).is_empty());
    }

    #[test]
    fn host_file_api_is_root_confined() {
        // A detector cannot read outside the root via absolute path or `..`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("in.txt"), "inside").unwrap();
        let mut host = DetectorHost::new();
        host.load_detector(
            "probe",
            r#"fn detect() {
                let out = [];
                out.push(#{ framework: file_read("in.txt"), install: [], list_cmd: "",
                            test_cmd: file_read("../../etc/hostname"), timeout_secs: 1, parallel: false });
                out
            }"#,
        )
        .unwrap();
        let specs = host.detect(dir.path());
        assert_eq!(specs[0].framework, "inside");
        assert_eq!(specs[0].test_cmd, "", "escape path must resolve to empty");
    }
}
