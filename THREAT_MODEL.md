# Threat model

This is the model used for the self-review in [SELF_AUDIT.md](SELF_AUDIT.md). It describes the Solana devnet pool in this repository and the riverrun M31 verifier it calls. It is not a claim that the system meets the model, and it is not a claim of quantum security.

The hosted UI is out of scope. It lives in a separate private repository and is not part of this review.

## System overview

An operator allowlists addresses. An allowlisted key deposits one fixed-size SPL note into a program-owned vault and publishes a 32-byte commitment. The program folds that commitment into a depth-127 Poseidon2-M31 Merkle tree. A later withdrawal pays the note, minus a capped fee, to a recipient token account. The withdrawal is accepted only when a buffer owned by the riverrun verifier program is finalized as a recipient-bound crowd proof (`kind` `0x83`) whose public inputs match this pool: action, round, root, nullifier, and recipient.

The commitment is `Poseidon2(secret || action)` truncated to 8 M31 limbs. The nullifier is `Poseidon2(secret || round)` truncated the same way. The leaf is hidden behind `C = compress(leaf, blinder)`. Two proofs share `C`: a binding proof (secret, action, round, nullifier) and a membership proof (the leaf sits under the named root). Challenges are derived on chain from the sealed proof bytes. The recipient is mixed into the Fiat-Shamir transcript for a bound proof.

A public exit reveals the secret, recomputes the same nullifier, and refunds only the depositor. Withdraw and exit share that nullifier account, so a note pays once.

## Trust boundaries

| Boundary | What crosses it |
|---|---|
| Depositor to pool | A signature, a fixed token amount, a commitment |
| Relayer to pool | A withdrawal transaction. The relayer chooses when to submit it. The recipient is fixed by the proof |
| Verifier buffer to pool | A finalized account. The pool trusts the verifier program id in source, the owner, the kind, the finalized flag, and the public fields |
| Operator | Allowlist membership and `k_min`. Not custody and not roots |
| Upgrade authority | Can replace either program. On devnet that key is the author's |

## Assets

- The SPL tokens in the vault. The vault authority is a PDA of this program. The program transfers exactly one note per spend.
- The nullifier set. Spending twice is the failure that drains other depositors.
- The anonymity set: deposits under the named root, minus public exits of notes in that root. It counts deposits, not people.
- The link between a depositor and a recipient. The proof is supposed to hide which leaf was spent.

## Adversaries

- A user who deposits, withdraws, or uploads a proof, including one who forges proof bytes.
- A relayer who censors, reorders, or copies an uploaded proof.
- The operator, who can fill the allowlist with their own notes and can set `k_min`. They cannot sign for the vault.
- A quantum-capable adversary. Grover and collision attacks reduce the cost of breaking Keccak-256 and a truncated Poseidon2-M31 digest. Shor's algorithm breaks the ed25519 signatures Solana uses for deposits, operator actions, and relayer fees. Those signatures do not by themselves authorize a payout. They do authorize deposits and the allowlist. A hash-based STARK is not, by itself, a proof that the system is quantum-secure. The FRI parameters in use are not a 128-bit set. See the self-audit.

## Assumptions

- Poseidon2-M31, truncated to 8 canonical limbs, is collision-resistant and preimage-resistant for the purposes of this design. The truncation is deliberate: publishing the full state would invert to the secret. The assumption is conjectural.
- Keccak-256, used for the Fiat-Shamir transcript and the Merkle caps, resists collisions and second-preimage attacks at the level the FRI queries actually provide, not at a blanket 256-bit or post-quantum level.
- The transcript binds the public inputs, the degree, and, for a bound proof, the recipient. A challenge is sampled only after the bytes it must bind have been absorbed. The verifier derives challenges. The client does not supply them.
- The Solana runtime enforces account ownership, signer bits, and the compute budget. A transaction that runs out of compute fails atomically. The pool's fold is capped per transaction because one compression does not fit the 1.4M unit budget for the whole tree.
- Account checks in the pool match the seeds: deposit records, the vault, the tree, the nullifier PDA, and the verifier buffer's owner. A buffer the verifier does not own is ignored.

## Attack surfaces

- Forged or shorter membership proofs. The membership AIR's first and last rows depend on the trace height. An unpinned height is a different-depth opening. C-01, fixed in the verifier source and not yet deployed.
- Rebinding a proof to another recipient. Bound proofs seed the transcript with the recipient. The pool also checks the buffer's recipient field against the token account.
- Replay. The nullifier PDA is created on the first spend and shared by withdraw and exit.
- Exit censorship. The secret is in the exit transaction. A party who delays it can try to withdraw to their own recipient before the exit lands.
- Anonymity shrinkage. `pool.exits` is global. An exit of a note outside the named root shrinks that root's set. The check fails closed. It can block a withdrawal that the set would still have allowed.
- Compute exhaustion. A caller can force work up to the budget. The transaction reverts. It does not partially pay.
- Upgrade. Replacing the verifier with a program that finalizes any buffer, or the pool with a program that drains the vault, takes the upgrade key.

## Mitigations and gaps

Mitigations that are in the source reviewed at `fc2de62`, plus the verifier pin that is not yet what devnet runs:

- The pool checks verifier owner, version, `finalized == 1`, `kind == 0x83`, canonical limbs, and equality of action, round, root, nullifier, and recipient.
- Nullifier and exit share one PDA. Canonical limbs stop a second encoding from creating a second account.
- Fee is capped at 1000 bps and must be strictly less than the note. The transfer amount is the note, not the vault balance.
- Inserts are permissionless and in order. The root history holds 16 roots the program itself computed.

Gaps:

- The deployed verifier does not yet pin `degree_bits`. The pin is [mirror-pool#1](https://github.com/Galmanus/mirror-pool/pull/1). This repository still depends on `ce5e8c7`.
- FRI query count, blowup, and proof-of-work are below a 128-bit target. Changing them means new proofs and a new verifier build.
- Upgrade authorities are trusted.
- The Stellar admit path does not bind the holder. It is not in this repository. See H-01 in the self-audit.
- Leaves and internal nodes are not domain-separated.
- A public exit publishes the secret.
