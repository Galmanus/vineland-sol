import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePrivy } from "@privy-io/react-auth";
import { useCreateWallet, useSignTransaction, useWallets } from "@privy-io/react-auth/solana";
import { agentsApi, api, b64ToBytes, explorer, sendRaw, short, setAuthProvider, HOSTED } from "./api.js";

const DEVNET_RPC = "https://api.devnet.solana.com";
// vineland.app/app/authorize?user_code=...: the page an AI agent sends its owner to.
const AUTHORIZE = /\/authorize\.?\/?$/.test(window.location.pathname);
const USER_CODE = new URLSearchParams(window.location.search).get("user_code") || "";
const units = (n) => (n / 1_000_000).toFixed(2);

// ---- shells: who signs ----------------------------------------------------------------

export default function App({ privy }) {
  return privy ? <PrivyShell /> : <KeypairShell />;
}

function PrivyShell() {
  const { ready, authenticated, login, logout, user, getAccessToken } = usePrivy();
  setAuthProvider(getAccessToken);
  const { wallets, ready: walletsReady } = useWallets();
  const { createWallet } = useCreateWallet();
  const { signTransaction } = useSignTransaction();
  const [walletErr, setWalletErr] = useState("");
  const creating = useRef(false);
  const wallet = wallets?.[0];
  // The embedded wallet belongs to the account, not the site: a user who already
  // has one (created on another origin) gets it from linkedAccounts.
  const linked = user?.linkedAccounts?.find((a) => a.type === "wallet" && a.chainType === "solana");
  const address = wallet?.address || linked?.address;

  useEffect(() => {
    if (!authenticated || !walletsReady || wallet || linked || creating.current) return;
    creating.current = true;
    createWallet().catch((e) => setWalletErr(e?.message || "could not create the wallet"));
  }, [authenticated, walletsReady, wallet, linked, createWallet]);

  const signer = useMemo(
    () => ({
      kind: "privy",
      async signAndSend(txB64) {
        if (!wallet) throw new Error("Your wallet is still connecting. Wait a few seconds and try again.");
        const { signedTransaction } = await signTransaction({ transaction: b64ToBytes(txB64), wallet, chain: "solana:devnet" });
        return sendRaw(DEVNET_RPC, signedTransaction);
      },
    }),
    [wallet, signTransaction],
  );
  if (!ready) return <Splash text="Loading" />;
  if (!authenticated) return <Landing onLogin={login} authorize={AUTHORIZE} />;
  if (AUTHORIZE) return <Authorize code={USER_CODE} />;
  if (walletErr) return <Splash text={`Wallet error: ${walletErr}. Sign out and in again.`} />;
  if (!address) return <Splash text={walletsReady ? "Creating your Solana wallet" : "Loading your wallet"} />;
  return <Dashboard address={address} signer={signer} who={user?.email?.address} onLogout={logout} />;
}

function KeypairShell() {
  const [cfg, setCfg] = useState(null);
  useEffect(() => {
    api.config().then(setCfg).catch(() => setCfg({}));
  }, []);
  if (!cfg) return <Splash text="Loading" />;
  if (!cfg.keypair_wallet) return <Splash text="Start vineland-node with VINELAND_KEYPAIR, or build the dashboard with a Privy app id" />;
  return <Dashboard address={cfg.keypair_wallet} signer={{ kind: "keypair" }} />;
}

// ---- layout ------------------------------------------------------------------------------

function Mark() {
  return (
    <svg width="28" height="28" viewBox="0 0 512 512" aria-hidden="true">
      <rect width="512" height="512" rx="110" fill="#111" />
      <path d="M96 128h92l68 196 68-196h92L306 400h-100z" fill="#EFE8DD" />
      <circle cx="416" cy="404" r="22" fill="#F5B83E" />
    </svg>
  );
}

function Splash({ text }) {
  return (
    <div className="splash">
      <Mark />
      <p>{text}</p>
    </div>
  );
}

function Landing({ onLogin, authorize }) {
  return (
    <div className="landing">
      {authorize && <div className="banner">An AI agent is asking for access to your Vineland account. Sign in to review the request.</div>}
      <header className="topbar">
        <div className="brand">
          <Mark /> <span>Vineland</span>
        </div>
        <div className="pills">
          <span className="pill gold">post-quantum</span>
          <span className="pill">Solana devnet</span>
        </div>
      </header>
      <main className="hero">
        <p className="eyebrow">The privacy layer for stablecoin payments</p>
        <h1>
          Pay suppliers, payroll and services in stablecoins
          <br />
          <em>without publishing who you pay.</em>
        </h1>
        <p className="lede">
          On a public ledger every payment shows who paid whom. With Vineland the payment stays public and auditable, and the link back
          to you does not. For companies and for AI agents.
        </p>
        <p className="kyc-line">Every wallet passes KYC before it can deposit. Compliance at the door, privacy on the way out.</p>
        <button className="btn primary big" onClick={onLogin}>
          Sign in with email
        </button>
        <ul className="points">
          <li>
            <b>Verified by Solana itself.</b> No committee, nobody can approve a payout.
          </li>
          <li>
            <b>Post-quantum.</b> Built only from hashes, so the privacy does not expire.
          </li>
          <li>
            {HOSTED ? (
              <>
                <b>Try it in your browser.</b> Devnet test dollars. In this hosted demo the proof is made on Vineland's server; in-browser proving is next.
              </>
            ) : (
              <>
                <b>Your secret stays on this machine.</b> The proof is made here.
              </>
            )}
          </li>
        </ul>
      </main>
    </div>
  );
}

function Authorize({ code: initial }) {
  const [code, setCode] = useState(initial);
  const [state, setState] = useState("");
  const [err, setErr] = useState("");
  const act = async (action) => {
    setErr("");
    try {
      await agentsApi.verify(code.trim(), action);
      setState(action === "approve" ? "approved" : "denied");
    } catch (e) {
      setErr(e.message);
    }
  };
  return (
    <div className="landing">
      <header className="topbar">
        <div className="brand">
          <Mark /> <span>Vineland</span>
        </div>
        <div className="pills">
          <span className="pill gold">post-quantum</span>
          <span className="pill">Solana devnet</span>
        </div>
      </header>
      <main className="hero narrow">
        <p className="eyebrow">Agent access</p>
        <h1 className="h1-sm">An AI agent wants to pay from your Vineland account.</h1>
        {state === "approved" ? (
          <p className="lede">
            Approved. The agent can now make private payouts from your notes. You can revoke it any time from the Agents page.{" "}
            <a href="./">Open the dashboard →</a>
          </p>
        ) : state === "denied" ? (
          <p className="lede">
            Denied. The agent got nothing. <a href="./">Open the dashboard →</a>
          </p>
        ) : (
          <>
            <ul className="points single">
              <li>
                <b>It can</b> make private payouts from this account's notes through the Vineland API.
              </li>
              <li>
                <b>It never receives a key.</b> It gets a 15-minute token that renews for up to 30 days, only while you allow it.
              </li>
              <li>
                <b>You can revoke it</b> any time from the Agents page; renewals stop at once.
              </li>
            </ul>
            <label>
              Code shown by the agent
              <input value={code} onChange={(e) => setCode(e.target.value.toUpperCase())} placeholder="XXXXX-XXXXX" />
            </label>
            <div className="row">
              <button className="btn primary big-inline" disabled={!code.trim()} onClick={() => act("approve")}>
                Approve this agent
              </button>
              <button className="btn" onClick={() => act("deny")}>
                Deny
              </button>
            </div>
            {err && <div className="alert">{err}</div>}
          </>
        )}
      </main>
    </div>
  );
}

const PAGES = ["Overview", "Pay", "Top up", "Receipts", "Agents", "Settings"];

function Dashboard({ address, signer, who, onLogout }) {
  const [page, setPage] = useState("Overview");
  const [cfg, setCfg] = useState(null);
  const [st, setSt] = useState(null);
  const [err, setErr] = useState("");
  const refresh = useCallback(() => {
    api.status(address).then(setSt).catch((e) => setErr(e.message));
  }, [address]);
  useEffect(() => {
    api.config().then(setCfg).catch((e) => setErr(e.message));
    refresh();
    const t = setInterval(refresh, 8000);
    return () => clearInterval(t);
  }, [refresh]);

  return (
    <div className="app">
      <aside className="side">
        <div className="brand">
          <Mark /> <span>Vineland</span>
        </div>
        <nav>
          {PAGES.map((p) => (
            <button key={p} className={p === page ? "nav on" : "nav"} onClick={() => setPage(p)}>
              {p}
            </button>
          ))}
        </nav>
        <div className="side-foot">
          <span className="pill gold">post-quantum</span>
          <span className="pill">devnet</span>
        </div>
      </aside>
      <section className="main">
        <header className="bar">
          <h2>{page}</h2>
          <div className="who">
            <span className="mono">{short(address, 5)}</span>
            {who && <span className="muted">{who}</span>}
            {onLogout && (
              <button className="btn ghost" onClick={onLogout}>
                Sign out
              </button>
            )}
          </div>
        </header>
        {err && (
          <div className="alert" onClick={() => setErr("")}>
            {err}
          </div>
        )}
        {page === "Overview" && <Overview st={st} cfg={cfg} address={address} refresh={refresh} setErr={setErr} go={setPage} />}
        {page === "Pay" && <Pay st={st} address={address} refresh={refresh} />}
        {page === "Top up" && <TopUp st={st} address={address} signer={signer} refresh={refresh} />}
        {page === "Receipts" && <Receipts address={address} />}
        {page === "Agents" && <Agents cfg={cfg} />}
        {page === "Settings" && <Settings cfg={cfg} st={st} />}
      </section>
    </div>
  );
}

// ---- pages ---------------------------------------------------------------------------

function Stat({ label, value, sub }) {
  return (
    <div className="stat">
      <div className="label">{label}</div>
      <div className="value">{value}</div>
      {sub && <div className="sub">{sub}</div>}
    </div>
  );
}

function Overview({ st, cfg, address, refresh, setErr, go }) {
  const [busy, setBusy] = useState(false);
  const w = st?.wallet;
  const p = st?.pool;
  const note = p?.note_amount ?? 1_000_000;
  const ready = w?.notes?.ready ?? 0;
  const approve = async () => {
    setBusy(true);
    try {
      await api.onboard(address);
      refresh();
    } catch (e) {
      setErr(e.message);
    }
    setBusy(false);
  };
  return (
    <div className="grid">
      <div className="card wide intro">
        <p>
          Your payments leave from the Vineland vault, not from your wallet. Anyone can audit the amount and the time; nobody can link the
          payment back to you.
        </p>
      </div>
      <Stat label="Ready to pay" value={`${units(ready * note)}`} sub={`${ready} note${ready === 1 ? "" : "s"} of ${units(note)} test dollars`} />
      <Stat label="Privacy set" value={p?.privacy_set ?? "…"} sub={`approved deposits your payment hides among (floor ${p?.k_min ?? 3})`} />
      <Stat label="In your wallet" value={units(w?.test_dollars ?? 0)} sub={`test dollars · ${(w?.sol ?? 0).toFixed(3)} SOL for fees`} />
      <div className="card">
        <div className="label">Identity check (KYC)</div>
        {w?.approved ? (
          <>
            <div className="ok">KYC verified · approved to deposit</div>
            <p className="muted small">Your wallet is on the on-chain allowlist. Only verified wallets can put money in.</p>
          </>
        ) : (
          <>
            <div className="warn">KYC required before you can deposit</div>
            <p className="muted small">
              Vineland only admits verified wallets: that is what makes it usable by regulated businesses. In production an identity provider runs
              the check; on this devnet demo the operator approves you instantly.
            </p>
            {cfg?.operator && (
              <button className="btn primary" disabled={busy} onClick={approve}>
                {busy ? "Verifying…" : "Complete KYC (devnet demo)"}
              </button>
            )}
          </>
        )}
      </div>
      <div className="card wide">
        <div className="row between">
          <div className="label">Next step</div>
        </div>
        {!w?.approved ? (
          <p>Complete the identity check (KYC), then top up.</p>
        ) : ready === 0 && (w?.notes?.deposited ?? 0) === 0 ? (
          <p>
            Top up: deposit test dollars as private notes. <button className="link" onClick={() => go("Top up")}>Top up →</button>
          </p>
        ) : ready === 0 ? (
          <p>Your deposit is being folded into the on-chain tree. This takes a minute.</p>
        ) : (
          <p>
            You can pay privately now. <button className="link" onClick={() => go("Pay")}>Pay →</button>
          </p>
        )}
      </div>
    </div>
  );
}

function Pay({ st, address, refresh }) {
  const [to, setTo] = useState("");
  const [reference, setReference] = useState("");
  const [job, setJob] = useState(null);
  const [err, setErr] = useState("");
  const ready = st?.wallet?.notes?.ready ?? 0;
  const note = st?.pool?.note_amount ?? 1_000_000;
  const fee = st?.pool?.fee ?? 19_700;

  useEffect(() => {
    if (!job || job.state !== "running") return;
    const t = setInterval(async () => {
      try {
        const j = await api.job(job.id);
        setJob(j);
        if (j.state !== "running") refresh();
      } catch {}
    }, 2000);
    return () => clearInterval(t);
  }, [job, refresh]);

  const submit = async (e) => {
    e.preventDefault();
    setErr("");
    try {
      const { job: id } = await api.pay(address, to.trim(), 1, reference.trim());
      setJob({ id, state: "running", stage: "starting" });
    } catch (e) {
      setErr(e.message);
    }
  };

  const pct = job?.total ? Math.min(100, Math.round((100 * job.done) / job.total)) : job?.state === "paid" ? 100 : 4;
  const receipt = job?.receipts?.[0];
  return (
    <div className="grid">
      <form className="card wide" onSubmit={submit}>
        <label>
          Pay to (recipient wallet)
          <input value={to} onChange={(e) => setTo(e.target.value)} placeholder="Solana address of your supplier or service" required />
        </label>
        <label>
          Reference
          <input value={reference} onChange={(e) => setReference(e.target.value)} placeholder="Invoice number, for your records only" />
        </label>
        <div className="row between">
          <div className="muted">
            Amount: one note, {units(note)} test dollars · recipient gets {units(note - fee)} · fee {units(fee)} (1.97%)
          </div>
          <button className="btn primary" disabled={ready < 1 || job?.state === "running"}>
            {ready < 1 ? "No notes yet: top up first" : "Pay privately"}
          </button>
        </div>
        {err && <div className="alert">{err}</div>}
      </form>
      {job && (
        <div className="card wide">
          <div className="steps">
            <Step on={job.stage !== "starting"} done={!!job.total || job.state === "paid"} text={HOSTED ? "Proof made" : "Proof made on this machine"} />
            <Step
              on={!!job.total}
              done={job.state === "paid"}
              text={`${String(job.stage).startsWith("uploading") ? "Uploading the proof, then Solana verifies it" : "Solana verifies it"}${job.total ? ` · ${job.done}/${job.total} transactions` : ""}`}
            />
            <Step on={job.state === "paid"} done={job.state === "paid"} text="Paid from the Vineland vault" />
          </div>
          <div className="progress">
            <div style={{ width: `${pct}%` }} />
          </div>
          <div className="muted small">{job.state === "failed" ? `Failed: ${job.error}` : job.stage}</div>
          {receipt && (
            <div className="paid">
              <div>
                Paid {units(receipt.paid)} to <span className="mono">{short(receipt.to, 6)}</span>
              </div>
              <a href={explorer("tx", receipt.withdraw)} target="_blank" rel="noreferrer">
                See it on the explorer →
              </a>
              <div className="muted small">Your wallet does not appear in this transaction.</div>
            </div>
          )}
        </div>
      )}
      <div className="card wide muted small">
        On devnet a private payment takes about ten minutes today: the proof is checked on Solana itself across about two hundred transactions.{" "}
        {HOSTED
          ? "In this hosted devnet demo your notes and proofs live on Vineland's server; with the open-source node, or in-browser proving (next), the secret never leaves your device."
          : "Your secret never leaves this machine; the relayer only sees the proof and the recipient."}
      </div>
    </div>
  );
}

function Step({ on, done, text }) {
  return <div className={`step ${on ? "on" : ""} ${done ? "done" : ""}`}>{text}</div>;
}

function TopUp({ st, address, signer, refresh }) {
  const [count, setCount] = useState(1);
  const [log, setLog] = useState([]);
  const [busy, setBusy] = useState(false);
  const note = st?.pool?.note_amount ?? 1_000_000;
  const add = (line) => setLog((l) => [...l, line]);
  const deposit = async () => {
    setBusy(true);
    setLog([]);
    try {
      if (signer.kind === "keypair") {
        const r = await api.depositKeypair(count);
        r.deposits.forEach((d) => add({ text: "Deposited one note", sig: d.signature }));
      } else {
        const { transactions } = await api.depositPrepare(address, count);
        for (const t of transactions) {
          add({ text: "Sign the deposit in your wallet…" });
          const sig = await signer.signAndSend(t.transaction);
          add({ text: "Deposit sent", sig });
          await api.depositConfirm(t.note, sig);
          add({ text: "Deposit confirmed on chain" });
        }
      }
      add({ text: "Folding your deposit into the on-chain tree (about a minute)." });
      refresh();
    } catch (e) {
      add({ text: `Error: ${e.message}`, error: true });
    }
    setBusy(false);
  };
  return (
    <div className="grid">
      <div className="card wide">
        <p>
          Deposit test dollars as private notes of {units(note)} each. Your deposit is public and attributable to you (the auditor sees it);
          what nobody can see later is which payment it funded.
        </p>
        <div className="row">
          <input type="number" min="1" max="5" value={count} onChange={(e) => setCount(Math.max(1, Math.min(5, +e.target.value || 1)))} className="num" />
          <span className="muted">note{count > 1 ? "s" : ""} = {units(count * note)} test dollars</span>
          <button className="btn primary" disabled={busy || !st?.wallet?.approved} onClick={deposit}>
            {busy ? "Depositing…" : st?.wallet?.approved ? "Deposit" : "Complete KYC first"}
          </button>
        </div>
        <ul className="log">
          {log.map((l, i) => (
            <li key={i} className={l.error ? "err" : ""}>
              {l.text}{" "}
              {l.sig && (
                <a href={explorer("tx", l.sig)} target="_blank" rel="noreferrer">
                  {short(l.sig, 6)}
                </a>
              )}
            </li>
          ))}
        </ul>
      </div>
      <div className="card wide">
        <div className="label">Your notes</div>
        <div className="table-wrap"><table>
          <thead>
            <tr>
              <th>Note</th>
              <th>State</th>
              <th>Deposit</th>
            </tr>
          </thead>
          <tbody>
            {(st?.wallet?.note_list ?? []).map((n) => (
              <tr key={n.id}>
                <td className="mono">{short(n.id, 6)}</td>
                <td>{n.state}</td>
                <td>{n.deposit_sig ? <a href={explorer("tx", n.deposit_sig)} target="_blank" rel="noreferrer">{short(n.deposit_sig, 6)}</a> : "–"}</td>
              </tr>
            ))}
          </tbody>
        </table></div>
      </div>
    </div>
  );
}

function Receipts({ address }) {
  const [rows, setRows] = useState([]);
  useEffect(() => {
    api.receipts(address).then((r) => setRows(r.receipts)).catch(() => {});
  }, [address]);
  const exportCsv = async () => {
    const blob = await api.receiptsCsv(address);
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = "vineland-receipts.csv";
    a.click();
  };
  return (
    <div className="grid">
      <div className="card wide">
        <div className="row between">
          <p className="muted">
            Each private payment has a receipt only you hold: the note behind it. Export them to show your auditor exactly which payments were
            yours.
          </p>
          <button className="btn" onClick={exportCsv} disabled={!rows.length}>
            Export for auditor (CSV)
          </button>
        </div>
        <div className="table-wrap"><table>
          <thead>
            <tr>
              <th>When</th>
              <th>Reference</th>
              <th>Paid to</th>
              <th>Amount</th>
              <th>Transaction</th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 && (
              <tr>
                <td colSpan="5" className="muted">
                  No payments yet.
                </td>
              </tr>
            )}
            {rows
              .slice()
              .reverse()
              .map((r) => (
                <tr key={r.withdraw}>
                  <td>{new Date(r.time * 1000).toLocaleString()}</td>
                  <td>{r.reference || "–"}</td>
                  <td className="mono">{short(r.to, 6)}</td>
                  <td>{units(r.paid)}</td>
                  <td>
                    <a href={explorer("tx", r.withdraw)} target="_blank" rel="noreferrer">
                      {short(r.withdraw, 6)}
                    </a>
                  </td>
                </tr>
              ))}
          </tbody>
        </table></div>
      </div>
    </div>
  );
}

function ConnectedAgents() {
  const [grants, setGrants] = useState(null);
  const [err, setErr] = useState("");
  const load = useCallback(() => {
    agentsApi
      .grants()
      .then((g) => setGrants(Array.isArray(g) ? g : g.grants || g.data || []))
      .catch((e) => setErr(e.message));
  }, []);
  useEffect(load, [load]);
  const revoke = async (id) => {
    try {
      await agentsApi.revoke(id);
      load();
    } catch (e) {
      setErr(e.message);
    }
  };
  return (
    <div className="card wide">
      <div className="label">Agents with access to this account</div>
      <p className="muted small">Revoking stops renewals at once; an access token already issued expires within 15 minutes.</p>
      {err && <div className="alert">{err}</div>}
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Granted</th>
              <th>Last used</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {grants && grants.length === 0 && (
              <tr>
                <td colSpan="3" className="muted">
                  No agent has access.
                </td>
              </tr>
            )}
            {(grants || []).map((g) => (
              <tr key={g.id}>
                <td>{g.created_at ? new Date(g.created_at).toLocaleString() : "–"}</td>
                <td>{g.last_used_at ? new Date(g.last_used_at).toLocaleString() : "never"}</td>
                <td>
                  <button className="btn" onClick={() => revoke(g.id)}>
                    Revoke
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function Agents({ cfg }) {
  const [code, setCode] = useState("");
  const base = window.location.origin;
  const script = "https://raw.githubusercontent.com/Galmanus/vineland-sol/main/client/scripts/vineland-agent.py";
  const hostedSteps = `# 1. the agent asks for access (Python 3, standard library only)
curl -sO ${script}
python3 vineland-agent.py connect
#    -> prints a code and a link; you approve it here

# 2. once approved, the agent pays a provider privately: one call
python3 vineland-agent.py pay --to <PROVIDER_WALLET> --reference data-api-nov

# the same over plain HTTP, with the agent's access token
curl -s -X POST ${base}/api/pay \\
  -H "Authorization: Bearer $AGENT_ACCESS_TOKEN" -H "Content-Type: application/json" \\
  -d '{"to": "<PROVIDER_WALLET>", "notes": 1, "reference": "data-api-nov"}'`;
  const localSteps = `# what the agent's wallet can pay with right now
curl -s ${base}/api/status -H "Authorization: Bearer $VINELAND_TOKEN"

# pay a provider privately: one call, the proof is made on this machine
curl -s -X POST ${base}/api/pay \\
  -H "Authorization: Bearer $VINELAND_TOKEN" -H "Content-Type: application/json" \\
  -d '{"to": "<PROVIDER_WALLET>", "notes": 1, "reference": "data-api-oct"}'

# follow the payment until it is paid
curl -s ${base}/api/jobs/<JOB_ID> -H "Authorization: Bearer $VINELAND_TOKEN"`;
  return (
    <div className="grid">
      <div className="card wide">
        <p>
          An AI agent that pays for data, models and APIs in stablecoins tells everyone watching the chain who it works for and what it buys.
          Through Vineland the provider is paid from the vault: nobody can map which agent bought which service.
        </p>
        {HOSTED ? (
          <>
            <div className="steps three">
              <div className="step on">
                <b>1 · The agent asks.</b> It runs one command and shows you a short code.
              </div>
              <div className="step on">
                <b>2 · You approve.</b> Paste the code below, or open the link it printed. The agent never receives a key.
              </div>
              <div className="step on">
                <b>3 · It pays.</b> Private payouts from this account's notes, until you revoke it.
              </div>
            </div>
            <form
              className="row"
              onSubmit={(e) => {
                e.preventDefault();
                if (code.trim()) window.location.href = `./authorize?user_code=${encodeURIComponent(code.trim().toUpperCase())}`;
              }}
            >
              <input className="code-input" value={code} onChange={(e) => setCode(e.target.value)} placeholder="Agent's code, e.g. ABCDE-FGHIJ" />
              <button className="btn primary" disabled={!code.trim()}>
                Review and approve
              </button>
            </form>
            <pre>{hostedSteps}</pre>
            <p className="muted small">
              Access tokens last 15 minutes and renew for up to 30 days, only while you allow it. Want the agent's secrets on the agent's own machine
              instead? It can run the open-source node (keypair mode) and call it locally.
            </p>
          </>
        ) : (
          <>
            <p className="muted small">
              The agent runs this node with its own approved wallet (<span className="mono">VINELAND_KEYPAIR</span>) and calls the same API this dashboard uses.
              The token is printed when the node starts; the agent's secrets stay on the agent's machine.
            </p>
            <pre>{localSteps}</pre>
          </>
        )}
      </div>
      {HOSTED && <ConnectedAgents />}
    </div>
  );
}

function Settings({ cfg, st }) {
  if (!cfg) return null;
  const rel = st?.relayer;
  const rows = [
    ["Cluster", cfg.cluster],
    ["Pool", cfg.pool],
    ["Test dollar mint", cfg.mint],
    ["Pool program", cfg.pool_program],
    ["Verifier program", cfg.verifier_program],
    ["Relayer", `${cfg.relayer}${rel ? ` · ${rel.relayer} · ${Number(rel.balance_sol).toFixed(2)} SOL` : " · unreachable"}`],
  ];
  return (
    <div className="grid">
      <div className="card wide">
        <div className="table-wrap"><table>
          <tbody>
            {rows.map(([k, v]) => (
              <tr key={k}>
                <td className="muted">{k}</td>
                <td className="mono">{v}</td>
              </tr>
            ))}
          </tbody>
        </table></div>
        <p className="muted small">Devnet only. Amounts are public by design; the link between payer and payee is what Vineland removes.</p>
      </div>
    </div>
  );
}
