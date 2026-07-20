// sandbox.rhai — wraps `bash` in an OS-level sandbox: read-only filesystem
// outside the working directory, no network access (or a specific-domain
// allowlist instead of a blanket block, if configured — see below).
//
// This is the single most repeated finding from a comparative architecture
// study of Grok Build, pi (the project pirs itself is a Rust port of),
// qwen-code, mistral-vibe, cline, and aider: pirs runs shell commands fully
// unconfined, while several of those projects sandbox by construction
// (Grok Build: Landlock+bwrap+Seatbelt; qwen-code: Docker/Podman+Seatbelt,
// with an optional egress-allowlisting proxy instead of a blanket network
// block) and — notably — pi implements it exactly this way: as an extension
// that overrides its own bash tool, not a core change
// (packages/coding-agent/examples/extensions/sandbox/index.ts). This pack
// is pirs's rhai-native equivalent of both.
//
// It works by re-registering a tool named "bash" — a later-registered tool
// sharing a native tool's name wins on both dispatch and in the schema sent
// to the model (see pirs-agent's tool_defs/prepare_call), so this pack
// transparently replaces the built-in, unsandboxed bash whenever it's
// loaded from .pirs/extensions/ or ~/.pirs/extensions/.
//
// Policy (fixed for this first cut, not yet configurable beyond the
// allowlist file below):
//   - Linux, primary: bubblewrap (bwrap) — the whole filesystem is
//     read-only except the working directory (read-write) and a fresh,
//     empty /tmp; network is fully disabled (--unshare-net).
//   - Linux, fallback: if bwrap is missing, OR bwrap itself fails to start
//     the sandbox (see the AppArmor note below — a real, common case, not
//     hypothetical), and docker or podman is on PATH, this pack
//     transparently falls back to a container-based sandbox instead of
//     failing loud: bind-mount the host root read-only plus the working
//     directory read-write, `chroot` into it, `--network none`, then
//     `chown` any files the command created back to the calling user (the
//     container runs as root — required for chroot's CAP_SYS_CHROOT — so
//     without this step created files would be owned by root on the host).
//     This sidesteps bwrap's unprivileged-userns restriction entirely
//     because the container engine's already-privileged daemon sets up the
//     namespaces on the caller's behalf, rather than the caller creating an
//     unprivileged one itself. The output is annotated so it's never
//     silently a different sandbox than the model/user might expect.
//   - Egress allowlist (Linux, docker/podman only): if a project has
//     `.pirs/sandbox-allowlist.txt` (one domain per line, `#` comments
//     allowed), bwrap is skipped entirely — it has no facility for
//     domain-scoped network access, only all-or-nothing — and the command
//     instead runs in a container attached to a `--internal` docker/podman
//     network (no route to the outside world) plus HTTP(S)_PROXY pointed at
//     a small allowlisting CONNECT proxy this pack starts on that network's
//     gateway address. The proxy tunnels HTTPS (CONNECT, port 443 only) to
//     listed domains (and their subdomains) and 403s everything else; it's
//     a long-lived background process, reused across calls and restarted
//     only when the allowlist file's contents change. This is the same
//     shape as qwen-code's sandbox proxy
//     (docs/developers/examples/proxy-script.md +
//     packages/cli/src/utils/sandbox.ts), confirmed live in this repo's own
//     dev environment: allowed domains get a real response, disallowed ones
//     get a 403 from the proxy, and the container gets no network at all if
//     the proxy isn't reachable. No allowlist file → unchanged blanket
//     network block, as above.
//   - macOS (sandbox-exec): same intent via a generated Seatbelt profile,
//     with the same allowlist file honored by adding a single
//     `network-outbound` rule scoped to the local proxy port instead of
//     denying all outbound network.
//     NOTE: the whole Darwin path (including the allowlist addition) is
//     written from documented Seatbelt syntax but has not been exercised
//     live (this pack was authored on Linux) — treat it as a starting point
//     to verify on a real Mac, not a proven quantity.
//   - Anything else, or no sandboxing mechanism available at all: FAILS
//     LOUD with an actionable message. It never silently falls through to
//     running the command unsandboxed, nor silently ignores a configured
//     allowlist by giving the command either more or less network access
//     than the file asked for — that would be exactly the kind of
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
// even with `--unshare-user-try` — every bash call fails with something
// like "setting up uid map: Permission denied" or "loopback: Failed
// RTM_NEWADDR: Operation not permitted" before the wrapped command ever
// runs. The docker/podman fallback above exists specifically because of
// this: it's confirmed to work in exactly this environment. Without
// docker/podman on PATH, the remaining options are granting bwrap an
// AppArmor exception, running `sudo sysctl -w
// kernel.apparmor_restrict_unprivileged_userns=0` (a real host security
// posture change — decide deliberately, don't cargo-cult it), or removing
// this pack to go back to unsandboxed bash.

register_tool("bash", "Run a bash command, sandboxed: the filesystem is read-only outside the working directory, and network access is either fully disabled or limited to an allowlist (see .pirs/sandbox-allowlist.txt). stdout and stderr are combined. background/auto_restart are not supported in sandboxed mode.", #{
    type: "object",
    properties: #{
        command: #{ type: "string", description: "Command to run in bash" },
        timeout: #{ type: "integer", description: "Timeout in seconds; the process tree is killed on expiry" },
        background: #{ type: "boolean", description: "Not supported in sandboxed mode — remove sandbox.rhai for background jobs" },
        auto_restart: #{ type: "boolean", description: "Not supported in sandboxed mode" }
    },
    required: ["command"]
});

fn home_dir() {
    let h = exec("printf %s \"$HOME\"", 5).output;
    h.trim();
    h
}

fn egress_scratch_dir() {
    `${home_dir()}/.pirs/sandbox-egress`
}

// Reads .pirs/sandbox-allowlist.txt from the working directory: one domain
// per line, blank lines and `#` comments ignored. An empty result means "no
// allowlist configured" — callers treat that as "use the normal blanket
// network block", not as "allow nothing".
fn read_allowlist(cwd) {
    let content = fs_read(`${cwd}/.pirs/sandbox-allowlist.txt`);
    let domains = [];
    for line in content.split("\n") {
        let d = line;
        d.trim();
        if d != "" && !d.starts_with("#") {
            domains.push(d);
        }
    }
    domains
}

// Starts (or reuses) the allowlisting proxy bound to `bind_host:8877`,
// restarting it only if the requested domain set differs from whichever
// set it was last started with — so an edited allowlist takes effect on
// the next bash call without needing a manual restart, while unrelated
// calls with the same allowlist don't pay proxy-startup cost every time.
// Returns true once the proxy is confirmed up for these domains.
fn ensure_egress_proxy(bind_host, domains) {
    let scratch = egress_scratch_dir();
    exec(`mkdir -p "${scratch}"`, 5);

    let wanted = str_join(domains, "\n");
    let domains_file = `${scratch}/proxy.domains`;
    let pid_file = `${scratch}/proxy.pid`;
    let running_domains = fs_read(domains_file);
    let pid = fs_read(pid_file);
    pid.trim();
    let already_running = pid != "" && exec(`kill -0 ${pid}`, 5).code == 0;

    if already_running && running_domains == wanted {
        return true;
    }
    if already_running {
        exec(`kill ${pid}`, 5);
    }

    // The allowlisting CONNECT proxy: a plain-stdlib Python HTTPS tunnel (no
    // dependencies to install, since python3 is commonly preinstalled and
    // this runs on the host/gateway, outside the sandboxed filesystem). Only
    // CONNECT on port 443 to a listed domain (or subdomain of one) is
    // tunneled; everything else gets a 403 before any bytes reach it. Same
    // shape as qwen-code's reference proxy
    // (docs/developers/examples/proxy-script.md), ported to Python since
    // that's what's reliably present without a container image pull. (A
    // top-level `const` for this would be simpler, but rhai only resolves
    // top-level constants inside the one function the host calls directly —
    // a function called *from* that function, like this one, can't see
    // them, so the script lives here as a local instead.)
    let proxy_script = `import socket, threading, sys

domains = [d for d in sys.argv[1].split(",") if d]
bind_host = sys.argv[2]
port = int(sys.argv[3])

def allowed(host):
    return any(host == d or host.endswith("." + d) for d in domains)

def pipe(src, dst):
    try:
        while True:
            data = src.recv(4096)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass

def handle(conn):
    try:
        req = b""
        while b"\r\n\r\n" not in req:
            chunk = conn.recv(4096)
            if not chunk:
                conn.close()
                return
            req += chunk
        line = req.split(b"\r\n", 1)[0].decode(errors="replace")
        parts = line.split()
        if len(parts) < 2 or parts[0] != "CONNECT":
            conn.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            conn.close()
            return
        host, _, port_s = parts[1].partition(":")
        target_port = int(port_s or 443)
        if not allowed(host) or target_port != 443:
            conn.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            conn.close()
            return
        try:
            upstream = socket.create_connection((host, target_port), timeout=10)
        except OSError:
            conn.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            conn.close()
            return
        conn.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        t1 = threading.Thread(target=pipe, args=(conn, upstream), daemon=True)
        t2 = threading.Thread(target=pipe, args=(upstream, conn), daemon=True)
        t1.start()
        t2.start()
        t1.join()
        t2.join()
    finally:
        conn.close()

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind((bind_host, port))
srv.listen(128)
while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
`;

    let script_path = `${scratch}/proxy.py`;
    fs_write(script_path, proxy_script);
    let allow_arg = str_join(domains, ",");
    let start = exec(`nohup python3 "${script_path}" "${allow_arg}" "${bind_host}" 8877 > "${scratch}/proxy.log" 2>&1 < /dev/null & echo $!`, 5);
    let new_pid = start.output;
    new_pid.trim();
    if new_pid == "" {
        return false;
    }
    fs_write(pid_file, new_pid);
    fs_write(domains_file, wanted);
    // Give the listener a moment to bind before the sandboxed container
    // tries to reach it.
    exec("sleep 0.3", 5);
    exec(`kill -0 ${new_pid}`, 5).code == 0
}

// Ensures a `--internal` docker/podman network exists (no route to the
// outside world for anything attached to it) and returns its gateway
// address — the one address on this network that's actually reachable from
// the host, which is where the egress proxy binds and listens.
fn docker_network_gateway(engine) {
    let net_name = "pirs-sandbox-net";
    if exec(`${engine} network inspect ${net_name}`, 5).code != 0 {
        if exec(`${engine} network create --internal ${net_name}`, 10).code != 0 {
            return "";
        }
    }
    let gateway = exec(`${engine} network inspect ${net_name} --format "{{(index .IPAM.Config 0).Gateway}}"`, 5).output;
    gateway.trim();
    gateway
}

// Which container engine (if any) is available for the Linux fallback path.
// Docker is checked first only because it's the more common install; either
// is equally suitable here.
fn container_engine() {
    if exec("command -v docker", 5).code == 0 {
        return "docker";
    }
    if exec("command -v podman", 5).code == 0 {
        return "podman";
    }
    ""
}

// Bind-mount the host root read-only, the working directory read-write over
// it, chroot in, and disable networking. Runs as root inside the container
// (chroot needs CAP_SYS_CHROOT, which a non-root `--user` loses), so any
// files the command creates come out root-owned on the host unless fixed up
// — the trailing `chown` does that, and `ec=$?`/`exit $ec` around it
// preserve the wrapped command's own exit code rather than the chown's.
fn run_container_sandbox(engine, cwd, script_path, timeout) {
    let uid = exec("id -u", 5).output;
    uid.trim();
    let gid = exec("id -g", 5).output;
    gid.trim();
    let cmd = `${engine} run --rm --network none -v /:/hostfs:ro -v "${cwd}":"/hostfs${cwd}":rw alpine:3 chroot /hostfs sh -c "cd ${cwd} && sh ${script_path}; ec=\$?; chown -R ${uid}:${gid} ${cwd} 2>/dev/null; exit \$ec"`;
    exec(cmd, timeout)
}

// Same as run_container_sandbox, but attached to the egress-allowlisted
// internal network with HTTP(S)_PROXY pointed at the allowlisting proxy
// instead of `--network none` — network access is scoped to whatever
// ensure_egress_proxy started up for, not fully open and not fully blocked.
fn run_container_sandbox_with_proxy(engine, cwd, script_path, timeout, gateway) {
    let uid = exec("id -u", 5).output;
    uid.trim();
    let gid = exec("id -g", 5).output;
    gid.trim();
    let proxy_url = `http://${gateway}:8877`;
    let cmd = `${engine} run --rm --network pirs-sandbox-net -e HTTP_PROXY=${proxy_url} -e HTTPS_PROXY=${proxy_url} -e http_proxy=${proxy_url} -e https_proxy=${proxy_url} -v /:/hostfs:ro -v "${cwd}":"/hostfs${cwd}":rw alpine:3 chroot /hostfs sh -c "cd ${cwd} && sh ${script_path}; ec=\$?; chown -R ${uid}:${gid} ${cwd} 2>/dev/null; exit \$ec"`;
    exec(cmd, timeout)
}

// A `bwrap:`/`sandbox-exec:`-prefixed line means the sandboxing tool itself
// failed to set up (namespaces/binds/profile) before the wrapped command
// ever ran — as opposed to the wrapped command failing, whose output has no
// such prefix. The exact message varies by flag combination and
// kernel/AppArmor version, so this checks the prefix rather than one
// hardcoded string.
fn is_sandbox_setup_failure(out) {
    out.contains("bwrap:") || out.contains("sandbox-exec:")
}

fn format_sandbox_result(result, timeout, note) {
    let out = result.output;
    if note != "" {
        out = `${out}\n[${note}]`;
    }
    if result.timedOut {
        return `${out}\n[sandboxed command timed out after ${timeout}s]`;
    }
    if result.code != 0 {
        return `${out}\n[exit code ${result.code}]`;
    }
    out
}

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

    let allow_domains = read_allowlist(cwd);

    if uname == "Linux" {
        if allow_domains.len() > 0 {
            // bwrap has no domain-scoped network mode (only all-or-nothing),
            // so an allowlist forces the container-engine path regardless of
            // whether bwrap itself would otherwise work here.
            let engine = container_engine();
            if engine == "" {
                exec(`rm -f "${script_path}"`, 5);
                return "sandbox.rhai: .pirs/sandbox-allowlist.txt is present, which requires docker or podman for egress allowlisting (bwrap has no equivalent mechanism). Install one, or remove the allowlist file to run with a full network block via bwrap instead.";
            }
            let gateway = docker_network_gateway(engine);
            if gateway == "" {
                exec(`rm -f "${script_path}"`, 5);
                return `sandbox.rhai: could not create/inspect the "pirs-sandbox-net" ${engine} network needed for egress allowlisting. Check ${engine} is running and this user can reach its daemon.`;
            }
            if !ensure_egress_proxy(gateway, allow_domains) {
                exec(`rm -f "${script_path}"`, 5);
                return `sandbox.rhai: the egress-allowlisting proxy failed to start — see ${egress_scratch_dir()}/proxy.log. Requires python3 on PATH.`;
            }
            let cresult = run_container_sandbox_with_proxy(engine, cwd, script_path, timeout, gateway);
            exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);
            return format_sandbox_result(cresult, timeout, `sandboxed via ${engine} with an egress allowlist (${allow_domains.len()} domain(s) from .pirs/sandbox-allowlist.txt) — see sandbox.rhai`);
        }
        if exec("command -v bwrap", 5).code == 0 {
            let wrapped = `bwrap --die-with-parent --unshare-net --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --bind "${cwd}" "${cwd}" -- sh "${script_path}"`;
            let result = exec(wrapped, timeout);
            if !(result.code != 0 && is_sandbox_setup_failure(result.output)) {
                exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);
                return format_sandbox_result(result, timeout, "");
            }
            // bwrap itself failed to start (commonly
            // kernel.apparmor_restrict_unprivileged_userns=1) — fall back
            // to a container engine, which isn't subject to that
            // restriction.
            let engine = container_engine();
            if engine == "" {
                exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);
                return `${result.output}\n[bwrap sandbox setup failed before the command ran — commonly kernel.apparmor_restrict_unprivileged_userns=1 (the Ubuntu 23.10+/24.04+ default) blocking bwrap's unprivileged user namespace; no docker/podman fallback available either (neither found on PATH) — see the note at the top of sandbox.rhai]`;
            }
            let cresult = run_container_sandbox(engine, cwd, script_path, timeout);
            exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);
            return format_sandbox_result(cresult, timeout, `sandboxed via ${engine} fallback — bwrap failed to start; see sandbox.rhai`);
        }
        let engine = container_engine();
        if engine == "" {
            exec(`rm -f "${script_path}"`, 5);
            return "sandbox.rhai requires bubblewrap, docker, or podman on Linux. Install one (e.g. `apt install bubblewrap`) or remove sandbox.rhai from .pirs/extensions/ to run unsandboxed.";
        }
        let cresult = run_container_sandbox(engine, cwd, script_path, timeout);
        exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);
        return format_sandbox_result(cresult, timeout, `sandboxed via ${engine} (bwrap not installed)`);
    } else if uname == "Darwin" {
        if exec("command -v sandbox-exec", 5).code != 0 {
            exec(`rm -f "${script_path}"`, 5);
            return "sandbox.rhai requires sandbox-exec on macOS (normally preinstalled). Remove sandbox.rhai from .pirs/extensions/ to run unsandboxed if it's unavailable.";
        }
        let extra_profile_rule = "";
        if allow_domains.len() > 0 {
            if exec("command -v python3", 5).code != 0 {
                exec(`rm -f "${script_path}"`, 5);
                return "sandbox.rhai: .pirs/sandbox-allowlist.txt requires python3 on macOS to run the egress-allowlisting proxy.";
            }
            if !ensure_egress_proxy("127.0.0.1", allow_domains) {
                exec(`rm -f "${script_path}"`, 5);
                return `sandbox.rhai: the egress-allowlisting proxy failed to start — see ${egress_scratch_dir()}/proxy.log.`;
            }
            extra_profile_rule = "\n(allow network-outbound (remote tcp \"localhost:8877\"))\n";
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
        profile = profile + extra_profile_rule;
        fs_write(profile_path, profile);
        let wrapped = `sandbox-exec -f "${profile_path}" sh "${script_path}"`;
        let result = exec(wrapped, timeout);
        exec(`rm -rf "${cwd}/.pirs/sandbox-tmp"`, 5);
        let note = if allow_domains.len() > 0 { `egress allowlist active (${allow_domains.len()} domain(s) from .pirs/sandbox-allowlist.txt) — untested on real macOS, see sandbox.rhai` } else { "" };
        format_sandbox_result(result, timeout, note)
    } else {
        exec(`rm -f "${script_path}"`, 5);
        `sandbox.rhai does not support platform "${uname}". Remove sandbox.rhai from .pirs/extensions/ to run unsandboxed.`
    }
}
