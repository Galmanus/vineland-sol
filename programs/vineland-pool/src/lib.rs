//! # vineland-pool
//!
//! Private authorization for public money, on Solana. The payment is public; the
//! payer is not.
//!
//! An operator approves addresses (the allowlist: KYC happens at the door, before
//! a key can enter). An approved key deposits one fixed-size note of an SPL token
//! into the pool's vault and publishes a commitment, a hash of a secret it keeps.
//! Later the note leaves the vault to a fresh account, submitted by a relayer, so
//! the depositor's key never signs the withdrawal. The chain shows
//! `relayer -> vault -> clean account`; every note is the same size, so neither
//! the amount nor the signer says which approved depositor it was. Entry is
//! attributable and auditable, exit is authorized but unlinked.
//!
//! The engine underneath is riverrun (`github.com/Galmanus/mirror-pool`): the
//! commitment, nullifier and membership root are Poseidon2-M31 digests, and
//! membership is proven with riverrun's post-quantum Circle-STARK "crowd" proofs,
//! whose public inputs carry a blinded commitment instead of the leaf.
//!
//! ## What is trusted, named
//!
//! Those STARKs are 100+ KB and one verification costs more than a Solana
//! transaction's compute cap, so this program does not run them. A committee of
//! `N` verifier keys checks the proof off-chain and `M` of them sign an Ed25519
//! attestation over the exact `(pool, root, nullifier, recipient)` being settled;
//! the program checks the signatures through the native precompile and
//! instruction introspection. What that trusts: `M` colluding verifiers could
//! approve a withdrawal no proof backs (soundness). What it does not trust: the
//! committee for privacy. The proof hides the leaf, so a verifier learns the
//! nullifier and the recipient, never which deposit it was.
//!
//! The committee is fixed when the pool opens: there is no instruction to
//! replace it, because an authority that could swap in its own keys could sign
//! every note out. Rotation means a new pool. The membership root is published
//! by the operator (a Poseidon2-M31 tree is not yet computed on-chain here), and
//! an honest verifier recomputes it from the on-chain deposit records before
//! signing, so a root over leaves nobody deposited gets no quorum. The operator
//! is trusted for liveness and for the allowlist, not for custody. Staged
//! on-chain STARK verification is the roadmap that removes the committee.
//!
//! ## Exit and withdraw are one spend
//!
//! A depositor can always reclaim their note (`ragequit`), but an exit must burn
//! the same nullifier a withdrawal would, otherwise "withdraw privately, then
//! ragequit the deposit" pays the note twice out of everyone else's money. So an
//! exit reveals the note's nullifier and carries a committee attestation that it
//! belongs to this commitment, and the nullifier PDA is the single spent-flag for
//! both paths.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};
use solana_instructions_sysvar::{load_current_index_checked, load_instruction_at_checked};
use solana_sdk_ids::ed25519_program;

declare_id!("AsQwEN7XiUPW2MGHEbEkh8UCKixwpSBfE1KKt5QVgYgx");

/// Domain separators, so an attestation for one path can never be replayed as
/// the other, or as anything else the verifier keys sign.
const WITHDRAW_DOMAIN: &[u8; 16] = b"vineland-wdrw-v1";
const EXIT_DOMAIN: &[u8; 16] = b"vineland-exit-v1";
/// domain(16) + pool(32) + root(32) + nullifier(32) + recipient(32). Binding the
/// recipient token account is what stops a relayer from redirecting the payout.
const WITHDRAW_MSG_LEN: usize = 144;
/// domain(16) + pool(32) + commitment(32) + nullifier(32).
const EXIT_MSG_LEN: usize = 112;

/// Cap on the committee size (riverrun's Wake's Four, generalized to M-of-N).
const MAX_VERIFIERS: usize = 8;
/// The protocol fee is charged on withdrawal only and capped here, so a pool
/// cannot be initialized to confiscate notes.
const MAX_FEE_BPS: u16 = 1_000;

/// Layout of a native Ed25519 instruction's data (one signature): a 16-byte
/// header, then the public key, the signature, and the message.
const ED25519_HEADER_LEN: usize = 16;
const ED25519_PUBKEY_OFFSET: usize = ED25519_HEADER_LEN;
const ED25519_SIGNATURE_OFFSET: usize = ED25519_PUBKEY_OFFSET + 32;
const ED25519_MESSAGE_OFFSET: usize = ED25519_SIGNATURE_OFFSET + 64;

#[program]
pub mod vineland_pool {
    use super::*;

    /// Open a pool for one SPL mint and one note size, with a verifier committee,
    /// a protocol fee in basis points (paid to `treasury` on withdrawal), and an
    /// anonymity floor `k_min` below which no withdrawal settles.
    pub fn initialize(
        ctx: Context<Initialize>,
        verifiers: Vec<Pubkey>,
        threshold: u8,
        note_amount: u64,
        fee_bps: u16,
        k_min: u32,
    ) -> Result<()> {
        validate_committee(&verifiers, threshold)?;
        require!(note_amount > 0, PoolError::InvalidConfig);
        require!(fee_bps <= MAX_FEE_BPS, PoolError::InvalidConfig);
        require!(fee_of(note_amount, fee_bps) < note_amount, PoolError::InvalidConfig);
        require!(k_min >= 1, PoolError::InvalidConfig);

        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.authority.key();
        pool.mint = ctx.accounts.mint.key();
        pool.treasury = ctx.accounts.treasury.key();
        pool.note_amount = note_amount;
        pool.fee_bps = fee_bps;
        pool.k_min = k_min;
        pool.verifiers = verifiers;
        pool.threshold = threshold;
        pool.membership_root = [0u8; 32];
        pool.root_set_size = 0;
        pool.leaf_count = 0;
        pool.live_notes = 0;
        pool.exits = 0;
        pool.bump = ctx.bumps.pool;
        pool.vault_bump = ctx.bumps.vault;
        Ok(())
    }

    /// Approve `member` to deposit. The approval is an account whose existence is
    /// the permission, so it is checkable by anyone and costs the operator rent.
    pub fn allow(ctx: Context<Allow>, member: Pubkey) -> Result<()> {
        ctx.accounts.allow_entry.member = member;
        ctx.accounts.allow_entry.bump = ctx.bumps.allow_entry;
        Ok(())
    }

    /// Withdraw an approval. It gates future deposits only: a note already in
    /// the pool stays withdrawable and exitable, so revoking cannot trap funds.
    pub fn revoke(_ctx: Context<Revoke>, _member: Pubkey) -> Result<()> {
        Ok(())
    }

    /// Publish the membership root withdrawals are proven against, and how many
    /// deposited leaves it covers. Authority-gated (see the module docs). The
    /// size cannot exceed the deposits that exist, so the floor below is a floor
    /// on real deposits, not on a number the operator types.
    pub fn publish_root(ctx: Context<Admin>, root: [u8; 32], set_size: u32) -> Result<()> {
        require!(root != [0u8; 32], PoolError::RootNotPublished);
        require!(is_canonical_m31(&root), PoolError::NonCanonical);
        let pool = &mut ctx.accounts.pool;
        require!(set_size <= pool.leaf_count, PoolError::InvalidSetSize);
        pool.membership_root = root;
        pool.root_set_size = set_size;
        emit!(RootPublished { pool: pool.key(), root, set_size });
        Ok(())
    }

    /// Deposit one note. Only an approved key may, and the entry is public on
    /// purpose: the depositor signs, the amount is the pool's fixed note, and the
    /// commitment is recorded against the depositor so they can always exit.
    /// A commitment can be deposited once (its record is a PDA).
    pub fn deposit(ctx: Context<Deposit>, commitment: [u8; 32]) -> Result<()> {
        require!(commitment != [0u8; 32], PoolError::InvalidCommitment);
        require!(is_canonical_m31(&commitment), PoolError::NonCanonical);
        let note = ctx.accounts.pool.note_amount;
        token::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.depositor_token.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            note,
            ctx.accounts.mint.decimals,
        )?;

        let pool = &mut ctx.accounts.pool;
        let index = pool.leaf_count;
        pool.leaf_count = index.checked_add(1).ok_or(PoolError::PoolFull)?;
        pool.live_notes = pool.live_notes.checked_add(1).ok_or(PoolError::PoolFull)?;

        let record = &mut ctx.accounts.deposit_record;
        record.pool = pool.key();
        record.depositor = ctx.accounts.depositor.key();
        record.commitment = commitment;
        record.index = index;
        record.exited = false;
        record.bump = ctx.bumps.deposit_record;

        emit!(Deposited { pool: pool.key(), commitment, index });
        Ok(())
    }

    /// Withdraw one note to `recipient`, submitted by any relayer. Requires a
    /// committee quorum over exactly `(pool, root, nullifier, recipient)`, the
    /// current root, a root covering at least `k_min` deposits, and a nullifier
    /// never spent before (its PDA is created here; a second spend finds it).
    /// Pays `note - fee` to the recipient and `fee` to the treasury.
    pub fn withdraw(ctx: Context<Withdraw>, nullifier: [u8; 32], root: [u8; 32]) -> Result<()> {
        require!(is_canonical_m31(&nullifier), PoolError::NonCanonical);
        let pool = &ctx.accounts.pool;
        require!(pool.membership_root != [0u8; 32], PoolError::RootNotPublished);
        require!(root == pool.membership_root, PoolError::RootMismatch);
        // A set of one is not an anonymity set: the payout belongs to that
        // depositor by elimination. An exit is public (it reveals which
        // commitment left), so exited notes do not count toward the set.
        require!(
            pool.root_set_size.saturating_sub(pool.exits) >= pool.k_min,
            PoolError::AnonymitySetTooSmall
        );

        verify_quorum(
            &ctx.accounts.instructions,
            &pool.verifiers,
            pool.threshold,
            &withdraw_message(&pool.key(), &root, &nullifier, &ctx.accounts.recipient.key()),
        )?;

        let note = pool.note_amount;
        let fee = fee_of(note, pool.fee_bps);
        pay_from_vault(
            pool,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.recipient.to_account_info(),
            &ctx.accounts.token_program,
            note - fee,
        )?;
        if fee > 0 {
            pay_from_vault(
                pool,
                &ctx.accounts.vault,
                &ctx.accounts.mint,
                &ctx.accounts.treasury.to_account_info(),
                &ctx.accounts.token_program,
                fee,
            )?;
        }

        let record = &mut ctx.accounts.nullifier_record;
        record.bump = ctx.bumps.nullifier_record;
        let pool = &mut ctx.accounts.pool;
        pool.live_notes = pool.live_notes.checked_sub(1).ok_or(PoolError::VaultAccounting)?;

        emit!(Withdrawn {
            pool: pool.key(),
            nullifier,
            recipient: ctx.accounts.recipient.key(),
        });
        Ok(())
    }

    /// The depositor reclaims their own note in full, to their own token
    /// account. They leave the anonymity set publicly. The exit burns the note's
    /// nullifier, with a committee attestation that the nullifier belongs to this
    /// commitment, so a note that was withdrawn cannot also be exited, and an
    /// exited note cannot also be withdrawn.
    pub fn ragequit(ctx: Context<Ragequit>, commitment: [u8; 32], nullifier: [u8; 32]) -> Result<()> {
        require!(!ctx.accounts.deposit_record.exited, PoolError::AlreadyExited);
        require!(is_canonical_m31(&nullifier), PoolError::NonCanonical);
        let pool = &ctx.accounts.pool;
        verify_quorum(
            &ctx.accounts.instructions,
            &pool.verifiers,
            pool.threshold,
            &exit_message(&pool.key(), &commitment, &nullifier),
        )?;

        pay_from_vault(
            pool,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.depositor_token.to_account_info(),
            &ctx.accounts.token_program,
            pool.note_amount,
        )?;

        ctx.accounts.deposit_record.exited = true;
        ctx.accounts.nullifier_record.bump = ctx.bumps.nullifier_record;
        let pool = &mut ctx.accounts.pool;
        pool.live_notes = pool.live_notes.checked_sub(1).ok_or(PoolError::VaultAccounting)?;
        pool.exits = pool.exits.checked_add(1).ok_or(PoolError::VaultAccounting)?;

        emit!(Exited { pool: pool.key(), commitment, nullifier });
        Ok(())
    }
}

/// The Mersenne-31 prime. Commitments, nullifiers and roots are eight M31 limbs,
/// each a u32 LE. A limb `v` and `v + P` are the same field element but
/// different bytes, so without this check one nullifier would have two PDAs and
/// one note could be spent twice.
const M31_P: u32 = (1 << 31) - 1;

pub fn is_canonical_m31(digest: &[u8; 32]) -> bool {
    digest
        .chunks_exact(4)
        .all(|limb| u32::from_le_bytes([limb[0], limb[1], limb[2], limb[3]]) < M31_P)
}

/// The fee on one note, rounded down, computed in u128 so no note size overflows.
pub fn fee_of(note_amount: u64, fee_bps: u16) -> u64 {
    ((note_amount as u128 * fee_bps as u128) / 10_000) as u64
}

/// Move `amount` out of the vault, signed by the pool PDA (the vault's
/// authority). No member key and no operator key can move a note.
fn pay_from_vault<'info>(
    pool: &Account<'info, Pool>,
    vault: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    to: &AccountInfo<'info>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let seeds: &[&[u8]] = &[b"pool", pool.authority.as_ref(), pool.mint.as_ref(), &[pool.bump]];
    token::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked {
                from: vault.to_account_info(),
                mint: mint.to_account_info(),
                to: to.clone(),
                authority: pool.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

pub fn withdraw_message(
    pool: &Pubkey,
    root: &[u8; 32],
    nullifier: &[u8; 32],
    recipient: &Pubkey,
) -> [u8; WITHDRAW_MSG_LEN] {
    let mut m = [0u8; WITHDRAW_MSG_LEN];
    m[..16].copy_from_slice(WITHDRAW_DOMAIN);
    m[16..48].copy_from_slice(pool.as_ref());
    m[48..80].copy_from_slice(root);
    m[80..112].copy_from_slice(nullifier);
    m[112..144].copy_from_slice(recipient.as_ref());
    m
}

pub fn exit_message(pool: &Pubkey, commitment: &[u8; 32], nullifier: &[u8; 32]) -> [u8; EXIT_MSG_LEN] {
    let mut m = [0u8; EXIT_MSG_LEN];
    m[..16].copy_from_slice(EXIT_DOMAIN);
    m[16..48].copy_from_slice(pool.as_ref());
    m[48..80].copy_from_slice(commitment);
    m[80..112].copy_from_slice(nullifier);
    m
}

/// At least one verifier, no more than the cap, a threshold in `1..=N`, and no
/// duplicate keys (a duplicate would let one key count twice).
fn validate_committee(verifiers: &[Pubkey], threshold: u8) -> Result<()> {
    let n = verifiers.len();
    require!((1..=MAX_VERIFIERS).contains(&n), PoolError::InvalidCommittee);
    require!(threshold >= 1 && (threshold as usize) <= n, PoolError::InvalidCommittee);
    for i in 0..n {
        for j in (i + 1)..n {
            require_keys_neq!(verifiers[i], verifiers[j], PoolError::InvalidCommittee);
        }
    }
    Ok(())
}

/// If `ix` is a self-contained native Ed25519 sigverify instruction over exactly
/// `expected`, return the signer's key. The signature itself is checked by the
/// precompile (if the transaction executes, it is valid); what is checked here is
/// that the precompile was asked about this message, with the key and message
/// inside its own data (source index `0xFFFF`) rather than read from elsewhere.
fn attestation_signer(
    ix: &anchor_lang::solana_program::instruction::Instruction,
    expected: &[u8],
) -> Option<[u8; 32]> {
    if ix.program_id != ed25519_program::ID {
        return None;
    }
    let data = &ix.data;
    let mlen = expected.len();
    if data.len() != ED25519_MESSAGE_OFFSET + mlen {
        return None;
    }
    if data[0] != 1 || data[1] != 0 {
        return None;
    }
    let field = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
    let self_contained = field(2) as usize == ED25519_SIGNATURE_OFFSET
        && field(6) as usize == ED25519_PUBKEY_OFFSET
        && field(10) as usize == ED25519_MESSAGE_OFFSET
        && field(12) as usize == mlen
        && field(4) == u16::MAX
        && field(8) == u16::MAX
        && field(14) == u16::MAX;
    if !self_contained || &data[ED25519_MESSAGE_OFFSET..] != expected {
        return None;
    }
    let mut signer = [0u8; 32];
    signer.copy_from_slice(&data[ED25519_PUBKEY_OFFSET..ED25519_PUBKEY_OFFSET + 32]);
    Some(signer)
}

/// Require attestations over `expected` from at least `threshold` distinct
/// committee members among the instructions that precede this one.
fn verify_quorum(
    instructions: &UncheckedAccount,
    verifiers: &[Pubkey],
    threshold: u8,
    expected: &[u8],
) -> Result<()> {
    let index = load_current_index_checked(instructions)?;
    let mut counted = [false; MAX_VERIFIERS];
    let mut have = 0u8;
    for i in 0..index {
        let Ok(ix) = load_instruction_at_checked(i as usize, instructions) else {
            continue;
        };
        let Some(signer) = attestation_signer(&ix, expected) else {
            continue;
        };
        if let Some(slot) = verifiers.iter().position(|v| v.as_ref() == signer) {
            if !counted[slot] {
                counted[slot] = true;
                have += 1;
            }
        }
    }
    require!(have >= threshold, PoolError::NotEnoughAttestations);
    Ok(())
}

#[account]
pub struct Pool {
    pub authority: Pubkey,
    pub mint: Pubkey,
    /// Token account (of `mint`) that receives the protocol fee.
    pub treasury: Pubkey,
    /// The fixed note. Every deposit and every payout is this size, so the
    /// amount leaving the vault says nothing about who deposited it.
    pub note_amount: u64,
    pub fee_bps: u16,
    /// Withdrawals are refused while the published root covers fewer deposits.
    pub k_min: u32,
    pub verifiers: Vec<Pubkey>,
    pub threshold: u8,
    pub membership_root: [u8; 32],
    /// How many deposited leaves `membership_root` covers (<= `leaf_count`).
    pub root_set_size: u32,
    /// Deposits ever made: the next leaf index. Append-only.
    pub leaf_count: u32,
    /// Notes still in the vault: deposits minus withdrawals minus exits. The
    /// vault holds exactly `live_notes * note_amount` unless someone donates.
    pub live_notes: u32,
    /// Notes that left through `ragequit`, publicly linked to their commitment.
    pub exits: u32,
    pub bump: u8,
    pub vault_bump: u8,
}

impl Pool {
    pub const SPACE: usize =
        8 + 32 + 32 + 32 + 8 + 2 + 4 + (4 + MAX_VERIFIERS * 32) + 1 + 32 + 4 + 4 + 4 + 4 + 1 + 1;
}

/// Existence is the approval.
#[account]
pub struct AllowEntry {
    pub member: Pubkey,
    pub bump: u8,
}

impl AllowEntry {
    pub const SPACE: usize = 8 + 32 + 1;
}

/// One per deposit. `pool` comes first so a client can fetch every leaf of a
/// pool with one `getProgramAccounts` memcmp at offset 8, and rebuild the tree
/// from `(index, commitment)` without parsing logs.
#[account]
pub struct DepositRecord {
    pub pool: Pubkey,
    pub depositor: Pubkey,
    pub commitment: [u8; 32],
    pub index: u32,
    pub exited: bool,
    pub bump: u8,
}

impl DepositRecord {
    pub const SPACE: usize = 8 + 32 + 32 + 32 + 4 + 1 + 1;
}

/// Existence means "this note is spent", by withdrawal or by exit.
#[account]
pub struct NullifierRecord {
    pub bump: u8,
}

impl NullifierRecord {
    pub const SPACE: usize = 8 + 1;
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = Pool::SPACE,
        seeds = [b"pool", authority.key().as_ref(), mint.key().as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,
    pub mint: Account<'info, Mint>,
    #[account(
        init,
        payer = authority,
        seeds = [b"vault", pool.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = pool,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(token::mint = mint)]
    pub treasury: Account<'info, TokenAccount>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Admin<'info> {
    #[account(
        mut,
        seeds = [b"pool", authority.key().as_ref(), pool.mint.as_ref()],
        bump = pool.bump,
        has_one = authority
    )]
    pub pool: Account<'info, Pool>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(member: Pubkey)]
pub struct Allow<'info> {
    #[account(
        seeds = [b"pool", authority.key().as_ref(), pool.mint.as_ref()],
        bump = pool.bump,
        has_one = authority
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        init,
        payer = authority,
        space = AllowEntry::SPACE,
        seeds = [b"allow", pool.key().as_ref(), member.as_ref()],
        bump
    )]
    pub allow_entry: Account<'info, AllowEntry>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(member: Pubkey)]
pub struct Revoke<'info> {
    #[account(
        seeds = [b"pool", authority.key().as_ref(), pool.mint.as_ref()],
        bump = pool.bump,
        has_one = authority
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        close = authority,
        seeds = [b"allow", pool.key().as_ref(), member.as_ref()],
        bump = allow_entry.bump
    )]
    pub allow_entry: Account<'info, AllowEntry>,
    #[account(mut)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(commitment: [u8; 32])]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.authority.as_ref(), pool.mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    /// Must exist: this is the allowlist check.
    #[account(
        seeds = [b"allow", pool.key().as_ref(), depositor.key().as_ref()],
        bump = allow_entry.bump
    )]
    pub allow_entry: Account<'info, AllowEntry>,
    #[account(
        init,
        payer = depositor,
        space = DepositRecord::SPACE,
        seeds = [b"deposit", pool.key().as_ref(), commitment.as_ref()],
        bump
    )]
    pub deposit_record: Account<'info, DepositRecord>,
    #[account(address = pool.mint)]
    pub mint: Account<'info, Mint>,
    #[account(mut, token::mint = pool.mint, token::authority = depositor)]
    pub depositor_token: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"vault", pool.key().as_ref()], bump = pool.vault_bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub depositor: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(nullifier: [u8; 32])]
pub struct Withdraw<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.authority.as_ref(), pool.mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    /// `init` is the anti-replay: a spent nullifier already has this account.
    #[account(
        init,
        payer = relayer,
        space = NullifierRecord::SPACE,
        seeds = [b"nullifier", pool.key().as_ref(), nullifier.as_ref()],
        bump
    )]
    pub nullifier_record: Account<'info, NullifierRecord>,
    #[account(address = pool.mint)]
    pub mint: Account<'info, Mint>,
    #[account(mut, seeds = [b"vault", pool.key().as_ref()], bump = pool.vault_bump)]
    pub vault: Account<'info, TokenAccount>,
    /// Bound into the attestation, so the relayer cannot swap it.
    #[account(
        mut,
        token::mint = pool.mint,
        constraint = recipient.key() != vault.key() @ PoolError::InvalidRecipient
    )]
    pub recipient: Account<'info, TokenAccount>,
    #[account(mut, address = pool.treasury)]
    pub treasury: Account<'info, TokenAccount>,
    /// Any relayer: the depositor's key never appears in a withdrawal.
    #[account(mut)]
    pub relayer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    /// CHECK: address-checked; read only through the instructions-sysvar helpers.
    #[account(address = solana_sdk_ids::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(commitment: [u8; 32], nullifier: [u8; 32])]
pub struct Ragequit<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.authority.as_ref(), pool.mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        seeds = [b"deposit", pool.key().as_ref(), commitment.as_ref()],
        bump = deposit_record.bump,
        has_one = depositor @ PoolError::NotDepositor
    )]
    pub deposit_record: Account<'info, DepositRecord>,
    #[account(
        init,
        payer = depositor,
        space = NullifierRecord::SPACE,
        seeds = [b"nullifier", pool.key().as_ref(), nullifier.as_ref()],
        bump
    )]
    pub nullifier_record: Account<'info, NullifierRecord>,
    #[account(address = pool.mint)]
    pub mint: Account<'info, Mint>,
    #[account(mut, seeds = [b"vault", pool.key().as_ref()], bump = pool.vault_bump)]
    pub vault: Account<'info, TokenAccount>,
    /// The refund goes to an account the depositor owns, nowhere else.
    #[account(mut, token::mint = pool.mint, token::authority = depositor)]
    pub depositor_token: Account<'info, TokenAccount>,
    #[account(mut)]
    pub depositor: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    /// CHECK: address-checked; read only through the instructions-sysvar helpers.
    #[account(address = solana_sdk_ids::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
}

#[event]
pub struct Deposited {
    pub pool: Pubkey,
    pub commitment: [u8; 32],
    pub index: u32,
}

#[event]
pub struct Withdrawn {
    pub pool: Pubkey,
    pub nullifier: [u8; 32],
    pub recipient: Pubkey,
}

#[event]
pub struct Exited {
    pub pool: Pubkey,
    pub commitment: [u8; 32],
    pub nullifier: [u8; 32],
}

#[event]
pub struct RootPublished {
    pub pool: Pubkey,
    pub root: [u8; 32],
    pub set_size: u32,
}

#[error_code]
pub enum PoolError {
    #[msg("note size, fee or floor is out of range")]
    InvalidConfig,
    #[msg("the verifier committee is empty, too large, has a bad threshold, or has duplicates")]
    InvalidCommittee,
    #[msg("fewer than the required threshold of committee verifiers attested this exact spend")]
    NotEnoughAttestations,
    #[msg("the pool has not published a membership root yet")]
    RootNotPublished,
    #[msg("the withdrawal's root is not the pool's current root")]
    RootMismatch,
    #[msg("the root covers more deposits than exist")]
    InvalidSetSize,
    #[msg("the anonymity set is below the pool's floor: a withdrawal here would not be private")]
    AnonymitySetTooSmall,
    #[msg("a commitment cannot be all zeros")]
    InvalidCommitment,
    #[msg("the recipient cannot be the vault")]
    InvalidRecipient,
    #[msg("only the original depositor can exit this note")]
    NotDepositor,
    #[msg("this note already exited")]
    AlreadyExited,
    #[msg("the pool is full")]
    PoolFull,
    #[msg("vault accounting underflow")]
    VaultAccounting,
    #[msg("a digest limb is not a canonical Mersenne-31 value: one field element, one encoding")]
    NonCanonical,
}
