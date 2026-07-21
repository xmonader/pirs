//! Skills loader — [agentskills.io](https://agentskills.io) compatible.
//!
//! Layout: `~/.pirs/skills/<name>/SKILL.md` (+ optional scripts/references/assets).
//! Progressive disclosure: system prompt gets name+description only; full body
//! via `skill_view` tool or CLI `skills show`.
//!
//! Discovery roots (harness + claw): project `.pirs`/`.agents`/`.claude` skills
//! dirs and the same under `$HOME`.

use std::fs;
use std::path::{Path, PathBuf};

/// Default user skills root: `~/.pirs/skills`.
pub fn default_skills_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pirs").join("skills")
}

/// Multi-root skill directories for `cwd` (project + home).
pub fn skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        cwd.join(".pirs").join("skills"),
        cwd.join(".agents").join("skills"),
        cwd.join(".claude").join("skills"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".pirs").join("skills"));
        roots.push(home.join(".agents").join("skills"));
        roots.push(home.join(".claude").join("skills"));
    }
    let mut seen = std::collections::HashSet::new();
    roots.retain(|r| {
        let key = fs::canonicalize(r).unwrap_or_else(|_| r.clone());
        seen.insert(key)
    });
    roots
}

/// Discover skills under all roots for `cwd` (dedupe by name; earlier roots win).
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let mut names = std::collections::HashSet::new();
    for root in skill_roots(cwd) {
        for sk in load_skills(&root) {
            if names.insert(sk.name.clone()) {
                out.push(sk);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<String>,
    /// Free-form metadata key=value lines (agentskills `metadata:` map, flattened).
    pub metadata: Vec<(String, String)>,
}

impl Skill {
    /// Directory containing SKILL.md (skill root).
    pub fn root_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.path.clone())
    }
}

/// agentskills.io `name` rules: 1–64 chars, `[a-z0-9-]`, no leading/trailing/double hyphen.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!(
            "skill name must be 1–64 characters (got {})",
            name.len()
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("skill name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        return Err("skill name must not contain consecutive hyphens".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "skill name must be lowercase letters, digits, and hyphens only (agentskills.io)".into(),
        );
    }
    Ok(())
}

pub fn validate_description(desc: &str) -> Result<(), String> {
    let t = desc.trim();
    if t.is_empty() {
        return Err("skill description must be non-empty".into());
    }
    if t.len() > 1024 {
        return Err(format!(
            "skill description must be ≤1024 characters (got {})",
            t.len()
        ));
    }
    Ok(())
}

/// Full skill validation (name + description).
pub fn validate_skill(sk: &Skill) -> Result<(), String> {
    validate_skill_name(&sk.name)?;
    validate_description(&sk.description)?;
    Ok(())
}

/// Parse optional YAML frontmatter between `---` fences (agentskills-shaped).
pub fn parse_skill_md(raw: &str, path: &Path) -> Skill {
    let mut name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .filter(|s| *s != "skills" && !s.is_empty())
        .or_else(|| path.file_stem().and_then(|s| s.to_str()))
        .unwrap_or("skill")
        .to_string();
    let mut description = String::new();
    let mut license = None;
    let mut compatibility = None;
    let mut allowed_tools = None;
    let mut metadata = Vec::new();
    let body;

    if let Some(rest) = raw.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            body = rest[end + 4..].trim_start().to_string();
            let mut in_metadata = false;
            for line in fm.lines() {
                let line_raw = line;
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "metadata:" {
                    in_metadata = true;
                    continue;
                }
                if in_metadata {
                    if let Some(rest) = line_raw.strip_prefix("  ") {
                        if let Some((k, v)) = rest.split_once(':') {
                            metadata.push((
                                k.trim().to_string(),
                                v.trim().trim_matches('"').to_string(),
                            ));
                            continue;
                        }
                    }
                    if !line.starts_with(' ') && line.contains(':') {
                        in_metadata = false;
                    } else {
                        continue;
                    }
                }
                if let Some(v) = line.strip_prefix("name:") {
                    name = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("license:") {
                    license = Some(v.trim().trim_matches('"').to_string());
                } else if let Some(v) = line.strip_prefix("compatibility:") {
                    compatibility = Some(v.trim().trim_matches('"').to_string());
                } else if let Some(v) = line.strip_prefix("allowed-tools:") {
                    allowed_tools = Some(v.trim().trim_matches('"').to_string());
                }
            }
        } else {
            body = raw.to_string();
        }
    } else {
        body = raw.to_string();
    }

    Skill {
        name,
        description,
        body: body.trim().to_string(),
        path: path.to_path_buf(),
        license,
        compatibility,
        allowed_tools,
        metadata,
    }
}

/// Walk skills dir for SKILL.md (preferred) or loose .md (max depth 3).
pub fn load_skills(dir: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    walk(dir, 0, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<Skill>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            let skill_md = p.join("SKILL.md");
            if skill_md.is_file() {
                if let Ok(raw) = fs::read_to_string(&skill_md) {
                    out.push(parse_skill_md(&raw, &skill_md));
                }
            } else {
                walk(&p, depth + 1, out);
            }
        } else if p.extension().and_then(|e| e.to_str()) == Some("md")
            && p.file_name().and_then(|n| n.to_str()) != Some("usage.json")
        {
            // Loose .md only at top-ish levels when not already SKILL.md dir.
            if p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(&p) {
                out.push(parse_skill_md(&raw, &p));
            }
        }
    }
}

/// Level-0 progressive disclosure: names + descriptions only (no full bodies).
pub fn skills_prompt_section(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\n## Available skills (agentskills.io)\n\
         Load full instructions with the `skill_view` tool when needed.\n",
    );
    for sk in skills {
        let desc = if sk.description.is_empty() {
            "(no description)"
        } else {
            sk.description.as_str()
        };
        s.push_str(&format!("- **{}**: {}\n", sk.name, desc));
    }
    s
}

/// Full bodies for isolated job prompts (cron skill attach).
pub fn skills_full_section(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## Attached skills\n");
    for sk in skills {
        s.push_str(&format!(
            "\n### {}\n{}\n\n{}\n",
            sk.name,
            sk.description,
            sk.body
        ));
        record_usage(&sk.name);
    }
    s
}

pub fn find_skill<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills
        .iter()
        .find(|s| s.name == name || s.name.eq_ignore_ascii_case(name))
}

/// Copy a skill file or directory into `~/.pirs/skills/<name>/` (SKILL.md + optional subdirs).
pub fn install_skill(src: &Path, dest_root: &Path) -> anyhow::Result<PathBuf> {
    if !src.exists() {
        anyhow::bail!("skill path not found: {}", src.display());
    }
    fs::create_dir_all(dest_root)?;
    if src.is_dir() {
        let skill_md = src.join("SKILL.md");
        let raw = fs::read_to_string(&skill_md)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", skill_md.display()))?;
        let sk = parse_skill_md(&raw, &skill_md);
        if let Err(e) = validate_skill(&sk) {
            anyhow::bail!("invalid skill: {e}");
        }
        let dir = dest_root.join(&sk.name);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        copy_dir_filtered(src, &dir)?;
        Ok(dir.join("SKILL.md"))
    } else {
        let raw = fs::read_to_string(src)?;
        let sk = parse_skill_md(&raw, src);
        if let Err(e) = validate_skill(&sk) {
            anyhow::bail!("invalid skill: {e}");
        }
        let dir = dest_root.join(&sk.name);
        fs::create_dir_all(&dir)?;
        let dest = dir.join("SKILL.md");
        fs::write(&dest, raw)?;
        Ok(dest)
    }
}

fn copy_dir_filtered(src: &Path, dest: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dest)?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let p = ent.path();
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if name_s.starts_with('.') {
            continue;
        }
        let target = dest.join(&name);
        if p.is_dir() {
            // agentskills optional dirs + any nested content one level deep
            copy_dir_filtered(&p, &target)?;
        } else {
            fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

/// Install SKILL.md from an HTTP(S) URL into the skills root.
pub fn install_skill_url(url: &str, dest_root: &Path) -> anyhow::Result<PathBuf> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pirs-claw")
        .build()?;
    let resp = client.get(url).send()?;
    if !resp.status().is_success() {
        anyhow::bail!("skills install: HTTP {} for {url}", resp.status());
    }
    let raw = resp.text()?;
    let tmp_name = url
        .rsplit('/')
        .next()
        .unwrap_or("skill.md")
        .trim_end_matches(".md");
    let path = PathBuf::from(format!("{tmp_name}.md"));
    let sk = parse_skill_md(&raw, &path);
    if let Err(e) = validate_skill(&sk) {
        anyhow::bail!("invalid skill from URL: {e}");
    }
    let dir = dest_root.join(&sk.name);
    fs::create_dir_all(&dir)?;
    let dest = dir.join("SKILL.md");
    fs::write(&dest, raw)?;
    Ok(dest)
}

/// Remove an installed skill directory by name.
pub fn remove_skill(name: &str, dest_root: &Path) -> anyhow::Result<bool> {
    validate_skill_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let dir = dest_root.join(name);
    if !dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// Read a relative file under a skill root (references/, scripts/ notes).
pub fn read_skill_resource(skill: &Skill, rel: &str) -> anyhow::Result<String> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.contains("..") {
        anyhow::bail!("invalid skill resource path");
    }
    let path = skill.root_dir().join(rel);
    if !path.starts_with(skill.root_dir()) {
        anyhow::bail!("path escapes skill root");
    }
    fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// Write or overwrite a skill (create/patch body).
pub fn write_skill(
    dest_root: &Path,
    name: &str,
    description: &str,
    body: &str,
) -> anyhow::Result<PathBuf> {
    validate_skill_name(name).map_err(|e| anyhow::anyhow!(e))?;
    validate_description(description).map_err(|e| anyhow::anyhow!(e))?;
    let dir = dest_root.join(name);
    fs::create_dir_all(&dir)?;
    let content = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n{}\n",
        body.trim()
    );
    let dest = dir.join("SKILL.md");
    fs::write(&dest, content)?;
    Ok(dest)
}

fn usage_path() -> PathBuf {
    default_skills_dir().join("usage.json")
}

/// Bump usage counter for a skill name (best-effort) — call on activation.
pub fn record_usage(name: &str) {
    let path = usage_path();
    let mut map: std::collections::BTreeMap<String, u64> = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    *map.entry(name.to_string()).or_insert(0) += 1;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, serde_json::to_string_pretty(&map).unwrap_or_default());
}

pub fn usage_counts() -> std::collections::BTreeMap<String, u64> {
    fs::read_to_string(usage_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Resolve skills by name from a loaded list (for cron attach).
pub fn select_skills(all: &[Skill], names: &[String]) -> Vec<Skill> {
    names
        .iter()
        .filter_map(|n| find_skill(all, n).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agentskills_frontmatter() {
        let raw = "---\nname: fix-rust\ndescription: when cargo fails\nlicense: MIT\nmetadata:\n  author: pirs\n---\nRun cargo test.\n";
        let sk = parse_skill_md(raw, Path::new("/x/fix-rust/SKILL.md"));
        assert_eq!(sk.name, "fix-rust");
        assert_eq!(sk.description, "when cargo fails");
        assert_eq!(sk.license.as_deref(), Some("MIT"));
        assert!(sk.body.contains("cargo test"));
        assert!(sk.metadata.iter().any(|(k, v)| k == "author" && v == "pirs"));
    }

    #[test]
    fn validate_name_rules() {
        assert!(validate_skill_name("pdf-processing").is_ok());
        assert!(validate_skill_name("PDF").is_err());
        assert!(validate_skill_name("-x").is_err());
        assert!(validate_skill_name("a--b").is_err());
        assert!(validate_skill_name("").is_err());
    }

    #[test]
    fn progressive_section_has_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("my-skill");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("SKILL.md"),
            "---\nname: my-skill\ndescription: demo\n---\nSECRET_BODY_LINE\n",
        )
        .unwrap();
        let skills = load_skills(dir.path());
        let section = skills_prompt_section(&skills);
        assert!(section.contains("my-skill"));
        assert!(section.contains("demo"));
        assert!(
            !section.contains("SECRET_BODY_LINE"),
            "progressive must not dump body: {section}"
        );
        assert!(skills_full_section(&skills).contains("SECRET_BODY_LINE"));
    }

    #[test]
    fn install_skill_from_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("x.md");
        fs::write(
            &src,
            "---\nname: installed-skill\ndescription: d\n---\nBody here.\n",
        )
        .unwrap();
        let dest = install_skill(&src, dest_dir.path()).unwrap();
        assert!(dest.is_file());
        let loaded = load_skills(dest_dir.path());
        assert!(loaded.iter().any(|s| s.name == "installed-skill"));
    }

    #[test]
    fn install_dir_copies_references() {
        let src = tempfile::tempdir().unwrap();
        let dest_root = tempfile::tempdir().unwrap();
        let skill = src.path().join("cool-skill");
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: cool-skill\ndescription: cool\n---\nMain.\n",
        )
        .unwrap();
        fs::write(skill.join("references").join("REF.md"), "detail").unwrap();
        install_skill(&skill, dest_root.path()).unwrap();
        let ref_path = dest_root
            .path()
            .join("cool-skill")
            .join("references")
            .join("REF.md");
        assert_eq!(fs::read_to_string(ref_path).unwrap(), "detail");
    }

    #[test]
    fn write_and_remove_skill() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "learned-thing", "from experience", "step 1").unwrap();
        assert!(find_skill(&load_skills(dir.path()), "learned-thing").is_some());
        assert!(remove_skill("learned-thing", dir.path()).unwrap());
        assert!(load_skills(dir.path()).is_empty());
    }
}
