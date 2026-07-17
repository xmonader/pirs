// env-doctor.rhai — block tool calls for missing toolchains with install hints.

const KNOWN = [
    #{ bin: "cargo", hint: "rustup install stable" },
    #{ bin: "npm", hint: "install nodejs (e.g. apt install nodejs npm)" },
    #{ bin: "pytest", hint: "pip install pytest" },
    #{ bin: "go", hint: "install golang" },
    #{ bin: "make", hint: "apt install build-essential" },
    #{ bin: "docker", hint: "install docker" },
    #{ bin: "rg", hint: "install ripgrep" },
];

fn on_tool_call(id, name, args) {
    if name != "bash" {
        return ();
    }
    let parts = args.command.split(" ");
    let first = parts[0];
    for k in KNOWN {
        if first == k.bin {
            let check = exec(`command -v ${k.bin}`, 5);
            if check.code != 0 {
                return #{
                    block: true,
                    reason: `Toolchain missing: '${k.bin}' is not installed. Hint: ${k.hint}. Ask the user or install it first.`
                };
            }
        }
    }
    ()
}
