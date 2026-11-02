# big_oil

Path of Exile market index. Two feeds, two SQLite files:

- **public stash stream** (`market.db`) — GGG's `GET /public-stash-tabs`, every
  stash and item, with a listing history (listed / price changed / relisted /
  delisted);
- **your own tabs** (`own.db`) — the website's `character-window/get-stash-items`,
  diffed so you can see what you just dropped in, valued against poe.ninja
  economy data (the poe.ninja snapshot lives in this file too).

It all stays offline-queryable.

```sh
big_oil watch --from-head     # long-running listener; later runs resume from the cursor
big_oil search --base "Leather Belt" --max-price 5 --currency divine
big_oil item <item-id>        # item detail + its listing history
big_oil stats
```

## Without an OAuth client: your own tabs

Only the public stream needs an OAuth application. Pricing your own tabs needs a
session cookie instead:

```sh
export BIG_OIL_POESESSID=...            # from a logged-in pathofexile.com tab
export BIG_OIL_ACCOUNT='Name#1234'
export BIG_OIL_CONTACT=you@example.com  # sent in the User-Agent, as poe.ninja and GGG ask
export BIG_OIL_LEAGUE=Allflame

big_oil ninja fetch          # poe.ninja snapshot (no auth)
big_oil stash watch          # read your tabs every 5 minutes, price what appears
big_oil dump                 # per tab: price, time at that price, what is underpriced
big_oil extract --tab 3      # triage one identified tab
big_oil overlay --tab 3      # frames over that tab in game
```

Read [the risk](#your-own-tabs-and-the-terms-of-use) before running
`stash watch`.

## Access: the public stream needs an OAuth client

Guest access to the Public Stash API was removed in 3.20.0. GGG's
[API changelog](https://www.pathofexile.com/developer/docs/changelog) (updated
28/11/2022):

> The Public Stashes endpoint no longer allows guest access. You must now use an
> OAuth client with the `service:psapi` scope.

Without a token the endpoint answers (observed):

```
GET /public-stash-tabs
=> HTTP 401 {"error":{"code":8,"message":"Unauthorized"}}
```

so a request needs a **confidential OAuth client** using the
`client_credentials` grant with the **`service:psapi`** scope. GGG's
[developer docs](https://www.pathofexile.com/developer/docs) currently state:
*"We are currently unable to process new applications."* If you do not already
own such an application, this listener cannot talk to the API, and nothing on
this side can change that — the token exchange is the only way in (a
`POESESSID` cookie is rejected as well). Check **pathofexile.com → profile →
Manage applications** for an existing client.

That client's secret has to stay secret. GGG's docs say executables that run on
a player's machine "must use a public OAuth client if interacting with our
APIs", and that public clients "Cannot use any `service:*` scopes"
([authorization docs](https://www.pathofexile.com/developer/docs/authorization#clients)).
So `big_oil watch` is not something to hand out as a desktop app: run it only
where the client secret is safe (your own machine or server), and never put the
secret in a distributed binary.

Create `$XDG_CONFIG_HOME/big_oil/config.json` (or set the environment
variables) with:

```json
{
  "client_id": "...",
  "client_secret": "...",
  "contact": "you@example.com"
}
```

| config key      | env                     |
| --------------- | ----------------------- |
| `client_id`     | `BIG_OIL_CLIENT_ID`     |
| `client_secret` | `BIG_OIL_CLIENT_SECRET` |
| `contact`       | `BIG_OIL_CONTACT`       |
| `db_path`       | `BIG_OIL_DB`            |
| `own_db_path`   | `BIG_OIL_OWN_DB`        |
| `interval_secs` | `BIG_OIL_INTERVAL`      |

Credentials are never read from this repository. If `config.json` holds a
secret and other users can read it, big_oil warns (`chmod 600` it). The
`User-Agent` is built as GGG requires:
`OAuth <client_id>/<version> (contact: <contact>)`.

```sh
big_oil auth            # exchange credentials for a token (cached, mode 0600)
big_oil auth --check    # also fetch one page to verify the scope
big_oil auth --force    # re-mint after revoking/rotating the secret
```

Client-credentials tokens have no expiry; the cached token is reused until the
API rejects it, at which point `watch` refreshes it once automatically.

## Starting the stream

```sh
big_oil watch --from-head                 # first run: start at the live edge
big_oil watch                             # later runs: resume from the stored cursor
big_oil watch --from-id <next_change_id>  # start from a known page
big_oil cursor show | set <id> | head     # inspect or move the cursor
```

The live edge comes from the trade site's
`https://www.pathofexile.com/api/trade/data/change-ids` (no auth, not part of
the documented API). With no stored cursor and neither flag, `watch` refuses to
start rather than crawl the stream from its very first page.

When something goes wrong mid-stream:

- a page that does not parse is saved to `bad_pages/<change_id>.json` next to
  `market.db` and skipped, using its own `next_change_id` (or the live edge when
  even that cannot be read);
- a cursor the API rejects (HTTP 400) makes `watch` jump to the live edge once;
  the pages in between are skipped;
- a rejected token is refreshed once, keeping the learned rate limits;
- anything else backs off, and ten failures in a row stop the listener.

Console realms: `--realm xbox` or `--realm sony`.

## Your own tab (auto-priced drops)

```sh
big_oil ninja fetch --league Allflame          # economy snapshot (no auth)
big_oil stash watch --league Allflame          # poll tabs, record + price drops
big_oil stash list                             # per-tab index, type, value, time at price
big_oil search --source own --limit 20         # your items, with a VALUE column
```

Every pass, `stash watch` reads the tab list, then each visible tab (all of
them, or the ones named with `--tab`, repeatable), diffs item ids against what
it recorded (`dropped` / `removed` events), and attaches a poe.ninja estimate.
New items are printed as they appear, so leaving it running next to the game
gives you a priced drop feed. Details:

- tab 0's items come with the tab list, so it is not fetched twice;
- a tab that fails is skipped for that pass, the rest still update;
- a tab deleted from the account is closed (its items get `removed` events);
- requests are at least 1.5 s apart, published rate limits are honoured and a
  429 is backed off; `--interval` is at least 30 s (default 300);
- an expired or rejected `POESESSID` stops the watcher (with a desktop
  notification under `--notify`) instead of retrying forever.

Credentials, only needed for this feature:

| key         | env                  |
| ----------- | -------------------- |
| `poesessid` | `BIG_OIL_POESESSID`  |
| `account`   | `BIG_OIL_ACCOUNT` (`Name#1234`) |

Copy `POESESSID` from a logged-in pathofexile.com tab (DevTools → Application →
Cookies). It is a **full account session**: big_oil sends it only to
pathofexile.com (plain-http `--base-url` overrides are refused except for
localhost), never logs it, and stores it nowhere except where you put it. It
expires; when the watcher stops with a session error, copy a fresh one.

### Your own tabs and the Terms of Use

The stash endpoint is the website's own, not part of GGG's documented API. GGG's
[developer docs](https://www.pathofexile.com/developer/docs) say:

> It is against our Terms of Use (section 7i) to reverse-engineer endpoints
> outside of this documentation.

and, for executables that run independently from the game:

> Introducing features against these guidelines or our Terms of Use can result
> in account termination for you *as well as* your users.

So account termination is the stated risk of `stash watch`. The pacing above
limits the load it puts on the site; it does not change the rule.
The documented `account:stashes` alternative needs a registered OAuth app,
which is the same closed registration as the stream.

### What can be priced

poe.ninja prices item *types*, not listings. Covered:

- uniques (weapon, armour, accessory, flask, jewel, relic, tincture); foil
  variants are matched separately from plain uniques;
- gems (with `--all`);
- unique, blighted and blight-ravaged maps;
- the exchange categories: currency, fragments, scarabs, oils, fossils,
  resonators, essences, divination cards, tattoos, omens, delirium orbs,
  astrolabes, …

**Not priced: rares, base types and cluster jewels.** A rare's value is in its
mods, which no type price captures, so they show `-` rather than a guess.

Matching is name first, then base type, corruption, links, gem level/quality
and map tier. When a unique has several variants the item JSON cannot
distinguish (e.g. Mageblood's flask effects), the most widely listed variant
wins.

Notes are converted to chaos with the exchange rates, by trade-site short form
(`divine`, `mirror-shard`, `alch`, …), full name, or a few aliases (`div`,
`ex`, `c`, …). `~b/o 1/10 chaos` prices one unit of a stack, `1,5` is 1.5.
Notes in a currency with no rate are counted in the report, not guessed.

Refresh: poe.ninja serves `cache-control: max-age=1800` and an ETag, and its
PoE 1 data updates roughly every 15 minutes, so `stash watch` refreshes every
`--price-interval` seconds (default 1800) using conditional requests. `--all`
adds gems and base types (several MB more).

## Underpriced items and tab prices

A tab named like a price note — `~price 1 divine`, `~b/o 50 chaos` — prices
every item in it that has no note of its own; an item's own note overrides
the tab. big_oil applies that rule when it records a tab, so every report sees
the price a buyer sees, and `(tab)` marks prices that come from the tab name.

One rule decides "underpriced" everywhere (`dump`, `extract`, `overlay`,
`stash watch`): the market value is at least `--pull` percent of the listed
price (default 150) and at least `--min-value` chaos (default 5). Such an item
sells the moment someone sees it, at the listed price.

```sh
big_oil dump --league Allflame                # every tab
big_oil dump --league Allflame --tab 3        # one tab, by index
big_oil stash watch --league Allflame --notify
```

Example (a synthetic tab from the test mock, priced with live poe.ninja data):

```
market values from 2026-11-02T04:01:39Z (2s old)

TABS
#  TAB              ITEMS  PRICE     AT PRICE  VALUE  SCANNED  FLAGS
0  ~price 1 divine  2      1 divine  2s        4717c  2s       1 underpriced
1  Currency         1      -         -         40c    0s

UNDERPRICED — worth ≥150% of the listed price
ITEM                       TAB              LISTED          VALUE  ×     MISSING  FOR
Headhunter (Leather Belt)  ~price 1 divine  1 divine (tab)  4715c  12.0  4322c    2s

1 underpriced (~4322c at risk)
```

- **TABS** — one row per tab: index, name, item count, the price in its name,
  **how long the tab has sat at that price**, total market value (whole
  stacks), and when it was last scanned. The `at price` clock restarts only
  when the tab is renamed to a different price.
- **UNDERPRICED** — the listed price, market value, the ratio, the chaos missing
  (times the stack) and how long the item has been at that price.

Tabs are picked with `--tab`: a number is the tab index, anything else a
case-insensitive name substring. Priced tabs are renamed every time they are
re-priced, so the index is the handle that stays put; a name that matches
several tabs is an error that lists them.

`stash watch --notify` raises a desktop notification (`notify-send`) only when
something new becomes underpriced in the tabs it just scanned; `dump --notify`
sends the summary line.

Limits: poe.ninja prices types, not instances, and a stale snapshot means stale
values (its age is shown at the top of the report).

## Triage after a mapping session

The loop this is built for: dump drops unidentified, identify the tab, price
the tab, then pull out the liquid stuff worth more than its price.

```sh
big_oil extract --league Allflame --tab 0
```

Example (same synthetic tab):

```
~price 1 divine (tab 0) — 2 items, tab price 1 divine (~393c)

LIQUID (1) — priced by poe.ninja; pull what is worth more than its price
POS    ITEM                       LISTED          VALUE  ×     LISTINGS  VERDICT
(7,3)  Headhunter (Leather Belt)  1 divine (tab)  4715c  12.0  460       PULL

1 to pull (4715c), 0 fair, 0 stuck, 1 illiquid, 0 unidentified
```

- **ID FIRST** — unidentified items can't be valued, and an unidentified unique
  can be anything. These are listed before anything else, unique frame first.
- **LIQUID** — items poe.ninja prices (uniques, gems, currency, cards, maps),
  with the stash position so you can find them in the tab. `PULL` by the shared
  rule above, `stuck` when worth at most half the listed price (`--stuck 200`),
  `fair` in between.
- **LISTINGS** — supply for the matched market line, a rough liquidity signal
  for "will this sell quickly".
- Rares are counted as illiquid rather than guessed at.

## In-game overlay

`big_oil overlay --league Allflame --tab 0` draws a frame around every item in
that tab, on the compositor's **overlay** layer, so it stays visible over a
fullscreen game — niri and Hyprland both render only that layer above
fullscreen windows. The surface has an empty input region, so every click still
lands in the game.

```
green  — underpriced by the shared rule, or an unidentified unique:
         price it by hand before it sells for the tab price
blue   — belongs in a specialised tab: gems, currency/cards, flasks,
         contracts/blueprints, maps
grey   — settled
```

Geometry comes from **little_oil's calibration**: its quad-tab grid (24×24, in
frame space relative to `game_window_region`) and `stash_region` for the
surface. Quad tabs map one to one; normal and premium tabs (12×12) cover two
quad cells per cell. Other tab types (currency, maps, …) have no plain grid and
are refused.

```sh
big_oil overlay --league Allflame --tab 0 --dry-run   # print frames, draw nothing
big_oil overlay --league Allflame --tab 0 --config ~/.config/little_oil/config.json
```

Running `overlay` again closes the one already on screen, so one compositor
hotkey toggles it. It snapshots the index when it starts and warns when the tab
was last scanned more than 15 minutes ago; it needs `stash watch` to have seen
that tab, since item positions come from the stash API.

## Build

```sh
nix develop          # or: cargo build --release
cargo test
```

## Commands

```
big_oil [--db PATH] [--own-db PATH] <command>

watch      Stream pages into market.db (--from-head, --from-id, --once,
           --interval, --max-pages, --realm xbox|sony, --base-url,
           --min-interval-ms)
cursor     show | set <id> | head   (the stream cursor)
ingest     Ingest a saved page: a JSON file, or - for stdin (--no-cursor)
auth       Show/refresh the OAuth token (--force, --check)
search     Query the index (see filters below)
item       One item, its mods and listing history (--source, --json)
history    Just the listing events for an item (--source, --json)
stats      Stream index summary (--json)
paths      Config, token, database and bad-page locations
stash      watch | list | value   (your own tabs)
ninja      fetch                  (poe.ninja economy snapshot)
dump       Underpriced items per tab, and time at price (--tab, --pull,
           --min-value, --json, --notify)
extract    Triage one identified tab: what to pull, what is stuck
overlay    In-game frames around the items in one tab (Wayland)
```

Search filters: `--league --name --base --rarity --account --mods --min-ilvl
--max-ilvl --min-price --max-price --currency --min-links --since 6h --source
stream|own (default stream) --include-delisted --limit --offset --order
newest|oldest|price-asc|price-desc|ilvl-desc --json`. `item` and `history` look
in `own.db` first, then `market.db`, unless `--source` says which.

`--mods` runs a full-text (FTS5) match over implicit/explicit/enchant/crafted/
fractured/utility mod lines only (names and bases have their own filters).

## How the data is stored

Two files with the same schema, under `$XDG_DATA_HOME/big_oil/` (WAL):
`market.db` for the public stream and `own.db` for your tabs and the poe.ninja
snapshot. They never share rows.

| table      | contents |
| ---------- | -------- |
| `meta`     | `next_change_id` cursor, page counter, first/last poll |
| `stashes`  | one row per stash tab: account, name, type, league, tab index, public, seen window, when its name last changed price |
| `items`    | one row per item id: name/base/type/rarity/ilvl/sockets/links, effective price and whether it came from the tab name, when that price was set, mods text, influences/fractured/synthesised/affix counts, first/last seen, delisted_at, item JSON in `raw` |
| `listings` | event log: `listed`/`dropped`, `price_changed`, `relisted`/`restored`, `delisted`/`removed` |
| `items_fts`| FTS5 index over name/type/base/mods |
| `price_ref`| latest poe.ninja economy lines per league/category |
| `price_snapshots` | ETag + line count per fetched category |
| `valuations` | each change in an item's estimate |

Because the stream only sends tabs that changed — and each change carries that
tab's full contents — a page is diffed against the last snapshot of each stash:
items that vanish are marked delisted, new item ids are listings, and a changed
effective price (the item's note, or the tab name when it has none) becomes a
`price_changed` event. A `public: false` entry is the unlist event and closes
every open item in that tab.

The cursor is written in the same transaction as the page, so restarting (or
crashing) never skips a page. `ingest --no-cursor` replays a saved page without
disturbing a live listener's position.

Notes:

- Nothing is pruned yet: both files grow for as long as the feeds run.
- The API lags the game by roughly five minutes.
- When `stashes` is empty you are at the end of the stream; `watch` sleeps
  `interval_secs` and re-polls the same cursor.
- Rate limits are read from `X-Rate-Limit-*` headers and pre-emptively honoured
  (including requests the server counted that this process did not see); 429s
  wait for `Retry-After`, 5xx backs off exponentially.
- Items that move between tabs can produce one `delisted` + `relisted` pair when
  the old tab's snapshot is processed before the new one. Final state is
  correct either way.

## Testing without API access

The test suite runs entirely offline: `tests/fixtures/page_1.json` is a
synthetic page in the live API's shape, and the client tests point at a local
mock server (including 429/`Retry-After`, 400, 403 and malformed pages).

```sh
big_oil --db /tmp/market.db ingest tests/fixtures/page_1.json
big_oil --db /tmp/market.db search --league Standard --limit 10
```

This product isn't affiliated with or endorsed by Grinding Gear Games in any way.
