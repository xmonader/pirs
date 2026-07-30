// scope-creep.rhai — block edits to env/build config unless allowlisted.
//
// Failure autopsies (SWE-bench-style coding agents) show a recurring mode:
// the agent fights the environment (setup.py, tox.ini, requirements pins)
// instead of the bug, polluting the patch. This pack hard-denies those paths
// by default. Allow exceptions via `.pirs/scope-creep-allow.txt` (substring
// lines) or by omitting this pack from the profile.
//
// Not a security boundary — speed bump for weak executors.

fn path_of(args) {
    if args.contains("path") {
        return `${args.path}`;
    }
    if args.contains("target_file") {
        return `${args.target_file}`;
    }
    if args.contains("file_path") {
        return `${args.file_path}`;
    }
    ""
}

fn is_blocked_path(p) {
    let s = `${p}`;
    s.replace("\\", "/");
    // Exact basename matches (common agent spellings).
    let parts = s.split("/");
    let base = if parts.len() > 0 { parts[parts.len() - 1] } else { s };
    if base == "setup.py" || base == "setup.cfg" || base == "tox.ini" || base == "noxfile.py" {
        return true;
    }
    if base == "package-lock.json" || base == "yarn.lock" || base == "pnpm-lock.yaml" {
        return true;
    }
    if base == "Cargo.lock" || base == "cargo.lock" || base == "poetry.lock" {
        return true;
    }
    if base == "Pipfile.lock" || base == "Gemfile.lock" || base == "composer.lock" {
        return true;
    }
    if base == "Makefile" || base == "makefile" || base == "Dockerfile" || base == "CMakeLists.txt" {
        return true;
    }
    if base == "requirements.txt" || base == "pyproject.toml" {
        return true;
    }
    // Nested / patterned paths.
    if s.contains("requirements-") || s.contains("requirements/") {
        return true;
    }
    if s.contains(".github/workflows/") {
        return true;
    }
    if s.contains("docker-compose") {
        return true;
    }
    false
}

fn on_tool_call(id, name, args) {
    if name != "edit" && name != "write" && name != "edit_block" && name != "safe_edit" && name != "ast_edit" {
        return ();
    }
    let p = path_of(args);
    if p == "" {
        return ();
    }
    if !is_blocked_path(p) {
        return ();
    }
    // Optional allowlist (best-effort; missing file is fine).
    try {
        let cwd = exec("pwd", 5).output;
        cwd.trim();
        let content = fs_read(`${cwd}/.pirs/scope-creep-allow.txt`);
        for line in content.split("\n") {
            let l = line;
            l.trim();
            if l != "" && !l.starts_with("#") && p.contains(l) {
                return ();
            }
        }
    } catch (e) {}
    #{
        block: true,
        reason: `scope-creep: editing '${p}' looks like env/build config, not the bug. Fix source under the package (or add a line to .pirs/scope-creep-allow.txt if this path is intentional).`
    }
}
