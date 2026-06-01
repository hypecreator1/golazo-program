use anchor_lang::prelude::*;
use anchor_lang::solana_program::keccak;
use anchor_lang::solana_program::pubkey; // brings the `pubkey!` macro into scope
use anchor_spl::token::{burn, Burn, Mint, Token, TokenAccount};

// Deployed program id (mainnet). MUST equal the on-chain address, else every
// instruction fails with DeclaredProgramIdMismatch (0x1004) AND a verified
// build would mismatch (declare_id is baked into the binary).
declare_id!("8o9XyJy6jvPfSmABmVdHyenjBFx83tTbgWziz8QMS3BC");

/// SlotHashes sysvar address (on-chain entropy source). Hard-coded so the build
/// never depends on a version-specific sysvar module path.
const SLOT_HASHES_ID: Pubkey = pubkey!("SysvarS1otHashes111111111111111111111111111");

/// Golazo Box — trustless burn engine.
///
/// The fairness-critical decision ("which rarity did I pull?") happens fully
/// on-chain: the user burns $GOLAZO, the program rolls a tier from on-chain
/// entropy (SlotHashes), records an immutable `Pull` account, and emits an
/// event. The backend mints the matching compressed-NFT for the already-decided
/// tier (it cannot change the outcome). A `RedeemRecord` PDA makes physical
/// redemption single-use.
///
/// Money recovery: `close_pull` (user refund) and `close_config` (admin,
/// decommission) refund account rent. The program stays UPGRADEABLE (never
/// `--final`) so program/buffer rent is reclaimable via the CLI. All SOL is
/// withdrawable.
///
/// The $GOLAZO mint is set AFTER deploy via `set_mint`, so the program can ship
/// before the token launches.
#[program]
pub mod golazo {
    use super::*;

    pub const NUM_TIERS: usize = 8;

    /// One-time global setup. Admin = signer. Mint left unset until token launch.
    pub fn initialize(
        ctx: Context<Initialize>,
        treasury: Pubkey,
        min_burn: u64,
        odds: [u16; NUM_TIERS],
    ) -> Result<()> {
        require!(odds.iter().any(|w| *w > 0), GolazoError::BadOdds);
        let cfg = &mut ctx.accounts.config;
        cfg.admin = ctx.accounts.admin.key();
        cfg.golazo_mint = Pubkey::default(); // set later via set_mint
        cfg.treasury = treasury;
        cfg.min_burn = min_burn;
        cfg.odds = odds;
        cfg.burn_count = 0;
        cfg.total_burned = 0;
        cfg.paused = false;
        cfg.bump = ctx.bumps.config;
        Ok(())
    }

    /// Wire the real $GOLAZO mint once the token is launched (admin only).
    pub fn set_mint(ctx: Context<AdminOnly>, mint: Pubkey) -> Result<()> {
        ctx.accounts.config.golazo_mint = mint;
        msg!("golazo_mint set: {}", mint);
        Ok(())
    }

    /// Tune economics (admin only).
    pub fn set_params(
        ctx: Context<AdminOnly>,
        min_burn: u64,
        odds: [u16; NUM_TIERS],
        treasury: Pubkey,
    ) -> Result<()> {
        require!(odds.iter().any(|w| *w > 0), GolazoError::BadOdds);
        let cfg = &mut ctx.accounts.config;
        cfg.min_burn = min_burn;
        cfg.odds = odds;
        cfg.treasury = treasury;
        Ok(())
    }

    /// Emergency pause / resume of burns (admin only).
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        Ok(())
    }

    /// Hand admin to a new key (admin only).
    pub fn set_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.config.admin = new_admin;
        Ok(())
    }

    /// Burn $GOLAZO and trustlessly roll a rarity tier.
    pub fn burn_for_pull(ctx: Context<BurnForPull>, amount: u64) -> Result<()> {
        // read scalars into locals BEFORE any CPI (guide pitfall #8)
        let cfg_key = ctx.accounts.config.key();
        let paused = ctx.accounts.config.paused;
        let configured_mint = ctx.accounts.config.golazo_mint;
        let min_burn = ctx.accounts.config.min_burn;
        let odds = ctx.accounts.config.odds;
        let nonce = ctx.accounts.config.burn_count;
        let user_key = ctx.accounts.user.key();

        require!(!paused, GolazoError::Paused);
        require!(configured_mint != Pubkey::default(), GolazoError::MintNotSet);
        require_keys_eq!(ctx.accounts.mint.key(), configured_mint, GolazoError::WrongMint);
        require!(amount >= min_burn, GolazoError::BelowMinimum);

        // burn the tokens (CPI)
        let cpi = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.mint.to_account_info(),
                from: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        );
        burn(cpi, amount)?;

        // on-chain entropy from SlotHashes sysvar
        let entropy = {
            let data = ctx.accounts.recent_slothashes.try_borrow_data()?;
            // layout: [u64 num][ (u64 slot, [u8;32] hash) ... ]; first hash at [16..48]
            require!(data.len() >= 48, GolazoError::NoEntropy);
            let mut h = [0u8; 32];
            h.copy_from_slice(&data[16..48]);
            h
        };
        let clock = Clock::get()?;
        let slot_bytes = clock.slot.to_le_bytes();
        let ts_bytes = clock.unix_timestamp.to_le_bytes();
        let nonce_bytes = nonce.to_le_bytes();
        let amount_bytes = amount.to_le_bytes();
        let seed = keccak::hashv(&[
            entropy.as_ref(),
            slot_bytes.as_ref(),
            ts_bytes.as_ref(),
            user_key.as_ref(),
            nonce_bytes.as_ref(),
            amount_bytes.as_ref(),
        ]);
        let roll = u64::from_le_bytes(seed.0[0..8].try_into().unwrap());

        let luck = luck_bonus(amount, min_burn);
        let tier = weighted_tier(&odds, luck, roll);

        // record the immutable pull
        let pull = &mut ctx.accounts.pull;
        pull.authority = user_key;
        pull.tier = tier;
        pull.amount = amount;
        pull.nonce = nonce;
        pull.roll = roll;
        pull.timestamp = clock.unix_timestamp;
        pull.bump = ctx.bumps.pull;

        // update global stats
        let cfg = &mut ctx.accounts.config;
        cfg.burn_count = cfg.burn_count.checked_add(1).ok_or(GolazoError::Overflow)?;
        cfg.total_burned = cfg
            .total_burned
            .checked_add(amount as u128)
            .ok_or(GolazoError::Overflow)?;

        emit!(PullRolled {
            config: cfg_key,
            authority: user_key,
            tier,
            amount,
            nonce,
            roll,
        });
        Ok(())
    }

    /// Mark a card (cNFT asset id) redeemed-for-physical. Single-use: the PDA
    /// can only be initialized once, so a second redeem of the same asset fails
    /// at the account layer. Intentionally NOT closeable (closing would re-open
    /// a second redeem).
    pub fn redeem(ctx: Context<Redeem>, asset_id: [u8; 32]) -> Result<()> {
        let rec = &mut ctx.accounts.redeem_record;
        rec.claimant = ctx.accounts.claimant.key();
        rec.asset_id = asset_id;
        rec.timestamp = Clock::get()?.unix_timestamp;
        rec.bump = ctx.bumps.redeem_record;
        emit!(Redeemed { claimant: rec.claimant, asset_id });
        Ok(())
    }

    /// Close a Pull and refund its rent to the original user.
    pub fn close_pull(_ctx: Context<ClosePull>) -> Result<()> {
        Ok(())
    }

    /// Close the global Config and refund rent to admin (decommission, admin only).
    pub fn close_config(_ctx: Context<CloseConfig>) -> Result<()> {
        Ok(())
    }
}

/// 0..=200 bonus weight added to higher tiers as the burn grows past the min.
fn luck_bonus(amount: u64, min_burn: u64) -> u32 {
    if min_burn == 0 || amount <= min_burn {
        return 0;
    }
    let ratio = amount / min_burn;
    let mut bonus = 0u32;
    let mut r = ratio;
    while r > 1 && bonus < 200 {
        r >>= 1;
        bonus += 20;
    }
    bonus
}

/// Weighted pick over 8 tiers; `luck` nudges weight toward higher-index tiers.
fn weighted_tier(odds: &[u16; 8], luck: u32, roll: u64) -> u8 {
    let mut weights = [0u64; 8];
    let mut total = 0u64;
    for i in 0..8 {
        let w = odds[i] as u64 + (luck as u64 * i as u64) / 8;
        weights[i] = w;
        total += w;
    }
    if total == 0 {
        return 0;
    }
    let mut target = roll % total;
    for i in 0..8 {
        if target < weights[i] {
            return i as u8;
        }
        target -= weights[i];
    }
    7
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = admin, space = 8 + Config::SIZE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Account<'info, Config>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct BurnForPull<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut, address = config.golazo_mint @ GolazoError::WrongMint)]
    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = user_token_account.mint == mint.key() @ GolazoError::WrongMint,
        constraint = user_token_account.owner == user.key() @ GolazoError::WrongOwner
    )]
    pub user_token_account: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = user,
        space = 8 + Pull::SIZE,
        seeds = [b"pull", user.key().as_ref(), &config.burn_count.to_le_bytes()],
        bump
    )]
    pub pull: Account<'info, Pull>,

    /// CHECK: validated by address == SlotHashes sysvar; read-only entropy source
    #[account(address = SLOT_HASHES_ID)]
    pub recent_slothashes: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(asset_id: [u8; 32])]
pub struct Redeem<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut)]
    pub claimant: Signer<'info>,

    #[account(
        init,
        payer = claimant,
        space = 8 + RedeemRecord::SIZE,
        seeds = [b"redeem", asset_id.as_ref()],
        bump
    )]
    pub redeem_record: Account<'info, RedeemRecord>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClosePull<'info> {
    #[account(
        mut,
        close = authority,
        has_one = authority,
        seeds = [b"pull", authority.key().as_ref(), &pull.nonce.to_le_bytes()],
        bump = pull.bump
    )]
    pub pull: Account<'info, Pull>,
    #[account(mut)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct CloseConfig<'info> {
    #[account(mut, close = admin, seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub admin: Signer<'info>,
}

#[account]
pub struct Config {
    pub admin: Pubkey,       // 32
    pub golazo_mint: Pubkey, // 32 (zero until set_mint)
    pub treasury: Pubkey,    // 32
    pub min_burn: u64,       // 8
    pub burn_count: u64,     // 8 (also the pull nonce)
    pub total_burned: u128,  // 16
    pub odds: [u16; 8],      // 16
    pub paused: bool,        // 1
    pub bump: u8,            // 1
}
impl Config {
    pub const SIZE: usize = 32 + 32 + 32 + 8 + 8 + 16 + 16 + 1 + 1;
}

#[account]
pub struct Pull {
    pub authority: Pubkey, // 32
    pub tier: u8,          // 1
    pub amount: u64,       // 8
    pub nonce: u64,        // 8
    pub roll: u64,         // 8
    pub timestamp: i64,    // 8
    pub bump: u8,          // 1
}
impl Pull {
    pub const SIZE: usize = 32 + 1 + 8 + 8 + 8 + 8 + 1;
}

#[account]
pub struct RedeemRecord {
    pub claimant: Pubkey,   // 32
    pub asset_id: [u8; 32], // 32
    pub timestamp: i64,     // 8
    pub bump: u8,           // 1
}
impl RedeemRecord {
    pub const SIZE: usize = 32 + 32 + 8 + 1;
}

#[event]
pub struct PullRolled {
    pub config: Pubkey,
    pub authority: Pubkey,
    pub tier: u8,
    pub amount: u64,
    pub nonce: u64,
    pub roll: u64,
}

#[event]
pub struct Redeemed {
    pub claimant: Pubkey,
    pub asset_id: [u8; 32],
}

#[error_code]
pub enum GolazoError {
    #[msg("Burns are paused")]
    Paused,
    #[msg("$GOLAZO mint not set yet")]
    MintNotSet,
    #[msg("Wrong mint")]
    WrongMint,
    #[msg("Token account owner mismatch")]
    WrongOwner,
    #[msg("Below minimum burn")]
    BelowMinimum,
    #[msg("Odds must have at least one non-zero weight")]
    BadOdds,
    #[msg("Could not read entropy")]
    NoEntropy,
    #[msg("Arithmetic overflow")]
    Overflow,
}
