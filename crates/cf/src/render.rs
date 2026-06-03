//! Output rendering (ARCHITECTURE.md §1, §4.6): jsonl / json / pretty. stdout = events ONLY,
//! stderr = logs (`tracing`). Never mix the two streams.

use cf_core::event;
use cf_core::{ChangeEvent, FeedEnvelope};

/// Output format selected by the cli flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Format {
    /// One minified event per line (default for agents).
    #[default]
    Jsonl,
    /// A single JSON document.
    Json,
    /// Human-readable, colorized (terminal use).
    Pretty,
}

/// Render events to stdout in the chosen format (events stream only — never logs). Returns the bytes
/// to write to stdout; an empty string means "print nothing" (the no-change cheap path).
pub fn render_events(events: &[ChangeEvent], format: Format) -> String {
    match format {
        Format::Jsonl => {
            let mut out = String::new();
            for ev in events {
                if let Ok(line) = event::to_wire(ev) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            out
        }
        Format::Json => {
            // A single object for one event, else `{"events":[...]}`.
            if events.len() == 1 {
                serde_json::to_string_pretty(&events[0]).unwrap_or_default()
            } else {
                serde_json::to_string_pretty(&serde_json::json!({ "events": events }))
                    .unwrap_or_default()
            }
        }
        Format::Pretty => {
            let mut out = String::new();
            for ev in events {
                out.push_str(&event::render_event_pretty(ev));
            }
            out
        }
    }
}

/// Render a feed envelope to stdout in the chosen format. For `cf check`, a no-change envelope
/// renders to an EMPTY string (exit 0 prints nothing); a baseline / change envelope renders per
/// format.
pub fn render_envelope(envelope: &FeedEnvelope, format: Format) -> String {
    match format {
        Format::Jsonl => {
            // For a change envelope we stream each event on its own line; for a baseline/no-change
            // envelope (no events) we emit the single envelope line so the agent's `--format jsonl`
            // baseline branch (exit 11) has a defined object.
            if envelope.events.is_empty() {
                match event::envelope_to_wire(envelope) {
                    Ok(s) => format!("{s}\n"),
                    Err(_) => String::new(),
                }
            } else {
                render_events(&envelope.events, Format::Jsonl)
            }
        }
        Format::Json => serde_json::to_string_pretty(envelope).unwrap_or_default(),
        Format::Pretty => event::render_envelope_pretty(envelope),
    }
}

/// Whether this envelope should print anything for `cf check` (a no-change crawl prints NOTHING and
/// exits 0; a baseline or change envelope prints). `etag_hit`/`to_rev==from_rev`/`n:0` is no-change.
pub fn is_no_change_for_check(envelope: &FeedEnvelope) -> bool {
    let c = &envelope.crawl;
    c.n == 0 && c.baseline != Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_core::event;
    use cf_core::FetchTier;

    fn baseline() -> FeedEnvelope {
        event::baseline_envelope(
            "t",
            "cfb_1",
            "2026-06-02T00:00:00Z",
            1,
            Some("https://x/y".into()),
            FetchTier::Http,
            200,
            None,
            Some("blake3:00".into()),
        )
    }

    #[test]
    fn no_change_envelope_is_silent_for_check() {
        let nc = event::no_change_envelope(
            "t",
            "cfb_1",
            "2026-06-02T00:00:00Z",
            41,
            None,
            FetchTier::Http,
            200,
            None,
            None,
            true,
        );
        assert!(is_no_change_for_check(&nc));
        // A baseline envelope is NOT a no-change (it prints the baseline shape, exit 11).
        assert!(!is_no_change_for_check(&baseline()));
    }

    #[test]
    fn baseline_jsonl_emits_one_envelope_line() {
        let s = render_envelope(&baseline(), Format::Jsonl);
        assert!(s.ends_with('\n'));
        assert!(s.contains(r#""baseline":true"#));
        // Exactly one line.
        assert_eq!(s.lines().count(), 1);
    }
}
