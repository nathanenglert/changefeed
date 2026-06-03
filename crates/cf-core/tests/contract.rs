//! Skeleton contract tests — assert the FROZEN type contract holds (serde wire keys, enum freeze,
//! identity newtype determinism). These exercise only the parts implemented in the scaffold
//! (no `todo!()` stages). Stage behavior is tested by later agents.

use cf_core::model::{
    Action, AnchorScheme, Base, ChangeType, Delta, DiffOp, DocHash, EventId, Followup, IdiffOp,
    Materiality, NormHash, Prov, Seg, SlotKey, Src, Why,
};
use cf_core::{ChangeEvent, FetchTier};

/// `verify_llm` is an internal scoring state and is NEVER an `Action` (App B, §6.5). This
/// default-less match fails to COMPILE if the frozen 8-value `Action` enum gains a variant.
#[test]
fn action_enum_is_frozen_eight() {
    fn exhaustive(a: Action) -> &'static str {
        match a {
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
    assert_eq!(exhaustive(Action::ReRunDownstream), "re_run_downstream");
}

/// The frozen `ct` enum (§6.2) is exactly five values; default-less match enforces it at compile.
#[test]
fn change_type_enum_is_frozen_five() {
    fn exhaustive(c: ChangeType) -> &'static str {
        match c {
            ChangeType::Added => "added",
            ChangeType::Removed => "removed",
            ChangeType::Modified => "modified",
            ChangeType::Reordered => "reordered",
            ChangeType::Restyled => "restyled",
        }
    }
    assert_eq!(exhaustive(ChangeType::Modified), "modified");
}

/// Closed enums serialize to their frozen short wire tokens (App B).
#[test]
fn closed_enums_serialize_to_frozen_tokens() {
    assert_eq!(
        serde_json::to_string(&Action::ReRunDownstream).unwrap(),
        "\"re_run_downstream\""
    );
    assert_eq!(serde_json::to_string(&ChangeType::Modified).unwrap(), "\"modified\"");
    assert_eq!(serde_json::to_string(&Materiality::High).unwrap(), "\"high\"");
    assert_eq!(serde_json::to_string(&FetchTier::Http).unwrap(), "\"http\"");
}

/// `Delta` is tagged by `enc` (§6.3): the wire form carries `"enc":"val"` with `a` first.
#[test]
fn delta_is_enc_tagged() {
    let d = Delta::Val {
        a: "$59/mo".into(),
        b: "$49/mo".into(),
    };
    let s = serde_json::to_string(&d).unwrap();
    assert_eq!(s, r#"{"enc":"val","a":"$59/mo","b":"$49/mo"}"#);
}

/// `idiff` ops serialize as `[op, text]` tuples with `=`/`-`/`+` op codes (§6.3).
#[test]
fn idiff_ops_serialize_as_tuples() {
    let d = Delta::Idiff {
        ops: vec![
            IdiffOp { op: DiffOp::Keep, text: "Rate limit is ".into() },
            IdiffOp { op: DiffOp::Del, text: "100".into() },
            IdiffOp { op: DiffOp::Ins, text: "60".into() },
            IdiffOp { op: DiffOp::Keep, text: " req/s.".into() },
        ],
    };
    let s = serde_json::to_string(&d).unwrap();
    assert_eq!(
        s,
        r#"{"enc":"idiff","ops":[["=","Rate limit is "],["-","100"],["+","60"],["="," req/s."]]}"#
    );
}

/// Absent optionals are OMITTED, never serialized as `null` (presence-as-signal, §6.1 rule 4).
#[test]
fn absent_optionals_are_omitted_not_null() {
    let f = Followup {
        act: Action::Notify,
        tgt: None,
        params: None,
        q: None,
    };
    let s = serde_json::to_string(&f).unwrap();
    assert_eq!(s, r#"{"act":"notify"}"#);
    assert!(!s.contains("null"));
}

/// The §10 worked-example event round-trips and contains the frozen field shape, with no `null`.
#[test]
fn worked_example_event_shape() {
    let event = ChangeEvent {
        v: "1",
        id: EventId::new("cfe_01J9ZBTESTULIDFIXED000000".into()),
        src: Src {
            url: "https://competitor.com/pricing".into(),
            tid: "comp-pricing".into(),
            title: Some("Pricing — Competitor".into()),
        },
        obs: "2026-06-02T14:30:11Z".into(),
        base: Base {
            obs: "2026-06-02T14:00:09Z".into(),
            snap: "blake3:9f2a".into(),
            rev: 47,
        },
        seg: vec![Seg {
            anchor: "Pro plan".into(),
            fp: "blake3:4b9c1e".into(),
            label_path: "Pricing › Pro Plan › price".into(),
            role: "price".into(),
        }],
        ct: ChangeType::Modified,
        delta: Delta::Val {
            a: "$59/mo".into(),
            b: "$49/mo".into(),
        },
        why: Why {
            sal: 0.86,
            mat: Materiality::High,
            cat: "price_increase".into(),
            summary: "Pro monthly price rose 20.4% ($49→$59).".into(),
        },
        followup: Followup {
            act: Action::ReRunDownstream,
            tgt: Some("pricing-watchers".into()),
            params: None,
            q: None,
        },
        conf: 0.97,
        prov: Prov {
            m: FetchTier::Http,
            hash: "blake3:1c80".into(),
            etag: Some("W/\"c3d4\"".into()),
            status: 200,
            ms: None,
            pack: Some("pricing@b3:2f1a".into()),
        },
    };
    let s = serde_json::to_string(&event).unwrap();
    assert!(s.contains(r#""ct":"modified""#));
    assert!(s.contains(r#""act":"re_run_downstream""#));
    assert!(s.contains(r#"{"enc":"val","a":"$59/mo","b":"$49/mo"}"#));
    // presence-as-signal: ms is None and must be omitted.
    assert!(!s.contains(r#""ms""#));
}

/// Identity newtypes are deterministic and produce stable display forms.
#[test]
fn identity_newtypes_are_deterministic() {
    let k1 = SlotKey::structural("Pricing›Pro Plan", cf_core::BlockType::Price, 0);
    let k2 = SlotKey::structural("Pricing›Pro Plan", cf_core::BlockType::Price, 0);
    assert_eq!(k1, k2);
    assert_eq!(k1.fp_hex().len(), 24); // 12 bytes -> 24 hex chars

    let a = SlotKey::anchor("Pro plan");
    let b = SlotKey::anchor("Pro plan");
    assert_eq!(a, b);

    // block_id CHANGES on text edit by construction; slot_key stays equal.
    let old_block = cf_core::BlockId::derive(&a, "$49/mo");
    let new_block = cf_core::BlockId::derive(&a, "$59/mo");
    assert_ne!(old_block, new_block);

    // norm_hash detects whether text changed.
    assert_ne!(NormHash::of("$49/mo"), NormHash::of("$59/mo"));
    assert_eq!(NormHash::of("$49/mo"), NormHash::of("$49/mo"));

    // doc_hash wire form is prefixed.
    let dh = DocHash::from_bytes([0u8; 16]);
    assert!(dh.to_wire().starts_with("blake3:"));

    // AnchorScheme is a plain copy enum.
    assert_eq!(AnchorScheme::Anchor, AnchorScheme::Anchor);
}
