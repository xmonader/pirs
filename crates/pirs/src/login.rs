//! Parse `pirs login` pseudo-subcommand from flattened prompt args.

/// Detect the `login` pseudo-subcommand and resolve which provider to store a
/// key for, returning `None` when this is not a login invocation.
///
/// `prompt` is the *flattened* one-shot prompt (all trailing argv joined by
/// single spaces by the time this is called), `mode` is `--mode`, and
/// `provider` is the already-resolved `--provider`/config value.
///
/// Matches only the exact shapes `login`, `login openai`, `login anthropic`
/// (or `--mode login`). We split the flattened prompt back into tokens because
/// after flattening `cli.prompt.first()` is the whole `"login openai"` string,
/// not `"login"` -- the original bug where `pirs login openai` silently ran the
/// agent with "login openai" as a prompt instead of logging in. The narrow
/// match keeps a genuine prompt that merely starts with the word "login"
/// (e.g. `pirs "login and restart the box"`) out of the login path. An explicit
/// `login <provider>` token wins over the ambient `--provider` value.
pub fn parse_login_request(
    prompt: Option<&str>,
    mode: &str,
    provider: &str,
) -> Option<&'static str> {
    let tokens: Vec<&str> = prompt
        .map(|s| s.split_whitespace().collect())
        .unwrap_or_default();
    let is_login = mode == "login"
        || matches!(
            tokens.as_slice(),
            ["login"] | ["login", "openai"] | ["login", "anthropic"]
        );
    if !is_login {
        return None;
    }
    Some(match tokens.get(1).copied() {
        Some("anthropic") => "anthropic",
        Some("openai") => "openai",
        _ if provider == "anthropic" => "anthropic",
        _ => "openai",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_bare_uses_ambient_provider() {
        assert_eq!(
            parse_login_request(Some("login"), "repl", "openai"),
            Some("openai")
        );
        assert_eq!(
            parse_login_request(Some("login"), "repl", "anthropic"),
            Some("anthropic")
        );
    }

    #[test]
    fn login_with_provider_token_wins_over_ambient() {
        assert_eq!(
            parse_login_request(Some("login openai"), "repl", "anthropic"),
            Some("openai")
        );
        assert_eq!(
            parse_login_request(Some("login anthropic"), "repl", "openai"),
            Some("anthropic")
        );
    }

    #[test]
    fn mode_login_forces_login_regardless_of_prompt() {
        assert_eq!(parse_login_request(None, "login", "openai"), Some("openai"));
        assert_eq!(
            parse_login_request(None, "login", "anthropic"),
            Some("anthropic")
        );
    }

    #[test]
    fn genuine_prompt_starting_with_login_is_not_intercepted() {
        assert_eq!(
            parse_login_request(Some("login and restart the box"), "repl", "openai"),
            None
        );
        assert_eq!(
            parse_login_request(Some("login foo"), "repl", "openai"),
            None
        );
        assert_eq!(parse_login_request(None, "repl", "openai"), None);
    }
}
