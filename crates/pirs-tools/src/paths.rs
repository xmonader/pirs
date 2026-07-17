use std::path::{Path, PathBuf};

pub fn resolve(cwd: &Path, input: &str) -> PathBuf {
    let expanded = expand_tilde(input);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn expand_tilde(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_resolves_against_cwd() {
        assert_eq!(
            resolve(Path::new("/work"), "src/main.rs"),
            PathBuf::from("/work/src/main.rs")
        );
    }

    #[test]
    fn absolute_kept() {
        assert_eq!(
            resolve(Path::new("/work"), "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
    }
}
