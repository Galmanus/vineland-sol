//! # vineland-client
//!
//! The off-chain half of Vineland on Solana. The program (`programs/vineland-pool`)
//! only ever sees 32-byte digests and committee signatures; everything that makes
//! those digests mean something lives here:
//!
//! - a **note** is one secret. Its leaf is `permute(secret ‖ action)[0..8]` and its
//!   nullifier `permute(secret ‖ round)[0..8]`, both Poseidon2 over Mersenne-31,
//!   with `action` and `round` fixed per pool, so one note has exactly one
//!   nullifier, ever;
//! - the **deposit tree** is riverrun's depth-127 sparse Merkle tree over those
//!   leaves, at their deposit index, rebuildable by anyone from the pool's
//!   `DepositRecord` accounts;
//! - a **withdrawal** is a pair of riverrun crowd proofs (binding + membership,
//!   composed by a blinded commitment `C`), so the leaf is in no public input;
//! - the **committee check** is what an honest verifier runs before signing:
//!   verify both proofs, and recompute the root from the on-chain deposits rather
//!   than trusting the root the operator published.
//!
//! Encoding: a digest is eight canonical M31 limbs, each a u32 LE, 32 bytes. The
//! program refuses non-canonical limbs; [`enc`] never produces them.

use std::collections::BTreeMap;
use std::io::Read;

use riverrun_m31::{
    compress, permute, prove_binding_crowd, prove_membership_crowd, verify_binding_crowd,
    verify_crowd, zk::Seed, CrowdBindingProof, CrowdMembershipProof, PathStep, CONTEXT_LEN,
    CROWD_DEPTH, DIGEST_LEN, SECRET_LEN, WIDTH,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
};

pub const PROGRAM_ID: Pubkey = solana_sdk::pubkey!("AsQwEN7XiUPW2MGHEbEkh8UCKixwpSBfE1KKt5QVgYgx");

/// Proof parameters: the point riverrun's `prove_crowd_real` and the Stellar
/// gate use (12 FRI queries, blowup 2^7, 2^7 binding rows); riverrun-m31's crowd
/// tests explain why it clears both the hiding margin and the FRI budget.
pub const QUERIES: usize = 12;
pub const LOG_BLOWUP: usize = 7;
pub const LOG_ROWS: usize = 7;

pub const M31_P: u64 = (1 << 31) - 1;

pub type Digest8 = [u64; DIGEST_LEN];

// ---- encoding -------------------------------------------------------------

/// Eight canonical M31 limbs to the program's 32 bytes. Panics on a
/// non-canonical limb: every digest here comes out of the permutation, so one
/// would be a bug, not an input.
pub fn enc(d: &Digest8) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, v) in d.iter().enumerate() {
        assert!(*v < M31_P, "non-canonical M31 limb");
        out[4 * i..4 * i + 4].copy_from_slice(&(*v as u32).to_le_bytes());
    }
    out
}

/// The inverse, refusing any limb outside `[0, P)`.
pub fn dec(b: &[u8; 32]) -> Option<Digest8> {
    let mut out = [0u64; DIGEST_LEN];
    for (i, limb) in b.chunks_exact(4).enumerate() {
        let v = u32::from_le_bytes(limb.try_into().unwrap()) as u64;
        if v >= M31_P {
            return None;
        }
        out[i] = v;
    }
    Some(out)
}

fn canonical(d: &Digest8) -> bool {
    d.iter().all(|v| *v < M31_P)
}

/// Eight M31 limbs from a domain-separated hash of the pool address. Used for
/// the per-pool `action` (the leaf's context) and `round` (the nullifier's), so a
/// note belongs to one pool and has one nullifier. The tiny bias of the mask is
/// irrelevant: these are public constants, not secrets.
fn pool_context(domain: &[u8], pool: &Pubkey) -> [u64; CONTEXT_LEN] {
    let h = Sha256::new().chain_update(domain).chain_update(pool.as_ref()).finalize();
    core::array::from_fn(|i| {
        let v = u32::from_le_bytes(h[4 * i..4 * i + 4].try_into().unwrap()) as u64 & M31_P;
        if v == M31_P {
            0
        } else {
            v
        }
    })
}

pub fn note_action(pool: &Pubkey) -> [u64; CONTEXT_LEN] {
    pool_context(b"vineland/note/v1", pool)
}

pub fn spend_round(pool: &Pubkey) -> [u64; CONTEXT_LEN] {
    pool_context(b"vineland/spend/v1", pool)
}

// ---- randomness -------------------------------------------------------------

fn os_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("read /dev/urandom");
    b
}

/// Uniform over M31 by rejection: 31 bits of OS entropy per limb.
fn os_m31() -> Digest8 {
    core::array::from_fn(|_| loop {
        let v = u32::from_le_bytes(os_bytes::<4>()) as u64 & M31_P;
        if v != M31_P {
            return v;
        }
    })
}

fn os_seed() -> Seed {
    Seed::from_bytes(os_bytes::<32>())
}

// ---- notes ------------------------------------------------------------------

/// One note: the only secret a depositor keeps. Losing it loses the note's
/// private withdrawal (the depositor can still exit with it, publicly);
/// leaking it lets anyone withdraw the note.
#[derive(Clone, Serialize, Deserialize)]
pub struct Note {
    pub pool: String,
    pub secret: [u64; SECRET_LEN],
}

impl Note {
    pub fn new(pool: &Pubkey) -> Self {
        Note { pool: pool.to_string(), secret: os_m31() }
    }

    pub fn pool(&self) -> Pubkey {
        self.pool.parse().expect("note pool address")
    }

    fn pack(&self, context: [u64; CONTEXT_LEN]) -> [u64; WIDTH] {
        let mut input = [0u64; WIDTH];
        input[..SECRET_LEN].copy_from_slice(&self.secret);
        input[SECRET_LEN..].copy_from_slice(&context);
        input
    }

    pub fn leaf(&self) -> Digest8 {
        let out = permute(self.pack(note_action(&self.pool())));
        core::array::from_fn(|i| out[i])
    }

    /// What `deposit` publishes.
    pub fn commitment(&self) -> [u8; 32] {
        enc(&self.leaf())
    }
}

// ---- the deposit tree ---------------------------------------------------------

/// riverrun's depth-127 sparse Merkle tree over `compress`, leaf `i` at deposit
/// index `i`, empty slots the canonical empty subtree (`E[0] = 0`, which no real
/// deposit can be: the program refuses an all-zero commitment).
pub struct Tree {
    leaves: BTreeMap<u128, Digest8>,
}

impl Tree {
    pub fn new(leaves: impl IntoIterator<Item = (u32, Digest8)>) -> Self {
        Tree { leaves: leaves.into_iter().map(|(i, l)| (i as u128, l)).collect() }
    }

    pub fn leaf(&self, index: u32) -> Option<Digest8> {
        self.leaves.get(&(index as u128)).copied()
    }

    pub fn root(&self) -> Digest8 {
        self.fold(None).0
    }

    pub fn path(&self, index: u32) -> [PathStep; CROWD_DEPTH] {
        self.fold(Some(index)).1.expect("path requested")
    }

    fn fold(&self, track: Option<u32>) -> (Digest8, Option<[PathStep; CROWD_DEPTH]>) {
        let mut empty = [[0u64; DIGEST_LEN]; CROWD_DEPTH + 1];
        for k in 1..=CROWD_DEPTH {
            empty[k] = compress(empty[k - 1], empty[k - 1]);
        }
        let mut level = self.leaves.clone();
        let mut path = Vec::with_capacity(CROWD_DEPTH);
        let mut cur = track.map(|i| i as u128);
        for k in 0..CROWD_DEPTH {
            let get = |lvl: &BTreeMap<u128, Digest8>, idx: u128| lvl.get(&idx).copied().unwrap_or(empty[k]);
            if let Some(c) = cur {
                path.push(PathStep { sibling: get(&level, c ^ 1), node_on_right: (c & 1) == 1 });
            }
            let mut parents = BTreeMap::new();
            for &idx in level.keys() {
                let p = idx >> 1;
                parents.entry(p).or_insert_with(|| compress(get(&level, p << 1), get(&level, (p << 1) | 1)));
            }
            level = parents;
            cur = cur.map(|c| c >> 1);
        }
        let root = level.get(&0).copied().unwrap_or(empty[CROWD_DEPTH]);
        let path = track.map(|_| path.try_into().unwrap_or_else(|_| unreachable!()));
        (root, path)
    }
}

/// The tree an honest party rebuilds from the chain: the deposits with index
/// below `set_size` (the leaves the published root claims to cover). Refuses a
/// non-canonical commitment rather than guessing what it meant.
pub fn tree_from_deposits(deposits: &[DepositRecord], set_size: u32) -> Option<Tree> {
    let mut leaves = Vec::new();
    for d in deposits.iter().filter(|d| d.index < set_size) {
        leaves.push((d.index, dec(&d.commitment)?));
    }
    Some(Tree::new(leaves))
}

// ---- withdrawal ---------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct WithdrawBundle {
    pub pool: String,
    pub root: Digest8,
    /// The blinded commitment both proofs share; the leaf appears nowhere.
    pub c: Digest8,
    pub nullifier: Digest8,
    #[serde(with = "hex::serde")]
    pub binding: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub membership: Vec<u8>,
}

impl WithdrawBundle {
    pub fn nullifier_bytes(&self) -> [u8; 32] {
        enc(&self.nullifier)
    }
    pub fn root_bytes(&self) -> [u8; 32] {
        enc(&self.root)
    }
}

/// Prove that `note` sits at `index` of `tree` and reveal its nullifier, with a
/// fresh OS blinder (the whole unlinkability mechanism) and fresh prover seeds.
/// Runs on the depositor's machine: the secret never leaves it.
pub fn prove_withdraw(note: &Note, tree: &Tree, index: u32) -> WithdrawBundle {
    let pool = note.pool();
    let leaf = note.leaf();
    assert_eq!(tree.leaf(index), Some(leaf), "the note is not the leaf at this index");
    let blinder = os_m31();
    let (bproof, c, leaf_digest, nullifier) = prove_binding_crowd(
        note.secret,
        note_action(&pool),
        spend_round(&pool),
        blinder,
        QUERIES,
        LOG_BLOWUP,
        LOG_ROWS,
        os_seed(),
    );
    assert_eq!(leaf_digest, leaf, "the crate's leaf must equal the note's");
    let (mproof, c2, root) =
        prove_membership_crowd(leaf, blinder, &tree.path(index), QUERIES, LOG_BLOWUP, os_seed());
    assert_eq!(c, c2, "one (leaf, blinder) commits identically in both relations");
    assert_eq!(root, tree.root());
    WithdrawBundle {
        pool: pool.to_string(),
        root,
        c,
        nullifier,
        binding: bproof.to_postcard(),
        membership: mproof.to_postcard(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    WrongPool,
    NonCanonical,
    /// The published root is not the tree of the deposits on chain.
    RootNotTheDeposits,
    /// The bundle was proven against some other root.
    RootNotCurrent,
    Malformed,
    ProofRejected,
}

/// What an honest verifier checks before signing a withdrawal. `onchain_root`
/// and `set_size` are read from the pool account, `deposits` from its
/// `DepositRecord` accounts. Whether the nullifier is already spent is the
/// chain's job (its PDA exists or it does not).
pub fn committee_check_withdraw(
    b: &WithdrawBundle,
    pool: &Pubkey,
    onchain_root: &[u8; 32],
    set_size: u32,
    deposits: &[DepositRecord],
) -> Result<(), Refusal> {
    if b.pool != pool.to_string() {
        return Err(Refusal::WrongPool);
    }
    if !(canonical(&b.root) && canonical(&b.c) && canonical(&b.nullifier)) {
        return Err(Refusal::NonCanonical);
    }
    let tree = tree_from_deposits(deposits, set_size).ok_or(Refusal::NonCanonical)?;
    let real_root = tree.root();
    if enc(&real_root) != *onchain_root {
        return Err(Refusal::RootNotTheDeposits);
    }
    if b.root != real_root {
        return Err(Refusal::RootNotCurrent);
    }
    let bp = CrowdBindingProof::from_postcard(&b.binding).ok_or(Refusal::Malformed)?;
    let mp = CrowdMembershipProof::from_postcard(&b.membership).ok_or(Refusal::Malformed)?;
    if !verify_crowd(
        &bp,
        &mp,
        note_action(pool),
        spend_round(pool),
        b.c,
        b.nullifier,
        b.root,
        QUERIES,
        LOG_BLOWUP,
    ) {
        return Err(Refusal::ProofRejected);
    }
    Ok(())
}

pub fn withdraw_message(pool: &Pubkey, root: &[u8; 32], nullifier: &[u8; 32], recipient: &Pubkey) -> Vec<u8> {
    [b"vineland-wdrw-v1".as_ref(), pool.as_ref(), root, nullifier, recipient.as_ref()].concat()
}

// ---- exit ---------------------------------------------------------------------

/// A public exit: the depositor shows that the note's nullifier belongs to the
/// commitment it deposited, by a binding proof whose `C` anyone recomputes from
/// the commitment and the revealed blinder. The exit is public anyway, so
/// revealing the blinder costs nothing.
#[derive(Clone, Serialize, Deserialize)]
pub struct ExitBundle {
    pub pool: String,
    pub commitment: Digest8,
    pub blinder: Digest8,
    pub nullifier: Digest8,
    #[serde(with = "hex::serde")]
    pub binding: Vec<u8>,
}

impl ExitBundle {
    pub fn nullifier_bytes(&self) -> [u8; 32] {
        enc(&self.nullifier)
    }
    pub fn commitment_bytes(&self) -> [u8; 32] {
        enc(&self.commitment)
    }
}

pub fn prove_exit(note: &Note) -> ExitBundle {
    let pool = note.pool();
    let blinder = os_m31();
    let (bproof, _c, leaf, nullifier) = prove_binding_crowd(
        note.secret,
        note_action(&pool),
        spend_round(&pool),
        blinder,
        QUERIES,
        LOG_BLOWUP,
        LOG_ROWS,
        os_seed(),
    );
    ExitBundle {
        pool: pool.to_string(),
        commitment: leaf,
        blinder,
        nullifier,
        binding: bproof.to_postcard(),
    }
}

/// Before signing an exit: the commitment was deposited in this pool (the
/// caller passes the on-chain record), and the nullifier is the note's own.
pub fn committee_check_exit(b: &ExitBundle, pool: &Pubkey, record: &DepositRecord) -> Result<(), Refusal> {
    if b.pool != pool.to_string() || record.pool != *pool {
        return Err(Refusal::WrongPool);
    }
    if !(canonical(&b.commitment) && canonical(&b.blinder) && canonical(&b.nullifier)) {
        return Err(Refusal::NonCanonical);
    }
    if enc(&b.commitment) != record.commitment {
        return Err(Refusal::Malformed);
    }
    let c = compress(b.commitment, b.blinder);
    let bp = CrowdBindingProof::from_postcard(&b.binding).ok_or(Refusal::Malformed)?;
    if !verify_binding_crowd(&bp, note_action(pool), spend_round(pool), c, b.nullifier, QUERIES, LOG_BLOWUP) {
        return Err(Refusal::ProofRejected);
    }
    Ok(())
}

pub fn exit_message(pool: &Pubkey, commitment: &[u8; 32], nullifier: &[u8; 32]) -> Vec<u8> {
    [b"vineland-exit-v1".as_ref(), pool.as_ref(), commitment, nullifier].concat()
}

/// One committee signature as the native Ed25519 instruction the program
/// accepts: key, signature and message all inside this instruction's data.
pub fn attestation_ix(signer: &Keypair, msg: &[u8]) -> Instruction {
    let sig = signer.sign_message(msg);
    let mut data = Vec::with_capacity(112 + msg.len());
    data.push(1);
    data.push(0);
    for v in [48u16, u16::MAX, 16u16, u16::MAX, 112u16, msg.len() as u16, u16::MAX] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    data.extend_from_slice(&signer.pubkey().to_bytes());
    data.extend_from_slice(sig.as_ref());
    data.extend_from_slice(msg);
    Instruction { program_id: solana_sdk::ed25519_program::ID, accounts: vec![], data }
}

// ---- accounts -------------------------------------------------------------------

pub fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &PROGRAM_ID).0
}

pub fn pool_address(authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[b"pool", authority.as_ref(), mint.as_ref()])
}

pub fn vault_address(pool: &Pubkey) -> Pubkey {
    pda(&[b"vault", pool.as_ref()])
}

pub struct PoolState {
    pub authority: Pubkey,
    pub mint: Pubkey,
    pub treasury: Pubkey,
    pub note_amount: u64,
    pub fee_bps: u16,
    pub k_min: u32,
    pub verifiers: Vec<Pubkey>,
    pub threshold: u8,
    pub membership_root: [u8; 32],
    pub root_set_size: u32,
    pub leaf_count: u32,
    pub live_notes: u32,
    pub exits: u32,
}

impl PoolState {
    /// Anchor's borsh layout of `Pool` (8-byte discriminator first).
    pub fn parse(d: &[u8]) -> Option<Self> {
        let key = |o: usize| Pubkey::try_from(&d[o..o + 32]).ok();
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let n = u32_at(8 + 96 + 8 + 2 + 4) as usize;
        let after = 8 + 96 + 8 + 2 + 4 + 4 + 32 * n;
        if d.len() < after + 1 + 32 + 16 {
            return None;
        }
        Some(PoolState {
            authority: key(8)?,
            mint: key(40)?,
            treasury: key(72)?,
            note_amount: u64::from_le_bytes(d[104..112].try_into().unwrap()),
            fee_bps: u16::from_le_bytes(d[112..114].try_into().unwrap()),
            k_min: u32_at(114),
            verifiers: (0..n).map(|i| key(122 + 32 * i)).collect::<Option<_>>()?,
            threshold: d[after],
            membership_root: d[after + 1..after + 33].try_into().unwrap(),
            root_set_size: u32_at(after + 33),
            leaf_count: u32_at(after + 37),
            live_notes: u32_at(after + 41),
            exits: u32_at(after + 45),
        })
    }
}

#[derive(Clone, Debug)]
pub struct DepositRecord {
    pub pool: Pubkey,
    pub depositor: Pubkey,
    pub commitment: [u8; 32],
    pub index: u32,
    pub exited: bool,
}

/// `DepositRecord` accounts are 110 bytes, `pool` at offset 8: the memcmp
/// filter a client uses to fetch one pool's leaves.
pub const DEPOSIT_RECORD_LEN: usize = 110;

impl DepositRecord {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() != DEPOSIT_RECORD_LEN {
            return None;
        }
        Some(DepositRecord {
            pool: Pubkey::try_from(&d[8..40]).ok()?,
            depositor: Pubkey::try_from(&d[40..72]).ok()?,
            commitment: d[72..104].try_into().unwrap(),
            index: u32::from_le_bytes(d[104..108].try_into().unwrap()),
            exited: d[108] == 1,
        })
    }
}

// ---- instructions -------------------------------------------------------------------

fn disc(name: &str) -> [u8; 8] {
    Sha256::digest(format!("global:{name}").as_bytes())[..8].try_into().unwrap()
}

pub fn initialize_ix(
    authority: &Pubkey,
    mint: &Pubkey,
    treasury: &Pubkey,
    verifiers: &[Pubkey],
    threshold: u8,
    note_amount: u64,
    fee_bps: u16,
    k_min: u32,
) -> Instruction {
    let pool = pool_address(authority, mint);
    let mut data = disc("initialize").to_vec();
    data.extend_from_slice(&(verifiers.len() as u32).to_le_bytes());
    for v in verifiers {
        data.extend_from_slice(v.as_ref());
    }
    data.push(threshold);
    data.extend_from_slice(&note_amount.to_le_bytes());
    data.extend_from_slice(&fee_bps.to_le_bytes());
    data.extend_from_slice(&k_min.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_address(&pool), false),
            AccountMeta::new_readonly(*treasury, false),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

pub fn allow_ix(pool: &Pubkey, authority: &Pubkey, member: &Pubkey) -> Instruction {
    let mut data = disc("allow").to_vec();
    data.extend_from_slice(member.as_ref());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*pool, false),
            AccountMeta::new(pda(&[b"allow", pool.as_ref(), member.as_ref()]), false),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

pub fn publish_root_ix(pool: &Pubkey, authority: &Pubkey, root: [u8; 32], set_size: u32) -> Instruction {
    let mut data = disc("publish_root").to_vec();
    data.extend_from_slice(&root);
    data.extend_from_slice(&set_size.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![AccountMeta::new(*pool, false), AccountMeta::new_readonly(*authority, true)],
        data,
    }
}

pub fn deposit_ix(pool: &Pubkey, mint: &Pubkey, depositor: &Pubkey, depositor_token: &Pubkey, commitment: [u8; 32]) -> Instruction {
    let mut data = disc("deposit").to_vec();
    data.extend_from_slice(&commitment);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(pda(&[b"allow", pool.as_ref(), depositor.as_ref()]), false),
            AccountMeta::new(pda(&[b"deposit", pool.as_ref(), &commitment]), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*depositor_token, false),
            AccountMeta::new(vault_address(pool), false),
            AccountMeta::new(*depositor, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

pub fn withdraw_ix(
    pool: &Pubkey,
    mint: &Pubkey,
    treasury: &Pubkey,
    relayer: &Pubkey,
    recipient: &Pubkey,
    nullifier: [u8; 32],
    root: [u8; 32],
) -> Instruction {
    let mut data = disc("withdraw").to_vec();
    data.extend_from_slice(&nullifier);
    data.extend_from_slice(&root);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new(pda(&[b"nullifier", pool.as_ref(), &nullifier]), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_address(pool), false),
            AccountMeta::new(*recipient, false),
            AccountMeta::new(*treasury, false),
            AccountMeta::new(*relayer, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(solana_sdk::sysvar::instructions::ID, false),
        ],
        data,
    }
}

pub fn ragequit_ix(
    pool: &Pubkey,
    mint: &Pubkey,
    depositor: &Pubkey,
    depositor_token: &Pubkey,
    commitment: [u8; 32],
    nullifier: [u8; 32],
) -> Instruction {
    let mut data = disc("ragequit").to_vec();
    data.extend_from_slice(&commitment);
    data.extend_from_slice(&nullifier);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new(pda(&[b"deposit", pool.as_ref(), &commitment]), false),
            AccountMeta::new(pda(&[b"nullifier", pool.as_ref(), &nullifier]), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_address(pool), false),
            AccountMeta::new(*depositor_token, false),
            AccountMeta::new(*depositor, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(solana_sdk::sysvar::instructions::ID, false),
        ],
        data,
    }
}
