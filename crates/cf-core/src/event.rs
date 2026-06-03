//! `changefeed/v1` wire build + minified serde + JSON Schema (ARCHITECTURE.md §1, §6.1).
//!
//! The minifier is the standard `serde_json` compact form; presence-as-signal (§6.1 rule 4) is
//! enforced by `skip_serializing_if` on every optional field in `model`, so an absent field is
//! omitted rather than serialized as `null`.
//!
//! This module also holds the no-change / first-observation (baseline) envelope builders (§6.8),
//! the open-vocab `why.cat` fallback mapping (§6.4/§6.9 — unknown → `content_edit`), the published
//! JSON Schema 2020-12 document (with the `$defs` the MCP `$ref`s point at, §4.10), and the
//! human-readable `--pretty` renderer (§4.8).
//!
//! DETERMINISM: nothing here reads the clock or uses randomness. `obs`/ids/`rev` are injected by
//! the caller (the cli boundary), keeping the whole module replayable in tests.

use crate::model::{ChangeEvent, Crawl, Delta, FeedEnvelope, FetchTier, Materiality};

/// The published JSON Schema 2020-12 for `changefeed/v1`, embedded via `include_str!`
/// (served by `cf schema --version 1`).
pub const SCHEMA_V1: &str = include_str!("schema/v1.json");

/// The closed `why.cat` controlled vocabulary (§6.4 / App B). It is *open* on the wire — any string
/// deserializes — but this is the recognized set; anything outside it maps to `content_edit`
/// (§6.9 "treat unknown `cat` as `content_edit`").
pub const CAT_VOCAB: &[&str] = &[
    "price_increase",
    "price_decrease",
    "plan_added",
    "plan_removed",
    "api_breaking",
    "api_deprecation",
    "api_addition",
    "incident_open",
    "incident_update",
    "incident_resolved",
    "version_bump",
    "availability_out",
    "availability_in",
    "legal_filing",
    "job_posted",
    "job_removed",
    "content_edit",
    "cosmetic",
];

/// The closed `seg.role` open vocabulary (§6.2 / App B) — recognized roles for display routing.
pub const ROLE_VOCAB: &[&str] = &[
    "price",
    "date",
    "version",
    "status",
    "link",
    "prose",
    "code",
    "table-cell",
    "nav",
    "meta",
];

// ===========================================================================================
// Wire serialization (§6.1)
// ===========================================================================================

/// Minify one event to its canonical wire bytes (§6.1: ~792 B / ~305 tokens for Example 1).
pub fn to_wire(event: &ChangeEvent) -> Result<String, serde_json::Error> {
    serde_json::to_string(event)
}

/// Minify one feed envelope to its canonical wire bytes (§6.8).
pub fn envelope_to_wire(envelope: &FeedEnvelope) -> Result<String, serde_json::Error> {
    serde_json::to_string(envelope)
}

// ===========================================================================================
// Open-vocab `why.cat` fallback (§6.4, §6.9)
// ===========================================================================================

/// Map a (possibly unknown) `why.cat` string to a recognized category, per §6.9: a value outside
/// the controlled vocabulary is treated as `content_edit`. Returns a borrowed `&str` so callers can
/// branch without allocation. Recognized values pass through unchanged.
pub fn normalize_cat(cat: &str) -> &str {
    if CAT_VOCAB.contains(&cat) {
        cat
    } else {
        "content_edit"
    }
}

/// `true` iff `cat` is in the recognized §6.4 controlled vocabulary.
pub fn is_known_cat(cat: &str) -> bool {
    CAT_VOCAB.contains(&cat)
}

// ===========================================================================================
// Envelope builders — no-change & first-observation baseline (§6.8)
// ===========================================================================================

/// Build the **no-change** feed envelope (§6.8): `to_rev == from_rev`, `n:0`, `events:[]`. This is
/// the unambiguous "nothing changed" signal. `etag_hit:true` records a ~0-byte 304 short-circuit.
///
/// `obs`/`batch`/`rev` are injected by the caller; nothing here reads the clock.
#[allow(clippy::too_many_arguments)]
pub fn no_change_envelope(
    feed: impl Into<String>,
    batch: impl Into<String>,
    obs: impl Into<String>,
    rev: u64,
    url: Option<String>,
    tier: FetchTier,
    status: u16,
    ms: Option<u32>,
    hash: Option<String>,
    etag_hit: bool,
) -> FeedEnvelope {
    FeedEnvelope {
        v: "1",
        feed: feed.into(),
        batch: batch.into(),
        obs: obs.into(),
        crawl: Crawl {
            from_rev: Some(rev),
            to_rev: rev,
            url,
            m: tier,
            status,
            ms,
            hash,
            etag_hit: if etag_hit { Some(true) } else { None },
            n: 0,
            baseline: None,
            retry_after: None,
            err: None,
        },
        events: Vec::new(),
        next: None,
        next_cursor: None,
    }
}

/// Build the **first-observation (baseline)** envelope (§6.8, exit 11): `from_rev:null`,
/// `baseline:true`, `n:0`, `events:[]`. Tells the agent "first time seen, nothing to diff."
#[allow(clippy::too_many_arguments)]
pub fn baseline_envelope(
    feed: impl Into<String>,
    batch: impl Into<String>,
    obs: impl Into<String>,
    to_rev: u64,
    url: Option<String>,
    tier: FetchTier,
    status: u16,
    ms: Option<u32>,
    hash: Option<String>,
) -> FeedEnvelope {
    FeedEnvelope {
        v: "1",
        feed: feed.into(),
        batch: batch.into(),
        obs: obs.into(),
        crawl: Crawl {
            from_rev: None,
            to_rev,
            url,
            m: tier,
            status,
            ms,
            hash,
            etag_hit: None,
            n: 0,
            baseline: Some(true),
            retry_after: None,
            err: None,
        },
        events: Vec::new(),
        next: None,
        next_cursor: None,
    }
}

/// Build the **rate-limited** envelope (§4.4/§4.5/§4.6, exit 6): `n:0`, `events:[]`, and the crucial
/// `crawl.retry_after` so the canonical agent loop backs off WITHOUT a second fetch
/// (`json.loads(stdout)["crawl"]["retry_after"]`). `retry_after` is `None` when the 429 carried no
/// parseable `Retry-After` header (the agent then applies its own default, per the §4.6 `// 60`).
#[allow(clippy::too_many_arguments)]
pub fn rate_limited_envelope(
    feed: impl Into<String>,
    batch: impl Into<String>,
    obs: impl Into<String>,
    to_rev: u64,
    url: Option<String>,
    status: u16,
    retry_after: Option<u32>,
) -> FeedEnvelope {
    FeedEnvelope {
        v: "1",
        feed: feed.into(),
        batch: batch.into(),
        obs: obs.into(),
        crawl: Crawl {
            from_rev: Some(to_rev),
            to_rev,
            url,
            m: FetchTier::Http,
            status,
            ms: None,
            hash: None,
            etag_hit: None,
            n: 0,
            baseline: None,
            retry_after,
            err: Some("rate_limited".to_string()),
        },
        events: Vec::new(),
        next: None,
        next_cursor: None,
    }
}

/// A `cf feed` page envelope (§4.7 / §6.8): wraps a catch-up page with `n` = page event count and an
/// optional `next_cursor` the agent passes to `--after-cursor` to resume. `crawl` is informational
/// (the feed re-derives from stored snapshots, so there is no single fetch to describe).
pub fn feed_page_envelope(
    feed: impl Into<String>,
    obs: impl Into<String>,
    to_rev: u64,
    n: u32,
    events: Vec<ChangeEvent>,
    next_cursor: Option<String>,
) -> FeedEnvelope {
    FeedEnvelope {
        v: "1",
        feed: feed.into(),
        batch: "cfb_feed".to_string(),
        obs: obs.into(),
        crawl: Crawl {
            from_rev: None,
            to_rev,
            url: None,
            m: FetchTier::Http,
            status: 200,
            ms: None,
            hash: None,
            etag_hit: None,
            n,
            baseline: None,
            retry_after: None,
            err: None,
        },
        events,
        next: None,
        next_cursor,
    }
}

/// Build a **changed** feed envelope (§6.8) wrapping `events`; `n` is set from the event count.
#[allow(clippy::too_many_arguments)]
pub fn changed_envelope(
    feed: impl Into<String>,
    batch: impl Into<String>,
    obs: impl Into<String>,
    from_rev: u64,
    to_rev: u64,
    url: Option<String>,
    tier: FetchTier,
    status: u16,
    ms: Option<u32>,
    hash: Option<String>,
    events: Vec<ChangeEvent>,
) -> FeedEnvelope {
    let n = events.len() as u32;
    FeedEnvelope {
        v: "1",
        feed: feed.into(),
        batch: batch.into(),
        obs: obs.into(),
        crawl: Crawl {
            from_rev: Some(from_rev),
            to_rev,
            url,
            m: tier,
            status,
            ms,
            hash,
            etag_hit: None,
            n,
            baseline: None,
            retry_after: None,
            err: None,
        },
        events,
        next: None,
        next_cursor: None,
    }
}

// ===========================================================================================
// JSON Schema 2020-12 (§4.10, §6.9)
// ===========================================================================================

/// The published JSON Schema for `cf schema --version <v>`. Only version `1` exists in MVP.
///
/// Returns the static `changefeed/v1` document (with the `$defs` the MCP `$ref`s point at, §4.10).
pub fn schema_for_version(version: &str) -> Option<&'static str> {
    match version {
        "1" => Some(SCHEMA_V1),
        _ => None,
    }
}

// ===========================================================================================
// Pretty (TTY) renderer (§4.8)
// ===========================================================================================

/// Materiality badge text used in the pretty renderer (§4.8 `[HIGH]`).
fn mat_badge(mat: Materiality) -> &'static str {
    match mat {
        Materiality::None => "NONE",
        Materiality::Low => "LOW",
        Materiality::Medium => "MED",
        Materiality::High => "HIGH",
        Materiality::Critical => "CRIT",
    }
}

/// Human-readable label for `delta.enc` rendering.
fn render_delta(delta: &Delta) -> String {
    match delta {
        Delta::Val { a, b } => format!("{b}  →  {a}"),
        Delta::Idiff { ops } => {
            use crate::model::DiffOp;
            let mut s = String::new();
            for op in ops {
                match op.op {
                    DiffOp::Keep => s.push_str(&op.text),
                    DiffOp::Del => {
                        s.push_str("[-");
                        s.push_str(&op.text);
                        s.push_str("-]");
                    }
                    DiffOp::Ins => {
                        s.push_str("{+");
                        s.push_str(&op.text);
                        s.push_str("+}");
                    }
                }
            }
            s
        }
        Delta::Block { a, b, atrunc } => match b {
            Some(b) => {
                let suffix = if *atrunc { " …" } else { "" };
                format!("{b}  →  {a}{suffix}")
            }
            None => {
                let suffix = if *atrunc { " …" } else { "" };
                format!("{a}{suffix}")
            }
        },
        Delta::Move { from, to, key } => format!("moved {key:?}: pos {from} → {to}"),
        Delta::Struct {
            added,
            removed,
            modified,
            truncated,
            ..
        } => format!(
            "+{added} / -{removed} / ~{modified} (set change; {truncated} more)"
        ),
    }
}

/// The change-type glyph (§4.8: `~` modified, `+` added, `-` removed, etc.).
fn ct_glyph(ct: crate::model::ChangeType) -> char {
    use crate::model::ChangeType;
    match ct {
        ChangeType::Added => '+',
        ChangeType::Removed => '-',
        ChangeType::Modified => '~',
        ChangeType::Reordered => '↕',
        ChangeType::Restyled => '≈',
    }
}

/// Render one event to the human-readable `--pretty` form (§4.8). No ANSI colors are embedded so
/// the output is testable and pipe-safe; the cli layer colorizes on a TTY.
pub fn render_event_pretty(event: &ChangeEvent) -> String {
    let mut out = String::new();
    let title = event.src.title.as_deref().unwrap_or(&event.src.tid);
    out.push_str(&format!(
        "● {}  {}            [{}] {}\n",
        event.src.tid,
        event.src.url,
        mat_badge(event.why.mat),
        normalize_cat(&event.why.cat),
    ));
    let label = event
        .seg
        .first()
        .map(|s| s.label_path.as_str())
        .unwrap_or(title);
    out.push_str(&format!(
        "  {} {}  {}\n",
        ct_glyph(event.ct),
        label,
        render_delta(&event.delta),
    ));
    out.push_str(&format!(
        "  why: {} ({})   (sal {:.2})\n",
        normalize_cat(&event.why.cat),
        event.why.summary,
        event.why.sal,
    ));
    out.push_str(&format!(
        "  → suggested: {}\n",
        action_token(event.followup.act),
    ));
    out.push_str(&format!(
        "  rev {} (prev {})  · conf {:.2}\n",
        event.base.rev,
        event.base.rev.saturating_sub(1),
        event.conf,
    ));
    out
}

/// Render a whole feed envelope to the `--pretty` form (§4.8). Emits a one-line header for the
/// no-change / baseline cases so an operator sees that the crawl ran.
pub fn render_envelope_pretty(envelope: &FeedEnvelope) -> String {
    if envelope.events.is_empty() {
        let c = &envelope.crawl;
        if c.baseline == Some(true) {
            return format!(
                "○ {}  baseline stored (rev {}, first observation)\n",
                envelope.feed, c.to_rev
            );
        }
        let etag = if c.etag_hit == Some(true) {
            " (304 not modified)"
        } else {
            ""
        };
        return format!(
            "○ {}  no change (rev {}){}\n",
            envelope.feed, c.to_rev, etag
        );
    }
    let mut out = String::new();
    for ev in &envelope.events {
        out.push_str(&render_event_pretty(ev));
    }
    out
}

/// The frozen 8-value `followup.act` wire token (§6.5). Kept here so the pretty renderer and any
/// caller share one source of truth without re-serializing through serde.
fn action_token(act: crate::model::Action) -> &'static str {
    use crate::model::Action;
    match act {
        Action::Ignore => "ignore",
        Action::Notify => "notify",
        Action::RefetchLinked => "refetch_linked",
        Action::ReembedKb => "reembed_kb",
        Action::ReRunDownstream => "re_run_downstream",
        Action::OpenTicket => "open_ticket",
        Action::EscalateHuman => "escalate_human",
        Action::PageOncall => "page_oncall",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Action, Base, ChangeType, DiffOp, EventId, Followup, IdiffOp, Prov, Seg, Src, Why,
    };

    /// Reconstruct the §6.7 Example 1 event (the canonical price-change case) faithfully — the same
    /// bytes the doc measures at 792 B ≈ 305 tokens.
    fn example_1() -> ChangeEvent {
        let mut params = serde_json::Map::new();
        params.insert("urgency".into(), serde_json::Value::String("high".into()));
        ChangeEvent {
            v: "1",
            id: EventId::new("cfe_01J9Z4K7QH8M2N3P4R5S6T7V8W".into()),
            src: Src {
                url: "https://acme.com/pricing".into(),
                tid: "acme-pricing".into(),
                title: Some("Pricing — Acme".into()),
            },
            obs: "2026-06-02T14:03:11Z".into(),
            base: Base {
                obs: "2026-05-26T14:01:55Z".into(),
                snap: "blake3:9f2c5d…e1".into(),
                rev: 41,
            },
            seg: vec![Seg {
                anchor: "Pro plan".into(),
                fp: "blake3:4b9c1e".into(),
                label_path: "Pricing › Pro Plan › price".into(),
                role: "price".into(),
            }],
            ct: ChangeType::Modified,
            delta: Delta::Val {
                a: "$39/mo".into(),
                b: "$29/mo".into(),
            },
            why: Why {
                sal: 0.86,
                mat: Materiality::High,
                cat: "price_increase".into(),
                summary: "Pro monthly price rose 34% ($29→$39).".into(),
            },
            followup: Followup {
                act: Action::ReRunDownstream,
                tgt: Some("pricing-watchers".into()),
                params: Some(params),
                q: Some("Did the annual price or feature list change too?".into()),
            },
            conf: 0.97,
            prov: Prov {
                m: FetchTier::Http,
                hash: "blake3:1c8a72…d4".into(),
                etag: Some("W/\"3f1-9aXc\"".into()),
                status: 200,
                ms: None,
                pack: Some("pricing@b3:2f1a".into()),
            },
        }
    }

    // ---- Wire size + round-trip (§6.7 Example 1) -----------------------------------------------

    #[test]
    fn example_1_minified_byte_length_near_792() {
        let s = to_wire(&example_1()).unwrap();
        let n = s.len();
        assert!(
            (740..=860).contains(&n),
            "Example 1 minified is {n} B; expected within 740..=860 (≈792). wire: {s}"
        );
    }

    #[test]
    fn example_1_round_trips_equal() {
        let ev = example_1();
        let s = to_wire(&ev).unwrap();
        let back: ChangeEvent = serde_json::from_str(&s).unwrap();
        // Re-serialize and compare bytes — struct field order is fixed, so equal value ⇒ equal bytes.
        assert_eq!(to_wire(&back).unwrap(), s);
        // Spot-check a few load-bearing fields survived the trip.
        assert_eq!(back.id.as_str(), "cfe_01J9Z4K7QH8M2N3P4R5S6T7V8W");
        assert_eq!(back.ct, ChangeType::Modified);
        match back.delta {
            Delta::Val { a, b } => {
                assert_eq!(a, "$39/mo");
                assert_eq!(b, "$29/mo");
            }
            _ => panic!("delta enc changed across round-trip"),
        }
        assert_eq!(back.followup.act, Action::ReRunDownstream);
    }

    // ---- Presence-as-signal: no field serializes as null (§6.1 rule 4) -------------------------

    #[test]
    fn no_field_serializes_as_null() {
        // A fully-populated event has no nulls.
        assert!(!to_wire(&example_1()).unwrap().contains("null"));

        // A maximally-sparse event (every optional absent) also emits zero `null`s and omits the keys.
        let sparse = ChangeEvent {
            v: "1",
            id: EventId::new("cfe_sparse".into()),
            src: Src {
                url: "https://x/y".into(),
                tid: "t".into(),
                title: None,
            },
            obs: "2026-06-02T00:00:00Z".into(),
            base: Base {
                obs: "2026-06-01T00:00:00Z".into(),
                snap: "blake3:00".into(),
                rev: 0,
            },
            seg: vec![Seg {
                anchor: "a".into(),
                fp: "blake3:00".into(),
                label_path: "p".into(),
                role: "prose".into(),
            }],
            ct: ChangeType::Added,
            delta: Delta::Block {
                a: "new text".into(),
                b: None,
                atrunc: false,
            },
            why: Why {
                sal: 0.5,
                mat: Materiality::Medium,
                cat: "content_edit".into(),
                summary: "x".into(),
            },
            followup: Followup {
                act: Action::Notify,
                tgt: None,
                params: None,
                q: None,
            },
            conf: 0.9,
            prov: Prov {
                m: FetchTier::Http,
                hash: "blake3:00".into(),
                etag: None,
                status: 200,
                ms: None,
                pack: None,
            },
        };
        let s = to_wire(&sparse).unwrap();
        assert!(!s.contains("null"), "sparse event leaked a null: {s}");
        // The omitted optional keys are simply absent — zero bytes.
        for absent in ["\"title\"", "\"tgt\"", "\"params\"", "\"q\"", "\"etag\"", "\"ms\"", "\"pack\""] {
            assert!(!s.contains(absent), "absent optional {absent} should be omitted, got: {s}");
        }
        // The `b` of an `added` block (b:None) is omitted, not null.
        assert!(!s.contains("\"b\""), "added block should omit b: {s}");
    }

    // ---- followup.act: exactly the 8 allowed strings, others unrepresentable -------------------

    #[test]
    fn followup_act_serializes_to_exactly_the_eight_tokens() {
        let cases = [
            (Action::Ignore, "ignore"),
            (Action::Notify, "notify"),
            (Action::RefetchLinked, "refetch_linked"),
            (Action::ReembedKb, "reembed_kb"),
            (Action::ReRunDownstream, "re_run_downstream"),
            (Action::OpenTicket, "open_ticket"),
            (Action::EscalateHuman, "escalate_human"),
            (Action::PageOncall, "page_oncall"),
        ];
        for (act, token) in cases {
            let f = Followup {
                act,
                tgt: None,
                params: None,
                q: None,
            };
            let s = to_wire_followup(&f);
            assert_eq!(s, format!("{{\"act\":{token:?}}}"));
        }
    }

    /// `verify_llm` is unrepresentable as an `Action`: the type only admits the 8 tokens. This
    /// default-less exhaustive match fails to COMPILE if the enum gains a variant (App B freeze).
    #[test]
    fn action_enum_is_closed_eight_no_verify_llm() {
        fn exhaustive(a: Action) -> &'static str {
            action_token(a)
        }
        assert_eq!(exhaustive(Action::PageOncall), "page_oncall");
        // The wire never contains the internal scoring state.
        for a in [
            Action::Ignore,
            Action::Notify,
            Action::RefetchLinked,
            Action::ReembedKb,
            Action::ReRunDownstream,
            Action::OpenTicket,
            Action::EscalateHuman,
            Action::PageOncall,
        ] {
            assert_ne!(action_token(a), "verify_llm");
        }
    }

    fn to_wire_followup(f: &Followup) -> String {
        serde_json::to_string(f).unwrap()
    }

    // ---- ct enum is exactly the five frozen tokens (§6.2) --------------------------------------

    #[test]
    fn change_type_serializes_to_exactly_five_tokens() {
        let cases = [
            (ChangeType::Added, "\"added\""),
            (ChangeType::Removed, "\"removed\""),
            (ChangeType::Modified, "\"modified\""),
            (ChangeType::Reordered, "\"reordered\""),
            (ChangeType::Restyled, "\"restyled\""),
        ];
        for (ct, token) in cases {
            assert_eq!(serde_json::to_string(&ct).unwrap(), token);
        }
    }

    // ---- Open-vocab cat + unknown-field tolerance (§6.9) ---------------------------------------

    #[test]
    fn unknown_cat_deserializes_and_maps_to_content_edit() {
        // An UNKNOWN open-vocab `cat` string deserializes fine (it is a plain string on the wire).
        let wire = wire_with_cat_and_extra("brand_new_category_2099", "");
        let ev: ChangeEvent = serde_json::from_str(&wire).unwrap();
        assert_eq!(ev.why.cat, "brand_new_category_2099");
        // ...and the §6.9 fallback maps it to content_edit for routing.
        assert_eq!(normalize_cat(&ev.why.cat), "content_edit");
        assert!(!is_known_cat(&ev.why.cat));

        // A KNOWN cat passes through unchanged.
        let known: ChangeEvent =
            serde_json::from_str(&wire_with_cat_and_extra("price_increase", "")).unwrap();
        assert_eq!(normalize_cat(&known.why.cat), "price_increase");
        assert!(is_known_cat(&known.why.cat));
    }

    #[test]
    fn unknown_top_level_field_is_ignored() {
        // An additive, unknown field MUST be ignored on deserialize (§6.9 forward-compat).
        let wire = wire_with_cat_and_extra("content_edit", r#","future_field":{"x":[1,2,3]}"#);
        let ev: ChangeEvent = serde_json::from_str(&wire).expect("unknown field must be tolerated");
        assert_eq!(ev.id.as_str(), "cfe_x");
        assert_eq!(ev.why.cat, "content_edit");
    }

    /// Build a minimal valid event wire string with the given `cat` and an optional extra trailing
    /// fragment injected after `conf` (e.g. an unknown top-level field).
    fn wire_with_cat_and_extra(cat: &str, extra: &str) -> String {
        format!(
            concat!(
                r#"{{"v":"1","id":"cfe_x",""#,
                r#"src":{{"url":"https://x/y","tid":"t"}},"#,
                r#""obs":"2026-06-02T00:00:00Z","#,
                r#""base":{{"obs":"2026-06-01T00:00:00Z","snap":"blake3:00","rev":0}},"#,
                r#""seg":[{{"anchor":"a","fp":"blake3:00","label_path":"p","role":"price"}}],"#,
                r#""ct":"modified","#,
                r#""delta":{{"enc":"val","a":"$2","b":"$1"}},"#,
                r#""why":{{"sal":0.5,"mat":"low","cat":"{cat}","summary":"s"}},"#,
                r#""followup":{{"act":"notify"}},"#,
                r#""conf":0.9{extra},"#,
                r#""prov":{{"m":"http","hash":"blake3:00","status":200}}}}"#,
            ),
            cat = cat,
            extra = extra,
        )
    }

    // ---- No-change & baseline envelopes (§6.8) -------------------------------------------------

    #[test]
    fn no_change_envelope_has_equal_revs_and_n_zero() {
        let env = no_change_envelope(
            "acme-pricing",
            "cfb_01J9",
            "2026-06-02T14:03:11Z",
            41,
            None,
            FetchTier::Http,
            200,
            None,
            None,
            true,
        );
        assert_eq!(env.crawl.from_rev, Some(41));
        assert_eq!(env.crawl.to_rev, 41);
        assert_eq!(env.crawl.from_rev, Some(env.crawl.to_rev));
        assert_eq!(env.crawl.n, 0);
        assert!(env.events.is_empty());
        assert_eq!(env.crawl.etag_hit, Some(true));
        assert_eq!(env.crawl.baseline, None);

        let s = envelope_to_wire(&env).unwrap();
        assert!(!s.contains("null"), "no-change envelope leaked null: {s}");
        assert!(s.contains(r#""n":0"#));
        assert!(s.contains(r#""etag_hit":true"#));
        // from_rev present (Some) and == to_rev.
        assert!(s.contains(r#""from_rev":41"#));
        assert!(s.contains(r#""to_rev":41"#));
        // events array is empty.
        assert!(s.contains(r#""events":[]"#));
    }

    #[test]
    fn baseline_envelope_has_from_rev_null_and_baseline_true() {
        let env = baseline_envelope(
            "acme-pricing",
            "cfb_01J9",
            "2026-06-02T14:03:11Z",
            1,
            None,
            FetchTier::Http,
            200,
            None,
            Some("blake3:1c8a72…d4".into()),
        );
        assert_eq!(env.crawl.from_rev, None);
        assert_eq!(env.crawl.baseline, Some(true));
        assert_eq!(env.crawl.to_rev, 1);
        assert_eq!(env.crawl.n, 0);
        assert!(env.events.is_empty());

        let s = envelope_to_wire(&env).unwrap();
        // §6.8: baseline emits from_rev:null EXPLICITLY (the one place a null is meaningful in the
        // envelope is that from_rev is skipped, so it must NOT appear at all and baseline:true signals it).
        // Our model omits None from_rev (presence-as-signal), so from_rev must be absent, not null.
        assert!(!s.contains("null"));
        assert!(!s.contains(r#""from_rev""#), "baseline omits from_rev (presence-as-signal): {s}");
        assert!(s.contains(r#""baseline":true"#));
        assert!(s.contains(r#""to_rev":1"#));
        assert!(s.contains(r#""n":0"#));
        assert!(s.contains(r#""events":[]"#));
    }

    // ---- idiff tuple wire form (§6.3) ----------------------------------------------------------

    #[test]
    fn idiff_ops_round_trip_as_tuples() {
        let d = Delta::Idiff {
            ops: vec![
                IdiffOp { op: DiffOp::Keep, text: "Default ".into() },
                IdiffOp { op: DiffOp::Del, text: "100".into() },
                IdiffOp { op: DiffOp::Ins, text: "25".into() },
                IdiffOp { op: DiffOp::Keep, text: " per page.".into() },
            ],
        };
        let s = serde_json::to_string(&d).unwrap();
        assert_eq!(
            s,
            r#"{"enc":"idiff","ops":[["=","Default "],["-","100"],["+","25"],["="," per page."]]}"#
        );
        let back: Delta = serde_json::from_str(&s).unwrap();
        match back {
            Delta::Idiff { ops } => {
                assert_eq!(ops.len(), 4);
                assert_eq!(ops[1].op, DiffOp::Del);
                assert_eq!(ops[2].text, "25");
            }
            _ => panic!("idiff enc lost on round-trip"),
        }
    }

    // ---- Schema selection (§4.10) --------------------------------------------------------------

    #[test]
    fn schema_for_version_one_is_json_and_unknown_is_none() {
        let v1 = schema_for_version("1").expect("v1 schema exists");
        let parsed: serde_json::Value =
            serde_json::from_str(v1).expect("v1 schema is valid JSON");
        assert_eq!(parsed["$schema"], "https://json-schema.org/draft/2020-12/schema");
        // The MCP $refs (§4.10) point at these $defs — they MUST exist.
        for def in ["seg", "ct", "delta", "why", "followup"] {
            assert!(
                parsed["$defs"].get(def).is_some(),
                "schema missing $defs/{def} that the MCP $ref points at"
            );
        }
        assert!(schema_for_version("2").is_none());
        assert!(schema_for_version("nope").is_none());
    }

    #[test]
    fn example_1_validates_against_published_schema() {
        let schema_json: serde_json::Value =
            serde_json::from_str(schema_for_version("1").unwrap()).unwrap();
        let validator = jsonschema::validator_for(&schema_json).expect("schema compiles");
        let instance: serde_json::Value =
            serde_json::from_str(&to_wire(&example_1()).unwrap()).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "Example 1 failed schema: {errors:?}");
        assert!(validator.is_valid(&instance));
    }

    #[test]
    fn schema_rejects_unknown_followup_act_and_extra_field() {
        let schema_json: serde_json::Value =
            serde_json::from_str(schema_for_version("1").unwrap()).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();

        // An illegal followup.act must be rejected by the schema (the wire never emits it, but the
        // schema is the integrator's contract).
        let mut bad: serde_json::Value =
            serde_json::from_str(&to_wire(&example_1()).unwrap()).unwrap();
        bad["followup"]["act"] = serde_json::Value::String("verify_llm".into());
        assert!(!validator.is_valid(&bad), "schema accepted verify_llm act");

        // additionalProperties:false ⇒ an unknown top-level key is rejected by the SCHEMA, even
        // though the deserializer tolerates it (§6.9). These are two different layers.
        let mut extra: serde_json::Value =
            serde_json::from_str(&to_wire(&example_1()).unwrap()).unwrap();
        extra["surprise"] = serde_json::Value::Bool(true);
        assert!(!validator.is_valid(&extra), "schema accepted unknown top-level key");
    }

    // ---- Pretty renderer (§4.8) ----------------------------------------------------------------

    #[test]
    fn pretty_renderer_shows_badge_delta_and_followup() {
        let out = render_event_pretty(&example_1());
        assert!(out.contains("acme-pricing"));
        assert!(out.contains("[HIGH]"));
        assert!(out.contains("price_increase"));
        assert!(out.contains("$29/mo  →  $39/mo"));
        assert!(out.contains("sal 0.86"));
        assert!(out.contains("re_run_downstream"));
        assert!(out.contains("rev 41"));
    }

    #[test]
    fn pretty_envelope_renders_no_change_and_baseline_lines() {
        let nc = no_change_envelope(
            "acme-pricing",
            "cfb_1",
            "2026-06-02T14:03:11Z",
            41,
            None,
            FetchTier::Http,
            200,
            None,
            None,
            true,
        );
        let s = render_envelope_pretty(&nc);
        assert!(s.contains("no change"));
        assert!(s.contains("304 not modified"));

        let base = baseline_envelope(
            "acme-pricing",
            "cfb_1",
            "2026-06-02T14:03:11Z",
            1,
            None,
            FetchTier::Http,
            200,
            None,
            None,
        );
        let s = render_envelope_pretty(&base);
        assert!(s.contains("baseline stored"));
        assert!(s.contains("first observation"));
    }
}
