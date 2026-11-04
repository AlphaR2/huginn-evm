# Huginn demo: a Solana transaction, proven on an EVM chain

What you show:
1. a memo is sent on Solana;
2. it is proven final and included by Solana's validators' own signatures;
3. an EVM contract accepts the proof and remembers the transaction;
4. changing one byte of the memo makes the contract refuse.

## Before recording

In `huginn`, three things:

**1. Start the local Alpenglow validator with the certificate plugin**, and leave it running:

```bash
solana-test-validator --alpenglow --ledger .local-ledger \
  --geyser-plugin-config plugins/footer-dump/config.json
```

**2. Build ledger-dump**, once:

```bash
cargo build --release -p ledger-dump
```

**3. Make sure the validator set file exists for the current epoch.** Epoch 0 is already in `fixtures/local`.

Then in `hugin-evm`:

```bash
cd app && cargo build --release && cd ..
./app/target/release/huginn-evm
```

Make the terminal at least 150 × 34 characters.

## The run (mock proofs, ~10 s)

Press `e` and type a memo, e.g. `gm from solana`, then `enter`. Press `enter` again to run everything. What each step shows:

| # | Step | What you should see |
|---|---|---|
| 1 | Check tools | ✓ for solana, forge, cast, anvil, ledger-dump, validator, footers. `!` for Docker/circuits is fine in mock mode |
| 2 | Send memo on Solana | `$ solana transfer … --with-memo "gm from solana"`, then the signature |
| 3 | Wait for finality | `finalized in slot N` |
| 4 | Fetch certificate + shreds | `certs for N and N-1 verify`, `computed block_id` equal to `certified block_id`, `MATCH` |
| 5 | Prove with SP1 | 1 validator, 32 shreds; ~5.1M cycles (3.9M finality, 0.6M inclusion); program key `0x000482e7…` |
| 6 | Start EVM chain | anvil, chain 31337 |
| 7 | Deploy light client | the `SolanaLightClient` address |
| 8 | Submit proof | `verified Solana slot N` and the block_id; ~105k gas |
| 9 | Ask the contract | `isVerified(signature) = true` |
| 10 | Forged proof rejected | the real output is accepted; the memo with one character changed gets `rejected: InvalidProof` |

The three boxes at the top fill in as you go: Solana (tx, slot, finalized), proof (mode, cycles, time) and EVM (client, gas, verified).

## Talking points

- **Nothing is trusted but Solana's validators.** The proof checks:
  - their BLS signatures;
  - that they hold over 80% of the stake (fast path);
  - the block_id rebuilt from the shreds;
  - the transaction's bytes inside the block.
- **Mock vs real.** In mock mode SP1 runs the same program but makes no proof. Press `m` for `compressed`: a real STARK proof, ~5 minutes on a laptop. Real chains take `groth16`: ~260 bytes, ~270k gas to verify on Base.
- **Inclusion is not success.** A failed Solana transaction is in its block too. Step 3 says if it failed; the contract sees the raw transaction.
- **Today's trust gap.** The client's owner stores each epoch's validator set hash. Proving the set handover from one epoch to the next is planned.

## Without the TUI

```bash
./app/target/release/huginn-evm run --memo "gm from solana"
```

The same steps, printed one after another.
