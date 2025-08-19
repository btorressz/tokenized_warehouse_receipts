use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{
    self, Burn, Mint, MintTo, SetAuthority, Token, TokenAccount, Transfer,
};

// ProgramID
declare_id!("programid");

// ==========
// Constants
// ==========
const VERSION: u8 = 1;
const DEAL_VERSION: u8 = 1;
const BPS_DENOMINATOR: u64 = 10_000;
const MAX_COLLATERALS: usize = 4;

// ==========
// Enums
// ==========
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum SettlementKind {
    Cash = 0,
    Physical = 1,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

// ==========
// Program
// ==========
#[program]
pub mod tokenized_warehouse_receipts {
    use super::*;

    // --- Market lifecycle ---
    pub fn init_market(
        ctx: Context<InitMarket>,
        fee_bps: u16,
        oracle_authority: Pubkey,
        governance_authority: Pubkey,
        base_initial_margin_bps: u16,
        maintenance_margin_bps: u16,
        vol_multiplier_bps: u16, // scales how aggressively vol raises required margin
    ) -> Result<()> {
        require!(fee_bps <= 1000, ErrorCode::FeeTooHigh); // <= 10%
        let market = &mut ctx.accounts.market;
        market.version = VERSION;
        market.authority = ctx.accounts.authority.key();
        market.governance_authority = governance_authority;
        market.quote_mint = ctx.accounts.quote_mint.key();
        market.receipt_mint = ctx.accounts.receipt_mint.key();
        market.oracle_authority = oracle_authority;
        market.fee_bps = fee_bps;
        market.is_paused = false;
        market.last_price = 0;
        market.last_vol_bps = 0;
        market.price_exponent = -6; // default 6 decimals (e.g., USDC quote)
        market.settle_ts = 0;
        market.base_initial_margin_bps = base_initial_margin_bps;
        market.maintenance_margin_bps = maintenance_margin_bps;
        market.vol_multiplier_bps = vol_multiplier_bps;
        market.allowed_collaterals = [Pubkey::default(); MAX_COLLATERALS];
        market.allowed_count = 0;
        market.strategy_operator = Pubkey::default();

        emit!(MarketInitialized {
            market: market.key(),
            authority: market.authority,
            governance_authority,
            quote_mint: market.quote_mint,
            receipt_mint: market.receipt_mint,
            fee_bps,
        });
        Ok(())
    }

    /// Allow governance/authority to add an allowed collateral mint (multi-stable support).
    pub fn add_allowed_collateral(ctx: Context<AdminMarketWrite>, collateral_mint: Pubkey) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        let m = &mut ctx.accounts.market;
        require!(!m.is_paused, ErrorCode::MarketPaused);

        for i in 0..m.allowed_count as usize {
            if m.allowed_collaterals[i] == collateral_mint {
                return Ok(()); // already present
            }
        }
        require!((m.allowed_count as usize) < MAX_COLLATERALS, ErrorCode::TooManyCollaterals);
        let idx = m.allowed_count as usize;
        m.allowed_collaterals[idx] = collateral_mint;
        m.allowed_count += 1;
        emit!(CollateralAdded { market: m.key(), collateral_mint });
        Ok(())
    }

    pub fn remove_allowed_collateral(ctx: Context<AdminMarketWrite>, collateral_mint: Pubkey) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        let m = &mut ctx.accounts.market;
        require!(!m.is_paused, ErrorCode::MarketPaused);

        let mut idx: Option<usize> = None;
        for i in 0..m.allowed_count as usize {
            if m.allowed_collaterals[i] == collateral_mint {
                idx = Some(i);
                break;
            }
        }
        require!(idx.is_some(), ErrorCode::CollateralNotFound);
        let i = idx.unwrap();
        let last = (m.allowed_count - 1) as usize;
        m.allowed_collaterals[i] = m.allowed_collaterals[last];
        m.allowed_collaterals[last] = Pubkey::default();
        m.allowed_count -= 1;
        emit!(CollateralRemoved { market: m.key(), collateral_mint });
        Ok(())
    }

    pub fn pause_market(ctx: Context<AdminMarketWrite>) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        let m = &mut ctx.accounts.market;
        m.is_paused = true;
        emit!(MarketPaused { market: m.key() });
        Ok(())
    }

    pub fn unpause_market(ctx: Context<AdminMarketWrite>) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        let m = &mut ctx.accounts.market;
        m.is_paused = false;
        emit!(MarketUnpaused { market: m.key() });
        Ok(())
    }

    /// Oracle/authority posts price + volatility to drive dynamic margining.
    pub fn post_price(
        ctx: Context<PostPrice>,
        price: u64,
        exponent: i32,
        settle_ts: i64,
        vol_bps: u16,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let signer = ctx.accounts.poster.key();
        require!(
            signer == market.oracle_authority || signer == market.authority || signer == market.governance_authority,
            ErrorCode::Unauthorized
        );
        market.last_price = price;
        market.price_exponent = exponent;
        market.settle_ts = settle_ts;
        market.last_vol_bps = vol_bps;
        emit!(PricePosted {
            market: market.key(),
            price,
            exponent,
            settle_ts,
            vol_bps,
        });
        Ok(())
    }

    // --- Warehouse lifecycle ---
    pub fn init_warehouse(ctx: Context<InitWarehouse>) -> Result<()> {
        require_keys_eq!(ctx.accounts.market.receipt_mint, ctx.accounts.receipt_mint.key(), ErrorCode::ConstraintMismatch);
        require_keys_eq!(ctx.accounts.market.quote_mint, ctx.accounts.quote_mint.key(), ErrorCode::ConstraintMismatch);
        require_keys_eq!(ctx.accounts.market.authority, ctx.accounts.authority.key(), ErrorCode::ConstraintMismatch);

        let warehouse = &mut ctx.accounts.warehouse;
        warehouse.market = ctx.accounts.market.key();
        warehouse.authority = ctx.accounts.warehouse_authority.key();
        warehouse.receipt_mint = ctx.accounts.receipt_mint.key();
        warehouse.bump = ctx.bumps.receipt_mint_auth;

        token::set_authority(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                SetAuthority {
                    account_or_mint: ctx.accounts.receipt_mint.to_account_info(),
                    current_authority: ctx.accounts.warehouse_authority.to_account_info(),
                },
            ),
            anchor_spl::token::spl_token::instruction::AuthorityType::MintTokens,
            Some(ctx.accounts.receipt_mint_auth.key()),
        )?;

        emit!(WarehouseInitialized {
            market: warehouse.market,
            warehouse: warehouse.key(),
            warehouse_authority: warehouse.authority,
            receipt_mint: warehouse.receipt_mint,
        });
        Ok(())
    }

    pub fn mint_receipt(ctx: Context<MintReceipt>, amount: u64) -> Result<()> {
        require_keys_eq!(ctx.accounts.warehouse.market, ctx.accounts.market.key(), ErrorCode::ConstraintMismatch);
        require_keys_eq!(ctx.accounts.warehouse.authority, ctx.accounts.warehouse_authority.key(), ErrorCode::Unauthorized);
        require_keys_eq!(ctx.accounts.warehouse.receipt_mint, ctx.accounts.receipt_mint.key(), ErrorCode::ConstraintMismatch);

        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.receipt_mint.to_account_info(),
                    to: ctx.accounts.to_receipt_ata.to_account_info(),
                    authority: ctx.accounts.receipt_mint_auth.to_account_info(),
                },
                &[&[
                    b"receipt_auth",
                    ctx.accounts.warehouse.key().as_ref(),
                    &[ctx.accounts.warehouse.bump],
                ]],
            ),
            amount,
        )?;
        emit!(ReceiptMinted {
            warehouse: ctx.accounts.warehouse.key(),
            to: ctx.accounts.to_receipt_ata.owner,
            amount,
        });
        Ok(())
    }

    pub fn burn_receipt(ctx: Context<BurnReceipt>, amount: u64) -> Result<()> {
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.receipt_mint.to_account_info(),
                    from: ctx.accounts.from_receipt_ata.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
        )?;
        emit!(ReceiptBurned {
            owner: ctx.accounts.owner.key(),
            amount,
        });
        Ok(())
    }

    // --- Deal lifecycle ---
    pub fn open_deal(
        ctx: Context<OpenDeal>,
        deal_id: u64,
        deal_version: u8,
        strike_price: u64,        // quote per 1.0 receipt unit
        qty_receipt_amount: u64,  // in receipt mint decimals
        settle_ts: i64,
        settlement_kind: crate::SettlementKind,
        initial_margin_long: u64,
        initial_margin_short: u64,
    ) -> Result<()> {
        let market = &ctx.accounts.market;
        require!(!market.is_paused, ErrorCode::MarketPaused);
        require!(deal_version == DEAL_VERSION, ErrorCode::DealVersionMismatch);
        require!(settle_ts > Clock::get()?.unix_timestamp, ErrorCode::InvalidSettlementTime);
        require!(is_allowed_collateral(market, &ctx.accounts.quote_mint.key()), ErrorCode::CollateralNotAllowed);

        let deal = &mut ctx.accounts.deal;
        deal.version = VERSION;
        deal.deal_version = deal_version;
        deal.market = market.key();
        deal.deal_id = deal_id;
        deal.long = ctx.accounts.long.key();
        deal.short = ctx.accounts.short.key();
        deal.quote_mint = ctx.accounts.quote_mint.key();
        deal.receipt_mint = market.receipt_mint;
        deal.strike_price = strike_price;
        deal.price_exponent = market.price_exponent;
        deal.qty_receipt_amount = qty_receipt_amount;
        deal.settle_ts = settle_ts;
        deal.settlement_kind = settlement_kind as u8;
        deal.long_margin = 0;
        deal.short_margin = 0;
        deal.fee_bps = market.fee_bps;
        deal.is_settled = false;
        deal.is_frozen = false;
        deal.bump = ctx.bumps.deal;
        deal.vault_bump = ctx.bumps.vault_auth;

        // Margin checks (dynamic)
        let snap = MarketSnapshot::from(market);
        let required = required_initial_margin(&snap, strike_price, qty_receipt_amount);
        require!(initial_margin_long >= required && initial_margin_short >= required, ErrorCode::InsufficientInitialMargin);

        // Fund margin vaults
        if initial_margin_long > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.long_quote_ata.to_account_info(),
                        to: ctx.accounts.long_margin_vault.to_account_info(),
                        authority: ctx.accounts.long.to_account_info(),
                    },
                ),
                initial_margin_long,
            )?;
            deal.long_margin = deal.long_margin.checked_add(initial_margin_long).ok_or(ErrorCode::MathOverflow)?;
        }
        if initial_margin_short > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.short_quote_ata.to_account_info(),
                        to: ctx.accounts.short_margin_vault.to_account_info(),
                        authority: ctx.accounts.short.to_account_info(),
                    },
                ),
                initial_margin_short,
            )?;
            deal.short_margin = deal.short_margin.checked_add(initial_margin_short).ok_or(ErrorCode::MathOverflow)?;
        }

        emit!(DealOpened {
            market: market.key(),
            deal: deal.key(),
            deal_id,
            long: deal.long,
            short: deal.short,
            quote_mint: deal.quote_mint,
            receipt_mint: deal.receipt_mint,
            strike_price,
            qty_receipt_amount,
            settle_ts,
            kind: deal.settlement_kind,
            fee_bps: deal.fee_bps,
        });
        Ok(())
    }

    pub fn freeze_deal(ctx: Context<AdminDealWrite>) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        let d = &mut ctx.accounts.deal;
        d.is_frozen = true;
        emit!(DealFrozen { deal: d.key() });
        Ok(())
    }

    pub fn unfreeze_deal(ctx: Context<AdminDealWrite>) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        let d = &mut ctx.accounts.deal;
        d.is_frozen = false;
        emit!(DealUnfrozen { deal: d.key() });
        Ok(())
    }

    pub fn deposit_margin(ctx: Context<DepositMargin>, side: crate::Side, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_frozen, ErrorCode::DealFrozen);

        match side {
            crate::Side::Long => {
                require_keys_eq!(deal.long, ctx.accounts.payer.key(), ErrorCode::Unauthorized);
                token::transfer(
                    CpiContext::new(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.payer_quote_ata.to_account_info(),
                            to: ctx.accounts.long_margin_vault.to_account_info(),
                            authority: ctx.accounts.payer.to_account_info(),
                        },
                    ),
                    amount,
                )?;
                deal.long_margin = deal.long_margin.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
                emit!(MarginDeposited { deal: deal.key(), side: 0, amount });
            }
            crate::Side::Short => {
                require_keys_eq!(deal.short, ctx.accounts.payer.key(), ErrorCode::Unauthorized);
                token::transfer(
                    CpiContext::new(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.payer_quote_ata.to_account_info(),
                            to: ctx.accounts.short_margin_vault.to_account_info(),
                            authority: ctx.accounts.payer.to_account_info(),
                        },
                    ),
                    amount,
                )?;
                deal.short_margin = deal.short_margin.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
                emit!(MarginDeposited { deal: deal.key(), side: 1, amount });
            }
        }
        Ok(())
    }

    /// Cross-Margin: create a per-(market, owner, quote_mint) vault (PDA) to share margin across deals.
    pub fn cm_create(ctx: Context<CmCreate>) -> Result<()> {
        let cm = &mut ctx.accounts.cross_margin;
        cm.market = ctx.accounts.market.key();
        cm.owner = ctx.accounts.owner.key();
        cm.quote_mint = ctx.accounts.quote_mint.key();
        cm.vault_bump = ctx.bumps.cm_vault_auth;
        emit!(CrossMarginCreated {
            market: cm.market,
            owner: cm.owner,
            quote_mint: cm.quote_mint,
            vault: ctx.accounts.cm_vault_ata.key()
        });
        Ok(())
    }

    /// Deposit to cross-margin vault
    pub fn cm_deposit(ctx: Context<CmDeposit>, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.owner_quote_ata.to_account_info(),
                    to: ctx.accounts.cm_vault_ata.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
        )?;
        emit!(CrossMarginDeposited {
            market: ctx.accounts.cross_margin.market,
            owner: ctx.accounts.owner.key(),
            amount
        });
        Ok(())
    }

    /// Withdraw from cross-margin vault
    pub fn cm_withdraw(ctx: Context<CmWithdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.cm_vault_ata,
            &ctx.accounts.owner_quote_ata,
            &ctx.accounts.cm_vault_auth,
            &ctx.accounts.cross_margin.key(),
            ctx.accounts.cross_margin.vault_bump,
            amount,
        )?;
        emit!(CrossMarginWithdrawn {
            market: ctx.accounts.cross_margin.market,
            owner: ctx.accounts.owner.key(),
            amount
        });
        Ok(())
    }

    /// Move funds from Cross-Margin vault into a specific deal's margin vault (for either side).
    pub fn cm_move_to_deal(ctx: Context<CmMoveToDeal>, side: crate::Side, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        let deal = &mut ctx.accounts.deal;
        require!(deal.quote_mint == ctx.accounts.cross_margin.quote_mint, ErrorCode::ConstraintMismatch);
        require_keys_eq!(deal.market, ctx.accounts.market.key(), ErrorCode::ConstraintMismatch);
        require!(!deal.is_frozen, ErrorCode::DealFrozen);

        match side {
            crate::Side::Long => require_keys_eq!(deal.long, ctx.accounts.owner.key(), ErrorCode::Unauthorized),
            crate::Side::Short => require_keys_eq!(deal.short, ctx.accounts.owner.key(), ErrorCode::Unauthorized),
        }

        let dst = match side {
            crate::Side::Long => &ctx.accounts.long_margin_vault,
            crate::Side::Short => &ctx.accounts.short_margin_vault,
        };

        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.cm_vault_ata,
            dst,
            &ctx.accounts.cm_vault_auth,
            &ctx.accounts.cross_margin.key(),
            ctx.accounts.cross_margin.vault_bump,
            amount,
        )?;

        match side {
            crate::Side::Long => deal.long_margin = deal.long_margin.checked_add(amount).ok_or(ErrorCode::MathOverflow)?,
            crate::Side::Short => deal.short_margin = deal.short_margin.checked_add(amount).ok_or(ErrorCode::MathOverflow)?,
        }

        emit!(CrossMarginToDeal {
            deal: deal.key(),
            side: if matches!(side, crate::Side::Long) { 0 } else { 1 },
            amount
        });
        Ok(())
    }

    /// Move funds back from deal-specific margin vault to cross-margin vault.
    pub fn cm_move_from_deal(ctx: Context<CmMoveFromDeal>, side: crate::Side, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_frozen, ErrorCode::DealFrozen);
        match side {
            crate::Side::Long => require_keys_eq!(deal.long, ctx.accounts.owner.key(), ErrorCode::Unauthorized),
            crate::Side::Short => require_keys_eq!(deal.short, ctx.accounts.owner.key(), ErrorCode::Unauthorized),
        }

        let src = match side {
            crate::Side::Long => &ctx.accounts.long_margin_vault,
            crate::Side::Short => &ctx.accounts.short_margin_vault,
        };

        transfer_signed(
            &ctx.accounts.token_program,
            src,
            &ctx.accounts.cm_vault_ata,
            &ctx.accounts.vault_auth,
            &deal.key(),
            deal.vault_bump,
            amount,
        )?;

        match side {
            crate::Side::Long => deal.long_margin = deal.long_margin.checked_sub(amount).ok_or(ErrorCode::MathOverflow)?,
            crate::Side::Short => deal.short_margin = deal.short_margin.checked_sub(amount).ok_or(ErrorCode::MathOverflow)?,
        }

        emit!(DealToCrossMargin {
            deal: deal.key(),
            side: if matches!(side, crate::Side::Long) { 0 } else { 1 },
            amount
        });
        Ok(())
    }

    /// Cash settlement (full).
    pub fn settle_cash(ctx: Context<SettleCash>) -> Result<()> {
        require_keys_eq!(ctx.accounts.deal.market, ctx.accounts.market.key(), ErrorCode::ConstraintMismatch);
        let market = &ctx.accounts.market;
        require!(!ctx.accounts.deal.is_frozen, ErrorCode::DealFrozen);
        require!(!ctx.accounts.deal.is_settled, ErrorCode::AlreadySettled);
        require!(ctx.accounts.deal.settlement_kind == crate::SettlementKind::Cash as u8, ErrorCode::WrongSettlementKind);
        let now = Clock::get()?.unix_timestamp;
        require!(now >= ctx.accounts.deal.settle_ts, ErrorCode::TooEarlyToSettle);
        require!(market.last_price > 0, ErrorCode::NoSettlementPrice);

        // Immutable snapshots first (no mutable deal borrow yet)
        let ds = DealSnapshot::from(&ctx.accounts.deal);
        let ms = MarketSnapshot::from(market);

        let pnl_long = calc_pnl_long(&ds, &ms, ds.qty_receipt_amount);

        // Call helper without borrowing the whole Context
        settle_cash_inner(
            &ctx.accounts.token_program,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.long_receive_quote_ata,
            &ctx.accounts.short_receive_quote_ata,
            &ctx.accounts.fee_vault,
            &ctx.accounts.vault_auth,
            &ds,
            pnl_long,
        )?;

        // Now mutate the deal
        let deal_mut = &mut ctx.accounts.deal;
        deal_mut.is_settled = true;

        emit!(CashSettled {
            deal: ds.deal,
            final_price: ms.last_price,
            pnl_long,
        });
        Ok(())
    }

    /// Physical settlement (full).
    pub fn settle_physical(ctx: Context<SettlePhysical>) -> Result<()> {
        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_frozen, ErrorCode::DealFrozen);
        require!(!deal.is_settled, ErrorCode::AlreadySettled);
        require!(deal.settlement_kind == crate::SettlementKind::Physical as u8, ErrorCode::WrongSettlementKind);
        let now = Clock::get()?.unix_timestamp;
        require!(now >= deal.settle_ts, ErrorCode::TooEarlyToSettle);

        let ds = DealSnapshot::from(deal);

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.short_receipt_ata.to_account_info(),
                    to: ctx.accounts.long_receipt_ata.to_account_info(),
                    authority: ctx.accounts.short.to_account_info(),
                },
            ),
            ds.qty_receipt_amount,
        )?;

        let pay_amount = notional_at_strike(&ds);
        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.short_receive_quote_ata,
            &ctx.accounts.vault_auth,
            &ds.deal,
            ds.vault_bump,
            pay_amount,
        )?;

        payout_leftovers_after_settlement(&ctx.accounts.token_program, &ctx.accounts.long_margin_vault, &ctx.accounts.long_receive_quote_ata, &ctx.accounts.vault_auth, &ds)?;
        payout_leftovers_after_settlement(&ctx.accounts.token_program, &ctx.accounts.short_margin_vault, &ctx.accounts.short_receive_quote_ata, &ctx.accounts.vault_auth, &ds)?;

        deal.is_settled = true;
        emit!(PhysicalSettled {
            deal: ds.deal,
            qty_receipt_amount: ds.qty_receipt_amount,
            pay_amount,
        });
        Ok(())
    }

    /// Partial physical settlement by `amount_receipt` (<= remaining).
    pub fn settle_partial_physical(ctx: Context<SettlePhysical>, amount_receipt: u64) -> Result<()> {
        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_frozen, ErrorCode::DealFrozen);
        require!(!deal.is_settled, ErrorCode::AlreadySettled);
        require!(deal.settlement_kind == crate::SettlementKind::Physical as u8, ErrorCode::WrongSettlementKind);
        let now = Clock::get()?.unix_timestamp;
        require!(now >= deal.settle_ts, ErrorCode::TooEarlyToSettle);
        require!(amount_receipt > 0 && amount_receipt <= deal.qty_receipt_amount, ErrorCode::InvalidPartialAmount);

        let mut ds = DealSnapshot::from(deal);
        ds.qty_receipt_amount = amount_receipt;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.short_receipt_ata.to_account_info(),
                    to: ctx.accounts.long_receipt_ata.to_account_info(),
                    authority: ctx.accounts.short.to_account_info(),
                },
            ),
            amount_receipt,
        )?;

        let pay_amount = notional_at_strike(&ds);
        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.short_receive_quote_ata,
            &ctx.accounts.vault_auth,
            &ds.deal,
            ds.vault_bump,
            pay_amount,
        )?;

        deal.qty_receipt_amount = deal.qty_receipt_amount.checked_sub(amount_receipt).ok_or(ErrorCode::MathOverflow)?;
        let is_now_settled = deal.qty_receipt_amount == 0;
        deal.is_settled = is_now_settled;

        emit!(PartialPhysicalSettled {
            deal: ds.deal,
            amount_receipt,
            pay_amount,
            fully_settled: is_now_settled,
        });
        Ok(())
    }

    // --- Yield (POC) ---
    pub fn yield_set_operator(ctx: Context<AdminMarketWrite>, operator: Pubkey) -> Result<()> {
        only_admin(&ctx.accounts.market, &ctx.accounts.signer)?;
        ctx.accounts.market.strategy_operator = operator;
        emit!(YieldOperatorSet { market: ctx.accounts.market.key(), operator });
        Ok(())
    }

    pub fn yield_park_from_deal(ctx: Context<YieldPark>, side: crate::Side, amount: u64) -> Result<()> {
        only_strategy_operator(&ctx.accounts.market, &ctx.accounts.operator)?;
        let deal = &ctx.accounts.deal;
        let src = match side {
            crate::Side::Long => &ctx.accounts.long_margin_vault,
            crate::Side::Short => &ctx.accounts.short_margin_vault,
        };
        transfer_signed(
            &ctx.accounts.token_program,
            src,
            &ctx.accounts.strategy_vault_ata,
            &ctx.accounts.vault_auth,
            &deal.key(),
            deal.vault_bump,
            amount,
        )?;
        emit!(YieldParked { deal: deal.key(), side: if matches!(side, crate::Side::Long) {0} else {1}, amount });
        Ok(())
    }

    pub fn yield_unpark_to_deal(ctx: Context<YieldPark>, side: crate::Side, amount: u64) -> Result<()> {
        only_strategy_operator(&ctx.accounts.market, &ctx.accounts.operator)?;
        let deal = &ctx.accounts.deal;
        let dst = match side {
            crate::Side::Long => &ctx.accounts.long_margin_vault,
            crate::Side::Short => &ctx.accounts.short_margin_vault,
        };
        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.strategy_vault_ata,
            dst,
            &ctx.accounts.vault_auth,
            &deal.key(),
            deal.vault_bump,
            amount,
        )?;
        emit!(YieldUnparked { deal: deal.key(), side: if matches!(side, crate::Side::Long) {0} else {1}, amount });
        Ok(())
    }
}

// ==========
// Accounts
// ==========
#[derive(Accounts)]
pub struct InitMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    pub quote_mint: Box<Account<'info, Mint>>,    // e.g., USDC
    pub receipt_mint: Box<Account<'info, Mint>>,  // tokenized receipt SPL mint
    #[account(
        init,
        payer = authority,
        space = 8 + Market::SIZE,
        seeds = [b"market", authority.key().as_ref(), receipt_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump,
    )]
    pub market: Account<'info, Market>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminMarketWrite<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
}

#[derive(Accounts)]
pub struct PostPrice<'info> {
    #[account(mut)]
    pub market: Account<'info, Market>,
    /// CHECK: authority check done in handler
    pub poster: Signer<'info>,
}

#[derive(Accounts)]
pub struct InitWarehouse<'info> {
    #[account(mut)]
    pub warehouse_authority: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
    /// CHECK: Market authority (for equality check)
    pub authority: SystemAccount<'info>,
    pub quote_mint: Box<Account<'info, Mint>>,
    #[account(mut)]
    pub receipt_mint: Box<Account<'info, Mint>>,
    /// PDA that will become mint authority
    /// CHECK: PDA only used as signer for CPI; seeds enforced
    #[account(
        seeds = [b"receipt_auth", warehouse.key().as_ref()],
        bump
    )]
    pub receipt_mint_auth: UncheckedAccount<'info>,
    #[account(
        init,
        payer = warehouse_authority,
        space = 8 + Warehouse::SIZE,
        seeds = [b"warehouse", market.key().as_ref(), warehouse_authority.key().as_ref()],
        bump
    )]
    pub warehouse: Account<'info, Warehouse>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MintReceipt<'info> {
    #[account(mut)]
    pub warehouse: Account<'info, Warehouse>,
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub receipt_mint: Box<Account<'info, Mint>>,
    /// CHECK: PDA signer for mint authority
    #[account(
        seeds = [b"receipt_auth", warehouse.key().as_ref()],
        bump = warehouse.bump
    )]
    pub receipt_mint_auth: UncheckedAccount<'info>,
    #[account(mut)]
    pub to_receipt_ata: Box<Account<'info, TokenAccount>>,
    pub warehouse_authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct BurnReceipt<'info> {
    pub owner: Signer<'info>,
    #[account(mut)]
    pub receipt_mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        constraint = from_receipt_ata.mint == receipt_mint.key(),
        constraint = from_receipt_ata.owner == owner.key()
    )]
    pub from_receipt_ata: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct OpenDeal<'info> {
    #[account(mut)]
    pub market: Account<'info, Market>,

    /// Parties
    #[account(mut)]
    pub long: Signer<'info>,
    #[account(mut)]
    pub short: Signer<'info>,

    // Quote token mint and ATAs for initial margin funding
    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        constraint = long_quote_ata.owner == long.key(),
        constraint = long_quote_ata.mint == quote_mint.key()
    )]
    pub long_quote_ata: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = short_quote_ata.owner == short.key(),
        constraint = short_quote_ata.mint == quote_mint.key()
    )]
    pub short_quote_ata: Box<Account<'info, TokenAccount>>,

    // Deal account
    #[account(
        init,
        payer = long,
        space = 8 + Deal::SIZE,
        seeds = [b"deal", market.key().as_ref(), long.key().as_ref(), short.key().as_ref()],
        bump
    )]
    pub deal: Account<'info, Deal>,

    /// Margin vaults (PDAs) that hold quote tokens for each side
    #[account(
        init,
        payer = long,
        associated_token::mint = quote_mint,
        associated_token::authority = vault_auth,
    )]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = short,
        associated_token::mint = quote_mint,
        associated_token::authority = vault_auth,
    )]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    /// Vault authority PDA shared by both margin vaults
    /// CHECK: Seeds used for signing CPIs
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump
    )]
    pub vault_auth: UncheckedAccount<'info>,

    /// Fee vault owned by market account (ATA)
    #[account(
        init_if_needed,
        payer = long,
        associated_token::mint = quote_mint,
        associated_token::authority = market,
    )]
    pub fee_vault: Box<Account<'info, TokenAccount>>,

    // SPL Programs
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminDealWrite<'info> {
    pub signer: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub deal: Account<'info, Deal>,
}

#[derive(Accounts)]
pub struct DepositMargin<'info> {
    #[account(mut)]
    pub deal: Account<'info, Deal>,
    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        mut,
        constraint = payer_quote_ata.owner == payer.key(),
        constraint = payer_quote_ata.mint == quote_mint.key()
    )]
    pub payer_quote_ata: Box<Account<'info, TokenAccount>>,

    /// Vault authority and side-specific margin vaults
    /// CHECK:
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump
    )]
    pub vault_auth: UncheckedAccount<'info>,

    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct SettleCash<'info> {
    #[account(mut)]
    pub market: Account<'info, Market>,

    #[account(mut)]
    pub deal: Account<'info, Deal>,

    pub quote_mint: Box<Account<'info, Mint>>,
    pub receipt_mint: Box<Account<'info, Mint>>,

    /// CHECK: vault auth PDA
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump
    )]
    pub vault_auth: UncheckedAccount<'info>,

    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    // recipients
    #[account(mut, constraint = long_receive_quote_ata.mint == quote_mint.key())]
    pub long_receive_quote_ata: Box<Account<'info, TokenAccount>>,
    #[account(mut, constraint = short_receive_quote_ata.mint == quote_mint.key())]
    pub short_receive_quote_ata: Box<Account<'info, TokenAccount>>,

    /// Fee destination: ATA owned by market account
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = market)]
    pub fee_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct SettlePhysical<'info> {
    #[account(mut)]
    pub deal: Account<'info, Deal>,
    pub market: Account<'info, Market>,

    pub quote_mint: Box<Account<'info, Mint>>,
    pub receipt_mint: Box<Account<'info, Mint>>,

    /// CHECK: vault auth PDA
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump
    )]
    pub vault_auth: UncheckedAccount<'info>,

    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    // Parties
    #[account(mut, address = deal.long)]
    pub long: Signer<'info>,
    #[account(mut, address = deal.short)]
    pub short: Signer<'info>,

    // Receipt ATAs
    #[account(
        mut,
        constraint = long_receipt_ata.mint == receipt_mint.key(),
        constraint = long_receipt_ata.owner == long.key()
    )]
    pub long_receipt_ata: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = short_receipt_ata.mint == receipt_mint.key(),
        constraint = short_receipt_ata.owner == short.key()
    )]
    pub short_receipt_ata: Box<Account<'info, TokenAccount>>,

    // Quote recipients
    #[account(mut, constraint = long_receive_quote_ata.mint == quote_mint.key())]
    pub long_receive_quote_ata: Box<Account<'info, TokenAccount>>,
    #[account(mut, constraint = short_receive_quote_ata.mint == quote_mint.key())]
    pub short_receive_quote_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct CmCreate<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = owner,
        space = 8 + CrossMargin::SIZE,
        seeds = [b"cross_margin", market.key().as_ref(), owner.key().as_ref(), quote_mint.key().as_ref()],
        bump
    )]
    pub cross_margin: Account<'info, CrossMargin>,

    /// CHECK: PDA authority for CM vault ATA
    #[account(
        seeds = [b"cm_vault_auth", cross_margin.key().as_ref()],
        bump
    )]
    pub cm_vault_auth: UncheckedAccount<'info>,

    #[account(
        init,
        payer = owner,
        associated_token::mint = quote_mint,
        associated_token::authority = cm_vault_auth,
    )]
    pub cm_vault_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CmDeposit<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,
    #[account(mut, has_one = market, has_one = quote_mint)]
    pub cross_margin: Account<'info, CrossMargin>,
    /// CHECK: PDA authority for CM vault
    #[account(
        seeds = [b"cm_vault_auth", cross_margin.key().as_ref()],
        bump = cross_margin.vault_bump
    )]
    pub cm_vault_auth: UncheckedAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = cm_vault_auth)]
    pub cm_vault_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = owner_quote_ata.owner == owner.key(),
        constraint = owner_quote_ata.mint == quote_mint.key()
    )]
    pub owner_quote_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct CmWithdraw<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,
    #[account(mut, has_one = market, has_one = quote_mint)]
    pub cross_margin: Account<'info, CrossMargin>,
    /// CHECK
    #[account(
        seeds = [b"cm_vault_auth", cross_margin.key().as_ref()],
        bump = cross_margin.vault_bump
    )]
    pub cm_vault_auth: UncheckedAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = cm_vault_auth)]
    pub cm_vault_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = owner_quote_ata.owner == owner.key(),
        constraint = owner_quote_ata.mint == quote_mint.key()
    )]
    pub owner_quote_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct CmMoveToDeal<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,
    #[account(mut)]
    pub deal: Account<'info, Deal>,
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump = deal.vault_bump
    )]
    /// CHECK
    pub vault_auth: UncheckedAccount<'info>,

    #[account(mut, has_one = market, has_one = quote_mint)]
    pub cross_margin: Account<'info, CrossMargin>,
    /// CHECK
    #[account(
        seeds = [b"cm_vault_auth", cross_margin.key().as_ref()],
        bump = cross_margin.vault_bump
    )]
    pub cm_vault_auth: UncheckedAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = cm_vault_auth)]
    pub cm_vault_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CmMoveFromDeal<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,
    #[account(mut)]
    pub deal: Account<'info, Deal>,
    /// CHECK
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump = deal.vault_bump
    )]
    pub vault_auth: UncheckedAccount<'info>,

    #[account(mut, has_one = market, has_one = quote_mint)]
    pub cross_margin: Account<'info, CrossMargin>,
    /// CHECK
    #[account(
        seeds = [b"cm_vault_auth", cross_margin.key().as_ref()],
        bump = cross_margin.vault_bump
    )]
    pub cm_vault_auth: UncheckedAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = cm_vault_auth)]
    pub cm_vault_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct YieldPark<'info> {
    pub operator: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub deal: Account<'info, Deal>,
    /// CHECK
    #[account(
        seeds = [b"vault_auth", deal.key().as_ref()],
        bump = deal.vault_bump
    )]
    pub vault_auth: UncheckedAccount<'info>,

    pub quote_mint: Box<Account<'info, Mint>>,

    // deal margin vaults
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub long_margin_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub short_margin_vault: Box<Account<'info, TokenAccount>>,

    // strategy vault (same authority PDA for simplicity)
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = vault_auth)]
    pub strategy_vault_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

// ==========
// State
// ==========
#[account]
pub struct Market {
    pub version: u8,
    pub authority: Pubkey,
    pub governance_authority: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub oracle_authority: Pubkey,
    pub fee_bps: u16,
    pub is_paused: bool,
    pub last_price: u64,     // e.g., 123_456_789 with exponent -6 => 123456.789 quote per unit
    pub price_exponent: i32, // typically -6 for USDC-like decimals
    pub settle_ts: i64,
    // Dynamic margining
    pub base_initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub vol_multiplier_bps: u16,
    pub last_vol_bps: u16,
    // Multi-collateral
    pub allowed_collaterals: [Pubkey; MAX_COLLATERALS],
    pub allowed_count: u8,
    // Strategy operator for yield POC
    pub strategy_operator: Pubkey,
}
impl Market {
    pub const SIZE: usize =
        1 + 32 + 32 + 32 + 32 + 32 + 2 + 1 + 8 + 4 + 8 + 2 + 2 + 2 + 2 + (32 * MAX_COLLATERALS) + 1 + 32;
}

#[account]
pub struct Warehouse {
    pub market: Pubkey,
    pub authority: Pubkey,        // certified warehouse signer
    pub receipt_mint: Pubkey,
    pub bump: u8,                 // for receipt_mint_auth PDA
}
impl Warehouse { pub const SIZE: usize = 32 + 32 + 32 + 1; }

#[account]
pub struct Deal {
    pub version: u8,
    pub deal_version: u8,
    pub market: Pubkey,
    pub deal_id: u64,
    pub long: Pubkey,
    pub short: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub strike_price: u64,       // price with exponent
    pub price_exponent: i32,
    pub qty_receipt_amount: u64, // in receipt mint decimals
    pub settle_ts: i64,
    pub settlement_kind: u8,     // 0=cash, 1=physical
    pub long_margin: u64,
    pub short_margin: u64,
    pub fee_bps: u16,
    pub is_settled: bool,
    pub is_frozen: bool,
    pub bump: u8,        // deal PDA bump
    pub vault_bump: u8,  // vault_auth PDA bump
}
impl Deal {
    pub const SIZE: usize =
        1 + 1 + 32 + 8 + 32 + 32 + 32 + 32 + 8 + 4 + 8 + 8 + 1 + 8 + 8 + 2 + 1 + 1 + 1 + 1;
}

#[account]
pub struct CrossMargin {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub quote_mint: Pubkey,
    pub vault_bump: u8,
}
impl CrossMargin {
    pub const SIZE: usize = 32 + 32 + 32 + 1;
}

// ==========
// Events
// ==========
#[event]
pub struct MarketInitialized {
    pub market: Pubkey,
    pub authority: Pubkey,
    pub governance_authority: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub fee_bps: u16,
}
#[event] pub struct MarketPaused { pub market: Pubkey }
#[event] pub struct MarketUnpaused { pub market: Pubkey }
#[event] pub struct PricePosted { pub market: Pubkey, pub price: u64, pub exponent: i32, pub settle_ts: i64, pub vol_bps: u16 }
#[event] pub struct CollateralAdded { pub market: Pubkey, pub collateral_mint: Pubkey }
#[event] pub struct CollateralRemoved { pub market: Pubkey, pub collateral_mint: Pubkey }

#[event] pub struct WarehouseInitialized { pub market: Pubkey, pub warehouse: Pubkey, pub warehouse_authority: Pubkey, pub receipt_mint: Pubkey }
#[event] pub struct ReceiptMinted { pub warehouse: Pubkey, pub to: Pubkey, pub amount: u64 }
#[event] pub struct ReceiptBurned { pub owner: Pubkey, pub amount: u64 }

#[event]
pub struct DealOpened {
    pub market: Pubkey,
    pub deal: Pubkey,
    pub deal_id: u64,
    pub long: Pubkey,
    pub short: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub strike_price: u64,
    pub qty_receipt_amount: u64,
    pub settle_ts: i64,
    pub kind: u8,
    pub fee_bps: u16,
}

#[event] pub struct DealFrozen { pub deal: Pubkey }
#[event] pub struct DealUnfrozen { pub deal: Pubkey }

#[event] pub struct MarginDeposited { pub deal: Pubkey, pub side: u8, pub amount: u64 }
#[event] pub struct CashSettled { pub deal: Pubkey, pub final_price: u64, pub pnl_long: i128 }
#[event] pub struct PhysicalSettled { pub deal: Pubkey, pub qty_receipt_amount: u64, pub pay_amount: u64 }
#[event] pub struct PartialPhysicalSettled { pub deal: Pubkey, pub amount_receipt: u64, pub pay_amount: u64, pub fully_settled: bool }

#[event] pub struct CrossMarginCreated { pub market: Pubkey, pub owner: Pubkey, pub quote_mint: Pubkey, pub vault: Pubkey }
#[event] pub struct CrossMarginDeposited { pub market: Pubkey, pub owner: Pubkey, pub amount: u64 }
#[event] pub struct CrossMarginWithdrawn { pub market: Pubkey, pub owner: Pubkey, pub amount: u64 }
#[event] pub struct CrossMarginToDeal { pub deal: Pubkey, pub side: u8, pub amount: u64 }
#[event] pub struct DealToCrossMargin { pub deal: Pubkey, pub side: u8, pub amount: u64 }

#[event] pub struct YieldOperatorSet { pub market: Pubkey, pub operator: Pubkey }
#[event] pub struct YieldParked { pub deal: Pubkey, pub side: u8, pub amount: u64 }
#[event] pub struct YieldUnparked { pub deal: Pubkey, pub side: u8, pub amount: u64 }

// ==========
// Snapshots (immutable copies to avoid borrow issues)
// ==========
#[derive(Clone, Copy)]
struct DealSnapshot {
    pub deal: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub strike_price: u64,
    pub price_exponent: i32,
    pub qty_receipt_amount: u64,
    pub fee_bps: u16,
    pub vault_bump: u8,
}
impl DealSnapshot {
    fn from(d: &Account<Deal>) -> Self {
        Self {
            deal: d.key(),
            quote_mint: d.quote_mint,
            receipt_mint: d.receipt_mint,
            strike_price: d.strike_price,
            price_exponent: d.price_exponent,
            qty_receipt_amount: d.qty_receipt_amount,
            fee_bps: d.fee_bps,
            vault_bump: d.vault_bump,
        }
    }
}

#[derive(Clone, Copy)]
struct MarketSnapshot {
    pub last_price: u64,
    pub price_exponent: i32,
    pub base_initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub vol_multiplier_bps: u16,
    pub last_vol_bps: u16,
}
impl MarketSnapshot {
    fn from(m: &Account<Market>) -> Self {
        Self {
            last_price: m.last_price,
            price_exponent: m.price_exponent,
            base_initial_margin_bps: m.base_initial_margin_bps,
            maintenance_margin_bps: m.maintenance_margin_bps,
            vol_multiplier_bps: m.vol_multiplier_bps,
            last_vol_bps: m.last_vol_bps,
        }
    }
}

// ==========
// Helpers & Math
// ==========
fn only_admin(market: &Market, signer: &Signer) -> Result<()> {
    require!(
        signer.key() == market.authority || signer.key() == market.governance_authority,
        ErrorCode::Unauthorized
    );
    Ok(())
}

fn only_strategy_operator(market: &Market, operator: &Signer) -> Result<()> {
    require!(operator.key() == market.strategy_operator, ErrorCode::Unauthorized);
    Ok(())
}

fn is_allowed_collateral(market: &Market, mint: &Pubkey) -> bool {
    if *mint == market.quote_mint {
        return true;
    }
    for i in 0..market.allowed_count as usize {
        if market.allowed_collaterals[i] == *mint {
            return true;
        }
    }
    false
}

fn pow10_u128(p: u32) -> u128 { (10u128).pow(p) }
fn int_pow10_i128(p: u32) -> i128 { (10i128).pow(p) }

fn notional_at_strike(ds: &DealSnapshot) -> u64 {
    let n = (ds.strike_price as u128)
        .saturating_mul(ds.qty_receipt_amount as u128)
        / pow10_u128(ds.price_exponent.abs() as u32);
    n as u64
}

fn calc_pnl_long(ds: &DealSnapshot, ms: &MarketSnapshot, qty: u64) -> i128 {
    let strike = ds.strike_price as i128;
    let final_price = ms.last_price as i128;
    let qty_i = qty as i128;
    (final_price - strike) * qty_i / int_pow10_i128(ds.price_exponent.abs() as u32)
}

/// Dynamic initial margin requirement:
fn required_initial_margin(ms: &MarketSnapshot, strike_price: u64, qty: u64) -> u64 {
    let notional = (strike_price as u128)
        .saturating_mul(qty as u128)
        / pow10_u128(ms.price_exponent.abs() as u32);

    let vol_adj_bps = (ms.vol_multiplier_bps as u128)
        .saturating_mul(ms.last_vol_bps as u128)
        / (BPS_DENOMINATOR as u128);

    let total_bps = (ms.base_initial_margin_bps as u128)
        .saturating_add(vol_adj_bps);

    (notional.saturating_mul(total_bps) / (BPS_DENOMINATOR as u128)) as u64
}

// Transfer using PDA signer (generic lifetime to satisfy invariance)
fn transfer_signed<'info>(
    token_program: &Program<'info, Token>,
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    vault_auth: &UncheckedAccount<'info>,
    seed_key: &Pubkey,
    vault_bump: u8,
    amount: u64,
) -> Result<()> {
    token::transfer(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            Transfer {
                from: from.to_account_info(),
                to: to.to_account_info(),
                authority: vault_auth.to_account_info(),
            },
            &[&[b"vault_auth", seed_key.as_ref(), &[vault_bump]]],
        ),
        amount,
    )
}

// Return remaining funds from a vault to its party after settlement
fn payout_leftovers_after_settlement<'info>(
    token_program: &Program<'info, Token>,
    vault: &Account<'info, TokenAccount>,
    recipient: &Account<'info, TokenAccount>,
    vault_auth: &UncheckedAccount<'info>,
    ds: &DealSnapshot,
) -> Result<()> {
    let amt = vault.amount;
    if amt > 0 {
        transfer_signed(token_program, vault, recipient, vault_auth, &ds.deal, ds.vault_bump, amt)?;
    }
    Ok(())
}

// Cash settlement internal: moves PnL + fees, then returns leftovers (no &Context borrow).
fn settle_cash_inner<'info>(
    token_program: &Program<'info, Token>,
    short_margin_vault: &Account<'info, TokenAccount>,
    long_margin_vault: &Account<'info, TokenAccount>,
    long_receive_quote_ata: &Account<'info, TokenAccount>,
    short_receive_quote_ata: &Account<'info, TokenAccount>,
    fee_vault: &Account<'info, TokenAccount>,
    vault_auth: &UncheckedAccount<'info>,
    ds: &DealSnapshot,
    pnl_long: i128,
) -> Result<()> {
    let fee_bps = ds.fee_bps as i128;
    if pnl_long > 0 {
        let fee = pnl_long * fee_bps / BPS_DENOMINATOR as i128;
        let amount = (pnl_long - fee) as u64;
        transfer_signed(
            token_program,
            short_margin_vault,
            long_receive_quote_ata,
            vault_auth,
            &ds.deal,
            ds.vault_bump,
            amount,
        )?;
        if fee > 0 {
            transfer_signed(
                token_program,
                short_margin_vault,
                fee_vault,
                vault_auth,
                &ds.deal,
                ds.vault_bump,
                fee as u64,
            )?;
        }
    } else if pnl_long < 0 {
        let pnl_short = -pnl_long;
        let fee = pnl_short * fee_bps / BPS_DENOMINATOR as i128;
        let amount = (pnl_short - fee) as u64;
        transfer_signed(
            token_program,
            long_margin_vault,
            short_receive_quote_ata,
            vault_auth,
            &ds.deal,
            ds.vault_bump,
            amount,
        )?;
        if fee > 0 {
            transfer_signed(
                token_program,
                long_margin_vault,
                fee_vault,
                vault_auth,
                &ds.deal,
                ds.vault_bump,
                fee as u64,
            )?;
        }
    }

    payout_leftovers_after_settlement(
        token_program,
        long_margin_vault,
        long_receive_quote_ata,
        vault_auth,
        ds,
    )?;
    payout_leftovers_after_settlement(
        token_program,
        short_margin_vault,
        short_receive_quote_ata,
        vault_auth,
        ds,
    )?;
    Ok(())
}

// ==========
// Errors
// ==========
#[error_code]
pub enum ErrorCode {
    #[msg("Fee too high")] FeeTooHigh,
    #[msg("Unauthorized")] Unauthorized,
    #[msg("Market is paused")] MarketPaused,
    #[msg("Invalid settlement time")] InvalidSettlementTime,
    #[msg("Math overflow")] MathOverflow,
    #[msg("Zero amount not allowed")] ZeroAmount,
    #[msg("Already settled")] AlreadySettled,
    #[msg("Wrong settlement kind for this instruction")] WrongSettlementKind,
    #[msg("Too early to settle")] TooEarlyToSettle,
    #[msg("No posted settlement price")] NoSettlementPrice,
    #[msg("Constraint mismatch")] ConstraintMismatch,
    #[msg("Too many collaterals")] TooManyCollaterals,
    #[msg("Collateral not found")] CollateralNotFound,
    #[msg("Collateral mint not allowed")] CollateralNotAllowed,
    #[msg("Insufficient initial margin")] InsufficientInitialMargin,
    #[msg("Deal version mismatch")] DealVersionMismatch,
    #[msg("Deal is frozen")] DealFrozen,
    #[msg("Invalid partial amount")] InvalidPartialAmount,
}


