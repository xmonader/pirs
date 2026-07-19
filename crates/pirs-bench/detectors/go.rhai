// Go detector. `gotestsum` emits JUnit natively; the probe (`go test -list`)
// confirms the toolchain. Because every candidate is probe-confirmed and every
// run must produce JUnit, shipping this best-effort is safe: if gotestsum is
// absent or the id format is wrong for a given repo, the run simply fails to
// produce JUnit and the driver surfaces it — it never yields a false pass.
//
// Go's `-run` takes a regex, so multiple target ids are joined with `|` (an
// alternation) via `test_join`, not the default space (which would be a regex
// matching nothing). The probe uses the built-in `go test -list` so discovery
// works on a bare toolchain; `gotestsum` (the JUnit emitter) is installed during
// bootstrap. If the install can't run (offline container), the test run produces
// no JUnit and the driver surfaces it — never a false pass.
fn detect() {
    let specs = [];
    if path_exists("go.mod") {
        specs.push(#{
            framework: "go",
            install: ["go install gotest.tools/gotestsum@latest 2>/dev/null || true"],
            list_cmd: "go test -list \".*\" ./... 2>/dev/null | head -n 5",
            test_cmd: "gotestsum --junitfile {junit} --format standard-quiet -- -run \"{tests}\" ./...",
            test_join: "|",
            timeout_secs: 900,
            parallel: true,
        });
    }
    specs
}
