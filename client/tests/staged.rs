//! Committee-free withdrawal, client half: a note's recipient-bound crowd proofs
//! driven through riverrun's staged on-chain verifier inside LiteSVM, under the
//! real limits (every stage behind an explicit 1.4M CU limit and the 256 KB
//! heap, every transaction checked against the 1232-byte packet), and the
//! finalized buffer read back the way the pool program will read it.
//!
//! Needs the verifier's SBF artifact in `RIVERRUN_VERIFIER_SO` (mirror-pool at
//! ce5e8c7, `cargo build-sbf` in programs/riverrun-m31-verifier, or the deployed
//! one: `solana program dump -u devnet 8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo`):
//!   cargo test --release --test staged -- --ignored --nocapture --test-threads 1

use std::sync::OnceLock;
use std::time::Instant;

use litesvm::LiteSVM;
use riverrun_m31_verifier::{client as staged_client, staged::*};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::InstructionError,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
use vineland_client::*;

/// The id the verifier's own tests load it at.
const VERIFIER_ID: Pubkey = solana_sdk::pubkey!("r1Vrrunm31VeriFier1111111111111111111111111");
const CU_CAP: u64 = 1_400_000;
const PACKET: usize = 1232;

struct Fixture {
    pool: Pubkey,
    note: Note,
    root: Digest8,
    recipient: Pubkey,
    bundle: BoundWithdraw,
}

/// Three notes in one pool, note 1 proven bound to the recipient. Proved once
/// for both tests.
fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let pool = Pubkey::new_unique();
        let notes: Vec<Note> = (0..3).map(|_| Note::new(&pool)).collect();
        let tree = Tree::new(notes.iter().enumerate().map(|(i, n)| (i as u32, n.leaf())));
        let recipient = Pubkey::new_unique();
        let t = Instant::now();
        let bundle = prove_withdraw_bound(&notes[1], &tree, 1, &recipient);
        println!(
            "proved note 1 bound to {recipient}: binding {} B, membership {} B, {} ms",
            bundle.binding.len(),
            bundle.membership.len(),
            t.elapsed().as_millis()
        );
        Fixture { pool, note: notes[1].clone(), root: tree.root(), recipient, bundle }
    })
}

fn svm() -> (LiteSVM, Keypair) {
    let mut svm = LiteSVM::new();
    let so = std::env::var("RIVERRUN_VERIFIER_SO").expect("set RIVERRUN_VERIFIER_SO to the verifier .so, e.g. `solana program dump -u devnet 8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo /tmp/riverrun_m31_verifier.so`");
    svm.add_program_from_file(VERIFIER_ID, &so).expect("load the verifier .so (cargo build-sbf in mirror-pool)");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    (svm, payer)
}

struct Run {
    txs: usize,
    max_cu: u64,
    max_cu_label: String,
    max_size: usize,
    /// The first transaction the verifier refused: (label, custom error).
    refused: Option<(String, u32)>,
}

/// Send the client's plan in order under the real limits; stop at the first
/// refusal.
fn drive(svm: &mut LiteSVM, payer: &Keypair, buffer: &Keypair, b: &BoundWithdraw) -> Run {
    let plan = staged_withdraw_txs(&VERIFIER_ID, &payer.pubkey(), &buffer.pubkey(), b);
    let mut run = Run { txs: 0, max_cu: 0, max_cu_label: String::new(), max_size: 0, refused: None };
    for t in &plan {
        svm.expire_blockhash();
        let tx = t.sign(payer, buffer, svm.latest_blockhash());
        let size = 1 + 64 * tx.signatures.len() + tx.message.serialize().len();
        assert!(size <= PACKET, "{}: {size} B, over the {PACKET} B packet", t.label);
        run.max_size = run.max_size.max(size);
        run.txs += 1;
        match svm.send_transaction(tx) {
            Ok(meta) => {
                assert!(meta.compute_units_consumed <= CU_CAP, "{}: {} CU", t.label, meta.compute_units_consumed);
                if meta.compute_units_consumed > run.max_cu {
                    run.max_cu = meta.compute_units_consumed;
                    run.max_cu_label = t.label.clone();
                }
            }
            Err(e) => match e.err {
                TransactionError::InstructionError(_, InstructionError::Custom(c)) => {
                    run.refused = Some((t.label.clone(), c));
                    break;
                }
                other => panic!("{}: {other:?}\n{}", t.label, e.meta.logs.join("\n")),
            },
        }
    }
    run
}

fn data(svm: &LiteSVM, k: &Pubkey) -> Vec<u8> {
    svm.get_account(k).map(|a| a.data).unwrap_or_default()
}

#[test]
#[ignore = "proves at production parameters and needs the verifier .so: run with --release"]
fn a_bound_withdrawal_finalizes_on_chain_with_the_pools_fields() {
    let f = fixture();
    let (mut svm, payer) = svm();
    let buffer = Keypair::new();
    let run = drive(&mut svm, &payer, &buffer, &f.bundle);
    assert!(run.refused.is_none(), "refused: {:?}", run.refused);
    let size = staged_buffer_len(&f.bundle);
    println!(
        "FINALIZED in {} transactions; max {} CU ({}, {:.1}% of the cap); largest tx {} B; buffer {size} B, rent {} lamports",
        run.txs,
        run.max_cu,
        run.max_cu_label,
        100.0 * run.max_cu as f64 / CU_CAP as f64,
        run.max_size,
        svm.minimum_balance_for_rent_exemption(size)
    );

    let d = data(&svm, &buffer.pubkey());
    assert_eq!(d.len(), size);
    assert_eq!(d[B_FINALIZED], FINAL_SETTLEABLE);
    assert_eq!(d[B_KIND], 0x83);
    assert_eq!(d[B_KIND], KIND_CROWD | KIND_BOUND);
    assert_eq!(&d[B_VK..B_VK + 32], &vk_tag(KIND_CROWD | KIND_BOUND)[..]);
    assert_eq!(&d[B_ROOT..B_ROOT + 32], &enc(&f.root)[..]);
    assert_eq!(&d[B_NULLIFIER..B_NULLIFIER + 32], &enc(&f.note.nullifier())[..]);
    assert_eq!(&d[B_ACTION..B_ACTION + 32], &enc(&note_action(&f.pool))[..]);
    let round = u128::from_le_bytes(d[B_ROUND..B_ROUND + 16].try_into().unwrap());
    assert_eq!(round, spend_round_u64(&f.pool) as u128);
    assert_eq!(&d[B_RECIPIENT..B_RECIPIENT + 32], f.recipient.as_ref());
    assert_eq!(f.bundle.nullifier_bytes(), enc(&f.note.nullifier()));
    assert_eq!(f.bundle.root_bytes(), enc(&f.root));
}

#[test]
#[ignore = "proves at production parameters and needs the verifier .so: run with --release"]
fn the_same_proof_under_another_recipient_never_finalizes() {
    let f = fixture();
    let (mut svm, payer) = svm();
    let buffer = Keypair::new();
    let r2 = Pubkey::new_unique();
    let mut redirected = f.bundle.clone();
    redirected.recipient = r2.to_string();
    let run = drive(&mut svm, &payer, &buffer, &redirected);
    let (label, code) = run.refused.expect("a proof bound to R must not verify for R2");
    println!("declared recipient R2: refused at {label} with error {code}, after {} transactions", run.txs);
    assert!(
        (label.contains("TRANSCRIPT") && code == StagedError::TranscriptRejected as u32)
            || (label.contains("QUERIES") && code == StagedError::QueryRejected as u32),
        "{label}: {code}"
    );
    let d = data(&svm, &buffer.pubkey());
    assert_eq!(&d[B_RECIPIENT..B_RECIPIENT + 32], r2.as_ref());
    assert_eq!(d[B_FINALIZED], 0);

    // FINALIZE itself is refused: some stage never completed.
    let ix = staged_client::stage(&VERIFIER_ID, &buffer.pubkey(), &payer.pubkey(), IX_FINALIZE, &[]);
    let ixs = [
        ComputeBudgetInstruction::set_compute_unit_limit(CU_CAP as u32),
        ComputeBudgetInstruction::request_heap_frame(256 * 1024),
        ix,
    ];
    svm.expire_blockhash();
    let tx = Transaction::new(&[&payer], Message::new(&ixs, Some(&payer.pubkey())), svm.latest_blockhash());
    let err = svm.send_transaction(tx).expect_err("FINALIZE of an unverified buffer");
    assert_eq!(err.err, TransactionError::InstructionError(2, InstructionError::Custom(StagedError::Incomplete as u32)));
    assert_eq!(data(&svm, &buffer.pubkey())[B_FINALIZED], 0);
}
