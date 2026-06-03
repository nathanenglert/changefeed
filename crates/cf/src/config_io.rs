//! Config + secrets disk I/O (ARCHITECTURE.md §1, §12): read `changefeed.toml` +
//! `.changefeed/secrets.env`, `${ENV}`-expand secret-bearing values. The IMPURE config boundary
//! (the pure `cf_core::config::parse` takes already-read, already-expanded bytes).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use cf_core::Config;

use crate::store_sqlite::expand_env;

/// The config file name in a project dir.
pub const CONFIG_FILE: &str = "changefeed.toml";
/// The store dir name.
pub const STORE_DIR: &str = ".changefeed";
/// The secrets file under the store dir.
pub const SECRETS_FILE: &str = "secrets.env";

/// Read `changefeed.toml` from `dir`, expand `${ENV}` references from `.changefeed/secrets.env` +
/// the process environment, then hand the expanded bytes to `cf_core::config::parse`.
pub fn load(dir: &Path) -> Result<Config> {
    let path = dir.join(CONFIG_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let secrets = load_secrets(dir);
    let expanded = expand_env(&raw, &|name| secrets.get(name).cloned());
    let cfg = cf_core::config::parse(&expanded).map_err(anyhow::Error::from)?;
    Ok(cfg)
}

/// Whether a `changefeed.toml` exists in `dir`.
pub fn config_exists(dir: &Path) -> bool {
    dir.join(CONFIG_FILE).is_file()
}

/// Build the secret lookup table: `.changefeed/secrets.env` (KEY=VALUE lines) layered UNDER the
/// process environment (the environment wins, so an operator can override a checked-in secrets file
/// from the shell). Secrets are NEVER written back to the store or logged.
fn load_secrets(dir: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let secrets_path = dir.join(STORE_DIR).join(SECRETS_FILE);
    if let Ok(text) = std::fs::read_to_string(&secrets_path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), unquote(v.trim()).to_string());
            }
        }
    }
    // Process env wins (overlays the file).
    for (k, v) in std::env::vars() {
        map.insert(k, v);
    }
    map
}

fn unquote(s: &str) -> &str {
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    s
}

/// Scaffold a fresh project: write a starter `changefeed.toml` and create `.changefeed/`. Returns an
/// error if either already exists (so `cf init` is not destructive). Idempotency is the caller's
/// concern — the cli reports a clean message.
pub fn scaffold(dir: &Path) -> Result<()> {
    let config_path = dir.join(CONFIG_FILE);
    let store_dir = dir.join(STORE_DIR);
    if !config_path.exists() {
        std::fs::write(&config_path, STARTER_TOML)
            .with_context(|| format!("writing {}", config_path.display()))?;
    }
    std::fs::create_dir_all(&store_dir)
        .with_context(|| format!("creating {}", store_dir.display()))?;
    Ok(())
}

/// The starter `changefeed.toml` written by `cf init`.
pub const STARTER_TOML: &str = r#"# changefeed configuration — see DESIGN.md §4.9.
# Run `cf watch <url>` to append targets, or edit this file directly.

[defaults]
schedule = "15m"
render = "auto"          # auto | never | chromium
timeout = "30s"
respect_robots = true
min_salience = "low"     # none | low | medium | high | critical
store_format = "zstd"

# Example target (uncomment and edit, or use `cf watch`):
# [[target]]
# id = "acme-pricing"
# url = "https://acme.com/pricing"
# archetype = "pricing"
# select = [".PricingTable"]
# ignore = [".live-counter", { attr = "data-csrf-nonce" }]
"#;

/// Append a `[[target]]` block to `changefeed.toml` (the `cf watch` writer). Creates the file (and
/// `.changefeed/`) if missing. The id defaults to a URL-derived slug.
pub fn append_target(
    dir: &Path,
    id: &str,
    url: &str,
    archetype: Option<&str>,
) -> Result<()> {
    let config_path = dir.join(CONFIG_FILE);
    if !config_path.exists() {
        scaffold(dir)?;
    }
    let mut block = String::new();
    block.push_str("\n[[target]]\n");
    block.push_str(&format!("id = {}\n", toml_str(id)));
    block.push_str(&format!("url = {}\n", toml_str(url)));
    if let Some(a) = archetype {
        block.push_str(&format!("archetype = {}\n", toml_str(a)));
    }
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let combined = format!("{existing}{block}");
    std::fs::write(&config_path, combined)
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok(())
}

/// Minimal TOML string escaping for the `cf watch` writer.
fn toml_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_creates_config_and_store_dir() {
        let dir = tempfile::tempdir().unwrap();
        scaffold(dir.path()).unwrap();
        assert!(dir.path().join(CONFIG_FILE).is_file());
        assert!(dir.path().join(STORE_DIR).is_dir());
        // Idempotent: a second scaffold does not error and does not clobber.
        std::fs::write(dir.path().join(CONFIG_FILE), "custom").unwrap();
        scaffold(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            "custom"
        );
    }

    #[test]
    fn append_target_round_trips_via_parse() {
        let dir = tempfile::tempdir().unwrap();
        scaffold(dir.path()).unwrap();
        append_target(dir.path(), "acme", "https://acme.com/pricing", Some("pricing")).unwrap();
        let cfg = load(dir.path()).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets[0].id, "acme");
        assert_eq!(cfg.targets[0].archetype.as_deref(), Some("pricing"));
    }

    #[test]
    fn secrets_expand_from_file_and_env() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(STORE_DIR)).unwrap();
        std::fs::write(
            dir.path().join(STORE_DIR).join(SECRETS_FILE),
            "API_TOKEN=\"tok_from_file\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
[[target]]
id = "secure"
url = "https://x/y"
[target.auth]
header = { Authorization = "Bearer ${API_TOKEN}" }
"#,
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap();
        match &cfg.targets[0].auth {
            Some(cf_core::AuthCfg::Header { headers }) => {
                let (_, v) = &headers[0];
                assert_eq!(v, "Bearer tok_from_file");
            }
            other => panic!("expected header auth, got {other:?}"),
        }
    }
}
