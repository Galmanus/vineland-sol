# Vineland on Solana: local validator run

- cluster: `http://127.0.0.1:8899` (a `solana-test-validator` on a laptop, since stopped: the signatures below existed only there and do not resolve on any public cluster; the devnet run will replace them with checkable ones)
- program: `AsQwEN7XiUPW2MGHEbEkh8UCKixwpSBfE1KKt5QVgYgx`
- pool: `HZDHxhFyLd4ALDyyS4QTurL7V7RkAfkqsRZp36RuwNxE`
- test dollar mint: `7wTai1d7tcGAVYxYP2MSKfEnck4dRT4NcdD9epKFK2xp` (6 decimals, devnet only)
- note: 1000000 base units, fee 197 bps, k_min 3, committee 2-of-2
- relayer: `8BTKY6VSFL1AbhDNrnbVqb8oU6qXfio7npaLsw47v1MB`
- clean wallet: `3RSvxZwNF4y4wp8hhGEAvNDgnrUW5fCC8yJuvFePmZPH` (never funded; received 980300)
- withdrawal: [2MYDVXmNMBLnrASwWtxnFbuYv6KeVmVPBHMs5wuJNeVA23E8dgnc5zELaDk5HWjewhumTnExxNAWoqTUSeGVyR9w](https://explorer.solana.com/tx/2MYDVXmNMBLnrASwWtxnFbuYv6KeVmVPBHMs5wuJNeVA23E8dgnc5zELaDk5HWjewhumTnExxNAWoqTUSeGVyR9w?cluster=custom&customUrl=http://127.0.0.1:8899)
- proofs: binding 113397 B + membership 82723 B, proved in 2899 ms, verified in 11 ms per verifier (this machine)
- final state: leaf_count 3, live_notes 1, exits 1

| step | signature |
|---|---|
| fund roles | [`8osVb66QLouamNCT…`](https://explorer.solana.com/tx/8osVb66QLouamNCTNGtvkuVucircs2vNrMBRZRJMvQ5TxXGC9ZC7qjfE9jwH3k9bGQWHw2L5gs18i26NsfzTJZB?cluster=custom&customUrl=http://127.0.0.1:8899) |
| create test dollar + treasury | [`3Zk98QXRgjru1hVt…`](https://explorer.solana.com/tx/3Zk98QXRgjru1hVt47xiq8WfUkUmbqn1WgyqUztcRfxCQJGVgPg3Y9XGqQo7qffmkvVrHzoxooVB1KbBQaBny3d9?cluster=custom&customUrl=http://127.0.0.1:8899) |
| initialize pool | [`4a8eWRBChQcB3EKN…`](https://explorer.solana.com/tx/4a8eWRBChQcB3EKN4vcARnYNd8ZxUTET5kgTzgRKAppR4MG3QvbJQRX9xxtTjudXcacPvLFZYj4rC4QjsGiMaTxh?cluster=custom&customUrl=http://127.0.0.1:8899) |
| member 0: token account + 1 note | [`ZU6YbxuVjNxTky2W…`](https://explorer.solana.com/tx/ZU6YbxuVjNxTky2WL1ywobBr89qUz83J4xHs8MpjwNngBnN8MVPYSf6dPxDTDGspMLfMBkdY2nLViGMPnHZDaw7?cluster=custom&customUrl=http://127.0.0.1:8899) |
| deposit before approval | refused: `0xbc4 (AccountNotInitialized: no allowlist entry for this key)` |
| operator approves member 0 | [`57gnT4muMQmcvGo6…`](https://explorer.solana.com/tx/57gnT4muMQmcvGo6LMQJRCSzifgqziUgkCUujrPG7dcUyHgjMhq5MyuT2yNf72tMqe73BV3fBeQkDRSL8mkBJja2?cluster=custom&customUrl=http://127.0.0.1:8899) |
| member 0 deposits (signs) | [`5HEcrzHHF99YbGm3…`](https://explorer.solana.com/tx/5HEcrzHHF99YbGm3b9ZFYhz5E2dG9HfXdEg8KtaCUNfv4K5ZtbtJY8vLNAuAbStn2KxjbGY4uMrxiLBVAUq92P5c?cluster=custom&customUrl=http://127.0.0.1:8899) |
| member 1: token account + note + approval | [`2AmsSzsENx1D8wFf…`](https://explorer.solana.com/tx/2AmsSzsENx1D8wFfkeFPYLxCQ5cRxxwDVZHFttupMd5wnBJnGAhkuW5uzuNxCkYHdBuPC6FP7nMErzTFmWk22fiM?cluster=custom&customUrl=http://127.0.0.1:8899) |
| member 1 deposits (signs) | [`648rxMhdKi7HT8Db…`](https://explorer.solana.com/tx/648rxMhdKi7HT8DbNu3CivX95ncngpdJ8TGXNYyy65vTf4N7Q3g3HE7NSkKv3xJFqFaiDHCraiQWxGNenAWHgLia?cluster=custom&customUrl=http://127.0.0.1:8899) |
| member 2: token account + note + approval | [`4ZRhJm7MKYZV8PTs…`](https://explorer.solana.com/tx/4ZRhJm7MKYZV8PTs3ubJ3mLADXLTJXfswwuYXaK5XKbp92q33h75ZKkZVUj4REKTwM67xRsKRRVmpusBoanUCePP?cluster=custom&customUrl=http://127.0.0.1:8899) |
| member 2 deposits (signs) | [`5CThrryiM6b8GcAX…`](https://explorer.solana.com/tx/5CThrryiM6b8GcAXpsEtJZsCmMyrepENQx8mUq3pPZAHEExcnjAHLXmmU4UPXPrvXVkQhY25nC8R7sNYQcJm7Z9d?cluster=custom&customUrl=http://127.0.0.1:8899) |
| publish root | [`84scyGebpFoaSHoZ…`](https://explorer.solana.com/tx/84scyGebpFoaSHoZC192wMfcSgGH2jHwvnnLtdeGFkhYu41s7SVN1N4e4EvEHx2dKqTEKyj4HzA6rmV8StmX9uB?cluster=custom&customUrl=http://127.0.0.1:8899) |
| relayer opens clean wallet account | [`5XJPNiQ1HdufnFcr…`](https://explorer.solana.com/tx/5XJPNiQ1HdufnFcrk7rDjfSS1oLexVcKXfBeEvNjosKos3GZwcZud15PK63sSSHxYGYMZMiCwnQU7bDunVvGt5qB?cluster=custom&customUrl=http://127.0.0.1:8899) |
| relayer withdraws to clean wallet | [`2MYDVXmNMBLnrASw…`](https://explorer.solana.com/tx/2MYDVXmNMBLnrASwWtxnFbuYv6KeVmVPBHMs5wuJNeVA23E8dgnc5zELaDk5HWjewhumTnExxNAWoqTUSeGVyR9w?cluster=custom&customUrl=http://127.0.0.1:8899) |
| replay the same withdrawal | refused: `0x0 (AccountAlreadyInUse: this nullifier is already spent)` |
| member 0 also tries to exit | refused: `0x0 (AccountAlreadyInUse: this nullifier is already spent)` |
| member 2 exits (public refund) | [`4ySaq9d36dn966Tn…`](https://explorer.solana.com/tx/4ySaq9d36dn966TnE6UFKkH4LNepMEsyPwy9ThTPGGGxYXqnqcTVuJjWZuLufvJqLGsyejnZNWZ9bVYH2yJ2FhJr?cluster=custom&customUrl=http://127.0.0.1:8899) |
| withdraw below the k floor | refused: `0x1776 (AnonymitySetTooSmall: the set under the root is below k_min)` |
