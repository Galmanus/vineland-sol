//! The pool's invariants, each pinned against the compiled `.so` in LiteSVM.
//!
//! Run: `cargo-build-sbf && cargo test` from `programs/vineland-pool`.
//!
//! Every rejection test asserts the exact error, so a test cannot pass because
//! the transaction failed for some unrelated reason. The verifier buffer here is
//! written directly into an account owned by `VERIFIER_ID`, byte for byte in the
//! verifier's layout, so these tests pin what the pool accepts; that the
//! verifier only writes such a buffer after both proofs verify is pinned by the
//! verifier's own tests and by the client's end-to-end test.

use litesvm::LiteSVM;
use riverrun_m31::{compress, permute, CONTEXT_LEN, CROWD_DEPTH, DIGEST_LEN, SECRET_LEN, WIDTH};
use sha2::{Digest, Sha256};
use solana_sdk::{
    account::Account,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction, InstructionError},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::{Transaction, TransactionError},
};
use std::collections::BTreeMap;

const PROGRAM_ID: Pubkey = solana_sdk::pubkey!("3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi");
const VERIFIER_ID: Pubkey = solana_sdk::pubkey!("8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo");
const NOTE: u64 = 1_000_000; // 1.0 of a 6-decimal token
const FEE_BPS: u16 = 197; // 1.97%
const FEE: u64 = 19_700;
const DECIMALS: u8 = 6;
const M31_P: u64 = (1 << 31) - 1;
/// Levels folded per `insert` call: 48 compressions stay under the 1.4M cap.
const LEVELS: u8 = 48;

// Anchor custom errors, in declaration order from 6000.
const PROOF_NOT_VERIFIED: u32 = 6001;
const PROOF_MISMATCH: u32 = 6002;
const UNKNOWN_ROOT: u32 = 6003;
const OUT_OF_ORDER: u32 = 6004;
const ANONYMITY_SET_TOO_SMALL: u32 = 6005;
const SECRET_MISMATCH: u32 = 6008;
const NOT_DEPOSITOR_ACCOUNT: u32 = 6009;
const NON_CANONICAL: u32 = 6013;
// Anchor framework errors.
const ACCOUNT_NOT_INITIALIZED: u32 = 3012;

// Verifier buffer layout (riverrun-m31-verifier/src/staged.rs at ce5e8c7).
const B_FINALIZED: usize = 40;
const B_ROOT: usize = 41;
const B_NULLIFIER: usize = 73;
const B_ROUND: usize = 105;
const B_ACTION: usize = 121;
const B_RECIPIENT: usize = 153;
const B_VERSION: usize = 185;
const B_KIND: usize = 186;
const B_PROOFS: usize = 2432;

// `Tree` account layout (zero-copy, after the 8-byte discriminator).
const T_ROOTS: usize = 8 + 127 * 32;
const T_SIZES: usize = T_ROOTS + 16 * 32;

fn disc(name: &str) -> [u8; 8] {
    let h = Sha256::digest(format!("global:{name}").as_bytes());
    h[..8].try_into().unwrap()
}

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &PROGRAM_ID).0
}

fn custom_code(err: &TransactionError) -> Option<u32> {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
        _ => None,
    }
}

/// The nullifier PDA already exists: the system program refuses to create it
/// again (`AccountAlreadyInUse`, custom 0) inside the instruction at `ix`.
fn assert_already_spent(err: &TransactionError, ix: u8) {
    assert_eq!(
        err,
        &TransactionError::InstructionError(ix, InstructionError::Custom(0)),
        "expected the nullifier PDA to already exist"
    );
}

type D8 = [u64; DIGEST_LEN];

fn enc(d: &D8) -> [u8; 32] {
    let mut b = [0u8; 32];
    for (i, v) in d.iter().enumerate() {
        b[4 * i..4 * i + 4].copy_from_slice(&(*v as u32).to_le_bytes());
    }
    b
}

// ---- the client's derivations, restated so the test is independent of the program

fn note_action(pool: &Pubkey) -> [u64; CONTEXT_LEN] {
    let h = Sha256::new().chain_update(b"vineland/note/v1").chain_update(pool.as_ref()).finalize();
    core::array::from_fn(|i| {
        let v = u32::from_le_bytes(h[4 * i..4 * i + 4].try_into().unwrap()) as u64 & M31_P;
        if v == M31_P { 0 } else { v }
    })
}

fn spend_round_u64(pool: &Pubkey) -> u64 {
    let h = Sha256::new().chain_update(b"vineland/spend/v2").chain_update(pool.as_ref()).finalize();
    u64::from_le_bytes(h[..8].try_into().unwrap())
}

fn spend_round(pool: &Pubkey) -> [u64; CONTEXT_LEN] {
    let r = spend_round_u64(pool);
    core::array::from_fn(|i| if i < 4 { (r >> (16 * i)) & 0xffff } else { 0 })
}

struct Note {
    secret: [u64; SECRET_LEN],
}

impl Note {
    fn new(seed: u64) -> Self {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        Note {
            secret: core::array::from_fn(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % M31_P
            }),
        }
    }
    fn digest(&self, ctx: [u64; CONTEXT_LEN]) -> D8 {
        let mut input = [0u64; WIDTH];
        input[..SECRET_LEN].copy_from_slice(&self.secret);
        input[SECRET_LEN..].copy_from_slice(&ctx);
        let out = permute(input);
        core::array::from_fn(|i| out[i])
    }
    fn commitment(&self, pool: &Pubkey) -> [u8; 32] {
        enc(&self.digest(note_action(pool)))
    }
    fn nullifier(&self, pool: &Pubkey) -> [u8; 32] {
        enc(&self.digest(spend_round(pool)))
    }
    fn secret_bytes(&self) -> [u8; 32] {
        enc(&self.secret)
    }
}

/// The client's sparse-tree fold (client/src/lib.rs `Tree::fold`), root only.
fn root_of(leaves: &[[u8; 32]]) -> [u8; 32] {
    let dec = |b: &[u8; 32]| -> D8 { core::array::from_fn(|i| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap()) as u64) };
    let mut empty = [[0u64; DIGEST_LEN]; CROWD_DEPTH + 1];
    for k in 1..=CROWD_DEPTH {
        empty[k] = compress(empty[k - 1], empty[k - 1]);
    }
    let mut level: BTreeMap<u128, D8> = leaves.iter().enumerate().map(|(i, l)| (i as u128, dec(l))).collect();
    for k in 0..CROWD_DEPTH {
        let get = |lvl: &BTreeMap<u128, D8>, idx: u128| lvl.get(&idx).copied().unwrap_or(empty[k]);
        let mut parents = BTreeMap::new();
        for &idx in level.keys() {
            let p = idx >> 1;
            parents.entry(p).or_insert_with(|| compress(get(&level, p << 1), get(&level, (p << 1) | 1)));
        }
        level = parents;
    }
    enc(&level.get(&0).copied().unwrap_or(empty[CROWD_DEPTH]))
}

/// What the verifier leaves in a buffer after FINALIZE of a recipient-bound
/// crowd verification. Tests mutate one field at a time from here.
struct Buffer {
    data: Vec<u8>,
}

impl Buffer {
    fn settleable(pool: &Pubkey, root: [u8; 32], nullifier: [u8; 32], recipient: &Pubkey) -> Self {
        let mut data = vec![0u8; B_PROOFS + 64];
        data[B_VERSION] = 1;
        data[B_FINALIZED] = 1;
        data[B_KIND] = 0x83;
        data[B_ROOT..B_ROOT + 32].copy_from_slice(&root);
        data[B_NULLIFIER..B_NULLIFIER + 32].copy_from_slice(&nullifier);
        data[B_ROUND..B_ROUND + 16].copy_from_slice(&(spend_round_u64(pool) as u128).to_le_bytes());
        data[B_ACTION..B_ACTION + 32].copy_from_slice(&enc(&note_action(pool)));
        data[B_RECIPIENT..B_RECIPIENT + 32].copy_from_slice(recipient.as_ref());
        Buffer { data }
    }
}

struct Env {
    svm: LiteSVM,
    authority: Keypair,
    mint: Pubkey,
    pool: Pubkey,
    tree: Pubkey,
    vault: Pubkey,
    treasury: Pubkey,
    relayer: Keypair,
    /// Commitments in deposit order, for the expected roots.
    leaves: Vec<[u8; 32]>,
}

impl Env {
    fn new(k_min: u32) -> Self {
        let mut svm = LiteSVM::new();
        let so = concat!(env!("CARGO_MANIFEST_DIR"), "/target/deploy/vineland_pool.so");
        svm.add_program_from_file(PROGRAM_ID, so).expect("run `cargo-build-sbf` first");

        let authority = Keypair::new();
        svm.airdrop(&authority.pubkey(), 10_000_000_000).unwrap();
        let relayer = Keypair::new();
        svm.airdrop(&relayer.pubkey(), 1_000_000_000).unwrap();

        let mint = Keypair::new();
        let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
        let ixs = [
            system_instruction::create_account(
                &authority.pubkey(),
                &mint.pubkey(),
                rent,
                spl_token::state::Mint::LEN as u64,
                &spl_token::id(),
            ),
            spl_token::instruction::initialize_mint2(&spl_token::id(), &mint.pubkey(), &authority.pubkey(), None, DECIMALS)
                .unwrap(),
        ];
        let tx = Transaction::new_signed_with_payer(&ixs, Some(&authority.pubkey()), &[&authority, &mint], svm.latest_blockhash());
        svm.send_transaction(tx).unwrap();
        let mint = mint.pubkey();

        let mut env = Env {
            svm,
            authority,
            mint,
            pool: Pubkey::default(),
            tree: Pubkey::default(),
            vault: Pubkey::default(),
            treasury: Pubkey::default(),
            relayer,
            leaves: vec![],
        };
        let owner = env.authority.pubkey();
        env.treasury = env.token_account(&owner);
        env.pool = pda(&[b"pool", owner.as_ref(), mint.as_ref()]);
        env.tree = pda(&[b"tree", env.pool.as_ref()]);
        env.vault = pda(&[b"vault", env.pool.as_ref()]);

        let mut data = disc("initialize").to_vec();
        data.extend_from_slice(&NOTE.to_le_bytes());
        data.extend_from_slice(&FEE_BPS.to_le_bytes());
        data.extend_from_slice(&k_min.to_le_bytes());
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(env.pool, false),
                AccountMeta::new(env.tree, false),
                AccountMeta::new_readonly(env.mint, false),
                AccountMeta::new(env.vault, false),
                AccountMeta::new_readonly(env.treasury, false),
                AccountMeta::new(owner, true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let a = env.authority.insecure_clone();
        env.send(&[ix], &a, &[]).expect("initialize");
        env
    }

    fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<u64, TransactionError> {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, self.svm.latest_blockhash());
        let res = self.svm.send_transaction(tx).map(|m| m.compute_units_consumed).map_err(|f| f.err);
        self.svm.expire_blockhash();
        res
    }

    fn token_account(&mut self, owner: &Pubkey) -> Pubkey {
        let acct = Keypair::new();
        let rent = self.svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
        let ixs = [
            system_instruction::create_account(
                &self.authority.pubkey(),
                &acct.pubkey(),
                rent,
                spl_token::state::Account::LEN as u64,
                &spl_token::id(),
            ),
            spl_token::instruction::initialize_account3(&spl_token::id(), &acct.pubkey(), &self.mint, owner).unwrap(),
        ];
        let a = self.authority.insecure_clone();
        self.send(&ixs, &a, &[&acct]).unwrap();
        acct.pubkey()
    }

    fn balance(&self, account: &Pubkey) -> u64 {
        let data = self.svm.get_account(account).unwrap().data;
        spl_token::state::Account::unpack(&data).unwrap().amount
    }

    fn pool_data(&self) -> Vec<u8> {
        self.svm.get_account(&self.pool).unwrap().data
    }

    /// `Pool` layout: 8 disc, 3 keys, note u64, fee u16, k_min u32, action
    /// [32], round u64, leaf_count, live_notes, exits.
    fn live_notes(&self) -> u32 {
        let d = self.pool_data();
        u32::from_le_bytes(d[162..166].try_into().unwrap())
    }

    fn member(&mut self, notes: u64) -> (Keypair, Pubkey) {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 1_000_000_000).unwrap();
        let ta = self.token_account(&kp.pubkey());
        let ix = spl_token::instruction::mint_to(&spl_token::id(), &self.mint, &ta, &self.authority.pubkey(), &[], notes * NOTE)
            .unwrap();
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[]).unwrap();
        (kp, ta)
    }

    fn allow(&mut self, member: &Pubkey) {
        let mut data = disc("allow").to_vec();
        data.extend_from_slice(member.as_ref());
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(self.pool, false),
                AccountMeta::new(pda(&[b"allow", self.pool.as_ref(), member.as_ref()]), false),
                AccountMeta::new(self.authority.pubkey(), true),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[]).unwrap();
    }

    fn deposit(&mut self, member: &Keypair, token: &Pubkey, commitment: [u8; 32]) -> Result<u64, TransactionError> {
        let mut data = disc("deposit").to_vec();
        data.extend_from_slice(&commitment);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new_readonly(pda(&[b"allow", self.pool.as_ref(), member.pubkey().as_ref()]), false),
                AccountMeta::new(pda(&[b"deposit", self.pool.as_ref(), &commitment]), false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(*token, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(member.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let res = self.send(&[ix], member, &[]);
        if res.is_ok() {
            self.leaves.push(commitment);
        }
        res
    }

    fn insert_once(&mut self, commitment: [u8; 32], levels: u8) -> Result<u64, TransactionError> {
        let mut data = disc("insert").to_vec();
        data.push(levels);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(self.pool, false),
                AccountMeta::new(self.tree, false),
                AccountMeta::new_readonly(pda(&[b"deposit", self.pool.as_ref(), &commitment]), false),
            ],
            data,
        };
        let r = self.relayer.insecure_clone();
        self.send(&[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), ix], &r, &[])
    }

    /// Fold one deposit to the top; returns the CU of each call.
    fn insert(&mut self, commitment: [u8; 32]) -> Vec<u64> {
        (0..3).map(|_| self.insert_once(commitment, LEVELS).expect("insert")).collect()
    }

    fn tree_root(&self, slot: usize) -> ([u8; 32], u32) {
        let d = self.svm.get_account(&self.tree).unwrap().data;
        let root = d[T_ROOTS + 32 * slot..T_ROOTS + 32 * slot + 32].try_into().unwrap();
        let size = u32::from_le_bytes(d[T_SIZES + 4 * slot..T_SIZES + 4 * slot + 4].try_into().unwrap());
        (root, size)
    }

    /// `k` approved members, each with one real note deposited and folded in.
    fn with_deposits(&mut self, k: u8) -> Vec<(Keypair, Pubkey, Note)> {
        let mut out = vec![];
        for _ in 0..k {
            let (m, ta) = self.member(1);
            self.allow(&m.pubkey());
            // Seeded by deposit count, so repeated calls never reuse a commitment.
            let note = Note::new(1000 + self.leaves.len() as u64);
            let c = note.commitment(&self.pool);
            self.deposit(&m, &ta, c).unwrap();
            self.insert(c);
            out.push((m, ta, note));
        }
        out
    }

    fn current_root(&self) -> [u8; 32] {
        root_of(&self.leaves)
    }

    /// Put `buffer` on chain as an account owned by `owner`.
    fn put_buffer(&mut self, buffer: &Buffer, owner: Pubkey) -> Pubkey {
        let key = Pubkey::new_unique();
        let lamports = self.svm.minimum_balance_for_rent_exemption(buffer.data.len());
        self.svm
            .set_account(key, Account { lamports, data: buffer.data.clone(), owner, executable: false, rent_epoch: 0 })
            .unwrap();
        key
    }

    fn withdraw(&mut self, nullifier: [u8; 32], proof: &Pubkey, recipient: &Pubkey) -> Result<u64, TransactionError> {
        let mut data = disc("withdraw").to_vec();
        data.extend_from_slice(&nullifier);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new_readonly(self.tree, false),
                AccountMeta::new_readonly(*proof, false),
                AccountMeta::new(pda(&[b"nullifier", self.pool.as_ref(), &nullifier]), false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(*recipient, false),
                AccountMeta::new(self.treasury, false),
                AccountMeta::new(self.relayer.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let r = self.relayer.insecure_clone();
        self.send(&[ix], &r, &[])
    }

    /// A settleable buffer for `note`, bound to `recipient`, then the withdrawal.
    fn withdraw_note(&mut self, note: &Note, recipient: &Pubkey) -> Result<u64, TransactionError> {
        let nf = note.nullifier(&self.pool);
        let buf = Buffer::settleable(&self.pool, self.current_root(), nf, recipient);
        let key = self.put_buffer(&buf, VERIFIER_ID);
        self.withdraw(nf, &key, recipient)
    }

    fn exit(&mut self, secret: [u8; 32], nullifier: [u8; 32], commitment: [u8; 32], token: &Pubkey) -> Result<u64, TransactionError> {
        let mut data = disc("exit").to_vec();
        data.extend_from_slice(&secret);
        data.extend_from_slice(&nullifier);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new(pda(&[b"deposit", self.pool.as_ref(), &commitment]), false),
                AccountMeta::new(pda(&[b"nullifier", self.pool.as_ref(), &nullifier]), false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(*token, false),
                AccountMeta::new(self.relayer.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let r = self.relayer.insecure_clone();
        self.send(&[ix], &r, &[])
    }

    fn assert_vault_backs_live_notes(&self) {
        assert_eq!(self.balance(&self.vault), self.live_notes() as u64 * NOTE);
    }
}

#[test]
fn the_contexts_are_derived_on_chain_from_the_pool_address() {
    let env = Env::new(1);
    let d = env.pool_data();
    assert_eq!(&d[118..150], &enc(&note_action(&env.pool)));
    assert_eq!(u64::from_le_bytes(d[150..158].try_into().unwrap()), spend_round_u64(&env.pool));
}

#[test]
fn only_an_approved_key_can_deposit() {
    let mut env = Env::new(1);
    let (m, ta) = env.member(1);
    let c = Note::new(1).commitment(&env.pool);
    let err = env.deposit(&m, &ta, c).unwrap_err();
    assert_eq!(custom_code(&err), Some(ACCOUNT_NOT_INITIALIZED));
    env.allow(&m.pubkey());
    env.deposit(&m, &ta, c).unwrap();
    env.assert_vault_backs_live_notes();
}

#[test]
fn the_tree_is_folded_on_chain_and_matches_the_client() {
    let mut env = Env::new(1);
    // Every level of every fold reads `EMPTY` (levels above the leaves are all
    // empty siblings), so these roots matching the client's pins the whole table.
    for i in 0..3u8 {
        let (m, ta) = env.member(1);
        env.allow(&m.pubkey());
        let c = Note::new(50 + i as u64).commitment(&env.pool);
        env.deposit(&m, &ta, c).unwrap();
        let cus = env.insert(c);
        println!("insert leaf {i}: {cus:?} CU");
        assert!(cus.iter().all(|&cu| cu < 1_400_000));
        let (root, size) = env.tree_root(i as usize);
        assert_eq!(size, i as u32 + 1);
        assert_eq!(root, env.current_root(), "on-chain root after leaf {i}");
    }
}

#[test]
fn leaves_enter_the_tree_in_deposit_order() {
    let mut env = Env::new(1);
    let mut cs = vec![];
    for i in 0..2u8 {
        let (m, ta) = env.member(1);
        env.allow(&m.pubkey());
        let c = Note::new(70 + i as u64).commitment(&env.pool);
        env.deposit(&m, &ta, c).unwrap();
        cs.push(c);
    }
    let err = env.insert_once(cs[1], LEVELS).unwrap_err();
    assert_eq!(custom_code(&err), Some(OUT_OF_ORDER));
    // Halfway through leaf 0, leaf 1 still cannot jump in.
    env.insert_once(cs[0], LEVELS).unwrap();
    let err = env.insert_once(cs[1], LEVELS).unwrap_err();
    assert_eq!(custom_code(&err), Some(OUT_OF_ORDER));
}

#[test]
fn a_verified_proof_pays_the_bound_recipient_minus_the_fee_exactly_once() {
    let mut env = Env::new(3);
    let deps = env.with_deposits(3);
    let clean = Keypair::new().pubkey();
    let clean_ta = env.token_account(&clean);
    let cu = env.withdraw_note(&deps[1].2, &clean_ta).unwrap();
    println!("withdraw: {cu} CU");
    assert_eq!(env.balance(&clean_ta), NOTE - FEE);
    assert_eq!(env.balance(&env.treasury), FEE);
    env.assert_vault_backs_live_notes();

    let err = env.withdraw_note(&deps[1].2, &clean_ta).unwrap_err();
    assert_already_spent(&err, 0);
    assert_eq!(env.balance(&clean_ta), NOTE - FEE);
}

#[test]
fn only_a_finalized_bound_crowd_buffer_of_the_verifier_counts() {
    let mut env = Env::new(3);
    let deps = env.with_deposits(3);
    let clean_ta = env.token_account(&Keypair::new().pubkey());
    let nf = deps[0].2.nullifier(&env.pool);
    let good = Buffer::settleable(&env.pool, env.current_root(), nf, &clean_ta);

    // Same bytes, owned by anyone else: anyone can write those.
    let forged = env.put_buffer(&good, Pubkey::new_unique());
    assert_eq!(custom_code(&env.withdraw(nf, &forged, &clean_ta).unwrap_err()), Some(PROOF_NOT_VERIFIED));

    for (at, value) in [(B_FINALIZED, 0u8), (B_FINALIZED, 2), (B_KIND, 0x03), (B_KIND, 0x81), (B_VERSION, 0)] {
        let mut b = Buffer { data: good.data.clone() };
        b.data[at] = value;
        let key = env.put_buffer(&b, VERIFIER_ID);
        let err = env.withdraw(nf, &key, &clean_ta).unwrap_err();
        assert_eq!(custom_code(&err), Some(PROOF_NOT_VERIFIED), "byte {at} = {value}");
    }
    let short = env.put_buffer(&Buffer { data: good.data[..B_PROOFS - 1].to_vec() }, VERIFIER_ID);
    assert_eq!(custom_code(&env.withdraw(nf, &short, &clean_ta).unwrap_err()), Some(PROOF_NOT_VERIFIED));

    let key = env.put_buffer(&good, VERIFIER_ID);
    env.withdraw(nf, &key, &clean_ta).unwrap();
}

#[test]
fn every_public_input_must_be_this_pools() {
    let mut env = Env::new(3);
    let deps = env.with_deposits(3);
    let clean_ta = env.token_account(&Keypair::new().pubkey());
    let other_ta = env.token_account(&Keypair::new().pubkey());
    let nf = deps[2].2.nullifier(&env.pool);
    let good = Buffer::settleable(&env.pool, env.current_root(), nf, &clean_ta);
    let other_pool = Pubkey::new_unique();

    // The relayer names another recipient than the one the proofs are bound to.
    let key = env.put_buffer(&good, VERIFIER_ID);
    assert_eq!(custom_code(&env.withdraw(nf, &key, &other_ta).unwrap_err()), Some(PROOF_MISMATCH));

    let cases: Vec<(usize, Vec<u8>)> = vec![
        (B_ACTION, enc(&note_action(&other_pool)).to_vec()),
        (B_ROUND, (spend_round_u64(&other_pool) as u128).to_le_bytes().to_vec()),
        (B_NULLIFIER, deps[1].2.nullifier(&env.pool).to_vec()),
    ];
    for (at, bytes) in cases {
        let mut b = Buffer { data: good.data.clone() };
        b.data[at..at + bytes.len()].copy_from_slice(&bytes);
        let key = env.put_buffer(&b, VERIFIER_ID);
        assert_eq!(custom_code(&env.withdraw(nf, &key, &clean_ta).unwrap_err()), Some(PROOF_MISMATCH), "field at {at}");
    }

    // A root nobody computed here (an operator's, say) is not a root.
    let mut b = Buffer { data: good.data.clone() };
    b.data[B_ROOT..B_ROOT + 32].copy_from_slice(&[9u8; 32]);
    let key = env.put_buffer(&b, VERIFIER_ID);
    assert_eq!(custom_code(&env.withdraw(nf, &key, &clean_ta).unwrap_err()), Some(UNKNOWN_ROOT));

    let mut bad = nf;
    bad[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(custom_code(&env.withdraw(bad, &key, &clean_ta).unwrap_err()), Some(NON_CANONICAL));

    let key = env.put_buffer(&good, VERIFIER_ID);
    env.withdraw(nf, &key, &clean_ta).unwrap();
}

#[test]
fn below_the_floor_no_withdrawal_settles_and_exits_leave_the_set() {
    let mut env = Env::new(3);
    let deps = env.with_deposits(2);
    let clean_ta = env.token_account(&Keypair::new().pubkey());
    let err = env.withdraw_note(&deps[0].2, &clean_ta).unwrap_err();
    assert_eq!(custom_code(&err), Some(ANONYMITY_SET_TOO_SMALL));

    let more = env.with_deposits(1);
    // Three in the set; one exits publicly, two remain: still below 3.
    let (_, ta, note) = &more[0];
    env.exit(note.secret_bytes(), note.nullifier(&env.pool), note.commitment(&env.pool), ta).unwrap();
    let err = env.withdraw_note(&deps[0].2, &clean_ta).unwrap_err();
    assert_eq!(custom_code(&err), Some(ANONYMITY_SET_TOO_SMALL));

    env.with_deposits(1);
    env.withdraw_note(&deps[0].2, &clean_ta).unwrap();
}

#[test]
fn an_exit_refunds_the_depositor_in_full_and_burns_the_same_nullifier() {
    let mut env = Env::new(1);
    let deps = env.with_deposits(3);
    let pool = env.pool;
    let (_, ta0, n0) = &deps[0];
    let (_, ta1, n1) = &deps[1];

    // A secret that does not open this deposit, or a nullifier that is not its.
    let err = env.exit(n1.secret_bytes(), n0.nullifier(&pool), n0.commitment(&pool), ta0).unwrap_err();
    assert_eq!(custom_code(&err), Some(SECRET_MISMATCH));
    let err = env.exit(n0.secret_bytes(), n1.nullifier(&pool), n0.commitment(&pool), ta0).unwrap_err();
    assert_eq!(custom_code(&err), Some(SECRET_MISMATCH));
    // The refund cannot be pointed at anyone else's account.
    let err = env.exit(n0.secret_bytes(), n0.nullifier(&pool), n0.commitment(&pool), ta1).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_DEPOSITOR_ACCOUNT));

    let cu = env.exit(n0.secret_bytes(), n0.nullifier(&pool), n0.commitment(&pool), ta0).unwrap();
    println!("exit: {cu} CU");
    assert_eq!(env.balance(ta0), NOTE);
    env.assert_vault_backs_live_notes();

    // Exited, so it cannot be withdrawn privately afterwards.
    let clean_ta = env.token_account(&Keypair::new().pubkey());
    assert_already_spent(&env.withdraw_note(n0, &clean_ta).unwrap_err(), 0);

    // Withdrawn privately, so it cannot be exited afterwards.
    env.withdraw_note(n1, &clean_ta).unwrap();
    let err = env.exit(n1.secret_bytes(), n1.nullifier(&pool), n1.commitment(&pool), ta1).unwrap_err();
    assert_already_spent(&err, 0);
    assert_eq!(env.balance(ta1), 0);
    env.assert_vault_backs_live_notes();
}
