# tokenized_warehouse_receipts

#### ***📦 Tokenized Warehouse Receipts on Solana***

#### ***🌐 Overview***

The tokenized_warehouse_receipts program is a Solana smart contract that brings commodities and futures trading onto the blockchain. It models a system where certified warehouses can issue tokenized receipts for physical goods, which can then be used in derivative contracts such as futures deals.

At a high level:

- 🏭 Warehouses certify and tokenize real-world assets as SPL tokens (receipts).

- 🏦 Markets define the trading environment, including fee structures and oracle authorities.

- 🤝 Deals represent futures contracts between a long and a short party, which can be settled either in cash or physically.

This program ensures transparent settlement, automated margin handling, and programmable trust around commodity-backed receipts.

⚠️ Note: This is a proof of concept developed using Solana Playground. The next version will be exported and maintained in VS Code for production readiness.

---

#### ***🏛️ Program Architecture***
The program is structured around three main lifecycles:

#### ***1. Market Lifecycle***
- **init_market 🏁**  
  Creates a new market. Defines the authority, the `quote_mint` (e.g., USDC), the `receipt_mint` (commodity token), a fee structure (`fee_bps`), and an `oracle_authority`.

- **post_price 📈**  
  Allows either the oracle or the market authority to publish a settlement price and timestamp. This is crucial for cash settlement of deals.

#### ***2. Warehouse Lifecycle***
- **init_warehouse 🏬**  
  Registers a certified warehouse. The warehouse authority hands over minting rights of the receipt mint to a PDA (Program-Derived Address), ensuring trustless issuance.

- **mint_receipt 🎟️**  
  Lets the warehouse authority mint new receipt tokens (backed by real-world goods).

- **burn_receipt 🔥**  
  Optional helper for burning receipts upon physical redemption of the goods.

#### ***3. Futures (Deal) Lifecycle***
- **open_deal 📜**  
  Creates a futures contract between a long and short party. Parameters include:  
  - `deal_id`  
  - `strike_price`  
  - `qty_receipt_amount`  
  - `settle_ts`  
  - `settlement_kind` (cash or physical)  
  - initial margins for long and short  

- **deposit_margin 💰**  
  Lets long or short add extra collateral during the lifetime of a deal.

- **settle_cash 💵**  
  Cash settlement of a deal. Uses `market.last_price` to calculate PnL (profit and loss). Automatically transfers winnings, fees, and returns remaining margins.

- **settle_physical 🚚**  
  Physical settlement. The short delivers receipt tokens to the long and receives strike price × quantity in quote tokens. Margins are reconciled afterward.

---
