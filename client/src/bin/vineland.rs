//! `vineland`: the committee-free pool from a terminal.
//!
//!   vineland operator init  --mint M --note N --fee-bps B --k-min K [--treasury T] [--keypair OP]
//!   vineland operator allow --pool P --member W [--keypair OP]
//!   vineland note new       --pool P --out note.json
//!   vineland deposit        --note note.json --keypair member.json [--relayer URL]
//!   vineland withdraw       --note note.json --to WALLET --relayer URL
//!   vineland exit           --note note.json --payer K
//!   vineland status         --pool P
//!
//! Every command takes `--url` (default: $VINELAND_RPC, else devnet) and
//! refuses mainnet: the programs are not audited. The operator keypair
//! defaults to the Solana CLI wallet (~/.config/solana/id.json).
//!
//! The note file is the money: it holds the secret that withdraws the note.
//! It is written once with 0600 permissions and never leaves this machine.
//! `withdraw` proves locally and sends the relayer only the recipient-bound
//! proof bundle (no secret, and no member key is used). `exit` is the public
//! fallback: it reveals the secret on chain and refunds the depositor.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature, Signer},
};
use vineland_client::v2::{self, Rpc};
use vineland_client::{enc, prove_withdraw_bound, tree_from_deposits, Note, M31_P};

const USAGE: &str = "usage:
  vineland operator init  --mint M --note N --fee-bps B --k-min K [--treasury T] [--keypair OP]
  vineland operator allow --pool P --member W [--keypair OP]
  vineland note new       --pool P --out note.json
  vineland deposit        --note note.json --keypair member.json [--relayer URL]
  vineland withdraw       --note note.json --to WALLET --relayer URL
  vineland exit           --note note.json --payer K
  vineland status         --pool P
every command also takes --url RPC (default $VINELAND_RPC, else devnet)";

type Res<T> = Result<T, String>;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&argv) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// `--name value` pairs after the command words; only `allowed` names, each once.
struct Flags(HashMap<String, String>);

impl Flags {
    fn parse(args: &[String], allowed: &[&str]) -> Res<Self> {
        let mut map = HashMap::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let name = a.strip_prefix("--").ok_or_else(|| format!("unexpected argument {a:?}\n{USAGE}"))?;
            if name != "url" && !allowed.contains(&name) {
                return Err(format!("unknown flag --{name}\n{USAGE}"));
            }
            let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
            if map.insert(name.to_string(), value.clone()).is_some() {
                return Err(format!("--{name} given twice"));
            }
        }
        Ok(Flags(map))
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn req(&self, name: &str) -> Res<&str> {
        self.get(name).ok_or_else(|| format!("missing --{name}\n{USAGE}"))
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> Res<T> {
        self.req(name)?.parse().map_err(|_| format!("--{name}: not a valid number"))
    }

    fn key(&self, name: &str) -> Res<Pubkey> {
        self.req(name)?.parse().map_err(|_| format!("--{name}: not a public key"))
    }

    /// Connect to `--url`, else $VINELAND_RPC, else devnet; refuse mainnet.
    fn rpc(&self) -> Res<Rpc> {
        let url = match self.get("url") {
            Some(u) => u.to_string(),
            None => std::env::var("VINELAND_RPC").unwrap_or_else(|_| v2::DEVNET_URL.to_string()),
        };
        let rpc = Rpc::new(&url);
        rpc.refuse_mainnet()?;
        Ok(rpc)
    }

    fn keypair(&self, name: &str, default_cli_wallet: bool) -> Res<Keypair> {
        let path = match (self.get(name), default_cli_wallet) {
            (Some(p), _) => p.to_string(),
            (None, true) => format!("{}/.config/solana/id.json", std::env::var("HOME").map_err(|_| "HOME is not set")?),
            (None, false) => return Err(format!("missing --{name}\n{USAGE}")),
        };
        read_keypair_file(&path).map_err(|e| format!("cannot read keypair {path}: {e}"))
    }
}

fn run(argv: &[String]) -> Res<()> {
    let words: Vec<&str> = argv.iter().take_while(|a| !a.starts_with("--")).map(String::as_str).collect();
    let rest = &argv[words.len()..];
    match words.as_slice() {
        ["operator", "init"] => operator_init(&Flags::parse(rest, &["mint", "note", "fee-bps", "k-min", "treasury", "keypair"])?),
        ["operator", "allow"] => operator_allow(&Flags::parse(rest, &["pool", "member", "keypair"])?),
        ["note", "new"] => note_new(&Flags::parse(rest, &["pool", "out"])?),
        ["deposit"] => deposit(&Flags::parse(rest, &["note", "keypair", "relayer"])?),
        ["withdraw"] => withdraw(&Flags::parse(rest, &["note", "to", "relayer"])?),
        ["exit"] => exit(&Flags::parse(rest, &["note", "payer"])?),
        ["status"] => status(&Flags::parse(rest, &["pool"])?),
        _ => Err(USAGE.to_string()),
    }
}

/// Send, or fail with the program error named.
fn land(rpc: &Rpc, what: &str, ixs: &[solana_sdk::instruction::Instruction], payer: &Keypair, extra: &[&Keypair]) -> Res<Signature> {
    rpc.land(ixs, payer, extra).map(|(sig, _)| sig).map_err(|e| format!("{what}: {}", v2::explain(e.lines().next().unwrap_or(&e))))
}

// ---- operator -----------------------------------------------------------------------

fn operator_init(f: &Flags) -> Res<()> {
    let rpc = f.rpc()?;
    let op = f.keypair("keypair", true)?;
    let mint = f.key("mint")?;
    let note: u64 = f.num("note")?;
    let fee_bps: u16 = f.num("fee-bps")?;
    let k_min: u32 = f.num("k-min")?;
    let pool = v2::pool_address(&op.pubkey(), &mint);
    if rpc.exists(&pool)? {
        return Err(format!("this operator already has a pool for this mint: {pool}"));
    }
    match rpc.account(&mint)? {
        Some(a) if a.owner == spl_token::id() => {}
        _ => return Err(format!("{mint} is not an SPL Token mint on this cluster")),
    }
    let mut ixs = vec![];
    let treasury = match f.get("treasury") {
        Some(t) => t.parse().map_err(|_| "--treasury: not a public key".to_string())?,
        None => {
            ixs.push(v2::create_ata_ix(&op.pubkey(), &op.pubkey(), &mint));
            v2::ata(&op.pubkey(), &mint)
        }
    };
    ixs.push(v2::initialize_ix(&op.pubkey(), &mint, &treasury, note, fee_bps, k_min));
    let sig = land(&rpc, "initialize", &ixs, &op, &[])?;
    println!("pool: {pool}");
    println!("operator: {}", op.pubkey());
    println!("mint: {mint}");
    println!("treasury: {treasury}");
    println!("note: {note}  fee: {fee_bps} bps  k_min: {k_min}");
    println!("signature: {sig}");
    Ok(())
}

fn operator_allow(f: &Flags) -> Res<()> {
    let rpc = f.rpc()?;
    let op = f.keypair("keypair", true)?;
    let pool = f.key("pool")?;
    let member = f.key("member")?;
    let p = rpc.pool(&pool)?;
    if p.authority != op.pubkey() {
        return Err(format!("{} is not this pool's operator ({})", op.pubkey(), p.authority));
    }
    if rpc.exists(&v2::allow_address(&pool, &member))? {
        return Err(format!("{member} is already allowed"));
    }
    let sig = land(&rpc, "allow", &[v2::allow_ix(&pool, &op.pubkey(), &member)], &op, &[])?;
    println!("allowed: {member}");
    println!("signature: {sig}");
    Ok(())
}

// ---- notes ---------------------------------------------------------------------------

fn note_new(f: &Flags) -> Res<()> {
    let pool = f.key("pool")?;
    let out = f.req("out")?;
    let note = Note::new(&pool);
    let body = serde_json::to_string_pretty(&note).map_err(|e| e.to_string())?;
    // create_new: never overwrite a note, an overwritten note is lost money.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| format!("cannot create {out}: {e} (an existing note file is never overwritten)"))?;
    file.write_all(body.as_bytes()).and_then(|_| file.sync_all()).map_err(|e| format!("write {out}: {e}"))?;
    eprintln!(
        "WARNING: {out} IS THE MONEY. It holds the note's secret: whoever reads it can withdraw the note,\n\
         and if you lose it you can neither withdraw nor exit. Back it up offline, never share it.\n\
         The relayer never needs it."
    );
    println!("note: {out}");
    println!("pool: {pool}");
    println!("commitment: {}", hex::encode(note.commitment()));
    Ok(())
}

/// Read a note file, refusing a malformed one (a non-canonical limb would make
/// a secret no program can open). Warns if others can read it.
fn load_note(path: &str) -> Res<Note> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        eprintln!("warning: {path} is readable by other users; chmod 600 it");
    }
    let raw = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let note: Note = serde_json::from_slice(&raw).map_err(|_| format!("{path} is not a Vineland note file"))?;
    if note.pool.parse::<Pubkey>().is_err() || note.secret.iter().any(|v| *v >= M31_P) {
        return Err(format!("{path} is not a valid Vineland note file"));
    }
    Ok(note)
}

// ---- relayer client ---------------------------------------------------------------------

/// POST JSON to the relayer, retrying while it is busy (409) for up to ~10 min.
fn post(relayer: &str, path: &str, body: &Value) -> Res<Value> {
    let url = format!("{}{path}", relayer.trim_end_matches('/'));
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|e| e.to_string())?;
    for _ in 0..120 {
        let resp = http.post(&url).json(body).send().map_err(|e| format!("{url}: {e}"))?;
        let code = resp.status().as_u16();
        let text = resp.text().map_err(|e| format!("{url}: {e}"))?;
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "error": text }));
        match code {
            200 => return Ok(v),
            409 => {
                eprintln!("relayer busy; retrying in 5 s");
                std::thread::sleep(Duration::from_secs(5));
            }
            _ => return Err(format!("relayer answered {code}: {}", serde_json::to_string_pretty(&v).unwrap_or(text))),
        }
    }
    Err("relayer stayed busy for 10 minutes".into())
}

// ---- member --------------------------------------------------------------------------

fn deposit(f: &Flags) -> Res<()> {
    let note = load_note(f.req("note")?)?;
    let rpc = f.rpc()?;
    let member = f.keypair("keypair", false)?;
    let pool = note.pool();
    let p = rpc.pool(&pool)?;
    let commitment = note.commitment();
    if rpc.deposit_record(&pool, &commitment)?.is_some() {
        return Err("this note is already deposited".into());
    }
    if !rpc.exists(&v2::allow_address(&pool, &member.pubkey()))? {
        return Err(format!("{} is not on this pool's allowlist", member.pubkey()));
    }
    let from = v2::ata(&member.pubkey(), &p.mint);
    match rpc.token_balance(&from)? {
        Some(b) if b >= p.note_amount => {}
        Some(b) => return Err(format!("{from} holds {b}, a note is {}", p.note_amount)),
        None => return Err(format!("{} has no token account for {} (expected {from})", member.pubkey(), p.mint)),
    }
    let sig = land(&rpc, "deposit", &[v2::deposit_ix(&pool, &p.mint, &member.pubkey(), &from, commitment)], &member, &[])?;
    let index = rpc.deposit_record(&pool, &commitment)?.map(|r| r.index);
    println!("deposit: {sig}");
    println!("leaf index: {}", index.map_or("?".into(), |i| i.to_string()));

    // Fold it into the on-chain tree, so it can be withdrawn.
    match f.get("relayer") {
        Some(relayer) => {
            let r = post(relayer, "/crank", &json!({ "pool": pool.to_string() }))?;
            println!("folded by the relayer: {} insert transactions, inserted {} of {}", r["signatures"].as_array().map_or(0, Vec::len), r["inserted"], r["leaf_count"]);
        }
        None => {
            let mut sigs = vec![];
            rpc.crank(&pool, &member, &mut sigs).map_err(|e| format!("fold: {e} ({} insert transactions landed)", sigs.len()))?;
            let t = rpc.tree_state(&pool)?;
            println!("folded: {} insert transactions, inserted {}", sigs.len(), t.inserted);
        }
    }
    Ok(())
}

fn withdraw(f: &Flags) -> Res<()> {
    let note = load_note(f.req("note")?)?;
    let wallet = f.key("to")?;
    let relayer = f.req("relayer")?;
    let rpc = f.rpc()?;
    let pool = note.pool();
    let p = rpc.pool(&pool)?;
    let commitment = note.commitment();
    let nullifier = enc(&note.nullifier());
    if rpc.exists(&v2::nullifier_address(&pool, &nullifier))? {
        return Err("this note is already spent (withdrawn or exited)".into());
    }
    let deposits = rpc.deposits(&pool)?;
    let index = deposits.iter().find(|d| d.commitment == commitment).map(|d| d.index).ok_or_else(|| format!("this note was never deposited in {pool}"))?;
    let mut t = rpc.tree_state(&pool)?;
    if index >= t.inserted {
        eprintln!("leaf {index} is not folded into the tree yet; asking the relayer to fold");
        post(relayer, "/crank", &json!({ "pool": pool.to_string() }))?;
        t = rpc.tree_state(&pool)?;
        if index >= t.inserted {
            return Err(format!("leaf {index} is still not in the tree (inserted {})", t.inserted));
        }
    }
    let (root, size) = t.latest().ok_or("the pool has no root yet")?;
    let deposits = rpc.deposits(&pool)?;
    let tree = tree_from_deposits(&deposits, size).ok_or("a deposit record holds a non-canonical commitment")?;
    if enc(&tree.root()) != root {
        return Err("the tree rebuilt from the deposit records does not match the pool's latest root; refusing to prove".into());
    }
    if size.saturating_sub(p.exits) < p.k_min {
        return Err(format!(
            "anonymity set too small: {size} leaves under the latest root, {} exited, pool floor k_min = {}; the pool would refuse. Wait for more deposits.",
            p.exits, p.k_min
        ));
    }
    let recipient = v2::ata(&wallet, &p.mint);
    eprintln!("proving locally: leaf {index} of {size}, bound to {recipient} (the token account of {wallet})");
    let started = Instant::now();
    let bundle = prove_withdraw_bound(&note, &tree, index, &recipient);
    eprintln!("proved in {} ms; sending the bundle to {relayer}", started.elapsed().as_millis());

    // Only the bound bundle goes over the wire (no secret), plus the wallet so
    // the relayer may create its token account; it cannot change the recipient.
    let mut body = serde_json::to_value(&bundle).map_err(|e| e.to_string())?;
    body["owner"] = json!(wallet.to_string());
    let r = post(relayer, "/withdraw", &body)?;
    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    Ok(())
}

fn exit(f: &Flags) -> Res<()> {
    let note = load_note(f.req("note")?)?;
    let rpc = f.rpc()?;
    let payer = f.keypair("payer", false)?;
    let pool = note.pool();
    let p = rpc.pool(&pool)?;
    let commitment = note.commitment();
    let record = rpc.deposit_record(&pool, &commitment)?.ok_or_else(|| format!("this note was never deposited in {pool}"))?;
    if record.exited {
        return Err("this note already exited".into());
    }
    let nullifier = enc(&note.nullifier());
    if rpc.exists(&v2::nullifier_address(&pool, &nullifier))? {
        return Err("this note is already spent (withdrawn)".into());
    }
    let to = v2::ata(&record.depositor, &p.mint);
    let mut ixs = vec![];
    if !rpc.exists(&to)? {
        ixs.push(v2::create_ata_ix(&payer.pubkey(), &record.depositor, &p.mint));
    }
    ixs.push(v2::exit_ix(&pool, &p.mint, commitment, enc(&note.secret), nullifier, &to, &payer.pubkey()));
    eprintln!("exit is public: it reveals this note's secret on chain and refunds the depositor {} in full", record.depositor);
    let sig = land(&rpc, "exit", &ixs, &payer, &[])?;
    println!("exit: {sig}");
    println!("refunded to: {to} (owner {})", record.depositor);
    println!("balance: {}", rpc.token_balance(&to)?.unwrap_or(0));
    Ok(())
}

fn status(f: &Flags) -> Res<()> {
    let rpc = f.rpc()?;
    let pool = f.key("pool")?;
    let p = rpc.pool(&pool)?;
    let t = rpc.tree_state(&pool)?;
    let vault = rpc.token_balance(&v2::vault_address(&pool))?.unwrap_or(0);
    println!("pool: {pool}");
    println!("operator: {}", p.authority);
    println!("mint: {}", p.mint);
    println!("treasury: {}", p.treasury);
    println!("note: {}  fee: {} bps  k_min: {}", p.note_amount, p.fee_bps, p.k_min);
    println!("leaf count: {}", p.leaf_count);
    println!("inserted: {}{}", t.inserted, if t.level > 0 { format!(" (fold of leaf {} at level {})", t.inserted, t.level) } else { String::new() });
    println!("live notes: {}", p.live_notes);
    println!("exits: {}", p.exits);
    println!("vault: {vault}");
    println!("recent roots (newest first):");
    for (root, size) in t.recent_roots() {
        println!("  {}  covers {size}", hex::encode(root));
    }
    Ok(())
}
