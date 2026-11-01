// The node's JSON API. The token arrives injected into index.html by
// vineland-node; nothing here leaves the user's machine except what the node
// sends to the relayer (a recipient-bound proof, never a secret).

const token = document.querySelector('meta[name="vineland-token"]')?.content ?? "";

// Hosted: the Privy session token authenticates each call; local: the node's token.
let authProvider = null;
export function setAuthProvider(fn) {
  authProvider = fn;
}
async function bearer() {
  if (authProvider) return (await authProvider()) || "";
  return token;
}

export const HOSTED = import.meta.env.VITE_HOSTED === "1";

async function call(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: { Authorization: `Bearer ${await bearer()}`, ...(body ? { "Content-Type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

export const api = {
  token,
  config: () => call("GET", "/api/config"),
  status: (wallet) => call("GET", `/api/status${wallet ? `?wallet=${wallet}` : ""}`),
  onboard: (wallet) => call("POST", "/api/onboard", { wallet }),
  depositPrepare: (wallet, count) => call("POST", "/api/deposit/prepare", { wallet, count }),
  depositConfirm: (note, signature) => call("POST", "/api/deposit/confirm", { note, signature }),
  depositKeypair: (count) => call("POST", "/api/deposit", { count }),
  pay: (wallet, to, notes, reference) => call("POST", "/api/pay", { wallet, to, notes, reference }),
  job: (id) => call("GET", `/api/jobs/${id}`),
  receipts: (wallet) => call("GET", `/api/receipts${wallet ? `?wallet=${wallet}` : ""}`),
  exitPrepare: (note) => call("POST", "/api/exit/prepare", { note }),
  exitConfirm: (note, signature) => call("POST", "/api/exit/confirm", { note, signature }),
  async receiptsCsv(wallet) {
    const res = await fetch(`/api/receipts.csv${wallet ? `?wallet=${wallet}` : ""}`, { headers: { Authorization: `Bearer ${await bearer()}` } });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return res.blob();
  },
};

export const b64ToBytes = (b64) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
export const bytesToB64 = (bytes) => btoa(String.fromCharCode(...bytes));

// Send a signed transaction through the cluster's JSON-RPC and return its signature.
export async function sendRaw(rpcUrl, bytes) {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "sendTransaction",
      params: [bytesToB64(bytes), { encoding: "base64", preflightCommitment: "confirmed" }],
    }),
  });
  const data = await res.json();
  if (data.error) throw new Error(data.error.message || "sendTransaction failed");
  return data.result;
}

export const explorer = (kind, id) => `https://explorer.solana.com/${kind}/${id}?cluster=devnet`;
export const short = (s, n = 4) => (s ? `${s.slice(0, n)}…${s.slice(-n)}` : "");
