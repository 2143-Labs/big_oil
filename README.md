# big_oil

Path of Exile public stash market listener. It polls GGG's Public Stash API
(`GET /public-stash-tabs`), keeps every stash and item it sees in SQLite, and
derives a listing history (listed / price changed / relisted / delisted) that you
can query offline.

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
```

Search filters: `--league --name --base --rarity --account --mods --min-ilvl
--max-ilvl --min-price --max-price --currency --min-links --since 6h
--include-delisted --limit --offset --order newest|oldest|price-asc|price-desc|ilvl-desc
--json`.

`--mods` runs a full-text (FTS5) match over implicit/explicit/enchant/crafted/
fractured/utility mod lines.

## How the data is stored

`$XDG_DATA_HOME/big_oil/market.db` (WAL):

| table      | contents |
| ---------- | -------- |
| `meta`     | `next_change_id` cursor, page counter, first/last poll |
| `stashes`  | one row per stash tab: account, name, type, league, public, seen window |
| `items`    | one row per item id: name/base/type/rarity/ilvl/sockets/links, price, mods text, first/last seen, delisted_at, exact JSON in `raw` |
| `listings` | event log: `listed`, `price_changed`, `relisted`, `delisted` |
| `items_fts`| FTS5 index over name/type/base/mods |

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
