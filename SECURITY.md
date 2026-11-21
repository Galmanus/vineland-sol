# Security

Vineland on Solana is research-stage software. It runs on devnet. It has not been independently audited, and it is not claimed to be quantum-secure. Do not put value you cannot afford to lose into a devnet pool.

## Reporting a vulnerability

Email [space.galmanus@gmail.com](mailto:space.galmanus@gmail.com).

Please include the affected repository and commit, what an attacker can do, and whether you have a proof of concept. A soundness or theft bug should stay in that email until there is a fix. A public issue is fine for a problem that does not move funds or break a proof.

Please do not exploit a bug past the point needed to show it, and do not test against a pool that holds someone else's tokens.

There is no bug-bounty program and no response-time promise. Reports are read by the author.

The Stellar contracts live in [Galmanus/vineland-stellar](https://github.com/Galmanus/vineland-stellar), not in this repository. The same address covers them. The staged verifier lives in [Galmanus/mirror-pool](https://github.com/Galmanus/mirror-pool).

## What this repository is

`programs/vineland-pool` is the settlement program. Membership proofs are checked by the separate riverrun verifier program. [SELF_AUDIT.md](SELF_AUDIT.md) is a self-review of that boundary. [THREAT_MODEL.md](THREAT_MODEL.md) is the model that review used. [LICENSE](LICENSE) is MIT, copyright 2026 Manuel Guilherme.
