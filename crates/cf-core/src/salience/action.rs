//! §8.4 delta → one of the 8 agent-facing actions (first-match-wins over the pack's declared
//! rules). `verify_llm` is an internal scoring state and is NEVER an `Action` (App B, §6.5).
//!
//! First-match-wins (§8.4):
//! 1. **Sticky rule hit?** → use that rule's action (bypasses banding).
//! 2. **Else any matched rule with an explicit action?** → take the **highest-cost** action.
//! 3. **Else the band-default map** (per-pack overridable):
//!    `none→ignore, low→notify, medium→notify, high→re_run_downstream, critical→escalate_human`.
//! 4. **Uncertainty gate (internal):** `verify_llm` is INERT in MVP (`llm.enabled=false`) — the
//!    step-3 action stands and `verify_llm` is NEVER emitted.

use crate::model::{Action, Materiality};
use crate::packs::Pack;

/// How the action was decided (§8.5 `decided_by`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidedBy {
    /// A sticky or matched-rule action (§8.4 steps 1–2).
    Rule,
    /// The band-default map (§8.4 step 3).
    Band,
}

/// The §8.4 action decision: the chosen action, how it was decided, and the deciding rule id.
#[derive(Clone, Debug)]
pub struct ActionDecision {
    pub action: Action,
    pub decided_by: DecidedBy,
    /// The deciding rule id (sticky or highest-cost matched rule), if a rule decided it.
    pub rule_id: Option<String>,
}

/// §6.5 cost order (escalating). Lower index = cheaper. Used for the "highest-cost" tie-break.
pub fn action_cost(a: Action) -> u8 {
    match a {
        Action::Ignore => 0,
        Action::Notify => 1,
        Action::RefetchLinked => 2,
        Action::ReembedKb => 3,
        Action::ReRunDownstream => 4,
        Action::OpenTicket => 5,
        Action::EscalateHuman => 6,
        Action::PageOncall => 7,
    }
}

/// §8.4 — map a change unit's text + its materiality band to exactly one `Action`. `text` is the
/// new-side (present) text used to evaluate the pack's regex rules (first-match-wins, §8.3).
pub fn action_for(text: &str, mat: Materiality, pack: &Pack) -> ActionDecision {
    // (1) Sticky rule hit — bypasses banding. First sticky match in declared order wins.
    if let Some(rule) = pack
        .kw_rules
        .iter()
        .find(|r| r.sticky && r.action.is_some() && r.regex.is_match(text))
    {
        return ActionDecision {
            action: rule.action.expect("sticky rule has an action (filtered above)"),
            decided_by: DecidedBy::Rule,
            rule_id: Some(rule.id.clone()),
        };
    }

    // (2) Any matched rule with an explicit action → highest-cost among them.
    let mut best: Option<(&str, Action)> = None;
    for rule in pack.kw_rules.iter() {
        if let Some(act) = rule.action {
            if rule.regex.is_match(text) {
                match best {
                    Some((_, cur)) if action_cost(cur) >= action_cost(act) => {}
                    _ => best = Some((rule.id.as_str(), act)),
                }
            }
        }
    }
    if let Some((id, act)) = best {
        return ActionDecision {
            action: act,
            decided_by: DecidedBy::Rule,
            rule_id: Some(id.to_string()),
        };
    }

    // (3) Band-default map (per-pack overridable). (4) The LLM gate is inert in MVP.
    ActionDecision {
        action: pack.band_action.for_band(mat),
        decided_by: DecidedBy::Band,
        rule_id: None,
    }
}

/// §8.4 — the action decision for a category-routed unit. If a pack category rule names an action
/// it takes precedence over the band-default (a `cat`-row action is an explicit policy); a sticky
/// or matched regex rule still wins over it (steps 1–2 above). A `none`-band unit is the one
/// exception: it is below the noise floor (§6.4), so the cat-row action does NOT apply and the unit
/// defaults to `ignore` per the §8.4 band map — see the guard below.
pub fn action_for_cat(text: &str, cat: &str, mat: Materiality, pack: &Pack) -> ActionDecision {
    // Steps 1–2 (sticky / matched regex rule) take precedence.
    let regex_decision = action_for(text, mat, pack);
    if regex_decision.decided_by == DecidedBy::Rule {
        return regex_decision;
    }
    // A category routing row's explicit action (e.g. plan_removed → open_ticket) beats the band map
    // — but ONLY for a unit that actually cleared the noise floor. A `none`-band unit is, by
    // definition, below the §6.4 threshold: a noise-damped live counter (§7.3) or a sub-threshold
    // cosmetic edit. Such a unit must default to `ignore` per the §8.4 band map (`none→ignore`), not
    // be rescued to the category's action — otherwise a zero-token no-op would carry `notify`. The
    // ONLY documented way to escalate a sub-threshold change is a *sticky* rule (step 1, "regardless
    // of banding"), and that path already returned above. So gate the cat-row override on `mat≠none`.
    if mat != Materiality::None {
        if let Some(c) = pack.cat_rule(cat) {
            return ActionDecision {
                action: c.act,
                decided_by: DecidedBy::Rule,
                rule_id: None,
            };
        }
    }
    regex_decision
}
