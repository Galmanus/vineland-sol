//! # vineland-pool
//!
//! Private authorization for public money, on Solana. The payment is public; the
//! payer is not.
//!
//! An operator approves addresses (the allowlist: KYC happens at the door, before
//! a key can enter). An approved key deposits one fixed-size note of an SPL token
//! into the vault and publishes a commitment, a hash of a secret it keeps. Later
//! the note leaves the vault to a fresh account, submitted by a relayer, so the
//! depositor's key never signs the withdrawal. The chain shows
//! `relayer -> vault -> clean account`; every note is the same size, so neither
//! the amount nor the signer says which approved depositor it was. Entry is
//! attributable and auditable, exit is authorized but unlinked.
//!
//! The engine underneath is riverrun (`github.com/Galmanus/mirror-pool`): the
//! commitment, nullifier and membership root are Poseidon2-M31 digests, and
//! membership is proven with riverrun's post-quantum Circle-STARK "crowd" proofs,
//! whose public inputs carry a blinded commitment instead of the leaf.
//!
//! ## No committee, no published root
//!
//! This version trusts no signer for soundness. Two things used to be a
//! committee's job, and both are on chain now:
//!
//! - **The root.** Every deposit is folded into a depth-127 Poseidon2-M31 tree
//!   held in a program account (`insert`, permissionless, a few transactions per
//!   deposit because one fold costs more than a transaction's compute cap). The
//!   operator cannot publish a root over leaves nobody deposited, because nobody
//!   publishes roots: a withdrawal must name one this program computed.
//! - **The proof.** A withdrawal reads a buffer account owned by the riverrun
//!   M31 verifier program ([`VERIFIER_ID`]) that verified both crowd proofs on
//!   chain, staged across transactions, recipient-bound, and marked the buffer
//!   settleable. The pool checks the owner, the settleable mark, the kind, and
//!   that every public input matches this pool: action, round, root, nullifier,
//!   recipient.
//!
//! So spending a note needs no elliptic-curve signature from its owner and no
//! committee: authority is knowledge of the note's secret, proven with a
//! hash-only STARK. What remains trusted is named: the two programs' code and
//! their upgrade authorities, and the operator for the allowlist (who may
//! deposit), not for custody.
//!
//! ## Exit and withdraw are one spend
//!
//! A depositor can reclaim their note in public (`exit`), but an exit must burn
//! the same nullifier a withdrawal would, otherwise "withdraw privately, then exit
//! the deposit" pays the note twice out of everyone else's money. An exit reveals
//! the note's secret; the program recomputes the leaf and the nullifier from it
//! with the same permutation the prover uses, and the nullifier PDA is the single
//! spent-flag for both paths. The refund can only go to the depositor's own
//! token account, so revealing the secret to trigger it gives nobody else money.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};
use riverrun_m31::{compress, permute, CONTEXT_LEN, DIGEST_LEN, SECRET_LEN, WIDTH};

mod empty;
pub use empty::EMPTY;

declare_id!("3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi");

/// The riverrun M31 staged verifier (`programs/riverrun-m31-verifier` in
/// mirror-pool at ce5e8c7). Fixed in code: an operator who could name the
/// verifier could name one that finalizes anything.
pub const VERIFIER_ID: Pubkey = pubkey!("8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo");

/// Depth of riverrun's crowd membership tree (`riverrun_m31::CROWD_DEPTH`).
pub const DEPTH: usize = 127;
/// Recent roots a withdrawal may name, so a deposit landing between proving and
/// settling does not invalidate the proof.
pub const ROOT_HISTORY: usize = 16;

/// The protocol fee is charged on withdrawal only and capped here, so a pool
/// cannot be initialized to confiscate notes.
const MAX_FEE_BPS: u16 = 1_000;

// Verifier buffer layout (`riverrun-m31-verifier/src/staged.rs`, ce5e8c7).
const B_FINALIZED: usize = 40;
const B_ROOT: usize = 41;
const B_NULLIFIER: usize = 73;
const B_ROUND: usize = 105;
const B_ACTION: usize = 121;
const B_RECIPIENT: usize = 153;
const B_VERSION: usize = 185;
const B_KIND: usize = 186;
/// Where proof bytes start; any real buffer is at least this long.
const B_PROOFS: usize = 2432;
/// `FINAL_SETTLEABLE`: both crowd proofs verified, recipient-bound.
const FINAL_SETTLEABLE: u8 = 1;
/// `KIND_CROWD | KIND_BOUND`, the only kind a pool should accept.
const KIND_CROWD_BOUND: u8 = 0x83;

/// Domain of the leaf context (`client::note_action`).
const NOTE_DOMAIN: &[u8] = b"vineland/note/v1";
/// Domain of the nullifier context (`client::spend_round_u64`). v2: the verifier
/// accepts a round only as a u64 in 16-bit limbs, so the context is a u64.
const SPEND_DOMAIN: &[u8] = b"vineland/spend/v2";

#[program]
pub mod vineland_pool {
    use super::*;

    /// Open a pool for one SPL mint and one note size, with a protocol fee in
    /// basis points (paid to `treasury` on withdrawal) and an anonymity floor
    /// `k_min` below which no withdrawal settles. The note's contexts are derived
    /// here from the pool's own address, so nobody chooses them.
    pub fn initialize(ctx: Context<Initialize>, note_amount: u64, fee_bps: u16, k_min: u32) -> Result<()> {
        require!(note_amount > 0, PoolError::InvalidConfig);
        require!(fee_bps <= MAX_FEE_BPS, PoolError::InvalidConfig);
        require!(fee_of(note_amount, fee_bps) < note_amount, PoolError::InvalidConfig);
        require!(k_min >= 1, PoolError::InvalidConfig);

        let key = ctx.accounts.pool.key();
        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.authority.key();
        pool.mint = ctx.accounts.mint.key();
        pool.treasury = ctx.accounts.treasury.key();
        pool.note_amount = note_amount;
        pool.fee_bps = fee_bps;
        pool.k_min = k_min;
        pool.action = note_action(&key);
        pool.round = spend_round_u64(&key);
        pool.leaf_count = 0;
        pool.live_notes = 0;
        pool.exits = 0;
        pool.bump = ctx.bumps.pool;
        pool.vault_bump = ctx.bumps.vault;
        pool.tree_bump = ctx.bumps.tree;
        ctx.accounts.tree.load_init()?;
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

    /// Deposit one note. Only an approved key may, and the entry is public on
    /// purpose: the depositor signs, the amount is the pool's fixed note, and the
    /// commitment is recorded against the depositor so they can always exit.
    /// A commitment can be deposited once (its record is a PDA). The leaf joins
    /// the tree through `insert`.
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

    /// Fold the next deposit into the tree, at most `max_levels` levels per call
    /// (one level is one Poseidon2-M31 compression, about 21k CU). Anyone may
    /// call it; leaves enter strictly in deposit order. When the fold reaches the
    /// top, the new root (covering every leaf up to this one) joins the history.
    pub fn insert(ctx: Context<Insert>, max_levels: u8) -> Result<()> {
        require!(max_levels > 0, PoolError::InvalidConfig);
        let record = &ctx.accounts.deposit_record;
        let tree = &mut ctx.accounts.tree.load_mut()?;
        require!(record.index == tree.inserted, PoolError::OutOfOrder);

        let index = record.index as u64;
        let start = tree.level as usize;
        if start == 0 {
            tree.node = record.commitment;
        }
        let end = core::cmp::min(start + max_levels as usize, DEPTH);
        let mut node = limbs(&tree.node);
        for level in start..end {
            let right = level < 64 && (index >> level) & 1 == 1;
            if right {
                node = compress(limbs(&tree.filled[level]), node);
            } else {
                tree.filled[level] = bytes(&node);
                node = compress(node, limbs(&EMPTY[level]));
            }
        }
        tree.node = bytes(&node);
        tree.level = end as u32;

        if end == DEPTH {
            let size = record.index + 1;
            let slot = (tree.next_root as usize) % ROOT_HISTORY;
            tree.roots[slot] = tree.node;
            tree.root_sizes[slot] = size;
            tree.next_root = tree.next_root.wrapping_add(1);
            tree.inserted = size;
            tree.level = 0;
            emit!(RootComputed { pool: ctx.accounts.pool.key(), root: tree.node, set_size: size });
        }
        Ok(())
    }

    /// Withdraw one note to `recipient`, submitted by any relayer. Authorized by
    /// a verifier buffer in which both crowd proofs verified on chain, bound to
    /// this recipient, against a root this program computed, with this pool's
    /// contexts and this nullifier. The root must cover at least `k_min` notes
    /// still in the set, and the nullifier must never have been spent (its PDA is
    /// created here; a second spend finds it). Pays `note - fee` to the
    /// recipient and `fee` to the treasury.
    pub fn withdraw(ctx: Context<Withdraw>, nullifier: [u8; 32]) -> Result<()> {
        require!(is_canonical_m31(&nullifier), PoolError::NonCanonical);
        let pool = &ctx.accounts.pool;
        let root = {
            let buf = ctx.accounts.proof.try_borrow_data()?;
            require!(buf.len() >= B_PROOFS, PoolError::ProofNotVerified);
            require!(
                buf[B_VERSION] == 1 && buf[B_FINALIZED] == FINAL_SETTLEABLE && buf[B_KIND] == KIND_CROWD_BOUND,
                PoolError::ProofNotVerified
            );
            require!(buf[B_NULLIFIER..B_NULLIFIER + 32] == nullifier, PoolError::ProofMismatch);
            require!(buf[B_ACTION..B_ACTION + 32] == pool.action, PoolError::ProofMismatch);
            require!(buf[B_ROUND..B_ROUND + 16] == (pool.round as u128).to_le_bytes(), PoolError::ProofMismatch);
            require!(
                buf[B_RECIPIENT..B_RECIPIENT + 32] == ctx.accounts.recipient.key().to_bytes(),
                PoolError::ProofMismatch
            );
            let mut root = [0u8; 32];
            root.copy_from_slice(&buf[B_ROOT..B_ROOT + 32]);
            root
        };

        let size = {
            let tree = ctx.accounts.tree.load()?;
            tree.size_of(&root).ok_or(PoolError::UnknownRoot)?
        };
        // A set of one is not an anonymity set: the payout belongs to that
        // depositor by elimination. An exit is public (it reveals which
        // commitment left), so exited notes do not count toward the set.
        require!(size.saturating_sub(pool.exits) >= pool.k_min, PoolError::AnonymitySetTooSmall);

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

        ctx.accounts.nullifier_record.bump = ctx.bumps.nullifier_record;
        let pool = &mut ctx.accounts.pool;
        pool.live_notes = pool.live_notes.checked_sub(1).ok_or(PoolError::VaultAccounting)?;

        emit!(Withdrawn { pool: pool.key(), nullifier, recipient: ctx.accounts.recipient.key() });
        Ok(())
    }

    /// Reclaim a note in full, to the depositor's own token account, by revealing
    /// its secret. The program recomputes the leaf (must be this deposit's
    /// commitment) and the nullifier (must be the one named, whose PDA is created
    /// here), so a withdrawn note cannot also exit and an exited note cannot also
    /// be withdrawn. Anyone may submit it; the money can only go to the depositor.
    pub fn exit(ctx: Context<Exit>, secret: [u8; 32], nullifier: [u8; 32]) -> Result<()> {
        require!(!ctx.accounts.deposit_record.exited, PoolError::AlreadyExited);
        require!(is_canonical_m31(&secret), PoolError::NonCanonical);
        let pool = &ctx.accounts.pool;
        let s = limbs(&secret);
        let leaf = bytes(&digest(&s, limbs(&pool.action)));
        require!(leaf == ctx.accounts.deposit_record.commitment, PoolError::SecretMismatch);
        let spent = bytes(&digest(&s, round_limbs(pool.round)));
        require!(spent == nullifier, PoolError::SecretMismatch);

        pay_from_vault(
            pool,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.depositor_token.to_account_info(),
            &ctx.accounts.token_program,
            pool.note_amount,
        )?;

        let commitment = ctx.accounts.deposit_record.commitment;
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

fn limbs(b: &[u8; 32]) -> [u64; DIGEST_LEN] {
    core::array::from_fn(|i| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]) as u64)
}

fn bytes(d: &[u64; DIGEST_LEN]) -> [u8; 32] {
    let mut b = [0u8; 32];
    for (i, v) in d.iter().enumerate() {
        b[4 * i..4 * i + 4].copy_from_slice(&(*v as u32).to_le_bytes());
    }
    b
}

/// `permute(secret ‖ context)[0..8]`: the leaf (context = action) or the
/// nullifier (context = round), exactly as `client::Note` derives them.
fn digest(secret: &[u64; SECRET_LEN], context: [u64; CONTEXT_LEN]) -> [u64; DIGEST_LEN] {
    let mut input = [0u64; WIDTH];
    input[..SECRET_LEN].copy_from_slice(secret);
    input[SECRET_LEN..].copy_from_slice(&context);
    let out = permute(input);
    core::array::from_fn(|i| out[i])
}

/// Eight M31 limbs from a domain-separated SHA-256 of the pool address (the
/// leaf context, `client::note_action`).
pub fn note_action(pool: &Pubkey) -> [u8; 32] {
    let h = solana_sha256_hasher::hashv(&[NOTE_DOMAIN, pool.as_ref()]).to_bytes();
    let mut out = [0u8; 32];
    for i in 0..8 {
        let mut v = u32::from_le_bytes([h[4 * i], h[4 * i + 1], h[4 * i + 2], h[4 * i + 3]]) & M31_P;
        if v == M31_P {
            v = 0;
        }
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// The nullifier context as a u64 (`client::spend_round_u64`).
pub fn spend_round_u64(pool: &Pubkey) -> u64 {
    let h = solana_sha256_hasher::hashv(&[SPEND_DOMAIN, pool.as_ref()]).to_bytes();
    u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]])
}

/// The verifier's round encoding: four 16-bit limbs, then zeros.
pub fn round_limbs(round: u64) -> [u64; CONTEXT_LEN] {
    let mut r = [0u64; CONTEXT_LEN];
    for (i, limb) in r.iter_mut().take(4).enumerate() {
        *limb = (round >> (16 * i)) & 0xffff;
    }
    r
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
    /// Withdrawals are refused while the named root covers fewer live notes.
    pub k_min: u32,
    /// Leaf context, eight M31 limbs, derived from this pool's address.
    pub action: [u8; 32],
    /// Nullifier context, derived from this pool's address.
    pub round: u64,
    /// Deposits ever made: the next leaf index. Append-only.
    pub leaf_count: u32,
    /// Notes still in the vault: deposits minus withdrawals minus exits. The
    /// vault holds exactly `live_notes * note_amount` unless someone donates.
    pub live_notes: u32,
    /// Notes that left through `exit`, publicly linked to their commitment.
    pub exits: u32,
    pub bump: u8,
    pub vault_bump: u8,
    pub tree_bump: u8,
}

impl Pool {
    pub const SPACE: usize = 8 + 32 + 32 + 32 + 8 + 2 + 4 + 32 + 8 + 4 + 4 + 4 + 1 + 1 + 1;
}

/// The deposit tree, folded on chain. `filled[l]` is the last left node seen at
/// level `l` (the classic incremental Merkle frontier); `node` and `level` hold
/// a fold in progress, since one fold spans several transactions.
#[account(zero_copy)]
pub struct Tree {
    pub filled: [[u8; 32]; DEPTH],
    pub roots: [[u8; 32]; ROOT_HISTORY],
    pub root_sizes: [u32; ROOT_HISTORY],
    pub node: [u8; 32],
    /// Leaves fully folded in: the index of the next leaf to fold.
    pub inserted: u32,
    /// Next level of the fold in progress; 0 when none is in progress.
    pub level: u32,
    pub next_root: u32,
    pub _reserved: u32,
}

impl Tree {
    pub const SPACE: usize = 8 + core::mem::size_of::<Tree>();

    /// How many leaves `root` covers, if it is one of the recent roots.
    pub fn size_of(&self, root: &[u8; 32]) -> Option<u32> {
        (0..ROOT_HISTORY).find(|&i| self.root_sizes[i] > 0 && &self.roots[i] == root).map(|i| self.root_sizes[i])
    }
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
    #[account(
        init,
        payer = authority,
        space = Tree::SPACE,
        seeds = [b"tree", pool.key().as_ref()],
        bump
    )]
    pub tree: AccountLoader<'info, Tree>,
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
pub struct Insert<'info> {
    #[account(seeds = [b"pool", pool.authority.as_ref(), pool.mint.as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [b"tree", pool.key().as_ref()], bump = pool.tree_bump)]
    pub tree: AccountLoader<'info, Tree>,
    /// The leaf being folded; its seeds tie it to this pool.
    #[account(
        seeds = [b"deposit", pool.key().as_ref(), deposit_record.commitment.as_ref()],
        bump = deposit_record.bump
    )]
    pub deposit_record: Account<'info, DepositRecord>,
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
    #[account(seeds = [b"tree", pool.key().as_ref()], bump = pool.tree_bump)]
    pub tree: AccountLoader<'info, Tree>,
    /// CHECK: a buffer of the riverrun M31 verifier; only that program can write
    /// it, and every field read is checked against this pool in `withdraw`.
    #[account(owner = VERIFIER_ID @ PoolError::ProofNotVerified)]
    pub proof: UncheckedAccount<'info>,
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
    /// Bound into the proofs, so the relayer cannot swap it.
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
}

#[derive(Accounts)]
#[instruction(secret: [u8; 32], nullifier: [u8; 32])]
pub struct Exit<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.authority.as_ref(), pool.mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        seeds = [b"deposit", pool.key().as_ref(), deposit_record.commitment.as_ref()],
        bump = deposit_record.bump
    )]
    pub deposit_record: Account<'info, DepositRecord>,
    #[account(
        init,
        payer = payer,
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
    #[account(
        mut,
        token::mint = pool.mint,
        constraint = depositor_token.owner == deposit_record.depositor @ PoolError::NotDepositorAccount
    )]
    pub depositor_token: Account<'info, TokenAccount>,
    /// Whoever submits the exit pays the nullifier record's rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[event]
pub struct Deposited {
    pub pool: Pubkey,
    pub commitment: [u8; 32],
    pub index: u32,
}

#[event]
pub struct RootComputed {
    pub pool: Pubkey,
    pub root: [u8; 32],
    pub set_size: u32,
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

#[error_code]
pub enum PoolError {
    #[msg("note size, fee or floor is out of range")]
    InvalidConfig,
    #[msg("the proof buffer is not a finalized, recipient-bound crowd verification by the riverrun verifier")]
    ProofNotVerified,
    #[msg("the verified proof is for another pool, nullifier or recipient")]
    ProofMismatch,
    #[msg("the proof's root is not one this pool computed recently")]
    UnknownRoot,
    #[msg("leaves enter the tree in deposit order")]
    OutOfOrder,
    #[msg("the anonymity set is below the pool's floor: a withdrawal here would not be private")]
    AnonymitySetTooSmall,
    #[msg("a commitment cannot be all zeros")]
    InvalidCommitment,
    #[msg("the recipient cannot be the vault")]
    InvalidRecipient,
    #[msg("the revealed secret does not open this deposit or this nullifier")]
    SecretMismatch,
    #[msg("an exit refunds only to a token account the depositor owns")]
    NotDepositorAccount,
    #[msg("this note already exited")]
    AlreadyExited,
    #[msg("the pool is full")]
    PoolFull,
    #[msg("vault accounting underflow")]
    VaultAccounting,
    #[msg("a digest limb is not a canonical Mersenne-31 value: one field element, one encoding")]
    NonCanonical,
}
