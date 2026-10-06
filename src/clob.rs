//! Client for the Lucyd order book (CLOB): EIP-712 signing, the trading WebSocket and HTTP reads.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::{Address, B256, U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::{Eip712Domain, SolStruct},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::config::{CHAIN_ID, EXCHANGE};

/// Price step: 0.0025 USDC.
pub const TICK: f64 = 0.0025;
/// USDC base units per tick for one whole token (0.0025 * 1e6).
const UNITS_PER_TICK: u64 = 2_500;
/// Base units per whole token or USDC.
pub const ONE: u64 = 1_000_000;
const PROTOCOL_VERSION: u16 = 1;

sol! {
    struct WebSocketAuthV1 {
        uint16 protocolVersion;
        address account;
        address signer;
        bytes32 challenge;
        uint64 issuedAt;
        uint64 challengeExpiration;
        uint64 sessionExpiration;
    }

    struct SettlementOrderV1 {
        uint16 protocolVersion;
        address maker;
        address signer;
        address taker;
        bytes32 outcomeId;
        bytes32 marketId;
        uint8 side;
        uint8 timeInForce;
        uint64 makerAmount;
        uint64 takerAmount;
        uint64 minFillAmount;
        uint64 expiration;
        uint64 nonceEpoch;
        bytes32 salt;
    }

    struct CancelOrdersV1 {
        uint16 protocolVersion;
        address account;
        address signer;
        bytes32 orderIdsHash;
        uint64 nonce;
        uint64 expiration;
    }

    struct HttpGetRequestV1 {
        uint16 protocolVersion;
        address account;
        address signer;
        bytes32 targetHash;
        uint64 nonce;
        uint64 expiration;
    }
}

fn domain(name: &'static str, chain_id: u64, verifying_contract: Address) -> Eip712Domain {
    Eip712Domain::new(
        Some(name.into()),
        Some("1".into()),
        Some(U256::from(chain_id)),
        Some(verifying_contract),
        None,
    )
}

fn sign<T: SolStruct>(
    key: &PrivateKeySigner,
    domain: &Eip712Domain,
    data: &T,
) -> Result<String, String> {
    let sig = key
        .sign_hash_sync(&data.eip712_signing_hash(domain))
        .map_err(|e| format!("signing failed: {e}"))?;
    Ok(format!("0x{}", alloy::hex::encode(sig.as_bytes())))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Outcome token id: keccak256(marketId ++ uint8(index)); 0 = up, 1 = down.
pub fn outcome_id(market: B256, index: u8) -> B256 {
    let mut buf = [0u8; 33];
    buf[..32].copy_from_slice(market.as_slice());
    buf[32] = index;
    keccak256(buf)
}

/// Converts a price like 0.55 to ticks of 0.0025, rejecting prices off the grid.
pub fn price_to_ticks(price: f64) -> Result<u64, String> {
    let ticks = (price / TICK).round();
    if !(1.0..=399.0).contains(&ticks) || (ticks * TICK - price).abs() > 1e-9 {
        return Err(format!(
            "price {price} must be between 0.0025 and 0.9975 in steps of 0.0025"
        ));
    }
    Ok(ticks as u64)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TimeInForce {
    Gtc,
    Gtd,
    Fak,
    Fok,
}

impl TimeInForce {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "gtc" => Ok(Self::Gtc),
            "gtd" => Ok(Self::Gtd),
            "fak" | "ioc" => Ok(Self::Fak),
            "fok" => Ok(Self::Fok),
            other => Err(format!(
                "unknown time_in_force {other}: use gtc, gtd, fak or fok"
            )),
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Gtc => 0,
            Self::Gtd => 1,
            Self::Fak => 2,
            Self::Fok => 3,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Gtc => "gtc",
            Self::Gtd => "gtd",
            Self::Fak => "fak",
            Self::Fok => "fok",
        }
    }
}

pub struct OrderSpec {
    pub market: B256,
    pub outcome: u8,
    pub side: Side,
    pub shares: u64,
    pub ticks: u64,
    pub tif: TimeInForce,
    /// Unix seconds, 0 for gtc.
    pub expiration: u64,
}

impl OrderSpec {
    pub fn usdc_amount(&self) -> u64 {
        self.shares * self.ticks * UNITS_PER_TICK
    }
}

pub struct Clob {
    pub http: reqwest::Client,
    pub base_url: String,
}

impl Clob {
    fn ws_url(&self, path: &str) -> String {
        let base = self
            .base_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}{path}")
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .query(query)
            .send()
            .await
            .map_err(|e| format!("order book request failed: {e}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("bad order book response: {e}"))?;
        if !status.is_success() {
            return Err(format!("order book returned {status}: {body}"));
        }
        Ok(body)
    }

    /// Open orders of the wallet, `GET /v1/orders` signed with headers.
    pub async fn open_orders(&self, key: &PrivateKeySigner) -> Result<Value, String> {
        let me = key.address();
        let nonce = now_ms();
        let expiration = nonce + 20_000;
        let req = HttpGetRequestV1 {
            protocolVersion: PROTOCOL_VERSION,
            account: me,
            signer: me,
            targetHash: keccak256(b"GET\n/v1/orders"),
            nonce,
            expiration,
        };
        let signature = sign(key, &domain("Exchange HTTP", CHAIN_ID, EXCHANGE), &req)?;
        let resp = self
            .http
            .get(format!("{}/v1/orders", self.base_url))
            .header("X-Exchange-Account", me.to_string())
            .header("X-Exchange-Signer", me.to_string())
            .header("X-Exchange-Nonce", nonce.to_string())
            .header("X-Exchange-Expiration", expiration.to_string())
            .header("X-Exchange-Signature", signature)
            .send()
            .await
            .map_err(|e| format!("open orders request failed: {e}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("bad open orders response: {e}"))?;
        if !status.is_success() {
            return Err(format!("open orders returned {status}: {body}"));
        }
        Ok(body)
    }

    pub async fn connect(&self, key: &PrivateKeySigner) -> Result<Session, String> {
        let (ws, _) = connect_async(self.ws_url("/v1/ws"))
            .await
            .map_err(|e| format!("cannot connect to the exchange: {e}"))?;
        let mut session = Session {
            ws,
            key: key.clone(),
            nonce: 0,
            seq: 0,
        };
        session.authenticate().await?;
        Ok(session)
    }
}

pub struct Session {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    key: PrivateKeySigner,
    nonce: u64,
    seq: u64,
}

impl Session {
    fn request_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{}-{}", now_ms(), self.seq)
    }

    async fn send(&mut self, msg: Value) -> Result<(), String> {
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| format!("exchange connection lost: {e}"))
    }

    /// Next JSON message, or None when `deadline` passes.
    async fn recv(&mut self, deadline: Instant) -> Result<Option<Value>, String> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let msg = match tokio::time::timeout(left, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Err("exchange closed the connection".into()),
                Ok(Some(Err(e))) => return Err(format!("exchange connection lost: {e}")),
                Ok(Some(Ok(m))) => m,
            };
            match msg {
                Message::Text(t) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        return Ok(Some(v));
                    }
                }
                Message::Close(frame) => {
                    return Err(format!("exchange closed the connection: {frame:?}"));
                }
                _ => {}
            }
        }
    }

    async fn authenticate(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let ch = loop {
            match self.recv(deadline).await? {
                Some(m) if m["type"] == "auth_challenge" => break m,
                Some(_) => continue,
                None => return Err("no auth challenge from the exchange".into()),
            }
        };
        let field = |name: &str| {
            ch[name]
                .as_u64()
                .ok_or(format!("auth challenge without {name}"))
        };
        let challenge: B256 = ch["challenge"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or("auth challenge without challenge")?;
        let chain_id = field("chainId")?;
        let contract: Address = ch["verifyingContract"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or("auth challenge without verifyingContract")?;
        let me = self.key.address();
        let auth = WebSocketAuthV1 {
            protocolVersion: PROTOCOL_VERSION,
            account: me,
            signer: me,
            challenge,
            issuedAt: field("issuedAt")?,
            challengeExpiration: field("challengeExpiration")?,
            sessionExpiration: field("sessionExpiration")?,
        };
        let signature = sign(
            &self.key,
            &domain("Exchange WebSocket", chain_id, contract),
            &auth,
        )?;
        let request_id = self.request_id("auth");
        self.send(json!({
            "type": "authenticate", "requestId": request_id,
            "account": me.to_string(), "signer": me.to_string(), "signature": signature,
        }))
        .await?;
        loop {
            match self.recv(deadline).await? {
                Some(m) if m["type"] == "authenticated" => return Ok(()),
                Some(m) if m["type"] == "auth_error" => {
                    return Err(format!("exchange rejected the login: {m}"));
                }
                Some(_) => continue,
                None => return Err("exchange did not answer the login".into()),
            }
        }
    }

    /// Signs and submits one order, then collects its events for a few seconds.
    pub async fn submit(&mut self, o: &OrderSpec) -> Result<Value, String> {
        let me = self.key.address();
        let outcome = outcome_id(o.market, o.outcome);
        let tokens = o.shares * ONE;
        let usdc = o.usdc_amount();
        let (maker_amount, taker_amount) = match o.side {
            Side::Buy => (usdc, tokens),
            Side::Sell => (tokens, usdc),
        };
        let salt = B256::from(rand::random::<[u8; 32]>());
        let order = SettlementOrderV1 {
            protocolVersion: PROTOCOL_VERSION,
            maker: me,
            signer: me,
            taker: Address::ZERO,
            outcomeId: outcome,
            marketId: o.market,
            side: if o.side == Side::Buy { 0 } else { 1 },
            timeInForce: o.tif.code(),
            makerAmount: maker_amount,
            takerAmount: taker_amount,
            minFillAmount: 0,
            expiration: o.expiration,
            nonceEpoch: 0,
            salt,
        };
        let signature = sign(&self.key, &domain("Exchange", CHAIN_ID, EXCHANGE), &order)?;
        let request_id = self.request_id("order");
        self.send(json!({
            "type": "submit_order",
            "requestId": request_id,
            "order": {
                "protocolVersion": PROTOCOL_VERSION,
                "clOrdId": 1,
                "settlementOrder": {
                    "maker": {"kind": "evm", "value": me.to_string()},
                    "signer": {"kind": "evm", "value": me.to_string()},
                    "taker": null,
                    "outcomeId": outcome.to_string(),
                    "side": if o.side == Side::Buy { "buy" } else { "sell" },
                    "makerAmount": maker_amount,
                    "takerAmount": taker_amount,
                    "nonceEpoch": 0,
                    "salt": salt.to_string(),
                },
                "marketId": o.market.to_string(),
                "timeInForce": o.tif.name(),
                "minFillAmount": 0,
                "expiration": o.expiration,
                "signature": signature,
            }
        }))
        .await?;

        let deadline = Instant::now() + Duration::from_secs(10);
        let order_id = loop {
            match self.recv(deadline).await? {
                Some(m) if m["requestId"] == request_id.as_str() => {
                    if m["type"] == "accepted" {
                        break m["orderId"].as_str().unwrap_or_default().to_string();
                    }
                    return Err(format!(
                        "order not accepted: {}",
                        m["error"].as_str().unwrap_or(&m.to_string())
                    ));
                }
                Some(_) => continue,
                None => return Err("exchange did not answer the order".into()),
            }
        };

        // Events for this order. Stop shortly after it rests, is rejected or stops filling.
        let mut events = Vec::new();
        let hard_stop = Instant::now() + Duration::from_secs(8);
        let mut quiet_until = hard_stop;
        loop {
            let until = quiet_until.min(hard_stop);
            let Some(m) = self.recv(until).await? else {
                break;
            };
            if m["orderId"] != order_id.as_str() {
                continue;
            }
            let kind = m["type"].as_str().unwrap_or_default().to_string();
            events.push(m);
            match kind.as_str() {
                "rejected" | "canceled" => break,
                "placed" | "trade" => quiet_until = Instant::now() + Duration::from_millis(1500),
                _ => {}
            }
        }
        Ok(json!({ "orderId": order_id, "events": events }))
    }

    pub async fn cancel(&mut self, order_ids: &[B256]) -> Result<Value, String> {
        let me = self.key.address();
        let mut packed = Vec::with_capacity(order_ids.len() * 32);
        for id in order_ids {
            packed.extend_from_slice(id.as_slice());
        }
        self.nonce += 1;
        let expiration = now_ms() + 20_000;
        let req = CancelOrdersV1 {
            protocolVersion: PROTOCOL_VERSION,
            account: me,
            signer: me,
            orderIdsHash: keccak256(&packed),
            nonce: self.nonce,
            expiration,
        };
        let signature = sign(
            &self.key,
            &domain("Exchange WebSocket", CHAIN_ID, EXCHANGE),
            &req,
        )?;
        let request_id = self.request_id("cancel");
        let ids: Vec<String> = order_ids.iter().map(|id| id.to_string()).collect();
        self.send(json!({
            "type": "cancel", "requestId": request_id, "orderIds": ids,
            "account": me.to_string(), "signer": me.to_string(),
            "nonce": self.nonce, "expiration": expiration, "signature": signature,
        }))
        .await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.recv(deadline).await? {
                Some(m) if m["requestId"] == request_id.as_str() => return Ok(m),
                Some(_) => continue,
                None => return Err("exchange did not answer the cancel".into()),
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}
