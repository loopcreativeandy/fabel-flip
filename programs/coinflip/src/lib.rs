//! Coinflip — bet 0.1..10 SOL on heads or tails.
//!
//! The player wins with 49% probability (the flip lands on their pick) and
//! loses with 51% probability, giving the house a 2% edge in the long run.
//! A win pays out 2x the bet (net +bet); a loss forfeits the bet to the
//! treasury.
//!
//! Randomness is a two-transaction commit/reveal against a *future* slot
//! hash: `place_bet` commits to `current_slot + COMMIT_DELAY_SLOTS`, and only
//! after that slot has been produced can `settle_bet` read its hash from the
//! SlotHashes sysvar. The outcome therefore cannot be known (or simulated)
//! when the bet is placed. Settlement is permissionless, and bets that are
//! not settled within the SlotHashes retention window forfeit to the house,
//! so a player gains nothing by refusing to settle a losing bet.
//!
//! Solvency: every accepted bet reserves its full 2x payout in
//! `Config.locked_lamports`. Bets that the treasury could not pay out are
//! rejected, and the admin can never withdraw reserved lamports, so the
//! treasury can always honor every outstanding bet.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hashv;
use anchor_lang::solana_program::sysvar::slot_hashes;
use anchor_lang::system_program::{transfer, Transfer};

declare_id!("7ffE4JF4ZNCmnxQZWFxFT3ny9VDsf3LDJ1vbUNjLspX3");

/// Minimum bet: 0.1 SOL.
pub const MIN_BET_LAMPORTS: u64 = 100_000_000;
/// Maximum bet: 10 SOL.
pub const MAX_BET_LAMPORTS: u64 = 10_000_000_000;
/// Player wins iff roll (uniform in 0..100) < 49.
pub const WIN_PERCENT: u64 = 49;
/// Slots between placing a bet and the slot whose hash decides it.
pub const COMMIT_DELAY_SLOTS: u64 = 2;
/// SlotHashes retains the most recent 512 slot hashes; after that the bet
/// can no longer be settled and forfeits to the house.
pub const SETTLE_WINDOW_SLOTS: u64 = 512;

pub const CONFIG_SEED: &[u8] = b"config";
pub const TREASURY_SEED: &[u8] = b"treasury";
pub const BET_SEED: &[u8] = b"bet";

#[program]
pub mod coinflip {
    use super::*;

    /// One-time setup: creates the config and makes the treasury PDA
    /// rent-exempt so it can never be garbage-collected.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.locked_lamports = 0;
        config.bump = ctx.bumps.config;
        config.treasury_bump = ctx.bumps.treasury;

        let rent_min = Rent::get()?.minimum_balance(0);
        let top_up = rent_min.saturating_sub(ctx.accounts.treasury.lamports());
        if top_up > 0 {
            transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.admin.to_account_info(),
                        to: ctx.accounts.treasury.to_account_info(),
                    },
                ),
                top_up,
            )?;
        }
        Ok(())
    }

    /// Anyone may add house liquidity to the treasury.
    pub fn fund_treasury(ctx: Context<FundTreasury>, amount: u64) -> Result<()> {
        transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.funder.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            amount,
        )
    }

    /// Place a bet of `amount` lamports on `choice`. The outcome is decided
    /// by the hash of slot `current_slot + COMMIT_DELAY_SLOTS`, which does
    /// not exist yet.
    pub fn place_bet(ctx: Context<PlaceBet>, nonce: u64, amount: u64, choice: Choice) -> Result<()> {
        require!(amount >= MIN_BET_LAMPORTS, CoinflipError::BetTooSmall);
        require!(amount <= MAX_BET_LAMPORTS, CoinflipError::BetTooLarge);

        let payout = amount.checked_mul(2).ok_or(CoinflipError::MathOverflow)?;

        // The treasury (before this deposit) must cover every already-locked
        // payout plus the house's exposure on this bet (payout - deposit =
        // amount). Otherwise a win could be unpayable.
        let config = &mut ctx.accounts.config;
        let rent_min = Rent::get()?.minimum_balance(0);
        let available = ctx
            .accounts
            .treasury
            .lamports()
            .checked_sub(rent_min)
            .and_then(|v| v.checked_sub(config.locked_lamports))
            .ok_or(CoinflipError::InsufficientTreasury)?;
        require!(available >= amount, CoinflipError::InsufficientTreasury);

        config.locked_lamports = config
            .locked_lamports
            .checked_add(payout)
            .ok_or(CoinflipError::MathOverflow)?;

        transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.player.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            amount,
        )?;

        let target_slot = Clock::get()?
            .slot
            .checked_add(COMMIT_DELAY_SLOTS)
            .ok_or(CoinflipError::MathOverflow)?;

        let bet = &mut ctx.accounts.bet;
        bet.player = ctx.accounts.player.key();
        bet.amount = amount;
        bet.choice = choice;
        bet.target_slot = target_slot;
        bet.nonce = nonce;
        bet.bump = ctx.bumps.bet;

        emit!(BetPlaced {
            player: bet.player,
            nonce,
            amount,
            choice,
            target_slot,
        });
        Ok(())
    }

    /// Settle a bet once its target slot has been produced. Permissionless:
    /// anyone may crank this; the payout always goes to the recorded player.
    pub fn settle_bet(ctx: Context<SettleBet>) -> Result<()> {
        let bet = &ctx.accounts.bet;
        let clock = Clock::get()?;
        require!(clock.slot > bet.target_slot, CoinflipError::TooEarly);
        let deadline = bet
            .target_slot
            .checked_add(SETTLE_WINDOW_SLOTS)
            .ok_or(CoinflipError::MathOverflow)?;
        require!(clock.slot <= deadline, CoinflipError::SettleWindowPassed);

        let slot_hash = {
            let data = ctx.accounts.slot_hashes.try_borrow_data()?;
            find_slot_hash(&data, bet.target_slot).ok_or(CoinflipError::SlotHashNotFound)?
        };

        let digest = hashv(&[
            &slot_hash,
            ctx.accounts.bet.key().as_ref(),
            &bet.target_slot.to_le_bytes(),
        ]);
        let roll = u64::from_le_bytes(digest.to_bytes()[..8].try_into().unwrap()) % 100;
        let win = roll < WIN_PERCENT;

        let payout = bet.amount.checked_mul(2).ok_or(CoinflipError::MathOverflow)?;
        let config = &mut ctx.accounts.config;
        config.locked_lamports = config
            .locked_lamports
            .checked_sub(payout)
            .ok_or(CoinflipError::MathOverflow)?;

        if win {
            let signer_seeds: &[&[&[u8]]] = &[&[TREASURY_SEED, &[config.treasury_bump]]];
            transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.system_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.treasury.to_account_info(),
                        to: ctx.accounts.player.to_account_info(),
                    },
                    signer_seeds,
                ),
                payout,
            )?;
        }

        let bet = &ctx.accounts.bet;
        emit!(BetSettled {
            player: bet.player,
            nonce: bet.nonce,
            amount: bet.amount,
            choice: bet.choice,
            result: if win { bet.choice } else { bet.choice.opposite() },
            win,
            payout: if win { payout } else { 0 },
        });
        // Bet account is closed (rent refunded to the player) via `close`.
        Ok(())
    }

    /// Forfeit a bet whose settlement window has passed. The bet lamports
    /// stay in the treasury; only the bet account rent is refunded to the
    /// player. Permissionless. (A refund here would let players settle only
    /// winning bets, so expiry must favor the house.)
    pub fn expire_bet(ctx: Context<ExpireBet>) -> Result<()> {
        let bet = &ctx.accounts.bet;
        let deadline = bet
            .target_slot
            .checked_add(SETTLE_WINDOW_SLOTS)
            .ok_or(CoinflipError::MathOverflow)?;
        require!(Clock::get()?.slot > deadline, CoinflipError::NotExpired);

        let payout = bet.amount.checked_mul(2).ok_or(CoinflipError::MathOverflow)?;
        let config = &mut ctx.accounts.config;
        config.locked_lamports = config
            .locked_lamports
            .checked_sub(payout)
            .ok_or(CoinflipError::MathOverflow)?;

        emit!(BetExpired {
            player: bet.player,
            nonce: bet.nonce,
            amount: bet.amount,
        });
        Ok(())
    }

    /// Admin withdrawal of house profit. Lamports reserved for pending
    /// payouts (and the treasury's rent minimum) can never be withdrawn.
    pub fn withdraw_house(ctx: Context<WithdrawHouse>, amount: u64) -> Result<()> {
        let rent_min = Rent::get()?.minimum_balance(0);
        let available = ctx
            .accounts
            .treasury
            .lamports()
            .checked_sub(rent_min)
            .and_then(|v| v.checked_sub(ctx.accounts.config.locked_lamports))
            .ok_or(CoinflipError::InsufficientTreasury)?;
        require!(amount <= available, CoinflipError::InsufficientTreasury);

        let signer_seeds: &[&[&[u8]]] = &[&[TREASURY_SEED, &[ctx.accounts.config.treasury_bump]]];
        transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.treasury.to_account_info(),
                    to: ctx.accounts.destination.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
        )
    }
}

/// Scan the raw SlotHashes sysvar data for `target_slot`.
/// Layout: u64 entry count, then (u64 slot, [u8; 32] hash) pairs sorted by
/// slot in descending order.
fn find_slot_hash(data: &[u8], target_slot: u64) -> Option<[u8; 32]> {
    let len = u64::from_le_bytes(data.get(..8)?.try_into().ok()?) as usize;
    for i in 0..len {
        let off = 8 + i * 40;
        let slot = u64::from_le_bytes(data.get(off..off + 8)?.try_into().ok()?);
        if slot == target_slot {
            return data.get(off + 8..off + 40)?.try_into().ok();
        }
        if slot < target_slot {
            return None; // entries are descending; target is not present
        }
    }
    None
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum Choice {
    Heads,
    Tails,
}

impl Choice {
    pub fn opposite(self) -> Self {
        match self {
            Choice::Heads => Choice::Tails,
            Choice::Tails => Choice::Heads,
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Sum of the 2x payouts of all pending bets. Invariant:
    /// treasury_lamports >= rent_min + locked_lamports.
    pub locked_lamports: u64,
    pub bump: u8,
    pub treasury_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Bet {
    pub player: Pubkey,
    pub amount: u64,
    pub choice: Choice,
    /// The bet is decided by this slot's hash.
    pub target_slot: u64,
    pub nonce: u64,
    pub bump: u8,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        init,
        payer = admin,
        space = 8 + Config::INIT_SPACE,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"treasury"], bump)]
    pub treasury: SystemAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundTreasury<'info> {
    #[account(mut)]
    pub funder: Signer<'info>,
    #[account(mut, seeds = [b"treasury"], bump)]
    pub treasury: SystemAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(nonce: u64)]
pub struct PlaceBet<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"treasury"], bump = config.treasury_bump)]
    pub treasury: SystemAccount<'info>,
    #[account(
        init,
        payer = player,
        space = 8 + Bet::INIT_SPACE,
        seeds = [b"bet", player.key().as_ref(), &nonce.to_le_bytes()],
        bump
    )]
    pub bet: Account<'info, Bet>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SettleBet<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"treasury"], bump = config.treasury_bump)]
    pub treasury: SystemAccount<'info>,
    #[account(
        mut,
        close = player,
        has_one = player,
        seeds = [b"bet", bet.player.as_ref(), &bet.nonce.to_le_bytes()],
        bump = bet.bump
    )]
    pub bet: Account<'info, Bet>,
    /// CHECK: must match `bet.player` (has_one); receives payout and rent.
    #[account(mut)]
    pub player: UncheckedAccount<'info>,
    /// CHECK: address constraint pins this to the SlotHashes sysvar.
    #[account(address = slot_hashes::ID)]
    pub slot_hashes: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExpireBet<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        close = player,
        has_one = player,
        seeds = [b"bet", bet.player.as_ref(), &bet.nonce.to_le_bytes()],
        bump = bet.bump
    )]
    pub bet: Account<'info, Bet>,
    /// CHECK: must match `bet.player` (has_one); receives the rent refund.
    #[account(mut)]
    pub player: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct WithdrawHouse<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"treasury"], bump = config.treasury_bump)]
    pub treasury: SystemAccount<'info>,
    /// CHECK: any destination the admin chooses.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event]
pub struct BetPlaced {
    pub player: Pubkey,
    pub nonce: u64,
    pub amount: u64,
    pub choice: Choice,
    pub target_slot: u64,
}

#[event]
pub struct BetSettled {
    pub player: Pubkey,
    pub nonce: u64,
    pub amount: u64,
    pub choice: Choice,
    pub result: Choice,
    pub win: bool,
    pub payout: u64,
}

#[event]
pub struct BetExpired {
    pub player: Pubkey,
    pub nonce: u64,
    pub amount: u64,
}

#[error_code]
pub enum CoinflipError {
    #[msg("Bet is below the 0.1 SOL minimum")]
    BetTooSmall,
    #[msg("Bet is above the 10 SOL maximum")]
    BetTooLarge,
    #[msg("Treasury cannot cover the potential payout")]
    InsufficientTreasury,
    #[msg("Target slot has not been produced yet")]
    TooEarly,
    #[msg("Settlement window has passed; the bet must be expired")]
    SettleWindowPassed,
    #[msg("Slot hash for the target slot is not available")]
    SlotHashNotFound,
    #[msg("Bet has not expired yet")]
    NotExpired,
    #[msg("Arithmetic overflow")]
    MathOverflow,
}
