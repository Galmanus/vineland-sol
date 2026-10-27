//! The pool's invariants, each pinned against the compiled `.so` in LiteSVM.
//!
//! Run: `cargo-build-sbf && cargo test` from `programs/vineland-pool`.
//!
//! Every rejection test asserts the exact error, so a test cannot pass because
//! the transaction failed for some unrelated reason.

use litesvm::LiteSVM;
use sha2::{Digest, Sha256};
use solana_sdk::{
    instruction::{AccountMeta, Instruction, InstructionError},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::{Transaction, TransactionError},
};

const PROGRAM_ID: Pubkey = solana_sdk::pubkey!("AsQwEN7XiUPW2MGHEbEkh8UCKixwpSBfE1KKt5QVgYgx");
const NOTE: u64 = 1_000_000; // 1.0 of a 6-decimal token
const FEE_BPS: u16 = 197; // 1.97%
const FEE: u64 = 19_700;
const DECIMALS: u8 = 6;

// Anchor custom errors, in declaration order from 6000.
const INVALID_CONFIG: u32 = 6000;
const INVALID_COMMITTEE: u32 = 6001;
const NOT_ENOUGH_ATTESTATIONS: u32 = 6002;
const ROOT_NOT_PUBLISHED: u32 = 6003;
const ROOT_MISMATCH: u32 = 6004;
const INVALID_SET_SIZE: u32 = 6005;
const ANONYMITY_SET_TOO_SMALL: u32 = 6006;
const INVALID_RECIPIENT: u32 = 6008;
const NOT_DEPOSITOR: u32 = 6009;
const NON_CANONICAL: u32 = 6013;
// Anchor framework errors.
const ACCOUNT_NOT_INITIALIZED: u32 = 3012;
const CONSTRAINT_HAS_ONE: u32 = 2001;
const CONSTRAINT_SEEDS: u32 = 2006;
const CONSTRAINT_TOKEN_MINT: u32 = 2014;
const DUPLICATE_MUTABLE: u32 = 2040;

fn disc(name: &str) -> [u8; 8] {
    let h = Sha256::digest(format!("global:{name}").as_bytes());
    h[..8].try_into().unwrap()
}

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &PROGRAM_ID).0
}

/// The instruction index and custom code a failed transaction returned.
fn custom_code(err: &TransactionError) -> Option<u32> {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
        _ => None,
    }
}

/// The exact Ed25519 precompile layout the program accepts: one signature, key,
/// signature and message all inside this instruction's own data.
fn ed25519_ix(signer: &Keypair, msg: &[u8]) -> Instruction {
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

fn withdraw_msg(pool: &Pubkey, root: &[u8; 32], nullifier: &[u8; 32], recipient: &Pubkey) -> Vec<u8> {
    [b"vineland-wdrw-v1".as_ref(), pool.as_ref(), root, nullifier, recipient.as_ref()].concat()
}

fn exit_msg(pool: &Pubkey, commitment: &[u8; 32], nullifier: &[u8; 32]) -> Vec<u8> {
    [b"vineland-exit-v1".as_ref(), pool.as_ref(), commitment, nullifier].concat()
}

struct Env {
    svm: LiteSVM,
    authority: Keypair,
    mint: Pubkey,
    pool: Pubkey,
    vault: Pubkey,
    treasury: Pubkey,
    verifiers: Vec<Keypair>,
    relayer: Keypair,
    root: [u8; 32],
}

impl Env {
    /// A pool with `n` verifiers at threshold `m` and floor `k_min`.
    fn new(n: usize, m: u8, k_min: u32) -> Self {
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
            spl_token::instruction::initialize_mint2(
                &spl_token::id(),
                &mint.pubkey(),
                &authority.pubkey(),
                None,
                DECIMALS,
            )
            .unwrap(),
        ];
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&authority.pubkey()),
            &[&authority, &mint],
            svm.latest_blockhash(),
        );
        svm.send_transaction(tx).unwrap();
        let mint = mint.pubkey();

        let mut env = Env {
            svm,
            authority,
            mint,
            pool: Pubkey::default(),
            vault: Pubkey::default(),
            treasury: Pubkey::default(),
            verifiers: (0..n).map(|_| Keypair::new()).collect(),
            relayer,
            root: [7u8; 32],
        };
        let owner = env.authority.pubkey();
        env.treasury = env.token_account(&owner);
        env.pool = pda(&[b"pool", owner.as_ref(), mint.as_ref()]);
        env.vault = pda(&[b"vault", env.pool.as_ref()]);

        let keys: Vec<Pubkey> = env.verifiers.iter().map(|k| k.pubkey()).collect();
        env.initialize(&keys, m, NOTE, FEE_BPS, k_min).expect("initialize");
        env
    }

    fn initialize(&mut self, keys: &[Pubkey], m: u8, note: u64, fee: u16, k_min: u32) -> Result<(), TransactionError> {
        let mut data = disc("initialize").to_vec();
        data.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for k in keys {
            data.extend_from_slice(k.as_ref());
        }
        data.push(m);
        data.extend_from_slice(&note.to_le_bytes());
        data.extend_from_slice(&fee.to_le_bytes());
        data.extend_from_slice(&k_min.to_le_bytes());
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(self.treasury, false),
                AccountMeta::new(self.authority.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[])
    }

    fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<(), TransactionError> {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&payer.pubkey()),
            &signers,
            self.svm.latest_blockhash(),
        );
        let res = self.svm.send_transaction(tx).map(|_| ()).map_err(|f| f.err);
        self.svm.expire_blockhash();
        res
    }

    /// A fresh token account of the pool's mint owned by `owner`.
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
            spl_token::instruction::initialize_account3(&spl_token::id(), &acct.pubkey(), &self.mint, owner)
                .unwrap(),
        ];
        let a = self.authority.insecure_clone();
        self.send(&ixs, &a, &[&acct]).unwrap();
        acct.pubkey()
    }

    fn balance(&self, account: &Pubkey) -> u64 {
        let data = self.svm.get_account(account).unwrap().data;
        spl_token::state::Account::unpack(&data).unwrap().amount
    }

    fn pool_u32_at(&self, offset: usize) -> u32 {
        let data = self.svm.get_account(&self.pool).unwrap().data;
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    /// `Pool` layout: 8 disc, 3 keys, note, fee_bps, k_min, verifiers (len + n),
    /// threshold, root, root_set_size, leaf_count, live_notes. Anchor serializes
    /// the Vec at its real length, so offsets after it depend on `n`.
    fn live_notes(&self) -> u32 {
        let n = self.verifiers.len();
        let after_vec = 8 + 32 * 3 + 8 + 2 + 4 + 4 + 32 * n;
        self.pool_u32_at(after_vec + 1 + 32 + 4 + 4)
    }

    /// A member: a funded key with a token account holding `notes` notes.
    fn member(&mut self, notes: u64) -> (Keypair, Pubkey) {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 1_000_000_000).unwrap();
        let ta = self.token_account(&kp.pubkey());
        let ix = spl_token::instruction::mint_to(
            &spl_token::id(),
            &self.mint,
            &ta,
            &self.authority.pubkey(),
            &[],
            notes * NOTE,
        )
        .unwrap();
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[]).unwrap();
        (kp, ta)
    }

    fn allow_ix(&self, signer: &Pubkey, member: &Pubkey) -> Instruction {
        let mut data = disc("allow").to_vec();
        data.extend_from_slice(member.as_ref());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(self.pool, false),
                AccountMeta::new(pda(&[b"allow", self.pool.as_ref(), member.as_ref()]), false),
                AccountMeta::new(*signer, true),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        }
    }

    fn allow(&mut self, member: &Pubkey) {
        let ix = self.allow_ix(&self.authority.pubkey(), member);
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[]).unwrap();
    }

    fn revoke(&mut self, member: &Pubkey) {
        let mut data = disc("revoke").to_vec();
        data.extend_from_slice(member.as_ref());
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(self.pool, false),
                AccountMeta::new(pda(&[b"allow", self.pool.as_ref(), member.as_ref()]), false),
                AccountMeta::new(self.authority.pubkey(), true),
            ],
            data,
        };
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[]).unwrap();
    }

    fn publish_root_ix(&self, signer: &Pubkey, root: [u8; 32], set_size: u32) -> Instruction {
        let mut data = disc("publish_root").to_vec();
        data.extend_from_slice(&root);
        data.extend_from_slice(&set_size.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![AccountMeta::new(self.pool, false), AccountMeta::new_readonly(*signer, true)],
            data,
        }
    }

    fn publish_root(&mut self, set_size: u32) -> Result<(), TransactionError> {
        let ix = self.publish_root_ix(&self.authority.pubkey(), self.root, set_size);
        let a = self.authority.insecure_clone();
        self.send(&[ix], &a, &[])
    }

    fn deposit(&mut self, member: &Keypair, token: &Pubkey, commitment: [u8; 32]) -> Result<(), TransactionError> {
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
        self.send(&[ix], member, &[])
    }

    fn withdraw_ix(&self, nullifier: [u8; 32], root: [u8; 32], recipient: &Pubkey) -> Instruction {
        let mut data = disc("withdraw").to_vec();
        data.extend_from_slice(&nullifier);
        data.extend_from_slice(&root);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new(pda(&[b"nullifier", self.pool.as_ref(), &nullifier]), false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(*recipient, false),
                AccountMeta::new(self.treasury, false),
                AccountMeta::new(self.relayer.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(solana_sdk::sysvar::instructions::ID, false),
            ],
            data,
        }
    }

    /// Submit a withdrawal as the relayer, preceded by `attestations`.
    fn withdraw_with(
        &mut self,
        attestations: Vec<Instruction>,
        nullifier: [u8; 32],
        root: [u8; 32],
        recipient: &Pubkey,
    ) -> Result<(), TransactionError> {
        let mut ixs = attestations;
        ixs.push(self.withdraw_ix(nullifier, root, recipient));
        let r = self.relayer.insecure_clone();
        self.send(&ixs, &r, &[])
    }

    /// A withdrawal attested by the first `threshold` verifiers, as intended.
    fn withdraw(&mut self, nullifier: [u8; 32], recipient: &Pubkey) -> Result<(), TransactionError> {
        let msg = withdraw_msg(&self.pool, &self.root, &nullifier, recipient);
        let atts = self.quorum(&msg);
        let root = self.root;
        self.withdraw_with(atts, nullifier, root, recipient)
    }

    fn quorum(&self, msg: &[u8]) -> Vec<Instruction> {
        let m = self.threshold();
        self.verifiers[..m].iter().map(|v| ed25519_ix(v, msg)).collect()
    }

    fn threshold(&self) -> usize {
        let n = self.verifiers.len();
        let data = self.svm.get_account(&self.pool).unwrap().data;
        data[8 + 32 * 3 + 8 + 2 + 4 + 4 + 32 * n] as usize
    }

    fn ragequit_with(
        &mut self,
        attestations: Vec<Instruction>,
        member: &Keypair,
        token: &Pubkey,
        commitment: [u8; 32],
        nullifier: [u8; 32],
    ) -> Result<(), TransactionError> {
        let mut data = disc("ragequit").to_vec();
        data.extend_from_slice(&commitment);
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
                AccountMeta::new(member.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(solana_sdk::sysvar::instructions::ID, false),
            ],
            data,
        };
        let mut ixs = attestations;
        ixs.push(ix);
        self.send(&ixs, member, &[])
    }

    fn ragequit(&mut self, member: &Keypair, token: &Pubkey, commitment: [u8; 32], nullifier: [u8; 32]) -> Result<(), TransactionError> {
        let atts = self.quorum(&exit_msg(&self.pool, &commitment, &nullifier));
        self.ragequit_with(atts, member, token, commitment, nullifier)
    }

    /// `k` approved members, each with one note deposited, and a root over them.
    fn with_deposits(&mut self, k: u8) -> Vec<(Keypair, Pubkey, [u8; 32])> {
        let mut out = vec![];
        for i in 0..k {
            let (m, ta) = self.member(1);
            self.allow(&m.pubkey());
            let c = [i + 1; 32];
            self.deposit(&m, &ta, c).unwrap();
            out.push((m, ta, c));
        }
        self.publish_root(k as u32).unwrap();
        out
    }

    /// The vault holds exactly one note per live note: nothing leaked, nothing stuck.
    fn assert_vault_backs_live_notes(&self) {
        assert_eq!(self.balance(&self.vault), self.live_notes() as u64 * NOTE);
    }
}

/// The nullifier PDA already exists: the system program refuses to create it
/// again (`AccountAlreadyInUse`, custom 0) inside the pool instruction at `ix`.
/// Exact, so a failure for any other reason does not count as "already spent".
fn assert_already_spent(err: &TransactionError, ix: u8) {
    assert_eq!(
        err,
        &TransactionError::InstructionError(ix, InstructionError::Custom(0)),
        "expected the nullifier PDA to already exist"
    );
}

/// A canonical test nullifier: every u32 limb stays below the M31 prime.
fn nf(byte: u8) -> [u8; 32] {
    assert!(byte < 0x40);
    [0x20 + byte; 32]
}

#[test]
fn only_an_approved_key_can_deposit() {
    let mut env = Env::new(1, 1, 1);
    let (m, ta) = env.member(3);

    let err = env.deposit(&m, &ta, [1; 32]).unwrap_err();
    assert_eq!(custom_code(&err), Some(ACCOUNT_NOT_INITIALIZED), "{err:?}");

    env.allow(&m.pubkey());
    env.deposit(&m, &ta, [1; 32]).unwrap();

    env.revoke(&m.pubkey());
    let err = env.deposit(&m, &ta, [2; 32]).unwrap_err();
    assert_eq!(custom_code(&err), Some(ACCOUNT_NOT_INITIALIZED), "{err:?}");
}

#[test]
fn a_deposit_escrows_exactly_one_note_and_a_commitment_enters_once() {
    let mut env = Env::new(1, 1, 1);
    let (m, ta) = env.member(3);
    env.allow(&m.pubkey());

    env.deposit(&m, &ta, [1; 32]).unwrap();
    assert_eq!(env.balance(&ta), 2 * NOTE);
    assert_eq!(env.balance(&env.vault), NOTE);
    env.assert_vault_backs_live_notes();

    // the record layout clients rebuild the tree from: pool at 8 (the memcmp
    // offset), depositor, commitment, then the leaf index
    let rec = env.svm.get_account(&pda(&[b"deposit", env.pool.as_ref(), &[1; 32]])).unwrap().data;
    assert_eq!(&rec[8..40], env.pool.as_ref());
    assert_eq!(&rec[40..72], m.pubkey().as_ref());
    assert_eq!(&rec[72..104], &[1u8; 32]);
    assert_eq!(u32::from_le_bytes(rec[104..108].try_into().unwrap()), 0);

    // the same commitment again: its record already exists
    // the deposit record PDA exists: the system program refuses to create it again
    let err = env.deposit(&m, &ta, [1; 32]).unwrap_err();
    assert_already_spent(&err, 0);
    assert_eq!(env.balance(&env.vault), NOTE);
}

#[test]
fn a_withdrawal_pays_a_clean_account_minus_the_fee_and_the_depositor_never_signs() {
    let mut env = Env::new(1, 1, 2);
    let members = env.with_deposits(2);

    // a brand-new key the depositor never touched; it holds no SOL at all
    let clean = Keypair::new();
    let recipient = env.token_account(&clean.pubkey());

    env.withdraw(nf(1), &recipient).unwrap();
    assert_eq!(env.balance(&recipient), NOTE - FEE);
    assert_eq!(env.balance(&env.treasury), FEE);
    assert_eq!(env.live_notes(), 1);
    env.assert_vault_backs_live_notes();
    // the depositors' token accounts are untouched by the withdrawal
    for (_, ta, _) in &members {
        assert_eq!(env.balance(ta), 0);
    }
}

#[test]
fn a_nullifier_pays_once() {
    let mut env = Env::new(1, 1, 1);
    env.with_deposits(2);
    let r = env.token_account(&Pubkey::new_unique());

    env.withdraw(nf(1), &r).unwrap();
    // a fresh, valid attestation does not help: the nullifier PDA exists
    let err = env.withdraw(nf(1), &r).unwrap_err();
    assert_already_spent(&err, 1);
    assert_eq!(env.balance(&r), NOTE - FEE);
    env.assert_vault_backs_live_notes();
}

#[test]
fn a_withdrawal_without_a_quorum_is_refused() {
    let mut env = Env::new(3, 2, 1);
    env.with_deposits(2);
    let r = env.token_account(&Pubkey::new_unique());
    let msg = withdraw_msg(&env.pool, &env.root, &nf(1), &r);
    let root = env.root;

    // no attestation at all
    let err = env.withdraw_with(vec![], nf(1), root, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");

    // one of the required two
    let one = vec![ed25519_ix(&env.verifiers[0], &msg)];
    let err = env.withdraw_with(one, nf(1), root, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");

    // the same verifier twice still counts once
    let twice = vec![ed25519_ix(&env.verifiers[0], &msg), ed25519_ix(&env.verifiers[0], &msg)];
    let err = env.withdraw_with(twice, nf(1), root, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");

    // a verifier plus an outsider
    let impostor = Keypair::new();
    let mixed = vec![ed25519_ix(&env.verifiers[0], &msg), ed25519_ix(&impostor, &msg)];
    let err = env.withdraw_with(mixed, nf(1), root, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");

    assert_eq!(env.balance(&r), 0);
    // two distinct verifiers: settles
    env.withdraw(nf(1), &r).unwrap();
    assert_eq!(env.balance(&r), NOTE - FEE);
}

#[test]
fn the_relayer_cannot_redirect_the_payout() {
    let mut env = Env::new(1, 1, 1);
    env.with_deposits(2);
    let intended = env.token_account(&Pubkey::new_unique());
    let relayers_own = env.token_account(&env.relayer.pubkey());

    let msg = withdraw_msg(&env.pool, &env.root, &nf(1), &intended);
    let atts = env.quorum(&msg);
    let root = env.root;
    let err = env.withdraw_with(atts, nf(1), root, &relayers_own).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");
    assert_eq!(env.balance(&relayers_own), 0);
}

#[test]
fn the_vault_cannot_be_named_as_recipient_and_the_mint_must_match() {
    let mut env = Env::new(1, 1, 1);
    env.with_deposits(2);
    // Anchor already refuses the vault passed twice as mutable (2040, duplicate
    // mutable account); the program's own constraint is the second layer.
    let vault = env.vault;
    let err = env.withdraw(nf(1), &vault).unwrap_err();
    assert!(
        matches!(custom_code(&err), Some(DUPLICATE_MUTABLE) | Some(INVALID_RECIPIENT)),
        "{err:?}"
    );

    // a token account of a different mint
    let other_mint = Keypair::new();
    let rent = env.svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
    let a = env.authority.insecure_clone();
    env.send(
        &[
            system_instruction::create_account(&a.pubkey(), &other_mint.pubkey(), rent, 82, &spl_token::id()),
            spl_token::instruction::initialize_mint2(&spl_token::id(), &other_mint.pubkey(), &a.pubkey(), None, 6)
                .unwrap(),
        ],
        &a,
        &[&other_mint],
    )
    .unwrap();
    let foreign = Keypair::new();
    let rent = env.svm.minimum_balance_for_rent_exemption(165);
    env.send(
        &[
            system_instruction::create_account(&a.pubkey(), &foreign.pubkey(), rent, 165, &spl_token::id()),
            spl_token::instruction::initialize_account3(
                &spl_token::id(),
                &foreign.pubkey(),
                &other_mint.pubkey(),
                &a.pubkey(),
            )
            .unwrap(),
        ],
        &a,
        &[&foreign],
    )
    .unwrap();
    let err = env.withdraw(nf(1), &foreign.pubkey()).unwrap_err();
    assert_eq!(custom_code(&err), Some(CONSTRAINT_TOKEN_MINT), "{err:?}");
}

#[test]
fn a_stale_or_missing_root_is_refused() {
    let mut env = Env::new(1, 1, 1);
    let (m, ta) = env.member(1);
    env.allow(&m.pubkey());
    env.deposit(&m, &ta, [1; 32]).unwrap();
    let r = env.token_account(&Pubkey::new_unique());

    // no root yet
    let err = env.withdraw(nf(1), &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(ROOT_NOT_PUBLISHED), "{err:?}");

    env.publish_root(1).unwrap();
    // attested and submitted against a root that is not the current one
    let stale = [9u8; 32];
    let msg = withdraw_msg(&env.pool, &stale, &nf(1), &r);
    let atts = env.quorum(&msg);
    let err = env.withdraw_with(atts, nf(1), stale, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(ROOT_MISMATCH), "{err:?}");
}

#[test]
fn below_the_floor_no_withdrawal_settles() {
    let mut env = Env::new(1, 1, 3);
    let members = env.with_deposits(2);
    let r = env.token_account(&Pubkey::new_unique());
    let err = env.withdraw(nf(1), &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(ANONYMITY_SET_TOO_SMALL), "{err:?}");

    // the root cannot claim more deposits than exist
    let err = env.publish_root(3).unwrap_err();
    assert_eq!(custom_code(&err), Some(INVALID_SET_SIZE), "{err:?}");

    // a third deposit, a root over three: now it settles
    let (m, ta) = env.member(1);
    env.allow(&m.pubkey());
    env.deposit(&m, &ta, [3; 32]).unwrap();
    env.publish_root(3).unwrap();
    env.withdraw(nf(1), &r).unwrap();
    assert_eq!(members.len(), 2);
}

/// The double payout the Stellar pool allows (vineland-stellar/vineland-pool:
/// `withdraw` never touches the deposit record, `ragequit` only checks it).
#[test]
fn a_withdrawn_note_cannot_also_exit() {
    let mut env = Env::new(1, 1, 1);
    let members = env.with_deposits(2);
    let (m, ta, c) = (&members[0].0.insecure_clone(), members[0].1, members[0].2);
    let r = env.token_account(&Pubkey::new_unique());

    env.withdraw(nf(1), &r).unwrap();
    let vault_before = env.balance(&env.vault);
    let err = env.ragequit(m, &ta, c, nf(1)).unwrap_err();
    assert_already_spent(&err, 1);
    assert_eq!(env.balance(&env.vault), vault_before);
    assert_eq!(env.balance(&ta), 0);
    env.assert_vault_backs_live_notes();
}

#[test]
fn an_exited_note_cannot_also_be_withdrawn() {
    let mut env = Env::new(1, 1, 1);
    let members = env.with_deposits(2);
    let (m, ta, c) = (&members[0].0.insecure_clone(), members[0].1, members[0].2);

    env.ragequit(m, &ta, c, nf(1)).unwrap();
    assert_eq!(env.balance(&ta), NOTE, "an exit refunds the whole note, no fee");
    env.assert_vault_backs_live_notes();

    let r = env.token_account(&Pubkey::new_unique());
    let err = env.withdraw(nf(1), &r).unwrap_err();
    assert_already_spent(&err, 1);
    assert_eq!(env.balance(&r), 0);
    env.assert_vault_backs_live_notes();
}

#[test]
fn only_the_depositor_exits_and_only_once() {
    let mut env = Env::new(1, 1, 1);
    let members = env.with_deposits(2);
    let (owner, owner_ta, c) = (members[0].0.insecure_clone(), members[0].1, members[0].2);
    let (thief, thief_ta) = (members[1].0.insecure_clone(), members[1].1);

    let err = env.ragequit(&thief, &thief_ta, c, nf(1)).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_DEPOSITOR), "{err:?}");

    env.ragequit(&owner, &owner_ta, c, nf(1)).unwrap();
    // a second exit with a different nullifier: the record says exited
    let err = env.ragequit(&owner, &owner_ta, c, nf(2)).unwrap_err();
    assert_eq!(custom_code(&err), Some(6010), "{err:?}");
    assert_eq!(env.balance(&owner_ta), NOTE);
}

#[test]
fn an_exit_needs_the_committee_and_an_exit_attestation_cannot_settle_a_withdrawal() {
    let mut env = Env::new(1, 1, 1);
    let members = env.with_deposits(2);
    let (m, ta, c) = (members[0].0.insecure_clone(), members[0].1, members[0].2);

    let err = env.ragequit_with(vec![], &m, &ta, c, nf(1)).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");

    // a withdrawal attestation does not authorize an exit
    let r = env.token_account(&Pubkey::new_unique());
    let wmsg = withdraw_msg(&env.pool, &env.root, &nf(1), &r);
    let atts = env.quorum(&wmsg);
    let err = env.ragequit_with(atts, &m, &ta, c, nf(1)).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");

    // an exit attestation does not authorize a withdrawal
    let emsg = exit_msg(&env.pool, &c, &nf(1));
    let atts = env.quorum(&emsg);
    let root = env.root;
    let err = env.withdraw_with(atts, nf(1), root, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(NOT_ENOUGH_ATTESTATIONS), "{err:?}");
}

#[test]
fn only_the_authority_administers() {
    let mut env = Env::new(1, 1, 1);
    let outsider = Keypair::new();
    env.svm.airdrop(&outsider.pubkey(), 1_000_000_000).unwrap();

    // the allow PDA's pool must be the outsider's pool, which does not exist
    let ix = env.allow_ix(&outsider.pubkey(), &outsider.pubkey());
    let err = env.send(&[ix], &outsider, &[]).unwrap_err();
    assert!(
        matches!(custom_code(&err), Some(CONSTRAINT_SEEDS) | Some(CONSTRAINT_HAS_ONE)),
        "{err:?}"
    );

    let ix = env.publish_root_ix(&outsider.pubkey(), [1; 32], 0);
    let err = env.send(&[ix], &outsider, &[]).unwrap_err();
    assert!(
        matches!(custom_code(&err), Some(CONSTRAINT_SEEDS) | Some(CONSTRAINT_HAS_ONE)),
        "{err:?}"
    );
}

#[test]
fn a_pool_cannot_be_opened_with_a_bad_committee_or_a_confiscatory_fee() {
    // each case on a fresh mint, since the pool PDA is per (authority, mint)
    let mut env = Env::new(1, 1, 1);
    let keys: Vec<Pubkey> = vec![Pubkey::new_unique(), Pubkey::new_unique()];

    let fresh = |env: &mut Env| {
        let mint = Keypair::new();
        let a = env.authority.insecure_clone();
        let rent = env.svm.minimum_balance_for_rent_exemption(82);
        env.send(
            &[
                system_instruction::create_account(&a.pubkey(), &mint.pubkey(), rent, 82, &spl_token::id()),
                spl_token::instruction::initialize_mint2(&spl_token::id(), &mint.pubkey(), &a.pubkey(), None, 6)
                    .unwrap(),
            ],
            &a,
            &[&mint],
        )
        .unwrap();
        env.mint = mint.pubkey();
        let owner = a.pubkey();
        env.treasury = env.token_account(&owner);
        env.pool = pda(&[b"pool", owner.as_ref(), env.mint.as_ref()]);
        env.vault = pda(&[b"vault", env.pool.as_ref()]);
    };

    fresh(&mut env);
    let err = env.initialize(&keys, 3, NOTE, FEE_BPS, 1).unwrap_err();
    assert_eq!(custom_code(&err), Some(INVALID_COMMITTEE), "{err:?}");
    let dup = vec![keys[0], keys[0]];
    let err = env.initialize(&dup, 1, NOTE, FEE_BPS, 1).unwrap_err();
    assert_eq!(custom_code(&err), Some(INVALID_COMMITTEE), "{err:?}");
    let err = env.initialize(&keys, 1, NOTE, 1_001, 1).unwrap_err();
    assert_eq!(custom_code(&err), Some(INVALID_CONFIG), "{err:?}");
    let err = env.initialize(&keys, 1, 0, FEE_BPS, 1).unwrap_err();
    assert_eq!(custom_code(&err), Some(INVALID_CONFIG), "{err:?}");
    env.initialize(&keys, 2, NOTE, FEE_BPS, 1).unwrap();
}

/// A mixed run: deposits, withdrawals and exits interleaved; after every step the
/// vault holds exactly `live_notes * NOTE` and the totals reconcile.
#[test]
fn the_vault_always_backs_exactly_the_live_notes() {
    let mut env = Env::new(3, 2, 2);
    let members = env.with_deposits(5);
    env.assert_vault_backs_live_notes();

    let mut paid = 0u64;
    for i in 0..3u8 {
        let r = env.token_account(&Pubkey::new_unique());
        env.withdraw(nf(i), &r).unwrap();
        paid += env.balance(&r);
        env.assert_vault_backs_live_notes();
    }
    let (m, ta, c) = (members[4].0.insecure_clone(), members[4].1, members[4].2);
    env.ragequit(&m, &ta, c, nf(9)).unwrap();
    env.assert_vault_backs_live_notes();

    assert_eq!(env.live_notes(), 1);
    assert_eq!(paid, 3 * (NOTE - FEE));
    assert_eq!(env.balance(&env.treasury), 3 * FEE);
    // conservation: 5 notes in = 3 payouts + 3 fees + 1 refund + 1 still in the vault
    assert_eq!(paid + env.balance(&env.treasury) + env.balance(&ta) + env.balance(&env.vault), 5 * NOTE);
}

#[test]
fn one_field_element_has_one_encoding() {
    let mut env = Env::new(1, 1, 1);
    let members = env.with_deposits(2);
    let r = env.token_account(&Pubkey::new_unique());

    // limb 0 of the alias is v + P: the same Mersenne-31 value as v, other bytes.
    // Even a careless committee that attests the alias cannot get it paid.
    let mut alias = nf(1);
    let v = u32::from_le_bytes(alias[..4].try_into().unwrap());
    alias[..4].copy_from_slice(&(v + 0x7FFF_FFFF).to_le_bytes());
    env.withdraw(nf(1), &r).unwrap();
    let err = env.withdraw(alias, &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(NON_CANONICAL), "{err:?}");
    assert_eq!(env.balance(&r), NOTE - FEE);

    // exits, deposits and roots are held to the same rule
    let (m0, ta0, c0) = (members[0].0.insecure_clone(), members[0].1, members[0].2);
    let err = env.ragequit(&m0, &ta0, c0, alias).unwrap_err();
    assert_eq!(custom_code(&err), Some(NON_CANONICAL), "{err:?}");
    let (m, ta) = env.member(1);
    env.allow(&m.pubkey());
    let err = env.deposit(&m, &ta, [0xFF; 32]).unwrap_err();
    assert_eq!(custom_code(&err), Some(NON_CANONICAL), "{err:?}");
    let ix = env.publish_root_ix(&env.authority.pubkey(), [0xFF; 32], 2);
    let a = env.authority.insecure_clone();
    let err = env.send(&[ix], &a, &[]).unwrap_err();
    assert_eq!(custom_code(&err), Some(NON_CANONICAL), "{err:?}");
    env.assert_vault_backs_live_notes();
}

#[test]
fn exited_notes_do_not_count_toward_the_floor() {
    let mut env = Env::new(1, 1, 2);
    let members = env.with_deposits(3);
    for (i, (m, ta, c)) in members.iter().take(2).enumerate() {
        let m = m.insecure_clone();
        env.ragequit(&m, ta, *c, nf(10 + i as u8)).unwrap();
    }
    // the root still covers three leaves, but two of them left in public
    let r = env.token_account(&Pubkey::new_unique());
    let err = env.withdraw(nf(1), &r).unwrap_err();
    assert_eq!(custom_code(&err), Some(ANONYMITY_SET_TOO_SMALL), "{err:?}");
    env.assert_vault_backs_live_notes();
}
