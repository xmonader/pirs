// Example pirs extension: a word-count tool plus a safety hook.

register_tool("word_count", "Count words in a string", #{
    type: "object",
    properties: #{ text: #{ type: "string" } },
    required: ["text"]
});

fn tool_word_count(args) {
    let n = args.text.split(" ").filter(|w| w != "").len();
    `${n} words`
}

fn on_tool_call(id, name, args) {
    if name == "bash" && args.command.contains("rm -rf") {
        return #{ block: true, reason: "destructive command rejected by policy" };
    }
    ()
}
