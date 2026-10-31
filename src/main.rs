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
use big_oil::model::StashPage;
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
                "league", "item", "ilvl", "price", "age", "status", "stash", "account"
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
    let (mods_text, raw) = store.item_extra(id)?.unwrap_or_default();

    if json {
        let value = serde_json::json!({
            "item": item,
            "history": history,
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
