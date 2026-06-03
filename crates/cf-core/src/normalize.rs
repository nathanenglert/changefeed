//! §5.3 normalize — the volatile-strip set (the no-false-positive linchpin): nonce/csrf/high-
//! entropy strip, cache-buster collapse, URL canonicalization (utm-swap-is-noise), NFC/whitespace,
//! relative-time → `⟦TS⟧`. Pure: `fn(DomSubtree, &Profile) -> Result<NormalizedDom, CfError>`.
//!
//! This is where we earn the "no false-positive diffs" promise (DESIGN §5.3). EVERYTHING stripped
//! here is removed *before* `norm_hash`/`doc_hash` are computed downstream, so a page whose only
//! run-to-run difference is a rotating nonce, cache-buster, or relative timestamp normalizes to a
//! BYTE-IDENTICAL [`NormalizedDom`] and hits the §5.6 no-store short-circuit.
//!
//! Determinism (ARCHITECTURE §4): no clock, no RNG, no I/O. The relative-time rule is purely
//! pattern-based (it never reads `now()`); it rewrites the *shape* of a timestamp to a fixed
//! sentinel, so two observations a day apart produce identical normalized text.
//!
//! ## §7.0 interplay (ignore masking)
//!
//! Ignore masking (DESIGN §7.0) runs in the diff engine, *after* normalize, and is the per-target
//! escape hatch: `attr` rules strip a named attribute and `regex` rules redact a text span (with
//! the `￼` sentinel) before the block hash. Normalize is the *global, profile-independent* layer
//! beneath that — it strips the universally-volatile set on every target. The two are additive:
//! normalize handles nonces/cache-busters/framework-churn/relative-time everywhere; §7.0 ignore
//! rules handle the per-target long tail. To keep that seam clean, normalize's strip set is itself
//! profile-extensible via [`Profile::strip_attrs`] (extra attribute names to drop) and
//! [`Profile::strip_text`] (extra regex sources whose matches are replaced with the `⟦TS⟧`
//! sentinel), so a profile can promote a site-specific volatile attribute/text into the global pass
//! without waiting for a §7.0 ignore rule.

use std::collections::BTreeSet;

use unicode_normalization::UnicodeNormalization;
use url::Url;

use crate::extract::{DomSubtree, ExtractNode};
use crate::model::Profile;
use crate::CfError;

/// The relative/volatile-time placeholder (DESIGN §5.3). A fixed sentinel so two observations whose
/// only difference is "3 minutes ago" vs "5 minutes ago" normalize identically. The doc uses the
/// bracketed-TS glyph `⟦TS⟧` (U+27E6 … U+27E7).
pub const TS_SENTINEL: &str = "⟦TS⟧";

/// Shannon-entropy gate (bits/char) for the high-entropy token rule (DESIGN §5.3). A token whose
/// per-character entropy exceeds this is treated as a generated token (session id / nonce / build
/// hash) regardless of its length.
const ENTROPY_BITS_PER_CHAR: f64 = 3.5;

/// Minimum length for the structural high-entropy token rule `^[A-Za-z0-9_-]{24,}$` (DESIGN §5.3).
const HIGH_ENTROPY_MIN_LEN: usize = 24;

/// The normalized DOM produced by §5.3 — handed to segment. Two pages differing ONLY by volatile
/// tokens MUST normalize to an identical `NormalizedDom` (the §5.3 → §5.6 linchpin).
///
/// The authoritative output is [`NormalizedDom::roots`] — the normalized element forest that segment
/// walks. `html` is a deterministic serialization of that forest, retained for diagnostics/storage
/// and so the §5.6 `doc_hash` short-circuit has a byte-comparable representation in tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NormalizedDom {
    /// The normalized element forest (the real output — a tree, not a string): text NFC-composed and
    /// whitespace-collapsed (code/pre exempt), volatile attributes stripped, URLs canonicalized.
    pub roots: Vec<ExtractNode>,
    /// Deterministic serialization of `roots` (diagnostics/storage). Byte-identical iff `roots` is.
    pub html: String,
    /// Count of attributes stripped during normalization (added to `DocStats.stripped_attrs`).
    /// Exposed as `normalize.stripped_count` (DESIGN §5.3) so a 90%-volatile mis-tuned profile is
    /// visible.
    pub stripped_attrs: u32,
    /// Raw byte length carried through from extract (feeds `DocStats.bytes_raw`).
    pub bytes_raw: u64,
}

/// §5.3 — strip volatile tokens, NFC-normalize, collapse whitespace, canonicalize URLs.
///
/// The `final base` for URL resolution is the post-redirect document URL; the cli threads it in via
/// [`Profile`]-independent state in later wiring. For the pure stage we resolve against the
/// best-available base in priority order: a `<base href>` element found in the subtree, else any
/// absolute URL we encounter, else relative URLs are left as-is (canonicalized only for casing /
/// query-sorting / tracking-strip, which is still deterministic). Absolute URLs always canonicalize
/// fully.
pub fn normalize(subtree: DomSubtree, profile: &Profile) -> Result<NormalizedDom, CfError> {
    let DomSubtree {
        roots,
        html: _,
        stripped_attrs: _, // element-level prune count from extract is NOT carried into the
        // attribute-strip count; normalize reports its OWN stripped-attr count (the §5.3 metric).
        bytes_raw,
    } = subtree;

    let rules = StripRules::new(profile)?;

    // Determine the base URL for relative→absolute resolution: a <base href> in the subtree wins.
    let base = find_base_href(&roots).and_then(|href| Url::parse(&href).ok());

    let mut stripped = 0u32;
    let normalized: Vec<ExtractNode> = roots
        .into_iter()
        .map(|node| normalize_node(node, &rules, base.as_ref(), false, &mut stripped))
        .collect();

    let mut html = String::new();
    for r in &normalized {
        write_html(r, &mut html);
    }

    Ok(NormalizedDom {
        roots: normalized,
        html,
        stripped_attrs: stripped,
        bytes_raw,
    })
}

// ===========================================================================================
// Recursive node normalization.
// ===========================================================================================

/// `in_code` is set once we enter a `<pre>`/`<code>` subtree: its text whitespace is load-bearing
/// and exempt from collapse/trim (DESIGN §5.3), though NFC + entity decoding still apply.
fn normalize_node(
    node: ExtractNode,
    rules: &StripRules,
    base: Option<&Url>,
    in_code: bool,
    stripped: &mut u32,
) -> ExtractNode {
    match node {
        ExtractNode::Text(t) => ExtractNode::Text(normalize_text(&t, rules, in_code)),
        ExtractNode::Element {
            name,
            attrs,
            children,
        } => {
            let now_in_code = in_code || name == "pre" || name == "code";

            // Strip the volatile attribute set; canonicalize URL-bearing attributes.
            let mut kept: Vec<(String, String)> = Vec::with_capacity(attrs.len());
            for (k, v) in attrs {
                if rules.is_volatile_attr(&k, &v) {
                    *stripped += 1;
                    continue;
                }
                if is_url_attr(&k) {
                    kept.push((k, canonicalize_url(&v, base)));
                } else {
                    kept.push((k, v));
                }
            }
            // Keep attrs sorted by name (extract's invariant) for deterministic serialization.
            kept.sort_by(|a, b| a.0.cmp(&b.0));

            let children = children
                .into_iter()
                .map(|c| normalize_node(c, rules, base, now_in_code, stripped))
                .collect();

            ExtractNode::Element {
                name,
                attrs: kept,
                children,
            }
        }
    }
}

// ===========================================================================================
// Text normalization: entities (parser-decoded) → NFC → whitespace collapse → relative-time.
// ===========================================================================================

/// Normalize a single text node. Entities are already decoded by html5ever at parse time (§5.2), so
/// we do NOT re-decode (a second decode would corrupt a literal `&amp;` the author wrote as
/// `&amp;amp;`). We then NFC-compose, collapse whitespace (unless `in_code`), and replace
/// relative-time spans with the [`TS_SENTINEL`].
fn normalize_text(text: &str, rules: &StripRules, in_code: bool) -> String {
    // 1) Unicode NFC. Composes decomposed sequences (e.g. e + U+0301 → é) so visually-identical
    //    text hashes identically regardless of the source's normalization form. ASCII is already
    //    in NFC by definition, so we skip the composing iterator's allocation for the common
    //    all-ASCII text node (behavior-identical: NFC(ascii) == ascii).
    let composed: std::borrow::Cow<str> = if text.is_ascii() {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.nfc().collect())
    };

    if in_code {
        // Code/pre: whitespace is load-bearing — NFC only, NO collapse/trim, NO relative-time
        // rewrite (a literal "3 minutes ago" inside a code sample is content, not chrome).
        return composed.into_owned();
    }

    // 2) Collapse all whitespace (incl. NBSP U+00A0 and zero-width U+200B/U+FEFF) to one ASCII
    //    space, then trim. We treat zero-width chars as whitespace that VANISHES (they collapse
    //    with their neighbors rather than becoming a visible space).
    let collapsed = collapse_whitespace(&composed);

    // 3) Relative-time → sentinel (and profile.strip_text → sentinel).
    rules.replace_volatile_text(&collapsed)
}

/// Collapse runs of whitespace — Unicode whitespace plus NBSP (U+00A0) — to a single ASCII space,
/// dropping zero-width characters (U+200B ZERO WIDTH SPACE, U+FEFF ZERO WIDTH NO-BREAK SPACE /
/// BOM, U+200C/U+200D zero-width (non-)joiners) entirely, then trim leading/trailing space.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    let mut wrote_any = false;
    for ch in s.chars() {
        if is_zero_width(ch) {
            // Vanishes: neither a space nor content. Adjacent text fuses.
            continue;
        }
        if is_collapsible_ws(ch) {
            // Defer: only emit a space if real content follows.
            if wrote_any {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        wrote_any = true;
    }
    out
}

/// Whitespace that collapses to a single ASCII space. NBSP (U+00A0) is included (DESIGN §5.3);
/// `char::is_whitespace` already covers NBSP and the other Unicode space separators, but we spell
/// out NBSP for clarity and keep the ASCII controls.
#[inline]
fn is_collapsible_ws(ch: char) -> bool {
    ch == '\u{00A0}' || ch.is_whitespace()
}

/// Zero-width characters that are dropped entirely (DESIGN §5.3 calls out U+200B and U+FEFF; we also
/// drop the zero-width joiner/non-joiner which carry no visible content for diffing purposes).
#[inline]
fn is_zero_width(ch: char) -> bool {
    matches!(ch, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}')
}

// ===========================================================================================
// URL canonicalization (DESIGN §5.3): relative→absolute, lowercase scheme+host, drop default
// ports, sort query params, strip tracking params, drop fragments.
// ===========================================================================================

/// Attribute names that carry a URL we canonicalize (DESIGN §5.3 / §7.2 attr-delta path).
fn is_url_attr(name: &str) -> bool {
    matches!(name, "href" | "src" | "poster" | "cite" | "action" | "data")
}

/// Tracking query parameters stripped wholesale (DESIGN §5.3): `utm_*`, `gclid`, `fbclid`, `ref`,
/// `_ga`. A `utm`-only difference between two observations is therefore a non-event.
fn is_tracking_param(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.starts_with("utm_") || matches!(k.as_str(), "gclid" | "fbclid" | "ref" | "_ga")
}

/// A `?v=`/`?hash=` cache-buster query param (DESIGN §5.3 lists these alongside fingerprinted asset
/// paths as volatile churn stripped before the §5.6 doc_hash). We gate on the VALUE shape so a
/// *semantic* key — e.g. a `youtube.com/watch?v=<opaque-id>` resource handle — is preserved while a
/// `?v=12345 → ?v=99999` or `?hash=4f3a2b` rotation is stripped. Cache-buster shapes: all-digits, a
/// dotted numeric version (`1.2.3`), or a hex token (≥4 chars, all hex digits — a typical build hash).
fn is_cache_buster_param(key: &str, value: &str) -> bool {
    let k = key.to_ascii_lowercase();
    if !matches!(k.as_str(), "v" | "hash") {
        return false;
    }
    looks_like_cache_buster_value(value)
}

/// True if a query value is version/build/hash-shaped (see [`is_cache_buster_param`]). An opaque id
/// carrying non-hex letters (e.g. `dQw4w9WgXcQ`) is NOT cache-buster-shaped and is preserved.
fn looks_like_cache_buster_value(v: &str) -> bool {
    if v.is_empty() {
        return false;
    }
    // all-digits or a dotted numeric version like `1.2.3`
    if v.chars().all(|c| c.is_ascii_digit() || c == '.') && v.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    // a hex build/content hash (≥4 chars, all hex digits)
    v.len() >= 4 && v.chars().all(|c| c.is_ascii_hexdigit())
}

/// Canonicalize a single URL string (DESIGN §5.3). Order of operations:
/// 1. strip the cache-buster / fingerprint shape from the *path* (`app.4f3a2b1.js` → `app.js`);
/// 2. resolve relative → absolute against `base` when possible;
/// 3. lowercase scheme + host, drop the default port, sort query params, strip tracking params,
///    drop the fragment.
///
/// If the value does not parse as a URL even after base-resolution (e.g. a `mailto:`/`tel:` or a
/// bare relative path with no base), we still apply the deterministic textual rules we safely can
/// (fingerprint strip + fragment drop + query sort/strip) so the result is stable run-to-run.
fn canonicalize_url(raw: &str, base: Option<&Url>) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }

    // Try to resolve to an absolute URL: parse directly, else join onto the base.
    let parsed = Url::parse(raw)
        .ok()
        .or_else(|| base.and_then(|b| b.join(raw).ok()));

    match parsed {
        Some(mut url) => {
            // scheme + host lowercasing is handled by the `url` crate already; default-port drop:
            if is_default_port(&url) {
                let _ = url.set_port(None);
            }
            url.set_fragment(None);
            canonicalize_path(&mut url);
            sort_and_strip_query(&mut url);
            // `url` lowercases the host and scheme on parse; emit without a trailing default port.
            url.to_string()
        }
        None => {
            // Non-URL (mailto:, tel:, data:, or relative with no base): apply only the safe textual
            // rules so the value is still deterministic. Drop fragment, strip fingerprint on the
            // path-ish portion, sort/strip any query.
            canonicalize_unparsed(raw)
        }
    }
}

/// True if `url` is on its scheme's default port (so we can drop it).
fn is_default_port(url: &Url) -> bool {
    match (url.scheme(), url.port()) {
        (_, None) => false,
        (s, Some(p)) => url::Url::parse(&format!("{s}://x"))
            .ok()
            .and_then(|u| u.port_or_known_default())
            .map(|def| def == p)
            .unwrap_or(false),
    }
}

/// Rewrite a fingerprinted asset filename in the URL path: `app.4f3a2b1.js` → `app.js`.
fn canonicalize_path(url: &mut Url) {
    let path = url.path().to_string();
    let new = strip_fingerprint_path(&path);
    if new != path {
        url.set_path(&new);
    }
}

/// Sort query parameters by `(key, value)` and strip tracking params (DESIGN §5.3). An empty result
/// clears the query entirely (so `?utm_source=x` alone becomes no query at all).
fn sort_and_strip_query(url: &mut Url) {
    let Some(_) = url.query() else {
        return;
    };
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, v)| !is_tracking_param(k) && !is_cache_buster_param(k, v))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    if pairs.is_empty() {
        url.set_query(None);
    } else {
        let mut ser = url.query_pairs_mut();
        ser.clear();
        for (k, v) in &pairs {
            ser.append_pair(k, v);
        }
        drop(ser);
    }
}

/// Best-effort canonicalization for a value that does not parse as an absolute URL and has no base.
/// Deterministic textual rules only: drop the fragment, strip the fingerprint from the last path
/// segment, sort/strip query params.
fn canonicalize_unparsed(raw: &str) -> String {
    // Split off fragment.
    let (no_frag, _frag) = match raw.split_once('#') {
        Some((a, b)) => (a, Some(b)),
        None => (raw, None),
    };
    // Split path?query.
    let (path, query) = match no_frag.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (no_frag, None),
    };

    let path = strip_fingerprint_path(path);

    let query_out = query.and_then(|q| {
        let mut pairs: Vec<(&str, &str)> = q
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (k, v),
                None => (p, ""),
            })
            .filter(|(k, v)| !is_tracking_param(k) && !is_cache_buster_param(k, v))
            .collect();
        if pairs.is_empty() {
            return None;
        }
        pairs.sort();
        Some(
            pairs
                .into_iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .collect::<Vec<_>>()
                .join("&"),
        )
    });

    match query_out {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

/// Strip a fingerprint segment from the LAST path component: `app.4f3a2b1.js` → `app.js`,
/// `/static/main.0a9f3c.css` → `/static/main.css`. We only treat the *middle* segment of a
/// `name.<hash>.ext` filename as a fingerprint, and only when `<hash>` looks like a content hash
/// (≥6 chars of `[0-9a-f]` with at least one digit, so a real word like `min` in `jquery.min.js` is
/// NOT stripped).
fn strip_fingerprint_path(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, file)) => {
            let stripped = strip_fingerprint_filename(file);
            format!("{dir}/{stripped}")
        }
        None => strip_fingerprint_filename(path),
    }
}

/// `app.4f3a2b1.js` → `app.js`. Requires the form `<name>.<hash>.<ext>` where `<hash>` is a content
/// hash (≥6 hex-ish chars, contains a digit). `jquery.min.js` and `app.js` pass through unchanged.
fn strip_fingerprint_filename(file: &str) -> String {
    let parts: Vec<&str> = file.split('.').collect();
    // Need at least name.hash.ext.
    if parts.len() < 3 {
        return file.to_string();
    }
    // The fingerprint is the second-to-last segment (just before the extension).
    let ext = parts[parts.len() - 1];
    let hash = parts[parts.len() - 2];
    if looks_like_fingerprint(hash) {
        let name = &parts[..parts.len() - 2];
        format!("{}.{ext}", name.join("."))
    } else {
        file.to_string()
    }
}

/// A path/asset fingerprint hash: ≥6 chars, all `[0-9a-fA-F]`, at least one digit (so `min`, `prod`
/// etc. — non-hex or all-alpha words — are not mistaken for fingerprints).
fn looks_like_fingerprint(seg: &str) -> bool {
    seg.len() >= 6
        && seg.chars().all(|c| c.is_ascii_hexdigit())
        && seg.chars().any(|c| c.is_ascii_digit())
}

// ===========================================================================================
// Strip rules: the compiled volatile-attribute / volatile-text policy (profile-extensible).
// ===========================================================================================

/// Compiled strip policy for one [`normalize`] call. Holds the framework-churn id matcher, the
/// relative-time matcher, and the profile-extensible attr/text additions. Built once per call so the
/// regexes compile once (ARCHITECTURE §3 RegexSet posture).
struct StripRules {
    /// `id` values matching framework-generated patterns (`:r0:`, `radix-…`, `headlessui-…`,
    /// `mui-…`, `ember\d+`). Drops the `id` attribute when its VALUE matches.
    framework_id: regex::Regex,
    /// High-entropy structural token: `^[A-Za-z0-9_-]{24,}$`.
    high_entropy_token: regex::Regex,
    /// Relative-time / "as of …" spans → [`TS_SENTINEL`].
    relative_time: regex::Regex,
    /// Profile-supplied extra attribute names to always strip (`profile.strip_attrs`), lowercased.
    extra_attrs: BTreeSet<String>,
    /// Profile-supplied extra text regexes (`profile.strip_text`) whose matches → [`TS_SENTINEL`].
    extra_text: Vec<regex::Regex>,
}

/// Framework-churn attribute NAMES that are always stripped regardless of value (DESIGN §5.3):
/// `data-reactid`, and the `data-v-*` / `data-svelte-*` families (matched by prefix).
fn is_framework_attr_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "data-reactid"
        || n == "nonce"
        || n.starts_with("data-v-")
        || n.starts_with("data-svelte-")
}

/// Attribute NAMES that denote a nonce/csrf token regardless of value (DESIGN §5.3). Matched as a
/// substring of the lowercased name so `csrf-token`, `data-csrf-nonce`, `x-nonce`, `csrfmiddlewaretoken`
/// all qualify.
fn is_nonce_csrf_attr_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("nonce") || n.contains("csrf")
}

impl StripRules {
    fn new(profile: &Profile) -> Result<Self, CfError> {
        let compile = |src: &str| -> Result<regex::Regex, CfError> {
            regex::Regex::new(src)
                .map_err(|e| CfError::Usage(format!("invalid normalize regex {src:?}: {e}")))
        };

        // Framework-generated id values: :r0: / :r1a: , radix-… , headlessui-… , mui-… , ember123.
        let framework_id = compile(r"^(:r[0-9a-z]+:|radix-|headlessui-|mui-|ember\d)")?;
        let high_entropy_token = compile(r"^[A-Za-z0-9_-]{24,}$")?;

        // Relative-time: "<n> <unit> ago", "in <n> <unit>", "as of <time>", "<n> <unit> from now".
        // Case-insensitive. Units: second/minute/hour/day/week/month/year (+ optional plural).
        let relative_time = compile(
            r"(?ix)
              \b(
                  # 'N unit ago' / 'N unit from now' / 'a unit ago'
                  (?:\d+|an?|a\sfew|several)\s+
                  (?:second|minute|hour|day|week|month|year)s?\s+
                  (?:ago|from\s+now)
                | # 'in N unit'
                  in\s+(?:\d+|an?|a\sfew|several)\s+
                  (?:second|minute|hour|day|week|month|year)s?
                | # 'as of HH:MM[ TZ]' / 'as of <date-ish>'
                  as\s+of\s+\d{1,2}:\d{2}(?::\d{2})?(?:\s*[A-Za-z]{2,4})?
                | # bare 'updated/last updated HH:MM[ TZ]'
                  (?:last\s+)?updated\s+\d{1,2}:\d{2}(?::\d{2})?(?:\s*[A-Za-z]{2,4})?
                | # 'just now' / 'moments ago'
                  just\s+now
                | moments?\s+ago
              )\b
            ",
        )?;

        let extra_attrs = profile
            .strip_attrs
            .iter()
            .map(|a| a.to_ascii_lowercase())
            .collect();

        let mut extra_text = Vec::with_capacity(profile.strip_text.len());
        for src in &profile.strip_text {
            extra_text.push(compile(src)?);
        }

        Ok(StripRules {
            framework_id,
            high_entropy_token,
            relative_time,
            extra_attrs,
            extra_text,
        })
    }

    /// Decide whether an attribute `(name, value)` is volatile and should be dropped (DESIGN §5.3).
    fn is_volatile_attr(&self, name: &str, value: &str) -> bool {
        let lname = name.to_ascii_lowercase();

        // Profile-extensible attribute names.
        if self.extra_attrs.contains(&lname) {
            return true;
        }
        // Nonce / csrf by name.
        if is_nonce_csrf_attr_name(&lname) {
            return true;
        }
        // Framework churn by name (data-reactid, data-v-*, data-svelte-*, nonce).
        if is_framework_attr_name(&lname) {
            return true;
        }
        // Framework-generated id VALUES (:r0:, radix-…, headlessui-…, mui-…, ember123).
        if lname == "id" && self.framework_id.is_match(value) {
            return true;
        }
        // High-entropy token VALUE (structural length rule OR Shannon entropy > threshold). We apply
        // this only to attributes that plausibly *carry* a token, never to URL/class/style/visible
        // attributes whose long values are legitimately high-entropy content.
        if is_token_bearing_attr(&lname) && is_high_entropy_value(value, &self.high_entropy_token) {
            return true;
        }
        false
    }

    /// Replace relative-time spans and any profile `strip_text` matches with [`TS_SENTINEL`].
    fn replace_volatile_text(&self, text: &str) -> String {
        let replaced = self.relative_time.replace_all(text, TS_SENTINEL);
        let mut out = replaced.into_owned();
        for re in &self.extra_text {
            out = re.replace_all(&out, TS_SENTINEL).into_owned();
        }
        out
    }
}

/// Attributes whose value plausibly carries a generated token (so the high-entropy rule applies).
/// We deliberately EXCLUDE `href`/`src`/`class`/`style`/visible-text-ish attributes: a long URL or
/// class list is legitimately high-entropy but is NOT a volatile token, and URL volatility is
/// handled by canonicalization instead.
fn is_token_bearing_attr(name: &str) -> bool {
    matches!(
        name,
        "id" | "name"
            | "value"
            | "content"
            | "token"
            | "data-token"
            | "data-nonce"
            | "data-key"
            | "data-id"
            | "data-hash"
            | "data-build"
            | "data-version-hash"
            | "integrity"
            | "data-csrf"
    ) || name.starts_with("data-") && name.ends_with("-token")
}

/// High-entropy *token* test (DESIGN §5.3): the structural rule `^[A-Za-z0-9_-]{24,}$` OR Shannon
/// entropy > [`ENTROPY_BITS_PER_CHAR`] bits/char. Both arms require the value to be a single
/// whitespace-free *token* — natural-language prose ("The best pricing for teams") carries ~4
/// bits/char of entropy but is NOT a generated token, so the entropy arm only fires on a value with
/// no internal whitespace and a long-enough run. This keeps the rule a token detector, not a prose
/// detector. Empty/short values are never high-entropy.
fn is_high_entropy_value(value: &str, high_entropy_token: &regex::Regex) -> bool {
    // Multi-word / whitespace-bearing values are prose/markup, never a single generated token.
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    if value.len() >= HIGH_ENTROPY_MIN_LEN && high_entropy_token.is_match(value) {
        return true;
    }
    // Entropy arm: only on token-length values (≥ the structural minimum) so a short word like
    // "starter" is never mistaken for a token regardless of its per-char entropy.
    value.len() >= HIGH_ENTROPY_MIN_LEN
        && shannon_entropy_bits_per_char(value) > ENTROPY_BITS_PER_CHAR
}

/// Shannon entropy in bits/char of `s` (over its byte distribution). Returns 0 for the empty string.
/// Deterministic: a pure function of the byte histogram, no clock/RNG.
fn shannon_entropy_bits_per_char(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let bytes = s.as_bytes();
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    let mut h = 0.0f64;
    for &c in counts.iter() {
        if c == 0 {
            continue;
        }
        let p = c as f64 / len;
        h -= p * p.log2();
    }
    h
}

// ===========================================================================================
// Helpers: <base href> discovery + deterministic HTML serialization (mirrors extract).
// ===========================================================================================

/// Find a `<base href="…">` in the subtree (document order) to use as the relative-URL base.
fn find_base_href(roots: &[ExtractNode]) -> Option<String> {
    fn walk(node: &ExtractNode) -> Option<String> {
        match node {
            ExtractNode::Text(_) => None,
            ExtractNode::Element {
                name,
                attrs,
                children,
            } => {
                if name == "base" {
                    if let Some((_, v)) = attrs.iter().find(|(k, _)| k == "href") {
                        if !v.trim().is_empty() {
                            return Some(v.trim().to_string());
                        }
                    }
                }
                for c in children {
                    if let Some(h) = walk(c) {
                        return Some(h);
                    }
                }
                None
            }
        }
    }
    roots.iter().find_map(walk)
}

/// Deterministic HTML serializer (mirrors `extract::ExtractNode::write_html`, which is private).
/// Minimal and stable — not a spec-perfect serializer — used to fill [`NormalizedDom::html`].
fn write_html(node: &ExtractNode, out: &mut String) {
    match node {
        ExtractNode::Text(t) => out.push_str(t),
        ExtractNode::Element {
            name,
            attrs,
            children,
        } => {
            out.push('<');
            out.push_str(name);
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(v);
                out.push('"');
            }
            if children.is_empty() {
                out.push_str("></");
                out.push_str(name);
                out.push('>');
            } else {
                out.push('>');
                for c in children {
                    write_html(c, out);
                }
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract;
    use crate::model::{ExtractStrategy, Profile, RenderMode, SourceMode};

    /// A bare profile (MVP defaults), optionally with selector root + strip extensions.
    fn profile(strategy: ExtractStrategy, root_selector: Option<&str>) -> Profile {
        Profile {
            profile_id: "test".to_string(),
            render: RenderMode::Auto,
            strategy,
            root_selector: root_selector.map(str::to_string),
            strip_attrs: Vec::new(),
            strip_text: Vec::new(),
            unordered: Vec::new(),
            mode: SourceMode::Page,
            max_pages: 1,
            types: Vec::new(),
            archetype: None,
            salience_hints: Vec::new(),
        }
    }

    /// Normalize raw HTML end-to-end via extract (`full` strategy unless a selector is given).
    fn norm(html: &str, prof: &Profile) -> NormalizedDom {
        let strat = prof.strategy;
        let sub = extract(html, prof).unwrap();
        assert_eq!(strat, prof.strategy); // sanity: profile not mutated
        normalize(sub, prof).unwrap()
    }

    /// Collect concatenated visible text of the normalized forest.
    fn text_of(dom: &NormalizedDom) -> String {
        dom.roots.iter().map(|r| r.text()).collect()
    }

    // --- direct-tree helpers (bypass extract to test a precise input) -------------------------

    fn elem(name: &str, attrs: &[(&str, &str)], children: Vec<ExtractNode>) -> ExtractNode {
        ExtractNode::Element {
            name: name.to_string(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            children,
        }
    }
    fn text(t: &str) -> ExtractNode {
        ExtractNode::Text(t.to_string())
    }
    fn subtree(roots: Vec<ExtractNode>) -> DomSubtree {
        DomSubtree {
            roots,
            html: String::new(),
            stripped_attrs: 0,
            bytes_raw: 0,
        }
    }

    fn first_elem_attrs(dom: &NormalizedDom) -> &[(String, String)] {
        match &dom.roots[0] {
            ExtractNode::Element { attrs, .. } => attrs,
            _ => panic!("expected element root"),
        }
    }

    // ======================================================================================
    // URL canonicalization — the §5.3 worked example.
    // ======================================================================================

    #[test]
    fn url_canonicalization_section_5_3_example() {
        // HTTP://Example.com:80/p?utm_source=x&b=2&a=1#top → http://example.com/p?a=1&b=2
        let out = canonicalize_url("HTTP://Example.com:80/p?utm_source=x&b=2&a=1#top", None);
        assert_eq!(out, "http://example.com/p?a=1&b=2");
    }

    #[test]
    fn url_canonicalization_in_attribute_position() {
        let dom = normalize(
            subtree(vec![elem(
                "a",
                &[("href", "HTTP://Example.com:80/p?utm_source=x&b=2&a=1#top")],
                vec![text("link")],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        let href = attrs.iter().find(|(k, _)| k == "href").unwrap();
        assert_eq!(href.1, "http://example.com/p?a=1&b=2");
    }

    #[test]
    fn url_drops_default_https_port_and_fragment() {
        let out = canonicalize_url("https://Example.COM:443/Path/?z=1&a=2#frag", None);
        assert_eq!(out, "https://example.com/Path/?a=2&z=1");
    }

    #[test]
    fn url_strips_all_tracking_params() {
        let out = canonicalize_url(
            "https://x.test/p?gclid=1&utm_medium=cpc&fbclid=2&ref=hn&_ga=GA1&keep=yes",
            None,
        );
        assert_eq!(out, "https://x.test/p?keep=yes");
    }

    #[test]
    fn url_utm_only_query_becomes_no_query() {
        let out = canonicalize_url("https://x.test/p?utm_source=newsletter", None);
        assert_eq!(out, "https://x.test/p");
    }

    #[test]
    fn url_strips_cache_buster_v_and_hash_but_keeps_semantic() {
        // §5.3: a `?v=`/`?hash=` cache-buster (numeric or hex shape) is volatile churn — a
        // cache-buster-only swap must normalize identically, so two observations differing only by
        // `?v=` collapse to the same canonical URL (and downstream to the same doc_hash).
        let a = canonicalize_url("https://acme.com/app.js?v=12345&keep=1", None);
        let b = canonicalize_url("https://acme.com/app.js?v=99999&keep=1", None);
        assert_eq!(a, b, "a `?v=` cache-buster swap is a non-event");
        assert_eq!(a, "https://acme.com/app.js?keep=1");
        assert_eq!(
            canonicalize_url("https://acme.com/style.css?hash=4f3a2b1", None),
            "https://acme.com/style.css"
        );
        // A semantic, opaque `?v=` (non-hex letters) is NOT a cache-buster and is preserved.
        assert_eq!(
            canonicalize_url("https://youtube.test/watch?v=dQw4w9WgXcQ", None),
            "https://youtube.test/watch?v=dQw4w9WgXcQ"
        );
    }

    #[test]
    fn url_relative_resolves_against_base_href() {
        let dom = normalize(
            subtree(vec![elem(
                "div",
                &[],
                vec![
                    elem("base", &[("href", "https://acme.test/docs/")], vec![]),
                    elem("a", &[("href", "../pricing?b=2&a=1")], vec![text("x")]),
                ],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        // Find the <a> href.
        fn find_href(n: &ExtractNode) -> Option<String> {
            if let ExtractNode::Element {
                name,
                attrs,
                children,
            } = n
            {
                if name == "a" {
                    return attrs
                        .iter()
                        .find(|(k, _)| k == "href")
                        .map(|(_, v)| v.clone());
                }
                for c in children {
                    if let Some(h) = find_href(c) {
                        return Some(h);
                    }
                }
            }
            None
        }
        let href = dom.roots.iter().find_map(find_href).unwrap();
        assert_eq!(href, "https://acme.test/pricing?a=1&b=2");
    }

    // ======================================================================================
    // Fingerprinted asset names.
    // ======================================================================================

    #[test]
    fn fingerprinted_asset_name_canonicalizes() {
        // app.4f3a2b1.js -> app.js
        let dom = normalize(
            subtree(vec![elem(
                "script",
                &[("src", "https://cdn.test/static/app.4f3a2b1.js")],
                vec![],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        let src = attrs.iter().find(|(k, _)| k == "src").unwrap();
        assert_eq!(src.1, "https://cdn.test/static/app.js");
    }

    #[test]
    fn fingerprint_strip_does_not_touch_min_js() {
        // jquery.min.js is NOT a fingerprint (no digit, all-alpha middle segment).
        assert_eq!(
            strip_fingerprint_path("/vendor/jquery.min.js"),
            "/vendor/jquery.min.js"
        );
        // app.js (no middle segment) untouched.
        assert_eq!(strip_fingerprint_path("/app.js"), "/app.js");
        // Real fingerprint stripped.
        assert_eq!(strip_fingerprint_path("/static/app.4f3a2b1.js"), "/static/app.js");
        assert_eq!(strip_fingerprint_path("main.0a9f3c.css"), "main.css");
    }

    // ======================================================================================
    // High-entropy / nonce / csrf attribute stripping.
    // ======================================================================================

    #[test]
    fn high_entropy_nonce_value_attribute_is_stripped() {
        // A token-bearing attr (`content`) whose value is a 40-char high-entropy token.
        let dom = normalize(
            subtree(vec![elem(
                "meta",
                &[
                    ("name", "csp-nonce"),
                    ("content", "aZ3kP9qLmX7vB2nR8sT4wY6cF1dG5hJ0eK2lM3n"),
                ],
                vec![],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        // The high-entropy content value is stripped; `name` survives.
        assert!(attrs.iter().any(|(k, _)| k == "name"));
        assert!(
            !attrs.iter().any(|(k, _)| k == "content"),
            "high-entropy content should be stripped, got {attrs:?}"
        );
        assert_eq!(dom.stripped_attrs, 1);
    }

    #[test]
    fn csrf_and_nonce_named_attributes_are_stripped_regardless_of_value() {
        let dom = normalize(
            subtree(vec![elem(
                "section",
                &[
                    ("data-csrf-nonce", "short"), // low entropy, but name says csrf+nonce
                    ("nonce", "abc"),
                    ("csrf-token", "tok"),
                    ("class", "PricingTable"), // survives
                ],
                vec![],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        let names: Vec<&str> = attrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["class"], "only class should survive: {attrs:?}");
        assert_eq!(dom.stripped_attrs, 3);
    }

    #[test]
    fn high_entropy_rule_does_not_strip_normal_text_value() {
        // A normal sentence in a `value`/`content` attr is low entropy → kept.
        let dom = normalize(
            subtree(vec![elem(
                "meta",
                &[("name", "description"), ("content", "The best pricing for teams")],
                vec![],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        assert!(attrs.iter().any(|(k, _)| k == "content"));
        assert_eq!(dom.stripped_attrs, 0);
    }

    #[test]
    fn framework_churn_attributes_and_ids_are_stripped() {
        let dom = normalize(
            subtree(vec![elem(
                "div",
                &[
                    ("data-reactid", ".0.1.2"),
                    ("data-v-7ba5bd90", ""),
                    ("data-svelte-h", "svelte-xyz"),
                    ("id", ":r0:"), // generated react/headless id
                    ("class", "real-class"),
                ],
                vec![],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        let names: Vec<&str> = attrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["class"], "{attrs:?}");
        assert_eq!(dom.stripped_attrs, 4);
    }

    #[test]
    fn generated_id_families_are_stripped_but_real_ids_survive() {
        let rules = StripRules::new(&profile(ExtractStrategy::Full, None)).unwrap();
        for gen in [":r0:", ":r1a:", "radix-:r3:", "headlessui-button-1", "mui-42", "ember123"] {
            assert!(
                rules.is_volatile_attr("id", gen),
                "expected {gen} to be a generated id"
            );
        }
        for real in ["pricing", "component-api", "v2-4-0", "section-2", "ember"] {
            assert!(
                !rules.is_volatile_attr("id", real),
                "expected {real} to be a real id (kept)"
            );
        }
    }

    // ======================================================================================
    // Relative-time text → ⟦TS⟧.
    // ======================================================================================

    #[test]
    fn relative_time_three_minutes_ago_becomes_sentinel() {
        let dom = normalize(
            subtree(vec![elem("p", &[], vec![text("Posted 3 minutes ago")])]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        assert_eq!(text_of(&dom), format!("Posted {TS_SENTINEL}"));
    }

    #[test]
    fn relative_time_as_of_clock_becomes_sentinel() {
        let dom = normalize(
            subtree(vec![elem("p", &[], vec![text("Status as of 14:32 UTC")])]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        assert_eq!(text_of(&dom), format!("Status {TS_SENTINEL}"));
    }

    #[test]
    fn relative_time_variants_all_collapse_to_one_sentinel() {
        let rules = StripRules::new(&profile(ExtractStrategy::Full, None)).unwrap();
        for s in [
            "5 minutes ago",
            "an hour ago",
            "in 3 days",
            "just now",
            "2 weeks from now",
            "moments ago",
        ] {
            let out = rules.replace_volatile_text(s);
            assert_eq!(out, TS_SENTINEL, "input {s:?} -> {out:?}");
        }
    }

    #[test]
    fn two_relative_times_differing_only_in_count_normalize_identically() {
        let p = profile(ExtractStrategy::Full, None);
        let a = normalize(
            subtree(vec![elem("p", &[], vec![text("Updated 3 minutes ago")])]),
            &p,
        )
        .unwrap();
        let b = normalize(
            subtree(vec![elem("p", &[], vec![text("Updated 47 minutes ago")])]),
            &p,
        )
        .unwrap();
        assert_eq!(a.html, b.html, "relative-time differences must vanish");
    }

    // ======================================================================================
    // Unicode NFC + whitespace.
    // ======================================================================================

    #[test]
    fn decomposed_unicode_is_nfc_composed() {
        // "e" + U+0301 (combining acute) → "é" (U+00E9), and the precomposed form is unchanged.
        let decomposed = "Cafe\u{0301}"; // Café in NFD
        let precomposed = "Caf\u{00E9}"; // Café in NFC
        assert_ne!(decomposed, precomposed);

        let p = profile(ExtractStrategy::Full, None);
        let a = normalize(subtree(vec![text(decomposed)]), &p).unwrap();
        let b = normalize(subtree(vec![text(precomposed)]), &p).unwrap();
        assert_eq!(text_of(&a), "Café");
        assert_eq!(a.html, b.html, "NFD and NFC inputs must normalize identically");
    }

    #[test]
    fn nbsp_and_zero_width_collapse_to_single_space() {
        // NBSP between words, plus zero-width chars that must vanish entirely, plus a run of ASCII
        // whitespace that collapses to one space, plus leading/trailing whitespace that is trimmed.
        let input = "  Hello\u{00A0}\u{00A0}\u{200B}World\u{FEFF}  \n\t  again  ";
        let p = profile(ExtractStrategy::Full, None);
        let dom = normalize(subtree(vec![text(input)]), &p).unwrap();
        assert_eq!(text_of(&dom), "Hello World again");
    }

    #[test]
    fn zero_width_inside_a_token_fuses_neighbors() {
        // U+200B inside "Pro" must not introduce a space: "Pro" stays "Pro".
        let p = profile(ExtractStrategy::Full, None);
        let dom = normalize(subtree(vec![text("Pr\u{200B}o Plan")]), &p).unwrap();
        assert_eq!(text_of(&dom), "Pro Plan");
    }

    #[test]
    fn code_and_pre_whitespace_is_preserved_exactly() {
        // Inside <pre>/<code> whitespace is load-bearing: NO collapse, NO trim, NO relative-time.
        let code_text = "  fn  main() {\n    let x = 1;   // 3 minutes ago\n}\n";
        let p = profile(ExtractStrategy::Full, None);
        let dom = normalize(
            subtree(vec![elem("pre", &[], vec![text(code_text)])]),
            &p,
        )
        .unwrap();
        // The text inside <pre> is byte-identical to the input (NFC of pure-ASCII is identity).
        assert_eq!(text_of(&dom), code_text);
        // The relative-time pattern inside code is NOT rewritten.
        assert!(text_of(&dom).contains("3 minutes ago"));
    }

    #[test]
    fn code_block_inside_normalized_prose_only_exempts_the_code() {
        let p = profile(ExtractStrategy::Full, None);
        let dom = normalize(
            subtree(vec![elem(
                "div",
                &[],
                vec![
                    elem("p", &[], vec![text("Edited   3 minutes ago")]),
                    elem("code", &[], vec![text("a    b")]),
                ],
            )]),
            &p,
        )
        .unwrap();
        let t = text_of(&dom);
        // Prose collapsed + relative-time replaced; code whitespace preserved.
        assert!(t.contains(&format!("Edited {TS_SENTINEL}")), "{t:?}");
        assert!(t.contains("a    b"), "code whitespace must survive: {t:?}");
    }

    // ======================================================================================
    // The linchpin: volatile-only difference → byte-identical normalized output.
    // ======================================================================================

    #[test]
    fn csrf_only_difference_produces_byte_identical_normalized_output() {
        // Two inputs differing ONLY by a CSRF nonce attribute value normalize byte-identically.
        let html_a = r#"<section data-csrf-nonce="a1b2c3d4e5f60718293a4b5c6d7e8f90">
            <h2>Pro Plan</h2><p class="price">$49/mo</p></section>"#;
        let html_b = r#"<section data-csrf-nonce="9988776655443322110aabbccddeeff0">
            <h2>Pro Plan</h2><p class="price">$49/mo</p></section>"#;

        let p = profile(ExtractStrategy::Selector, Some("section"));
        let a = norm(html_a, &p);
        let b = norm(html_b, &p);

        assert_eq!(a.html, b.html, "CSRF-only delta must normalize identically");
        assert_eq!(a.roots, b.roots, "normalized trees must be equal");
        // And the nonce attribute is actually gone.
        assert!(!a.html.contains("data-csrf-nonce"), "{}", a.html);
    }

    #[test]
    fn pricing_fixtures_volatile_only_delta_normalizes_identically() {
        // The §5.3 → §5.6 linchpin against the real fixtures: pricing_before vs pricing_noop differ
        // ONLY by a rotating CSRF nonce + a live viewer count — both volatile. The kept pricing
        // table must normalize byte-identically.
        let before = include_str!("../../../tests/fixtures/pricing_before.html");
        let noop = include_str!("../../../tests/fixtures/pricing_noop.html");

        // Strip the live-counter line via a profile strip_text rule (it is a per-target volatile,
        // exactly the profile-extensible seam), and the nonce attr via the global nonce rule.
        let mut p = profile(ExtractStrategy::Selector, Some("section.PricingTable"));
        p.strip_text = vec![r"\d+\s+viewing right now".to_string()];

        let a = norm(before, &p);
        let b = norm(noop, &p);

        assert_eq!(
            a.html, b.html,
            "volatile-only (nonce + viewer count) delta must normalize identically"
        );
        // Real content survived.
        assert!(a.html.contains("Pro Plan"));
        assert!(a.html.contains("$49/mo"));
        // Volatile content gone.
        assert!(!a.html.contains("data-csrf-nonce"));
        assert!(!a.html.contains("viewing right now"));
    }

    // ======================================================================================
    // stripped_count reflects removed attributes.
    // ======================================================================================

    #[test]
    fn stripped_count_reflects_removed_attributes() {
        let dom = normalize(
            subtree(vec![elem(
                "div",
                &[
                    ("nonce", "x"),               // 1
                    ("data-reactid", "y"),        // 2
                    ("data-v-abc", ""),           // 3
                    ("id", ":r0:"),               // 4
                    ("class", "kept"),            // kept
                    ("data-plan", "starter"),     // kept
                ],
                vec![elem(
                    "span",
                    &[("csrf-token", "z")], // 5 (nested)
                    vec![],
                )],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        assert_eq!(dom.stripped_attrs, 5);
    }

    #[test]
    fn profile_strip_attrs_extends_the_set() {
        let mut p = profile(ExtractStrategy::Full, None);
        p.strip_attrs = vec!["data-trace-id".to_string(), "X-Build".to_string()];
        let dom = normalize(
            subtree(vec![elem(
                "div",
                &[
                    ("data-trace-id", "abc"),
                    ("x-build", "123"), // matched case-insensitively
                    ("class", "kept"),
                ],
                vec![],
            )]),
            &p,
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        let names: Vec<&str> = attrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["class"]);
        assert_eq!(dom.stripped_attrs, 2);
    }

    // ======================================================================================
    // Entity decoding (already done by the parser; assert end-to-end).
    // ======================================================================================

    #[test]
    fn html_entities_are_decoded_end_to_end() {
        // &amp; → & , &lt;/&gt; → </> . html5ever decodes at parse; normalize preserves the decode.
        let dom = norm(
            r#"<div id="c"><p>SSO &amp; SAML &lt;ok&gt;</p></div>"#,
            &profile(ExtractStrategy::Selector, Some("#c")),
        );
        assert_eq!(text_of(&dom), "SSO & SAML <ok>");
    }

    // ======================================================================================
    // Determinism.
    // ======================================================================================

    #[test]
    fn normalize_is_deterministic_across_runs() {
        let html = include_str!("../../../tests/fixtures/pricing_before.html");
        let p = profile(ExtractStrategy::Selector, Some("section.PricingTable"));
        let a = norm(html, &p);
        let b = norm(html, &p);
        assert_eq!(a, b, "normalize must be byte-for-byte deterministic");
    }

    #[test]
    fn attrs_remain_sorted_after_normalization() {
        let dom = normalize(
            subtree(vec![elem(
                "div",
                &[("zeta", "1"), ("alpha", "2"), ("mid", "3")],
                vec![],
            )]),
            &profile(ExtractStrategy::Full, None),
        )
        .unwrap();
        let attrs = first_elem_attrs(&dom);
        let keys: Vec<&str> = attrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "mid", "zeta"]);
    }

    // ======================================================================================
    // Entropy unit.
    // ======================================================================================

    #[test]
    fn shannon_entropy_separates_words_from_tokens() {
        // A repetitive/natural string is low entropy; a random-looking token is high.
        let low = shannon_entropy_bits_per_char("aaaaaaaaaaaaaaaa");
        assert!(low < 1.0, "all-same is near-zero entropy: {low}");
        let word = shannon_entropy_bits_per_char("pricing pricing");
        assert!(word < ENTROPY_BITS_PER_CHAR, "natural text low: {word}");
        let token = shannon_entropy_bits_per_char("aZ3kP9qLmX7vB2nR8sT4wYcFdGhJeKlMnOpQrS");
        assert!(token > ENTROPY_BITS_PER_CHAR, "random token high: {token}");
    }

    #[test]
    fn invalid_profile_strip_text_regex_is_a_usage_error() {
        let mut p = profile(ExtractStrategy::Full, None);
        p.strip_text = vec!["(unclosed".to_string()];
        let err = normalize(subtree(vec![text("x")]), &p);
        assert!(matches!(err, Err(CfError::Usage(_))));
    }
}
