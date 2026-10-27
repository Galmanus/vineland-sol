//! The whole Vineland flow, live on Solana devnet, with real riverrun proofs.
//!
//! Run (from `client/`, after deploying the program):
//!   cargo run --release --bin devnet_demo
//!
//! The default CLI wallet (`~/.config/solana/id.json`) only funds fresh roles.
//! The privacy claim is about who signs what, so the roles never share a key:
//! operator, two verifiers (2-of-2), three approved members, one relayer, and a
//! clean wallet that is never funded by anyone but the payout. The note token is
//! a devnet test mint standing in for a dollar stablecoin (6 decimals).
//!
//! Prints every signature and writes `docs/DEVNET_RUN.md`.

use std::fmt::Write as _;
use std::time::Instant;

use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature, Signer},
    system_instruction,
    transaction::Transaction,
};
use vineland_client::*;

const NOTE: u64 = 1_000_000; // 1.00 of a 6-decimal test dollar
const FEE_BPS: u16 = 197;
const K_MIN: u32 = 3;
const MEMBERS: usize = 3;

/// `VINELAND_RPC` overrides the cluster (a local test validator, say); links then
/// point at a custom-cluster explorer view of that URL.
fn rpc_url() -> String {
    std::env::var("VINELAND_RPC").unwrap_or_else(|_| "https://api.devnet.solana.com".to_string())
}

fn link(sig: &Signature) -> String {
    match std::env::var("VINELAND_RPC") {
        Ok(url) => format!("https://explorer.solana.com/tx/{sig}?cluster=custom&customUrl={url}"),
        Err(_) => format!("https://explorer.solana.com/tx/{sig}?cluster=devnet"),
    }
}

struct Run {
    rpc: RpcClient,
    log: String,
}

impl Run {
    fn send(&mut self, label: &str, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Signature {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        let bh = self.rpc.get_latest_blockhash().expect("blockhash");
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, bh);
        let size = 1 + 64 * tx.signatures.len() + tx.message.serialize().len();
        match self.rpc.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                println!("  {label:<34} OK   {sig}  ({size} B)");
                let _ = writeln!(self.log, "| {label} | [`{}…`]({}) |", &sig.to_string()[..16], link(&sig));
                sig
            }
            Err(e) => {
                eprintln!("  {label:<34} FAIL {}", e.to_string().lines().next().unwrap_or(""));
                std::process::exit(1);
            }
        }
    }

    /// A transaction that must be refused. Sent with preflight, so the refusal
    /// costs nothing and the error is the program's own.
    fn refused(&mut self, label: &str, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) {
        let mut signers = vec![payer];
        signers.extend_from_slice(extra);
        let bh = self.rpc.get_latest_blockhash().expect("blockhash");
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, bh);
        match self.rpc.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                eprintln!("  {label:<34} SETTLED, expected a refusal: {sig}");
                std::process::exit(1);
            }
            Err(e) => {
                let raw = e.to_string();
                let why = explain(raw.lines().next().unwrap_or(""));
                println!("  {label:<34} REFUSED  {why}");
                let _ = writeln!(self.log, "| {label} | refused: `{why}` |");
            }
        }
    }

    fn pool_state(&self, pool: &Pubkey) -> PoolState {
        PoolState::parse(&self.rpc.get_account_data(pool).expect("pool account")).expect("pool layout")
    }

    /// Every leaf of the pool, from the chain: one getProgramAccounts with a
    /// memcmp on the pool address at offset 8.
    fn deposits(&self, pool: &Pubkey) -> Vec<DepositRecord> {
        let cfg = RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::DataSize(DEPOSIT_RECORD_LEN as u64),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(8, pool.as_ref())),
            ]),
            account_config: RpcAccountInfoConfig { commitment: Some(CommitmentConfig::confirmed()), ..Default::default() },
            ..Default::default()
        };
        let mut out: Vec<DepositRecord> = self
            .rpc
            .get_program_accounts_with_config(&PROGRAM_ID, cfg)
            .expect("deposit records")
            .into_iter()
            .filter_map(|(_, a)| DepositRecord::parse(&a.data))
            .collect();
        out.sort_by_key(|d| d.index);
        out
    }
}

/// The refusals this demo provokes, named. Codes: Anchor's 3012, the system
/// program's `AccountAlreadyInUse` (0) on the nullifier PDA, and the pool's own.
fn explain(line: &str) -> String {
    let code = line.split("custom program error: ").nth(1).map(|c| c.split(';').next().unwrap_or(c).trim());
    let name = match code {
        Some("0xbc4") => "AccountNotInitialized: no allowlist entry for this key",
        Some("0x0") => "AccountAlreadyInUse: this nullifier is already spent",
        Some("0x1776") => "AnonymitySetTooSmall: the set under the root is below k_min",
        Some("0x1772") => "NotEnoughAttestations",
        Some("0x177d") => "NonCanonical",
        _ => return line.to_string(),
    };
    format!("{} ({name})", code.unwrap())
}

fn token_account_ixs(payer: &Pubkey, acct: &Pubkey, mint: &Pubkey, owner: &Pubkey, rent: u64) -> Vec<Instruction> {
    vec![
        system_instruction::create_account(payer, acct, rent, 165, &spl_token::id()),
        spl_token::instruction::initialize_account3(&spl_token::id(), acct, mint, owner).unwrap(),
    ]
}

fn main() {
    let rpc = RpcClient::new_with_commitment(rpc_url(), CommitmentConfig::confirmed());
    let home = std::env::var("HOME").unwrap();
    let payer = read_keypair_file(format!("{home}/.config/solana/id.json")).expect("~/.config/solana/id.json");
    let mut run = Run { rpc, log: String::new() };

    let operator = Keypair::new();
    let committee = [Keypair::new(), Keypair::new()];
    let members: Vec<Keypair> = (0..MEMBERS).map(|_| Keypair::new()).collect();
    let relayer = Keypair::new();
    let clean = Keypair::new();
    let mint = Keypair::new();
    let treasury = Keypair::new();
    let pool = pool_address(&operator.pubkey(), &mint.pubkey());

    println!("program  : {PROGRAM_ID}\npool     : {pool}\nmint     : {}\nrelayer  : {}\nclean    : {}\n", mint.pubkey(), relayer.pubkey(), clean.pubkey());
    let _ = writeln!(run.log, "| step | signature |\n|---|---|");

    // 0. fund the roles that pay their own way; the clean wallet gets nothing
    let mut fund = vec![
        system_instruction::transfer(&payer.pubkey(), &operator.pubkey(), 60_000_000),
        system_instruction::transfer(&payer.pubkey(), &relayer.pubkey(), 20_000_000),
    ];
    for m in &members {
        fund.push(system_instruction::transfer(&payer.pubkey(), &m.pubkey(), 10_000_000));
    }
    run.send("fund roles", &fund, &payer, &[]);

    // 1. the operator opens the pool: test dollar mint, fee treasury, 2-of-2 committee
    let rent_mint = run.rpc.get_minimum_balance_for_rent_exemption(82).unwrap();
    let rent_acct = run.rpc.get_minimum_balance_for_rent_exemption(165).unwrap();
    let mut ixs = vec![
        system_instruction::create_account(&operator.pubkey(), &mint.pubkey(), rent_mint, 82, &spl_token::id()),
        spl_token::instruction::initialize_mint2(&spl_token::id(), &mint.pubkey(), &operator.pubkey(), None, 6).unwrap(),
    ];
    ixs.extend(token_account_ixs(&operator.pubkey(), &treasury.pubkey(), &mint.pubkey(), &operator.pubkey(), rent_acct));
    run.send("create test dollar + treasury", &ixs, &operator, &[&mint, &treasury]);
    let keys: Vec<Pubkey> = committee.iter().map(|k| k.pubkey()).collect();
    run.send(
        "initialize pool",
        &[initialize_ix(&operator.pubkey(), &mint.pubkey(), &treasury.pubkey(), &keys, 2, NOTE, FEE_BPS, K_MIN)],
        &operator,
        &[],
    );

    // 2. an unapproved key cannot deposit; approved members each deposit one note
    let mut notes = vec![];
    let mut tokens = vec![];
    for (i, m) in members.iter().enumerate() {
        let ta = Keypair::new();
        let mut ixs = token_account_ixs(&operator.pubkey(), &ta.pubkey(), &mint.pubkey(), &m.pubkey(), rent_acct);
        ixs.push(spl_token::instruction::mint_to(&spl_token::id(), &mint.pubkey(), &ta.pubkey(), &operator.pubkey(), &[], NOTE).unwrap());
        if i == 0 {
            run.send(&format!("member {i}: token account + 1 note"), &ixs, &operator, &[&ta]);
            let note = Note::new(&pool);
            run.refused(
                "deposit before approval",
                &[deposit_ix(&pool, &mint.pubkey(), &m.pubkey(), &ta.pubkey(), note.commitment())],
                m,
                &[],
            );
            run.send(&format!("operator approves member {i}"), &[allow_ix(&pool, &operator.pubkey(), &m.pubkey())], &operator, &[]);
        } else {
            ixs.push(allow_ix(&pool, &operator.pubkey(), &m.pubkey()));
            run.send(&format!("member {i}: token account + note + approval"), &ixs, &operator, &[&ta]);
        }
        let note = Note::new(&pool);
        run.send(
            &format!("member {i} deposits (signs)"),
            &[deposit_ix(&pool, &mint.pubkey(), &m.pubkey(), &ta.pubkey(), note.commitment())],
            m,
            &[],
        );
        notes.push(note);
        tokens.push(ta.pubkey());
    }

    // 3. the operator publishes the root of the deposits it reads from chain
    let records = run.deposits(&pool);
    assert_eq!(records.len(), MEMBERS);
    let tree = tree_from_deposits(&records, MEMBERS as u32).unwrap();
    run.send(
        "publish root",
        &[publish_root_ix(&pool, &operator.pubkey(), enc(&tree.root()), MEMBERS as u32)],
        &operator,
        &[],
    );

    // 4. member 0 proves locally; the bundle is all that leaves their machine
    let idx0 = records.iter().find(|d| d.commitment == notes[0].commitment()).unwrap().index;
    let t = Instant::now();
    let bundle = prove_withdraw(&notes[0], &tree, idx0);
    let prove_ms = t.elapsed().as_millis();
    println!("  member 0 proves locally            {prove_ms} ms, binding {} B, membership {} B", bundle.binding.len(), bundle.membership.len());

    // 5. the relayer makes the clean wallet's token account (no link to the member)
    let recipient = Keypair::new();
    run.send(
        "relayer opens clean wallet account",
        &token_account_ixs(&relayer.pubkey(), &recipient.pubkey(), &mint.pubkey(), &clean.pubkey(), rent_acct),
        &relayer,
        &[&recipient],
    );

    // 6. each verifier rebuilds the tree from chain, checks both proofs, signs
    let state = run.pool_state(&pool);
    let onchain = run.deposits(&pool);
    let mut atts = vec![];
    let t = Instant::now();
    for v in &committee {
        committee_check_withdraw(&bundle, &pool, &state.membership_root, state.root_set_size, &onchain).expect("verifier accepts");
        atts.push(attestation_ix(v, &withdraw_message(&pool, &bundle.root_bytes(), &bundle.nullifier_bytes(), &recipient.pubkey())));
    }
    let verify_ms = t.elapsed().as_millis() / committee.len() as u128;
    println!("  committee verified (each)          {verify_ms} ms");

    // 7. the relayer settles; member 0 signs nothing
    let mut ixs = atts.clone();
    ixs.push(withdraw_ix(&pool, &mint.pubkey(), &treasury.pubkey(), &relayer.pubkey(), &recipient.pubkey(), bundle.nullifier_bytes(), bundle.root_bytes()));
    let wsig = run.send("relayer withdraws to clean wallet", &ixs, &relayer, &[]);
    let paid = spl_token::state::Account::unpack_from_slice(&run.rpc.get_account_data(&recipient.pubkey()).unwrap()).unwrap().amount;
    println!("  clean wallet received              {paid} (note {NOTE}, fee {} bps)", FEE_BPS);

    // 8. replay, and exit-after-withdraw: both refused by the chain
    run.refused("replay the same withdrawal", &ixs, &relayer, &[]);
    let exit0 = prove_exit(&notes[0]);
    let rec0 = onchain.iter().find(|d| d.commitment == notes[0].commitment()).unwrap();
    committee_check_exit(&exit0, &pool, rec0).expect("the exit proof itself is valid");
    let msg = exit_message(&pool, &exit0.commitment_bytes(), &exit0.nullifier_bytes());
    let mut ixs: Vec<Instruction> = committee.iter().map(|v| attestation_ix(v, &msg)).collect();
    ixs.push(ragequit_ix(&pool, &mint.pubkey(), &members[0].pubkey(), &tokens[0], exit0.commitment_bytes(), exit0.nullifier_bytes()));
    run.refused("member 0 also tries to exit", &ixs, &members[0], &[]);

    // 9. member 2 exits publicly and gets the whole note back
    let exit2 = prove_exit(&notes[2]);
    let rec2 = onchain.iter().find(|d| d.commitment == notes[2].commitment()).unwrap();
    committee_check_exit(&exit2, &pool, rec2).expect("verifier accepts exit");
    let msg = exit_message(&pool, &exit2.commitment_bytes(), &exit2.nullifier_bytes());
    let mut ixs: Vec<Instruction> = committee.iter().map(|v| attestation_ix(v, &msg)).collect();
    ixs.push(ragequit_ix(&pool, &mint.pubkey(), &members[2].pubkey(), &tokens[2], exit2.commitment_bytes(), exit2.nullifier_bytes()));
    run.send("member 2 exits (public refund)", &ixs, &members[2], &[]);

    // 10. with one exit, the set under the root is below k_min: member 1 is protected from withdrawing into a set of two
    let b1 = prove_withdraw(&notes[1], &tree, onchain.iter().find(|d| d.commitment == notes[1].commitment()).unwrap().index);
    let mut ixs: Vec<Instruction> = committee
        .iter()
        .map(|v| attestation_ix(v, &withdraw_message(&pool, &b1.root_bytes(), &b1.nullifier_bytes(), &recipient.pubkey())))
        .collect();
    ixs.push(withdraw_ix(&pool, &mint.pubkey(), &treasury.pubkey(), &relayer.pubkey(), &recipient.pubkey(), b1.nullifier_bytes(), b1.root_bytes()));
    run.refused("withdraw below the k floor", &ixs, &relayer, &[]);

    let s = run.pool_state(&pool);
    let report = format!(
        "# Vineland on Solana: {} run\n\n\
         - cluster: `{}`\n- program: `{PROGRAM_ID}`\n\
         - pool: `{pool}`\n- test dollar mint: `{}` (6 decimals, devnet only)\n\
         - note: {NOTE} base units, fee {FEE_BPS} bps, k_min {K_MIN}, committee 2-of-2\n\
         - relayer: `{}`\n- clean wallet: `{}` (never funded; received {paid})\n\
         - withdrawal: [{wsig}]({})\n\
         - proofs: binding {} B + membership {} B, proved in {prove_ms} ms, verified in {verify_ms} ms per verifier (this machine)\n\
         - final state: leaf_count {}, live_notes {}, exits {}\n\n{}",
        if std::env::var("VINELAND_RPC").is_ok() { "local validator" } else { "devnet" },
        rpc_url(),
        mint.pubkey(),
        relayer.pubkey(),
        clean.pubkey(),
        link(&wsig),
        bundle.binding.len(),
        bundle.membership.len(),
        s.leaf_count,
        s.live_notes,
        s.exits,
        run.log
    );
    let path = match std::env::var("VINELAND_RPC") {
        Ok(_) => concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/LOCAL_RUN.md"),
        Err(_) => concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/DEVNET_RUN.md"),
    };
    std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs")).unwrap();
    std::fs::write(path, report).unwrap();
    println!("\nwrote {path}");
}
