//! `Clock` + `IdGen` (ARCHITECTURE.md §1, §4.2): the ONE place clock + RNG meet, isolated at emit
//! time. ULID (`cfe_`/`cfb_`) ids and the `obs` timestamp originate here; tests inject a frozen
//! clock + stepped id gen so golden tests assert exact event bytes including `id`/`obs`.

/// A source of the RFC3339 `obs` timestamp. Real impl reads the system clock; tests freeze it.
pub trait Clock {
    /// The current observation time as an RFC3339 string.
    fn now_rfc3339(&self) -> String;
}

/// A source of `cfe_`/`cfb_` ULIDs. Real impl uses clock + RNG; tests inject a stepped sequence.
pub trait IdGen {
    /// Mint a `cfe_<ULID>` event id.
    fn event_id(&mut self) -> String;
    /// Mint a `cfb_<ULID>` batch id.
    fn batch_id(&mut self) -> String;
}

/// The production clock (system time).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_rfc3339(&self) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Format as RFC3339 UTC ("Z") from the unix timestamp via the `time` crate (no extra deps).
        match cf_core::time::OffsetDateTime::from_unix_timestamp(secs) {
            Ok(dt) => format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                dt.year(),
                u8::from(dt.month()),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
            ),
            Err(_) => "1970-01-01T00:00:00Z".to_string(),
        }
    }
}

/// A frozen clock for tests/golden runs (`CF_FAKE_OBS`): always returns a fixed RFC3339 string.
#[derive(Clone, Debug)]
pub struct FixedClock(pub String);

impl Clock for FixedClock {
    fn now_rfc3339(&self) -> String {
        self.0.clone()
    }
}

/// A deterministic, stepped id generator for tests/golden runs. Mints `cfe_<prefix><n>` /
/// `cfb_<prefix><n>` with a monotonically increasing counter so golden event bytes are stable.
#[derive(Clone, Debug)]
pub struct SeededIdGen {
    prefix: String,
    event_n: u64,
    batch_n: u64,
}

impl SeededIdGen {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            event_n: 0,
            batch_n: 0,
        }
    }
}

impl IdGen for SeededIdGen {
    fn event_id(&mut self) -> String {
        let id = format!("cfe_{}{:06}", self.prefix, self.event_n);
        self.event_n += 1;
        id
    }

    fn batch_id(&mut self) -> String {
        let id = format!("cfb_{}{:06}", self.prefix, self.batch_n);
        self.batch_n += 1;
        id
    }
}

/// The production id generator (monotonic ULID over clock + RNG).
#[derive(Clone, Copy, Debug, Default)]
pub struct UlidGen;

impl IdGen for UlidGen {
    fn event_id(&mut self) -> String {
        format!("cfe_{}", ulid::Ulid::new())
    }

    fn batch_id(&mut self) -> String {
        format!("cfb_{}", ulid::Ulid::new())
    }
}
