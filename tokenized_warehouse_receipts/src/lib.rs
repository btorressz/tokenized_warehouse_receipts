use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{
    self, Burn, Mint, MintTo, SetAuthority, Token, TokenAccount, Transfer,
};

// Program ID
declare_id!("programid");

// ==========
// Constants
// ==========
const VERSION: u8 = 1;
const BPS_DENOMINATOR: u64 = 10_000;

#[program]
pub mod tokenized_warehouse_receipts {
    use super::*;

    // --- Market lifecycle ---
    pub fn init_market(
        ctx: Context<InitMarket>,
        fee_bps: u16,
        oracle_authority: Pubkey,
    ) -> Result<()> {
        require!(fee_bps <= 1000, ErrorCode::FeeTooHigh); // <= 10%
        let market = &mut ctx.accounts.market;
        market.version = VERSION;
        market.authority = ctx.accounts.authority.key();
        market.quote_mint = ctx.accounts.quote_mint.key();
        market.receipt_mint = ctx.accounts.receipt_mint.key();
        market.oracle_authority = oracle_authority;
        market.fee_bps = fee_bps;
        market.is_paused = false;
        market.last_price = 0;
        market.price_exponent = -6; // default 6 decimals (e.g., USDC quote)
        market.settle_ts = 0;
        Ok(())
    }

    /// Oracle or market authority posts a settlement price to be used for cash settlement.
    pub fn post_price(ctx: Context<PostPrice>, price: u64, exponent: i32, settle_ts: i64) -> Result<()> {
        let market = &mut ctx.accounts.market;
        // Only oracle authority or market authority may post
        let signer = ctx.accounts.poster.key();
        require!(
            signer == market.oracle_authority || signer == market.authority,
            ErrorCode::Unauthorized
        );
        market.last_price = price;
        market.price_exponent = exponent;
        market.settle_ts = settle_ts; // optional: per-market default settlement timestamp
        Ok(())
    }

    // --- Warehouse lifecycle ---
    /// Register a certified warehouse for this market and hand the Mint authority
    /// of the receipt mint to the program PDA so future `mint_receipt` calls are controlled.
    pub fn init_warehouse(ctx: Context<InitWarehouse>) -> Result<()> {
        // Manual equality checks (clearer compiler errors vs. has_one attrs)
        require_keys_eq!(
            ctx.accounts.market.receipt_mint,
            ctx.accounts.receipt_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.market.quote_mint,
            ctx.accounts.quote_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.market.authority,
            ctx.accounts.authority.key(),
            ErrorCode::ConstraintMismatch
        );

        let warehouse = &mut ctx.accounts.warehouse;
        warehouse.market = ctx.accounts.market.key();
        warehouse.authority = ctx.accounts.warehouse_authority.key();
        warehouse.receipt_mint = ctx.accounts.receipt_mint.key();
        // Anchor 0.28+: typed bumps struct
        warehouse.bump = ctx.bumps.receipt_mint_auth;

        // Transfer mint authority to PDA (receipt_mint_auth)
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

        Ok(())
    }

    /// Mint receipt tokens, callable only by the certified warehouse authority.
    pub fn mint_receipt(ctx: Context<MintReceipt>, amount: u64) -> Result<()> {
        // Manual equality checks
        require_keys_eq!(
            ctx.accounts.warehouse.market,
            ctx.accounts.market.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.warehouse.authority,
            ctx.accounts.warehouse_authority.key(),
            ErrorCode::Unauthorized
        );
        require_keys_eq!(
            ctx.accounts.warehouse.receipt_mint,
            ctx.accounts.receipt_mint.key(),
            ErrorCode::ConstraintMismatch
        );

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
        Ok(())
    }

    /// Burn receipt tokens when a physical redemption occurs (optional helper).
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
        )
    }

    // --- Futures (Deal) lifecycle ---
    pub fn open_deal(
        ctx: Context<OpenDeal>,
        deal_id: u64,
        strike_price: u64,        // quote per 1.0 receipt unit, using market.price_exponent decimals
        qty_receipt_amount: u64,  // amount of receipt tokens (in mint decimals) to settle
        settle_ts: i64,
        settlement_kind: SettlementKind,
        initial_margin_long: u64,
        initial_margin_short: u64,
    ) -> Result<()> {
        let market = &ctx.accounts.market;
        require!(!market.is_paused, ErrorCode::MarketPaused);
        require!(
            settle_ts > Clock::get()?.unix_timestamp,
            ErrorCode::InvalidSettlementTime
        );

        let deal = &mut ctx.accounts.deal;
        deal.version = VERSION;
        deal.market = market.key();
        deal.deal_id = deal_id;
        deal.long = ctx.accounts.long.key();
        deal.short = ctx.accounts.short.key();
        deal.quote_mint = market.quote_mint;
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
        // Typed bumps
        deal.bump = ctx.bumps.deal;
        deal.vault_bump = ctx.bumps.vault_auth;

        // Create and fund margin vaults
        // Transfer initial margin from long
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
            deal.long_margin = deal
                .long_margin
                .checked_add(initial_margin_long)
                .ok_or(ErrorCode::MathOverflow)?;
        }
        // Transfer initial margin from short
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
            deal.short_margin = deal
                .short_margin
                .checked_add(initial_margin_short)
                .ok_or(ErrorCode::MathOverflow)?;
        }

        Ok(())
    }

    pub fn deposit_margin(ctx: Context<DepositMargin>, side: Side, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        let deal = &mut ctx.accounts.deal;
        match side {
            Side::Long => {
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
            }
            Side::Short => {
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
            }
        }
        Ok(())
    }

    /// Cash settlement based on market.last_price. Transfers PnL in quote tokens.
    pub fn settle_cash(ctx: Context<SettleCash>) -> Result<()> {
        // Manual equality checks
        require_keys_eq!(
            ctx.accounts.deal.market,
            ctx.accounts.market.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.market.quote_mint,
            ctx.accounts.quote_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.market.receipt_mint,
            ctx.accounts.receipt_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.deal.quote_mint,
            ctx.accounts.quote_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.deal.receipt_mint,
            ctx.accounts.receipt_mint.key(),
            ErrorCode::ConstraintMismatch
        );

        let market = &ctx.accounts.market;
        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_settled, ErrorCode::AlreadySettled);
        require!(
            deal.settlement_kind == SettlementKind::Cash as u8,
            ErrorCode::WrongSettlementKind
        );
        let now = Clock::get()?.unix_timestamp;
        require!(now >= deal.settle_ts, ErrorCode::TooEarlyToSettle);
        require!(market.last_price > 0, ErrorCode::NoSettlementPrice);

        let strike = deal.strike_price as i128;
        let final_price = market.last_price as i128;
        let qty = deal.qty_receipt_amount as i128;
        // price * qty / 10^abs(exponent)
        let pnl_long: i128 =
            (final_price - strike) * qty / int_pow10_i128(deal.price_exponent.abs() as u32);

        // Cache deal key & bump to avoid borrow checker conflicts
        let deal_key = deal.key();
        let vault_bump = deal.vault_bump;

        // Apply fee on winner's payout
        let fee_bps = deal.fee_bps as i128;
        if pnl_long > 0 {
            let fee = pnl_long * fee_bps / BPS_DENOMINATOR as i128;
            let amount = (pnl_long - fee) as u64;
            transfer_signed(
                &ctx.accounts.token_program,
                &ctx.accounts.short_margin_vault,
                &ctx.accounts.long_receive_quote_ata,
                &ctx.accounts.vault_auth,
                &deal_key,
                vault_bump,
                amount,
            )?;
            if fee > 0 {
                transfer_signed(
                    &ctx.accounts.token_program,
                    &ctx.accounts.short_margin_vault,
                    &ctx.accounts.fee_vault,
                    &ctx.accounts.vault_auth,
                    &deal_key,
                    vault_bump,
                    fee as u64,
                )?;
            }
        } else if pnl_long < 0 {
            let pnl_short = -pnl_long;
            let fee = pnl_short * fee_bps / BPS_DENOMINATOR as i128;
            let amount = (pnl_short - fee) as u64;
            transfer_signed(
                &ctx.accounts.token_program,
                &ctx.accounts.long_margin_vault,
                &ctx.accounts.short_receive_quote_ata,
                &ctx.accounts.vault_auth,
                &deal_key,
                vault_bump,
                amount,
            )?;
            if fee > 0 {
                transfer_signed(
                    &ctx.accounts.token_program,
                    &ctx.accounts.long_margin_vault,
                    &ctx.accounts.fee_vault,
                    &ctx.accounts.vault_auth,
                    &deal_key,
                    vault_bump,
                    fee as u64,
                )?;
            }
        }

        // Return remaining margins to parties
        let long_remaining = ctx.accounts.long_margin_vault.amount;
        if long_remaining > 0 {
            transfer_signed(
                &ctx.accounts.token_program,
                &ctx.accounts.long_margin_vault,
                &ctx.accounts.long_receive_quote_ata,
                &ctx.accounts.vault_auth,
                &deal_key,
                vault_bump,
                long_remaining,
            )?;
        }
        let short_remaining = ctx.accounts.short_margin_vault.amount;
        if short_remaining > 0 {
            transfer_signed(
                &ctx.accounts.token_program,
                &ctx.accounts.short_margin_vault,
                &ctx.accounts.short_receive_quote_ata,
                &ctx.accounts.vault_auth,
                &deal_key,
                vault_bump,
                short_remaining,
            )?;
        }

        deal.is_settled = true;
        Ok(())
    }

    /// Physical settlement: short delivers receipt tokens to long, and receives
    /// strike * qty in quote tokens from long's margin vault.
    pub fn settle_physical(ctx: Context<SettlePhysical>) -> Result<()> {
        // Manual equality checks
        require_keys_eq!(
            ctx.accounts.deal.quote_mint,
            ctx.accounts.quote_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.deal.receipt_mint,
            ctx.accounts.receipt_mint.key(),
            ErrorCode::ConstraintMismatch
        );
        require_keys_eq!(
            ctx.accounts.deal.market,
            ctx.accounts.market.key(),
            ErrorCode::ConstraintMismatch
        );

        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_settled, ErrorCode::AlreadySettled);
        require!(
            deal.settlement_kind == SettlementKind::Physical as u8,
            ErrorCode::WrongSettlementKind
        );
        let now = Clock::get()?.unix_timestamp;
        require!(now >= deal.settle_ts, ErrorCode::TooEarlyToSettle);

        // Cache key & bump before any transfers
        let deal_key = deal.key();
        let vault_bump = deal.vault_bump;

        // Transfer receipts from short -> long
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.short_receipt_ata.to_account_info(),
                    to: ctx.accounts.long_receipt_ata.to_account_info(),
                    authority: ctx.accounts.short.to_account_info(),
                },
            ),
            deal.qty_receipt_amount,
        )?;

        // Pay short from long margin at strike
        let pay_amount = mul_div_u128(
            deal.strike_price as u128,
            deal.qty_receipt_amount as u128,
            pow10_u128(deal.price_exponent.abs() as u32),
        ) as u64;

        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.short_receive_quote_ata,
            &ctx.accounts.vault_auth,
            &deal_key,
            vault_bump,
            pay_amount,
        )?;

        // Return remaining margins
        let long_remaining = ctx.accounts.long_margin_vault.amount;
        if long_remaining > 0 {
            transfer_signed(
                &ctx.accounts.token_program,
                &ctx.accounts.long_margin_vault,
                &ctx.accounts.long_receive_quote_ata,
                &ctx.accounts.vault_auth,
                &deal_key,
                vault_bump,
                long_remaining,
            )?;
        }
        let short_remaining = ctx.accounts.short_margin_vault.amount;
        if short_remaining > 0 {
            transfer_signed(
                &ctx.accounts.token_program,
                &ctx.accounts.short_margin_vault,
                &ctx.accounts.short_receive_quote_ata,
                &ctx.accounts.vault_auth,
                &deal_key,
                vault_bump,
                short_remaining,
            )?;
        }

        deal.is_settled = true;
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
    pub receipt_mint: Box<Account<'info, Mint>>,  // commodity receipt SPL mint
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
pub struct PostPrice<'info> {
    #[account(mut)]
    pub market: Account<'info, Market>,
    /// CHECK: authority check done in handler
    pub poster: Signer<'info>,
    pub quote_mint: Box<Account<'info, Mint>>,   // same as market
    pub receipt_mint: Box<Account<'info, Mint>>, // same as market
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

// ==========
// State
// ==========
#[account]
pub struct Market {
    pub version: u8,
    pub authority: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub oracle_authority: Pubkey,
    pub fee_bps: u16,
    pub is_paused: bool,
    pub last_price: u64,    // e.g., 123_456_789 with exponent -6 => 123456.789 quote per unit
    pub price_exponent: i32, // typically -6 for USDC-like decimals
    pub settle_ts: i64,
}
impl Market {
    pub const SIZE: usize = 1 + 32 + 32 + 32 + 32 + 2 + 1 + 8 + 4 + 8;
}

#[account]
pub struct Warehouse {
    pub market: Pubkey,
    pub authority: Pubkey,       // certified warehouse signer
    pub receipt_mint: Pubkey,
    pub bump: u8,                // for receipt_mint_auth PDA
}
impl Warehouse {
    pub const SIZE: usize = 32 + 32 + 32 + 1;
}

#[account]
pub struct Deal {
    pub version: u8,
    pub market: Pubkey,
    pub deal_id: u64,
    pub long: Pubkey,
    pub short: Pubkey,
    pub quote_mint: Pubkey,
    pub receipt_mint: Pubkey,
    pub strike_price: u64,      // price with exponent
    pub price_exponent: i32,
    pub qty_receipt_amount: u64, // in receipt mint decimals
    pub settle_ts: i64,
    pub settlement_kind: u8,     // 0=cash, 1=physical
    pub long_margin: u64,
    pub short_margin: u64,
    pub fee_bps: u16,
    pub is_settled: bool,
    pub bump: u8,        // deal PDA bump
    pub vault_bump: u8,  // vault_auth PDA bump
}
impl Deal {
    pub const SIZE: usize =
        1 + 32 + 8 + 32 + 32 + 32 + 32 + 8 + 4 + 8 + 8 + 1 + 8 + 8 + 2 + 1 + 1 + 1;
}

// ==========
// Enums & Helpers
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

// Math helpers
fn pow10_u128(p: u32) -> u128 {
    (10u128).pow(p)
}
fn mul_div_u128(a: u128, b: u128, div: u128) -> u128 {
    a.saturating_mul(b) / div
}
fn int_pow10_i128(p: u32) -> i128 {
    (10i128).pow(p)
}

// CPI helper with unified lifetime to satisfy Anchor invariance rules.
fn transfer_signed<'info>(
    token_program: &Program<'info, Token>,
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    vault_auth: &UncheckedAccount<'info>,
    deal_key: &Pubkey,
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
            &[&[b"vault_auth", deal_key.as_ref(), &[vault_bump]]],
        ),
        amount,
    )
}

// ==========
// Errors
// ==========
#[error_code]
pub enum ErrorCode {
    #[msg("Fee too high")]
    FeeTooHigh,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Market is paused")]
    MarketPaused,
    #[msg("Invalid settlement time")]
    InvalidSettlementTime,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Zero amount not allowed")]
    ZeroAmount,
    #[msg("Already settled")]
    AlreadySettled,
    #[msg("Wrong settlement kind for this instruction")]
    WrongSettlementKind,
    #[msg("Too early to settle")]
    TooEarlyToSettle,
    #[msg("No posted settlement price")]
    NoSettlementPrice,
    #[msg("Constraint mismatch")]
    ConstraintMismatch,
}

