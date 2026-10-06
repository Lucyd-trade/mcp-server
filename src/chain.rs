//! On-chain reads and transactions on Sepolia: balances, approvals, redemptions.

use alloy::{
    network::EthereumWallet,
    primitives::{Address, B256, U256, utils::format_units},
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use serde_json::{Value, json};

use crate::clob::outcome_id;
use crate::config::{AUTO_REDEEMER, CONDITIONAL_TOKENS, EXCHANGE, USDC};

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }

    #[sol(rpc)]
    interface IConditionalTokens {
        function balanceOf(address account, uint256 id) external view returns (uint256);
        function isApprovedForAll(address account, address operator) external view returns (bool);
        function setApprovalForAll(address operator, bool approved) external;
        function redeemPositions(bytes32 marketId, uint8 side, uint256 amount) external;
        function markets(bytes32 marketId) external view returns (address settler, uint8 winningSide);
    }
}

const UNRESOLVED: u8 = 255;
const VOID: u8 = 2;

pub struct Chain {
    provider: DynProvider,
    me: Address,
}

fn usdc(v: U256) -> String {
    format_units(v, 6).unwrap_or_else(|_| v.to_string())
}

fn err(what: &str) -> impl Fn(alloy::contract::Error) -> String + '_ {
    move |e| format!("{what} failed: {e}")
}

impl Chain {
    pub fn new(rpc_url: &str, key: &PrivateKeySigner) -> Result<Self, String> {
        let url = rpc_url
            .parse()
            .map_err(|e| format!("bad LUCYD_RPC_URL: {e}"))?;
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(key.clone()))
            .connect_http(url)
            .erased();
        Ok(Self {
            provider,
            me: key.address(),
        })
    }

    /// Balances and approvals of the wallet.
    pub async fn wallet(&self) -> Result<Value, String> {
        let erc20 = IERC20::new(USDC, &self.provider);
        let ctf = IConditionalTokens::new(CONDITIONAL_TOKENS, &self.provider);
        let eth = self
            .provider
            .get_balance(self.me)
            .await
            .map_err(|e| format!("reading ETH balance failed: {e}"))?;
        let balance = erc20
            .balanceOf(self.me)
            .call()
            .await
            .map_err(err("reading USDC balance"))?;
        let allowance = erc20
            .allowance(self.me, EXCHANGE)
            .call()
            .await
            .map_err(err("reading USDC allowance"))?;
        let tokens_approved = ctf
            .isApprovedForAll(self.me, EXCHANGE)
            .call()
            .await
            .map_err(err("reading token approval"))?;
        let auto_redeem = ctf
            .isApprovedForAll(self.me, AUTO_REDEEMER)
            .call()
            .await
            .map_err(err("reading auto-redeem approval"))?;
        Ok(json!({
            "address": self.me.to_string(),
            "eth": format_units(eth, 18).unwrap_or_default(),
            "usdc": usdc(balance),
            "usdc_allowance_to_exchange": if allowance > U256::from(u64::MAX) { "unlimited".to_string() } else { usdc(allowance) },
            "can_sell_tokens": tokens_approved,
            "auto_redeem": auto_redeem,
            "ready_to_trade": allowance > U256::ZERO && tokens_approved,
        }))
    }

    async fn send(
        &self,
        tx: alloy::rpc::types::TransactionRequest,
        what: &str,
    ) -> Result<String, String> {
        let pending = self
            .provider
            .send_transaction(tx)
            .await
            .map_err(|e| format!("{what} failed: {e}"))?;
        let hash = *pending.tx_hash();
        let receipt = pending
            .get_receipt()
            .await
            .map_err(|e| format!("{what}: transaction {hash} sent but not confirmed: {e}"))?;
        if !receipt.status() {
            return Err(format!("{what}: transaction {hash} reverted"));
        }
        Ok(hash.to_string())
    }

    /// Unlimited USDC allowance and token approval for the Exchange, skipping what is already set.
    pub async fn approve_trading(&self) -> Result<Value, String> {
        let erc20 = IERC20::new(USDC, &self.provider);
        let ctf = IConditionalTokens::new(CONDITIONAL_TOKENS, &self.provider);
        let mut done = serde_json::Map::new();
        let allowance = erc20
            .allowance(self.me, EXCHANGE)
            .call()
            .await
            .map_err(err("reading USDC allowance"))?;
        if allowance < U256::from(u64::MAX) {
            let tx = erc20
                .approve(EXCHANGE, U256::MAX)
                .into_transaction_request();
            done.insert(
                "usdc_approve_tx".into(),
                self.send(tx, "USDC approve").await?.into(),
            );
        }
        if !ctf
            .isApprovedForAll(self.me, EXCHANGE)
            .call()
            .await
            .map_err(err("reading token approval"))?
        {
            let tx = ctf
                .setApprovalForAll(EXCHANGE, true)
                .into_transaction_request();
            done.insert(
                "token_approve_tx".into(),
                self.send(tx, "token approval").await?.into(),
            );
        }
        if done.is_empty() {
            done.insert("status".into(), "already approved".into());
        }
        Ok(Value::Object(done))
    }

    pub async fn set_auto_redeem(&self, enabled: bool) -> Result<Value, String> {
        let ctf = IConditionalTokens::new(CONDITIONAL_TOKENS, &self.provider);
        let current = ctf
            .isApprovedForAll(self.me, AUTO_REDEEMER)
            .call()
            .await
            .map_err(err("reading auto-redeem approval"))?;
        if current == enabled {
            return Ok(json!({ "auto_redeem": enabled, "status": "already set" }));
        }
        let tx = ctf
            .setApprovalForAll(AUTO_REDEEMER, enabled)
            .into_transaction_request();
        let hash = self.send(tx, "auto-redeem approval").await?;
        Ok(json!({ "auto_redeem": enabled, "tx": hash }))
    }

    /// Outcome token balances of the wallet on a market, in whole tokens.
    pub async fn token_balances(&self, market: B256) -> Result<[U256; 2], String> {
        let ctf = IConditionalTokens::new(CONDITIONAL_TOKENS, &self.provider);
        let up = ctf
            .balanceOf(self.me, outcome_id(market, 0).into())
            .call()
            .await
            .map_err(err("reading token balance"))?;
        let down = ctf
            .balanceOf(self.me, outcome_id(market, 1).into())
            .call()
            .await
            .map_err(err("reading token balance"))?;
        Ok([up, down])
    }

    /// Redeems the wallet's payable tokens on a resolved market.
    pub async fn redeem(&self, market: B256) -> Result<Value, String> {
        let ctf = IConditionalTokens::new(CONDITIONAL_TOKENS, &self.provider);
        let info = ctf
            .markets(market)
            .call()
            .await
            .map_err(err("reading market"))?;
        if info.settler == Address::ZERO {
            return Err("market not found on chain".into());
        }
        if info.winningSide == UNRESOLVED {
            return Err("market is not resolved yet".into());
        }
        let balances = self.token_balances(market).await?;
        let sides: Vec<u8> = if info.winningSide == VOID {
            vec![0, 1]
        } else {
            vec![info.winningSide]
        };
        let mut txs = Vec::new();
        for side in sides {
            let amount = balances[side as usize];
            if amount.is_zero() {
                continue;
            }
            let tx = ctf
                .redeemPositions(market, side, amount)
                .into_transaction_request();
            let hash = self.send(tx, "redeem").await?;
            txs.push(json!({ "outcome": if side == 0 { "up" } else { "down" }, "tokens": usdc(amount), "tx": hash }));
        }
        let result = match info.winningSide {
            0 => "up",
            1 => "down",
            _ => "void (each token pays 0.5 USDC)",
        };
        if txs.is_empty() {
            return Ok(json!({ "result": result, "status": "nothing to redeem" }));
        }
        Ok(json!({ "result": result, "redeemed": txs }))
    }
}
