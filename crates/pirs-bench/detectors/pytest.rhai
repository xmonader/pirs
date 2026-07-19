// pytest detector. The `--junitxml=PATH` flag is stable and universal, and a
// pytest node id (`path/to/test.py::TestClass::test_case`) is exactly what the
// space-joined `{tests}` placeholder needs — so this detector is a clean fit.
//
// The SWE-bench-heavy repos (django, sympy, sphinx, pytest itself) are all
// Python, so this is the flagship detector.
//
// Interpreter resolution: many environments ship only `python3` (or only
// `python`). Every command resolves `$PY` at runtime — `python3` first, then
// `python` — so the detector works regardless of which name exists on PATH.
fn detect() {
    let specs = [];
    let is_python =
        path_exists("pyproject.toml") || path_exists("setup.py") ||
        path_exists("setup.cfg")     || path_exists("tox.ini")  ||
        path_exists("conftest.py");
    if is_python {
        let py = "PY=$(command -v python3 || command -v python); ";
        specs.push(#{
            framework: "pytest",
            // Editable install is the usual prerequisite; tolerate its absence so
            // the probe still runs and surfaces the real error if it matters.
            install: [py + "\"$PY\" -m pip install -e . 2>/dev/null || true"],
            // Collect-only probe: proves pytest can import and enumerate tests.
            list_cmd: py + "\"$PY\" -m pytest --collect-only -q 2>/dev/null | head -n 5",
            // Space-joined node ids in {tests}; empty {tests} runs the whole suite.
            test_cmd: py + "\"$PY\" -m pytest {tests} --junitxml={junit} -p no:cacheprovider -q",
            timeout_secs: 1200,
            parallel: false,
        });
    }
    specs
}
