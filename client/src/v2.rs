//! The committee-free pool program (`programs/vineland-pool`, v2) from the
//! client side: its ids, account layouts, instruction builders, and the RPC
//! plumbing every v2 client needs (sending through public-RPC rate limits,
//! reading the deposit tree from chain, folding pending deposits). One copy,
//! shared by the devnet demo, the `vineland` CLI and the `relayer`.
//!
//! Layouts follow `programs/vineland-pool/src/lib.rs`: borsh for `#[account]`
//! structs (8-byte discriminator first), `repr(C)` for the zero-copy `Tree`.

use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSendTransactionConfig},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{
    account::Account,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    system_instruction, system_program,
    transaction::Transaction,
};

use crate::{DepositRecord, DEPOSIT_RECORD_LEN};

/// The v2 pool program.
pub const POOL_ID: Pubkey = solana_sdk::pubkey!("3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi");
/// The riverrun M31 staged verifier, hard-coded in the pool (`VERIFIER_ID`).
pub const VERIFIER_ID: Pubkey = solana_sdk::pubkey!("8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo");

pub const DEVNET_URL: &str = "https://api.devnet.solana.com";
/// mainnet-beta's genesis hash: the guard below refuses a cluster that has it,
/// whatever its URL says.
const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

/// Levels per `insert` call: 48 Poseidon2-M31 compressions, ~950k CU, so a
/// depth-127 fold is 3 calls.
pub const INSERT_LEVELS: u8 = 48;
const INSERT_CU: u32 = 1_400_000;
const DEPTH: u32 = 127;
pub const ROOT_HISTORY: usize = 16;

// `Tree` (zero-copy): filled[127][32], roots[16][32], root_sizes[16] u32,
// node[32], inserted, level, next_root, _reserved (u32 each).
const T_ROOTS: usize = 8 + DEPTH as usize * 32;
const T_ROOT_SIZES: usize = T_ROOTS + ROOT_HISTORY * 32;
const T_INSERTED: usize = T_ROOT_SIZES + ROOT_HISTORY * 4 + 32;
const T_LEVEL: usize = T_INSERTED + 4;
const T_NEXT_ROOT: usize = T_LEVEL + 4;
/// `Tree::SPACE`.
pub const TREE_LEN: usize = T_NEXT_ROOT + 8;
/// `Pool::SPACE`.
pub const POOL_LEN: usize = 173;

// ---- addresses ----------------------------------------------------------------

pub fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &POOL_ID).0
}

pub fn pool_address(authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[b"pool", authority.as_ref(), mint.as_ref()])
}

pub fn tree_address(pool: &Pubkey) -> Pubkey {
    pda(&[b"tree", pool.as_ref()])
}

pub fn vault_address(pool: &Pubkey) -> Pubkey {
    pda(&[b"vault", pool.as_ref()])
}

pub fn allow_address(pool: &Pubkey, member: &Pubkey) -> Pubkey {
    pda(&[b"allow", pool.as_ref(), member.as_ref()])
}

pub fn deposit_address(pool: &Pubkey, commitment: &[u8; 32]) -> Pubkey {
    pda(&[b"deposit", pool.as_ref(), commitment])
}

pub fn nullifier_address(pool: &Pubkey, nullifier: &[u8; 32]) -> Pubkey {
    pda(&[b"nullifier", pool.as_ref(), nullifier])
}

/// The associated token account of `wallet` for `mint` (classic SPL Token).
pub fn ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account_client::address::get_associated_token_address(wallet, mint)
}

/// Create [`ata`] if it does not exist; a no-op if it does.
pub fn create_ata_ix(payer: &Pubkey, wallet: &Pubkey, mint: &Pubkey) -> Instruction {
    spl_associated_token_account_client::instruction::create_associated_token_account_idempotent(
        payer,
        wallet,
        mint,
        &spl_token::id(),
    )
}

// ---- accounts ---------------------------------------------------------------------

fn disc(namespace: &str, name: &str) -> [u8; 8] {
    Sha256::digest(format!("{namespace}:{name}").as_bytes())[..8].try_into().unwrap()
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}

/// The v2 `Pool` account.
#[derive(Clone, Debug)]
pub struct PoolV2 {
    pub authority: Pubkey,
    pub mint: Pubkey,
    pub treasury: Pubkey,
    pub note_amount: u64,
    pub fee_bps: u16,
    pub k_min: u32,
    pub action: [u8; 32],
    pub round: u64,
    pub leaf_count: u32,
    pub live_notes: u32,
    pub exits: u32,
}

impl PoolV2 {
    /// Refuses anything that is not exactly a `Pool` (length and discriminator),
    /// so another account of the program is never read as one.
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() != POOL_LEN || d[..8] != disc("account", "Pool") {
            return None;
        }
        let key = |o: usize| Pubkey::try_from(&d[o..o + 32]).ok();
        Some(PoolV2 {
            authority: key(8)?,
            mint: key(40)?,
            treasury: key(72)?,
            note_amount: u64::from_le_bytes(d[104..112].try_into().unwrap()),
            fee_bps: u16::from_le_bytes(d[112..114].try_into().unwrap()),
            k_min: u32_at(d, 114),
            action: d[118..150].try_into().unwrap(),
            round: u64::from_le_bytes(d[150..158].try_into().unwrap()),
            leaf_count: u32_at(d, 158),
            live_notes: u32_at(d, 162),
            exits: u32_at(d, 166),
        })
    }

    pub fn fee(&self) -> u64 {
        ((self.note_amount as u128 * self.fee_bps as u128) / 10_000) as u64
    }
}

/// What a client needs of the on-chain `Tree`: the fold counters and the root
/// history (the frontier stays on chain).
#[derive(Clone, Debug)]
pub struct TreeState {
    /// Leaves fully folded in; the latest root covers exactly these.
    pub inserted: u32,
    /// Next level of a fold in progress (0: none).
    pub level: u32,
    pub next_root: u32,
    roots: [([u8; 32], u32); ROOT_HISTORY],
}

impl TreeState {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() != TREE_LEN || d[..8] != disc("account", "Tree") {
            return None;
        }
        Some(TreeState {
            inserted: u32_at(d, T_INSERTED),
            level: u32_at(d, T_LEVEL),
            next_root: u32_at(d, T_NEXT_ROOT),
            roots: core::array::from_fn(|i| {
                (d[T_ROOTS + 32 * i..T_ROOTS + 32 * (i + 1)].try_into().unwrap(), u32_at(d, T_ROOT_SIZES + 4 * i))
            }),
        })
    }

    /// How many leaves `root` covers, if the pool would accept it now (the
    /// program's `Tree::size_of`).
    pub fn size_of(&self, root: &[u8; 32]) -> Option<u32> {
        self.roots.iter().find(|(r, s)| *s > 0 && r == root).map(|(_, s)| *s)
    }

    /// Every root in the history with the leaves it covers, newest first.
    pub fn recent_roots(&self) -> Vec<([u8; 32], u32)> {
        (1..=ROOT_HISTORY as u32)
            .map(|k| self.roots[(self.next_root.wrapping_sub(k) as usize) % ROOT_HISTORY])
            .filter(|(_, s)| *s > 0)
            .collect()
    }

    pub fn latest(&self) -> Option<([u8; 32], u32)> {
        self.recent_roots().first().copied()
    }
}

// ---- instructions -------------------------------------------------------------------

fn ix_data(name: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut data = disc("global", name).to_vec();
    for a in args {
        data.extend_from_slice(a);
    }
    data
}

pub fn initialize_ix(authority: &Pubkey, mint: &Pubkey, treasury: &Pubkey, note_amount: u64, fee_bps: u16, k_min: u32) -> Instruction {
    let pool = pool_address(authority, mint);
    Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(tree_address(&pool), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_address(&pool), false),
            AccountMeta::new_readonly(*treasury, false),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ix_data("initialize", &[&note_amount.to_le_bytes(), &fee_bps.to_le_bytes(), &k_min.to_le_bytes()]),
    }
}

pub fn allow_ix(pool: &Pubkey, operator: &Pubkey, member: &Pubkey) -> Instruction {
    Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new_readonly(*pool, false),
            AccountMeta::new(allow_address(pool, member), false),
            AccountMeta::new(*operator, true),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ix_data("allow", &[member.as_ref()]),
    }
}

pub fn deposit_ix(pool: &Pubkey, mint: &Pubkey, member: &Pubkey, token: &Pubkey, commitment: [u8; 32]) -> Instruction {
    Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(allow_address(pool, member), false),
            AccountMeta::new(deposit_address(pool, &commitment), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*token, false),
            AccountMeta::new(vault_address(pool), false),
            AccountMeta::new(*member, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ix_data("deposit", &[&commitment]),
    }
}

/// One `insert` call (with its compute limit) folding up to [`INSERT_LEVELS`]
/// levels of the leaf `commitment`, which must be the next one in order.
pub fn insert_ixs(pool: &Pubkey, commitment: [u8; 32]) -> Vec<Instruction> {
    vec![
        ComputeBudgetInstruction::set_compute_unit_limit(INSERT_CU),
        Instruction {
            program_id: POOL_ID,
            accounts: vec![
                AccountMeta::new_readonly(*pool, false),
                AccountMeta::new(tree_address(pool), false),
                AccountMeta::new_readonly(deposit_address(pool, &commitment), false),
            ],
            data: ix_data("insert", &[&[INSERT_LEVELS]]),
        },
    ]
}

/// `recipient` must be the token account the buffer's proofs are bound to; the
/// program compares it with the buffer and refuses any other.
pub fn withdraw_ix(pool: &Pubkey, mint: &Pubkey, treasury: &Pubkey, buffer: &Pubkey, recipient: &Pubkey, relayer: &Pubkey, nullifier: [u8; 32]) -> Instruction {
    Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(tree_address(pool), false),
            AccountMeta::new_readonly(*buffer, false),
            AccountMeta::new(nullifier_address(pool, &nullifier), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_address(pool), false),
            AccountMeta::new(*recipient, false),
            AccountMeta::new(*treasury, false),
            AccountMeta::new(*relayer, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ix_data("withdraw", &[&nullifier]),
    }
}

/// A public exit of the deposit `commitment`, revealing `secret` (the note's
/// eight limbs, [`crate::enc`]) and burning `nullifier`. The refund goes to
/// `depositor_token`, which the program requires the depositor to own.
pub fn exit_ix(
    pool: &Pubkey,
    mint: &Pubkey,
    commitment: [u8; 32],
    secret: [u8; 32],
    nullifier: [u8; 32],
    depositor_token: &Pubkey,
    payer: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new(deposit_address(pool, &commitment), false),
            AccountMeta::new(nullifier_address(pool, &nullifier), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_address(pool), false),
            AccountMeta::new(*depositor_token, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ix_data("exit", &[&secret, &nullifier]),
    }
}

/// A plain (non-associated) token account at `acct`, which must co-sign.
pub fn token_account_ixs(payer: &Pubkey, acct: &Pubkey, mint: &Pubkey, owner: &Pubkey, rent: u64) -> Vec<Instruction> {
    vec![
        system_instruction::create_account(payer, acct, rent, spl_token::state::Account::LEN as u64, &spl_token::id()),
        spl_token::instruction::initialize_account3(&spl_token::id(), acct, mint, owner).unwrap(),
    ]
}

/// Name the program errors a client is likely to hit. Codes: Anchor's 3012,
/// the system program's `AccountAlreadyInUse` (0) on a PDA that exists, and the
/// pool's own (6000 + declaration order).
pub fn explain(line: &str) -> String {
    let code = line.split("custom program error: ").nth(1).map(|c| c.split([';', ' ', '"']).next().unwrap_or(c).trim().to_string());
    let name = match code.as_deref() {
        Some("0xbc4") => "AccountNotInitialized: the account (an allowlist entry?) does not exist",
        Some("0x0") => "AccountAlreadyInUse: this PDA exists (nullifier already spent, or already deposited/allowed)",
        Some("0x1770") => "InvalidConfig: note size, fee or floor out of range",
        Some("0x1771") => "ProofNotVerified: the verifier has not finalized this buffer",
        Some("0x1772") => "ProofMismatch: the proofs are for another pool, nullifier or recipient",
        Some("0x1773") => "UnknownRoot: the proof's root is not in the pool's recent history",
        Some("0x1774") => "OutOfOrder: leaves enter the tree in deposit order",
        Some("0x1775") => "AnonymitySetTooSmall: the set under the root is below k_min",
        Some("0x1776") => "InvalidCommitment: all-zero commitment",
        Some("0x1777") => "InvalidRecipient: the recipient is the vault",
        Some("0x1778") => "SecretMismatch: the secret does not open this deposit",
        Some("0x1779") => "NotDepositorAccount: the refund account is not the depositor's",
        Some("0x177a") => "AlreadyExited: this note already exited",
        Some("0x177d") => "NonCanonical: a digest limb is not canonical M31",
        _ => return line.to_string(),
    };
    format!("{} ({name})", code.unwrap())
}

// ---- RPC ---------------------------------------------------------------------------

/// An RPC endpoint at `confirmed` commitment, with the v2 reads and the
/// rate-limit-tolerant sends every client here uses.
pub struct Rpc {
    pub client: RpcClient,
    pub url: String,
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        Rpc { client: RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::confirmed()), url: url.to_string() }
    }

    /// The programs are not audited: no binary here may talk to mainnet. Refuses
    /// a URL that names it and, failing that, a cluster with mainnet-beta's
    /// genesis hash (a private RPC URL need not say "mainnet").
    pub fn refuse_mainnet(&self) -> Result<(), String> {
        if self.url.to_ascii_lowercase().contains("mainnet") {
            return Err(format!("refusing to run against {}: the Vineland programs are not audited; use devnet or a local validator", self.url));
        }
        let genesis = self.client.get_genesis_hash().map_err(|e| format!("cannot reach {}: {e}", self.url))?;
        if genesis.to_string() == MAINNET_GENESIS {
            return Err(format!("refusing to run against {}: it is mainnet-beta (genesis {genesis}); the programs are not audited", self.url));
        }
        Ok(())
    }

    /// Send and confirm `ixs`, surviving public-RPC rate limits: the signed
    /// transaction is resent as is (idempotent) and its status is checked
    /// before any re-sign, so a landed transaction is never sent twice.
    /// Returns the signature and the transaction's wire size.
    pub fn land(&self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<(Signature, usize), String> {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        for attempt in 0..10 {
            let bh = match self.client.get_latest_blockhash() {
                Ok(bh) => bh,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1500));
                    continue;
                }
            };
            let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, bh);
            let size = 1 + 64 * tx.signatures.len() + tx.message.serialize().len();
            let sig = tx.signatures[0];
            match self.client.send_and_confirm_transaction(&tx) {
                Ok(sig) => return Ok((sig, size)),
                Err(e) => {
                    let msg = e.to_string();
                    if let Ok(Some(status)) = self.client.get_signature_status(&sig) {
                        return match status {
                            Ok(()) => Ok((sig, size)),
                            Err(err) => Err(format!("{err}: {msg}")),
                        };
                    }
                    let transient = msg.contains("429")
                        || msg.contains("Too Many")
                        || msg.contains("blockhash")
                        || msg.contains("timed out")
                        || msg.contains("unable to confirm")
                        || msg.contains("error sending request");
                    if !transient {
                        return Err(msg);
                    }
                    std::thread::sleep(Duration::from_millis(1500 * (attempt + 1)));
                }
            }
        }
        Err("gave up after 10 attempts".into())
    }

    /// Land a batch of independent, idempotent transactions (the verifier's
    /// WRITEs: any order, any number of times, until SEAL) without waiting for
    /// each: send all, then poll their statuses, and re-sign with a fresh
    /// blockhash only those not seen by the time the round ends. A WRITE landing
    /// twice rewrites the same bytes; one landing after SEAL fails harmlessly.
    pub fn land_all(&self, batch: &[Vec<Instruction>], payer: &Keypair) -> Result<Vec<Signature>, String> {
        let mut sigs: Vec<Option<Signature>> = vec![None; batch.len()];
        let mut done = vec![false; batch.len()];
        let no_preflight = RpcSendTransactionConfig { skip_preflight: true, ..Default::default() };
        for _round in 0..5 {
            let pending: Vec<usize> = (0..batch.len()).filter(|&i| !done[i]).collect();
            if pending.is_empty() {
                break;
            }
            let bh = self.client.get_latest_blockhash().map_err(|e| format!("blockhash: {e}"))?;
            for &i in &pending {
                let tx = Transaction::new_signed_with_payer(&batch[i], Some(&payer.pubkey()), &[payer], bh);
                sigs[i] = Some(tx.signatures[0]);
                for attempt in 0..5u64 {
                    match self.client.send_transaction_with_config(&tx, no_preflight) {
                        Ok(_) => break,
                        Err(_) => std::thread::sleep(Duration::from_millis(500 * (attempt + 1))),
                    }
                }
            }
            // A blockhash lives ~60-90 s; poll well inside that, then re-sign.
            let deadline = Instant::now() + Duration::from_secs(45);
            while Instant::now() < deadline && done.iter().any(|d| !d) {
                std::thread::sleep(Duration::from_millis(800));
                let waiting: Vec<usize> = (0..batch.len()).filter(|&i| !done[i]).collect();
                for chunk in waiting.chunks(200) {
                    let ask: Vec<Signature> = chunk.iter().map(|&i| sigs[i].unwrap()).collect();
                    let Ok(resp) = self.client.get_signature_statuses(&ask) else { continue };
                    for (&i, status) in chunk.iter().zip(resp.value) {
                        if let Some(s) = status {
                            if let Some(err) = s.err {
                                return Err(format!("{}: {err}", sigs[i].unwrap()));
                            }
                            if s.satisfies_commitment(CommitmentConfig::confirmed()) {
                                done[i] = true;
                            }
                        }
                    }
                }
            }
        }
        if done.iter().any(|d| !d) {
            return Err(format!("{} of {} transactions did not land after 5 rounds", done.iter().filter(|d| !**d).count(), batch.len()));
        }
        Ok(sigs.into_iter().map(Option::unwrap).collect())
    }

    pub fn account(&self, key: &Pubkey) -> Result<Option<Account>, String> {
        self.client
            .get_account_with_commitment(key, CommitmentConfig::confirmed())
            .map(|r| r.value)
            .map_err(|e| format!("read {key}: {e}"))
    }

    pub fn exists(&self, key: &Pubkey) -> Result<bool, String> {
        Ok(self.account(key)?.is_some())
    }

    /// The pool at `pool`, refusing an account the v2 program does not own.
    pub fn pool(&self, pool: &Pubkey) -> Result<PoolV2, String> {
        let a = self.account(pool)?.ok_or_else(|| format!("no account at {pool}"))?;
        if a.owner != POOL_ID {
            return Err(format!("{pool} is not a v2 Vineland pool (owner {})", a.owner));
        }
        PoolV2::parse(&a.data).ok_or_else(|| format!("{pool} is not a v2 Vineland pool account"))
    }

    pub fn tree_state(&self, pool: &Pubkey) -> Result<TreeState, String> {
        let t = tree_address(pool);
        let a = self.account(&t)?.ok_or_else(|| format!("no tree account at {t}"))?;
        if a.owner != POOL_ID {
            return Err(format!("{t} is not owned by the pool program"));
        }
        TreeState::parse(&a.data).ok_or_else(|| format!("{t} is not a v2 tree account"))
    }

    pub fn deposit_record(&self, pool: &Pubkey, commitment: &[u8; 32]) -> Result<Option<DepositRecord>, String> {
        match self.account(&deposit_address(pool, commitment))? {
            Some(a) if a.owner == POOL_ID => Ok(DepositRecord::parse(&a.data).filter(|r| r.pool == *pool)),
            _ => Ok(None),
        }
    }

    /// Every deposit of `pool`, from the chain (one getProgramAccounts with a
    /// memcmp on the pool address at offset 8), sorted by leaf index.
    pub fn deposits(&self, pool: &Pubkey) -> Result<Vec<DepositRecord>, String> {
        let cfg = RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::DataSize(DEPOSIT_RECORD_LEN as u64),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(8, pool.as_ref())),
            ]),
            account_config: RpcAccountInfoConfig { commitment: Some(CommitmentConfig::confirmed()), ..Default::default() },
            ..Default::default()
        };
        let mut deposits: Vec<DepositRecord> = self
            .client
            .get_program_accounts_with_config(&POOL_ID, cfg)
            .map_err(|e| format!("deposit records of {pool}: {e}"))?
            .into_iter()
            .filter_map(|(_, a)| DepositRecord::parse(&a.data))
            .filter(|d| d.pool == *pool)
            .collect();
        deposits.sort_by_key(|d| d.index);
        Ok(deposits)
    }

    /// An SPL token account's balance, `None` if there is no such account.
    pub fn token_balance(&self, acct: &Pubkey) -> Result<Option<u64>, String> {
        match self.account(acct)? {
            Some(a) if a.owner == spl_token::id() => spl_token::state::Account::unpack(&a.data)
                .map(|t| Some(t.amount))
                .map_err(|e| format!("{acct} is not a token account: {e}")),
            Some(a) => Err(format!("{acct} is not a token account (owner {})", a.owner)),
            None => Ok(None),
        }
    }

    /// Fold every deposit not yet in the tree, in order, `payer` paying.
    /// Signatures are pushed to `sigs` as they land, so a caller keeps them on
    /// failure. Tolerates a concurrent folder (OutOfOrder: re-read and go on).
    pub fn crank(&self, pool: &Pubkey, payer: &Keypair, sigs: &mut Vec<Signature>) -> Result<(), String> {
        let mut deposits = self.deposits(pool)?;
        let mut budget: Option<u32> = None;
        loop {
            let p = self.pool(pool)?;
            let t = self.tree_state(pool)?;
            if t.inserted >= p.leaf_count {
                return Ok(());
            }
            // 3 calls per leaf, plus slack for refetches and lost races.
            let left = budget.get_or_insert((p.leaf_count - t.inserted) * DEPTH.div_ceil(INSERT_LEVELS as u32) + 4);
            if *left == 0 {
                return Err(format!("fold made no progress (inserted {} of {})", t.inserted, p.leaf_count));
            }
            *left -= 1;
            let Some(rec) = deposits.iter().find(|d| d.index == t.inserted).cloned() else {
                deposits = self.deposits(pool)?;
                continue;
            };
            match self.land(&insert_ixs(pool, rec.commitment), payer, &[]) {
                Ok((sig, _)) => sigs.push(sig),
                Err(e) if e.contains("0x1774") => continue,
                Err(e) => return Err(explain(e.lines().next().unwrap_or(&e))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_program() {
        // Pool::SPACE and Tree::SPACE in programs/vineland-pool/src/lib.rs.
        assert_eq!(POOL_LEN, 8 + 32 + 32 + 32 + 8 + 2 + 4 + 32 + 8 + 4 + 4 + 4 + 1 + 1 + 1);
        assert_eq!(TREE_LEN, 8 + 127 * 32 + 16 * 32 + 16 * 4 + 32 + 4 * 4);
        assert_eq!(T_ROOTS, 8 + 127 * 32, "the demo's T_ROOTS");
    }

    #[test]
    fn recent_roots_are_newest_first_and_skip_empty_slots() {
        let mut d = vec![0u8; TREE_LEN];
        d[..8].copy_from_slice(&disc("account", "Tree"));
        // Three roots folded: slots 0, 1, 2 cover 1, 2, 3 leaves.
        for i in 0..3usize {
            d[T_ROOTS + 32 * i] = i as u8 + 1;
            d[T_ROOT_SIZES + 4 * i..T_ROOT_SIZES + 4 * i + 4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
        }
        d[T_INSERTED..T_INSERTED + 4].copy_from_slice(&3u32.to_le_bytes());
        d[T_NEXT_ROOT..T_NEXT_ROOT + 4].copy_from_slice(&3u32.to_le_bytes());
        let t = TreeState::parse(&d).unwrap();
        let sizes: Vec<u32> = t.recent_roots().iter().map(|r| r.1).collect();
        assert_eq!(sizes, vec![3, 2, 1]);
        assert_eq!(t.latest().unwrap().0[0], 3);
        assert_eq!(t.size_of(&t.latest().unwrap().0), Some(3));
        assert_eq!(t.size_of(&[0u8; 32]), None, "an empty slot is not a root");
        d[0] ^= 1;
        assert!(TreeState::parse(&d).is_none(), "wrong discriminator");
    }

    #[test]
    fn explain_names_pool_errors() {
        let line = "RPC response error -32002: Transaction simulation failed: Error processing Instruction 1: custom program error: 0x1774";
        assert!(explain(line).contains("OutOfOrder"));
        assert_eq!(explain("something else"), "something else");
    }
}
