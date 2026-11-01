//! big_oil — Path of Exile public stash market listener.
//!
//! Streams `GET /public-stash-tabs`, stores every stash and item, derives
//! listing events (listed / price change / relist / delist), and lets you query
//! the resulting history offline.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use big_oil::auth;
use big_oil::client::{self, Client};
use big_oil::config::Config;
use big_oil::dump::{self, Settings as DumpSettings};
use big_oil::model::StashPage;
use big_oil::ninja::{self, NinjaClient, Overview};
use big_oil::own::OwnClient;
use big_oil::rates::Rates;
use big_oil::render;
use big_oil::store::{self, Order, SearchQuery, Store};

#[derive(Parser)]
#[command(
    name = "big_oil",
    version,
    about = "Path of Exile public stash market listener and item index",
    long_about = None
)]
struct Cli {
    /// Database file (default: $XDG_DATA_HOME/big_oil/market.db)
    #[arg(long, global = true, value_name = "PATH", env = "BIG_OIL_DB")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

// A parsed CLI is created once and immediately destructured; boxing variants
// would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Stream public stash pages into the database (long running)
    Watch(Watch),
    /// Ingest a saved page: a JSON file, or - for stdin
    Ingest(Ingest),
    /// Show or refresh the OAuth token
    Auth(Auth),
    /// Search indexed items
    Search(Search),
    /// Show one item and its listing history
    Item {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Print the listing history of one item
    History {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Index summary
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Your own stash tabs (needs POESESSID)
    Stash {
        #[command(subcommand)]
        cmd: StashCommand,
    },
    /// poe.ninja economy data (no auth)
    Ninja {
        #[command(subcommand)]
        cmd: NinjaCommand,
    },
    /// Underpriced items and overdue price cuts
    Dump(DumpArgs),
    /// Print config, token and database paths
    Paths,
}

#[derive(clap::Args)]
struct Watch {
    /// Fetch a single page and exit
    #[arg(long)]
    once: bool,
    /// Seconds to wait when the stream is drained
    #[arg(long)]
    interval: Option<u64>,
    /// Stop after N ingested pages
    #[arg(long)]
    max_pages: Option<usize>,
    /// xbox or sony (default: PC)
    #[arg(long)]
    realm: Option<String>,
    /// Override the endpoint (for tests/proxies)
    #[arg(long)]
    base_url: Option<String>,
    /// Minimum milliseconds between requests
    #[arg(long, default_value_t = 500)]
    min_interval_ms: u64,
}

#[derive(clap::Args)]
struct Ingest {
    /// JSON file, or - for stdin
    file: String,
    /// Do not move the stream cursor (safe for replayed fixtures)
    #[arg(long)]
    no_cursor: bool,
}

#[derive(clap::Args)]
struct Auth {
    /// Fetch a new token even if a cached one exists
    #[arg(long)]
    force: bool,
    /// Verify the token against the Public Stash API
    #[arg(long)]
    check: bool,
}

#[derive(Subcommand)]
enum StashCommand {
    /// Poll your tabs, record what you drop, and price it
    Watch(StashWatch),
    /// Summarise the tabs scanned so far
    List,
    /// Re-value own-tab items from the stored economy snapshot
    Value {
        #[arg(long, env = "BIG_OIL_LEAGUE")]
        league: String,
    },
}

#[derive(clap::Args)]
struct StashWatch {
    #[arg(long, env = "BIG_OIL_LEAGUE")]
    league: String,
    /// One pass, then exit
    #[arg(long)]
    once: bool,
    /// Seconds between passes
    #[arg(long, default_value_t = 120)]
    interval: u64,
    /// Comma-separated tab indexes to scan (default: every visible tab)
    #[arg(long)]
    tabs: Option<String>,
    /// Refresh poe.ninja prices before the first pass
    #[arg(long)]
    refresh_prices: bool,
    /// Price against every ninja category (adds gems and base types)
    #[arg(long)]
    all: bool,
    /// Refresh prices when older than this many seconds
    #[arg(long, default_value_t = 1800)]
    price_interval: u64,
    /// Override the endpoint (for tests/proxies)
    #[arg(long)]
    base_url: Option<String>,
    /// Desktop notification when something needs attention
    #[arg(long)]
    notify: bool,
}

#[derive(Subcommand)]
enum NinjaCommand {
    /// Download economy overviews into the database
    Fetch {
        #[arg(long, env = "BIG_OIL_LEAGUE")]
        league: String,
        /// Comma-separated item-overview categories (overrides the default set)
        #[arg(long)]
        types: Option<String>,
        /// Every category, including gems and base types
        #[arg(long)]
        all: bool,
        /// Skip the currency-overview categories
        #[arg(long)]
        no_currency: bool,
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(clap::Args)]
struct DumpArgs {
    #[arg(long, env = "BIG_OIL_LEAGUE")]
    league: String,
    /// Tab name substring (e.g. your dump tab)
    #[arg(long)]
    tab: Option<String>,
    /// Flag items listed below this percent of market value
    #[arg(long, default_value_t = 50.0)]
    underpriced: f64,
    /// Ignore items worth less than this many chaos
    #[arg(long, default_value_t = 20.0)]
    min_value: f64,
    /// Ignore gaps worth less than this many chaos
    #[arg(long, default_value_t = 10.0)]
    min_loss: f64,
    /// Suggest a cut after this long at the same price
    #[arg(long, default_value = "12h")]
    every: String,
    /// Cut size in percent
    #[arg(long, default_value_t = 20.0)]
    cut: f64,
    #[arg(long)]
    json: bool,
    /// Desktop notification with the summary
    #[arg(long)]
    notify: bool,
}

#[derive(clap::Args)]
struct Search {
    #[arg(long)]
    league: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    base: Option<String>,
    #[arg(long)]
    rarity: Option<String>,
    #[arg(long)]
    account: Option<String>,
    /// Full-text match against item mods
    #[arg(long)]
    mods: Option<String>,
    #[arg(long)]
    min_ilvl: Option<i64>,
    #[arg(long)]
    max_ilvl: Option<i64>,
    #[arg(long)]
    min_price: Option<f64>,
    #[arg(long)]
    max_price: Option<f64>,
    /// Price currency, e.g. chaos or divine
    #[arg(long)]
    currency: Option<String>,
    #[arg(long)]
    min_links: Option<i64>,
    /// Only items first seen within this window (e.g. 30m, 6h, 7d)
    #[arg(long)]
    since: Option<String>,
    /// Only own-stash items or only stream listings (stream|own)
    #[arg(long)]
    source: Option<String>,
    /// Include sold/unlisted items
    #[arg(long)]
    include_delisted: bool,
    #[arg(long, default_value = "25")]
    limit: usize,
    #[arg(long, default_value = "0")]
    offset: usize,
    /// newest | oldest | price-asc | price-desc | ilvl-desc
    #[arg(long, default_value = "newest")]
    order: String,
    #[arg(long)]
    json: bool,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();
    let cfg = Config::load(cli.db)?;

    match cli.command {
        Command::Watch(args) => watch(&cfg, args),
        Command::Ingest(args) => ingest(&cfg, args),
        Command::Auth(args) => run_auth(&cfg, args),
        Command::Search(args) => search(&cfg, args),
        Command::Item { id, json } => show_item(&cfg, &id, json),
        Command::History { id, json } => show_history(&cfg, &id, json),
        Command::Stats { json } => show_stats(&cfg, json),
        Command::Stash { cmd } => match cmd {
            StashCommand::Watch(args) => stash_watch(&cfg, args),
            StashCommand::List => stash_list(&cfg),
            StashCommand::Value { league } => stash_value(&cfg, &league),
        },
        Command::Ninja { cmd } => match cmd {
            NinjaCommand::Fetch {
                league,
                types,
                all,
                no_currency,
                base_url,
            } => ninja_fetch_cmd(&cfg, &league, types, all, no_currency, base_url),
        },
        Command::Dump(args) => run_dump(&cfg, args),
        Command::Paths => {
            for (name, path) in cfg.layout() {
                println!("{name:<9} {path}");
            }
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn open_store(cfg: &Config) -> Result<Store> {
    Store::open(&cfg.db_path)
}

fn load_token(cfg: &Config, force: bool) -> Result<auth::Token> {
    let (client_id, client_secret) = cfg.credentials()?;
    let contact = cfg.contact()?;
    let path = cfg.token_path();
    let now = store::unix_now();
    if !force && let Some(token) = auth::load_cached(&path, client_id, now) {
        return Ok(token);
    }
    tracing::info!("requesting OAuth token for client {client_id}");
    let token = auth::request_token(client_id, client_secret, contact)?;
    auth::save_cached(&path, &token)?;
    Ok(token)
}

fn make_client(cfg: &Config, token: &auth::Token, base_url: Option<String>) -> Result<Client> {
    let (client_id, _) = cfg.credentials()?;
    let contact = cfg.contact()?;
    Ok(Client::new(
        base_url.unwrap_or_else(|| client::DEFAULT_BASE.to_string()),
        token.access_token.clone(),
        contact,
        client_id,
    ))
}

fn watch(cfg: &Config, args: Watch) -> Result<()> {
    let mut store = open_store(cfg)?;
    let token = load_token(cfg, false)?;
    let mut client = make_client(cfg, &token, args.base_url.clone())?
        .with_min_interval(Duration::from_millis(args.min_interval_ms));
    let interval = Duration::from_secs(args.interval.unwrap_or(cfg.interval_secs));
    let mut pages = 0usize;
    let mut consecutive_errors = 0u32;
    let mut refreshed_token = false;
    let mut reset_cursor = false;

    loop {
        let cursor = store.next_change_id()?;
        match client.fetch(cursor.as_deref(), args.realm.as_deref()) {
            Ok(page) => {
                consecutive_errors = 0;
                refreshed_token = false;
                reset_cursor = false;
                let empty = page.stashes.is_empty();
                let stats = store.ingest_page(&page, true)?;
                pages += 1;

                tracing::info!(
                    page = pages,
                    change_id = %short(&page.next_change_id),
                    stashes = stats.stashes,
                    new_items = stats.items_new,
                    price_changes = stats.price_changes,
                    relisted = stats.relisted,
                    delisted = stats.delisted,
                    unlisted = stats.unlisted_stashes,
                    "ingested"
                );

                if args.once || args.max_pages.is_some_and(|max| pages >= max) {
                    println!(
                        "changes={} items_new={} price_changes={} delisted={}",
                        stats.stashes, stats.items_new, stats.price_changes, stats.delisted
                    );
                    return Ok(());
                }
                if empty {
                    // End of the stream: the endpoint returns the same cursor
                    // until new stashes appear.
                    std::thread::sleep(interval);
                }
            }
            Err(e) => {
                let text = e.to_string();
                if text.contains("HTTP 400") && !reset_cursor {
                    // A cursor the API no longer recognises: start over from the
                    // head rather than dying. The gap is logged.
                    tracing::warn!("cursor rejected by the API, restarting from the stream head");
                    reset_cursor = true;
                    store.clear_cursor()?;
                    continue;
                }
                let auth_problem = text.contains("401") || text.contains("403");
                if auth_problem && !refreshed_token {
                    tracing::warn!("token rejected, refreshing and retrying once");
                    refreshed_token = true;
                    let token = load_token(cfg, true)?;
                    client = make_client(cfg, &token, args.base_url.clone())?;
                    continue;
                }
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    return Err(e).context("too many consecutive failures; giving up");
                }
                let backoff = Duration::from_secs(5 * u64::from(consecutive_errors.min(6)));
                tracing::warn!(error = %e, ?backoff, "fetch failed, retrying");
                std::thread::sleep(backoff);
            }
        }
    }
}

fn short(change_id: &str) -> String {
    if change_id.len() > 24 {
        format!("{}…", &change_id[..24])
    } else {
        change_id.to_string()
    }
}

fn ingest(cfg: &Config, args: Ingest) -> Result<()> {
    let mut store = open_store(cfg)?;
    let bytes = if args.file == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        std::fs::read(&args.file).with_context(|| format!("reading {}", args.file))?
    };
    let page = StashPage::parse(&bytes)?;
    let change_id = page.next_change_id.clone();
    let stashes = page.stashes.len();
    let stats = store.ingest_page(&page, !args.no_cursor)?;
    println!(
        "change_id={} stashes={} new_items={} price_changes={} relisted={} delisted={} unlisted={}",
        short(&change_id),
        stashes,
        stats.items_new,
        stats.price_changes,
        stats.relisted,
        stats.delisted,
        stats.unlisted_stashes
    );
    Ok(())
}

fn run_auth(cfg: &Config, args: Auth) -> Result<()> {
    let token = load_token(cfg, args.force)?;
    println!("{}", token.describe());
    if args.check {
        let mut client = make_client(cfg, &token, None)?;
        let page = client.fetch(None, None)?;
        println!(
            "ok: change_id={} stashes={}",
            short(&page.next_change_id),
            page.stashes.len()
        );
    }
    Ok(())
}

fn search(cfg: &Config, args: Search) -> Result<()> {
    let store = open_store(cfg)?;
    let since = match &args.since {
        Some(raw) => Some(
            render::parse_since(raw)
                .with_context(|| format!("--since {raw:?} is not a duration like 30m/6h/7d"))?,
        ),
        None => None,
    };
    let order = match args.order.as_str() {
        "newest" => Order::Newest,
        "oldest" => Order::Oldest,
        "price-asc" => Order::PriceAsc,
        "price-desc" => Order::PriceDesc,
        "ilvl-desc" => Order::IlvlDesc,
        other => bail!("unknown --order {other:?} (newest|oldest|price-asc|price-desc|ilvl-desc)"),
    };
    let query = SearchQuery {
        league: args.league,
        name: args.name,
        base: args.base,
        rarity: args.rarity,
        account: args.account,
        mods: args.mods,
        min_ilvl: args.min_ilvl,
        max_ilvl: args.max_ilvl,
        min_price: args.min_price,
        max_price: args.max_price,
        currency: args.currency,
        min_links: args.min_links,
        since,
        source: args.source,
        include_delisted: args.include_delisted,
        limit: args.limit,
        offset: args.offset,
        order,
    };
    let rows = store.search(&query)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no matching items");
        return Ok(());
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.league.clone().unwrap_or_default(),
                r.display_name(),
                r.ilvl.map(|v| v.to_string()).unwrap_or_default(),
                r.price().unwrap_or_else(|| "-".into()),
                value_cell(r),
                render::age(r.first_seen),
                r.delisted_at
                    .map(|_| "delisted")
                    .unwrap_or("live")
                    .to_string(),
                r.stash_name.clone().unwrap_or_default(),
                r.account.clone().unwrap_or_default(),
            ]
        })
        .collect();
    print!(
        "{}",
        render::table(
            &[
                "league", "item", "ilvl", "price", "value", "age", "status", "stash", "account",
            ],
            &table,
            34
        )
    );
    Ok(())
}

fn show_item(cfg: &Config, id: &str, json: bool) -> Result<()> {
    let store = open_store(cfg)?;
    let Some(item) = store.item(id)? else {
        bail!("no item with id {id}");
    };
    let history = store.history(id)?;
    let valuations = store.valuations(id)?;
    let (mods_text, raw) = store.item_extra(id)?.unwrap_or_default();

    if json {
        let value = serde_json::json!({
            "item": item,
            "history": history,
            "valuations": valuations,
            "mods": mods_text,
            "raw": serde_json::from_str::<serde_json::Value>(&raw).ok(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("{}", item.display_name());
    println!("  id        {}", item.id);
    println!(
        "  league    {}",
        item.league.clone().unwrap_or_else(|| "?".into())
    );
    println!(
        "  stash     {} ({})",
        item.stash_name.clone().unwrap_or_else(|| "?".into()),
        item.account.clone().unwrap_or_else(|| "?".into())
    );
    println!(
        "  rarity    {} | ilvl {} | sockets {} | links {} {}",
        item.rarity.clone().unwrap_or_else(|| "?".into()),
        item.ilvl
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        item.sockets.clone().unwrap_or_else(|| "-".into()),
        item.links
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        if item.corrupted { "| corrupted" } else { "" }
    );
    println!(
        "  price     {} {}",
        item.price().unwrap_or_else(|| "-".into()),
        item.note
            .as_deref()
            .map(|n| format!("note={n:?}"))
            .unwrap_or_default()
    );
    if item.value_chaos.is_some() {
        println!(
            "  value     {} (poe.ninja, {})",
            value_cell(&item),
            item.valued_at.map(render::ts).unwrap_or_default()
        );
    }
    println!(
        "  seen      first {} ({}) | last {} ({}){}",
        render::ts(item.first_seen),
        render::age(item.first_seen),
        render::ts(item.last_seen),
        render::age(item.last_seen),
        item.delisted_at
            .map(|t| format!(" | delisted {}", render::ts(t)))
            .unwrap_or_default()
    );

    if !mods_text.trim().is_empty() {
        println!("  mods:");
        for line in mods_text.lines().filter(|l| !l.trim().is_empty()) {
            println!("    {line}");
        }
    }
    if valuations.len() > 1 {
        println!("  valuations:");
        for v in &valuations {
            println!(
                "    {}  {} chaos{} ({})",
                render::ts(v.at),
                v.chaos.map(trim_num).unwrap_or_else(|| "?".into()),
                v.divine
                    .map(|d| format!(" / {} divine", trim_num(d)))
                    .unwrap_or_default(),
                v.matched.clone().unwrap_or_default()
            );
        }
    }
    if !history.is_empty() {
        println!("  history:");
        for h in &history {
            let price = h
                .price_amount
                .map(|a| format!("{a} {}", h.price_currency.clone().unwrap_or_default()))
                .unwrap_or_else(|| "-".into());
            let old = h
                .old_amount
                .map(|a| format!("{a} {}", h.old_currency.clone().unwrap_or_default()))
                .unwrap_or_else(|| "-".into());
            println!(
                "    {}  {:<13} {} (was {})",
                render::ts(h.at),
                h.event,
                price,
                old
            );
        }
    }
    Ok(())
}

fn show_history(cfg: &Config, id: &str, json: bool) -> Result<()> {
    let store = open_store(cfg)?;
    if store.item(id)?.is_none() {
        bail!("no item with id {id}");
    }
    let history = store.history(id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&history)?);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = history
        .iter()
        .map(|h| {
            vec![
                render::ts(h.at),
                h.event.clone(),
                h.price_amount
                    .map(|a| format!("{a} {}", h.price_currency.clone().unwrap_or_default()))
                    .unwrap_or_else(|| "-".into()),
                h.old_amount
                    .map(|a| format!("{a} {}", h.old_currency.clone().unwrap_or_default()))
                    .unwrap_or_else(|| "-".into()),
                h.stash_id.clone().unwrap_or_default(),
            ]
        })
        .collect();
    print!(
        "{}",
        render::table(&["at", "event", "price", "was", "stash"], &rows, 40)
    );
    Ok(())
}

fn show_stats(cfg: &Config, json: bool) -> Result<()> {
    let store = open_store(cfg)?;
    let stats = store.stats()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }
    println!("database  {}", cfg.db_path.display());
    println!(
        "cursor    {}",
        stats
            .next_change_id
            .as_deref()
            .map(short)
            .unwrap_or_else(|| "-".into())
    );
    println!("pages     {}", stats.pages);
    println!(
        "last poll {}",
        stats
            .last_poll
            .map(|t| format!("{} ({})", render::ts(t), render::age(t)))
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "first     {}",
        stats
            .first_index
            .map(render::ts)
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "stashes   {} ({} public) | items {} ({} live, {} priced) | events {}",
        stats.stashes,
        stats.active_stashes,
        stats.items,
        stats.active_items,
        stats.priced_items,
        stats.events
    );
    if !stats.leagues.is_empty() {
        println!();
        let rows: Vec<Vec<String>> = stats
            .leagues
            .iter()
            .map(|l| {
                vec![
                    l.league.clone(),
                    l.active_items.to_string(),
                    l.priced_items.to_string(),
                ]
            })
            .collect();
        print!(
            "{}",
            render::table(&["league", "live items", "priced"], &rows, 32)
        );
    }
    Ok(())
}

fn contact(cfg: &Config) -> String {
    cfg.contact.clone().unwrap_or_else(|| "unset".to_string())
}

fn trim_num(v: f64) -> String {
    if (v.fract()).abs() < f64::EPSILON {
        format!("{}", v as i64)
    } else if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

fn value_cell(row: &store::ItemRow) -> String {
    match (row.value_chaos, row.value_divine) {
        (Some(c), Some(d)) => format!("{}c / {}d", trim_num(c), trim_num(d)),
        (Some(c), None) => format!("{}c", trim_num(c)),
        _ => "-".into(),
    }
}

fn parse_tab_filter(raw: &Option<String>) -> Option<Vec<i64>> {
    raw.as_ref().map(|spec| {
        spec.split(',')
            .filter_map(|p| p.trim().parse::<i64>().ok())
            .collect()
    })
}

fn selected_types(all: bool) -> Vec<String> {
    if all {
        ninja::all_item_types()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        ninja::DEFAULT_ITEM_TYPES
            .iter()
            .map(|t| (*t).to_string())
            .collect()
    }
}

/// Download the economy overviews for one league. Returns (lines stored,
/// categories changed).
fn fetch_prices(
    cfg: &Config,
    store: &mut Store,
    league: &str,
    item_types: &[String],
    include_currency: bool,
    base_url: Option<String>,
) -> Result<(usize, usize)> {
    let client = NinjaClient::with_base(
        base_url.unwrap_or_else(|| ninja::BASE.to_string()),
        &contact(cfg),
    );
    let mut stored = 0usize;
    let mut categories = 0usize;

    for category in item_types {
        let etag = store.price_etag(league, category)?;
        match client.item_overview(league, category, etag.as_deref())? {
            Overview::NotModified => tracing::debug!(category, "unchanged"),
            Overview::Lines { lines, etag } => {
                let n = store.upsert_price_lines(league, category, etag.as_deref(), &lines)?;
                stored += n;
                categories += 1;
                tracing::debug!(category, lines = n, "stored");
            }
        }
    }

    if include_currency {
        for category in ninja::POE1_CURRENCY_TYPES {
            let etag = store.price_etag(league, category)?;
            match client.currency_overview(league, category, etag.as_deref())? {
                Overview::NotModified => {}
                Overview::Lines { lines, etag } => {
                    let n = store.upsert_price_lines(league, category, etag.as_deref(), &lines)?;
                    stored += n;
                    categories += 1;
                    tracing::debug!(category, lines = n, "stored");
                }
            }
        }
    }

    Ok((stored, categories))
}

fn ninja_fetch_cmd(
    cfg: &Config,
    league: &str,
    types: Option<String>,
    all: bool,
    no_currency: bool,
    base_url: Option<String>,
) -> Result<()> {
    let mut store = open_store(cfg)?;
    let item_types: Vec<String> = match types {
        Some(spec) => spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        None => selected_types(all),
    };
    let (stored, categories) =
        fetch_prices(cfg, &mut store, league, &item_types, !no_currency, base_url)?;
    println!("{league}: {stored} price lines across {categories} changed categories");
    Ok(())
}

fn stash_watch(cfg: &Config, args: StashWatch) -> Result<()> {
    let cookie = cfg.poesessid.clone().with_context(|| {
        "no POESESSID configured.\n\
         The stash endpoints are the website's own, so they need a logged-in session:\n\
         pathofexile.com -> DevTools -> Application -> Cookies -> copy POESESSID,\n\
         then set BIG_OIL_POESESSID or put \"poesessid\" in the config file."
    })?;
    let account = cfg.account.clone().with_context(|| {
        "no account configured: set BIG_OIL_ACCOUNT or \"account\" in the config file (Name#1234)"
    })?;

    let mut store = open_store(cfg)?;
    let client = OwnClient::with_base(
        args.base_url
            .clone()
            .unwrap_or_else(|| big_oil::own::BASE.to_string()),
        cookie,
        &contact(cfg),
    );
    let filter = parse_tab_filter(&args.tabs);
    let item_types = selected_types(args.all);
    let mut first = true;

    loop {
        let stale = match store.priced_at(&args.league)? {
            Some(at) => store::unix_now() - at >= args.price_interval as i64,
            None => true,
        };
        if (first && args.refresh_prices) || stale {
            match fetch_prices(
                cfg,
                &mut store,
                &args.league,
                &item_types,
                true,
                args.base_url.clone(),
            ) {
                Ok((lines, categories)) => {
                    tracing::info!(lines, categories, "price snapshot refreshed")
                }
                Err(e) => tracing::warn!(error = %e, "price refresh failed; using stored snapshot"),
            }
        }

        match stash_pass(&mut store, &client, &account, &args, filter.as_deref()) {
            Ok(drops) => {
                print_drops(&drops);
                alert_dump(&store, &args.league, args.notify);
            }
            Err(e) if args.once => return Err(e),
            Err(e) => tracing::warn!(error = %e, interval = args.interval, "pass failed"),
        }

        first = false;
        if args.once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(args.interval));
    }
}

fn stash_pass(
    store: &mut Store,
    client: &OwnClient,
    account: &str,
    args: &StashWatch,
    filter: Option<&[i64]>,
) -> Result<Vec<store::ItemRow>> {
    let started = store::unix_now();
    let tabs = client.tabs(account, &args.league)?;
    let mut scanned = 0usize;

    for tab in &tabs.tabs {
        if tab.hidden {
            continue;
        }
        if let Some(filter) = filter
            && !filter.contains(&tab.index)
        {
            continue;
        }
        let items = client.tab(account, &args.league, tab.index)?;
        let stats = store.ingest_own_tab(
            &tab.stash_id(&args.league),
            &tab.name,
            &tab.kind,
            &args.league,
            Some(account),
            &items,
        )?;
        scanned += 1;
        if stats.items_new > 0 || stats.delisted > 0 {
            tracing::info!(
                tab = %tab.name,
                dropped = stats.items_new,
                removed = stats.delisted,
                "tab changed"
            );
        }
    }

    let valued = store.value_items(&args.league)?;
    let drops = store.search(&SearchQuery {
        source: Some("own".into()),
        since: Some(started),
        order: Order::Newest,
        limit: 100,
        ..Default::default()
    })?;
    tracing::info!(
        tabs = scanned,
        total_tabs = tabs.tabs.len(),
        valued,
        new = drops.len(),
        "pass complete"
    );
    Ok(drops)
}

fn print_drops(rows: &[store::ItemRow]) {
    if rows.is_empty() {
        return;
    }
    println!("new items:");
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.display_name(),
                r.stash_name.clone().unwrap_or_default(),
                r.ilvl.map(|v| v.to_string()).unwrap_or_default(),
                value_cell(r),
                render::age(r.first_seen),
            ]
        })
        .collect();
    print!(
        "{}",
        render::table(&["item", "tab", "ilvl", "value", "age"], &body, 44)
    );
}

fn stash_list(cfg: &Config) -> Result<()> {
    let store = open_store(cfg)?;
    let tabs = store.own_tabs()?;
    if tabs.is_empty() {
        println!("no own tabs scanned yet: big_oil stash watch --league <league>");
        return Ok(());
    }
    let body: Vec<Vec<String>> = tabs
        .iter()
        .map(|t| {
            vec![
                t.name.clone(),
                t.league.clone(),
                t.items.to_string(),
                t.live.to_string(),
                trim_num(t.value_chaos),
                trim_num(t.value_divine),
                t.last_seen.map(render::age).unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    print!(
        "{}",
        render::table(
            &[
                "tab", "league", "items", "live", "chaos", "divine", "scanned"
            ],
            &body,
            40
        )
    );
    let chaos: f64 = tabs.iter().map(|t| t.value_chaos).sum();
    let divine: f64 = tabs.iter().map(|t| t.value_divine).sum();
    println!(
        "total: {} chaos / {} divine across {} tabs",
        trim_num(chaos),
        trim_num(divine),
        tabs.len()
    );
    Ok(())
}

fn stash_value(cfg: &Config, league: &str) -> Result<()> {
    let mut store = open_store(cfg)?;
    if store.price_lines(league)?.is_empty() {
        bail!("no price snapshot for {league}: run `big_oil ninja fetch --league {league}` first");
    }
    let n = store.value_items(league)?;
    println!("valued {n} own items in {league}");
    Ok(())
}

fn dump_settings(args: &DumpArgs) -> Result<DumpSettings> {
    let cut_after_secs = render::parse_duration(&args.every)
        .with_context(|| format!("--every {:?} is not a duration like 12h or 1d", args.every))?;
    Ok(DumpSettings {
        tab: args.tab.clone(),
        underprice_ratio: (args.underpriced / 100.0).clamp(0.01, 1.0),
        min_value_chaos: args.min_value.max(0.0),
        min_loss_chaos: args.min_loss.max(0.0),
        cut_after_secs,
        cut_pct: args.cut.clamp(0.0, 95.0),
    })
}

fn build_report(store: &Store, league: &str, settings: &DumpSettings) -> Result<dump::Report> {
    let lines = store.price_lines(league)?;
    if lines.is_empty() {
        bail!("no price snapshot for {league}: run `big_oil ninja fetch --league {league}` first");
    }
    let rates = Rates::from_lines(&lines);
    let rows = store.search(&SearchQuery {
        source: Some("own".into()),
        limit: 100_000,
        ..Default::default()
    })?;
    Ok(dump::analyze(&rows, &rates, store::unix_now(), settings))
}

fn run_dump(cfg: &Config, args: DumpArgs) -> Result<()> {
    let settings = dump_settings(&args)?;
    let store = open_store(cfg)?;
    let report = build_report(&store, &args.league, &settings)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if let Some(at) = store.priced_at(&args.league)? {
            println!(
                "market values from {} ({} old)",
                render::ts(at),
                render::age(at)
            );
            println!();
        }
        print_report(&report, &settings);
    }
    if args.notify {
        notify_user(&report.summary());
    }
    Ok(())
}

fn print_report(report: &dump::Report, settings: &DumpSettings) {
    if report.is_quiet() {
        println!("nothing to do: no underpriced or stale listings");
        return;
    }

    if !report.underpriced.is_empty() {
        println!(
            "UNDERPRICED — listed below {}% of market",
            (settings.underprice_ratio * 100.0).round() as i64
        );
        let body: Vec<Vec<String>> = report
            .underpriced
            .iter()
            .map(|f| {
                vec![
                    f.name.clone(),
                    f.tab.clone().unwrap_or_default(),
                    f.listed.clone().unwrap_or_else(|| "-".into()),
                    f.value_chaos.map(chaos_cell).unwrap_or_else(|| "-".into()),
                    f.loss_chaos.map(chaos_cell).unwrap_or_else(|| "-".into()),
                    fmt_duration(f.age_secs),
                ]
            })
            .collect();
        print!(
            "{}",
            render::table(
                &["item", "tab", "listed", "value", "missing", "for"],
                &body,
                42
            )
        );
        println!(
            "value at risk: {} if these sell as listed",
            chaos_cell(report.value_at_risk)
        );
    }

    if !report.unpriced.is_empty() {
        if !report.underpriced.is_empty() {
            println!();
        }
        println!("NO PRICE — worth pricing");
        let body: Vec<Vec<String>> = report
            .unpriced
            .iter()
            .map(|f| {
                vec![
                    f.name.clone(),
                    f.tab.clone().unwrap_or_default(),
                    f.value_chaos.map(chaos_cell).unwrap_or_else(|| "-".into()),
                    fmt_duration(f.age_secs),
                ]
            })
            .collect();
        print!(
            "{}",
            render::table(&["item", "tab", "value", "for"], &body, 42)
        );
    }

    if !report.due.is_empty() {
        if !report.underpriced.is_empty() || !report.unpriced.is_empty() {
            println!();
        }
        println!(
            "DUE FOR A CUT — unchanged for {}h, cut {}%",
            settings.cut_after_secs / 3600,
            settings.cut_pct.round() as i64
        );
        let body: Vec<Vec<String>> = report
            .due
            .iter()
            .map(|f| {
                vec![
                    f.name.clone(),
                    f.tab.clone().unwrap_or_default(),
                    f.listed.clone().unwrap_or_else(|| "-".into()),
                    f.suggested.clone().unwrap_or_else(|| "-".into()),
                    fmt_duration(f.age_secs),
                ]
            })
            .collect();
        print!(
            "{}",
            render::table(&["item", "tab", "now", "cut to", "for"], &body, 42)
        );
    }

    println!("\n{}", report.summary());
}

/// Called after each stash pass so the reminder is impossible to miss.
fn alert_dump(store: &Store, league: &str, notify: bool) {
    let lines = match store.price_lines(league) {
        Ok(lines) if !lines.is_empty() => lines,
        _ => return,
    };
    let rates = Rates::from_lines(&lines);
    let rows = match store.search(&SearchQuery {
        source: Some("own".into()),
        limit: 100_000,
        ..Default::default()
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "dump check failed");
            return;
        }
    };
    let report = dump::analyze(&rows, &rates, store::unix_now(), &DumpSettings::default());
    if report.is_quiet() {
        return;
    }
    println!("dump: {}", report.summary());
    if notify {
        notify_user(&report.summary());
    }
}

fn notify_user(summary: &str) {
    match std::process::Command::new("notify-send")
        .args(["-u", "critical", "-a", "big_oil", "Dump tab", summary])
        .status()
    {
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "notify-send unavailable"),
    }
}

fn chaos_cell(value: f64) -> String {
    format!("{}c", trim_num(value))
}

fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
