//! The Vineland relayer: submits committee-free withdrawals for depositors who
//! must not sign them (their key on the withdrawal would link the payout to
//! their deposit).
//!
//! It takes a recipient-bound proof bundle ([`BoundWithdraw`]) over HTTP,
//! verifies it natively and checks it against the chain before spending
//! anything, then pays for and drives the staged on-chain verification, the
//! withdrawal, and the buffer close that returns its rent. The payout goes to
//! the token account bound inside the proofs: nothing here chooses or rewrites
//! a recipient, and the pool refuses any account but the bound one.
//!
//! Endpoints (JSON in, JSON out):
//!   POST /withdraw  a BoundWithdraw. It may carry one extra field, `owner`: the
//!                   wallet whose associated token account is the bound
//!                   recipient, so the relayer can create that account if it
//!                   does not exist yet (checked: ATA(owner, mint) must equal
//!                   the bound recipient, or the request is refused).
//!   POST /crank     {"pool": "..."}: fold every deposit not yet in the tree.
//!   GET  /health    relayer pubkey, SOL balance, cluster URL.
//!
//! One job (withdraw or crank) at a time: a job that arrives while another runs
//! gets 409 and should retry later (the `vineland` CLI does). /health answers
//! meanwhile. Errors are `{"error": "..."}` with 400 (refused before spending),
//! 409 (busy), 413 (body too large), 502 (a transaction failed; the response
//! carries the signatures that landed) or 503 (relayer underfunded).
//!
//! Config (env): VINELAND_RPC (default devnet), RELAYER_KEYPAIR (path to the
//! relayer's keypair, required), BIND (default 127.0.0.1:8787). Refuses
//! mainnet: the programs are not audited.
//!
//! Run (from `client/`):
//!   RELAYER_KEYPAIR=relayer.json cargo run --release --bin relayer

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Instant;

use riverrun_m31_verifier::client as staged_client;
use serde_json::{json, Value};
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    signature::{read_keypair_file, Keypair, Signature, Signer},
};
use tiny_http::{Header, Method, Request, Response, Server};
use vineland_client::v2::{self, Rpc, VERIFIER_ID};
use vineland_client::{staged_buffer_len, staged_withdraw_txs, verify_bound, BoundWithdraw};

/// A real bundle is ~390 KB of JSON (two hex-encoded proofs).
const MAX_BODY: usize = 2 * 1024 * 1024;
/// Request threads: one may be busy with a job, the rest answer /health and 409.
const WORKERS: usize = 4;
/// Fees and slack on top of the rents a withdrawal fronts.
const FEE_MARGIN: u64 = 10_000_000;

struct Relayer {
    rpc: Rpc,
    key: Keypair,
    job: Mutex<()>,
    busy: AtomicBool,
}

type Reply = (u16, Value);

fn refuse(code: u16, msg: impl Into<String>) -> Reply {
    (code, json!({ "error": msg.into() }))
}

fn main() {
    let url = std::env::var("VINELAND_RPC").unwrap_or_else(|_| v2::DEVNET_URL.to_string());
    let rpc = Rpc::new(&url);
    if let Err(e) = rpc.refuse_mainnet() {
        eprintln!("relayer: {e}");
        std::process::exit(1);
    }
    let Ok(path) = std::env::var("RELAYER_KEYPAIR") else {
        eprintln!("relayer: set RELAYER_KEYPAIR to the relayer's keypair file (it pays fees and fronts ~1.4 SOL of buffer rent per withdrawal)");
        std::process::exit(1);
    };
    let key = read_keypair_file(&path).unwrap_or_else(|e| {
        eprintln!("relayer: cannot read keypair {path}: {e}");
        std::process::exit(1)
    });
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8787".to_string());
    let server = Arc::new(Server::http(&bind).unwrap_or_else(|e| {
        eprintln!("relayer: cannot bind {bind}: {e}");
        std::process::exit(1)
    }));
    eprintln!("relayer {} listening on http://{bind}, cluster {url}", key.pubkey());

    let state = Arc::new(Relayer { rpc, key, job: Mutex::new(()), busy: AtomicBool::new(false) });
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (server, state) = (server.clone(), state.clone());
            std::thread::spawn(move || loop {
                match server.recv() {
                    Ok(req) => handle(&state, req),
                    Err(e) => eprintln!("relayer: accept: {e}"),
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

fn handle(st: &Relayer, mut req: Request) {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let (code, body) = match (req.method(), path.as_str()) {
        (Method::Get, "/health") => health(st),
        (Method::Post, "/withdraw") => match read_body(&mut req) {
            Ok(body) => exclusive(st, || withdraw(st, &body)),
            Err(r) => r,
        },
        (Method::Post, "/crank") => match read_body(&mut req) {
            Ok(body) => exclusive(st, || crank(st, &body)),
            Err(r) => r,
        },
        (_, "/health" | "/withdraw" | "/crank") => refuse(405, "method not allowed"),
        _ => refuse(404, "not found: POST /withdraw, POST /crank, GET /health"),
    };
    let json_type = Header::from_bytes("Content-Type", "application/json").unwrap();
    let resp = Response::from_string(body.to_string()).with_status_code(code).with_header(json_type);
    if let Err(e) = req.respond(resp) {
        eprintln!("relayer: respond: {e}");
    }
}

fn read_body(req: &mut Request) -> Result<Vec<u8>, Reply> {
    if req.body_length().is_some_and(|n| n > MAX_BODY) {
        return Err(refuse(413, format!("body over {MAX_BODY} bytes")));
    }
    let mut body = Vec::new();
    req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).map_err(|e| refuse(400, format!("read body: {e}")))?;
    if body.len() > MAX_BODY {
        return Err(refuse(413, format!("body over {MAX_BODY} bytes")));
    }
    Ok(body)
}

/// Run `job` if no other job is running, else 409. A panic (hostile input
/// reaching an assert somewhere below) becomes a 500, not a dead worker.
fn exclusive(st: &Relayer, job: impl FnOnce() -> Reply) -> Reply {
    let _guard = match st.job.try_lock() {
        Ok(g) => g,
        Err(TryLockError::WouldBlock) => return refuse(409, "busy: one job at a time, retry later"),
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
    };
    st.busy.store(true, Ordering::SeqCst);
    let reply = catch_unwind(AssertUnwindSafe(job)).unwrap_or_else(|_| refuse(500, "internal error (see the relayer log)"));
    st.busy.store(false, Ordering::SeqCst);
    reply
}

fn health(st: &Relayer) -> Reply {
    match st.rpc.client.get_balance(&st.key.pubkey()) {
        Ok(lamports) => (
            200,
            json!({
                "relayer": st.key.pubkey().to_string(),
                "balance_lamports": lamports,
                "balance_sol": lamports as f64 / LAMPORTS_PER_SOL as f64,
                "cluster": st.rpc.url,
                "busy": st.busy.load(Ordering::SeqCst),
            }),
        ),
        Err(e) => refuse(502, format!("cluster unreachable: {e}")),
    }
}

fn sig_list(sigs: &[Signature]) -> Vec<String> {
    sigs.iter().map(|s| s.to_string()).collect()
}

fn crank(st: &Relayer, body: &[u8]) -> Reply {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return refuse(400, format!("not JSON: {e}")),
    };
    let Some(pool) = req.get("pool").and_then(Value::as_str).and_then(|p| p.parse::<Pubkey>().ok()) else {
        return refuse(400, "expected {\"pool\": \"<pool address>\"}");
    };
    if let Err(e) = st.rpc.pool(&pool) {
        return refuse(400, e);
    }
    let started = Instant::now();
    let mut sigs = vec![];
    let result = st.rpc.crank(&pool, &st.key, &mut sigs);
    let (inserted, leaf_count) = match (st.rpc.tree_state(&pool), st.rpc.pool(&pool)) {
        (Ok(t), Ok(p)) => (Some(t.inserted), Some(p.leaf_count)),
        _ => (None, None),
    };
    eprintln!("crank {pool}: {} insert transactions, inserted {inserted:?} of {leaf_count:?}", sigs.len());
    let mut out = json!({
        "pool": pool.to_string(),
        "signatures": sig_list(&sigs),
        "inserted": inserted,
        "leaf_count": leaf_count,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    match result {
        Ok(()) => (200, out),
        Err(e) => {
            out["error"] = json!(e);
            (502, out)
        }
    }
}

/// Everything about a bundle the relayer can learn without spending: the
/// native verification, then the chain reads that decide whether the pool
/// would pay (pool, root, anonymity floor, nullifier, recipient account).
struct Checked {
    pool: Pubkey,
    recipient: Pubkey,
    p: v2::PoolV2,
    /// The recipient's wallet, when the recipient (its ATA) must be created.
    create_for: Option<Pubkey>,
    nullifier: [u8; 32],
}

fn check(st: &Relayer, b: &BoundWithdraw, owner: Option<&str>) -> Result<Checked, Reply> {
    let (pool, recipient) = verify_bound(b).map_err(|r| refuse(400, format!("bundle refused by native verification: {r:?}")))?;
    let p = st.rpc.pool(&pool).map_err(|e| refuse(400, e))?;
    if recipient == v2::vault_address(&pool) {
        return Err(refuse(400, "the recipient is the pool's vault"));
    }
    // verify_bound checked every digest is canonical, so `enc` cannot panic.
    let root = b.root_bytes();
    let nullifier = b.nullifier_bytes();
    let t = st.rpc.tree_state(&pool).map_err(|e| refuse(502, e))?;
    let size = t.size_of(&root).ok_or_else(|| refuse(400, "the bundle's root is not in the pool's recent root history: re-prove against the current tree"))?;
    if size.saturating_sub(p.exits) < p.k_min {
        return Err(refuse(
            400,
            format!("anonymity set too small: the root covers {size} leaves, {} exited, pool floor k_min = {}", p.exits, p.k_min),
        ));
    }
    if st.rpc.exists(&v2::nullifier_address(&pool, &nullifier)).map_err(|e| refuse(502, e))? {
        return Err(refuse(400, "this note is already spent (its nullifier record exists)"));
    }
    let create_for = match st.rpc.account(&recipient).map_err(|e| refuse(502, e))? {
        Some(a) => {
            if a.owner != spl_token::id() {
                return Err(refuse(400, format!("the recipient {recipient} is not an SPL token account")));
            }
            let ta = spl_token::state::Account::unpack(&a.data).map_err(|e| refuse(400, format!("the recipient {recipient}: {e}")))?;
            if ta.mint != p.mint {
                return Err(refuse(400, format!("the recipient {recipient} holds mint {}, the pool pays {}", ta.mint, p.mint)));
            }
            if ta.state != spl_token::state::AccountState::Initialized {
                return Err(refuse(400, format!("the recipient {recipient} is frozen")));
            }
            None
        }
        None => {
            // Only the account the proofs are bound to can be created: the
            // wallet is the requester's claim, checked against the binding.
            let wallet = owner
                .and_then(|o| o.parse::<Pubkey>().ok())
                .ok_or_else(|| refuse(400, format!("the recipient {recipient} does not exist; send \"owner\" (its wallet) so the relayer can create it")))?;
            if v2::ata(&wallet, &p.mint) != recipient {
                return Err(refuse(400, format!("the recipient {recipient} is not the associated token account of {wallet} for {}", p.mint)));
            }
            Some(wallet)
        }
    };
    Ok(Checked { pool, recipient, p, create_for, nullifier })
}

fn withdraw(st: &Relayer, body: &[u8]) -> Reply {
    let mut req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return refuse(400, format!("not JSON: {e}")),
    };
    let owner = req.as_object_mut().and_then(|o| o.remove("owner"));
    let b: BoundWithdraw = match serde_json::from_value(req) {
        Ok(b) => b,
        Err(e) => return refuse(400, format!("not a BoundWithdraw: {e}")),
    };
    let started = Instant::now();
    let c = match check(st, &b, owner.as_ref().and_then(Value::as_str)) {
        Ok(c) => c,
        Err(r) => {
            eprintln!("withdraw refused: {}", r.1["error"]);
            return r;
        }
    };
    let relayer = st.key.pubkey();
    // The buffer (refunded by CLOSE), the nullifier record, and maybe the
    // recipient's token account, at the rent the staged plan itself assumes.
    let rent = Rent::default();
    let need = rent.minimum_balance(staged_buffer_len(&b)) + rent.minimum_balance(9) + rent.minimum_balance(165) + FEE_MARGIN;
    match st.rpc.client.get_balance(&relayer) {
        Ok(have) if have >= need => {}
        Ok(have) => return refuse(503, format!("relayer underfunded: has {have} lamports, a withdrawal fronts about {need}")),
        Err(e) => return refuse(502, format!("cluster unreachable: {e}")),
    }
    eprintln!("withdraw pool={} recipient={} nullifier={}..", c.pool, c.recipient, &hex::encode(c.nullifier)[..16]);

    let mut sigs = serde_json::Map::new();
    let mut sent = 0usize;
    let fail = |stage: &str, e: String, sigs: serde_json::Map<String, Value>, closed: Option<bool>| -> Reply {
        eprintln!("withdraw failed at {stage}: {e}");
        let mut out = json!({ "error": format!("{stage}: {}", v2::explain(e.lines().next().unwrap_or(&e))), "signatures": sigs });
        if let Some(closed) = closed {
            out["buffer_closed"] = json!(closed);
        }
        (502, out)
    };

    if let Some(wallet) = c.create_for {
        match st.rpc.land(&[v2::create_ata_ix(&relayer, &wallet, &c.p.mint)], &st.key, &[]) {
            Ok((sig, _)) => {
                sent += 1;
                sigs.insert("create_recipient".into(), json!(sig.to_string()));
            }
            Err(e) => return fail("create recipient account", e, sigs, None),
        }
    }

    // The staged verification: create + INIT (the buffer co-signs), the WRITEs
    // (independent: sent as one batch), then each stage in order.
    let buffer = Keypair::new();
    let plan = staged_withdraw_txs(&VERIFIER_ID, &relayer, &buffer.pubkey(), &b);
    let close = |sigs: &mut serde_json::Map<String, Value>| -> bool {
        match st.rpc.land(&[staged_client::close(&VERIFIER_ID, &buffer.pubkey(), &relayer, &relayer)], &st.key, &[]) {
            Ok((sig, _)) => {
                sigs.insert("close".into(), json!(sig.to_string()));
                true
            }
            Err(e) => {
                eprintln!("close buffer {} failed: {e}", buffer.pubkey());
                false
            }
        }
    };
    let (init, rest) = plan.split_first().expect("a plan starts with create + INIT");
    match st.rpc.land(&init.instructions, &st.key, &[&buffer]) {
        Ok((sig, _)) => {
            sent += 1;
            sigs.insert("init".into(), json!(sig.to_string()));
        }
        Err(e) => return fail(&init.label, e, sigs, None),
    }
    let (writes, stages): (Vec<_>, Vec<_>) = rest.iter().partition(|t| t.label.contains("WRITE"));
    let batch: Vec<_> = writes.iter().map(|t| t.instructions.clone()).collect();
    match st.rpc.land_all(&batch, &st.key) {
        Ok(s) => sent += s.len(),
        Err(e) => {
            let closed = close(&mut sigs);
            return fail("WRITE", e, sigs, Some(closed));
        }
    }
    for step in stages {
        match st.rpc.land(&step.instructions, &st.key, &[]) {
            Ok((sig, _)) => {
                sent += 1;
                if step.label == "SEAL" {
                    sigs.insert("seal".into(), json!(sig.to_string()));
                } else if step.label == "FINALIZE" {
                    sigs.insert("finalize".into(), json!(sig.to_string()));
                }
            }
            Err(e) => {
                let closed = close(&mut sigs);
                return fail(&step.label, e, sigs, Some(closed));
            }
        }
    }

    let ix = v2::withdraw_ix(&c.pool, &c.p.mint, &c.p.treasury, &buffer.pubkey(), &c.recipient, &relayer, c.nullifier);
    match st.rpc.land(&[ix], &st.key, &[]) {
        Ok((sig, _)) => {
            sent += 1;
            sigs.insert("withdraw".into(), json!(sig.to_string()));
        }
        Err(e) => {
            let closed = close(&mut sigs);
            return fail("withdraw", e, sigs, Some(closed));
        }
    }
    let closed = close(&mut sigs);
    if closed {
        sent += 1;
    }
    let balance = st.rpc.token_balance(&c.recipient).ok().flatten();
    eprintln!("withdraw settled: {sent} transactions in {} ms, recipient balance {balance:?}", started.elapsed().as_millis());
    let mut out = json!({
        "pool": c.pool.to_string(),
        "recipient": c.recipient.to_string(),
        "nullifier": hex::encode(c.nullifier),
        "paid": c.p.note_amount - c.p.fee(),
        "fee": c.p.fee(),
        "signatures": sigs,
        "transactions": sent,
        "recipient_balance": balance,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    if !closed {
        out["warning"] = json!(format!("paid, but closing buffer {} failed: its rent is still locked", buffer.pubkey()));
    }
    (200, out)
}
