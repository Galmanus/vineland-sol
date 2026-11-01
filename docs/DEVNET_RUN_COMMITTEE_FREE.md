# Vineland on Solana: committee-free devnet run

No committee key exists in this run. The pool program computed the membership root itself, and the riverrun M31 verifier program verified both recipient-bound crowd proofs on chain before the pool paid.

- cluster: `https://api.devnet.solana.com`
- pool program (v2): [`3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi`](https://explorer.solana.com/address/3W1FD5Q1ELFzVAoJUasSj3aZGmgs4zrRdgfZZV1K5VXi?cluster=devnet)
- verifier program: [`8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo`](https://explorer.solana.com/address/8vPHUTH27WXYNvNoaBTJGWWceNSVrNXHZSTYkaywkcYo?cluster=devnet)
- pool: `GSZyL74Ap5FY6Xqufm4Gg6A81HJRs7SdMXAc84KrMoLA`
- test dollar mint: `AvS7vsgSt7gN3qvmfWqdYvgrgvYMvswb4WGm6JaywYvL` (6 decimals, devnet only)
- note: 1000000 base units, fee 197 bps, k_min 3
- relayer: `88QBJGQ7dSrPEdQRUiDzt1xixcyb4ywE3UY2mhL2PNNX`
- clean wallet: `6SsPJVjfzKuqVrm9dgNZzzoFtyP58KqLyk9v4kYmYKzJ` (never funded; received 980300)
- withdrawal: [Sg7NfZ2zgYiApPmxko1HRnRQmv6CmWGG6RACVjQPcg8yZ9wYRzeeDgmDqLEmqHrKq2uwQ14L3YhHawryJ59t9Cw](https://explorer.solana.com/tx/Sg7NfZ2zgYiApPmxko1HRnRQmv6CmWGG6RACVjQPcg8yZ9wYRzeeDgmDqLEmqHrKq2uwQ14L3YhHawryJ59t9Cw?cluster=devnet)
- public exit: [3QYDhzkquiENDEWeN47YfLdx7CWfjuLuj8RHsmb6LBsbQoeok8sSwpSfm2WhNrgAkuCgCfjvgJGxEHZ4gEojb74B](https://explorer.solana.com/tx/3QYDhzkquiENDEWeN47YfLdx7CWfjuLuj8RHsmb6LBsbQoeok8sSwpSfm2WhNrgAkuCgCfjvgJGxEHZ4gEojb74B?cluster=devnet)
- proofs: binding 109377 B + membership 84257 B, proved in 4986 ms on this machine, bound to the clean wallet's token account
- on-chain verification: 213 transactions in 618 s
- final balances: clean 980300, treasury 19700, vault 1000000, member 2 refunded 1000000
- transactions sent by this run: 239 in 703 s

| step | signature |
|---|---|
| fund roles | [`4rn467GTrxncR3Jg…`](https://explorer.solana.com/tx/4rn467GTrxncR3JgXYj5GSkbXwAHTiTUm9NQfSxHwaWmnqy3AF66bW7vxctm6FJxGbvt2AtPSP6ongb9HshUpnd6?cluster=devnet) |
| create test dollar + treasury | [`3wwDcgzxEFB58tXL…`](https://explorer.solana.com/tx/3wwDcgzxEFB58tXLsrvQpRukKuBU5AUUPPnzwBzq2K2qTLLB88Zc6yb6Z3Cunif8GAgQxUEvovWKLbGn2M92TWZS?cluster=devnet) |
| initialize pool (no committee) | [`5czxeaF4Hwu6EH69…`](https://explorer.solana.com/tx/5czxeaF4Hwu6EH69keNksGCXWowTr2icYKnzo3uCZk1mfKtJqq52cLiXRWKzEEpkAdcsq7DcxXeeYNxXumWy2ieX?cluster=devnet) |
| member 0: token account + 1 note | [`57DQeHH7XVAERcBW…`](https://explorer.solana.com/tx/57DQeHH7XVAERcBWQZQtJd7ww3pFP4VzxZmnbUB1CGLaio5PPxY2LfgXgDmCb7zqmzCqjqqhmzezj6RpnEm5hiM4?cluster=devnet) |
| member 1: token account + 1 note | [`4QnwoYZrAvQqhYXj…`](https://explorer.solana.com/tx/4QnwoYZrAvQqhYXjQNL7afL1zYxPPeJ39hMixxGri5tVXhvaxHkvzwNpmh4HfAh6VE5Gr6oYtjRcZz2mug5mhsWf?cluster=devnet) |
| member 2: token account + 1 note | [`3X1ot7fbN6822Si6…`](https://explorer.solana.com/tx/3X1ot7fbN6822Si6LJH8kMaKhdDJuMjmpAC2NU5q5XqpjQapeqgvwWMbVuDcGhJSe1K5vC33fsJ8HRDLLwEXkPYs?cluster=devnet) |
| member 0 deposits before approval | refused: `0xbc4 (AccountNotInitialized: no allowlist entry for this key)` |
| operator approves member 0 | [`26cDq7Lhb5HMHCKM…`](https://explorer.solana.com/tx/26cDq7Lhb5HMHCKMu5hg5qpZ7auW6yfNn67q6NmJLUcssF1TK4GgLaB3JWKzTcXQqF1VWpJJh9Wo33eE4n5nCUVU?cluster=devnet) |
| member 0 deposits (signs) | [`BvBRDLqV3MRVHQ7G…`](https://explorer.solana.com/tx/BvBRDLqV3MRVHQ7GVihoB4rAj3jRbDiFPGRZwNHeQ4ghAtmthcMFCv69pyGy3v1j5VPVboY88Df8dGdi35fLGqm?cluster=devnet) |
| operator approves member 1 | [`5rHnSmaNQbVLzusk…`](https://explorer.solana.com/tx/5rHnSmaNQbVLzuskVjS57CgCrd8odvd8sGjE9gvrXB552qUq8rMgbpTVbX1ysbd6MFZsgefzoMQiSfz1RKA2vCJr?cluster=devnet) |
| member 1 deposits (signs) | [`3kyiwz79KPnM26UU…`](https://explorer.solana.com/tx/3kyiwz79KPnM26UUMfEVmkGJy6o2TTaMGo1dakwhhUBSv5xLHaeQGKdF4NVUdoUwumuc79iBAzVwv8ekonamNr24?cluster=devnet) |
| operator approves member 2 | [`5QByfpTK8e2RJuQu…`](https://explorer.solana.com/tx/5QByfpTK8e2RJuQuShChyUaT7wDcW6uvuptDA1J1XF8HcTHBfQAJ3QZ7nqGipWqLnQLq6aq5bsyyzism8GRxLvbt?cluster=devnet) |
| member 2 deposits (signs) | [`7DwkKuogN3TFKBHX…`](https://explorer.solana.com/tx/7DwkKuogN3TFKBHXpxqE85aswjg2sE1qmTeVK9SNcDfKBKjxBATV9PuC346yJCgA9sBnGUJAnbmLKV1sy7A9WZZ?cluster=devnet) |
| fold leaf 1 before leaf 0 | refused: `0x1774 (OutOfOrder: leaves enter the tree in deposit order)` |
| fold leaf 0 into the tree (1/3) | [`3t33LaJPu95uEz8X…`](https://explorer.solana.com/tx/3t33LaJPu95uEz8XxEtX3ggoFRfyrPcqMQqPHhBQVPDP4hRLEczTQuRad4ermN6WUvxhB7dbo3TrFWjJwy2a2jUp?cluster=devnet) |
| fold leaf 0 into the tree (2/3) | [`4DxpnxYzWqnS8oqq…`](https://explorer.solana.com/tx/4DxpnxYzWqnS8oqqqk3fm8yaQkn7dhhMEcZyB7cgVzSjCFFWeBiB7uzoqyL5KQFdZae6LdTfncPQWimBDG1ViZWM?cluster=devnet) |
| fold leaf 0 into the tree (3/3) | [`2qLiGimm7tjuvSav…`](https://explorer.solana.com/tx/2qLiGimm7tjuvSavoXAefRqo6jmbJPmZRvZQJdfKfCZWnuXud8jnJz145Ksp4xNk6kHZqnL4UAHDcHcosjzjQbqF?cluster=devnet) |
| fold leaf 1 into the tree (1/3) | [`4MXANjaqJQaP6PQ1…`](https://explorer.solana.com/tx/4MXANjaqJQaP6PQ1vcoXmZvt7E9Awyg2or4H3VRGxUbswCmvMkVaL5xZG9h1sfxHY1dReLVT38RQbSPPtvEUaJkn?cluster=devnet) |
| fold leaf 1 into the tree (2/3) | [`3xnEeJ3ffYXV1LMh…`](https://explorer.solana.com/tx/3xnEeJ3ffYXV1LMhoVgfxRJ3ua831bEoPn3pqnmZTAbo7fLmL6MyWb7zjBNA1BbKuiMs1UPyTXvN42T1MXEQZThz?cluster=devnet) |
| fold leaf 1 into the tree (3/3) | [`3UXbWpz9qdzthZW3…`](https://explorer.solana.com/tx/3UXbWpz9qdzthZW3MyFr9CAgFwRZNNuDCwUzeCFreX96ZtRZaoAJemYTunuDKNQMaT5FegwmfhCytnziKr6YhSuf?cluster=devnet) |
| fold leaf 2 into the tree (1/3) | [`zPGjXuNh5YRiXf9V…`](https://explorer.solana.com/tx/zPGjXuNh5YRiXf9VpiXD951QkrWJRPA8p3dMNPbRYsU2rmHMQE3vmd4qQxoMM1Q58SNvUh26AoV2wMvgEEcZLwM?cluster=devnet) |
| fold leaf 2 into the tree (2/3) | [`t8tUYyZweco5pocF…`](https://explorer.solana.com/tx/t8tUYyZweco5pocF9zZnFaMHBPa8pp3ijjyF3bDXgJHMvX2xNuBuEYerP26iH5pReZB39ZawUZ3axkyCk9V3z44?cluster=devnet) |
| fold leaf 2 into the tree (3/3) | [`ESH9NZXFYnwVdgzX…`](https://explorer.solana.com/tx/ESH9NZXFYnwVdgzXcHD9vgXCh4eigmaDC45nZ8ZNrxSvJH7ZhQDaRvuxkqecTVkawKKxtnzamgSCJPSPYfDczUt?cluster=devnet) |
| relayer opens clean wallet account | [`67GGEYh64oSw4Jp8…`](https://explorer.solana.com/tx/67GGEYh64oSw4Jp8kiVxq3bJLNagkjf8ScjSVAkMCL9fZYCjHJRQjh5XttnZd8yLQCCUiTGqgq6aRK8oxMGn1Phv?cluster=devnet) |
| relayer opens an account of its own | [`k9D8882p6RZTE1r5…`](https://explorer.solana.com/tx/k9D8882p6RZTE1r54iQvhey4aauZHEytP4W5riKdAMtTMGSVNL8bkxdGy3vXquUmteTrs4CT3QzkBajwBQ6VK5G?cluster=devnet) |
| withdraw before verification finishes | refused: `0x1771 (ProofNotVerified: the verifier has not finalized this buffer)` |
| verifier: create + INIT buffer | [`3vpV9rxCr4UETG3x…`](https://explorer.solana.com/tx/3vpV9rxCr4UETG3x59ZDKqmENPjpPk6QkVykX1F7YDbepEzDEXBD9q7BWDvsVvrkcQ2DzeS2pgDrP6zqwae8jc4F?cluster=devnet) |
| verifier: SEAL (proof bytes frozen) | [`2JGJwakAxK4rohv6…`](https://explorer.solana.com/tx/2JGJwakAxK4rohv6RnkKouCBVhJPHziVvVVm59ZjHMg9KUAnnwXWfKP5ootnRnVTuzkmE44mkxfmHi4mutCVp4XN?cluster=devnet) |
| verifier: FINALIZE (both proofs verified) | [`2h6eEf5sEfkqXvdg…`](https://explorer.solana.com/tx/2h6eEf5sEfkqXvdgVmG9m87o7uoRYmT77qmt76kyZeTun7BH2RvAdYSLevKHk1q9jRrXRyn4stCMgSR79yEHdzhp?cluster=devnet) |
| verifier: all 213 transactions | buffer [`BXKbyy7ACgkmcn8onARf6ycdbHnE4rdfcpnpf7R8bviq`](https://explorer.solana.com/address/BXKbyy7ACgkmcn8onARf6ycdbHnE4rdfcpnpf7R8bviq?cluster=devnet) |
| relayer redirects payout to itself | refused: `0x1772 (ProofMismatch: the proofs are bound to another recipient)` |
| relayer withdraws to clean wallet | [`Sg7NfZ2zgYiApPmx…`](https://explorer.solana.com/tx/Sg7NfZ2zgYiApPmxko1HRnRQmv6CmWGG6RACVjQPcg8yZ9wYRzeeDgmDqLEmqHrKq2uwQ14L3YhHawryJ59t9Cw?cluster=devnet) |
| replay the same withdrawal | refused: `0x0 (AccountAlreadyInUse: this nullifier is already spent)` |
| member 0 also tries to exit | refused: `0x0 (AccountAlreadyInUse: this nullifier is already spent)` |
| exit member 2's note with member 1's secret | refused: `0x1778 (SecretMismatch: the secret does not open this deposit)` |
| member 2 exits (reveals its secret) | [`3QYDhzkquiENDEWe…`](https://explorer.solana.com/tx/3QYDhzkquiENDEWeN47YfLdx7CWfjuLuj8RHsmb6LBsbQoeok8sSwpSfm2WhNrgAkuCgCfjvgJGxEHZ4gEojb74B?cluster=devnet) |
| relayer closes the buffer (rent back) | [`3VFWxXbceSfWNc6T…`](https://explorer.solana.com/tx/3VFWxXbceSfWNc6TZ85iooyeSoMyirG4escZEWpnNQur3ZB6AVSghPnkD5oVQMTSBYeehYWwiXNKA5enCT5B4VoJ?cluster=devnet) |
