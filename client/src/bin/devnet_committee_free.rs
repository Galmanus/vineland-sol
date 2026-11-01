//! The committee-free Vineland flow, live on Solana devnet, with nothing mocked:
//! the v2 pool program folds every deposit into its own Poseidon2-M31 tree, the
//! riverrun M31 verifier program verifies the recipient-bound crowd proofs on
//! chain across transactions, and the pool pays the clean account from that
//! verified buffer. No committee key exists in this run.
//!
//! Run (from `client/`, after deploying both programs):
//!   cargo run --release --bin devnet_committee_free
//!
//! The default CLI wallet (`~/.config/solana/id.json`) only funds fresh roles:
//! operator, three approved members, one relayer, and a clean wallet that is
//! never funded by anyone but the payout. The note token is a devnet test mint
//! standing in for a dollar stablecoin (6 decimals).
//!
//! Prints every signature and writes `docs/DEVNET_RUN_COMMITTEE_FREE.md`.

use std::fmt::Write as _;
use std::time::Instant;

use riverrun_m31_verifier::client as staged_client;
use solana_sdk::{
    instruction::Instruction,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature, Signer},
    system_instruction,
    transaction::Transaction,
};
use vineland_client::v2::{self, allow_ix, deposit_ix, explain, insert_ixs, token_account_ixs, withdraw_ix, Rpc, POOL_ID, VERIFIER_ID};
use vineland_client::{enc, prove_withdraw_bound, staged_withdraw_txs, tree_from_deposits, Note, Tree};

const NOTE: u64 = 1_000_000; // 1.00 of a 6-decimal test dollar
const FEE_BPS: u16 = 197;
const K_MIN: u32 = 3;
const MEMBERS: usize = 3;

fn rpc_url() -> String {
    std::env::var("VINELAND_RPC").unwrap_or_else(|_| v2::DEVNET_URL.to_string())
}

fn link(kind: &str, id: &str) -> String {
    match std::env::var("VINELAND_RPC") {
        Ok(url) => format!("https://explorer.solana.com/{kind}/{id}?cluster=custom&customUrl={url}"),
        Err(_) => format!("https://explorer.solana.com/{kind}/{id}?cluster=devnet"),
    }
}

struct Run {
    rpc: Rpc,
    log: String,
    sent: usize,
}

impl Run {
    /// [`Rpc::land`], counting what landed.
    fn land(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<(Signature, usize), String> {
        let landed = self.rpc.land(ixs, payer, extra)?;
        self.sent += 1;
        Ok(landed)
    }

    fn send(&mut self, label: &str, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Signature {
        match self.land(ixs, payer, extra) {
            Ok((sig, size)) => {
                println!("  {label:<40} OK   {sig}  ({size} B)");
                let _ = writeln!(self.log, "| {label} | [`{}…`]({}) |", &sig.to_string()[..16], link("tx", &sig.to_string()));
                sig
            }
            Err(e) => {
                eprintln!("  {label:<40} FAIL {}", e.lines().next().unwrap_or(""));
                std::process::exit(1);
            }
        }
    }

    /// A transaction that must be refused. Sent with preflight, so the refusal
    /// costs nothing and the error is the program's own.
    fn refused(&mut self, label: &str, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        let bh = self.rpc.client.get_latest_blockhash().expect("blockhash");
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, bh);
        match self.rpc.client.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                eprintln!("  {label:<40} SETTLED, expected a refusal: {sig}");
                std::process::exit(1);
            }
            Err(e) => {
                let raw = e.to_string();
                let why = explain(raw.lines().next().unwrap_or(""));
                println!("  {label:<40} REFUSED  {why}");
                let _ = writeln!(self.log, "| {label} | refused: `{why}` |");
            }
        }
    }

    fn balance(&self, account: &Pubkey) -> u64 {
        spl_token::state::Account::unpack(&self.rpc.client.get_account_data(account).expect("token account")).unwrap().amount
    }

    /// Every leaf of the pool, from the chain, as the tree the client proves on.
    fn tree(&self, pool: &Pubkey) -> Tree {
        let deposits = self.rpc.deposits(pool).expect("deposit records");
        tree_from_deposits(&deposits, deposits.len() as u32).expect("canonical leaves")
    }
}

/// `secret` is whose secret is revealed; `note` is whose deposit it claims.
fn exit_ix(pool: &Pubkey, mint: &Pubkey, note: &Note, depositor_token: &Pubkey, payer: &Pubkey, secret: &Note) -> Instruction {
    v2::exit_ix(pool, mint, note.commitment(), enc(&secret.secret), enc(&secret.nullifier()), depositor_token, payer)
}

fn main() {
    let rpc = Rpc::new(&rpc_url());
    let home = std::env::var("HOME").unwrap();
    let payer = read_keypair_file(format!("{home}/.config/solana/id.json")).expect("~/.config/solana/id.json");
    let mut run = Run { rpc, log: String::new(), sent: 0 };
    let started = Instant::now();

    let operator = Keypair::new();
    let members: Vec<Keypair> = (0..MEMBERS).map(|_| Keypair::new()).collect();
    let relayer = Keypair::new();
    let clean = Keypair::new();
    let mint = Keypair::new();
    let treasury = Keypair::new();
    let pool = v2::pool_address(&operator.pubkey(), &mint.pubkey());
    let vault = v2::vault_address(&pool);
    let ta_rent = run.rpc.client.get_minimum_balance_for_rent_exemption(165).unwrap();
    let mint_rent = run.rpc.client.get_minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN).unwrap();

    println!("pool program     : {POOL_ID}");
    println!("verifier program : {VERIFIER_ID}");
    println!("pool             : {pool}");
    println!("relayer          : {}", relayer.pubkey());
    println!("clean            : {}\n", clean.pubkey());

    // 0. fund the roles that pay their own way; the clean wallet gets nothing.
    //    The relayer fronts the verifier buffer's rent (~1.37 SOL, refunded by CLOSE).
    let mut fund = vec![
        system_instruction::transfer(&payer.pubkey(), &operator.pubkey(), 100_000_000),
        system_instruction::transfer(&payer.pubkey(), &relayer.pubkey(), 1_600_000_000),
    ];
    for m in &members {
        fund.push(system_instruction::transfer(&payer.pubkey(), &m.pubkey(), 10_000_000));
    }
    run.send("fund roles", &fund, &payer, &[]);

    let mut ixs = vec![
        system_instruction::create_account(&operator.pubkey(), &mint.pubkey(), mint_rent, spl_token::state::Mint::LEN as u64, &spl_token::id()),
        spl_token::instruction::initialize_mint2(&spl_token::id(), &mint.pubkey(), &operator.pubkey(), None, 6).unwrap(),
    ];
    ixs.extend(token_account_ixs(&operator.pubkey(), &treasury.pubkey(), &mint.pubkey(), &operator.pubkey(), ta_rent));
    run.send("create test dollar + treasury", &ixs, &operator, &[&mint, &treasury]);

    let init = v2::initialize_ix(&operator.pubkey(), &mint.pubkey(), &treasury.pubkey(), NOTE, FEE_BPS, K_MIN);
    run.send("initialize pool (no committee)", &[init], &operator, &[]);

    // 1. members: token account + one note each. Member 0 tries before approval.
    let notes: Vec<Note> = (0..MEMBERS).map(|_| Note::new(&pool)).collect();
    let mut tokens = vec![];
    for (i, m) in members.iter().enumerate() {
        let ta = Keypair::new();
        let mut ixs = token_account_ixs(&operator.pubkey(), &ta.pubkey(), &mint.pubkey(), &m.pubkey(), ta_rent);
        ixs.push(spl_token::instruction::mint_to(&spl_token::id(), &mint.pubkey(), &ta.pubkey(), &operator.pubkey(), &[], NOTE).unwrap());
        run.send(&format!("member {i}: token account + 1 note"), &ixs, &operator, &[&ta]);
        tokens.push(ta.pubkey());
    }
    run.refused(
        "member 0 deposits before approval",
        &[deposit_ix(&pool, &mint.pubkey(), &members[0].pubkey(), &tokens[0], notes[0].commitment())],
        &members[0],
        &[],
    );
    for (i, m) in members.iter().enumerate() {
        run.send(&format!("operator approves member {i}"), &[allow_ix(&pool, &operator.pubkey(), &m.pubkey())], &operator, &[]);
        run.send(
            &format!("member {i} deposits (signs)"),
            &[deposit_ix(&pool, &mint.pubkey(), &m.pubkey(), &tokens[i], notes[i].commitment())],
            m,
            &[],
        );
    }

    // 2. the tree, folded on chain by anyone (the relayer here), in order.
    run.refused("fold leaf 1 before leaf 0", &insert_ixs(&pool, notes[1].commitment()), &relayer, &[]);
    for (i, n) in notes.iter().enumerate() {
        for part in 0..3 {
            run.send(&format!("fold leaf {i} into the tree ({}/3)", part + 1), &insert_ixs(&pool, n.commitment()), &relayer, &[]);
        }
    }
    let tree = run.tree(&pool);
    let onchain = run.rpc.tree_state(&pool).expect("tree account");
    let (onchain_root, covered) = onchain.latest().expect("a root after three folds");
    assert_eq!((onchain_root, covered), (enc(&tree.root()), 3), "the program's root must equal the client's");
    println!("  on-chain root == client root over the 3 deposits");

    // 3. the clean account: opened by the relayer, so no funding edge to anyone.
    let clean_ta = Keypair::new();
    run.send(
        "relayer opens clean wallet account",
        &token_account_ixs(&relayer.pubkey(), &clean_ta.pubkey(), &mint.pubkey(), &clean.pubkey(), ta_rent),
        &relayer,
        &[&clean_ta],
    );
    let other_ta = Keypair::new();
    run.send(
        "relayer opens an account of its own",
        &token_account_ixs(&relayer.pubkey(), &other_ta.pubkey(), &mint.pubkey(), &relayer.pubkey(), ta_rent),
        &relayer,
        &[&other_ta],
    );

    // 4. member 0 proves on its own machine, bound to the clean account.
    let t0 = Instant::now();
    let bundle = prove_withdraw_bound(&notes[0], &tree, 0, &clean_ta.pubkey());
    let prove_ms = t0.elapsed().as_millis();
    println!(
        "  member 0 proves locally (bound)            {} ms  (binding {} B + membership {} B)",
        prove_ms,
        bundle.binding.len(),
        bundle.membership.len()
    );
    let nullifier = bundle.nullifier_bytes();

    // 5. the chain verifies: upload into a verifier buffer, then every stage.
    let buffer = Keypair::new();
    let plan = staged_withdraw_txs(&VERIFIER_ID, &relayer.pubkey(), &buffer.pubkey(), &bundle);
    let total = plan.len();
    let t1 = Instant::now();
    let mut first_sig = None;
    let mut seal_sig = None;
    let mut finalize_sig = None;
    for (k, step) in plan.iter().enumerate() {
        if k + 1 == total {
            // One step short of FINALIZE: every stage ran, the buffer still is not settleable.
            run.refused(
                "withdraw before verification finishes",
                &[withdraw_ix(&pool, &mint.pubkey(), &treasury.pubkey(), &buffer.pubkey(), &clean_ta.pubkey(), &relayer.pubkey(), nullifier)],
                &relayer,
                &[],
            );
        }
        let extra: Vec<&Keypair> = if step.buffer_signs { vec![&buffer] } else { vec![] };
        let (sig, _) = run.land(&step.instructions, &relayer, &extra).unwrap_or_else(|e| {
            eprintln!("  {} FAIL {}", step.label, e.lines().next().unwrap_or(""));
            std::process::exit(1)
        });
        if k == 0 {
            first_sig = Some(sig);
        }
        if step.label.contains("SEAL") {
            seal_sig = Some(sig);
        }
        if k + 1 == total {
            finalize_sig = Some(sig);
        }
        if k % 25 == 0 || k + 1 == total {
            println!("  verifier: {:>3}/{total} {:<28} {sig}", k + 1, step.label);
        }
    }
    let verify_s = t1.elapsed().as_secs();
    for (label, sig) in [("verifier: create + INIT buffer", first_sig), ("verifier: SEAL (proof bytes frozen)", seal_sig), ("verifier: FINALIZE (both proofs verified)", finalize_sig)] {
        let sig = sig.unwrap();
        let _ = writeln!(run.log, "| {label} | [`{}…`]({}) |", &sig.to_string()[..16], link("tx", &sig.to_string()));
    }
    let _ = writeln!(run.log, "| verifier: all {total} transactions | buffer [`{}`]({}) |", buffer.pubkey(), link("address", &buffer.pubkey().to_string()));

    // 6. the payout, and what the program refuses around it.
    run.refused(
        "relayer redirects payout to itself",
        &[withdraw_ix(&pool, &mint.pubkey(), &treasury.pubkey(), &buffer.pubkey(), &other_ta.pubkey(), &relayer.pubkey(), nullifier)],
        &relayer,
        &[],
    );
    let wsig = run.send(
        "relayer withdraws to clean wallet",
        &[withdraw_ix(&pool, &mint.pubkey(), &treasury.pubkey(), &buffer.pubkey(), &clean_ta.pubkey(), &relayer.pubkey(), nullifier)],
        &relayer,
        &[],
    );
    run.refused(
        "replay the same withdrawal",
        &[withdraw_ix(&pool, &mint.pubkey(), &treasury.pubkey(), &buffer.pubkey(), &clean_ta.pubkey(), &relayer.pubkey(), nullifier)],
        &relayer,
        &[],
    );
    run.refused(
        "member 0 also tries to exit",
        &[exit_ix(&pool, &mint.pubkey(), &notes[0], &tokens[0], &relayer.pubkey(), &notes[0])],
        &relayer,
        &[],
    );
    run.refused(
        "exit member 2's note with member 1's secret",
        &[exit_ix(&pool, &mint.pubkey(), &notes[2], &tokens[2], &relayer.pubkey(), &notes[1])],
        &relayer,
        &[],
    );
    let esig = run.send(
        "member 2 exits (reveals its secret)",
        &[exit_ix(&pool, &mint.pubkey(), &notes[2], &tokens[2], &relayer.pubkey(), &notes[2])],
        &relayer,
        &[],
    );
    run.send(
        "relayer closes the buffer (rent back)",
        &[staged_client::close(&VERIFIER_ID, &buffer.pubkey(), &relayer.pubkey(), &relayer.pubkey())],
        &relayer,
        &[],
    );

    let clean_bal = run.balance(&clean_ta.pubkey());
    let treasury_bal = run.balance(&treasury.pubkey());
    let vault_bal = run.balance(&vault);
    let refund = run.balance(&tokens[2]);
    let elapsed = started.elapsed().as_secs();
    println!(
        "\nclean wallet {clean_bal}, treasury {treasury_bal}, vault {vault_bal}, member 2 refunded {refund}; {} transactions sent in {elapsed} s (verification {verify_s} s)",
        run.sent
    );
    assert_eq!(clean_bal, NOTE - NOTE * FEE_BPS as u64 / 10_000);

    let md = format!(
        "# Vineland on Solana: committee-free devnet run\n\n\
         No committee key exists in this run. The pool program computed the membership root itself, \
         and the riverrun M31 verifier program verified both recipient-bound crowd proofs on chain before the pool paid.\n\n\
         - cluster: `{}`\n\
         - pool program (v2): [`{POOL_ID}`]({})\n\
         - verifier program: [`{VERIFIER_ID}`]({})\n\
         - pool: `{pool}`\n\
         - test dollar mint: `{}` (6 decimals, devnet only)\n\
         - note: {NOTE} base units, fee {FEE_BPS} bps, k_min {K_MIN}\n\
         - relayer: `{}`\n\
         - clean wallet: `{}` (never funded; received {clean_bal})\n\
         - withdrawal: [{wsig}]({})\n\
         - public exit: [{esig}]({})\n\
         - proofs: binding {} B + membership {} B, proved in {prove_ms} ms on this machine, bound to the clean wallet's token account\n\
         - on-chain verification: {total} transactions in {verify_s} s\n\
         - final balances: clean {clean_bal}, treasury {treasury_bal}, vault {vault_bal}, member 2 refunded {refund}\n\
         - transactions sent by this run: {} in {elapsed} s\n\n\
         | step | signature |\n|---|---|\n{}",
        rpc_url(),
        link("address", &POOL_ID.to_string()),
        link("address", &VERIFIER_ID.to_string()),
        mint.pubkey(),
        relayer.pubkey(),
        clean.pubkey(),
        link("tx", &wsig.to_string()),
        link("tx", &esig.to_string()),
        bundle.binding.len(),
        bundle.membership.len(),
        run.sent,
        run.log
    );
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/DEVNET_RUN_COMMITTEE_FREE.md");
    std::fs::write(path, md).expect("write the run record");
    println!("wrote {path}");
}
