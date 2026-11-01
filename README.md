# big_oil

Path of Exile market index. Two feeds share one SQLite database:

- **public stash stream** — GGG's `GET /public-stash-tabs`, every stash and item,
  with a listing history (listed / price changed / relisted / delisted);
- **your own tabs** — the website's `character-window/get-stash-items`, diffed so
  you can see what you just dropped in, valued against poe.ninja economy data.

It all stays offline-queryable.

```sh
big_oil watch                 # long-running listener, resumes from its cursor
big_oil search --base "Leather Belt" --max-price 5 --currency divine
big_oil item <item-id>        # item detail + its listing history
big_oil stats
```

## Access: an OAuth client is required

Guest access to the Public Stash API was removed with 3.20.0 (28/11/2022). The
endpoint now answers

```
GET /public-stash-tabs
=> 403 Forbidden; You must use an OAuth client to access this endpoint
```

so a request needs a **confidential OAuth client** using the
`client_credentials` grant with the **`service:psapi`** scope. GGG's developer
docs currently state: *"We are currently unable to process new applications."*
If you do not already own such an application, this listener cannot talk to the
API, and nothing on this side can change that — the token exchange is the only
way in (a `POESESSID` cookie is rejected as well). Check
**pathofexile.com → profile → Manage applications** for an existing client.

Create `$XDG_CONFIG_HOME/big_oil/config.json` (or set the environment
variables) with:

```json
{
  "client_id": "...",
  "client_secret": "...",
  "contact": "you@example.com"
}
```

| config key      | env                    |
| --------------- | ---------------------- |
| `client_id`     | `BIG_OIL_CLIENT_ID`    |
| `client_secret` | `BIG_OIL_CLIENT_SECRET`|
| `contact`       | `BIG_OIL_CONTACT`      |
| `db_path`       | `BIG_OIL_DB`           |
| `interval_secs` | `BIG_OIL_INTERVAL`     |

Credentials are never read from this repository. The `User-Agent` is built as
GGG requires: `OAuth <client_id>/<version> (contact: <contact>)`.

```sh
big_oil auth            # exchange credentials for a token (cached, mode 0600)
big_oil auth --check    # also fetch one page to verify the scope
big_oil auth --force    # re-mint after revoking/rotating the secret
```

Client-credentials tokens have no expiry; the cached token is reused until the
API rejects it, at which point `watch` refreshes it once automatically.

## Your own tab (auto-priced drops)

```sh
big_oil ninja fetch --league Allflame          # economy snapshot (no auth)
big_oil stash watch --league Allflame          # poll tabs, record + price drops
big_oil stash list                             # per-tab item counts and value
big_oil search --source own --limit 20         # your items, with a VALUE column
```

`stash watch` reads the tab list once, then each visible tab (every 5 minutes by
default, `--tab-name dump` to scan just one), diffs item ids against what it
recorded (`dropped` / `removed` events), and attaches a poe.ninja estimate. New items are printed as they appear, so leaving it running
next to the game gives you a priced drop feed.

Credentials, only needed for this feature:

| key         | env                  |
| ----------- | -------------------- |
| `poesessid` | `BIG_OIL_POESESSID`  |
| `account`   | `BIG_OIL_ACCOUNT` (`Name#1234`) |

Copy `POESESSID` from a logged-in pathofexile.com tab (DevTools → Application →
Cookies). It is a **full account session**: big_oil sends it only to
pathofexile.com, never logs it, and stores it nowhere except where you put it.
It expires — when passes start failing with 403, copy a fresh one. Note that
this endpoint is the website's own and is not part of GGG's documented API
(their guidelines say not to reverse-engineer endpoints outside the docs); the
documented `account:stashes` alternative needs a registered OAuth app, which is
the same frozen-registration wall as the stream.

### What can be priced

poe.ninja prices item *types*, not listings. Covered: uniques (weapon, armour,
accessory, flask, jewel, relic, tincture), gems (with `--all`), cluster jewels,
maps, base types (with `--all`), currency, fragments, essences, oils, fossil,
scarabs, tattoos, omens and the rest of the currency categories. **Rare armour,
weapons and jewels are not priced anywhere** — they have no per-item data, so
they show `-`.

Matching is name → base type, then corruption, links, gem level/quality and map
tier. When a unique has several variants the item JSON cannot distinguish (e.g.
Mageblood's flask effects), the most widely listed variant wins.

Refresh: poe.ninja serves `cache-control: max-age=1800` and an ETag, and its PoE 1
data updates roughly every 15 minutes, so `stash watch` refreshes every
`--price-interval` seconds (default 1800) using conditional requests. The default
category set is ~3 MB; `--all` adds gems and base types (~6 MB more).

## Underpricing and price decay

The workflow this is built around: dump every drop into a premium/quad tab,
price the whole tab at once (tab prices apply to every item in it), and step the
price down every few hours or days until things move. Or run a ladder of tabs —
5c / 10c / 20c / 50c / 1div — and move items down it. Either way there are two
ways to lose money: an item worth far more than the tab price sells instantly at
the tab price, and a tab nobody touches never gets cheaper.

Because the price is a property of the tab, the report leads with tabs:

```
market values from 2026-11-01T02:22:01Z (3h old)

TABS
TAB   ITEMS  NOPRICE  AT        UNCHANGED  NEXT        VALUE  FLAGS
Dump  13              1 divine  1d         0.8 divine  6136c  2 underpriced

UNDERPRICED — listed below 50% of market
ITEM                               TAB   LISTED    VALUE  MISSING  FOR
Headhunter (Leather Belt)          Dump  1 divine  4542c  4222c    1d
Pearl of Tsoatha (Prismatic Ring)  Dump  1 divine  1514c  1194c    1d
value at risk: 5417c if these sell as listed

2 underpriced (~5417c at risk), 1 tab due a cut
```

```sh
big_oil dump --league Allflame --tab dump            # before pricing a filled tab
big_oil dump --league Allflame --every 6h --cut 20   # soak time before the next cut
big_oil dump --league Allflame --step 10             # ladder: 50c -> 40c
big_oil stash watch --league Allflame --tab-name dump --notify
```

- **TABS** — one row per tab: item count, unpriced count, the price most items
  share, how long the oldest item has been at that price, the next step, total
  market value, and how many outliers sit inside it. A 400-item quad tab is one
  row, not 400.
- **UNDERPRICED** — items listed below `--underpriced` percent of market, worth
  at least `--min-value` chaos, missing at least `--min-loss` chaos. This is the
  list to check before you set a tab price, and the one that catches an item
  whose market moved after you priced the tab.
- **NO PRICE** — valuable items with no note. In a tab-priced tab everything
  inherits the price, so this only shows up in unpriced tabs or manual notes.
- **DUE FOR A CUT** — per-item rows, shown only for mixed tabs (items with
  individual prices); tab-priced tabs are covered by their TABS row.

Stepping down: `--cut` takes a percentage of the current price, `--step` takes a
fixed amount in the tab's own currency (the queue-method ladder: 50c → 40c with
`--step 10`). Suggestions round to whole chaos or one decimal.

Notes are converted to chaos using the currency overview, including short forms
(`divine`, `alch`, `exalt`, `mirror`, `fuse`, `alt`, …).

The decay clock is the item's own price history: the next stash pass sees your
new note as a `price_changed` event, so the reminder clears itself once you
re-price. Moving an item down the ladder resets it only because the new tab's
price changed the note; a move that keeps the same price does not.

Limits to keep in mind: poe.ninja prices types, not instances, so rares (and
uniques with indistinguishable variants) can only ever appear under DUE FOR A
CUT, and a stale snapshot (shown at the top of the report) means stale values —
`stash watch` refreshes every 30 minutes by default.

`--notify` shells out to `notify-send -u critical` for the summary line, from
both `dump` and `stash watch`.

## Triage after a mapping session

The loop this is built for: dump drops unidentified, identify the tab, price the
tab, then pull out the liquid stuff worth more than the tab price and leave the
rest to decay.

```sh
big_oil extract --league Allflame --tab dump
```
```
Dump — 11 items, tab price 1 divine (~320c)

ID FIRST (1) — an unidentified unique could be anything
POS    ITEM          TYPE
(5,5)  Leather Belt  unique

LIQUID (3) — priced by poe.ninja; pull when worth more than the tab
POS    ITEM                                 VALUE  TAB×   LISTINGS  VERDICT
(7,3)  Headhunter (Leather Belt)            4542c  14.2x  477       PULL
(2,0)  Circle of Ambition (Prismatic Ring)  10c    0.03x  23548     stuck
(5,0)  The Eternal Struggle (Onyx Amulet)   5c     0.02x  19608     stuck

1 to pull (4542c), 0 fair, 2 stuck, 7 illiquid, 1 unidentified
```

- **ID FIRST** — unidentified items can't be valued, and an unidentified unique
  can be anything. These are listed before anything else, unique frame first.
- **LIQUID** — items poe.ninja prices (uniques, gems, currency, cards, maps),
  with the stash position so you can find them in the tab. `PULL` at ≥150% of
  the listed price (default), `fair` in between, `stuck` below half.
- **LISTINGS** — supply for the matched market line, a rough liquidity signal
  for "will this sell quickly".
- Rares are counted as illiquid rather than guessed at: nothing offline prices
  an individual rare.

`--min-value` (default 5c) keeps junk out, `--pull` / `--stuck` tune the
verdict thresholds. Positions come from the stash response, so they are the
in-tab coordinates.

## In-game overlay

`big_oil overlay --league Allflame --tab dump` draws a frame around every item
in the tab, on the compositor's **overlay** layer, so it stays visible over a
fullscreen game — niri and Hyprland both render only that layer above
fullscreen windows. The surface has an empty input region, so every click still
lands in the game.

```
green  — worth at least --cheap% (default 150%) of its listed price, or an
         unidentified unique: price it by hand before it sells for the tab price
blue   — belongs in a specialised tab: gems, currency/cards, flasks,
         contracts/blueprints, maps
grey   — settled at this tab's price
```

Geometry comes from **little_oil's calibration**, so it shares the stash grid
you already calibrated there:

```sh
big_oil overlay --league Allflame --tab dump --dry-run   # print frames, draw nothing
big_oil overlay --league Allflame --tab dump --config ~/.config/little_oil/config.json
```

The overlay snapshots the index when it starts; re-run it (bind it to a hotkey
in your compositor) after `stash watch` refreshes. It needs `stash watch` to
have seen that tab at least once, since item positions come from the stash API.

## Build

```sh
nix develop          # or: cargo build --release
cargo test
```

## Commands

```
big_oil [--db PATH] <command>

watch      Stream pages into the database (--once, --interval, --max-pages,
           --realm xbox|sony, --base-url, --min-interval-ms)
ingest     Ingest a saved page: a JSON file, or - for stdin (--no-cursor)
auth       Show/refresh the OAuth token (--force, --check)
search     Query the index (see filters below)
item       One item, its mods and listing history (--json)
history    Just the listing events for an item (--json)
stats      Index summary (--json)
paths      Config, token and database locations
stash      watch | list | value   (your own tabs)
ninja      fetch                  (poe.ninja economy snapshot)
dump       Underpriced items and overdue price cuts (--json, --notify)
extract    Triage a freshly identified tab: what to pull, what is stuck
overlay    In-game frames around items in the open tab (Wayland)
```

Search filters: `--league --name --base --rarity --account --mods --min-ilvl
--max-ilvl --min-price --max-price --currency --min-links --since 6h --source
stream|own --include-delisted --limit --offset --order
newest|oldest|price-asc|price-desc|ilvl-desc --json`.

`--mods` runs a full-text (FTS5) match over implicit/explicit/enchant/crafted/
fractured/utility mod lines.

## How the data is stored

`$XDG_DATA_HOME/big_oil/market.db` (WAL):

| table      | contents |
| ---------- | -------- |
| `meta`     | `next_change_id` cursor, page counter, first/last poll |
| `stashes`  | one row per stash tab: account, name, type, league, public, seen window |
| `items`    | one row per item id: name/base/type/rarity/ilvl/sockets/links, price, mods text, first/last seen, delisted_at, exact JSON in `raw` |
| `listings` | event log: `listed`/`dropped`, `price_changed`, `relisted`/`restored`, `delisted`/`removed` |
| `items_fts`| FTS5 index over name/type/base/mods |
| `price_ref`| latest poe.ninja economy lines per league/category |
| `price_snapshots` | ETag + line count per fetched category |
| `valuations` | every estimate ever attached to an item |

`stashes.source` and `items.source` say where a row came from (`stream` or
`own`); everything else works the same for both.

Because the stream only sends tabs that changed — and each change carries that
tab's full contents — a page is diffed against the last snapshot of each stash:
items that vanish are marked delisted, new item ids are listings, and a changed
`note` (`~b/o 12 chaos`) becomes a `price_changed` event. A `public: false`
entry is the unlist event and closes every open item in that tab.

The cursor is written in the same transaction as the page, so restarting (or
crashing) never skips a page. `ingest --no-cursor` replays a saved page without
disturbing a live listener's position.

Notes:

- The API lags the game by roughly five minutes.
- When `stashes` is empty you are at the end of the stream; `watch` sleeps
  `interval_secs` and re-polls the same cursor.
- Rate limits are read from `X-Rate-Limit-*` headers and pre-emptively honoured;
  429s wait for `Retry-After`, 5xx backs off exponentially.
- Items that move between tabs can produce one `delisted` + `relisted` pair when
  the old tab's snapshot is processed before the new one. Final state is
  correct either way.

## Testing without API access

The test suite runs entirely offline: `tests/fixtures/page_1.json` is a real
page built from live item JSON, and the client tests point at a local mock
server (including 429/`Retry-After` and 403 behaviour).

```sh
big_oil --db /tmp/market.db ingest tests/fixtures/page_1.json
big_oil --db /tmp/market.db search --league Standard --limit 10
```

This product isn't affiliated with or endorsed by Grinding Gear Games in any way.
