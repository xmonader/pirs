//! Soulforge-style project toolchain profile.
//!
//! Detects test / lint / typecheck / build / format / run commands from marker
//! files (and package.json scripts when present). Exposed as the `project`
//! agent tool and as a system-prompt summary so models stop guessing shells.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use pirs_agent::tool::{AgentTool, ExecutionMode, ToolExecContext, ToolOutput};
use serde_json::{json, Value};

/// Detected commands for a project root (Soulforge `ProjectProfile` shape).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectProfile {
    pub toolchain: Option<String>,
    pub test: Option<String>,
    pub build: Option<String>,
    pub lint: Option<String>,
    pub typecheck: Option<String>,
    pub format: Option<String>,
    pub run: Option<String>,
}

impl ProjectProfile {
    pub fn is_empty(&self) -> bool {
        self.test.is_none()
            && self.build.is_none()
            && self.lint.is_none()
            && self.typecheck.is_none()
            && self.format.is_none()
            && self.run.is_none()
    }

    pub fn command_for(&self, action: &str) -> Option<&str> {
        match action {
            "test" => self.test.as_deref(),
            "build" => self.build.as_deref(),
            "lint" => self.lint.as_deref(),
            "typecheck" => self.typecheck.as_deref(),
            "format" => self.format.as_deref(),
            "run" => self.run.as_deref(),
            _ => None,
        }
    }

    /// Compact system-prompt block (Soulforge-style injection).
    pub fn prompt_section(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut parts = Vec::new();
        if let Some(t) = &self.toolchain {
            parts.push(format!("toolchain: {t}"));
        }
        for (label, cmd) in [
            ("lint", &self.lint),
            ("typecheck", &self.typecheck),
            ("test", &self.test),
            ("build", &self.build),
            ("format", &self.format),
        ] {
            if let Some(c) = cmd {
                parts.push(format!("{label}: `{c}`"));
            }
        }
        format!(
            "\n\n## Project commands (auto-detected)\n\
             Prefer the `project` tool with these actions instead of inventing shell commands.\n\
             {}\n",
            parts.join(" · ")
        )
    }
}

fn has(root: &Path, f: &str) -> bool {
    root.join(f).exists()
}

fn has_ext(root: &Path, ext: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(root) else {
        return false;
    };
    rd.flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .ends_with(ext)
    })
}

fn read_package_scripts(root: &Path) -> std::collections::HashMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(root.join("package.json")) else {
        return Default::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Default::default();
    };
    let mut out = std::collections::HashMap::new();
    if let Some(obj) = v.get("scripts").and_then(|s| s.as_object()) {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

fn detect_js_pm(root: &Path) -> &'static str {
    // Walk up a few parents for monorepo lockfiles.
    let mut dir = root.to_path_buf();
    for _ in 0..5 {
        if dir.join("pnpm-lock.yaml").exists() {
            return "pnpm";
        }
        if dir.join("yarn.lock").exists() {
            return "yarn";
        }
        if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
            return "bun";
        }
        if dir.join("package-lock.json").exists() {
            return "npm";
        }
        if !dir.pop() {
            break;
        }
    }
    "npm"
}

fn detect_js_linter(root: &Path, runner: &str) -> Option<String> {
    let run = if runner.is_empty() {
        String::new()
    } else {
        format!("{runner} ")
    };
    if has(root, "biome.json") || has(root, "biome.jsonc") {
        return Some(format!("{run}biome check ."));
    }
    if has(root, "oxlintrc.json") || has(root, ".oxlintrc.json") {
        return Some(format!("{run}oxlint ."));
    }
    if has(root, "eslint.config.js")
        || has(root, "eslint.config.mjs")
        || has(root, "eslint.config.ts")
        || has(root, ".eslintrc")
        || has(root, ".eslintrc.js")
        || has(root, ".eslintrc.json")
        || has(root, ".eslintrc.yml")
    {
        return Some(format!("{run}eslint ."));
    }
    None
}

fn detect_js_formatter(root: &Path, runner: &str) -> Option<String> {
    let run = if runner.is_empty() {
        String::new()
    } else {
        format!("{runner} ")
    };
    if has(root, "biome.json") || has(root, "biome.jsonc") {
        return Some(format!("{run}biome format --write"));
    }
    if has(root, "dprint.json") || has(root, "dprint.jsonc") {
        return Some(format!("{run}dprint fmt"));
    }
    if has(root, ".prettierrc")
        || has(root, ".prettierrc.js")
        || has(root, ".prettierrc.json")
        || has(root, ".prettierrc.yml")
        || has(root, "prettier.config.js")
        || has(root, "prettier.config.cjs")
        || has(root, "prettier.config.mjs")
    {
        return Some(format!("{run}prettier --write ."));
    }
    None
}

/// Detect toolchain profile for `cwd` (first matching ecosystem wins).
pub fn detect_profile(cwd: &Path) -> ProjectProfile {
    let mut p = ProjectProfile::default();
    let scripts = read_package_scripts(cwd);

    // Bun
    if has(cwd, "bun.lock") || has(cwd, "bun.lockb") {
        p.toolchain = Some("bun".into());
        p.test = Some(
            scripts
                .get("test")
                .map(|_| "bun run test".into())
                .unwrap_or_else(|| "bun test".into()),
        );
        p.build = scripts.get("build").map(|_| "bun run build".into());
        p.lint = scripts
            .get("lint")
            .map(|_| "bun run lint".into())
            .or_else(|| detect_js_linter(cwd, "bunx"));
        p.typecheck = scripts
            .get("typecheck")
            .map(|_| "bun run typecheck".into())
            .or_else(|| {
                if has(cwd, "tsconfig.json") {
                    Some("bunx tsc --noEmit".into())
                } else {
                    None
                }
            });
        p.run = scripts
            .get("dev")
            .map(|_| "bun run dev".into())
            .or_else(|| scripts.get("start").map(|_| "bun run start".into()));
        p.format = scripts
            .get("format")
            .map(|_| "bun run format".into())
            .or_else(|| detect_js_formatter(cwd, "bunx"));
        return p;
    }

    // Deno
    if has(cwd, "deno.json") || has(cwd, "deno.lock") {
        p.toolchain = Some("deno".into());
        p.test = Some("deno test".into());
        p.lint = Some("deno lint".into());
        p.typecheck = Some("deno check .".into());
        p.format = Some("deno fmt".into());
        p.run = Some("deno run main.ts".into());
        return p;
    }

    // Node package.json
    if has(cwd, "package.json") {
        let pm = detect_js_pm(cwd);
        p.toolchain = Some(pm.into());
        let run = if pm == "npm" { "npm run" } else { pm };
        p.test = scripts.get("test").map(|_| format!("{run} test"));
        p.build = scripts.get("build").map(|_| format!("{run} build"));
        p.lint = scripts
            .get("lint")
            .map(|_| format!("{run} lint"))
            .or_else(|| detect_js_linter(cwd, "npx"));
        p.typecheck = scripts
            .get("typecheck")
            .map(|_| format!("{run} typecheck"))
            .or_else(|| {
                if has(cwd, "tsconfig.json") {
                    Some("npx tsc --noEmit".into())
                } else {
                    None
                }
            });
        p.run = scripts
            .get("dev")
            .map(|_| format!("{run} dev"))
            .or_else(|| scripts.get("start").map(|_| format!("{run} start")));
        p.format = scripts
            .get("format")
            .map(|_| format!("{run} format"))
            .or_else(|| detect_js_formatter(cwd, "npx"));
        return p;
    }

    // Rust
    if has(cwd, "Cargo.toml") {
        p.toolchain = Some("cargo (rust)".into());
        p.test = Some("cargo test".into());
        p.build = Some("cargo build".into());
        p.lint = Some("cargo clippy --all-targets -- -D warnings".into());
        p.typecheck = Some("cargo check".into());
        p.run = Some("cargo run".into());
        p.format = Some("cargo fmt".into());
        return p;
    }

    // Go
    if has(cwd, "go.mod") {
        p.toolchain = Some("go".into());
        p.test = Some("go test ./...".into());
        p.build = Some("go build ./...".into());
        p.lint = Some(
            if has(cwd, ".golangci.yml") || has(cwd, ".golangci.yaml") {
                "golangci-lint run".into()
            } else {
                "go vet ./...".into()
            },
        );
        p.typecheck = Some("go build ./...".into());
        p.run = Some("go run .".into());
        p.format = Some("gofmt -w .".into());
        return p;
    }

    // Python
    if has(cwd, "pyproject.toml")
        || has(cwd, "setup.py")
        || has(cwd, "requirements.txt")
        || has(cwd, "pytest.ini")
        || cwd.join("tests").is_dir()
    {
        let prefix = if has(cwd, "uv.lock") {
            p.toolchain = Some("uv (python)".into());
            "uv run "
        } else if has(cwd, "poetry.lock") {
            p.toolchain = Some("poetry (python)".into());
            "poetry run "
        } else if has(cwd, "Pipfile.lock") {
            p.toolchain = Some("pipenv (python)".into());
            "pipenv run "
        } else {
            p.toolchain = Some("pip (python)".into());
            ""
        };
        p.test = Some(format!("{prefix}pytest"));
        p.lint = Some(if has(cwd, "ruff.toml") || has(cwd, ".ruff.toml") {
            format!("{prefix}ruff check")
        } else {
            format!("{prefix}flake8")
        });
        p.typecheck = Some(if has(cwd, "pyrightconfig.json") {
            format!("{prefix}pyright")
        } else {
            format!("{prefix}mypy .")
        });
        p.format = Some(if has(cwd, "ruff.toml") || has(cwd, ".ruff.toml") {
            format!("{prefix}ruff format")
        } else {
            format!("{prefix}black .")
        });
        if has(cwd, "manage.py") {
            p.run = Some(format!("{prefix}python manage.py runserver"));
        }
        return p;
    }

    // .NET
    if has(cwd, "global.json") || has_ext(cwd, ".csproj") || has_ext(cwd, ".sln") {
        p.toolchain = Some("dotnet".into());
        p.test = Some("dotnet test".into());
        p.build = Some("dotnet build".into());
        p.lint = Some("dotnet format --verify-no-changes".into());
        p.typecheck = Some("dotnet build".into());
        p.run = Some("dotnet run".into());
        return p;
    }

    // PHP
    if has(cwd, "composer.json") {
        p.toolchain = Some("composer (php)".into());
        p.test = Some("vendor/bin/phpunit".into());
        if has(cwd, "pint.json") {
            p.lint = Some("vendor/bin/pint --test".into());
            p.format = Some("vendor/bin/pint".into());
        }
        if has(cwd, "phpstan.neon") || has(cwd, "phpstan.neon.dist") {
            p.typecheck = Some("vendor/bin/phpstan analyse".into());
        } else if has(cwd, "psalm.xml") || has(cwd, "psalm.xml.dist") {
            p.typecheck = Some("vendor/bin/psalm".into());
        }
        return p;
    }

    // Ruby
    if has(cwd, "Gemfile") {
        p.toolchain = Some("bundler (ruby)".into());
        p.test = Some(if has(cwd, "spec") {
            "bundle exec rspec".into()
        } else {
            "bundle exec rails test".into()
        });
        p.lint = Some("bundle exec rubocop".into());
        p.format = Some("bundle exec rubocop -a".into());
        return p;
    }

    // Java Gradle
    if has(cwd, "gradlew") || has(cwd, "build.gradle") || has(cwd, "build.gradle.kts") {
        let gw = if has(cwd, "gradlew") {
            "./gradlew"
        } else {
            "gradle"
        };
        p.toolchain = Some("gradle (jvm)".into());
        p.test = Some(format!("{gw} test"));
        p.build = Some(format!("{gw} build"));
        p.lint = Some(format!("{gw} check"));
        p.typecheck = Some(if has(cwd, "build.gradle.kts") {
            format!("{gw} compileKotlin")
        } else {
            format!("{gw} compileJava")
        });
        return p;
    }

    // Maven
    if has(cwd, "pom.xml") || has(cwd, "mvnw") {
        let mvn = if has(cwd, "mvnw") { "./mvnw" } else { "mvn" };
        p.toolchain = Some("maven (jvm)".into());
        p.test = Some(format!("{mvn} test"));
        p.build = Some(format!("{mvn} package"));
        p.typecheck = Some(format!("{mvn} compile"));
        return p;
    }

    // CMake / Make
    if has(cwd, "CMakeLists.txt") {
        p.toolchain = Some("cmake (c/c++)".into());
        p.test = Some("ctest --test-dir build".into());
        p.build = Some("cmake --build build".into());
        return p;
    }
    if has(cwd, "Makefile") || has(cwd, "makefile") {
        p.toolchain = Some("make".into());
        p.test = Some("make test".into());
        p.build = Some("make".into());
        return p;
    }

    // Zig
    if has(cwd, "build.zig") || has(cwd, "build.zig.zon") {
        p.toolchain = Some("zig".into());
        p.test = Some("zig build test".into());
        p.build = Some("zig build".into());
        p.format = Some("zig fmt .".into());
        return p;
    }

    p
}

/// Label-only toolchain string (first marker match), for short status lines.
pub fn detect_toolchain_label(cwd: &Path) -> Option<String> {
    detect_profile(cwd).toolchain
}

// ─── Monorepo discovery ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub path: String,
    pub toolchain: Option<String>,
    pub has_lint: bool,
    pub has_test: bool,
    pub has_typecheck: bool,
}

/// Discover workspace packages (pnpm/npm/yarn workspaces, Cargo members, go.work).
pub fn discover_packages(cwd: &Path) -> Vec<PackageInfo> {
    let mut packages = Vec::new();

    for glob in js_workspace_globs(cwd) {
        let base = glob
            .trim_end_matches("/*")
            .trim_end_matches("/**")
            .trim_end_matches('*')
            .trim_end_matches('/');
        if base.is_empty() {
            continue;
        }
        scan_dir_for_marker(&cwd.join(base), cwd, &mut packages, "package.json");
    }

    if has(cwd, "Cargo.toml") {
        if let Ok(cargo) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
            if let Some(start) = cargo.find("members") {
                let slice = &cargo[start..];
                if let Some(ob) = slice.find('[') {
                    if let Some(cb) = slice[ob..].find(']') {
                        let inner = &slice[ob + 1..ob + cb];
                        for m in inner.split(',') {
                            let m = m.trim().trim_matches('"').trim_matches('\'');
                            if m.is_empty() {
                                continue;
                            }
                            if m.contains('*') {
                                let base = m.trim_end_matches("/*").trim_end_matches('*');
                                scan_dir_for_marker(
                                    &cwd.join(base),
                                    cwd,
                                    &mut packages,
                                    "Cargo.toml",
                                );
                            } else if has(&cwd.join(m), "Cargo.toml") {
                                push_package(&mut packages, &cwd.join(m), cwd);
                            }
                        }
                    }
                }
            }
        }
    }

    if has(cwd, "go.work") {
        if let Ok(gw) = std::fs::read_to_string(cwd.join("go.work")) {
            for line in gw.lines() {
                let line = line.trim();
                let dir = if let Some(rest) = line.strip_prefix("use ") {
                    rest.trim().trim_matches('"')
                } else {
                    continue;
                };
                if dir == "(" || dir == ")" || dir.is_empty() {
                    continue;
                }
                if has(&cwd.join(dir), "go.mod") {
                    push_package(&mut packages, &cwd.join(dir), cwd);
                }
            }
        }
    }

    packages.sort_by(|a, b| a.path.cmp(&b.path));
    packages.dedup_by(|a, b| a.path == b.path);
    packages
}

fn js_workspace_globs(cwd: &Path) -> Vec<String> {
    if has(cwd, "pnpm-workspace.yaml") {
        if let Ok(raw) = std::fs::read_to_string(cwd.join("pnpm-workspace.yaml")) {
            let mut globs = Vec::new();
            for line in raw.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("- ") {
                    let g = rest.trim().trim_matches('"').trim_matches('\'');
                    if !g.is_empty() {
                        globs.push(g.to_string());
                    }
                }
            }
            if !globs.is_empty() {
                return globs;
            }
        }
    }
    if has(cwd, "package.json") {
        if let Ok(raw) = std::fs::read_to_string(cwd.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(arr) = v.get("workspaces").and_then(|w| w.as_array()) {
                    return arr
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                }
                if let Some(arr) = v
                    .get("workspaces")
                    .and_then(|w| w.get("packages"))
                    .and_then(|p| p.as_array())
                {
                    return arr
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                }
            }
        }
    }
    Vec::new()
}

fn scan_dir_for_marker(dir: &Path, root: &Path, packages: &mut Vec<PackageInfo>, marker: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        if !ent.path().is_dir() {
            continue;
        }
        if has(&ent.path(), marker) {
            push_package(packages, &ent.path(), root);
        }
    }
}

fn push_package(packages: &mut Vec<PackageInfo>, pkg_dir: &Path, root: &Path) {
    let rel = pkg_dir
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| pkg_dir.display().to_string());
    if packages.iter().any(|p| p.path == rel) {
        return;
    }
    let profile = detect_profile(pkg_dir);
    let name = if has(pkg_dir, "package.json") {
        std::fs::read_to_string(pkg_dir.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .unwrap_or_else(|| rel.clone())
    } else if has(pkg_dir, "Cargo.toml") {
        std::fs::read_to_string(pkg_dir.join("Cargo.toml"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.trim().strip_prefix("name = ").map(|n| n.trim_matches('"').to_string()))
            })
            .unwrap_or_else(|| rel.clone())
    } else {
        rel.clone()
    };
    packages.push(PackageInfo {
        name,
        path: rel,
        toolchain: profile.toolchain,
        has_lint: profile.lint.is_some(),
        has_test: profile.test.is_some(),
        has_typecheck: profile.typecheck.is_some(),
    });
}

// ─── Native pre-commit checks (config tools only, not package.json scripts) ─

/// Lint + typecheck from config files (Soulforge `detectNativeChecks`).
/// Used before `git commit` via bash tool. Returns shell command or None.
pub fn detect_native_checks(cwd: &Path) -> Option<String> {
    let mut cmds = Vec::new();

    if has(cwd, "package.json")
        || has(cwd, "bun.lock")
        || has(cwd, "bun.lockb")
        || has(cwd, "tsconfig.json")
    {
        let runner = if has(cwd, "bun.lock") || has(cwd, "bun.lockb") {
            "bunx"
        } else if detect_js_pm(cwd) == "pnpm" {
            "pnpm exec"
        } else {
            "npx"
        };
        if let Some(lint) = detect_js_linter(cwd, runner) {
            cmds.push(lint);
        }
        if has(cwd, "tsconfig.json") {
            let tc = if has(cwd, "bun.lock") || has(cwd, "bun.lockb") {
                "bunx tsc --noEmit"
            } else {
                "npx tsc --noEmit"
            };
            cmds.push(tc.into());
        }
        if !cmds.is_empty() {
            return Some(cmds.join(" && "));
        }
    }

    if has(cwd, "deno.json") || has(cwd, "deno.lock") {
        return Some("deno lint && deno check .".into());
    }
    if has(cwd, "Cargo.toml") {
        return Some("cargo clippy --all-targets -- -D warnings && cargo check".into());
    }
    if has(cwd, "go.mod") {
        let lint = if has(cwd, ".golangci.yml") || has(cwd, ".golangci.yaml") {
            "golangci-lint run"
        } else {
            "go vet ./..."
        };
        return Some(format!("{lint} && go build ./..."));
    }
    if has(cwd, "pyproject.toml") || has(cwd, "setup.py") || has(cwd, "requirements.txt") {
        let pm = if has(cwd, "uv.lock") {
            "uv run "
        } else if has(cwd, "poetry.lock") {
            "poetry run "
        } else {
            ""
        };
        let lint = if has(cwd, "ruff.toml") || has(cwd, ".ruff.toml") {
            format!("{pm}ruff check")
        } else {
            format!("{pm}flake8")
        };
        let tc = if has(cwd, "pyrightconfig.json") {
            format!("{pm}pyright")
        } else {
            format!("{pm}mypy .")
        };
        return Some(format!("{lint} && {tc}"));
    }
    None
}

/// True if this shell command looks like a git commit (for pre-commit gate).
pub fn looks_like_git_commit(command: &str) -> bool {
    let c = command.trim();
    // Match common forms without catching `git commit-tree` etc. carelessly.
    let lower = c.to_ascii_lowercase();
    lower.contains("git commit")
        && !lower.contains("git commit-tree")
        && !lower.contains("git commit-graph")
}

/// Map a raw shell command to a `project` action hint (Soulforge shell redirect).
pub fn detect_project_command_hint(command: &str) -> Option<String> {
    let c = command.trim();
    let mapped = if c.starts_with("cargo test")
        || c.starts_with("bun test")
        || c.starts_with("npm test")
        || c.starts_with("pnpm test")
        || c.starts_with("yarn test")
        || c.starts_with("go test")
        || c.starts_with("pytest")
        || c.contains(" run test")
    {
        Some("test")
    } else if c.starts_with("cargo clippy")
        || c.starts_with("cargo fmt")
        || c.contains("eslint")
        || c.contains("biome check")
        || c.contains("ruff check")
        || c.contains("golangci-lint")
        || c.starts_with("go vet")
    {
        if c.starts_with("cargo fmt") || c.contains("biome format") || c.contains("ruff format") {
            Some("format")
        } else {
            Some("lint")
        }
    } else if c.starts_with("cargo check")
        || c.contains("tsc --noEmit")
        || c.contains("mypy")
        || c.contains("pyright")
        || c.starts_with("deno check")
    {
        Some("typecheck")
    } else if c.starts_with("cargo build")
        || c.starts_with("go build")
        || c.contains(" run build")
        || c.starts_with("cmake --build")
    {
        Some("build")
    } else {
        None
    }?;
    Some(format!(
        "\n\n[hint] Next time use project(action: \"{mapped}\") — auto-detected toolchain, structured output."
    ))
}

/// Preferred verify command for weak auto-verify: profile.test when present.
/// Returns a short ecosystem id (rust/go/node/python/…) for callers that branch on it.
pub fn detect_verify_from_profile(root: &Path) -> Option<(String, String)> {
    let p = detect_profile(root);
    let cmd = p.test?;
    let eco = match p.toolchain.as_deref().unwrap_or("") {
        s if s.contains("rust") || s.contains("cargo") => "rust",
        s if s.starts_with("go") => "go",
        s if s.contains("python") || s.contains("uv") || s.contains("poetry") || s.contains("pip") => {
            "python"
        }
        "bun" | "deno" | "pnpm" | "yarn" | "npm" => "node",
        "make" => "make",
        other if !other.is_empty() => {
            return Some((other.to_string(), cmd));
        }
        _ => "project",
    };
    Some((eco.into(), cmd))
}

pub struct ProjectTool {
    cwd: PathBuf,
}

impl ProjectTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for ProjectTool {
    fn name(&self) -> &str {
        "project"
    }

    fn description(&self) -> &str {
        "Run auto-detected project commands (test, lint, typecheck, build, format, run) \
         or list the detected profile. Prefer this over inventing shell commands."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["test", "lint", "typecheck", "build", "format", "run", "list", "packages"],
                    "description": "Action to run; list = root profile; packages = monorepo packages"
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional subdirectory (monorepo package path relative to project root)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Max seconds (default 300)"
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let action = ctx
            .args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let root = if let Some(rel) = ctx.args.get("cwd").and_then(|v| v.as_str()) {
            // Contain to workspace root — absolute `rel` or `../` must not
            // escape and run cargo/npm test outside the agent cwd (plan mode
            // used to treat `project` as readonly).
            let p = crate::paths::resolve_contained(&self.cwd, rel)?;
            if !p.exists() {
                anyhow::bail!("cwd not found: {}", p.display());
            }
            p
        } else {
            self.cwd.clone()
        };
        let profile = detect_profile(&root);

        if action == "packages" {
            let pkgs = discover_packages(&root);
            if pkgs.is_empty() {
                return Ok(ToolOutput::text(
                    "no monorepo packages detected (no workspaces / Cargo members / go.work)",
                ));
            }
            let mut s = format!("{} packages:\n", pkgs.len());
            for p in &pkgs {
                let mut caps = Vec::new();
                if p.has_lint {
                    caps.push("lint");
                }
                if p.has_typecheck {
                    caps.push("typecheck");
                }
                if p.has_test {
                    caps.push("test");
                }
                s.push_str(&format!(
                    "  {} -- {} ({}) [{}]\n",
                    p.name,
                    p.path,
                    p.toolchain.as_deref().unwrap_or("?"),
                    caps.join(", ")
                ));
            }
            s.push_str("\nUse project(action: \"lint\", cwd: \"<path>\") to target a package.\n");
            return Ok(ToolOutput::text(s));
        }

        if action == "list" {
            if profile.is_empty() {
                return Ok(ToolOutput::text(
                    "no project toolchain detected (no Cargo.toml, package.json, go.mod, …)",
                ));
            }
            let mut s = String::new();
            if let Some(t) = &profile.toolchain {
                s.push_str(&format!("toolchain: {t}\n"));
            }
            for (k, v) in [
                ("test", &profile.test),
                ("lint", &profile.lint),
                ("typecheck", &profile.typecheck),
                ("build", &profile.build),
                ("format", &profile.format),
                ("run", &profile.run),
            ] {
                if let Some(cmd) = v {
                    s.push_str(&format!("{k}: {cmd}\n"));
                }
            }
            let pkgs = discover_packages(&root);
            if !pkgs.is_empty() {
                s.push_str(&format!(
                    "\n{} monorepo package(s) — project(action: \"packages\") for details\n",
                    pkgs.len()
                ));
            }
            return Ok(ToolOutput::text(s));
        }

        let Some(cmd) = profile.command_for(action).map(str::to_string) else {
            return Ok(ToolOutput::text(format!(
                "no {action} command detected for {} (toolchain={:?}). Try project(action: \"list\").",
                root.display(),
                profile.toolchain
            )));
        };

        let timeout = ctx
            .args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);
        ctx.emit_update(format!("project {action}: {cmd}"));
        let out = crate::bash::exec_local(&cmd, &root, Some(Duration::from_secs(timeout))).await?;
        let combined = format!("{}{}", out.stdout, out.stderr);
        let passed = matches!(out.code, Some(0)) && !out.timed_out;
        let verdict = if out.timed_out {
            format!("TIMEOUT after {timeout}s")
        } else if passed {
            "PASS".into()
        } else {
            match out.code {
                Some(n) => format!("FAIL (exit {n})"),
                None => "FAIL (signal)".into(),
            }
        };
        let tail = tail_lines(&combined, 50);
        let text = format!(
            "[{action}] {cmd} — {verdict}\n\n{tail}",
            tail = if tail.is_empty() { "(no output)" } else { &tail }
        );
        Ok(ToolOutput::text(text).with_details(json!({
            "action": action,
            "command": cmd,
            "passed": passed,
            "toolchain": profile.toolchain,
        })))
    }
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= n {
        return s.to_string();
    }
    lines[lines.len() - n..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_rust_cargo() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        let p = detect_profile(dir.path());
        assert_eq!(p.toolchain.as_deref(), Some("cargo (rust)"));
        assert_eq!(p.test.as_deref(), Some("cargo test"));
        assert_eq!(p.lint.as_deref(), Some("cargo clippy --all-targets -- -D warnings"));
        assert!(p.prompt_section().contains("cargo test"));
    }

    #[test]
    fn detects_bun_with_biome() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bun.lock"), "").unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"bun test","lint":"biome check ."}}"#,
        )
        .unwrap();
        fs::write(dir.path().join("biome.json"), "{}").unwrap();
        let p = detect_profile(dir.path());
        assert_eq!(p.toolchain.as_deref(), Some("bun"));
        assert!(p.test.as_deref().unwrap().contains("bun"));
        assert_eq!(p.lint.as_deref(), Some("bun run lint"));
    }

    #[test]
    fn detects_pnpm_and_eslint_fallback() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"scripts":{"test":"vitest"}}"#).unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        fs::write(dir.path().join("eslint.config.js"), "export default []").unwrap();
        let p = detect_profile(dir.path());
        assert_eq!(p.toolchain.as_deref(), Some("pnpm"));
        assert_eq!(p.test.as_deref(), Some("pnpm test"));
        assert!(p.lint.as_ref().unwrap().contains("eslint"));
    }

    #[test]
    fn detects_python_uv_ruff() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        fs::write(dir.path().join("uv.lock"), "").unwrap();
        fs::write(dir.path().join("ruff.toml"), "").unwrap();
        let p = detect_profile(dir.path());
        assert_eq!(p.toolchain.as_deref(), Some("uv (python)"));
        assert_eq!(p.test.as_deref(), Some("uv run pytest"));
        assert_eq!(p.lint.as_deref(), Some("uv run ruff check"));
        assert_eq!(p.format.as_deref(), Some("uv run ruff format"));
    }

    #[test]
    fn detects_go_vet_fallback() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/x\n\ngo 1.21\n").unwrap();
        let p = detect_profile(dir.path());
        assert_eq!(p.lint.as_deref(), Some("go vet ./..."));
    }

    #[test]
    fn empty_dir_is_empty_profile() {
        let dir = tempfile::tempdir().unwrap();
        let p = detect_profile(dir.path());
        assert!(p.is_empty());
        assert!(p.prompt_section().is_empty());
    }

    #[test]
    fn verify_from_profile_rust() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        let (eco, cmd) = detect_verify_from_profile(dir.path()).unwrap();
        assert!(eco.contains("rust") || eco.contains("cargo"));
        assert_eq!(cmd, "cargo test");
    }

    #[test]
    fn native_checks_rust() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        let c = detect_native_checks(dir.path()).unwrap();
        assert!(c.contains("clippy"));
        assert!(c.contains("cargo check"));
    }

    #[test]
    fn monorepo_cargo_members() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("crates/a")).unwrap();
        fs::create_dir_all(dir.path().join("crates/b")).unwrap();
        fs::write(
            dir.path().join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("crates/b/Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let pkgs = discover_packages(dir.path());
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs.iter().any(|p| p.name == "a"));
        assert!(pkgs.iter().any(|p| p.path.contains("crates/b")));
    }

    #[test]
    fn monorepo_pnpm_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pnpm-workspace.yaml"), "packages:\n  - 'packages/*'\n").unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::create_dir_all(dir.path().join("packages/web")).unwrap();
        fs::write(
            dir.path().join("packages/web/package.json"),
            r#"{"name":"@app/web"}"#,
        )
        .unwrap();
        let pkgs = discover_packages(dir.path());
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "@app/web");
    }

    #[test]
    fn shell_hint_and_git_commit() {
        assert!(looks_like_git_commit("git commit -m 'x'"));
        assert!(!looks_like_git_commit("git status"));
        let h = detect_project_command_hint("cargo clippy --all-targets").unwrap();
        assert!(h.contains("project(action: \"lint\")"));
        assert!(detect_project_command_hint("echo hi").is_none());
    }

    /// Hard path: this repo's own workspace must detect as cargo + packages.
    #[test]
    fn detects_pirs_workspace_root() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let p = detect_profile(&root);
        assert!(
            p.toolchain.as_deref().unwrap_or("").contains("cargo")
                || p.test.as_deref() == Some("cargo test"),
            "pirs root profile: {p:?}"
        );
        assert_eq!(p.test.as_deref(), Some("cargo test"));
        let pkgs = discover_packages(&root);
        assert!(
            pkgs.len() >= 5,
            "expected monorepo crates, got {}: {:?}",
            pkgs.len(),
            pkgs.iter().map(|p| &p.path).collect::<Vec<_>>()
        );
        assert!(
            pkgs.iter().any(|p| p.path.contains("pirs-tools") || p.name.contains("pirs-tools")),
            "packages={pkgs:?}"
        );
    }
}
