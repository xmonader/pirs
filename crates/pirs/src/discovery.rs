use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct FileCommand {
    pub name: String,
    pub description: String,
    pub body: String,
    #[allow(dead_code)]
    pub path: PathBuf,
}

fn home_dirs() -> Vec<PathBuf> {
    std::env::var("HOME")
        .map(|h| vec![PathBuf::from(h)])
        .unwrap_or_default()
}

fn dedupe_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    roots.retain(|r| {
        let key = std::fs::canonicalize(r).unwrap_or_else(|_| r.clone());
        seen.insert(key)
    });
    roots
}

fn skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        cwd.join(".claude").join("skills"),
        cwd.join(".agents").join("skills"),
        cwd.join(".pirs").join("skills"),
    ];
    for home in home_dirs() {
        roots.push(home.join(".claude").join("skills"));
        roots.push(home.join(".agents").join("skills"));
        roots.push(home.join(".pirs").join("skills"));
    }
    dedupe_roots(roots)
}

fn command_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        cwd.join(".claude").join("commands"),
        cwd.join(".agents").join("commands"),
        cwd.join(".pirs").join("commands"),
    ];
    for home in home_dirs() {
        roots.push(home.join(".claude").join("commands"));
        roots.push(home.join(".agents").join("commands"));
        roots.push(home.join(".pirs").join("commands"));
    }
    dedupe_roots(roots)
}

pub fn parse_frontmatter(content: &str) -> (Vec<(String, String)>, String) {
    let mut fields = Vec::new();
    let trimmed = content.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return (fields, content.to_string());
    }
    let after = &trimmed[3..];
    let Some(end) = after.find("\n---") else {
        return (fields, content.to_string());
    };
    for line in after[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() {
                fields.push((key, value));
            }
        }
    }
    let body = after[end + 4..].trim_start_matches('\n').to_string();
    (fields, body)
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    for root in skill_roots(cwd) {
        let Ok(read) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            let skill_file = if entry.is_dir() {
                entry.join("SKILL.md")
            } else if entry.extension().and_then(|e| e.to_str()) == Some("md") {
                entry.clone()
            } else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&skill_file) else {
                continue;
            };
            let (fields, _) = parse_frontmatter(&content);
            let name = field(&fields, "name")
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    entry
                        .file_stem()
                        .or_else(|| entry.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                });
            if name.is_empty() {
                continue;
            }
            let description = field(&fields, "description").unwrap_or("").to_string();
            skills.push(Skill {
                name,
                description,
                path: skill_file,
            });
        }
    }
    skills
}

pub fn discover_commands(cwd: &Path) -> Vec<FileCommand> {
    let mut commands = Vec::new();
    for root in command_roots(cwd) {
        let Ok(read) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        entries.sort();
        for entry in entries {
            let Ok(content) = std::fs::read_to_string(&entry) else {
                continue;
            };
            let (fields, body) = parse_frontmatter(&content);
            let name = field(&fields, "name")
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    entry
                        .file_stem()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                });
            if name.is_empty() {
                continue;
            }
            let description = field(&fields, "description").unwrap_or("").to_string();
            commands.push(FileCommand {
                name,
                description,
                body,
                path: entry,
            });
        }
    }
    commands
}

pub fn skills_prompt_block(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("\n<available_skills>\n");
    for s in skills {
        out.push_str(&format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
            s.name,
            s.description,
            s.path.display()
        ));
    }
    out.push_str("</available_skills>\n");
    out.push_str("When a task matches a skill's description, read its SKILL.md with the read tool and follow it.\n");
    Some(out)
}

pub fn expand_command(cmd: &FileCommand, args: &str) -> String {
    if cmd.body.contains("$ARGUMENTS") {
        cmd.body.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        cmd.body.clone()
    } else {
        format!("{}\n\nArguments: {args}", cmd.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses_fields_and_body() {
        let (fields, body) = parse_frontmatter("---\nname: pdf\ndescription: \"Extract text\"\n---\n# Do the thing\n");
        assert_eq!(field(&fields, "name"), Some("pdf"));
        assert_eq!(field(&fields, "description"), Some("Extract text"));
        assert_eq!(body, "# Do the thing\n");
    }

    #[test]
    fn frontmatter_absent() {
        let (fields, body) = parse_frontmatter("# Just markdown\n");
        assert!(fields.is_empty());
        assert_eq!(body, "# Just markdown\n");
    }

    #[test]
    fn discovers_skills_and_commands() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".claude/skills/review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review code\n---\nSteps...\n",
        )
        .unwrap();
        let cmd_dir = dir.path().join(".agents/commands");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::write(
            cmd_dir.join("explain.md"),
            "---\ndescription: Explain a topic\n---\nExplain $ARGUMENTS simply.\n",
        )
        .unwrap();

        std::env::set_var("HOME", dir.path().join("no-home"));
        let skills = discover_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[0].description, "Review code");
        assert!(skills[0].path.ends_with("SKILL.md"));

        let commands = discover_commands(dir.path());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "explain");
        assert_eq!(
            expand_command(&commands[0], "monads"),
            "Explain monads simply.\n"
        );
    }

    #[test]
    fn skills_block_xml() {
        let skills = vec![Skill {
            name: "pdf".into(),
            description: "PDF tools".into(),
            path: PathBuf::from("/x/SKILL.md"),
        }];
        let block = skills_prompt_block(&skills).unwrap();
        assert!(block.contains("<available_skills>"));
        assert!(block.contains("<name>pdf</name>"));
        assert!(block.contains("/x/SKILL.md"));
        assert!(skills_prompt_block(&[]).is_none());
    }

    #[test]
    fn expand_without_placeholder_appends_args() {
        let cmd = FileCommand {
            name: "x".into(),
            description: "".into(),
            body: "Do it.".into(),
            path: PathBuf::new(),
        };
        assert_eq!(expand_command(&cmd, "fast"), "Do it.\n\nArguments: fast");
        assert_eq!(expand_command(&cmd, ""), "Do it.");
    }
}
