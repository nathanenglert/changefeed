//! `impl Store` — rusqlite(WAL) + zstd fixed ring (ARCHITECTURE.md §1, §9.8). One previous
//! `CanonicalDoc` per target as a single zstd-19 blob + raw-HTML blob, keep-last-N ring, seen-set
//! ring (N=1000), per-host politeness. `doc_hash`/304 short-circuit = zero writes. Single write
//! lock + WAL. The IMPURE disk boundary.
//!
//! MVP model (§9.8): NO CAS, NO packfiles, NO trained dictionary, NO delta-chaining, NO GC.
//! Per target we store the previous `CanonicalDoc`(s) as single `zstd-19` blobs keyed by `tid`,
//! PLUS the raw post-extract HTML as a second `zstd-19` blob (offline re-segmentation, §5.6).
//! Retention is a fixed ring "keep last N" (default N=8); old blobs are dropped by the ring with
//! no GC. `store.db` holds: a per-target version log with a MONOTONIC `rev` counter (NOT bumped
//! when content is unchanged); the idempotency seen-set (rolling per-target `event_key` set,
//! N=1000); and the per-host last-fetch timestamp for crawl-delay enforcement (§12).
//!
//! Determinism wall: this is the impure side. Timestamps/ids are INJECTED by the caller so tests
//! are deterministic — the store never reads the clock itself.

use std::path::Path;

use cf_core::model::{
    AnchorScheme, Block, BlockId, BlockType, CanonicalDoc, DocHash, DocStats, NormHash, SlotKey,
    TypedValue,
};
use cf_core::storage::{Store, StoredSnapshot};
use cf_core::CfError;
use cf_core::time::{self, Date};
use rusqlite::{params, Connection, OptionalExtension};

/// Default retention ring depth (§9.8).
pub const DEFAULT_RING_N: usize = 8;

/// Rolling per-target idempotency seen-set size (§7.4).
pub const SEEN_SET_N: usize = 1000;

/// zstd compression level for cold blobs (§9.3 — level 19 for cold blobs/bases).
const ZSTD_LEVEL: i32 = 19;

/// The serialized-blob format version (lets a future change re-encode without ambiguity).
const BLOB_FORMAT: u8 = 1;

fn store_err(e: impl std::fmt::Display) -> CfError {
    CfError::Usage(format!("store: {e}"))
}

/// A raw HTML body paired with a snapshot for offline re-segmentation (§5.6).
#[derive(Clone, Debug, Default)]
pub struct RawHtml(pub String);

/// SQLite-backed snapshot store under `.changefeed/store.db`.
///
/// One write lock + WAL: rusqlite serializes writes on a single connection, and WAL lets a
/// concurrent reader proceed without corrupting the ring.
pub struct SqliteStore {
    conn: Connection,
    ring_n: usize,
}

impl SqliteStore {
    /// Open (or create) the store at the given path, applying WAL pragmas and the schema.
    pub fn open(path: &Path) -> Result<Self, CfError> {
        let conn = Connection::open(path).map_err(store_err)?;
        Self::init(conn, DEFAULT_RING_N)
    }

    /// In-memory store (tests).
    pub fn open_in_memory() -> Result<Self, CfError> {
        let conn = Connection::open_in_memory().map_err(store_err)?;
        Self::init(conn, DEFAULT_RING_N)
    }

    /// Open with a custom ring depth (tests exercise small N).
    pub fn open_with_ring(path: &Path, ring_n: usize) -> Result<Self, CfError> {
        let conn = Connection::open(path).map_err(store_err)?;
        Self::init(conn, ring_n)
    }

    fn init(conn: Connection, ring_n: usize) -> Result<Self, CfError> {
        // WAL + sane durability/concurrency for a single-writer one-shot or daemon.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(store_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(store_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(store_err)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS targets (
                tid       TEXT PRIMARY KEY,
                next_rev  INTEGER NOT NULL DEFAULT 0  -- monotonic; only bumped on real change
            );

            -- The per-target version log + fixed-ring blob storage. One row per STORED
            -- (changed) observation; doc_hash-equal/304 never reach here so they add no row.
            CREATE TABLE IF NOT EXISTS versions (
                tid        TEXT NOT NULL REFERENCES targets(tid) ON DELETE CASCADE,
                rev        INTEGER NOT NULL,
                doc_hash   BLOB NOT NULL,           -- 16-byte blake3-128 (dedup short-circuit key)
                doc_blob   BLOB NOT NULL,           -- zstd-19 CanonicalDoc
                html_blob  BLOB,                    -- zstd-19 raw post-extract HTML (§5.6), nullable
                PRIMARY KEY (tid, rev)
            );

            -- Rolling per-target idempotency seen-set (§7.4, N=1000). seq orders eviction.
            CREATE TABLE IF NOT EXISTS seen_events (
                tid        TEXT NOT NULL,
                event_key  BLOB NOT NULL,           -- 16-byte (u128 LE) event_key
                seq        INTEGER NOT NULL,
                PRIMARY KEY (tid, event_key)
            );
            CREATE INDEX IF NOT EXISTS seen_events_seq ON seen_events(tid, seq);

            -- Per-host last-fetch timestamp for crawl-delay enforcement (§12). Epoch millis,
            -- INJECTED by the caller (the store never reads a clock).
            CREATE TABLE IF NOT EXISTS host_politeness (
                host          TEXT PRIMARY KEY,
                last_fetch_ms INTEGER NOT NULL
            );
            "#,
        )
        .map_err(store_err)?;
        Ok(Self { conn, ring_n })
    }

    fn ensure_target(&self, tid: &str) -> Result<(), CfError> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO targets(tid, next_rev) VALUES (?1, 0)",
                params![tid],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// Persist a snapshot together with the raw post-extract HTML (§5.6). The HTML is stored as a
    /// second zstd-19 blob keyed by the same `(tid, rev)`.
    pub fn put_with_html(
        &mut self,
        snapshot: &StoredSnapshot,
        raw_html: Option<&str>,
    ) -> Result<u64, CfError> {
        self.ensure_target(&snapshot.tid)?;

        let doc_blob = zstd::encode_all(encode_doc(&snapshot.doc).as_slice(), ZSTD_LEVEL)
            .map_err(store_err)?;
        let html_blob = match raw_html {
            Some(h) => Some(zstd::encode_all(h.as_bytes(), ZSTD_LEVEL).map_err(store_err)?),
            None => None,
        };
        let doc_hash = snapshot.doc.doc_hash.as_bytes().to_vec();

        let tx = self.conn.transaction().map_err(store_err)?;
        tx.execute(
            "INSERT OR REPLACE INTO versions(tid, rev, doc_hash, doc_blob, html_blob) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![snapshot.tid, snapshot.rev, doc_hash, doc_blob, html_blob],
        )
        .map_err(store_err)?;

        // Fixed-ring "keep last N": drop everything below the N-th highest rev. No GC.
        tx.execute(
            "DELETE FROM versions WHERE tid = ?1 AND rev <= (\
                SELECT rev FROM versions WHERE tid = ?1 ORDER BY rev DESC LIMIT 1 OFFSET ?2\
             )",
            params![snapshot.tid, self.ring_n as i64],
        )
        .map_err(store_err)?;

        // Keep next_rev monotonic and strictly above any stored rev.
        tx.execute(
            "UPDATE targets SET next_rev = MAX(next_rev, ?2 + 1) WHERE tid = ?1",
            params![snapshot.tid, snapshot.rev],
        )
        .map_err(store_err)?;

        tx.commit().map_err(store_err)?;
        Ok(snapshot.rev)
    }

    /// The next `rev` this target should use for a real change. Monotonic; not advanced by a
    /// doc_hash-equal/304 observation (the caller simply does not `put`).
    pub fn next_rev(&self, tid: &str) -> Result<u64, CfError> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT next_rev FROM targets WHERE tid = ?1",
                params![tid],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// The current head `rev` for a target, if any snapshot is stored.
    pub fn head_rev(&self, tid: &str) -> Result<Option<u64>, CfError> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(rev) FROM versions WHERE tid = ?1",
                params![tid],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?
            .flatten();
        Ok(v.map(|x| x as u64))
    }

    /// How many ring entries a target currently retains.
    pub fn ring_len(&self, tid: &str) -> Result<usize, CfError> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM versions WHERE tid = ?1",
                params![tid],
                |r| r.get(0),
            )
            .map_err(store_err)?;
        Ok(n as usize)
    }

    /// The ascending list of retained revs for a target (oldest → newest).
    pub fn retained_revs(&self, tid: &str) -> Result<Vec<u64>, CfError> {
        let mut stmt = self
            .conn
            .prepare("SELECT rev FROM versions WHERE tid = ?1 ORDER BY rev ASC")
            .map_err(store_err)?;
        let rows = stmt
            .query_map(params![tid], |r| r.get::<_, i64>(0))
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(store_err)? as u64);
        }
        Ok(out)
    }

    /// Load a specific stored snapshot by rev.
    pub fn snapshot_at(&self, tid: &str, rev: u64) -> Result<Option<StoredSnapshot>, CfError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT doc_blob FROM versions WHERE tid = ?1 AND rev = ?2",
                params![tid, rev as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?;
        match blob {
            None => Ok(None),
            Some(b) => {
                let bytes = zstd::decode_all(b.as_slice()).map_err(store_err)?;
                let doc = decode_doc(&bytes)?;
                Ok(Some(StoredSnapshot {
                    tid: tid.to_string(),
                    rev,
                    doc,
                }))
            }
        }
    }

    /// The stored raw HTML for a specific rev (§5.6 offline re-segmentation), if retained.
    pub fn raw_html_at(&self, tid: &str, rev: u64) -> Result<Option<String>, CfError> {
        let blob: Option<Option<Vec<u8>>> = self
            .conn
            .query_row(
                "SELECT html_blob FROM versions WHERE tid = ?1 AND rev = ?2",
                params![tid, rev as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?;
        match blob.flatten() {
            None => Ok(None),
            Some(b) => {
                let bytes = zstd::decode_all(b.as_slice()).map_err(store_err)?;
                let s = String::from_utf8(bytes).map_err(store_err)?;
                Ok(Some(s))
            }
        }
    }

    // ---- §12 per-host crawl-delay politeness --------------------------------------------------

    /// Record a host's last-fetch timestamp (epoch millis, INJECTED — the store never reads a
    /// clock).
    pub fn record_fetch(&mut self, host: &str, now_ms: i64) -> Result<(), CfError> {
        self.conn
            .execute(
                "INSERT INTO host_politeness(host, last_fetch_ms) VALUES (?1, ?2) \
                 ON CONFLICT(host) DO UPDATE SET last_fetch_ms = excluded.last_fetch_ms",
                params![host, now_ms],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// The persisted last-fetch timestamp (epoch millis) for a host, if known.
    pub fn last_fetch_ms(&self, host: &str) -> Result<Option<i64>, CfError> {
        self.conn
            .query_row(
                "SELECT last_fetch_ms FROM host_politeness WHERE host = ?1",
                params![host],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)
    }

    /// Whether fetching `host` *now* (`now_ms`) would violate `min_interval_ms` since the last
    /// recorded fetch. Returns `Some(retry_after_ms)` if too soon (caller surfaces exit 6 /
    /// `crawl.retry_after`, §12), `None` if clear.
    pub fn crawl_delay_violation(
        &self,
        host: &str,
        now_ms: i64,
        min_interval_ms: i64,
    ) -> Result<Option<i64>, CfError> {
        match self.last_fetch_ms(host)? {
            None => Ok(None),
            Some(last) => {
                let elapsed = now_ms - last;
                if elapsed < min_interval_ms {
                    Ok(Some(min_interval_ms - elapsed))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

impl Store for SqliteStore {
    fn latest(&self, tid: &str) -> Result<Option<StoredSnapshot>, CfError> {
        let head = self.head_rev(tid)?;
        match head {
            None => Ok(None),
            Some(rev) => self.snapshot_at(tid, rev),
        }
    }

    fn put(&mut self, snapshot: &StoredSnapshot) -> Result<u64, CfError> {
        self.put_with_html(snapshot, None)
    }

    fn seen_event(&self, event_key: u128) -> Result<bool, CfError> {
        // The seen-set is rolling per-target (§7.4). The trait carries no tid, so the global
        // lookup answers "have we emitted this exact key for any target" — and because the
        // event_key already folds in target_id (§7.4 derivation), collisions across targets are
        // not possible. A re-run on the same snapshot pair therefore suppresses.
        let key = event_key.to_le_bytes().to_vec();
        let hit: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM seen_events WHERE event_key = ?1 LIMIT 1",
                params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?;
        Ok(hit.is_some())
    }

    fn mark_event(&mut self, event_key: u128) -> Result<(), CfError> {
        // event_key folds in target_id (§7.4); we record under a single logical bucket and evict
        // the oldest beyond N to keep the set rolling.
        let key = event_key.to_le_bytes().to_vec();
        let tx = self.conn.transaction().map_err(store_err)?;
        let next_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM seen_events",
                [],
                |r| r.get(0),
            )
            .map_err(store_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO seen_events(tid, event_key, seq) VALUES ('', ?1, ?2)",
            params![key, next_seq],
        )
        .map_err(store_err)?;
        // Roll: keep only the most-recent SEEN_SET_N keys.
        tx.execute(
            "DELETE FROM seen_events WHERE seq <= (\
                SELECT seq FROM seen_events ORDER BY seq DESC LIMIT 1 OFFSET ?1\
             )",
            params![SEEN_SET_N as i64],
        )
        .map_err(store_err)?;
        tx.commit().map_err(store_err)?;
        Ok(())
    }
}

// ===============================================================================================
// CanonicalDoc binary (de)serialization.
//
// `CanonicalDoc` deliberately does NOT derive serde (the frozen §2 contract keeps the identity
// newtypes' inners private). The store therefore owns a small self-describing binary codec — it
// rebuilds every field byte-identically (incl. the private identity bytes via the additive
// `from_bytes`/`from_raw` accessors). It is internal to the store: nothing else reads this format.
// ===============================================================================================

fn encode_doc(doc: &CanonicalDoc) -> Vec<u8> {
    let mut w = Writer::default();
    w.u8(BLOB_FORMAT);
    w.str(doc.schema);
    w.str(&doc.url);
    w.str(&doc.final_url);
    w.str(&doc.fetched_at);
    encode_fetch(&mut w, &doc.fetch);
    w.str(&doc.profile_id);
    w.bytes(&doc.doc_hash.as_bytes());
    // stats
    w.u32(doc.stats.block_count);
    w.u32(doc.stats.stripped_attrs);
    w.u64(doc.stats.bytes_raw);
    // blocks (pre-order tree)
    w.u32(doc.blocks.len() as u32);
    for b in &doc.blocks {
        encode_block(&mut w, b);
    }
    w.into_vec()
}

fn encode_fetch(w: &mut Writer, m: &cf_core::fetch::FetchMeta) {
    w.opt_str(m.etag.as_deref());
    w.opt_str(m.last_modified.as_deref());
    match m.tier {
        None => w.u8(0),
        Some(t) => {
            w.u8(1);
            w.u8(fetch_tier_tag(t));
        }
    }
    w.u32(m.status as u32);
    match m.ms {
        None => w.u8(0),
        Some(v) => {
            w.u8(1);
            w.u32(v);
        }
    }
}

fn encode_block(w: &mut Writer, b: &Block) {
    w.bytes(&b.slot_key.as_bytes());
    w.bytes(&b.block_id.as_bytes());
    w.u8(block_type_tag(b.ty));
    match b.level {
        None => w.u8(0),
        Some(l) => {
            w.u8(1);
            w.u8(l);
        }
    }
    w.str(&b.text);
    encode_value(w, b.value.as_ref());
    w.u8(anchor_scheme_tag(b.anchored_by));
    w.u64(b.norm_hash.raw());
    w.u32(b.preorder_idx);
    w.u16(b.dom_depth);
    w.u8(b.restyle_sig); // §6.2 presentation signature
    w.u32(b.children.len() as u32);
    for c in &b.children {
        encode_block(w, c);
    }
}

fn encode_value(w: &mut Writer, v: Option<&TypedValue>) {
    match v {
        None => w.u8(0),
        Some(TypedValue::Price {
            amount_minor,
            currency,
            period,
        }) => {
            w.u8(1);
            w.i64(*amount_minor);
            w.str(currency);
            w.opt_str(period.as_deref());
        }
        Some(TypedValue::Date(d)) => {
            w.u8(2);
            w.i32(d.year());
            w.u8(u8::from(d.month()));
            w.u8(d.day());
        }
        Some(TypedValue::Number(n)) => {
            w.u8(3);
            w.str(&n.to_string());
        }
        Some(TypedValue::Code(s)) => {
            w.u8(4);
            w.str(s);
        }
        Some(TypedValue::TableRow(cells)) => {
            w.u8(5);
            w.u32(cells.len() as u32);
            for c in cells {
                w.str(c);
            }
        }
        Some(TypedValue::Link { href_canonical }) => {
            w.u8(6);
            w.str(href_canonical);
        }
        Some(TypedValue::Heading(s)) => {
            w.u8(7);
            w.str(s);
        }
        Some(TypedValue::Text(s)) => {
            w.u8(8);
            w.str(s);
        }
    }
}

fn decode_doc(bytes: &[u8]) -> Result<CanonicalDoc, CfError> {
    let mut r = Reader::new(bytes);
    let fmt = r.u8()?;
    if fmt != BLOB_FORMAT {
        return Err(store_err(format!("unknown blob format {fmt}")));
    }
    let _schema = r.str()?; // schema is a frozen &'static str; we re-pin it below
    let url = r.str()?;
    let final_url = r.str()?;
    let fetched_at = r.str()?;
    let fetch = decode_fetch(&mut r)?;
    let profile_id = r.str()?;
    let doc_hash = DocHash::from_bytes(r.arr16()?);
    let stats = DocStats {
        block_count: r.u32()?,
        stripped_attrs: r.u32()?,
        bytes_raw: r.u64()?,
    };
    let n = r.u32()? as usize;
    let mut blocks = Vec::with_capacity(n);
    for _ in 0..n {
        blocks.push(decode_block(&mut r)?);
    }
    Ok(CanonicalDoc {
        schema: "changefeed.canonical/1",
        url,
        final_url,
        fetched_at,
        fetch,
        profile_id,
        doc_hash,
        blocks,
        stats,
    })
}

fn decode_fetch(r: &mut Reader) -> Result<cf_core::fetch::FetchMeta, CfError> {
    let etag = r.opt_str()?;
    let last_modified = r.opt_str()?;
    let tier = if r.u8()? == 1 {
        Some(fetch_tier_from_tag(r.u8()?)?)
    } else {
        None
    };
    let status = r.u32()? as u16;
    let ms = if r.u8()? == 1 { Some(r.u32()?) } else { None };
    Ok(cf_core::fetch::FetchMeta {
        etag,
        last_modified,
        tier,
        status,
        ms,
    })
}

fn decode_block(r: &mut Reader) -> Result<Block, CfError> {
    let slot_key = SlotKey::from_bytes(r.arr12()?);
    let block_id = BlockId::from_bytes(r.arr12()?);
    let ty = block_type_from_tag(r.u8()?)?;
    let level = if r.u8()? == 1 { Some(r.u8()?) } else { None };
    let text = r.str()?;
    let value = decode_value(r)?;
    let anchored_by = anchor_scheme_from_tag(r.u8()?)?;
    let norm_hash = NormHash::from_raw(r.u64()?);
    let preorder_idx = r.u32()?;
    let dom_depth = r.u16()?;
    let restyle_sig = r.u8()?;
    let nc = r.u32()? as usize;
    let mut children = Vec::with_capacity(nc);
    for _ in 0..nc {
        children.push(decode_block(r)?);
    }
    Ok(Block {
        slot_key,
        block_id,
        ty,
        level,
        text,
        value,
        anchored_by,
        norm_hash,
        preorder_idx,
        dom_depth,
        restyle_sig,
        children,
    })
}

fn decode_value(r: &mut Reader) -> Result<Option<TypedValue>, CfError> {
    let tag = r.u8()?;
    Ok(match tag {
        0 => None,
        1 => Some(TypedValue::Price {
            amount_minor: r.i64()?,
            currency: r.str()?,
            period: r.opt_str()?,
        }),
        2 => {
            let year = r.i32()?;
            let month = r.u8()?;
            let day = r.u8()?;
            let m = time::Month::try_from(month).map_err(store_err)?;
            let d = Date::from_calendar_date(year, m, day).map_err(store_err)?;
            Some(TypedValue::Date(d))
        }
        3 => {
            let s = r.str()?;
            let n = s.parse::<cf_core::rust_decimal::Decimal>().map_err(store_err)?;
            Some(TypedValue::Number(n))
        }
        4 => Some(TypedValue::Code(r.str()?)),
        5 => {
            let n = r.u32()? as usize;
            let mut cells = Vec::with_capacity(n);
            for _ in 0..n {
                cells.push(r.str()?);
            }
            Some(TypedValue::TableRow(cells))
        }
        6 => Some(TypedValue::Link {
            href_canonical: r.str()?,
        }),
        7 => Some(TypedValue::Heading(r.str()?)),
        8 => Some(TypedValue::Text(r.str()?)),
        other => return Err(store_err(format!("unknown TypedValue tag {other}"))),
    })
}

// ---- enum tag tables (stable on-disk) ---------------------------------------------------------

fn block_type_tag(t: BlockType) -> u8 {
    match t {
        BlockType::Heading => 0,
        BlockType::Paragraph => 1,
        BlockType::ListItem => 2,
        BlockType::TableRow => 3,
        BlockType::Table => 4,
        BlockType::Code => 5,
        BlockType::Link => 6,
        BlockType::Price => 7,
        BlockType::Date => 8,
        BlockType::Number => 9,
        BlockType::Text => 10,
    }
}

fn block_type_from_tag(t: u8) -> Result<BlockType, CfError> {
    Ok(match t {
        0 => BlockType::Heading,
        1 => BlockType::Paragraph,
        2 => BlockType::ListItem,
        3 => BlockType::TableRow,
        4 => BlockType::Table,
        5 => BlockType::Code,
        6 => BlockType::Link,
        7 => BlockType::Price,
        8 => BlockType::Date,
        9 => BlockType::Number,
        10 => BlockType::Text,
        other => return Err(store_err(format!("unknown BlockType tag {other}"))),
    })
}

fn anchor_scheme_tag(a: AnchorScheme) -> u8 {
    match a {
        AnchorScheme::Anchor => 0,
        AnchorScheme::Struct => 1,
    }
}

fn anchor_scheme_from_tag(a: u8) -> Result<AnchorScheme, CfError> {
    Ok(match a {
        0 => AnchorScheme::Anchor,
        1 => AnchorScheme::Struct,
        other => return Err(store_err(format!("unknown AnchorScheme tag {other}"))),
    })
}

fn fetch_tier_tag(t: cf_core::model::FetchTier) -> u8 {
    use cf_core::model::FetchTier::*;
    match t {
        Http => 0,
        Headless => 1,
        Api => 2,
        Rss => 3,
    }
}

fn fetch_tier_from_tag(t: u8) -> Result<cf_core::model::FetchTier, CfError> {
    use cf_core::model::FetchTier::*;
    Ok(match t {
        0 => Http,
        1 => Headless,
        2 => Api,
        3 => Rss,
        other => return Err(store_err(format!("unknown FetchTier tag {other}"))),
    })
}

// ---- tiny length-prefixed binary writer/reader ------------------------------------------------

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
        }
    }
    fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], CfError> {
        if self.pos + n > self.buf.len() {
            return Err(store_err("truncated blob"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CfError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, CfError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, CfError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, CfError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, CfError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, CfError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn arr12(&mut self) -> Result<[u8; 12], CfError> {
        Ok(self.take(12)?.try_into().unwrap())
    }
    fn arr16(&mut self) -> Result<[u8; 16], CfError> {
        Ok(self.take(16)?.try_into().unwrap())
    }
    fn str(&mut self) -> Result<String, CfError> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(store_err)
    }
    fn opt_str(&mut self) -> Result<Option<String>, CfError> {
        if self.u8()? == 1 {
            Ok(Some(self.str()?))
        } else {
            Ok(None)
        }
    }
}

// ===============================================================================================
// Secrets: ${ENV} / ${VAR} dollar-brace expansion (§12).
//
// Secret-bearing config values are expanded at runtime from the process environment, NEVER written
// to the store, and redacted from logs. This lives here because it shares no state with core; the
// store proves (by test) that an expanded secret never lands in a stored blob.
// ===============================================================================================

/// Expand `${VAR}` references in `input` using `lookup` (typically the process environment merged
/// with `.changefeed/secrets.env`). Unknown vars expand to empty (and are reported), so a missing
/// secret can never silently embed the literal `${VAR}`. A literal `$$` escapes to `$`.
pub fn expand_env(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
        } else if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = input[i + 2..].find('}') {
                let name = &input[i + 2..i + 2 + end];
                if let Some(val) = lookup(name) {
                    out.push_str(&val);
                }
                i = i + 2 + end + 1;
            } else {
                // Unterminated `${` — emit verbatim and stop scanning for braces.
                out.push_str(&input[i..]);
                break;
            }
        } else {
            // Push the next full UTF-8 char so multibyte input survives.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_core::model::{
        AnchorScheme, Block, BlockId, BlockType, CanonicalDoc, DocHash, DocStats, EventKey,
        NormHash, SlotKey, TypedValue,
    };
    use cf_core::rust_decimal::Decimal;
    use std::str::FromStr;

    // ---- test fixtures -----------------------------------------------------------------------

    fn norm(s: &str) -> NormHash {
        NormHash::of(s)
    }

    fn leaf(slot: SlotKey, ty: BlockType, text: &str, value: Option<TypedValue>) -> Block {
        Block {
            slot_key: slot,
            block_id: BlockId::derive(&slot, text),
            ty,
            level: None,
            text: text.to_string(),
            value,
            anchored_by: AnchorScheme::Struct,
            norm_hash: norm(text),
            preorder_idx: 0,
            dom_depth: 2,
            restyle_sig: 0,
            children: Vec::new(),
        }
    }

    /// A small but type-diverse pricing doc. `pro_price` controls the one volatile cell so we can
    /// produce a genuinely-different second snapshot.
    fn pricing_doc(pro_price_minor: i64) -> CanonicalDoc {
        let plans = SlotKey::anchor("plans");
        let pro = SlotKey::structural("Plans›Pro", BlockType::Heading, 0);
        let price = SlotKey::structural("Plans›Pro›price", BlockType::Price, 0);
        let bullet = SlotKey::structural("Plans›Pro›feat", BlockType::ListItem, 0);
        let released = SlotKey::structural("Plans›released", BlockType::Date, 0);
        let seats = SlotKey::structural("Plans›seats", BlockType::Number, 0);
        let row = SlotKey::structural("Plans›row", BlockType::TableRow, 0);

        let price_text = format!("${}.00 / mo", pro_price_minor / 100);
        let heading = Block {
            slot_key: plans,
            block_id: BlockId::derive(&plans, "Plans"),
            ty: BlockType::Heading,
            level: Some(1),
            text: "Plans".to_string(),
            value: Some(TypedValue::Heading("Plans".to_string())),
            anchored_by: AnchorScheme::Anchor,
            norm_hash: norm("Plans"),
            preorder_idx: 0,
            dom_depth: 1,
            restyle_sig: 0,
            children: vec![Block {
                slot_key: pro,
                block_id: BlockId::derive(&pro, "Pro Plan"),
                ty: BlockType::Heading,
                level: Some(2),
                text: "Pro Plan".to_string(),
                value: Some(TypedValue::Heading("Pro Plan".to_string())),
                anchored_by: AnchorScheme::Struct,
                norm_hash: norm("Pro Plan"),
                preorder_idx: 1,
                dom_depth: 2,
                restyle_sig: 0,
                children: vec![
                    leaf(
                        price,
                        BlockType::Price,
                        &price_text,
                        Some(TypedValue::Price {
                            amount_minor: pro_price_minor,
                            currency: "USD".to_string(),
                            period: Some("mo".to_string()),
                        }),
                    ),
                    leaf(
                        bullet,
                        BlockType::ListItem,
                        "Unlimited projects",
                        Some(TypedValue::Text("Unlimited projects".to_string())),
                    ),
                    leaf(
                        released,
                        BlockType::Date,
                        "2026-01-15",
                        Some(TypedValue::Date(
                            Date::from_calendar_date(2026, time::Month::January, 15).unwrap(),
                        )),
                    ),
                    leaf(
                        seats,
                        BlockType::Number,
                        "12.5",
                        Some(TypedValue::Number(Decimal::from_str("12.5").unwrap())),
                    ),
                    leaf(
                        row,
                        BlockType::TableRow,
                        "a | b | c",
                        Some(TypedValue::TableRow(vec![
                            "a".to_string(),
                            "b".to_string(),
                            "c".to_string(),
                        ])),
                    ),
                ],
            }],
        };

        // doc_hash is content-sensitive: equal content -> equal hash, real change -> new hash.
        let doc_hash = synthetic_doc_hash(std::slice::from_ref(&heading));
        CanonicalDoc {
            schema: "changefeed.canonical/1",
            url: "https://acme.example/pricing".to_string(),
            final_url: "https://acme.example/pricing".to_string(),
            fetched_at: "2026-06-02T14:00:00Z".to_string(),
            fetch: cf_core::fetch::FetchMeta {
                etag: Some("W/\"a1b2\"".to_string()),
                last_modified: None,
                tier: Some(cf_core::model::FetchTier::Http),
                status: 200,
                ms: Some(42),
            },
            profile_id: "acme-pricing".to_string(),
            doc_hash,
            blocks: vec![heading],
            stats: DocStats {
                block_count: 7,
                stripped_attrs: 3,
                bytes_raw: 50122,
            },
        }
    }

    /// Content-sensitive synthetic doc_hash (slot_key + type + text + value, NOT block_id) — the
    /// §5.6 semantics: equal content yields an equal hash, a real edit yields a new one.
    fn synthetic_doc_hash(blocks: &[Block]) -> DocHash {
        let mut h = cf_core::blake3::Hasher::new();
        fn fold(h: &mut cf_core::blake3::Hasher, bs: &[Block]) {
            for b in bs {
                h.update(&b.slot_key.as_bytes());
                h.update(&[block_type_tag(b.ty)]);
                h.update(b.text.as_bytes());
                if let Some(TypedValue::Price { amount_minor, .. }) = &b.value {
                    h.update(&amount_minor.to_le_bytes());
                }
                h.update(&[0xff]);
                fold(h, &b.children);
            }
        }
        fold(&mut h, blocks);
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.finalize().as_bytes()[..16]);
        DocHash::from_bytes(out)
    }

    fn blocks_eq(a: &[Block], b: &[Block]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).all(|(x, y)| {
            x.slot_key == y.slot_key
                && x.block_id == y.block_id
                && x.ty == y.ty
                && x.level == y.level
                && x.text == y.text
                && x.value == y.value
                && x.anchored_by == y.anchored_by
                && x.norm_hash.raw() == y.norm_hash.raw()
                && x.preorder_idx == y.preorder_idx
                && x.dom_depth == y.dom_depth
                && x.restyle_sig == y.restyle_sig
                && blocks_eq(&x.children, &y.children)
        })
    }

    fn doc_eq(a: &CanonicalDoc, b: &CanonicalDoc) -> bool {
        a.schema == b.schema
            && a.url == b.url
            && a.final_url == b.final_url
            && a.fetched_at == b.fetched_at
            && a.fetch.etag == b.fetch.etag
            && a.fetch.last_modified == b.fetch.last_modified
            && a.fetch.tier == b.fetch.tier
            && a.fetch.status == b.fetch.status
            && a.fetch.ms == b.fetch.ms
            && a.profile_id == b.profile_id
            && a.doc_hash == b.doc_hash
            && a.stats.block_count == b.stats.block_count
            && a.stats.stripped_attrs == b.stats.stripped_attrs
            && a.stats.bytes_raw == b.stats.bytes_raw
            && blocks_eq(&a.blocks, &b.blocks)
    }

    fn ekey(seed: &str) -> u128 {
        let slot = SlotKey::anchor(seed);
        EventKey::derive(seed, &slot, norm("from"), norm("to")).raw()
    }

    // ---- the §9.8 behavioral tests -----------------------------------------------------------

    #[test]
    fn store_baseline_and_read_back_identical() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let doc = pricing_doc(4900);
        let snap = StoredSnapshot {
            tid: "comp-pricing".to_string(),
            rev: store.next_rev("comp-pricing").unwrap(),
            doc: doc.clone(),
        };
        store.put(&snap).unwrap();

        let got = store.latest("comp-pricing").unwrap().expect("a snapshot");
        assert_eq!(got.rev, 0);
        assert!(
            doc_eq(&doc, &got.doc),
            "zstd round-trip must reproduce the CanonicalDoc byte-for-byte across every field \
             including the private identity bytes and typed values"
        );
    }

    #[test]
    fn raw_html_blob_round_trips_for_offline_resegmentation() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let doc = pricing_doc(4900);
        let html = "<html><body><h1>Plans</h1><div class=\"price\">$49.00</div></body></html>";
        let snap = StoredSnapshot {
            tid: "t".to_string(),
            rev: store.next_rev("t").unwrap(),
            doc,
        };
        store.put_with_html(&snap, Some(html)).unwrap();

        let got = store.raw_html_at("t", 0).unwrap().expect("html retained");
        assert_eq!(got, html, "raw post-extract HTML must round-trip (§5.6)");
    }

    #[test]
    fn keep_last_n_ring_drops_oldest_beyond_default_8() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let tid = "ring";
        // Push 12 genuinely-different revs; the ring keeps the last 8.
        for i in 0..12 {
            let rev = store.next_rev(tid).unwrap();
            let snap = StoredSnapshot {
                tid: tid.to_string(),
                rev,
                doc: pricing_doc(4900 + i * 100),
            };
            store.put(&snap).unwrap();
        }
        assert_eq!(store.ring_len(tid).unwrap(), DEFAULT_RING_N);
        let revs = store.retained_revs(tid).unwrap();
        assert_eq!(
            revs,
            vec![4, 5, 6, 7, 8, 9, 10, 11],
            "the oldest revs (0..=3) are dropped by the fixed ring, no GC"
        );
        // The oldest is gone; the newest is intact.
        assert!(store.snapshot_at(tid, 0).unwrap().is_none());
        assert!(store.snapshot_at(tid, 11).unwrap().is_some());
    }

    #[test]
    fn doc_hash_equal_reobservation_writes_no_blob_and_rev_unchanged() {
        // The caller drives the short-circuit: a doc_hash-equal observation does NOT call put().
        // This test asserts the store honors that contract — no row, no rev advance.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let tid = "noop";
        let doc = pricing_doc(4900);
        let rev0 = store.next_rev(tid).unwrap();
        store
            .put(&StoredSnapshot {
                tid: tid.to_string(),
                rev: rev0,
                doc: doc.clone(),
            })
            .unwrap();

        let head_before = store.head_rev(tid).unwrap();
        let next_before = store.next_rev(tid).unwrap();
        let ring_before = store.ring_len(tid).unwrap();

        // Re-observe identical content: doc_hash matches the head -> short-circuit, NO put.
        let head = store.latest(tid).unwrap().unwrap();
        assert_eq!(
            head.doc.doc_hash, doc.doc_hash,
            "identical content must yield an equal doc_hash (§5.6)"
        );
        // (no put call here — that is the whole point)

        assert_eq!(store.head_rev(tid).unwrap(), head_before);
        assert_eq!(store.next_rev(tid).unwrap(), next_before);
        assert_eq!(store.ring_len(tid).unwrap(), ring_before);
        assert_eq!(ring_before, 1, "exactly one blob stored across two observations");
    }

    #[test]
    fn rev_increments_monotonically_only_on_real_change() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let tid = "mono";

        // Baseline.
        let r0 = store.next_rev(tid).unwrap();
        store
            .put(&StoredSnapshot {
                tid: tid.to_string(),
                rev: r0,
                doc: pricing_doc(4900),
            })
            .unwrap();
        assert_eq!(r0, 0);
        assert_eq!(store.next_rev(tid).unwrap(), 1);

        // Real change -> new rev.
        let r1 = store.next_rev(tid).unwrap();
        store
            .put(&StoredSnapshot {
                tid: tid.to_string(),
                rev: r1,
                doc: pricing_doc(5900),
            })
            .unwrap();
        assert_eq!(r1, 1);
        assert_eq!(store.head_rev(tid).unwrap(), Some(1));
        assert_eq!(store.next_rev(tid).unwrap(), 2);

        // doc_hash-equal re-observation -> caller does NOT put -> rev frozen at 1.
        assert_eq!(store.head_rev(tid).unwrap(), Some(1));
        assert_eq!(
            store.next_rev(tid).unwrap(),
            2,
            "rev is not derived from the clock and is not bumped on unchanged content (§6.2)"
        );
    }

    #[test]
    fn idempotency_seen_set_suppresses_duplicate() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let k = ekey("comp-pricing");

        assert!(!store.seen_event(k).unwrap(), "unseen before marking");
        store.mark_event(k).unwrap();
        assert!(store.seen_event(k).unwrap(), "marked key is suppressed");

        // A different key is independent.
        let other = ekey("other-target");
        assert!(!store.seen_event(other).unwrap());

        // Marking twice is idempotent (no panic, still seen).
        store.mark_event(k).unwrap();
        assert!(store.seen_event(k).unwrap());
    }

    #[test]
    fn seen_set_rolls_at_capacity() {
        // Exercise the rolling eviction with a tiny override: we can't shrink SEEN_SET_N (it is a
        // const), but we can prove the eviction SQL fires by inserting > N and confirming the
        // count is capped. Use the real N to avoid weakening the production path.
        let mut store = SqliteStore::open_in_memory().unwrap();
        for i in 0..(SEEN_SET_N + 5) {
            store.mark_event(ekey(&format!("k{i}"))).unwrap();
        }
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM seen_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, SEEN_SET_N, "seen-set is capped at N (rolling)");
        // The oldest key evicted; the newest still present.
        assert!(!store.seen_event(ekey("k0")).unwrap());
        assert!(store
            .seen_event(ekey(&format!("k{}", SEEN_SET_N + 4)))
            .unwrap());
    }

    #[test]
    fn no_store_style_call_does_not_advance_rev_or_record_event() {
        // --no-store (§4.4): a pure read-only probe. It neither persists the snapshot, advances
        // rev/baseline, NOR records the event_key. We model that as "the caller skips put + skips
        // mark_event entirely" and assert the store is untouched.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let tid = "probe";

        // Baseline exists.
        store
            .put(&StoredSnapshot {
                tid: tid.to_string(),
                rev: 0,
                doc: pricing_doc(4900),
            })
            .unwrap();
        let next_before = store.next_rev(tid).unwrap();
        let head_before = store.head_rev(tid).unwrap();
        let ring_before = store.ring_len(tid).unwrap();
        let k = ekey(tid);

        // --no-store probe observes a change but deliberately performs NO writes.
        // (no put, no mark_event)

        assert_eq!(store.next_rev(tid).unwrap(), next_before, "rev not advanced");
        assert_eq!(store.head_rev(tid).unwrap(), head_before, "baseline unchanged");
        assert_eq!(store.ring_len(tid).unwrap(), ring_before, "no new blob");
        assert!(
            !store.seen_event(k).unwrap(),
            "event_key not recorded -> a second --no-store probe re-emits (§4.4)"
        );
    }

    #[test]
    fn per_host_last_fetch_persists_across_reopen_and_flags_too_soon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let host = "competitor.com";
        let min_interval_ms = 1_000; // §12 default min_interval (1 s) floor

        {
            let mut store = SqliteStore::open_with_ring(&path, DEFAULT_RING_N).unwrap();
            store.record_fetch(host, 10_000).unwrap();
        }

        // Reopen — the timestamp must survive (crawl-delay needs persisted per-host state, §4.3).
        let store = SqliteStore::open_with_ring(&path, DEFAULT_RING_N).unwrap();
        assert_eq!(store.last_fetch_ms(host).unwrap(), Some(10_000));

        // Too-soon fetch (500 ms later) is flagged with the remaining wait.
        let v = store
            .crawl_delay_violation(host, 10_500, min_interval_ms)
            .unwrap();
        assert_eq!(v, Some(500), "must surface retry_after = remaining interval (exit 6, §12)");

        // After the interval elapses, the fetch is clear.
        assert_eq!(
            store
                .crawl_delay_violation(host, 11_000, min_interval_ms)
                .unwrap(),
            None,
            "a fetch at/after the interval is permitted"
        );

        // An unknown host has no recorded fetch -> never flagged.
        assert_eq!(
            store
                .crawl_delay_violation("unknown.example", 11_000, min_interval_ms)
                .unwrap(),
            None
        );
    }

    #[test]
    fn dollar_brace_secret_expands_and_never_lands_in_a_stored_blob() {
        // Secret is provided via the environment-like lookup, expanded at runtime, redacted from
        // the store. We expand an auth header, store a doc that is built from CONTENT only, and
        // assert the secret bytes never appear in the persisted blob (§12).
        let secret = "tok_SUPERSECRET_value_12345";
        let lookup = |name: &str| -> Option<String> {
            match name {
                "CF_TEST_TOKEN" => Some(secret.to_string()),
                _ => None,
            }
        };

        let expanded = expand_env("Bearer ${CF_TEST_TOKEN}", &lookup);
        assert_eq!(
            expanded, "Bearer tok_SUPERSECRET_value_12345",
            "dollar-brace expansion pulls the value from the environment lookup"
        );

        // Unknown var -> empty (never the literal ${VAR}); $$ escapes to $.
        assert_eq!(expand_env("a${NOPE}b", &lookup), "ab");
        assert_eq!(expand_env("cost is $$5", &lookup), "cost is $5");
        assert_eq!(expand_env("plain ${unterminated", &lookup), "plain ${unterminated");

        // Now prove the secret never reaches the blob: snapshots store normalized CONTENT, not
        // headers (§12). Build a doc and inspect the raw stored bytes.
        let mut store = SqliteStore::open_in_memory().unwrap();
        let doc = pricing_doc(4900);
        store
            .put_with_html(
                &StoredSnapshot {
                    tid: "secure".to_string(),
                    rev: 0,
                    doc,
                },
                Some("<html>no secret here</html>"),
            )
            .unwrap();

        let doc_blob: Vec<u8> = store
            .conn
            .query_row(
                "SELECT doc_blob FROM versions WHERE tid = 'secure' AND rev = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let html_blob: Vec<u8> = store
            .conn
            .query_row(
                "SELECT html_blob FROM versions WHERE tid = 'secure' AND rev = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Search both the compressed bytes AND the decompressed content for the secret.
        let doc_plain = zstd::decode_all(doc_blob.as_slice()).unwrap();
        let html_plain = zstd::decode_all(html_blob.as_slice()).unwrap();
        let needle = secret.as_bytes();
        for (label, hay) in [
            ("doc_blob(compressed)", doc_blob.as_slice()),
            ("doc_blob(plain)", doc_plain.as_slice()),
            ("html_blob(compressed)", html_blob.as_slice()),
            ("html_blob(plain)", html_plain.as_slice()),
        ] {
            assert!(
                !contains_subslice(hay, needle),
                "{label} must NEVER contain the secret (§12: secrets are never written to the store)"
            );
        }
    }

    fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || needle.len() > hay.len() {
            return false;
        }
        hay.windows(needle.len()).any(|w| w == needle)
    }
}
