// sandbox.rhai — wraps `bash` in an OS-level sandbox: read-only filesystem
// outside the working directory, no network access.
//
// This is the single most repeated finding from a comparative architecture
// study of Grok Build, pi (the project pirs itself is a Rust port of),
// qwen-code, mistral-vibe, cline, and aider: pirs runs shell commands fully
// unconfined, while several of those projects sandbox by construction
// (Grok Build: Landlock+bwrap+Seatbelt; qwen-code: Docker/Podman+Seatbelt)
// and — notably — pi implements it exactly this way: as an extension that
// overrides its own bash tool, not a core change
// (packages/coding-agent/examples/extensions/sandbox/index.ts). This pack
// is pirs's rhai-native equivalent of that.
//
// It works by re-registering a tool named "bash" — a later-registered tool
// sharing a native tool's name wins on both dispatch and in the schema sent
// to the model (see pirs-agent's tool_defs/prepare_call), so this pack
// transparently replaces the built-in, unsandboxed bash whenever it's
// loaded from .pirs/extensions/ or ~/.pirs/extensions/.
//
// Policy (fixed for this first cut, not yet configurable):
//   - Linux (bubblewrap): the whole filesystem is read-only except the
//     working directory (read-write) and a fresh, empty /tmp; network is
//     fully disabled (--unshare-net).
//   - macOS (sandbox-exec): same intent via a generated Seatbelt profile.
//     NOTE: this path is written from documented Seatbelt syntax but has
//     not been exercised live (this pack was authored on Linux) — treat it
//     as a starting point to verify on a real Mac, not a proven quantity.
//   - Anything else, or the required sandboxing binary missing: FAILS
//     LOUD with an actionable message. It never silently falls through to
//     running the command unsandboxed — that would be exactly the kind of
//     confident-wrong behavior this whole comparative study flagged as the
//     failure mode to design against.
//
// `background`/`auto_restart` (the native bash tool's job-management modes)
// are not supported here — sandboxing a long-running background job needs
// the native job registry this pack doesn't reimplement. Remove this pack
// (or ask the user to) for tasks that need those.
//
// Known environment constraint, confirmed while building this: on Ubuntu
// 23.10+/24.04+, `kernel.apparmor_restrict_unprivileged_userns=1` (the
// default) blocks bwrap's unprivileged user-namespace creation entirely,
// even with `--unshare-user-try`. If bash calls start failing with "setting
// up uid map: Permission denied", that's this — either grant bwrap an
// AppArmor exception, run `sudo sysctl -w
// kernel.apparmor_restrict_unprivileged_userns=0` (a real host security
// posture change — decide deliberately, don't cargo-cult it), or remove
// this pack to go back to unsandboxed bash.

register_tool("bash", "Run a bash command, sandboxed: the filesystem is read-only outside the working directory, and there is no network access. stdout and stderr are combined. background/auto_restart are not supported in sandboxed mode.", #{
    type: "object",
    properties: #{
        command: #{ type: "string", description: "Command to run in bash" },
        timeout: #{ type: "integer", description: "Timeout in seconds; the process tree is killed on expiry" },
        background: #{ type: "boolean", description: "Not supported in sandboxed mode — remove sandbox.rhai for background jobs" },
        auto_restart: #{ type: "boolean", description: "Not supported in sandboxed mode" }
    },
    required: ["command"]
});

fn tool_bash(args) {
    if (args.background ?? false) || (args.auto_restart ?? false) {
        return "sandboxed bash does not support background/auto_restart. Remove sandbox.rhai from .pirs/extensions/ (or ~/.pirs/extensions/) to run this command unsandboxed with background-job support.";
    }
    let timeout = args.timeout ?? 120;

    let cwd = exec("pwd", 5).output;
    cwd.trim();
    let uname = exec("uname", 5).output;
    uname.trim();

    // The command is written to a script file and executed by path rather
    // than embedded as a nested quoted string in the wrapper command — that
    // sidesteps an entire class of quote-escaping bugs a naive
    // `bwrap ... -- sh -c "${command}"` string-build would risk once the
    // command itself contains quotes.
    let script_path = `${cwd}/.pirs/sandbox-tmp/cmd-${now_millis()}.sh`;
    fs_write(script_path, args.command);

    let wrapped = "";
    if uname == "Linux" {
        if exec("command -v bwrap", 5).code != 0 {
            exec(`rm -f "${script_path}"`, 5);
            return "sandbox.rhai requires bubblewrap on Linux. Install it (e.g. `apt install bubblewrap` / `dnf install bubblewrap`) or remove sandbox.rhai from .pirs/extensions/ to run unsandboxed.";
        }
        wrapped = `bwrap --die-with-parent --unshare-net --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --bind "${cwd}" "${cwd}" -- sh "${script_path}"`;
    } else if uname == "Darwin" {
        if exec("command -v sandbox-exec", 5).code != 0 {
            exec(`rm -f "${script_path}"`, 5);
            return "sandbox.rhai requires sandbox-exec on macOS (normally preinstalled). Remove sandbox.rhai from .pirs/extensions/ to run unsandboxed if it's unavailable.";
        }
        let profile_path = `${cwd}/.pirs/sandbox-tmp/profile-${now_millis()}.sb`;
        let profile = `(version 1)
(deny default)
(allow process-fork)
(allow process-exec)
(allow signal (target self))
(allow sysctl-read)
(allow file-read*)
(allow file-write* (subpath "${cwd}"))
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/private/tmp"))
`;
        fs_write(profile_path, profile);
        wrapped = `sandbox-exec -f "${profile_path}" sh "${script_path}"`;
    } else {
        exec(`rm -f "${script_path}"`, 5);
        return `sandbox.rhai does not support platform "${uname}". Remove sandbox.rhai from .pirs/extensions/ to run unsandboxed.`;
    }

    let result = exec(wrapped, timeout);
    exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);

    let out = result.output;
    if result.timedOut {
        return `${out}\n[sandboxed command timed out after ${timeout}s]`;
    }
    if result.code != 0 {
        // Any bwrap-prefixed line means bwrap itself failed to set up the
        // sandbox (namespaces/binds) before the wrapped command ever ran —
        // as opposed to the wrapped command failing, whose output has no
        // such prefix. The exact message varies by flag combination and
        // kernel/AppArmor version ("setting up uid map: Permission denied",
        // "loopback: Failed RTM_NEWADDR: Operation not permitted", etc.) so
        // this checks the prefix rather than one hardcoded string.
        if out.contains("bwrap:") || out.contains("sandbox-exec:") {
            return `${out}\n[sandbox setup failed before the command ran — on Linux this is commonly kernel.apparmor_restrict_unprivileged_userns=1 (the Ubuntu 23.10+/24.04+ default) blocking bwrap's unprivileged user namespace; see the note at the top of sandbox.rhai]`;
        }
        return `${out}\n[exit code ${result.code}]`;
    }
    out
}
