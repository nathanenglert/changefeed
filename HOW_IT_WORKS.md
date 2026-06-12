# How changefeed works

A plain-English walkthrough of what happens when `changefeed` (the `cf` binary) looks at a web page. No prior knowledge of the code required.

If you want the deep technical spec, read [`ARCHITECTURE.md`](./ARCHITECTURE.md) and [`DESIGN.md`](./DESIGN.md). This file is the friendly version.

---

## The one-sentence idea

> Instead of re-sending a whole web page to an AI model every time you want to know "did anything change?", `changefeed` re-reads the page, figures out *exactly* what changed all by itself, and hands the agent a tiny note describing the change — or, far more often, just says "nothing happened" for free.

That "for free" part is the whole point. Most of the time a watched page hasn't meaningfully changed, and detecting that costs the agent **a single number check** (an exit code) and zero AI tokens.

### A picture of the problem it solves

The naive way to watch a page:

```
every 15 min:  download whole page  →  send entire page to the LLM  →  "did it change? what changed?"
               (expensive)             (very expensive, every time)     (slow, sometimes wrong)
```

The `changefeed` way:

```
every 15 min:  download page  →  deterministic pipeline figures out the change  →  exit code
               (cheap)           (cheap, no AI involved)                            0 = nothing (stop here, free)
                                                                                    10 = real change → tiny JSON note
```

The pipeline in the middle is **deterministic**: given the same two versions of a page, it always produces the same answer. It uses no AI, no clock, no randomness, and no network of its own. That makes its "did it change?" answer trustworthy and repeatable.

---

## The pipeline at a glance

When `cf check <target>` runs, the page flows through seven steps:

```
  fetch  →  extract  →  normalize  →  segment  →  diff  →  salience  →  event
   (1)        (2)          (3)          (4)        (5)       (6)         (7)

  get the   keep only    strip the    cut into   compare    score how    write the
  page      real         noise that   labeled    against    much the     tiny JSON
  cheaply   content      changes      blocks     last time  change       change note
                         every load                          matters
```

Around that pipeline sit three support pieces: a small local **store** that remembers the last version of each page, a **config file** (`changefeed.toml`) where you list what to watch, and **archetype packs** — pre-tuned rule bundles for common page types like pricing pages or status pages.

### A running example

To make each step concrete, we'll follow one change all the way through:

> A SaaS pricing page. Between yesterday and today, the **Pro Plan price changed from `$49/mo` to `$59/mo`**. Meanwhile the page also has a live "🔴 127 people viewing now" counter that ticks constantly, and a hidden security token that's different on every page load.

The goal: fire a clear event about the **price**, and completely ignore the **counter** and the **token**. Let's watch how each step makes that happen.

---

## Step 1 — Fetch: get the page, as cheaply as possible

**In one sentence:** go download the current version of the page, but avoid doing real work whenever you can.

The fetch step is the only part that touches the network, and it tries hard to be lazy and polite:

- **Ask first, download second.** The first time `cf` sees a page, the server gives it a little "version tag" (an HTTP `ETag`, or a `Last-Modified` date). On the next check, `cf` shows that tag and asks, *"has it changed since this version?"* If the server says **"304 Not Modified"**, `cf` stops immediately — it doesn't download the page, doesn't run any of the other six steps, and reports "no change." This is the cheapest possible outcome: one tiny round-trip.
- **Be a good citizen.** `cf` reads the site's `robots.txt` rules (and remembers them for 24 hours so it doesn't re-fetch that file constantly). If the site says "don't crawl this path," `cf` stops with a clear error. If the site asks crawlers to wait N seconds between visits, `cf` honors that.
- **Guardrails.** Requests time out (30s by default), the page size is capped, and rate-limit responses (HTTP 429) are respected.

**About JavaScript-heavy pages.** Some pages ship almost no real content in their HTML and instead build everything with JavaScript in the browser. `cf` can *detect* this (the downloaded HTML has almost no readable text). In this MVP version, `cf` does **not** run a real browser — so when it detects such a page it stops with a special "needs rendering" signal (exit code 7) rather than diffing an empty shell. If you know a page is fine to read as raw HTML, you set `render = "never"` and `cf` diffs the HTML as-is. A full headless-browser tier is designed but deferred to a later phase.

**In our example:** the server has changed (the price moved), so the conditional request comes back with a fresh `200 OK` and the new HTML. We proceed to step 2.

---

## Step 2 — Extract: keep only the content that matters

**In one sentence:** throw away the page's "furniture" (nav bars, footers, ads, cookie banners) and keep the real content — like a browser's "reader mode."

Raw HTML is full of stuff that isn't the content you care about. Extract removes it in two passes:

1. **Always-remove pass.** Some things are *never* content and get deleted no matter what: `<script>` and `<style>` tags, comments, icons/SVGs, iframes, and anything that looks like an ad, cookie banner, newsletter signup, breadcrumb, or social-share widget (matched by their CSS class names).
2. **Pick the main region.** Then one of three strategies decides what to keep:
   - **Selector** *(best for structured pages)* — you tell `cf` exactly where to look with a CSS selector, e.g. `select = [".PricingTable"]`. It keeps only that part. Precise and predictable.
   - **Readability** *(the default for articles/docs)* — a "reader-mode-style" scoring heuristic. It favors regions with lots of text and few links (that's usually the article) and ignores link-heavy regions (those are usually menus). It keeps the highest-scoring region automatically.
   - **Full** — keep the whole `<body>` minus the always-remove junk.

The output isn't a blob of text; it's a cleaned-up *tree* of elements, ready for the next steps to walk through.

**In our example:** with `select = [".PricingTable"]`, Extract keeps the pricing table and drops the surrounding header, nav, and footer. Notice the "127 viewing now" counter — if it lives *outside* the pricing table, it's already gone here. Good start.

---

## Step 3 — Normalize: strip the noise that changes on every load

**In one sentence:** remove the things that are different on every single page load but don't actually mean anything changed — so two identical pages look truly identical.

This is the heart of `changefeed`'s noise resistance. A page can look wildly different byte-for-byte on two loads while being *the same page*. Normalize erases that churn:

- **Volatile tokens** — security nonces, CSRF tokens, session IDs, and long random-looking strings. (Our hidden per-load security token dies here.)
- **Cache-busters** — URL bits like `?v=12345` or `app.4f3a2b1.js` that rotate on every deploy but point at the same thing.
- **Relative timestamps** — "updated 3 minutes ago", "as of 14:32". These are replaced with a single fixed placeholder (`⟦TS⟧`) so "3 minutes ago" and "5 minutes ago" count as the same.
- **Tracking junk in links** — `utm_*`, `gclid`, `fbclid`, etc. are stripped, and URLs are tidied into one canonical form (resolved, lowercased host, sorted query params, no `#fragment`).
- **Text tidying** — Unicode is put into one standard form (so visually identical characters match), and runs of whitespace are collapsed. (Code blocks are left alone, since whitespace can matter there.)

After all this, `cf` computes a single fingerprint (a hash) of the cleaned page. **If that fingerprint matches last time's, the whole rest of the pipeline is skipped** — no diff, no scoring, exit "no change." That's why a page that only churned its counters and tokens costs essentially nothing.

**In our example:** the random security token is gone, timestamps are flattened, and links are canonicalized. The only thing left that's genuinely different from yesterday is the price text. The fingerprint therefore *doesn't* match — so we keep going. (Had the price not changed, the fingerprint *would* match and we'd stop right here.)

---

## Step 4 — Segment: cut the page into labeled blocks (and give each one three IDs)

**In one sentence:** break the cleaned page into a list of meaningful blocks (a heading, a paragraph, a price, a table row…), and tag each block with three different IDs that serve three different jobs.

To compare today's page against yesterday's, `cf` needs blocks it can line up one-to-one. Segment produces those blocks. The clever bit is that **each block gets three separate identities**, because "which block is this?" and "did this block change?" are different questions:

Think of a **labeled bookshelf**:

| Identity | Bookshelf analogy | What it answers | Key property |
|----------|-------------------|-----------------|--------------|
| **`slot_key`** | the labeled shelf *position* ("Fiction, slot 3") | *Where on the page is this?* | **Ignores the text** — stays the same even when the words change |
| **`block_id`** | the specific book *sitting there today* | *What content is in this spot right now?* | Changes whenever the text changes |
| **`norm_hash`** | "is today's book different from yesterday's?" | *Did this block's content change?* | A quick yes/no fingerprint of the text |

Why does `slot_key` deliberately ignore the text? Because that's what lets `cf` recognize *"this is the same price line as yesterday, it just shows a different number."* If `cf` matched blocks by their text, a changed price would look like "the old price vanished and a brand-new price appeared" — noise instead of a clean "this price changed."

**In our example:** the Pro Plan price line gets a `slot_key` meaning roughly *"Pricing › Pro Plan › price."* Yesterday and today, that `slot_key` is **identical** (same spot on the page). But its `block_id` and `norm_hash` **differ**, because `$49/mo` became `$59/mo`. That mismatch is exactly the signal the diff step looks for.

---

## Step 5 — Diff: compare against last time

This is two sub-steps: first *line up* the blocks (which old block is which new block?), then figure out *what exactly changed* inside the ones that moved or differ.

### 5a — Alignment: match old blocks to new blocks

**In one sentence:** pair up each block from yesterday with its counterpart today, even if things got reworded, reordered, or wrapped in extra layout.

It does this in three increasingly-fuzzy passes:

1. **Exact anchors.** Match blocks that share the same `slot_key` (same spot on the page). Most blocks pair up instantly here — and because `slot_key` ignores text, a block whose words changed still matches its old self.
2. **Keep the stable spine.** Among those matches, find the longest run that's still in the same order (this uses a classic "longest increasing subsequence" trick). Those form a stable backbone; anything that jumped out of order is flagged as **moved/reordered**.
3. **Fuzzy match the leftovers.** Blocks that didn't anchor (a section was renamed, something was genuinely added or removed) are matched by *similarity* — how much their words overlap, how close they are on the page. A fast bucketing technique (locality-sensitive hashing) keeps this from having to compare every old block against every new block, so even a 5,000-block page stays quick.

The result is a clean verdict for every block: **unchanged, modified, added, removed, or moved.**

**In our example:** the Pro Plan price line anchors by `slot_key` in pass 1. The number is different, so it's marked **modified** — not "removed + added." Everything else on the pricing table matches and is **unchanged**.

### 5b — Inside a changed block: what *exactly* changed, and is it real?

**In one sentence:** for each block marked "modified," pinpoint the exact words/values that changed — then decide whether the change is meaningful or just cosmetic.

- **Pinpoint the change.** The block's text is split into tokens (words, numbers, punctuation) and a precise word-level diff finds the minimal edit. Whitespace is used to line things up but never reported. Long unchanged stretches are trimmed to a little context with an `…` so the delta stays small.
- **Score the noise.** Each change gets a "how cosmetic is this?" score from 0 (definitely real) to 1 (almost certainly junk):
  - whitespace-only change → 1.0 (pure noise),
  - a number changing right next to words like "viewing" / "online now" → high noise (it's a live counter),
  - a number changing in a block that's explicitly a **price / stock / date** → **0.0, never noise** (this is a real value).
- **Don't repeat yourself.** Every change gets a fingerprint (a hash of *which* block + its before/after). `cf` remembers recent fingerprints, so observing the same change twice doesn't fire twice.

**In our example:** the word-level diff says simply `$49` → `$59`. Because the block is a **price**, its noise score is 0.0 — this is exactly the case we *want* to fire on. (And if that "127 viewing now" counter had survived this far, *its* numeric change sits next to the word "viewing" and scores as high noise, so it stays quiet.)

---

## Step 6 — Salience: how much does this change matter?

**In one sentence:** for each surviving change, decide how *important* it is, what *kind* of change it is, how *confident* we are, and what an agent should *do* about it.

A change being *real* isn't the same as it being *worth waking someone up for*. Salience is the editor that decides whether a change is front-page news or a footnote. For each change it produces:

- **A salience score (0 to 1)** — overall importance.
- **A materiality band** — the score bucketed into a plain word: **none / low / medium / high / critical**. (Roughly: higher score → higher band. The exact cutoffs are tunable per archetype.)
- **A category** — a human-readable label like `price_increase`, `api_deprecation`, or `content_edit`.
- **A confidence (0 to 1)** — how much to trust this verdict, based on things like how cleanly the page parsed and how well the blocks aligned.
- **A suggested follow-up action** — one item from a fixed short list: `ignore`, `notify`, `re_run_downstream`, `open_ticket`, `escalate_human`, `page_oncall`, and a couple more.

**How the score is computed (the simple version).** `cf` looks at seven independent hints ("signals"): Is this a high-value block type like a price? How *much* text changed? How big is the numeric jump? Did alarming words like "deprecated" appear? Is it in a prominent spot? …and so on. These are combined with a rule that's basically: **"if *any one* signal is strongly worried, the overall score goes up."** It's like asking seven reporters whether something is important — if even one says "yes, very" with conviction, the story runs. Weak hints on their own don't add up to much.

**The gate.** You run `cf check ... --min-salience medium`. If the change's band is **at or above** your threshold, the event fires (exit 10). If it's **below**, `cf` treats it as nothing-to-see-here (exit 0). This is the knob that turns "tell me about everything" into "only wake me for the big stuff."

**In our example:** it's a **price** block (high-value), the number rose ~20%, and it's prominent → the signals push the score to roughly **0.77**, landing in the **high** band, category **`price_increase`**, with a suggested action of **`re_run_downstream`**. With `--min-salience medium`, that clears the bar → the event fires.

---

## Step 7 — Event: the tiny JSON note

**In one sentence:** package the change as one small, self-describing JSON object (~300 tokens) — far cheaper than shipping two whole page copies to an AI.

When a change clears the gate, `cf` prints a compact JSON event to standard output. Here's the example from the README, which is exactly our case:

```json
{
  "v": "1",
  "id": "cfe_01J9Z…",
  "src": { "url": "https://acme.com/pricing", "tid": "acme-pricing" },
  "obs": "2026-06-02T14:30:11Z",
  "base": { "obs": "2026-06-01T14:30:09Z", "snap": "blake3:7ec358f4…", "rev": 4 },
  "seg": [
    { "anchor": "Pro Plan", "fp": "blake3:b33bc20655ac",
      "label_path": "Pricing › Pro Plan › price", "role": "price" }
  ],
  "ct": "modified",
  "delta": { "enc": "val", "a": "$59/mo", "b": "$49/mo" },
  "why": { "sal": 0.77, "mat": "high", "cat": "price_increase",
           "summary": "Pro Plan rose 20.4% ($49/mo→$59/mo)." },
  "followup": { "act": "re_run_downstream" },
  "conf": 0.87,
  "prov": { "m": "http", "hash": "blake3:a68240e8…", "status": 200, "pack": "pricing@b3:39a6" }
}
```

Reading it field by field, in plain terms:

| Field | What it tells you |
|-------|-------------------|
| `v` | Schema version — always `"1"`. The format is **frozen**, so agent code built on it never breaks. |
| `id` | A unique ID for this event (`cfe_…`). |
| `src` | Where it came from: the `url` and the short target id (`tid`). |
| `obs` | When this observation happened (the timestamp is added by the CLI — the core has no clock). |
| `base` | What we compared against: the previous observation's time, snapshot hash, and revision number. |
| `seg` | Which block(s) changed — with a human-readable `label_path` ("Pricing › Pro Plan › price") and its `role` ("price"). |
| `ct` | The change type: `added` / `removed` / `modified` / `reordered` / `restyled`. |
| `delta` | The actual before/after. Here `enc: "val"` means a simple value change. |
| `why` | **Why it matters**: salience score (`sal`), materiality band (`mat`), category (`cat`), and a one-line human `summary`. |
| `followup` | The suggested action (`act`). |
| `conf` | Confidence that this is a real, meaningful change. |
| `prov` | Provenance: how it was fetched (`m`), the raw-HTML hash, HTTP status, and which rule pack was used. |

A nice detail: **optional fields are simply left out** when they don't apply (never sent as `null`). A field being present *means something*. This keeps events small and self-describing. The full machine-readable schema is published — run `cf schema` to print it.

---

## How an agent actually uses this: exit codes

This is the payoff. An agent polls in a loop and branches on the **exit code** — usually without reading standard output at all.

```bash
while sleep 900; do
  out=$(cf check acme-pricing --min-salience medium --format jsonl)
  case $? in
    0)  : ;;                                              # no change — empty output, do nothing (free)
    10) printf '%s\n' "$out" | your-agent ingest-change ;;# a real change — read it and act
    3)  sleep 60 ;;                                       # transient fetch error — back off
    6)  sleep "$(printf '%s' "$out" | jq -r '.crawl.retry_after // 60')" ;;  # rate-limited
  esac
done
```

The exit codes are a **frozen contract** — they won't change across the v1 lifetime, so agent loops keep working forever:

| Code | Meaning | What the agent does |
|-----:|---------|---------------------|
| **0**  | No material change | Nothing — output is empty. This is the cheap branch. |
| **10** | A change at/above your threshold | Read the JSON event and act. (The only time you parse output.) |
| **11** | First time seeing this page — a baseline was saved | Note it, keep polling. There was nothing to compare against yet. |
| **12** | A change happened, but *below* your threshold | Usually ignore. (Only emitted if you ask with `--emit-subthreshold`.) |
| **1**  | Bad usage / config | Fix the command or config; don't retry. |
| **2**  | Unknown target id | Fix the config or use a real URL. |
| **3**  | Temporary fetch failure (timeout, 5xx, TLS) | Back off and retry. |
| **4**  | Blocked by `robots.txt` | Don't retry without a policy change. |
| **5**  | Auth failed (401/403) | Refresh credentials, retry. |
| **6**  | Rate-limited (429 / crawl-delay) | Read `retry_after` and sleep that long. |
| **7**  | Page needs JavaScript rendering (no browser in MVP) | Render it elsewhere, or set `render = "never"`. |

The mental model: **0 and 10 are the two everyday outcomes**, 11/12 are setup/below-threshold, and 1–7 are problems to handle.

---

## The support cast: memory, config, and packs

The seven steps are a pure function — same inputs, same output. Three pieces around them make it usable day to day.

### What `cf` remembers — the local store

To diff "today vs. yesterday," `cf` has to *remember* yesterday. It keeps a small local database at `.changefeed/store.db` (SQLite) holding, per watched target:

- a **compressed snapshot** of the last cleaned-up page (so it can diff the next fetch against it),
- a **revision number** that ticks up only when the content actually changes,
- a rolling **"seen" list** of recent change-fingerprints (so the same change isn't reported twice),
- the last fetch time per host (for polite crawl-delays).

It only keeps the last handful of snapshots per target (8 by default) and quietly drops older ones. And when a fetch turns out identical to last time, it skips writing entirely — nothing changed, nothing to store.

### What to watch — `changefeed.toml`

A plain config file (no code) with two layers:

```toml
[defaults]                     # apply to everything
schedule = "15m"
render = "auto"
min_salience = "low"

[[target]]                     # one block per watched page
id = "acme-pricing"
url = "https://acme.com/pricing"
archetype = "pricing"          # which tuned rule pack to use
select = [".PricingTable"]     # focus extraction here
ignore = [".live-counter", { attr = "data-csrf-nonce" }]   # extra noise to drop
```

Per-target settings win over `[defaults]`. (`cf watch <url>` appends a target block for you; `cf init` scaffolds the file.)

### How changes are scored — archetype packs

Different page types care about different things. A **pack** is a reusable, pre-tuned bundle of scoring rules. The MVP ships four (all plain TOML, no code):

- **`default`** — the base layer, applied to everything.
- **`pricing`** — a bare `$49 → $59` move lands at **high**; "no longer available" escalates.
- **`status-page`** — words like "degraded" or "outage" trigger **`page_oncall`**.
- **`api-docs`** — "breaking change" or "rate limit" force **`open_ticket`**.

When a target says `archetype = "pricing"`, `cf` layers `default → pricing` (the specific pack wins on conflicts) so common page types behave sensibly out of the box, with no hand-tuning.

---

## Putting it all together

Back to our pricing page, top to bottom:

1. **Fetch** — conditional request comes back `200` with new HTML (the page really changed).
2. **Extract** — keep the `.PricingTable`; drop nav, footer, and the off-table visitor counter.
3. **Normalize** — strip the per-load security token, flatten timestamps, tidy URLs. Fingerprint differs from yesterday → keep going.
4. **Segment** — the Pro Plan price line keeps the **same `slot_key`** as yesterday but a **new `norm_hash`** (its text changed).
5. **Diff** — align by `slot_key` → marked **modified**; word-level diff says `$49 → $59`; it's a price block, so noise score is **0.0** (definitely real).
6. **Salience** — price + ~20% jump + prominent → score ~**0.77**, band **high**, category **`price_increase`**, action **`re_run_downstream`**. Clears `--min-salience medium`.
7. **Event** — emit the ~300-token JSON note and **exit 10**. The agent reads it and reruns its downstream work.

Meanwhile the "127 viewing now" counter and the rotating security token never made it past steps 2–5 — exactly as intended. And on every poll where *nothing* meaningful changed, the page never even gets past the fingerprint check: **exit 0, empty output, zero tokens.**

That's `changefeed`: a deterministic machine that turns "watch this page" into a cheap yes/no, and only spends real effort — yours or the model's — when something actually mattered.

---

## Mini-glossary

- **Deterministic** — same inputs always give the same output. No AI, clock, or randomness in the core.
- **`slot_key` / `block_id` / `norm_hash`** — a block's *location* (survives text edits), its *current content* (changes with the text), and a quick *did-it-change?* fingerprint.
- **Materiality band** — the salience score bucketed into a word: none / low / medium / high / critical.
- **Noisy-OR** — the "any one strong signal raises the score" rule used to combine the seven salience signals.
- **Archetype pack** — a pre-tuned bundle of scoring rules for a page type (pricing, status-page, api-docs).
- **Conditional request / 304** — asking the server "changed since my version?" and getting "no" for almost free.
- **Exit code** — the number `cf` returns; the agent branches on it (0 = nothing, 10 = change) without parsing output.
