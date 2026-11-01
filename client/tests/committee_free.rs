//! Committee-free, end to end, with nothing mocked: real notes deposited into
//! the v2 pool, the tree folded on chain, a recipient-bound crowd proof verified
//! by riverrun's staged verifier program, and the pool paying the clean account
//! from that verified buffer. No committee key exists anywhere in this test.
//!
//! Needs both SBF artifacts: `cargo build-sbf` in programs/vineland-pool, and in
//! mirror-pool's programs/riverrun-m31-verifier (ce5e8c7), or their paths in
//! `VINELAND_POOL_V2_SO` / `RIVERRUN_VERIFIER_SO`.
//!   cargo test --release --test committee_free -- --ignored --nocapture

use litesvm::LiteSVM;
use riverrun_m31_verifier::client as staged_client;
use sha2::{Digest, Sha256};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction, InstructionError},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::{Transaction, TransactionError},
};
use vineland_client::*;

const POOL_ID: Pubkey = solana_sdk::pubkey!("3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi");
const VERIFIER_ID: Pubkey = solana_sdk::pubkey!("8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo");
const NOTE_AMOUNT: u64 = 1_000_000;
const FEE_BPS: u16 = 197;
const FEE: u64 = 19_700;
const K_MIN: u32 = 3;
const T_ROOTS: usize = 8 + 127 * 32;

fn disc(name: &str) -> [u8; 8] {
    Sha256::digest(format!("global:{name}").as_bytes())[..8].try_into().unwrap()
}

fn v2(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &POOL_ID).0
}

struct Chain {
    svm: LiteSVM,
}

impl Chain {
    fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<u64, TransactionError> {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, self.svm.latest_blockhash());
        self.svm.send_transaction(tx).map(|m| m.compute_units_consumed).map_err(|f| f.err)
    }

    fn token_account(&mut self, payer: &Keypair, mint: &Pubkey, owner: &Pubkey) -> Pubkey {
        let acct = Keypair::new();
        let rent = self.svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
        let ixs = [
            system_instruction::create_account(&payer.pubkey(), &acct.pubkey(), rent, spl_token::state::Account::LEN as u64, &spl_token::id()),
            spl_token::instruction::initialize_account3(&spl_token::id(), &acct.pubkey(), mint, owner).unwrap(),
        ];
        self.send(&ixs, payer, &[&acct]).unwrap();
        acct.pubkey()
    }

    fn balance(&self, k: &Pubkey) -> u64 {
        spl_token::state::Account::unpack(&self.svm.get_account(k).unwrap().data).unwrap().amount
    }
}

#[test]
#[ignore = "proves at production parameters and needs both .so files: run with --release"]
fn a_private_payout_settles_with_no_committee_at_all() {
    let mut svm = LiteSVM::new();
    let pool_so = std::env::var("VINELAND_POOL_V2_SO")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../programs/vineland-pool/target/deploy/vineland_pool.so").into());
    let verifier_so = std::env::var("RIVERRUN_VERIFIER_SO").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../mirror-pool/.claude/worktrees/agent-ae0080ba263a865c1/programs/riverrun-m31-verifier/target/deploy/riverrun_m31_verifier.so"
        )
        .into()
    });
    svm.add_program_from_file(POOL_ID, &pool_so).expect("pool v2 .so");
    svm.add_program_from_file(VERIFIER_ID, &verifier_so).expect("verifier .so");
    let mut chain = Chain { svm };

    let operator = Keypair::new();
    let relayer = Keypair::new();
    chain.svm.airdrop(&operator.pubkey(), 10_000_000_000).unwrap();
    chain.svm.airdrop(&relayer.pubkey(), 10_000_000_000).unwrap();

    // A test dollar, the treasury, the pool.
    let mint_kp = Keypair::new();
    let rent = chain.svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
    chain
        .send(
            &[
                system_instruction::create_account(&operator.pubkey(), &mint_kp.pubkey(), rent, spl_token::state::Mint::LEN as u64, &spl_token::id()),
                spl_token::instruction::initialize_mint2(&spl_token::id(), &mint_kp.pubkey(), &operator.pubkey(), None, 6).unwrap(),
            ],
            &operator,
            &[&mint_kp],
        )
        .unwrap();
    let mint = mint_kp.pubkey();
    let treasury = chain.token_account(&operator, &mint, &operator.pubkey());
    let pool = v2(&[b"pool", operator.pubkey().as_ref(), mint.as_ref()]);
    let tree_acct = v2(&[b"tree", pool.as_ref()]);
    let vault = v2(&[b"vault", pool.as_ref()]);
    let mut data = disc("initialize").to_vec();
    data.extend_from_slice(&NOTE_AMOUNT.to_le_bytes());
    data.extend_from_slice(&FEE_BPS.to_le_bytes());
    data.extend_from_slice(&K_MIN.to_le_bytes());
    let init = Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(tree_acct, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(treasury, false),
            AccountMeta::new(operator.pubkey(), true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    chain.send(&[init], &operator, &[]).unwrap();

    // Three approved members, one note each, each leaf folded in on chain.
    let mut members = vec![];
    let mut notes = vec![];
    for _ in 0..3 {
        let m = Keypair::new();
        chain.svm.airdrop(&m.pubkey(), 1_000_000_000).unwrap();
        let ta = chain.token_account(&operator, &mint, &m.pubkey());
        chain
            .send(&[spl_token::instruction::mint_to(&spl_token::id(), &mint, &ta, &operator.pubkey(), &[], NOTE_AMOUNT).unwrap()], &operator, &[])
            .unwrap();
        let mut data = disc("allow").to_vec();
        data.extend_from_slice(m.pubkey().as_ref());
        let allow = Instruction {
            program_id: POOL_ID,
            accounts: vec![
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(v2(&[b"allow", pool.as_ref(), m.pubkey().as_ref()]), false),
                AccountMeta::new(operator.pubkey(), true),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        chain.send(&[allow], &operator, &[]).unwrap();

        let note = Note::new(&pool);
        let c = note.commitment();
        let mut data = disc("deposit").to_vec();
        data.extend_from_slice(&c);
        let deposit = Instruction {
            program_id: POOL_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(v2(&[b"allow", pool.as_ref(), m.pubkey().as_ref()]), false),
                AccountMeta::new(v2(&[b"deposit", pool.as_ref(), &c]), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(ta, false),
                AccountMeta::new(vault, false),
                AccountMeta::new(m.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        chain.send(&[deposit], &m, &[]).unwrap();
        for _ in 0..3 {
            let mut data = disc("insert").to_vec();
            data.push(48);
            let insert = Instruction {
                program_id: POOL_ID,
                accounts: vec![
                    AccountMeta::new_readonly(pool, false),
                    AccountMeta::new(tree_acct, false),
                    AccountMeta::new_readonly(v2(&[b"deposit", pool.as_ref(), &c]), false),
                ],
                data,
            };
            chain.send(&[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), insert], &relayer, &[]).unwrap();
        }
        members.push((m, ta));
        notes.push(note);
    }

    // The client's tree and the program's tree agree.
    let tree = Tree::new(notes.iter().enumerate().map(|(i, n)| (i as u32, n.leaf())));
    let t = chain.svm.get_account(&tree_acct).unwrap().data;
    assert_eq!(&t[T_ROOTS + 2 * 32..T_ROOTS + 3 * 32], &enc(&tree.root())[..], "root after the third leaf");

    // The relayer opens a clean account; note 1's proofs are bound to it.
    let clean = Keypair::new().pubkey();
    let clean_ta = chain.token_account(&relayer, &mint, &clean);
    let bundle = prove_withdraw_bound(&notes[1], &tree, 1, &clean_ta);
    let buffer = Keypair::new();
    let plan = staged_withdraw_txs(&VERIFIER_ID, &relayer.pubkey(), &buffer.pubkey(), &bundle);
    let mut max_cu = 0;
    for step in &plan {
        chain.svm.expire_blockhash();
        let tx = step.sign(&relayer, &buffer, chain.svm.latest_blockhash());
        let meta = chain.svm.send_transaction(tx).unwrap_or_else(|e| panic!("{}: {:?}", step.label, e.err));
        max_cu = max_cu.max(meta.compute_units_consumed);
    }
    println!("verified on chain in {} transactions, max {} CU", plan.len(), max_cu);

    let withdraw = |nullifier: [u8; 32]| {
        let mut data = disc("withdraw").to_vec();
        data.extend_from_slice(&nullifier);
        Instruction {
            program_id: POOL_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(tree_acct, false),
                AccountMeta::new_readonly(buffer.pubkey(), false),
                AccountMeta::new(v2(&[b"nullifier", pool.as_ref(), &nullifier]), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(vault, false),
                AccountMeta::new(clean_ta, false),
                AccountMeta::new(treasury, false),
                AccountMeta::new(relayer.pubkey(), true),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        }
    };
    let nf = bundle.nullifier_bytes();
    // The relayer alone signs: no member key, no committee key.
    let cu = chain.send(&[withdraw(nf)], &relayer, &[]).expect("withdraw from the verified buffer");
    println!("withdraw settled: {cu} CU; clean {} , treasury {}", chain.balance(&clean_ta), chain.balance(&treasury));
    assert_eq!(chain.balance(&clean_ta), NOTE_AMOUNT - FEE);
    assert_eq!(chain.balance(&treasury), FEE);
    assert_eq!(chain.balance(&vault), 2 * NOTE_AMOUNT);

    // The same verified buffer cannot pay twice.
    assert_eq!(
        chain.send(&[withdraw(nf)], &relayer, &[]).unwrap_err(),
        TransactionError::InstructionError(0, InstructionError::Custom(0))
    );

    // The relayer gets the buffer's rent back.
    let before = chain.svm.get_balance(&relayer.pubkey()).unwrap();
    chain.send(&[staged_client::close(&VERIFIER_ID, &buffer.pubkey(), &relayer.pubkey(), &relayer.pubkey())], &relayer, &[]).unwrap();
    println!("buffer closed, relayer +{} lamports", chain.svm.get_balance(&relayer.pubkey()).unwrap() as i128 - before as i128);

    // Note 2 exits in public by revealing its secret; the refund can only go home.
    let (_, ta2) = &members[2];
    let n2 = &notes[2];
    let c2 = n2.commitment();
    let nf2 = enc(&n2.nullifier());
    let mut data = disc("exit").to_vec();
    data.extend_from_slice(&enc(&n2.secret));
    data.extend_from_slice(&nf2);
    let exit = Instruction {
        program_id: POOL_ID,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(v2(&[b"deposit", pool.as_ref(), &c2]), false),
            AccountMeta::new(v2(&[b"nullifier", pool.as_ref(), &nf2]), false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(*ta2, false),
            AccountMeta::new(relayer.pubkey(), true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let cu = chain.send(&[exit], &relayer, &[]).expect("exit");
    println!("exit settled: {cu} CU; depositor 2 refunded {}", chain.balance(ta2));
    assert_eq!(chain.balance(ta2), NOTE_AMOUNT);
    assert_eq!(chain.balance(&vault), NOTE_AMOUNT);
}
