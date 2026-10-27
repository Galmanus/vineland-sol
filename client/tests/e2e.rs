//! End to end with real riverrun crowd proofs against the compiled program.
//!
//! Run (proving is slow unoptimized):
//!   (cd ../programs/vineland-pool && cargo-build-sbf) && cargo test --release

use litesvm::LiteSVM;
use solana_sdk::{
    instruction::{Instruction, InstructionError},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::{Transaction, TransactionError},
};
use std::time::Instant;
use vineland_client::*;

const NOTE: u64 = 1_000_000;
const FEE_BPS: u16 = 197;
const FEE: u64 = 19_700;
/// Solana's packet limit for a whole serialized transaction.
const PACKET: usize = 1232;

struct Chain {
    svm: LiteSVM,
    authority: Keypair,
    mint: Pubkey,
    pool: Pubkey,
    treasury: Pubkey,
}

impl Chain {
    fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<usize, TransactionError> {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, self.svm.latest_blockhash());
        let size = 1 + 64 * tx.signatures.len() + tx.message.serialize().len();
        let res = self.svm.send_transaction(tx).map(|_| size).map_err(|f| f.err);
        self.svm.expire_blockhash();
        res
    }

    fn token_account(&mut self, owner: &Pubkey, payer: &Keypair) -> Pubkey {
        let acct = Keypair::new();
        let rent = self.svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
        let ixs = [
            system_instruction::create_account(&payer.pubkey(), &acct.pubkey(), rent, 165, &spl_token::id()),
            spl_token::instruction::initialize_account3(&spl_token::id(), &acct.pubkey(), &self.mint, owner).unwrap(),
        ];
        self.send(&ixs, payer, &[&acct]).unwrap();
        acct.pubkey()
    }

    fn balance(&self, a: &Pubkey) -> u64 {
        spl_token::state::Account::unpack(&self.svm.get_account(a).unwrap().data).unwrap().amount
    }

    fn pool_state(&self) -> PoolState {
        PoolState::parse(&self.svm.get_account(&self.pool).unwrap().data).unwrap()
    }

    /// LiteSVM has no getProgramAccounts; a live client uses one memcmp at
    /// offset 8. Same records either way.
    fn records(&self, commitments: &[[u8; 32]]) -> Vec<DepositRecord> {
        commitments
            .iter()
            .map(|c| {
                let a = self.svm.get_account(&pda(&[b"deposit", self.pool.as_ref(), c])).unwrap();
                DepositRecord::parse(&a.data).unwrap()
            })
            .collect()
    }
}

fn setup(committee: &[Keypair], threshold: u8, k_min: u32) -> Chain {
    let mut svm = LiteSVM::new();
    let so = concat!(env!("CARGO_MANIFEST_DIR"), "/../programs/vineland-pool/target/deploy/vineland_pool.so");
    svm.add_program_from_file(PROGRAM_ID, so).expect("build the program first");
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), 10_000_000_000).unwrap();
    let mint = Keypair::new();
    let mut chain = Chain { svm, authority: authority.insecure_clone(), mint: mint.pubkey(), pool: Pubkey::default(), treasury: Pubkey::default() };
    let rent = chain.svm.minimum_balance_for_rent_exemption(82);
    chain
        .send(
            &[
                system_instruction::create_account(&authority.pubkey(), &mint.pubkey(), rent, 82, &spl_token::id()),
                spl_token::instruction::initialize_mint2(&spl_token::id(), &mint.pubkey(), &authority.pubkey(), None, 6).unwrap(),
            ],
            &authority,
            &[&mint],
        )
        .unwrap();
    chain.treasury = chain.token_account(&authority.pubkey(), &authority);
    chain.pool = pool_address(&authority.pubkey(), &chain.mint);
    let keys: Vec<Pubkey> = committee.iter().map(|k| k.pubkey()).collect();
    let ix = initialize_ix(&authority.pubkey(), &chain.mint, &chain.treasury, &keys, threshold, NOTE, FEE_BPS, k_min);
    chain.send(&[ix], &authority, &[]).unwrap();
    chain
}

struct Member {
    key: Keypair,
    token: Pubkey,
    note: Note,
}

/// An approved member who deposits one note.
fn join(chain: &mut Chain) -> Member {
    let key = Keypair::new();
    chain.svm.airdrop(&key.pubkey(), 1_000_000_000).unwrap();
    let a = chain.authority.insecure_clone();
    let token = chain.token_account(&key.pubkey(), &a);
    let mint_to = spl_token::instruction::mint_to(&spl_token::id(), &chain.mint, &token, &a.pubkey(), &[], NOTE).unwrap();
    chain.send(&[mint_to], &a, &[]).unwrap();
    chain.send(&[allow_ix(&chain.pool, &a.pubkey(), &key.pubkey())], &a, &[]).unwrap();
    let note = Note::new(&chain.pool);
    let ix = deposit_ix(&chain.pool, &chain.mint, &key.pubkey(), &token, note.commitment());
    chain.send(&[ix], &key, &[]).unwrap();
    Member { key, token, note }
}

#[test]
fn a_stark_backed_withdrawal_pays_a_clean_wallet_and_the_note_cannot_also_exit() {
    let committee = [Keypair::new(), Keypair::new()];
    let mut chain = setup(&committee, 2, 3);
    let members: Vec<Member> = (0..4).map(|_| join(&mut chain)).collect();
    let commitments: Vec<[u8; 32]> = members.iter().map(|m| m.note.commitment()).collect();

    // the operator publishes the root of the deposits it sees
    let records = chain.records(&commitments);
    let tree = tree_from_deposits(&records, 4).unwrap();
    let root = enc(&tree.root());
    let a = chain.authority.insecure_clone();
    chain.send(&[publish_root_ix(&chain.pool, &a.pubkey(), root, 4)], &a, &[]).unwrap();

    // member 0 proves on their own machine; the secret never leaves it
    let t = Instant::now();
    let bundle = prove_withdraw(&members[0].note, &tree, 0);
    let prove_ms = t.elapsed().as_millis();
    let json = serde_json::to_vec(&bundle).unwrap();

    // each verifier, independently: rebuild the tree from the chain, check the proofs, sign
    let clean = Keypair::new(); // never funded, never touched by the depositor
    let relayer = Keypair::new();
    chain.svm.airdrop(&relayer.pubkey(), 1_000_000_000).unwrap();
    let recipient = chain.token_account(&clean.pubkey(), &relayer);
    let state = chain.pool_state();
    let mut attestations = vec![];
    let t = Instant::now();
    for v in &committee {
        let received: WithdrawBundle = serde_json::from_slice(&json).unwrap();
        let onchain = chain.records(&commitments);
        committee_check_withdraw(&received, &chain.pool, &state.membership_root, state.root_set_size, &onchain)
            .expect("an honest verifier accepts a valid withdrawal");
        let msg = withdraw_message(&chain.pool, &received.root_bytes(), &received.nullifier_bytes(), &recipient);
        attestations.push(attestation_ix(v, &msg));
    }
    let verify_ms = t.elapsed().as_millis() / committee.len() as u128;

    // the relayer submits: the depositor's key signs nothing here
    let mut ixs = attestations;
    ixs.push(withdraw_ix(&chain.pool, &chain.mint, &chain.treasury, &relayer.pubkey(), &recipient, bundle.nullifier_bytes(), bundle.root_bytes()));
    let size = chain.send(&ixs, &relayer, &[]).expect("withdraw settles");
    assert!(size <= PACKET, "withdraw tx is {size} B, over the {PACKET} B packet");
    assert_eq!(chain.balance(&recipient), NOTE - FEE);
    assert_eq!(chain.balance(&chain.treasury), FEE);
    println!(
        "binding {} B, membership {} B, prove {prove_ms} ms, verify {verify_ms} ms per verifier, withdraw tx {size} B",
        bundle.binding.len(),
        bundle.membership.len()
    );

    // the same note tries to exit too. Its exit proof is valid (the nullifier
    // really is the note's), so an honest committee signs; the chain refuses,
    // because withdraw and exit burn the same nullifier.
    let exit = prove_exit(&members[0].note);
    assert_eq!(exit.nullifier, bundle.nullifier, "one note, one nullifier");
    let rec0 = &chain.records(&commitments[..1])[0];
    committee_check_exit(&exit, &chain.pool, rec0).unwrap();
    let msg = exit_message(&chain.pool, &exit.commitment_bytes(), &exit.nullifier_bytes());
    let mut ixs: Vec<Instruction> = committee.iter().map(|v| attestation_ix(v, &msg)).collect();
    ixs.push(ragequit_ix(&chain.pool, &chain.mint, &members[0].key.pubkey(), &members[0].token, exit.commitment_bytes(), exit.nullifier_bytes()));
    let err = chain.send(&ixs, &members[0].key, &[]).unwrap_err();
    assert_eq!(err, TransactionError::InstructionError(2, InstructionError::Custom(0)), "nullifier already spent");
    assert_eq!(chain.balance(&members[0].token), 0);

    // member 1 exits publicly and gets the whole note back
    let exit1 = prove_exit(&members[1].note);
    let rec1 = &chain.records(&commitments[1..2])[0];
    committee_check_exit(&exit1, &chain.pool, rec1).unwrap();
    let msg = exit_message(&chain.pool, &exit1.commitment_bytes(), &exit1.nullifier_bytes());
    let mut ixs: Vec<Instruction> = committee.iter().map(|v| attestation_ix(v, &msg)).collect();
    ixs.push(ragequit_ix(&chain.pool, &chain.mint, &members[1].key.pubkey(), &members[1].token, exit1.commitment_bytes(), exit1.nullifier_bytes()));
    chain.send(&ixs, &members[1].key, &[]).unwrap();
    assert_eq!(chain.balance(&members[1].token), NOTE);

    let s = chain.pool_state();
    assert_eq!((s.live_notes, s.exits), (2, 1));
    let vault = vault_address(&chain.pool);
    assert_eq!(chain.balance(&vault), 2 * NOTE);
}

#[test]
fn an_honest_verifier_refuses_what_it_should() {
    let committee = [Keypair::new()];
    let mut chain = setup(&committee, 1, 1);
    let members: Vec<Member> = (0..2).map(|_| join(&mut chain)).collect();
    let commitments: Vec<[u8; 32]> = members.iter().map(|m| m.note.commitment()).collect();
    let records = chain.records(&commitments);
    let tree = tree_from_deposits(&records, 2).unwrap();

    // 1. an operator that publishes a root over a leaf nobody deposited (its own
    //    note) gets no signature, even though the outsider's proof is valid for it
    let outsider = Note::new(&chain.pool);
    let forged = Tree::new([(0, members[0].note.leaf()), (1, members[1].note.leaf()), (2, outsider.leaf())]);
    let a = chain.authority.insecure_clone();
    chain.send(&[publish_root_ix(&chain.pool, &a.pubkey(), enc(&forged.root()), 2)], &a, &[]).unwrap();
    let s = chain.pool_state();
    let theft = prove_withdraw(&outsider, &forged, 2);
    assert_eq!(
        committee_check_withdraw(&theft, &chain.pool, &s.membership_root, s.root_set_size, &records),
        Err(Refusal::RootNotTheDeposits)
    );

    // 2. with the honest root published, the outsider's bundle is for another root
    chain.send(&[publish_root_ix(&chain.pool, &a.pubkey(), enc(&tree.root()), 2)], &a, &[]).unwrap();
    let s = chain.pool_state();
    assert_eq!(
        committee_check_withdraw(&theft, &chain.pool, &s.membership_root, s.root_set_size, &records),
        Err(Refusal::RootNotCurrent)
    );

    // 3. a real bundle with its nullifier swapped (to spend someone else's slot,
    //    or a fresh one) fails the proof
    let honest = prove_withdraw(&members[0].note, &tree, 0);
    committee_check_withdraw(&honest, &chain.pool, &s.membership_root, s.root_set_size, &records).unwrap();
    let mut swapped = honest.clone();
    swapped.nullifier[0] = (swapped.nullifier[0] + 1) % M31_P;
    assert_eq!(
        committee_check_withdraw(&swapped, &chain.pool, &s.membership_root, s.root_set_size, &records),
        Err(Refusal::ProofRejected)
    );

    // 4. a tampered membership proof
    let mut tampered = honest.clone();
    let mid = tampered.membership.len() / 2;
    tampered.membership[mid] ^= 0x01;
    assert!(matches!(
        committee_check_withdraw(&tampered, &chain.pool, &s.membership_root, s.root_set_size, &records),
        Err(Refusal::ProofRejected) | Err(Refusal::Malformed)
    ));

    // 5. a bundle for another pool
    let mut elsewhere = honest.clone();
    elsewhere.pool = Pubkey::new_unique().to_string();
    assert_eq!(
        committee_check_withdraw(&elsewhere, &chain.pool, &s.membership_root, s.root_set_size, &records),
        Err(Refusal::WrongPool)
    );

    // 6. an exit that claims another depositor's commitment, or another nullifier
    let exit0 = prove_exit(&members[0].note);
    assert_eq!(committee_check_exit(&exit0, &chain.pool, &records[1]), Err(Refusal::Malformed));
    let mut other_nf = exit0.clone();
    other_nf.nullifier[3] = (other_nf.nullifier[3] + 1) % M31_P;
    assert_eq!(committee_check_exit(&other_nf, &chain.pool, &records[0]), Err(Refusal::ProofRejected));
    committee_check_exit(&exit0, &chain.pool, &records[0]).unwrap();
}
