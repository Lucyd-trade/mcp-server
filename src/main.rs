mod chain;
mod clob;
mod config;

use std::sync::Arc;

use alloy::{
    primitives::{Address, B256, U256},
    signers::local::PrivateKeySigner,
};
use rmcp::{
    ServerHandler, ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_handler,
    tool_router, transport::stdio,
};
use serde::Deserialize;
use serde_json::{Value, json};

use chain::Chain;
use clob::{Clob, ONE, OrderSpec, Side, TICK, TimeInForce, now_secs, outcome_id, price_to_ticks};
use config::Config;

type ToolResult = Result<String, String>;

/// Expiration given to fak and fok orders, seconds.
const IMMEDIATE_ORDER_TTL: u64 = 60;
/// How far below the best bid close_position sells by default, in price.
const CLOSE_SLIPPAGE: f64 = 0.02;

#[derive(Clone)]
struct Lucyd(Arc<Inner>);

struct Inner {
    cfg: Config,
    http: reqwest::Client,
    clob: Clob,
    chain: Option<Chain>,
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn parse_market(s: &str) -> Result<B256, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{s} is not a market address (0x + 64 hex characters)"))
}

fn parse_outcome(s: Option<&str>) -> Result<u8, String> {
    match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("up" | "yes") => Ok(0),
        Some("down" | "no") => Ok(1),
        Some(other) => Err(format!("unknown outcome {other}: use up or down")),
    }
}

fn outcome_name(index: u8) -> &'static str {
    if index == 0 { "up" } else { "down" }
}

// ── Tool parameters ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ListMarketsParams {
    /// Market state: open, pending (ended, waiting for result), settled or voided. Comma separated for several.
    state: Option<String>,
    /// Market period: 5m, 15m, 30m, 1h, 4h or Daily. Comma separated for several.
    period: Option<String>,
    /// Sort by end date: asc or desc. Default desc.
    order: Option<String>,
    /// How many markets to return, 1 to 100. Default 20.
    limit: Option<u32>,
    /// Skip this many markets, for paging.
    offset: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MarketParams {
    /// Market address (its id), from list_markets.
    market: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct OrderBookParams {
    /// Market address.
    market: String,
    /// Outcome whose book to show: up or down. Default up.
    outcome: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct TradesParams {
    /// Only trades on this market address.
    market: Option<String>,
    /// Only trades of this wallet. Use "me" for the configured wallet.
    trader: Option<String>,
    /// Only trades after this time, unix seconds.
    from: Option<u64>,
    /// Only trades before this time, unix seconds.
    to: Option<u64>,
    /// How many trades, 1 to 100. Default 20.
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct PositionsParams {
    /// Wallet address. Default: the configured wallet.
    trader: Option<String>,
    /// Only this market address, or several comma separated.
    market: Option<String>,
    /// Market state filter: open, pending, settled or voided.
    state: Option<String>,
    /// How many positions, 1 to 100. Default 50.
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct TraderParams {
    /// Wallet address. Default: the configured wallet.
    address: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct PnlParams {
    /// Wallet address. Default: the configured wallet.
    address: Option<String>,
    /// Period: 1d, 1w, 1m or all. Default 1w.
    range: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct TopTradersParams {
    /// Sort field: pnl, volume, markets or win_rate. Default pnl.
    sort: Option<String>,
    /// How many traders, 1 to 50. Default 10.
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct OrderHistoryParams {
    /// Only orders on this market address, or several comma separated.
    market: Option<String>,
    /// Only orders on this outcome: up or down. Needs market.
    outcome: Option<String>,
    /// Only buy or sell orders.
    side: Option<String>,
    /// Only these statuses, comma separated: received, queued, resting, pending_settlement, settled,
    /// filled, cancelled, expired, rejected, settlement_failed.
    status: Option<String>,
    /// Only orders received after this time, unix seconds.
    from: Option<u64>,
    /// Only orders received before this time, unix seconds.
    to: Option<u64>,
    /// How many orders, 1 to 500. Default 50.
    limit: Option<u32>,
    /// nextCursor from the previous page, to get the next one.
    cursor: Option<String>,
    /// Include each order's events (placed, trades, settlement). Default false.
    events: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PlaceOrderParams {
    /// Market address.
    market: String,
    /// Outcome to trade: up or down.
    outcome: String,
    /// buy or sell.
    side: String,
    /// Number of whole tokens. Each winning token pays 1 USDC.
    shares: u64,
    /// Limit price per token in USDC, 0.0025 to 0.9975 in steps of 0.0025. Buy: the most you pay. Sell: the least you accept.
    price: f64,
    /// gtc (rests until filled or canceled, default), gtd (rests until expires_in_seconds),
    /// fak (fill what is possible now, cancel the rest; use for market orders), fok (fill fully now or not at all).
    time_in_force: Option<String>,
    /// Lifetime for gtd orders, seconds.
    expires_in_seconds: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ClosePositionParams {
    /// Market address.
    market: String,
    /// Which outcome to sell: up or down. Default: every outcome the wallet holds.
    outcome: Option<String>,
    /// Lowest price to accept. Default: best bid minus 0.02.
    min_price: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct CancelParams {
    /// Order ids to cancel. Leave empty to cancel all open orders (optionally only on `market`).
    order_ids: Option<Vec<String>>,
    /// With no order_ids: cancel only open orders on this market address.
    market: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AutoRedeemParams {
    /// true to have winnings paid out automatically after each market resolves, false to turn it off.
    enabled: bool,
}

// ── Tools ───────────────────────────────────────────────────────────────────────

#[tool_router]
impl Lucyd {
    #[tool(
        description = "List prediction markets on Lucyd. Each market asks whether the BTC price will be \
        Up or Down at its end date versus the strike (strike is the price times 1e8). Returns address (the market id), \
        state, period, start and end dates, strike, winner and volume."
    )]
    async fn list_markets(&self, Parameters(p): Parameters<ListMarketsParams>) -> ToolResult {
        let mut q = vec![("limit", p.limit.unwrap_or(20).clamp(1, 100).to_string())];
        q.extend(p.state.map(|v| ("state", v)));
        q.extend(p.period.map(|v| ("period", v)));
        q.extend(p.order.map(|v| ("order", v)));
        q.extend(p.offset.map(|v| ("offset", v.to_string())));
        self.api("/markets", &q).await.map(|v| pretty(&v))
    }

    #[tool(
        description = "Full details of one market, including its rules and the ids of its up and down outcome tokens."
    )]
    async fn get_market(&self, Parameters(p): Parameters<MarketParams>) -> ToolResult {
        let market = parse_market(&p.market)?;
        let mut v = self.api(&format!("/markets/{market}"), &[]).await?;
        v["outcome_ids"] = json!({
            "up": outcome_id(market, 0).to_string(),
            "down": outcome_id(market, 1).to_string(),
        });
        Ok(pretty(&v))
    }

    #[tool(
        description = "Order book of a market outcome: bids (best first) and asks with prices in USDC per token \
        and sizes in tokens, plus best bid, best ask and mid. Buying down at p is the same book as selling up at 1 - p."
    )]
    async fn get_order_book(&self, Parameters(p): Parameters<OrderBookParams>) -> ToolResult {
        let market = parse_market(&p.market)?;
        let outcome = parse_outcome(p.outcome.as_deref())?;
        let asset = outcome_id(market, outcome).to_string();
        let book = self
            .0
            .clob
            .get("/v1/book", &[("asset_id", asset.clone())])
            .await?;
        let prices = self
            .0
            .clob
            .get("/v1/prices", &[("asset_id", asset)])
            .await?;
        let levels = |side: &Value| -> Value {
            side.as_array()
                .map(|lv| {
                    lv.iter()
                        .map(|l| {
                            let size = l["size"]
                                .as_str()
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0);
                            json!({ "price": l["price"], "tokens": size / ONE as f64 })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(pretty(&json!({
            "outcome": outcome_name(outcome),
            "best_bid": prices["best_bid"],
            "best_ask": prices["best_ask"],
            "mid": prices["mid"],
            "bids": levels(&book["bids"]),
            "asks": levels(&book["asks"]),
        })))
    }

    #[tool(
        description = "Recent trades, newest first. Filter by market and/or trader. Amounts are in USDC and tokens."
    )]
    async fn list_trades(&self, Parameters(p): Parameters<TradesParams>) -> ToolResult {
        let mut q = vec![("limit", p.limit.unwrap_or(20).clamp(1, 100).to_string())];
        q.extend(p.market.map(|v| ("market", v)));
        if let Some(t) = p.trader {
            q.push(("trader", self.resolve_wallet(Some(t))?));
        }
        q.extend(p.from.map(|v| ("from", v.to_string())));
        q.extend(p.to.map(|v| ("to", v.to_string())));
        self.api("/trades", &q).await.map(|v| pretty(&v))
    }

    #[tool(
        description = "Positions of a wallet: tokens held per market and outcome, average price, current value and \
        profit. Settled markets show the payout. Defaults to the configured wallet."
    )]
    async fn get_positions(&self, Parameters(p): Parameters<PositionsParams>) -> ToolResult {
        let mut q = vec![
            ("trader", self.resolve_wallet(p.trader)?),
            ("limit", p.limit.unwrap_or(50).clamp(1, 100).to_string()),
        ];
        q.extend(p.market.map(|v| ("market", v)));
        q.extend(p.state.map(|v| ("state", v)));
        self.api("/positions", &q).await.map(|v| pretty(&v))
    }

    #[tool(
        description = "Stats of a wallet: total and realized profit, volume, markets traded, win rate. \
        Defaults to the configured wallet."
    )]
    async fn get_trader_stats(&self, Parameters(p): Parameters<TraderParams>) -> ToolResult {
        let addr = self.resolve_wallet(p.address)?;
        self.api(&format!("/traders/{addr}"), &[])
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Profit and loss over time of a wallet, as a series of points. Defaults to the configured wallet."
    )]
    async fn get_pnl_history(&self, Parameters(p): Parameters<PnlParams>) -> ToolResult {
        let addr = self.resolve_wallet(p.address)?;
        let range = p.range.unwrap_or_else(|| "1w".into());
        self.api(&format!("/traders/{addr}/pnl-history"), &[("range", range)])
            .await
            .map(|v| pretty(&v))
    }

    #[tool(description = "Leaderboard of traders, best first.")]
    async fn top_traders(&self, Parameters(p): Parameters<TopTradersParams>) -> ToolResult {
        let q = vec![
            ("sort", p.sort.unwrap_or_else(|| "pnl".into())),
            ("order", "desc".into()),
            ("limit", p.limit.unwrap_or(10).clamp(1, 50).to_string()),
        ];
        self.api("/traders", &q).await.map(|v| pretty(&v))
    }

    #[tool(
        description = "Health of the Lucyd service: API and database, settlement of ended markets on time, \
        live trade stream. Use it when something looks stuck, e.g. a market is not resolving."
    )]
    async fn get_status(&self) -> ToolResult {
        let mut out = serde_json::Map::new();
        for (name, path) in [
            ("api", "/health"),
            ("settlement", "/health/settlement"),
            ("live_trades", "/health/trades-live"),
        ] {
            // A 503 still carries the details, so keep the body instead of failing.
            let v = match self
                .0
                .http
                .get(format!("{}{path}", self.0.cfg.api_url))
                .send()
                .await
            {
                Ok(resp) => {
                    let ok = resp.status().is_success();
                    let body = resp.json::<Value>().await.unwrap_or(Value::Null);
                    json!({ "healthy": ok, "details": body })
                }
                Err(e) => json!({ "healthy": false, "error": format!("unreachable: {e}") }),
            };
            out.insert(name.into(), v);
        }
        Ok(pretty(&Value::Object(out)))
    }

    #[tool(
        description = "The configured wallet: address, ETH (for gas) and USDC balances, whether trading is approved \
        and whether auto-redeem is on. Check this before the first trade."
    )]
    async fn get_wallet(&self) -> ToolResult {
        self.chain()?.wallet().await.map(|v| pretty(&v))
    }

    #[tool(description = "Open (resting) orders of the configured wallet on the order book.")]
    async fn get_open_orders(&self) -> ToolResult {
        let orders = self.0.clob.open_orders(self.key()?).await?;
        Ok(pretty(&orders))
    }

    #[tool(
        description = "Past and current orders of the configured wallet, newest first: status, filled quantity, \
        reason. Kept until 7 days after the market resolves. Page with cursor = nextCursor until it is null."
    )]
    async fn get_order_history(&self, Parameters(p): Parameters<OrderHistoryParams>) -> ToolResult {
        let key = self.key()?;
        let markets = p
            .market
            .as_deref()
            .map(|m| {
                m.split(',')
                    .map(parse_market)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        // The query is signed exactly as sent, so it is built by hand from validated values only.
        let mut q = vec![format!("limit={}", p.limit.unwrap_or(50).clamp(1, 500))];
        if !markets.is_empty() {
            let ids: Vec<String> = markets.iter().map(|m| m.to_string()).collect();
            q.push(format!("market={}", ids.join(",")));
        }
        if let Some(o) = p.outcome {
            if markets.is_empty() {
                return Err("outcome needs market".into());
            }
            let index = parse_outcome(Some(&o))?;
            let ids: Vec<String> = markets
                .iter()
                .map(|m| outcome_id(*m, index).to_string())
                .collect();
            q.push(format!("outcome={}", ids.join(",")));
        }
        if let Some(s) = p.side {
            let s = s.trim().to_ascii_lowercase();
            if s != "buy" && s != "sell" {
                return Err("side must be buy or sell".into());
            }
            q.push(format!("side={s}"));
        }
        if let Some(s) = p.status {
            let s = s.replace(' ', "").to_ascii_lowercase();
            if !s
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == ',')
            {
                return Err(format!("bad status {s}"));
            }
            q.push(format!("status={s}"));
        }
        q.extend(p.from.map(|v| format!("from={v}")));
        q.extend(p.to.map(|v| format!("to={v}")));
        if let Some(c) = p.cursor {
            if !c.chars().all(|c| c.is_ascii_digit() || c == '_') {
                return Err(format!("bad cursor {c}"));
            }
            q.push(format!("cursor={c}"));
        }
        if p.events == Some(true) {
            q.push("events=true".into());
        }
        let history = self.0.clob.order_history(key, &q.join("&")).await?;
        Ok(pretty(&history))
    }

    #[tool(
        description = "Place a limit order. For a market order use time_in_force fak with a price you accept: \
        buying, the highest price; selling, the lowest. Amounts are whole tokens; cost of a buy is shares x price USDC. \
        Returns the order id and what happened: placed (resting), trade (filled, possibly partly) or rejected."
    )]
    async fn place_order(&self, Parameters(p): Parameters<PlaceOrderParams>) -> ToolResult {
        self.check_writable()?;
        let side = match p.side.trim().to_ascii_lowercase().as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            other => return Err(format!("unknown side {other}: use buy or sell")),
        };
        let tif = TimeInForce::parse(p.time_in_force.as_deref().unwrap_or("gtc"))?;
        let expiration = match tif {
            TimeInForce::Gtc => 0,
            TimeInForce::Gtd => {
                now_secs()
                    + p.expires_in_seconds
                        .ok_or("gtd orders need expires_in_seconds")?
            }
            TimeInForce::Fak | TimeInForce::Fok => now_secs() + IMMEDIATE_ORDER_TTL,
        };
        let order = OrderSpec {
            market: parse_market(&p.market)?,
            outcome: parse_outcome(Some(&p.outcome))?,
            side,
            shares: p.shares,
            ticks: price_to_ticks(p.price)?,
            tif,
            expiration,
        };
        self.submit(order).await
    }

    #[tool(
        description = "Close a position: sell all tokens the wallet holds on a market right away (fak order) \
        at the best available prices down to min_price."
    )]
    async fn close_position(&self, Parameters(p): Parameters<ClosePositionParams>) -> ToolResult {
        self.check_writable()?;
        let market = parse_market(&p.market)?;
        let balances = self.chain()?.token_balances(market).await?;
        let outcomes: Vec<u8> = match p.outcome {
            Some(o) => vec![parse_outcome(Some(&o))?],
            None => vec![0, 1],
        };
        let mut results = Vec::new();
        for outcome in outcomes {
            let shares: u64 = (balances[outcome as usize] / U256::from(ONE))
                .try_into()
                .unwrap_or(u64::MAX);
            if shares == 0 {
                continue;
            }
            let min_price = match p.min_price {
                Some(price) => price,
                None => {
                    let asset = outcome_id(market, outcome).to_string();
                    let prices = self
                        .0
                        .clob
                        .get("/v1/prices", &[("asset_id", asset)])
                        .await?;
                    let bid = prices["best_bid"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .or_else(|| prices["best_bid"].as_f64())
                        .ok_or(format!(
                            "nobody is buying {} on this market right now",
                            outcome_name(outcome)
                        ))?;
                    (((bid - CLOSE_SLIPPAGE) / TICK).floor() * TICK).max(TICK)
                }
            };
            let order = OrderSpec {
                market,
                outcome,
                side: Side::Sell,
                shares,
                ticks: price_to_ticks((min_price / TICK).round() * TICK)?,
                tif: TimeInForce::Fak,
                expiration: now_secs() + IMMEDIATE_ORDER_TTL,
            };
            let res = self.submit(order).await?;
            results.push(json!({ "outcome": outcome_name(outcome), "shares": shares, "min_price": min_price, "result": res }));
        }
        if results.is_empty() {
            return Err("the wallet holds no whole tokens on this market".into());
        }
        Ok(pretty(&json!(results)))
    }

    #[tool(
        description = "Cancel open orders by id. With no ids, cancels all open orders of the wallet, \
        or only those on `market` if given."
    )]
    async fn cancel_orders(&self, Parameters(p): Parameters<CancelParams>) -> ToolResult {
        self.check_writable()?;
        let key = self.key()?;
        let ids: Vec<B256> = match p.order_ids.filter(|v| !v.is_empty()) {
            Some(ids) => ids
                .iter()
                .map(|s| {
                    s.trim()
                        .parse()
                        .map_err(|_| format!("{s} is not an order id"))
                })
                .collect::<Result<_, _>>()?,
            None => {
                let market = p.market.as_deref().map(parse_market).transpose()?;
                let open = self.0.clob.open_orders(key).await?;
                open.as_array()
                    .into_iter()
                    .flatten()
                    .filter(|o| match market {
                        Some(m) => {
                            o["marketId"].as_str().and_then(|s| s.parse::<B256>().ok()) == Some(m)
                        }
                        None => true,
                    })
                    .filter_map(|o| o["orderId"].as_str()?.parse().ok())
                    .collect()
            }
        };
        if ids.is_empty() {
            return Ok("no open orders to cancel".into());
        }
        let mut session = self.0.clob.connect(key).await?;
        let mut results = Vec::new();
        for chunk in ids.chunks(64) {
            match session.cancel(chunk).await {
                Ok(r) => results.push(r),
                Err(e) => {
                    session.close().await;
                    return Err(e);
                }
            }
        }
        session.close().await;
        Ok(pretty(&json!(results)))
    }

    #[tool(
        description = "One-time setup before trading: approve the Exchange contract to spend the wallet's USDC and \
        to move its outcome tokens. Sends up to two transactions and needs a little Sepolia ETH for gas."
    )]
    async fn approve_trading(&self) -> ToolResult {
        self.check_writable()?;
        self.chain()?.approve_trading().await.map(|v| pretty(&v))
    }

    #[tool(
        description = "Turn automatic payout of winnings on or off. When on, winning tokens are redeemed to USDC \
        for the wallet soon after each market resolves. Sends one transaction."
    )]
    async fn set_auto_redeem(&self, Parameters(p): Parameters<AutoRedeemParams>) -> ToolResult {
        self.check_writable()?;
        self.chain()?
            .set_auto_redeem(p.enabled)
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Collect the payout of a resolved market: exchanges the wallet's winning tokens for USDC \
        (1 USDC per token minus the fee, 0.5 per token if the market was voided)."
    )]
    async fn redeem(&self, Parameters(p): Parameters<MarketParams>) -> ToolResult {
        self.check_writable()?;
        self.chain()?
            .redeem(parse_market(&p.market)?)
            .await
            .map(|v| pretty(&v))
    }
}

#[tool_handler(
    name = "lucyd",
    instructions = "Lucyd is a prediction market exchange. Markets ask whether the BTC price goes Up or Down over a period \
        (5m to 1 day). Outcome tokens trade between 0.0025 and 0.9975 USDC and a winning token pays 1 USDC. \
        Before the first trade check get_wallet and run approve_trading if it is not ready. \
        Confirm with the user before placing orders, closing positions or sending transactions. \
        The exchange runs on the Sepolia testnet with test USDC."
)]
impl ServerHandler for Lucyd {}

impl Lucyd {
    async fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let resp = self
            .0
            .http
            .get(format!("{}{path}", self.0.cfg.api_url))
            .query(query)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("reading response failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("API returned {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("bad API response: {e}"))
    }

    fn key(&self) -> Result<&PrivateKeySigner, String> {
        self.0.cfg.key.as_ref().ok_or_else(|| {
            "no wallet configured: set LUCYD_PRIVATE_KEY in the MCP server settings".into()
        })
    }

    fn chain(&self) -> Result<&Chain, String> {
        self.key()?;
        self.0
            .chain
            .as_ref()
            .ok_or_else(|| "wallet is not connected to the chain".into())
    }

    fn check_writable(&self) -> Result<(), String> {
        if self.0.cfg.read_only {
            return Err("the server runs in read-only mode (LUCYD_READ_ONLY)".into());
        }
        self.key().map(|_| ())
    }

    /// An explicit address, "me", or the configured wallet when none is given.
    fn resolve_wallet(&self, addr: Option<String>) -> Result<String, String> {
        match addr.as_deref().map(str::trim) {
            None | Some("" | "me") => Ok(self.key()?.address().to_string()),
            Some(a) => a
                .parse::<Address>()
                .map(|a| a.to_string())
                .map_err(|_| format!("{a} is not a wallet address")),
        }
    }

    async fn submit(&self, order: OrderSpec) -> ToolResult {
        if order.shares == 0 {
            return Err("shares must be at least 1".into());
        }
        let notional = order.usdc_amount() as f64 / ONE as f64;
        if notional < 1.0 {
            return Err(format!(
                "order is worth {notional} USDC, the minimum is 1 USDC"
            ));
        }
        if order.side == Side::Buy && notional > self.0.cfg.max_order_usd {
            return Err(format!(
                "order is worth {notional} USDC, above the limit of {} USDC (LUCYD_MAX_ORDER_USD)",
                self.0.cfg.max_order_usd
            ));
        }
        let mut session = self.0.clob.connect(self.key()?).await?;
        let res = session.submit(&order).await;
        session.close().await;
        let mut v = res?;
        v["outcome"] = outcome_name(order.outcome).into();
        v["shares"] = order.shares.into();
        v["price"] = (order.ticks as f64 * TICK).into();
        v["usdc"] = notional.into();
        Ok(pretty(&v))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // reqwest and the WebSocket both use rustls; pick its crypto backend once for the process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = Config::from_env()?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let chain = match &cfg.key {
        Some(key) => Some(Chain::new(&cfg.rpc_url, key).map_err(anyhow::Error::msg)?),
        None => None,
    };
    let clob = Clob {
        http: http.clone(),
        base_url: cfg.clob_url.clone(),
    };
    let server = Lucyd(Arc::new(Inner {
        cfg,
        http,
        clob,
        chain,
    }));
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}
