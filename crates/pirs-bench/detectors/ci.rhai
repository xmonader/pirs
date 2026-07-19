// CI-oracle detector — ranked FIRST (highest trust) because a project's own CI
// config is ground truth for how its tests are actually installed and run. Its
// real value over the structural pytest detector is the *install* list: env
// setup ("pip install -e .[test]", "-r requirements-test.txt") is a top failure
// bucket, and CI shows the true commands instead of a guess.
//
// Scope: pytest only. Cargo.toml / go.mod are already unambiguous, so CI adds
// nothing for Rust/Go; Python is where SWE-bench lives and where the install
// story is subtle.
//
// Safety: the extracted `pip install` commands run in the same sandbox as the
// test suite itself (which we already execute), so this grants no capability the
// harness didn't already have. We cut each extracted command at the first shell
// chain delimiter (&&, ;, |) so a trailing `&& pytest ...` or worse can't ride
// along. If the extracted setup is wrong, the probe fails and discovery falls
// through to the generic pytest detector — degrade-safe, never a false pass.

// Concatenate the known CI/build files into one haystack, bounded to the files
// we name (never a recursive directory read).
fn gather_ci() {
    let hay = "";
    let files = [
        ".gitlab-ci.yml", ".circleci/config.yml", "tox.ini",
        "noxfile.py", "Makefile", "makefile", "azure-pipelines.yml",
    ];
    for f in files {
        if path_exists(f) { hay += file_read(f); hay += "\n"; }
    }
    let wf = ".github/workflows";
    if path_exists(wf) {
        for name in read_dir(wf) {
            if name.ends_with(".yml") || name.ends_with(".yaml") {
                hay += file_read(wf + "/" + name);
                hay += "\n";
            }
        }
    }
    hay
}

// Pull a single `pip install ...` command out of one CI line, stripped of YAML
// step noise and truncated at the first shell chain delimiter. "" if none.
fn extract_pip(line) {
    let low = line.to_lower();
    let i = low.index_of("pip install");
    if i < 0 { i = low.index_of("pip3 install"); }
    if i < 0 { return ""; }

    let cmd = line.sub_string(i);
    for d in ["&&", ";", "|", "#"] {
        let j = cmd.index_of(d);
        if j >= 0 { cmd = cmd.sub_string(0, j); }
    }
    cmd.trim(); // Rhai's trim() mutates in place and returns unit — never assign it.
    // Drop a wrapping quote pair left over from the YAML value.
    if cmd.len() >= 2 && cmd.starts_with("\"") && cmd.ends_with("\"") {
        cmd = cmd.sub_string(1, cmd.len() - 2);
    }
    if cmd.len() >= 2 && cmd.starts_with("'") && cmd.ends_with("'") {
        cmd = cmd.sub_string(1, cmd.len() - 2);
    }
    cmd.trim();
    cmd
}

fn detect() {
    let specs = [];
    let hay = gather_ci();
    if hay == "" { return specs; }

    let low = hay.to_lower();
    if !(low.contains("pytest") || low.contains("py.test")) { return specs; }

    // Collect the real install commands, de-duplicated, in CI order.
    let installs = [];
    for line in hay.split('\n') {
        let cmd = extract_pip(line);
        if cmd != "" {
            let dup = false;
            for existing in installs { if existing == cmd { dup = true; } }
            if !dup { installs.push(cmd + " 2>/dev/null || true"); }
        }
    }
    // No explicit pip lines (e.g. deps come from tox) → fall back to the guess,
    // but still rank first so a CI-confirmed pytest wins on ordering.
    if installs.len() == 0 {
        installs.push("pip install -e . 2>/dev/null || true");
    }

    // Resolve the interpreter at runtime (python3 or python), matching pytest.rhai.
    let py = "PY=$(command -v python3 || command -v python); ";
    specs.push(#{
        framework: "pytest-ci",
        install: installs,
        list_cmd: py + "\"$PY\" -m pytest --collect-only -q 2>/dev/null | head -n 5",
        test_cmd: py + "\"$PY\" -m pytest {tests} --junitxml={junit} -p no:cacheprovider -q",
        timeout_secs: 1200,
        parallel: false,
    });
    specs
}
