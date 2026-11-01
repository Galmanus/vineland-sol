//! vineland-node: the Vineland dashboard and its local API, run on the payer's
//! own machine.
//!
//! The node keeps the notes (each one's secret is the money) in files only this
//! user can read, proves on this machine, and hands the relayer nothing but a
//! recipient-bound proof bundle. A company uses it through the dashboard; an AI
//! agent through the same JSON API with the bearer token.
//!
//!   vineland-node setup     open a demo pool on devnet (operator, test dollar,
//!                           two seed notes so the privacy floor can be met)
//!   vineland-node           serve the dashboard on http://localhost:8790
//!
//! Wallets: in the dashboard, a Privy embedded wallet signs deposits and exits
//! (the node prepares the unsigned transactions); with `VINELAND_KEYPAIR` set,
//! the node signs with that keypair instead (agents, scripts, tests).
//!
//! Local-only by construction: binds 127.0.0.1, every /api call needs the
//! bearer token printed at start (and injected into the served page), the Host
//! header must be this node (no DNS rebinding) and a POST must come from this
//! origin (no cross-site requests). Refuses mainnet: the programs are not audited.

use std::collections::HashMap;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::{
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature, Signer},
    system_instruction,
    transaction::Transaction,
};
use tiny_http::{Header, Method, Request, Response, Server};
use vineland_client::v2::{self, Rpc};
use vineland_client::{enc, prove_withdraw_bound, tree_from_deposits, Note};

const NOTE_AMOUNT: u64 = 1_000_000; // 1.00 of a 6-decimal test dollar
const FEE_BPS: u16 = 197;
const K_MIN: u32 = 3;
/// Notes the demo operator mints to a newly approved wallet.
const GRANT_NOTES: u64 = 5;
/// SOL the demo operator sends a newly approved wallet for fees and rent.
const GRANT_LAMPORTS: u64 = 30_000_000;

type Res<T> = Result<T, String>;
type Reply = (u16, Value);

fn refuse(code: u16, msg: impl Into<String>) -> Reply {
    (code, json!({ "error": msg.into() }))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Write `bytes` to `path` readable by this user only (0600), atomically.
fn write_private(path: &Path, bytes: &[u8]) -> Res<()> {
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

fn read_key(path: &Path) -> Res<Keypair> {
    read_keypair_file(path).map_err(|e| format!("read keypair {}: {e}", path.display()))
}

fn save_key(path: &Path, kp: &Keypair) -> Res<()> {
    let bytes: Vec<u8> = kp.to_bytes().to_vec();
    write_private(path, serde_json::to_string(&bytes).unwrap().as_bytes())
}

// ---- configuration and the local store ----------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    rpc: String,
    pool: String,
    mint: String,
    treasury: String,
    operator: String,
}

fn state_dir() -> PathBuf {
    std::env::var("VINELAND_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".vineland/devnet"))
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredNote {
    id: String,
    wallet: String,
    note: Note,
    /// pending (deposit prepared, not landed), deposited, spent, exited.
    state: String,
    deposit_sig: Option<String>,
    created: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Receipt {
    time: u64,
    wallet: String,
    reference: String,
    note: String,
    nullifier: String,
    to: String,
    recipient_account: String,
    paid: u64,
    fee: u64,
    withdraw: String,
}

struct Store {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl Store {
    fn notes_dir(&self) -> PathBuf {
        self.dir.join("notes")
    }

    fn put(&self, n: &StoredNote) -> Res<()> {
        let _g = self.lock.lock().unwrap();
        std::fs::create_dir_all(self.notes_dir()).map_err(|e| e.to_string())?;
        write_private(&self.notes_dir().join(format!("{}.json", n.id)), serde_json::to_string_pretty(n).unwrap().as_bytes())
    }

    fn all(&self) -> Vec<StoredNote> {
        let _g = self.lock.lock().unwrap();
        let Ok(rd) = std::fs::read_dir(self.notes_dir()) else { return vec![] };
        let mut out: Vec<StoredNote> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
            .collect();
        out.sort_by_key(|n| n.created);
        out
    }

    fn get(&self, id: &str) -> Option<StoredNote> {
        self.all().into_iter().find(|n| n.id == id)
    }

    fn receipts(&self) -> Vec<Receipt> {
        let _g = self.lock.lock().unwrap();
        std::fs::read(self.dir.join("receipts.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn add_receipt(&self, r: Receipt) -> Res<()> {
        let mut all = self.receipts();
        let _g = self.lock.lock().unwrap();
        all.push(r);
        write_private(&self.dir.join("receipts.json"), serde_json::to_string_pretty(&all).unwrap().as_bytes())
    }
}

// ---- setup: a demo pool on devnet ------------------------------------------------------

fn setup() -> Res<()> {
    let dir = state_dir();
    if dir.join("config.json").exists() {
        return Err(format!("{} already has a pool; remove it to start over", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let url = std::env::var("VINELAND_RPC").unwrap_or_else(|_| v2::DEVNET_URL.to_string());
    let rpc = Rpc::new(&url);
    rpc.refuse_mainnet()?;
    let payer = read_key(&home().join(".config/solana/id.json"))?;
    let operator = Keypair::new();
    let relayer = Keypair::new();
    save_key(&dir.join("operator.json"), &operator)?;
    save_key(&dir.join("relayer.json"), &relayer)?;
    let seeds: Vec<Keypair> = (0..2).map(|_| Keypair::new()).collect();

    let mut fund = vec![
        system_instruction::transfer(&payer.pubkey(), &operator.pubkey(), 250_000_000),
        system_instruction::transfer(&payer.pubkey(), &relayer.pubkey(), 1_600_000_000),
    ];
    for s in &seeds {
        fund.push(system_instruction::transfer(&payer.pubkey(), &s.pubkey(), 20_000_000));
    }
    let (sig, _) = rpc.land(&fund, &payer, &[])?;
    println!("funded operator, relayer and two seed members: {sig}");

    let mint = Keypair::new();
    let treasury = Keypair::new();
    let ta_rent = rpc.client.get_minimum_balance_for_rent_exemption(165).map_err(|e| e.to_string())?;
    let mint_rent = rpc.client.get_minimum_balance_for_rent_exemption(82).map_err(|e| e.to_string())?;
    let mut ixs = vec![
        system_instruction::create_account(&operator.pubkey(), &mint.pubkey(), mint_rent, 82, &spl_token::id()),
        spl_token::instruction::initialize_mint2(&spl_token::id(), &mint.pubkey(), &operator.pubkey(), None, 6).unwrap(),
    ];
    ixs.extend(v2::token_account_ixs(&operator.pubkey(), &treasury.pubkey(), &mint.pubkey(), &operator.pubkey(), ta_rent));
    let (sig, _) = rpc.land(&ixs, &operator, &[&mint, &treasury])?;
    println!("test dollar {} and treasury: {sig}", mint.pubkey());
    let init = v2::initialize_ix(&operator.pubkey(), &mint.pubkey(), &treasury.pubkey(), NOTE_AMOUNT, FEE_BPS, K_MIN);
    let (sig, _) = rpc.land(&[init], &operator, &[])?;
    let pool = v2::pool_address(&operator.pubkey(), &mint.pubkey());
    println!("pool {pool}: {sig}");

    for (i, s) in seeds.iter().enumerate() {
        let token = v2::ata(&s.pubkey(), &mint.pubkey());
        let ixs = vec![
            v2::allow_ix(&pool, &operator.pubkey(), &s.pubkey()),
            v2::create_ata_ix(&operator.pubkey(), &s.pubkey(), &mint.pubkey()),
            spl_token::instruction::mint_to(&spl_token::id(), &mint.pubkey(), &token, &operator.pubkey(), &[], NOTE_AMOUNT).unwrap(),
        ];
        rpc.land(&ixs, &operator, &[])?;
        let note = Note::new(&pool);
        let (sig, _) = rpc.land(&[v2::deposit_ix(&pool, &mint.pubkey(), &s.pubkey(), &token, note.commitment())], s, &[])?;
        println!("seed member {i} deposited: {sig}");
    }
    let mut sigs = vec![];
    rpc.crank(&pool, &operator, &mut sigs)?;
    println!("folded the seed notes into the tree ({} transactions)", sigs.len());

    let cfg = Config {
        rpc: url,
        pool: pool.to_string(),
        mint: mint.pubkey().to_string(),
        treasury: treasury.pubkey().to_string(),
        operator: operator.pubkey().to_string(),
    };
    write_private(&dir.join("config.json"), serde_json::to_string_pretty(&cfg).unwrap().as_bytes())?;
    println!("\nwrote {}\nnow start the relayer:\n  RELAYER_KEYPAIR={} VINELAND_RPC={} target/release/relayer", dir.join("config.json").display(), dir.join("relayer.json").display(), cfg.rpc);
    println!("and the node:\n  target/release/vineland-node");
    Ok(())
}

// ---- the node --------------------------------------------------------------------------

#[derive(Clone, Default, Serialize)]
struct Job {
    id: u64,
    state: String,
    stage: String,
    done: usize,
    total: usize,
    paid_notes: usize,
    notes: usize,
    error: Option<String>,
    receipts: Vec<Receipt>,
}

struct Node {
    rpc: Rpc,
    cfg: Config,
    pool: Pubkey,
    mint: Pubkey,
    relayer_url: String,
    token: String,
    port: u16,
    dashboard: PathBuf,
    store: Store,
    keypair: Option<Keypair>,
    operator: Option<Keypair>,
    jobs: Mutex<HashMap<u64, Job>>,
    next_job: AtomicU64,
}

fn random_token() -> String {
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("/dev/urandom");
    hex::encode(b)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("setup") {
        if let Err(e) = setup() {
            eprintln!("setup: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = serve() {
        eprintln!("vineland-node: {e}");
        std::process::exit(1);
    }
}

fn serve() -> Res<()> {
    let dir = state_dir();
    let cfg: Config = serde_json::from_slice(
        &std::fs::read(dir.join("config.json")).map_err(|_| format!("no pool configured in {}: run `vineland-node setup` first", dir.display()))?,
    )
    .map_err(|e| e.to_string())?;
    let rpc = Rpc::new(&cfg.rpc);
    rpc.refuse_mainnet()?;
    let port: u16 = std::env::var("VINELAND_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8790);
    let keypair = match std::env::var("VINELAND_KEYPAIR") {
        Ok(p) => Some(read_key(Path::new(&p))?),
        Err(_) => None,
    };
    let operator = read_key(&dir.join("operator.json")).ok();
    let dashboard = std::env::var("VINELAND_DASHBOARD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../dashboard/dist")));
    let node = Arc::new(Node {
        pool: cfg.pool.parse().map_err(|_| "bad pool in config")?,
        mint: cfg.mint.parse().map_err(|_| "bad mint in config")?,
        rpc,
        cfg,
        relayer_url: std::env::var("VINELAND_RELAYER").unwrap_or_else(|_| "http://127.0.0.1:8787".into()),
        token: random_token(),
        port,
        dashboard,
        store: Store { dir: dir.clone(), lock: Mutex::new(()) },
        keypair,
        operator,
        jobs: Mutex::new(HashMap::new()),
        next_job: AtomicU64::new(1),
    });
    let bind = format!("127.0.0.1:{port}");
    let server = Arc::new(Server::http(&bind).map_err(|e| format!("bind {bind}: {e}"))?);
    println!("Vineland dashboard: http://localhost:{port}");
    println!("API token (agents send it as `Authorization: Bearer <token>`): {}", node.token);
    println!("pool {} on {}, relayer {}", node.pool, node.cfg.rpc, node.relayer_url);
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (server, node) = (server.clone(), node.clone());
            std::thread::spawn(move || loop {
                if let Ok(req) = server.recv() {
                    handle(&node, req);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn handle(node: &Arc<Node>, mut req: Request) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query: HashMap<String, String> = url
        .split_once('?')
        .map(|(_, q)| q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
        .unwrap_or_default();

    // No DNS rebinding: the Host must be this node.
    let host_ok = header(&req, "Host").is_some_and(|h| h == format!("localhost:{}", node.port) || h == format!("127.0.0.1:{}", node.port));
    if !host_ok {
        return respond_json(req, refuse(403, "wrong Host"));
    }

    if !path.starts_with("/api/") {
        return serve_static(node, req, &path);
    }
    let authed = header(&req, "Authorization").is_some_and(|a| a == format!("Bearer {}", node.token));
    if !authed {
        return respond_json(req, refuse(401, "missing or wrong bearer token"));
    }
    if *req.method() == Method::Post {
        // No cross-site requests: a browser POST must come from this origin.
        if let Some(origin) = header(&req, "Origin") {
            if origin != format!("http://localhost:{}", node.port) && origin != format!("http://127.0.0.1:{}", node.port) {
                return respond_json(req, refuse(403, "cross-origin request refused"));
            }
        }
    }
    let mut body = Vec::new();
    let _ = req.as_reader().take(1 << 20).read_to_end(&mut body);
    let body: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).unwrap_or(json!({})) };

    let reply = match (req.method(), path.as_str()) {
        (Method::Get, "/api/config") => config(node),
        (Method::Get, "/api/status") => status(node, query.get("wallet").map(String::as_str)),
        (Method::Post, "/api/onboard") => onboard(node, &body),
        (Method::Post, "/api/deposit/prepare") => deposit_prepare(node, &body),
        (Method::Post, "/api/deposit/confirm") => deposit_confirm(node, &body),
        (Method::Post, "/api/deposit") => deposit_keypair(node, &body),
        (Method::Post, "/api/pay") => pay(node, &body),
        (Method::Get, p) if p.starts_with("/api/jobs/") => job(node, &p["/api/jobs/".len()..]),
        (Method::Get, "/api/receipts") => receipts(node, query.get("wallet").map(String::as_str)),
        (Method::Get, "/api/receipts.csv") => {
            let csv = receipts_csv(node, query.get("wallet").map(String::as_str));
            let resp = Response::from_string(csv)
                .with_header(Header::from_bytes("Content-Type", "text/csv").unwrap())
                .with_header(Header::from_bytes("Content-Disposition", "attachment; filename=\"vineland-receipts.csv\"").unwrap());
            let _ = req.respond(resp);
            return;
        }
        (Method::Post, "/api/exit/prepare") => exit_prepare(node, &body),
        (Method::Post, "/api/exit/confirm") => exit_confirm(node, &body),
        _ => refuse(404, "not found"),
    };
    respond_json(req, reply);
}

fn respond_json(req: Request, (code, body): Reply) {
    let resp = Response::from_string(body.to_string())
        .with_status_code(code)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    let _ = req.respond(resp);
}

fn serve_static(node: &Node, req: Request, path: &str) {
    let rel = if path == "/" || path.is_empty() { "index.html" } else { path.trim_start_matches('/') };
    if rel.contains("..") {
        return respond_json(req, refuse(400, "bad path"));
    }
    let file = node.dashboard.join(rel);
    let (file, rel) = if file.is_file() { (file, rel) } else { (node.dashboard.join("index.html"), "index.html") };
    let Ok(mut bytes) = std::fs::read(&file) else {
        return respond_json(req, refuse(404, format!("dashboard not built: {}", node.dashboard.display())));
    };
    if rel == "index.html" {
        bytes = String::from_utf8_lossy(&bytes).replace("__VINELAND_TOKEN__", &node.token).into_bytes();
    }
    let ctype = match Path::new(rel).extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    let resp = Response::from_data(bytes)
        .with_header(Header::from_bytes("Content-Type", ctype).unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    let _ = req.respond(resp);
}

// ---- API -------------------------------------------------------------------------------

fn wallet_of(node: &Node, v: Option<&str>) -> Res<Pubkey> {
    if let Some(w) = v.filter(|w| !w.is_empty()) {
        return w.parse().map_err(|_| format!("not a wallet address: {w}"));
    }
    node.keypair.as_ref().map(|k| k.pubkey()).ok_or_else(|| "no wallet: sign in, or start the node with VINELAND_KEYPAIR".to_string())
}

fn config(node: &Node) -> Reply {
    let p = node.rpc.pool(&node.pool).ok();
    (
        200,
        json!({
            "cluster": node.cfg.rpc,
            "pool": node.cfg.pool,
            "mint": node.cfg.mint,
            "pool_program": v2::POOL_ID.to_string(),
            "verifier_program": v2::VERIFIER_ID.to_string(),
            "note_amount": p.as_ref().map(|p| p.note_amount),
            "fee_bps": p.as_ref().map(|p| p.fee_bps),
            "k_min": p.as_ref().map(|p| p.k_min),
            "keypair_wallet": node.keypair.as_ref().map(|k| k.pubkey().to_string()),
            "operator": node.operator.is_some(),
            "relayer": node.relayer_url,
        }),
    )
}

fn relayer_get(node: &Node, path: &str) -> Option<Value> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?
        .get(format!("{}{path}", node.relayer_url))
        .send()
        .ok()?
        .json()
        .ok()
}

fn relayer_post(node: &Node, path: &str, body: &Value, timeout: Duration) -> Res<Value> {
    let http = reqwest::blocking::Client::builder().timeout(timeout).build().map_err(|e| e.to_string())?;
    for _ in 0..120 {
        let resp = http.post(format!("{}{path}", node.relayer_url)).json(body).send().map_err(|e| format!("relayer unreachable: {e}"))?;
        let code = resp.status().as_u16();
        let v: Value = resp.json().unwrap_or(json!({}));
        match code {
            200 => return Ok(v),
            409 => std::thread::sleep(Duration::from_secs(5)),
            _ => return Err(v["error"].as_str().unwrap_or("relayer error").to_string()),
        }
    }
    Err("relayer stayed busy".into())
}

fn status(node: &Node, wallet: Option<&str>) -> Reply {
    let p = match node.rpc.pool(&node.pool) {
        Ok(p) => p,
        Err(e) => return refuse(502, e),
    };
    let t = node.rpc.tree_state(&node.pool).ok();
    let set_size = t.as_ref().and_then(|t| t.latest()).map(|(_, s)| s).unwrap_or(0);
    let mut out = json!({
        "pool": {
            "deposits": p.leaf_count,
            "in_tree": t.as_ref().map(|t| t.inserted),
            "live_notes": p.live_notes,
            "exits": p.exits,
            "privacy_set": set_size.saturating_sub(p.exits),
            "k_min": p.k_min,
            "note_amount": p.note_amount,
            "fee": p.fee(),
        },
        "relayer": relayer_get(node, "/health"),
    });
    if let Ok(w) = wallet_of(node, wallet) {
        let notes: Vec<StoredNote> = node.store.all().into_iter().filter(|n| n.wallet == w.to_string()).collect();
        let count = |s: &str| notes.iter().filter(|n| n.state == s).count();
        let inserted = t.as_ref().map(|t| t.inserted).unwrap_or(0);
        let deposits = node.rpc.deposits(&node.pool).unwrap_or_default();
        let ready = notes
            .iter()
            .filter(|n| n.state == "deposited")
            .filter(|n| deposits.iter().any(|d| d.commitment == n.note.commitment() && d.index < inserted))
            .count();
        out["wallet"] = json!({
            "address": w.to_string(),
            "approved": node.rpc.exists(&v2::allow_address(&node.pool, &w)).unwrap_or(false),
            "sol": node.rpc.client.get_balance(&w).unwrap_or(0) as f64 / 1e9,
            "test_dollars": node.rpc.token_balance(&v2::ata(&w, &node.mint)).ok().flatten().unwrap_or(0),
            "notes": { "ready": ready, "deposited": count("deposited"), "pending": count("pending"), "spent": count("spent"), "exited": count("exited") },
            "note_list": notes.iter().map(|n| json!({"id": n.id, "state": n.state, "created": n.created, "deposit_sig": n.deposit_sig})).collect::<Vec<_>>(),
        });
    }
    (200, out)
}

/// Demo operator: approve a wallet and give it SOL for fees and a few test
/// dollars. On a real deployment, approval follows KYC and nobody mints.
fn onboard(node: &Node, body: &Value) -> Reply {
    let Some(op) = node.operator.as_ref() else { return refuse(403, "this node has no operator key") };
    let w = match wallet_of(node, body["wallet"].as_str()) {
        Ok(w) => w,
        Err(e) => return refuse(400, e),
    };
    let mut ixs = vec![];
    if !node.rpc.exists(&v2::allow_address(&node.pool, &w)).unwrap_or(false) {
        ixs.push(v2::allow_ix(&node.pool, &op.pubkey(), &w));
    }
    ixs.push(system_instruction::transfer(&op.pubkey(), &w, GRANT_LAMPORTS));
    let token = v2::ata(&w, &node.mint);
    if !node.rpc.exists(&token).unwrap_or(false) {
        ixs.push(v2::create_ata_ix(&op.pubkey(), &w, &node.mint));
    }
    ixs.push(spl_token::instruction::mint_to(&spl_token::id(), &node.mint, &token, &op.pubkey(), &[], GRANT_NOTES * NOTE_AMOUNT).unwrap());
    match node.rpc.land(&ixs, op, &[]) {
        Ok((sig, _)) => (200, json!({ "signature": sig.to_string(), "approved": w.to_string(), "test_dollars": GRANT_NOTES * NOTE_AMOUNT })),
        Err(e) => refuse(502, v2::explain(e.lines().next().unwrap_or(&e))),
    }
}

fn unsigned_b64(ixs: &[solana_sdk::instruction::Instruction], payer: &Pubkey, node: &Node) -> Res<String> {
    let bh = node.rpc.client.get_latest_blockhash().map_err(|e| e.to_string())?;
    let mut tx = Transaction::new_unsigned(Message::new(ixs, Some(payer)));
    tx.message.recent_blockhash = bh;
    Ok(base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx).map_err(|e| e.to_string())?))
}

fn new_note(node: &Node, w: &Pubkey) -> Res<StoredNote> {
    let note = Note::new(&node.pool);
    let n = StoredNote { id: hex::encode(note.commitment()), wallet: w.to_string(), note, state: "pending".into(), deposit_sig: None, created: now() };
    node.store.put(&n)?;
    Ok(n)
}

/// Privy: one unsigned deposit per note, for the wallet to sign and send.
fn deposit_prepare(node: &Node, body: &Value) -> Reply {
    let w = match wallet_of(node, body["wallet"].as_str()) {
        Ok(w) => w,
        Err(e) => return refuse(400, e),
    };
    let count = body["count"].as_u64().unwrap_or(1).clamp(1, 10);
    let token = v2::ata(&w, &node.mint);
    let mut txs = vec![];
    for _ in 0..count {
        let n = match new_note(node, &w) {
            Ok(n) => n,
            Err(e) => return refuse(500, e),
        };
        let ix = v2::deposit_ix(&node.pool, &node.mint, &w, &token, n.note.commitment());
        match unsigned_b64(&[ix], &w, node) {
            Ok(tx) => txs.push(json!({ "note": n.id, "transaction": tx })),
            Err(e) => return refuse(502, e),
        }
    }
    (200, json!({ "transactions": txs }))
}

fn crank_async(node: &Arc<Node>) {
    let node = node.clone();
    std::thread::spawn(move || {
        let _ = relayer_post(&node, "/crank", &json!({ "pool": node.cfg.pool }), Duration::from_secs(600));
    });
}

fn deposit_confirm(node: &Arc<Node>, body: &Value) -> Reply {
    let (Some(id), Some(sig)) = (body["note"].as_str(), body["signature"].as_str()) else {
        return refuse(400, "needs note and signature");
    };
    let Some(mut n) = node.store.get(id) else { return refuse(404, "no such note") };
    let Ok(commitment) = hex::decode(id).map(|b| <[u8; 32]>::try_from(b).unwrap_or([0; 32])) else { return refuse(400, "bad note id") };
    // The record on chain is the proof the deposit landed, whatever the browser says.
    for _ in 0..20 {
        if let Ok(Some(_)) = node.rpc.deposit_record(&node.pool, &commitment) {
            n.state = "deposited".into();
            n.deposit_sig = Some(sig.to_string());
            if let Err(e) = node.store.put(&n) {
                return refuse(500, e);
            }
            crank_async(node);
            return (200, json!({ "note": id, "state": "deposited" }));
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    refuse(409, "the deposit is not on chain yet")
}

/// Keypair mode (agents, scripts): the node signs and sends the deposits.
fn deposit_keypair(node: &Arc<Node>, body: &Value) -> Reply {
    let Some(kp) = node.keypair.as_ref() else { return refuse(400, "no keypair: in the dashboard, deposits are signed by your wallet") };
    let count = body["count"].as_u64().unwrap_or(1).clamp(1, 10);
    let token = v2::ata(&kp.pubkey(), &node.mint);
    let mut out = vec![];
    for _ in 0..count {
        let mut n = match new_note(node, &kp.pubkey()) {
            Ok(n) => n,
            Err(e) => return refuse(500, e),
        };
        match node.rpc.land(&[v2::deposit_ix(&node.pool, &node.mint, &kp.pubkey(), &token, n.note.commitment())], kp, &[]) {
            Ok((sig, _)) => {
                n.state = "deposited".into();
                n.deposit_sig = Some(sig.to_string());
                let _ = node.store.put(&n);
                out.push(json!({ "note": n.id, "signature": sig.to_string() }));
            }
            Err(e) => return refuse(502, v2::explain(e.lines().next().unwrap_or(&e))),
        }
    }
    crank_async(node);
    (200, json!({ "deposits": out }))
}

fn pay(node: &Arc<Node>, body: &Value) -> Reply {
    let w = match wallet_of(node, body["wallet"].as_str()) {
        Ok(w) => w,
        Err(e) => return refuse(400, e),
    };
    let Some(to) = body["to"].as_str().and_then(|t| t.parse::<Pubkey>().ok()) else { return refuse(400, "`to` must be a wallet address") };
    let count = body["notes"].as_u64().unwrap_or(1).clamp(1, 10) as usize;
    let reference = body["reference"].as_str().unwrap_or("").chars().take(80).collect::<String>();
    let available: Vec<StoredNote> = node.store.all().into_iter().filter(|n| n.wallet == w.to_string() && n.state == "deposited").collect();
    if available.len() < count {
        return refuse(400, format!("only {} note(s) ready to pay", available.len()));
    }
    let id = node.next_job.fetch_add(1, Ordering::SeqCst);
    let job = Job { id, state: "running".into(), stage: "queued".into(), notes: count, ..Default::default() };
    node.jobs.lock().unwrap().insert(id, job);
    let node2 = node.clone();
    let notes: Vec<StoredNote> = available.into_iter().take(count).collect();
    std::thread::spawn(move || run_pay(&node2, id, w, to, notes, reference));
    (202, json!({ "job": id }))
}

fn set_job(node: &Node, id: u64, f: impl FnOnce(&mut Job)) {
    if let Some(j) = node.jobs.lock().unwrap().get_mut(&id) {
        f(j);
    }
}

fn run_pay(node: &Arc<Node>, id: u64, wallet: Pubkey, to: Pubkey, notes: Vec<StoredNote>, reference: String) {
    let recipient = v2::ata(&to, &node.mint);
    for (k, mut n) in notes.into_iter().enumerate() {
        let result = (|| -> Res<Receipt> {
            set_job(node, id, |j| j.stage = "preparing".into());
            let commitment = n.note.commitment();
            // The leaf must be in the on-chain tree before a proof can name its root.
            for attempt in 0..3 {
                let deposits = node.rpc.deposits(&node.pool)?;
                let rec = deposits.iter().find(|d| d.commitment == commitment).ok_or("this note's deposit is not on chain")?;
                let t = node.rpc.tree_state(&node.pool)?;
                if rec.index < t.inserted {
                    break;
                }
                if attempt == 2 {
                    return Err("the deposit is not folded into the tree yet; retry in a minute".into());
                }
                set_job(node, id, |j| j.stage = "folding the deposit into the tree".into());
                relayer_post(node, "/crank", &json!({ "pool": node.cfg.pool }), Duration::from_secs(600))?;
            }
            let deposits = node.rpc.deposits(&node.pool)?;
            let t = node.rpc.tree_state(&node.pool)?;
            let tree = tree_from_deposits(&deposits, t.inserted).ok_or("non-canonical leaf on chain")?;
            let index = deposits.iter().find(|d| d.commitment == commitment).map(|d| d.index).ok_or("deposit vanished")?;
            set_job(node, id, |j| j.stage = "proving on this machine".into());
            let bundle = prove_withdraw_bound(&n.note, &tree, index, &recipient);
            let mut req = serde_json::to_value(&bundle).map_err(|e| e.to_string())?;
            req["owner"] = json!(to.to_string());
            set_job(node, id, |j| j.stage = "sent to the relayer".into());
            // Mirror the relayer's progress while it drives the on-chain verification.
            let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let (w2, n2) = (watching.clone(), node.clone());
            let watcher = std::thread::spawn(move || {
                while w2.load(Ordering::SeqCst) {
                    if let Some(p) = relayer_get(&n2, "/progress") {
                        let stage = p["stage"].as_str().unwrap_or("").to_string();
                        let (done, total) = (p["done"].as_u64().unwrap_or(0) as usize, p["total"].as_u64().unwrap_or(0) as usize);
                        if p["busy"].as_bool() == Some(true) {
                            set_job(&n2, id, |j| {
                                j.stage = match stage.as_str() {
                                    "uploading" => "uploading the proof".into(),
                                    "verifying" => "Solana is verifying the proof".into(),
                                    "paid" => "paid".into(),
                                    s => s.to_string(),
                                };
                                j.done = done;
                                j.total = total;
                            });
                        }
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
            let out = relayer_post(node, "/withdraw", &req, Duration::from_secs(3600));
            watching.store(false, Ordering::SeqCst);
            let _ = watcher.join();
            let out = out?;
            Ok(Receipt {
                time: now(),
                wallet: wallet.to_string(),
                reference: reference.clone(),
                note: n.id.clone(),
                nullifier: hex::encode(bundle.nullifier_bytes()),
                to: to.to_string(),
                recipient_account: recipient.to_string(),
                paid: out["paid"].as_u64().unwrap_or(0),
                fee: out["fee"].as_u64().unwrap_or(0),
                withdraw: out["signatures"]["withdraw"].as_str().unwrap_or("").to_string(),
            })
        })();
        match result {
            Ok(r) => {
                n.state = "spent".into();
                let _ = node.store.put(&n);
                let _ = node.store.add_receipt(r.clone());
                set_job(node, id, |j| {
                    j.paid_notes = k + 1;
                    j.receipts.push(r);
                });
            }
            Err(e) => {
                set_job(node, id, |j| {
                    j.state = "failed".into();
                    j.error = Some(e);
                });
                return;
            }
        }
    }
    set_job(node, id, |j| {
        j.state = "paid".into();
        j.stage = "paid".into();
    });
}

fn job(node: &Node, id: &str) -> Reply {
    let Ok(id) = id.parse::<u64>() else { return refuse(400, "bad job id") };
    match node.jobs.lock().unwrap().get(&id) {
        Some(j) => (200, serde_json::to_value(j).unwrap()),
        None => refuse(404, "no such job"),
    }
}

fn receipts_for(node: &Node, wallet: Option<&str>) -> Vec<Receipt> {
    let w = wallet_of(node, wallet).ok().map(|w| w.to_string());
    node.store.receipts().into_iter().filter(|r| w.as_deref().map_or(true, |w| r.wallet == w)).collect()
}

fn receipts(node: &Node, wallet: Option<&str>) -> Reply {
    (200, json!({ "receipts": receipts_for(node, wallet), "cluster": node.cfg.rpc }))
}

fn receipts_csv(node: &Node, wallet: Option<&str>) -> String {
    let mut s = String::from("time_utc,reference,recipient_wallet,recipient_token_account,paid_base_units,fee_base_units,note_commitment,nullifier,withdraw_signature,explorer\n");
    for r in receipts_for(node, wallet) {
        let clean = |x: &str| x.replace([',', '"', '\n'], " ");
        s.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},https://explorer.solana.com/tx/{}?cluster=devnet\n",
            r.time,
            clean(&r.reference),
            r.to,
            r.recipient_account,
            r.paid,
            r.fee,
            r.note,
            r.nullifier,
            r.withdraw,
            r.withdraw
        ));
    }
    s
}

/// The public exit: reveals the note's secret, refunds the depositor in full.
fn exit_prepare(node: &Node, body: &Value) -> Reply {
    let Some(n) = body["note"].as_str().and_then(|id| node.store.get(id)) else { return refuse(404, "no such note") };
    if n.state != "deposited" {
        return refuse(400, format!("note is {}", n.state));
    }
    let Ok(w) = n.wallet.parse::<Pubkey>() else { return refuse(500, "bad wallet in note") };
    let ix = v2::exit_ix(&node.pool, &node.mint, n.note.commitment(), enc(&n.note.secret), enc(&n.note.nullifier()), &v2::ata(&w, &node.mint), &w);
    if let Some(kp) = node.keypair.as_ref().filter(|k| k.pubkey() == w) {
        return match node.rpc.land(&[ix], kp, &[]) {
            Ok((sig, _)) => {
                let mut n = n;
                n.state = "exited".into();
                let _ = node.store.put(&n);
                (200, json!({ "signature": sig.to_string(), "state": "exited" }))
            }
            Err(e) => refuse(502, v2::explain(e.lines().next().unwrap_or(&e))),
        };
    }
    match unsigned_b64(&[ix], &w, node) {
        Ok(tx) => (200, json!({ "note": n.id, "transaction": tx })),
        Err(e) => refuse(502, e),
    }
}

fn exit_confirm(node: &Node, body: &Value) -> Reply {
    let Some(mut n) = body["note"].as_str().and_then(|id| node.store.get(id)) else { return refuse(404, "no such note") };
    let Some(sig) = body["signature"].as_str().and_then(|s| s.parse::<Signature>().ok()) else { return refuse(400, "bad signature") };
    for _ in 0..20 {
        if let Ok(Some(Ok(()))) = node.rpc.client.get_signature_status(&sig) {
            n.state = "exited".into();
            let _ = node.store.put(&n);
            return (200, json!({ "note": n.id, "state": "exited" }));
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    refuse(409, "the exit is not confirmed yet")
}
