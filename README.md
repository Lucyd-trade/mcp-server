# Lucyd MCP server

Connect an AI agent (Claude, Cursor or any [MCP](https://modelcontextprotocol.io) client) to [Lucyd](https://testnet.lucyd.trade), a prediction markets exchange. Ask in plain language to browse markets, read the order book, trade, close positions and collect winnings.

> Lucyd currently runs on the Ethereum Sepolia testnet with test USDC.

## What it can do

| Tool | What it does |
|---|---|
| `list_markets` | Markets with filters by state and period |
| `get_market` | One market: rules, strike, end date, outcome token ids |
| `get_order_book` | Bids and asks of an outcome, best bid, best ask, mid |
| `list_trades` | Recent trades, by market or trader |
| `get_positions` | Tokens held, average price, value and profit |
| `get_trader_stats` | Profit, volume, win rate of a wallet |
| `get_pnl_history` | Profit over time |
| `top_traders` | Leaderboard |
| `get_wallet` | Your address, ETH and USDC balances, approvals |
| `get_open_orders` | Your resting orders |
| `place_order` | Limit or market order (buy or sell up or down) |
| `close_position` | Sell everything you hold on a market right away |
| `cancel_orders` | Cancel by id, all on a market, or all |
| `approve_trading` | One-time approvals for the Exchange contract |
| `set_auto_redeem` | Turn automatic payout of winnings on or off |
| `redeem` | Collect the payout of a resolved market |

Example requests:

- "Show the open 1h markets and the order book of the current one"
- "Buy 10 up tokens in the current 15m market at 0.55 or better"
- "Close my position in the 5m market"
- "What are my positions and total profit this week?"
- "Cancel all my orders"

Without a wallet key the server works read-only: markets, books, trades and any wallet's public stats.

## Install

The easiest way needs only [Node.js](https://nodejs.org) 18 or newer: the client runs `npx -y @lucyd/mcp`, which downloads the binary for your system on first run and checks its SHA-256. Nothing else to install, see the configs below.

Without Node.js, download the binary for your system from [Releases](https://github.com/lucyd-trade/mcp-server/releases) and put it anywhere, for example `~/bin/lucyd-mcp` (`lucyd-mcp.exe` on Windows). On macOS and Linux make it executable with `chmod +x lucyd-mcp`. Then use its path instead of `npx` in the configs below.

Or build from source with [Rust](https://rustup.rs):

```bash
cargo install --git https://github.com/lucyd-trade/mcp-server
```

## Connect your agent

Replace the key with your trading wallet key. Restart the client afterwards.

**Claude Code**

```bash
claude mcp add lucyd -e LUCYD_PRIVATE_KEY=0xYOUR_KEY -- npx -y @lucyd/mcp
```

**Claude Desktop**: edit `claude_desktop_config.json` (macOS: `~/Library/Application Support/Claude/`, Windows: `%APPDATA%\Claude\`).

**Cursor**: edit `~/.cursor/mcp.json`.

Both use the same format:

```json
{
  "mcpServers": {
    "lucyd": {
      "command": "npx",
      "args": ["-y", "@lucyd/mcp"],
      "env": {
        "LUCYD_PRIVATE_KEY": "0xYOUR_KEY"
      }
    }
  }
}
```

If the client on Windows cannot start `npx`, use `"command": "cmd"` with `"args": ["/c", "npx", "-y", "@lucyd/mcp"]`.

## Before the first trade

The wallet needs USDC to buy and a little Sepolia ETH to pay gas for one-time approvals and redemptions. Then ask the agent to check your wallet and approve trading, or call `approve_trading` directly. Trading itself costs no gas: the exchange settles fills on chain.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `LUCYD_PRIVATE_KEY` | none | Wallet key that signs orders and transactions. Without it trading tools are off |
| `LUCYD_MAX_ORDER_USD` | `100` | Largest buy order in USDC |
| `LUCYD_READ_ONLY` | `false` | `true` turns off every tool that trades or sends transactions |
| `LUCYD_RPC_URL` | `https://ethereum-sepolia-rpc.publicnode.com` | Sepolia RPC node |
| `LUCYD_API_URL` | `https://api.testnet.lucyd.trade` | Markets API |
| `LUCYD_CLOB_URL` | `https://clob.testnet.lucyd.trade` | Order book |

## Safety

- The key never leaves your machine. The server signs locally and sends only signed orders and transactions.
- Use a separate wallet for the agent and keep only what you are ready to trade in it.
- The server tells the agent to ask you before trading, but that is up to the agent. Set `LUCYD_MAX_ORDER_USD` low, or `LUCYD_READ_ONLY=true` if you only want data.

## How it works

The server runs locally and talks to the agent over stdio. Market data comes from the public [API](https://api.testnet.lucyd.trade). Orders are signed with EIP-712 and sent to the order book over WebSocket (protocol in the [API docs](https://testnet.lucyd.trade)). Approvals and redemptions are plain transactions on Sepolia.

| Contract | Address |
|---|---|
| Exchange | `0xa2a6ee54609246c52dfa2a82cdf7886a40f7ceba` |
| ConditionalTokens (outcome tokens) | `0x275C24F2e3942B70d7dce5b655696577121773F1` |
| USDC (6 decimals) | `0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238` |
| AutoRedeemer | `0x7B8f86A503dEB47A51B672D435aD9610A571502a` |

## License

MIT
