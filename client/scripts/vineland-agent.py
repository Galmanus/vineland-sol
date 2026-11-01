#!/usr/bin/env python3
"""vineland-agent: an AI agent pays privately through Vineland, authorized by a
person, without ever holding a key.

  vineland-agent.py connect                 ask for access; a person approves it
                                            at vineland.app/app/authorize
  vineland-agent.py status                  what the account can pay with
  vineland-agent.py pay --to WALLET [--reference REF]
                                            one private payout, followed until paid

How it works: Privy's device authorization. The agent gets a short code, the
company approves it in the Vineland dashboard (signed in), and the agent receives
a 15-minute access token and a 30-day refresh token for that account. It calls
Vineland's API with them. The company can revoke the agent at any time from the
dashboard's Agents page. Tokens are kept in ~/.vineland/agent-token.json (0600).

Standard library only. Devnet only: the programs are not audited.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

PRIVY_APP_ID = os.environ.get("VINELAND_PRIVY_APP_ID", "cmvxuqdu403pc0gl5d5ngzug8")
API = os.environ.get("VINELAND_API", "https://vineland.app")
PRIVY = "https://auth.privy.io/api/oauth/v2"
TOKEN_FILE = os.path.expanduser(os.environ.get("VINELAND_AGENT_TOKEN", "~/.vineland/agent-token.json"))


def http(method, url, body=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    # Cloudflare in front of auth.privy.io refuses Python's default user agent (1010).
    req.add_header("User-Agent", "vineland-agent/0.1 (+https://github.com/Galmanus/vineland-sol)")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"{}")
        except ValueError:
            return e.code, {}


def save(tok):
    os.makedirs(os.path.dirname(TOKEN_FILE), exist_ok=True)
    fd = os.open(TOKEN_FILE + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(tok, f)
    os.replace(TOKEN_FILE + ".tmp", TOKEN_FILE)


def connect(_args):
    code, d = http("POST", f"{PRIVY}/device_authorization", {}, {"privy-app-id": PRIVY_APP_ID})
    if code != 200:
        sys.exit(f"device authorization refused ({code}): {d}")
    url = d.get("verification_uri_complete") or f"{d['verification_uri']}?user_code={d['user_code']}"
    print(f"Ask the account owner to open:\n  {url}\nand approve the code {d['user_code']} (valid {d['expires_in'] // 60} minutes).")
    interval = int(d.get("interval", 5))
    deadline = time.time() + int(d.get("expires_in", 600))
    while time.time() < deadline:
        time.sleep(interval)
        code, t = http("POST", f"{PRIVY}/token", {"grant_type": "urn:ietf:params:oauth:grant-type:device_code", "device_code": d["device_code"]}, {"privy-app-id": PRIVY_APP_ID})
        if code == 200 and t.get("access_token"):
            save({"access_token": t["access_token"], "refresh_token": t["refresh_token"], "expires_at": time.time() + int(t.get("expires_in", 900))})
            print("Approved. This agent can now pay from that Vineland account until the owner revokes it.")
            return
        err = t.get("error")
        if err == "authorization_pending":
            continue
        if err == "slow_down":
            interval += 5
            continue
        sys.exit(f"not approved: {err or t}")
    sys.exit("the code expired; run connect again")


def access_token():
    try:
        with open(TOKEN_FILE) as f:
            tok = json.load(f)
    except FileNotFoundError:
        sys.exit("not connected: run `vineland-agent.py connect` first")
    if tok["expires_at"] - time.time() > 60:
        return tok["access_token"]
    code, t = http("POST", f"{PRIVY}/token", {"grant_type": "refresh_token", "refresh_token": tok["refresh_token"]}, {"privy-app-id": PRIVY_APP_ID})
    if code != 200:
        sys.exit(f"access was revoked or expired ({t.get('error', code)}): run connect again")
    # The old refresh token is invalid from now on: store the new one first.
    save({"access_token": t["access_token"], "refresh_token": t["refresh_token"], "expires_at": time.time() + int(t.get("expires_in", 900))})
    return t["access_token"]


def api(method, path, body=None):
    code, d = http(method, f"{API}{path}", body, {"Authorization": f"Bearer {access_token()}"})
    if code >= 400:
        sys.exit(f"{method} {path}: {code} {d.get('error', d)}")
    return d


def status(_args):
    print(json.dumps(api("GET", "/api/status"), indent=2))


def pay(args):
    job = api("POST", "/api/pay", {"to": args.to, "notes": 1, "reference": args.reference})["job"]
    print(f"job {job}: paying {args.to} privately (about 10 minutes on devnet)")
    last = None
    while True:
        j = api("GET", f"/api/jobs/{job}")
        line = f"  {j['stage']}" + (f" {j['done']}/{j['total']}" if j.get("total") else "")
        if line != last:
            print(line, flush=True)
            last = line
        if j["state"] == "paid":
            r = j["receipts"][0]
            print(f"paid {r['paid']} to {r['to']}: https://explorer.solana.com/tx/{r['withdraw']}?cluster=devnet")
            return
        if j["state"] == "failed":
            sys.exit(f"failed: {j.get('error')}")
        time.sleep(5)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("connect").set_defaults(fn=connect)
    sub.add_parser("status").set_defaults(fn=status)
    p = sub.add_parser("pay")
    p.add_argument("--to", required=True)
    p.add_argument("--reference", default="")
    p.set_defaults(fn=pay)
    if "mainnet" in API:
        sys.exit("refusing mainnet: the programs are not audited")
    args = ap.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
