use alloy::{
    primitives::{Address, address},
    signers::local::PrivateKeySigner,
};

pub const CHAIN_ID: u64 = 11155111;
pub const EXCHANGE: Address = address!("0x6588c789c6b6df8f8b6b92e6433f813cc2d1d4d0");
pub const USDC: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
pub const CONDITIONAL_TOKENS: Address = address!("0xB62eFc43C16b4E2Bf1aAAD33002368e55708538D");
pub const AUTO_REDEEMER: Address = address!("0x42Bf669f0A3DCc95510001Ddbc43D56FdE67268F");

pub struct Config {
    pub api_url: String,
    pub clob_url: String,
    pub rpc_url: String,
    pub key: Option<PrivateKeySigner>,
    pub read_only: bool,
    /// Largest order notional in USDC.
    pub max_order_usd: f64,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let key = match env("LUCYD_PRIVATE_KEY") {
            Some(k) => Some(
                k.parse::<PrivateKeySigner>()
                    .map_err(|_| anyhow::anyhow!("LUCYD_PRIVATE_KEY is not a valid private key"))?,
            ),
            None => None,
        };
        let max_order_usd = match env("LUCYD_MAX_ORDER_USD") {
            Some(v) => v
                .parse()
                .map_err(|_| anyhow::anyhow!("LUCYD_MAX_ORDER_USD must be a number"))?,
            None => 100.0,
        };
        let url = |name, default: &str| {
            env(name)
                .unwrap_or_else(|| default.to_string())
                .trim_end_matches('/')
                .to_string()
        };
        Ok(Self {
            api_url: url("LUCYD_API_URL", "https://api.testnet.lucyd.trade"),
            clob_url: url("LUCYD_CLOB_URL", "https://clob.testnet.lucyd.trade"),
            rpc_url: url(
                "LUCYD_RPC_URL",
                "https://ethereum-sepolia-rpc.publicnode.com",
            ),
            key,
            read_only: matches!(
                env("LUCYD_READ_ONLY").as_deref(),
                Some("1" | "true" | "yes")
            ),
            max_order_usd,
        })
    }
}
