//! Config/Profile model + parse (ARCHITECTURE.md §1, §4.9). Pure: bytes -> `Config`; NO file I/O.
//!
//! `${ENV}` expansion and reading `changefeed.toml` / `.changefeed/secrets.env` from disk happen at
//! the impure cli boundary (`cf::config_io`). This module only parses already-read (already-expanded)
//! bytes.
//!
//! The TOML shape mirrors the §2.5 `Config` model:
//!
//! ```toml
//! [defaults]
//! schedule = "15m"
//! render = "auto"            # auto | never | chromium
//! timeout = "30s"
//! user_agent = "changefeed/1.0 (+https://…)"
//! respect_robots = true
//! min_salience = "low"       # none | low | medium | high | critical
//! store_format = "zstd"
//!
//! [[target]]
//! id = "acme-pricing"
//! url = "https://acme.com/pricing"
//! archetype = "pricing"      # resolves the rule pack + extract strategy preset
//! select = [".PricingTable"] # CSS selectors -> selector strategy
//! ignore = [".live-counter", { attr = "data-csrf-nonce" }, { regex = "\\d+ viewing" }]
//! salience_hints = ["per seat"]
//! render = "never"           # per-target override (last-wins over the archetype preset + defaults)
//!
//! [target.auth]              # optional; ${ENV}-expanded at the cli boundary, redacted from logs
//! header = { Authorization = "Bearer ${API_TOKEN}" }
//! ```
//!
//! Resolution is **archetype preset → per-target overrides (last-wins)**: each target's `Profile`
//! starts from the archetype's preset (strategy/render/strip/types), then per-target keys win.

use crate::model::{
    AuthCfg, Config, Defaults, Duration, ExtractStrategy, IgnoreRule, Materiality, Profile,
    RenderMode, SinkCfg, SourceMode, TargetCfg,
};
use crate::CfError;
use serde::Deserialize;

/// Parse `changefeed.toml` bytes into a resolved `Config` (archetype preset + per-target
/// overrides, last-wins). NO disk access — bytes in, `Config` out.
pub fn parse(toml_src: &str) -> Result<Config, CfError> {
    let raw: RawConfig =
        toml::from_str(toml_src).map_err(|e| CfError::Usage(format!("changefeed.toml: {e}")))?;

    let defaults = resolve_defaults(raw.defaults)?;

    let mut targets = Vec::with_capacity(raw.target.len());
    let mut seen_ids = std::collections::HashSet::new();
    for rt in raw.target {
        let id = rt.id.clone();
        if !seen_ids.insert(id.clone()) {
            return Err(CfError::Usage(format!("duplicate target id {id:?}")));
        }
        targets.push(resolve_target(rt, &defaults)?);
    }

    let sinks = raw
        .sink
        .into_iter()
        .map(resolve_sink)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Config {
        defaults,
        sinks,
        targets,
    })
}

/// Build a single ad-hoc `TargetCfg` (the `cf check <url>` no-config path). `id` is derived from the
/// URL host+path when not supplied. The profile uses the archetype preset (or `default`).
pub fn ad_hoc_target(
    url: &str,
    id: Option<&str>,
    archetype: Option<&str>,
    select: &[String],
    defaults: &Defaults,
) -> TargetCfg {
    let id = id.map(|s| s.to_string()).unwrap_or_else(|| derive_tid(url));
    // With an explicit selector, use the deterministic selector path; otherwise an ad-hoc target has
    // no root_selector, so a selector-preset archetype (`pricing`/`status-page`) would have nothing
    // to scope to — fall back to `full` (keep <body> minus boilerplate), which is still
    // deterministic and never errors on a missing root_selector.
    let strategy = if select.is_empty() {
        match preset_strategy(archetype) {
            ExtractStrategy::Selector => ExtractStrategy::Full,
            other => other,
        }
    } else {
        ExtractStrategy::Selector
    };
    let profile = Profile {
        profile_id: id.clone(),
        render: defaults.render,
        strategy,
        root_selector: select.first().cloned(),
        strip_attrs: Vec::new(),
        strip_text: Vec::new(),
        unordered: Vec::new(),
        mode: SourceMode::Page,
        max_pages: 1,
        types: Vec::new(),
        archetype: archetype.map(|s| s.to_string()),
        salience_hints: Vec::new(),
    };
    TargetCfg {
        id,
        url: url.to_string(),
        schedule: None,
        archetype: archetype.map(|s| s.to_string()),
        select: select.to_vec(),
        ignore: Vec::new(),
        salience_hints: Vec::new(),
        auth: None,
        profile,
    }
}

/// Derive a stable target id from a URL (host + path slug). Deterministic, no clock.
pub fn derive_tid(url: &str) -> String {
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let slug: String = trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "target".to_string()
    } else {
        slug
    }
}

// ===========================================================================================
// Resolution helpers.
// ===========================================================================================

fn resolve_defaults(raw: Option<RawDefaults>) -> Result<Defaults, CfError> {
    let raw = raw.unwrap_or_default();
    Ok(Defaults {
        schedule: parse_duration(raw.schedule.as_deref().unwrap_or("15m"))?,
        render: parse_render(raw.render.as_deref().unwrap_or("auto"))?,
        timeout: parse_duration(raw.timeout.as_deref().unwrap_or("30s"))?,
        user_agent: raw
            .user_agent
            .unwrap_or_else(|| "changefeed/1.0 (+https://github.com/changefeed/changefeed)".into()),
        respect_robots: raw.respect_robots.unwrap_or(true),
        min_salience: parse_materiality(raw.min_salience.as_deref().unwrap_or("low"))?,
        store_format: raw.store_format.unwrap_or_else(|| "zstd".into()),
    })
}

fn resolve_target(rt: RawTarget, defaults: &Defaults) -> Result<TargetCfg, CfError> {
    if rt.url.trim().is_empty() {
        return Err(CfError::Usage(format!("target {:?}: empty url", rt.id)));
    }

    // Archetype preset → per-target override (last-wins).
    let render = match rt.render.as_deref() {
        Some(s) => parse_render(s)?,
        None => defaults.render,
    };
    let has_root = rt.root_selector.is_some() || !rt.select.is_empty();
    let strategy = match rt.strategy.as_deref() {
        Some(s) => parse_strategy(s)?,
        None if !rt.select.is_empty() => ExtractStrategy::Selector,
        // A selector-preset archetype (`pricing`/`status-page`) with NO `select`/`root_selector`
        // would have nothing to scope to and would fail extraction; fall back to `full` (still
        // deterministic, keeps <body> minus boilerplate).
        None => match preset_strategy(rt.archetype.as_deref()) {
            ExtractStrategy::Selector if !has_root => ExtractStrategy::Full,
            other => other,
        },
    };
    let ignore = rt
        .ignore
        .into_iter()
        .map(parse_ignore_rule)
        .collect::<Result<Vec<_>, _>>()?;
    let strip_attrs: Vec<String> = ignore
        .iter()
        .filter_map(|r| match r {
            IgnoreRule::Attr(a) => Some(a.clone()),
            _ => None,
        })
        .collect();
    let strip_text: Vec<String> = ignore
        .iter()
        .filter_map(|r| match r {
            IgnoreRule::Regex(re) => Some(re.clone()),
            _ => None,
        })
        .collect();
    let types = rt
        .types
        .into_iter()
        .map(|(sel, ty)| Ok::<_, CfError>((sel, parse_block_type(&ty)?)))
        .collect::<Result<Vec<_>, _>>()?;

    let schedule = match rt.schedule.as_deref() {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };

    let auth = match rt.auth {
        Some(a) => Some(resolve_auth(a)?),
        None => None,
    };

    let profile = Profile {
        profile_id: rt.id.clone(),
        render,
        strategy,
        root_selector: rt.root_selector.or_else(|| rt.select.first().cloned()),
        strip_attrs,
        strip_text,
        unordered: rt.unordered,
        mode: SourceMode::Page,
        max_pages: 1,
        types,
        archetype: rt.archetype.clone(),
        salience_hints: rt.salience_hints.clone(),
    };

    Ok(TargetCfg {
        id: rt.id,
        url: rt.url,
        schedule,
        archetype: rt.archetype,
        select: rt.select,
        ignore,
        salience_hints: rt.salience_hints,
        auth,
        profile,
    })
}

/// The default extract strategy for an archetype (structured archetypes prefer the deterministic
/// selector path; otherwise readability for prose).
fn preset_strategy(archetype: Option<&str>) -> ExtractStrategy {
    match archetype {
        Some("pricing") | Some("status-page") => ExtractStrategy::Selector,
        Some("api-docs") => ExtractStrategy::Readability,
        _ => ExtractStrategy::Readability,
    }
}

fn resolve_auth(a: RawAuth) -> Result<AuthCfg, CfError> {
    if let Some(headers) = a.header {
        return Ok(AuthCfg::Header {
            headers: headers.into_iter().collect(),
        });
    }
    if let Some(cookies) = a.cookie {
        return Ok(AuthCfg::Cookie { cookies });
    }
    if let (Some(username), Some(password)) = (a.username.clone(), a.password.clone()) {
        return Ok(AuthCfg::Basic { username, password });
    }
    if a.browser.unwrap_or(false) {
        return Ok(AuthCfg::Browser);
    }
    Err(CfError::Usage(
        "target auth: expected one of header/cookie/basic(username+password)/browser".into(),
    ))
}

fn resolve_sink(s: RawSink) -> Result<SinkCfg, CfError> {
    Ok(SinkCfg {
        kind: s.kind.unwrap_or_else(|| "jsonl".into()),
        path_or_url: s.path.or(s.url).unwrap_or_default(),
        headers: s.headers.into_iter().collect(),
        min_salience: parse_materiality(s.min_salience.as_deref().unwrap_or("low"))?,
    })
}

// ===========================================================================================
// Scalar parsers (shared with the cli flag parsing where useful).
// ===========================================================================================

/// Parse a duration like `15m` / `30s` / `1h` into the clock-free `Duration` (whole seconds).
pub fn parse_duration(s: &str) -> Result<Duration, CfError> {
    let s = s.trim();
    // Accept a bare integer as seconds for convenience.
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len()),
    );
    let value: f64 = num
        .parse()
        .map_err(|_| CfError::Usage(format!("bad duration {s:?}")))?;
    let mult = match unit.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        other => return Err(CfError::Usage(format!("unknown duration unit {other:?}"))),
    };
    Ok(Duration::from_secs((value * mult) as u64))
}

/// Parse a `render` token.
pub fn parse_render(s: &str) -> Result<RenderMode, CfError> {
    Ok(match s.trim() {
        "auto" => RenderMode::Auto,
        "never" => RenderMode::Never,
        "chromium" => RenderMode::Chromium,
        other => return Err(CfError::Usage(format!("unknown render mode {other:?}"))),
    })
}

/// Parse an `ExtractStrategy` token.
pub fn parse_strategy(s: &str) -> Result<ExtractStrategy, CfError> {
    Ok(match s.trim() {
        "readability" => ExtractStrategy::Readability,
        "selector" => ExtractStrategy::Selector,
        "full" => ExtractStrategy::Full,
        other => return Err(CfError::Usage(format!("unknown extract strategy {other:?}"))),
    })
}

/// Parse a `min_salience` / materiality token.
pub fn parse_materiality(s: &str) -> Result<Materiality, CfError> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "none" => Materiality::None,
        "low" => Materiality::Low,
        "medium" | "med" => Materiality::Medium,
        "high" => Materiality::High,
        "critical" | "crit" => Materiality::Critical,
        other => return Err(CfError::Usage(format!("unknown salience level {other:?}"))),
    })
}

fn parse_block_type(s: &str) -> Result<crate::model::BlockType, CfError> {
    use crate::model::BlockType;
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "heading" => BlockType::Heading,
        "paragraph" => BlockType::Paragraph,
        "list_item" | "listitem" => BlockType::ListItem,
        "table_row" | "tablerow" => BlockType::TableRow,
        "table" => BlockType::Table,
        "code" => BlockType::Code,
        "link" => BlockType::Link,
        "price" => BlockType::Price,
        "date" => BlockType::Date,
        "number" => BlockType::Number,
        "text" => BlockType::Text,
        other => return Err(CfError::Usage(format!("unknown block type {other:?}"))),
    })
}

fn parse_ignore_rule(raw: RawIgnore) -> Result<IgnoreRule, CfError> {
    match raw {
        RawIgnore::Selector(s) => Ok(IgnoreRule::Selector(s)),
        RawIgnore::Object { attr, regex, selector } => {
            if let Some(a) = attr {
                Ok(IgnoreRule::Attr(a))
            } else if let Some(r) = regex {
                Ok(IgnoreRule::Regex(r))
            } else if let Some(s) = selector {
                Ok(IgnoreRule::Selector(s))
            } else {
                Err(CfError::Usage(
                    "ignore rule object: expected one of selector/attr/regex".into(),
                ))
            }
        }
    }
}

// ===========================================================================================
// TOML deserialization model.
// ===========================================================================================

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    defaults: Option<RawDefaults>,
    #[serde(default)]
    target: Vec<RawTarget>,
    #[serde(default)]
    sink: Vec<RawSink>,
}

#[derive(Deserialize, Default)]
struct RawDefaults {
    schedule: Option<String>,
    render: Option<String>,
    timeout: Option<String>,
    user_agent: Option<String>,
    respect_robots: Option<bool>,
    min_salience: Option<String>,
    store_format: Option<String>,
}

#[derive(Deserialize)]
struct RawTarget {
    id: String,
    url: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    archetype: Option<String>,
    #[serde(default)]
    select: Vec<String>,
    #[serde(default)]
    root_selector: Option<String>,
    #[serde(default)]
    ignore: Vec<RawIgnore>,
    #[serde(default)]
    salience_hints: Vec<String>,
    #[serde(default)]
    render: Option<String>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    unordered: Vec<String>,
    /// CSS-selector -> forced block type overrides (`[target.types]` table).
    #[serde(default)]
    types: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    auth: Option<RawAuth>,
}

/// An ignore rule is either a bare selector string or an object `{ attr | regex | selector }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawIgnore {
    Selector(String),
    Object {
        #[serde(default)]
        attr: Option<String>,
        #[serde(default)]
        regex: Option<String>,
        #[serde(default)]
        selector: Option<String>,
    },
}

#[derive(Deserialize)]
struct RawAuth {
    #[serde(default)]
    header: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    cookie: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    browser: Option<bool>,
}

#[derive(Deserialize)]
struct RawSink {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    min_salience: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[defaults]
schedule = "15m"
render = "auto"
timeout = "30s"
min_salience = "medium"

[[target]]
id = "acme-pricing"
url = "https://acme.com/pricing"
archetype = "pricing"
select = [".PricingTable"]
ignore = [".live-counter", { attr = "data-csrf-nonce" }, { regex = "\\d+ viewing" }]
salience_hints = ["per seat"]

[target.auth]
header = { Authorization = "Bearer SECRET" }

[[target]]
id = "docs"
url = "https://acme.com/docs"
archetype = "api-docs"
render = "never"
"#;

    #[test]
    fn parses_defaults_and_targets() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.defaults.schedule.as_secs(), 900);
        assert_eq!(cfg.defaults.timeout.as_secs(), 30);
        assert_eq!(cfg.defaults.min_salience, Materiality::Medium);
        assert_eq!(cfg.targets.len(), 2);

        let pricing = &cfg.targets[0];
        assert_eq!(pricing.id, "acme-pricing");
        assert_eq!(pricing.url, "https://acme.com/pricing");
        assert_eq!(pricing.archetype.as_deref(), Some("pricing"));
        // A non-empty `select` forces the selector strategy.
        assert_eq!(pricing.profile.strategy, ExtractStrategy::Selector);
        assert_eq!(pricing.profile.root_selector.as_deref(), Some(".PricingTable"));
        assert_eq!(pricing.ignore.len(), 3);
        assert!(matches!(pricing.ignore[0], IgnoreRule::Selector(_)));
        assert!(matches!(pricing.ignore[1], IgnoreRule::Attr(_)));
        assert!(matches!(pricing.ignore[2], IgnoreRule::Regex(_)));
        // attr/regex ignore rules also surface as profile strip lists.
        assert_eq!(pricing.profile.strip_attrs, vec!["data-csrf-nonce".to_string()]);
        assert_eq!(pricing.profile.strip_text.len(), 1);
        assert!(matches!(pricing.auth, Some(AuthCfg::Header { .. })));

        let docs = &cfg.targets[1];
        // No select + api-docs preset -> readability; per-target render override wins.
        assert_eq!(docs.profile.strategy, ExtractStrategy::Readability);
        assert_eq!(docs.profile.render, RenderMode::Never);
    }

    #[test]
    fn duplicate_target_id_is_usage_error() {
        let src = r#"
[[target]]
id = "dup"
url = "https://a/1"
[[target]]
id = "dup"
url = "https://a/2"
"#;
        let err = parse(src).unwrap_err();
        assert!(matches!(err, CfError::Usage(_)));
    }

    #[test]
    fn malformed_toml_is_usage_error() {
        let err = parse("this is = not [valid").unwrap_err();
        assert!(matches!(err, CfError::Usage(_)));
    }

    #[test]
    fn durations_parse_units() {
        assert_eq!(parse_duration("15m").unwrap().as_secs(), 900);
        assert_eq!(parse_duration("2h").unwrap().as_secs(), 7200);
        assert_eq!(parse_duration("45s").unwrap().as_secs(), 45);
        assert_eq!(parse_duration("60").unwrap().as_secs(), 60);
        assert!(parse_duration("12x").is_err());
    }

    #[test]
    fn derive_tid_is_deterministic_slug() {
        assert_eq!(derive_tid("https://acme.com/pricing"), "acme-com-pricing");
        assert_eq!(derive_tid("https://acme.com/pricing"), derive_tid("https://acme.com/pricing"));
    }

    #[test]
    fn empty_config_is_valid() {
        let cfg = parse("").unwrap();
        assert!(cfg.targets.is_empty());
        assert_eq!(cfg.defaults.min_salience, Materiality::Low);
    }
}
