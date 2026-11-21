# Self-audit

This is a self-review by the author. It is not an independent audit. The programs are devnet-only. Nothing here is production-ready, and nothing here is claimed to be quantum-secure.

## Scope

| Tree | Commit |
|---|---|
| This repository, `vineland-sol`, the pool and the client | `fc2de627605d2715e91b26b2b9ddb83e11352f2e` on `main`, plus the documentation in the commit that adds this file. No program instruction logic changes in that documentation commit |
| Verifier and crowd proofs, `Galmanus/mirror-pool` branch `feat/staged-onchain-verifier` | `ce5e8c7cfe55e116c2139f5413e76c961848079c`, which is the git revision `programs/vineland-pool` depends on |
| C-01 fix | `d3e9f69ac6af59ed48f0a1a5ff8494d82ba7c87a` on `cursor/pin-degree-bits-f381`, pull request [mirror-pool#1](https://github.com/Galmanus/mirror-pool/pull/1). Not merged, not deployed, not the revision this repository builds against |

Reviewed: `programs/vineland-pool/src/lib.rs`, the pool tests' account layout, `client/src/lib.rs` and `client/src/v2.rs` for the proof parameters and the public inputs, and in mirror-pool `programs/riverrun-m31-verifier/src/staged.rs`, `crates/riverrun-m31/src/staged.rs`, and `crates/riverrun-m31/src/crowd.rs`.

The Stellar contracts were read only far enough to confirm H-01. They are not in this repository. The private dashboard was not read.

Program ids, unchanged:

- Pool `3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi`
- Verifier `8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo`

## Methodology

Manual reading of the spend path: buffer owner, kind, finalized flag, public-input binding, nullifier PDA, exit, tree insert, and the membership AIR's row selectors. The staged verifier was compared with the way `NUM_QUERIES` is pinned. Stale comments were checked against the code.

For C-01, the pin was implemented and `cargo test --release --lib` was run in `crates/riverrun-m31` at the fix commit. 59 tests passed, 1 ignored. Two pre-existing `should_panic` tests in `asp_history` do not panic in release builds, because the prover checks constraints only when debug assertions are on. They are not part of this pin. `cargo check --lib` of the verifier crate succeeded. The on-chain LiteSVM suite of the verifier was not re-run. The pool's LiteSVM suite was not re-run for this document.

No fuzzer was run. The upstream tamper sweep was not re-run except as part of that library test binary, which includes `a_byte_flip_sweep_never_separates_the_two_paths` and it passed.

## Findings

### C-01 — Critical — Fixed in verifier source, not deployed

**Location.** `CrowdMembershipAir` in `crates/riverrun-m31/src/crowd.rs`. `when_first_row`, `when_transition`, and `when_last_row` are the only constraints that count the fold. The trace height is `Proof::degree_bits`, which the staged verifier accepted from the proof. `NUM_QUERIES` was pinned. The degree was not.

**Description.** A membership trace of height other than `CROWD_DEPTH + 1` (128 rows, 127 folds) is a different-depth opening. The STARK checks succeed for that shorter or longer statement. The pool treats a finalized buffer as membership in the depth-127 root it computed. A note that can be opened at more than one height can be spent more than once if the second opening carries a statement the pool still accepts, or the same nullifier machinery can be aimed at a root the proof never actually reached in 127 hashes.

**Fix.** `degree_bits` is derived, not hard-coded. Binding uses `LOG_ROWS + ZK_DEGREE_PAD`. Membership uses `log2(CROWD_DEPTH + 1) + ZK_DEGREE_PAD`. `CROWD_DEPTH` is `2^LOG_ROWS - 1`, which is the pool's production tree (`vineland-pool` `DEPTH` is 127, and the client passes `LOG_ROWS = 7`). `ZK_DEGREE_PAD` is the hiding PCS `is_zk()` bit. A const assert requires the two degrees to match. `Params.degree_bits` and `vk_tag` carry them. The staged stages and `SEAL` reject a mismatch, and so do `verify_binding_crowd` and `verify_membership_crowd`.

**Test.** `production_depth_passes_and_other_depths_fail_on_both_paths`. An honest proof at depth 127 passes on the monolithic path and the staged path. Proofs at depth 63 and depth 255, and binding proofs at `LOG_ROWS - 1` and `LOG_ROWS + 1`, are accepted by the unpinned STARK check and rejected by both verifiers.

**Status.** Fixed in the verifier source in [mirror-pool#1](https://github.com/Galmanus/mirror-pool/pull/1). Open on devnet until that program is upgraded in place. This repository still pins `ce5e8c7`, and the pool does not re-check `degree_bits` itself. Do not treat the deployed buffer as pinned.

### H-01 — High — Open

**Location.** Not in this repository. `Galmanus/vineland-stellar`, `vineland-eligibility-registry/src/lib.rs`, `VinelandEligibilityRegistry::admit`.

**Description.** `admit(holder, mem_proof, mem_publics)` checks that `mem_publics` is 128 bytes and that `keccak256` of bytes 64..128 matches the issuer root, then calls `verify_crowd_membership(proof, publics, nq, lb)`. The holder address is not an argument to the verifier and is not part of the public inputs. The proof shows that some member of the set knows a witness. It does not show that the caller controls `holder`.

**Impact.** One accepting membership proof admits any address the caller passes. Eligibility is the gate the compliance module reads.

**Status.** Open. No change in this repository, because the contract is not here, and no test was added here for the same reason. Recommendation: bind the holder into the Fiat-Shamir transcript or into the public inputs the verifier checks, and add a test that a proof made for address A is rejected when the admitted address is B. Until that lands, do not treat a Stellar admission as bound to the address that was written.

### Other findings

**High — Acknowledged.** Both programs are upgradeable. The upgrade authority can replace the verifier with one that finalizes anything, or the pool with one that moves the vault. The README names that key as the author's, on devnet. This was not checked on chain in this review. Recommendation: revoke or freeze the upgrade authority before any value that matters. Not a defect in the instruction logic.

**Medium — Acknowledged.** Production FRI is 12 queries, blowup `2^7`, no commit-phase proof of work, 8 bits of query-phase proof of work (`make_zk_config_bound`). That is on the order of 90 bits of conjectured query security before proximity-gap and Johnson losses, not a 128-bit parameter set. Keccak-256 collisions and a Poseidon2-M31 digest truncated to 248 bits are cheaper against a quantum collision attack than the classical figures. Solana ed25519 signatures remain classical. Recommendation: treat the parameter set as a research choice. Raising it means a new verifier build and new proofs. Do not describe the deployment as quantum-secure.

**Medium — Acknowledged.** `exit` puts the note's secret in the transaction. A party who can delay that transaction for the length of a verification (on the order of 200 transactions) can try to withdraw to their own recipient first. The exit pays only the depositor, so copying the secret into another exit does not steal. Recommendation: a commit-reveal, or an exit that is itself a proof. Location: `vineland_pool::exit` and the client exit instruction.

**Low — Open.** `pool.exits` counts every public exit, including notes that are not in the root a withdrawal names. The anonymity check uses that global counter, so an exit outside the root shrinks the set. The direction is fail-closed: it can refuse a withdrawal the set would still have allowed. It does not let a smaller set pass. Recommendation: subtract only exits whose deposit index is below the root's size, and refuse a record that is inside the root or not exited. A change was drafted locally and is not in this commit, because the pool tests were not re-run against it.

**Low — Acknowledged.** Leaves and internal nodes share `compress`, and `EMPTY[0]` is zero. The deposit path rejects an all-zero commitment. A collision between a leaf and an internal node would make two openings of the same root. Recommendation: domain-separate the leaf compression. Not a local one-line change.

**Low — Acknowledged.** `note_action` masks a SHA-256 limb with the Mersenne prime and maps the prime itself to zero, so zero is about twice as likely as any other limb. The same function runs on chain and in the client. It is a public value, not a secret. Recommendation: leave both sides alone unless both move together. A one-sided change desynchronizes existing pools.

**Info.** `Transcript::decode` reduces a non-canonical limb. The program writes canonical limbs. The bytes are not attacker-supplied after SEAL.

**Info.** The verifier `close` path zeros the account and relies on the runtime to collect a zero-lamport account. It does not assign the System program.

**Info.** `verify_staged_setup` rejects an under-reported FRI height and leaves an over-reported height to FRI. No exploit was shown.

**Info.** The operator can sybil the anonymity set. `k_min` is a floor on deposits, not an effective-anonymity measure. The relayer sees the recipient. The note size is fixed, which hides the amount and leaks the timing. A deposit of a commitment the depositor cannot open burns the note. Donations to the vault do not increase a withdrawal: the program pays the note, not the balance.

## What was checked and held

These are not a clean bill. They are the controls that were present in the source that was read.

- Canonical nullifiers, one PDA, shared by withdraw and exit.
- The pool requires the verifier owner, version 1, `finalized == 1`, and kind `0x83`.
- Public inputs checked by the pool include the recipient token account. Bound proofs also seed the transcript with that recipient.
- Challenges are derived after SEAL. The query bitmap requires each of the 12 positions once.
- Binding and membership share one stored `C`.
- Deposit, allow, and revoke check the signer and the PDA seeds that were read.
- Fee cap and checked arithmetic on the note count. Insert order is enforced. A compute-budget failure reverts the transaction.

## Result

No independent audit was done. The devnet verifier at the pinned revision does not pin membership depth; the source fix is in the pull request named above and is not deployed. The Stellar admit path does not bind the holder, and it was not changed. Do not use these programs for real funds.
