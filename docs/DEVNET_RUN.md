# Vineland on Solana: devnet run

- cluster: `https://api.devnet.solana.com`
- program: `AsQwEN7XiUPW2MGHEbEkh8UCKixwpSBfE1KKt5QVgYgx`
- pool: `9QH8aQoMQXtdUnzCtBKE4NwnFoeDh2xQvNJPaL4zSC9B`
- test dollar mint: `8oJY7Xaovthq44H8dG5AXmbP8p7RTxTaD4uFSBnErvf4` (6 decimals, devnet only)
- note: 1000000 base units, fee 197 bps, k_min 3, committee 2-of-2
- relayer: `BrZ3TGUZMoZVFPAfXApkAHcTHoJBzidP2AHHuDQX8CZ8`
- clean wallet: `FJdwfsLTSigw9o45Wr9cyvfvBz9yFsAtftQLxt9Z9aY4` (never funded; received 980300)
- withdrawal: [vWdahWzP15M12JkXHqbSN22Abh9SRFUH2fWGfA6vChCqqexiLxr2zcxUTHQvAgrfuQwtLARmx1XSEBtrbBtX38E](https://explorer.solana.com/tx/vWdahWzP15M12JkXHqbSN22Abh9SRFUH2fWGfA6vChCqqexiLxr2zcxUTHQvAgrfuQwtLARmx1XSEBtrbBtX38E?cluster=devnet)
- proofs: binding 113410 B + membership 82754 B, proved in 2434 ms, verified in 5 ms per verifier (this machine)
- final state: leaf_count 3, live_notes 1, exits 1

| step | signature |
|---|---|
| fund roles | [`2YDCB1YqFT6cKXPU…`](https://explorer.solana.com/tx/2YDCB1YqFT6cKXPUb8r1ZGykmR5LJ7JSyvoxgMVcZim5TbPoNts5F7h7YoQiKnRKVDd7jJwxduH5eNkw5eGV4DGa?cluster=devnet) |
| create test dollar + treasury | [`5UzDkxwaEb5uS8D7…`](https://explorer.solana.com/tx/5UzDkxwaEb5uS8D71GfazqoEW6Wki6LdF7oG2SG5PbFHiLcAWurcuMp6p3Hji8jxmeDPoKziZKDKQ2zEJoFu9EqG?cluster=devnet) |
| initialize pool | [`5mT2QrgENBWzBf83…`](https://explorer.solana.com/tx/5mT2QrgENBWzBf83BAiMErd4us4ywugu7Dqjwr4ExuRXRek2wMzTqSiBuLc85hLK4orF6nBJeMzmH7XHYEJkWHvf?cluster=devnet) |
| member 0: token account + 1 note | [`4cca4q7gR5ZencKc…`](https://explorer.solana.com/tx/4cca4q7gR5ZencKceSKyLrHJZK67Fr3xnN4rugzSURLWJ4VbspvDzTGjvSiUTZf4beujHdjtSv3oY5afMHDLMTjH?cluster=devnet) |
| deposit before approval | refused: `0xbc4 (AccountNotInitialized: no allowlist entry for this key)` |
| operator approves member 0 | [`4YkzFXJGVBoK16FZ…`](https://explorer.solana.com/tx/4YkzFXJGVBoK16FZ298DL6bidXLKazRJ9EeQEF9NXWFc5obnDnuWAYJFz9wiKNMSLHkahn6BAwGXbU5Ns6inFtE7?cluster=devnet) |
| member 0 deposits (signs) | [`3dju9WMAwpAnh9Ss…`](https://explorer.solana.com/tx/3dju9WMAwpAnh9Ss6RUz2947MAXoh6BnCvQ7DuzeSyY7yD4F8gHZr2Hz2kGsWGQJRrHh57rgnx5jnxtMwZSSjUC3?cluster=devnet) |
| member 1: token account + note + approval | [`2ho4ShLEo7c8nj7t…`](https://explorer.solana.com/tx/2ho4ShLEo7c8nj7tPFuEzMMzCRdLZFbQy3MCPLstRFRwvtHq2RB3tgfu4A2Mynk7KyBfWZyfB8F1EYPviFXvQhN1?cluster=devnet) |
| member 1 deposits (signs) | [`4bWGa4DtFWJWb716…`](https://explorer.solana.com/tx/4bWGa4DtFWJWb716tFyg5dnJC3ui4j9rF675443qd2mJUU4g8XmoXmN6qHV9F1qedL1ZcavfFdW3a8jkTSygxqcF?cluster=devnet) |
| member 2: token account + note + approval | [`JBtwpm4E3xwpXxpi…`](https://explorer.solana.com/tx/JBtwpm4E3xwpXxpiGju7YWd2nxMep9nrTsMbzwuntQir3SfYwX2Unikb21hqbMaksiHCw1FTscRHVEvWfdp9KX6?cluster=devnet) |
| member 2 deposits (signs) | [`4oSSCHPewMnH5zF5…`](https://explorer.solana.com/tx/4oSSCHPewMnH5zF5fTadxo9L5YwaGeESrLXSnJWnHJuUYU7ybey3uMxH4orT5PybuoomboqtThdUwkZvn2XPobhR?cluster=devnet) |
| publish root | [`5LYgdPo9SBBwExDr…`](https://explorer.solana.com/tx/5LYgdPo9SBBwExDrce1Lixuw1jjQ9xvMaJGoZ5EAAaq4soizuGh5PVubXkE6xNYgf3JyCimEgEyYze5zzcCrHco8?cluster=devnet) |
| relayer opens clean wallet account | [`tDrUXEXWtNDc7SNH…`](https://explorer.solana.com/tx/tDrUXEXWtNDc7SNHD4MMrEcKgnNBZzQ2fs8FvcY16AJ6PD2CdhCSwLnYd2h873b8JmGct46c1vH3gQsPbVeCKLp?cluster=devnet) |
| relayer withdraws to clean wallet | [`vWdahWzP15M12JkX…`](https://explorer.solana.com/tx/vWdahWzP15M12JkXHqbSN22Abh9SRFUH2fWGfA6vChCqqexiLxr2zcxUTHQvAgrfuQwtLARmx1XSEBtrbBtX38E?cluster=devnet) |
| replay the same withdrawal | refused: `0x0 (AccountAlreadyInUse: this nullifier is already spent)` |
| member 0 also tries to exit | refused: `0x0 (AccountAlreadyInUse: this nullifier is already spent)` |
| member 2 exits (public refund) | [`3ZoX9mqgL4o1MQmu…`](https://explorer.solana.com/tx/3ZoX9mqgL4o1MQmurvDL9S9RiVBgGA3ukesvLNp7MHcgXhC9HUXsSwgrzpfSa3ai5H5Vtx8StHzymh4xkaQVyq3s?cluster=devnet) |
| withdraw below the k floor | refused: `0x1776 (AnonymitySetTooSmall: the set under the root is below k_min)` |
