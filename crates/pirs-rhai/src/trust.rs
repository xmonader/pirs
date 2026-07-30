//! Script directory trust store and interactive prompt.
use std::path::Path;

pub enum TrustDecision {
    Allow,
    Deny,
    Skip,
}

pub(crate) fn trust_store_path() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::Path::new(&h).join(".pirs").join("trusted.json"))
}

pub(crate) fn load_trusted() -> std::collections::HashSet<String> {
    let Some(path) = trust_store_path() else {
        return std::collections::HashSet::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

pub fn trust_directory(dir: &Path) -> Result<(), String> {
    let canonical = dir.canonicalize().map_err(|e| e.to_string())?;
    let ext_dir = canonical.join(".pirs").join("extensions");
    if !ext_dir.exists() {
        return Err(format!(
            "{} has no .pirs/extensions directory",
            canonical.display()
        ));
    }
    // Store the same key prompt_trust looks up at load time: the canonical
    // extensions directory itself.
    let key = ext_dir.canonicalize().unwrap_or(ext_dir);
    save_trusted_key(trust_key(&key));
    Ok(())
}

pub(crate) fn save_trusted_key(key: String) {
    let Some(path) = trust_store_path() else {
        return;
    };
    let mut set = load_trusted();
    set.insert(key);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&set).unwrap_or_default(),
    );
}

pub(crate) fn scripts_hash(dir: &Path) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rhai"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for f in files {
        h.update(f.to_string_lossy().as_bytes());
        if let Ok(content) = std::fs::read(&f) {
            h.update(&content);
        }
    }
    format!("{:x}", h.finalize())
}

pub(crate) fn trust_key(dir: &Path) -> String {
    format!("{}#{}", dir.display(), scripts_hash(dir))
}

pub(crate) fn prompt_trust(dir: &Path) -> TrustDecision {
    if !dir.exists() {
        return TrustDecision::Skip;
    }
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    // Only pirs' own global dir is implicitly trusted; everything else asks.
    let home_ext =
        std::env::var("HOME").map(|h| std::path::Path::new(&h).join(".pirs").join("extensions"));
    if home_ext
        .as_ref()
        .map(|h| h.canonicalize().unwrap_or_else(|_| h.clone()))
        == Ok(canonical.clone())
    {
        return TrustDecision::Allow;
    }
    let trusted = load_trusted();
    if trusted.contains(&trust_key(&canonical))
        || trusted.contains(&canonical.display().to_string())
        || canonical
            .parent()
            .map(|p| trusted.contains(&p.display().to_string()))
            .unwrap_or(false)
    {
        return TrustDecision::Allow;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return TrustDecision::Deny;
    }
    // Show what you're granting, not "full permissions y/N": each script's
    // capability manifest (or its absence) is part of the prompt.
    let mut caps_lines = String::new();
    if let Ok(rd) = std::fs::read_dir(&canonical) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("rhai") {
                continue;
            }
            if let Ok(src) = std::fs::read_to_string(&p) {
                let c = crate::caps::parse_caps(&src);
                caps_lines.push_str(&format!(
                    "  {}: {}\n",
                    p.file_name().and_then(|f| f.to_str()).unwrap_or("?"),
                    c.summary()
                ));
            }
        }
    }
    eprintln!(
        "\nProject extensions found at {}\n{}\nThey run with the permissions shown above (tools, hooks, shell). Trust this directory? [y/N]",
        canonical.display(),
        if caps_lines.is_empty() {
            "  (no scripts found)\n".to_string()
        } else {
            caps_lines
        }
    );
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return TrustDecision::Deny;
    }
    if matches!(line.trim(), "y" | "yes" | "Y") {
        save_trusted_key(trust_key(&canonical));
        TrustDecision::Allow
    } else {
        TrustDecision::Deny
    }
}
