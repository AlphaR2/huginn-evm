# Huginn demo: a Solana transaction, proven on an EVM chain

What you show:
1. a memo is sent on Solana;
2. it is proven final and included by Solana's validators' own signatures;
3. an EVM contract accepts the proof and remembers the transaction;
4. a contract that only your Solana key controls changes its greeting to your memo;
5. changing one byte of the memo makes the contract refuse.

## Before recording

In `huginn`, three things:

**1. Start the local Alpenglow validator**, and leave it running:

```bash
solana-test-validator --alpenglow --ledger .local-ledger
```

The footer-dump plugin is optional: certificates are read from the shreds. To also show the plugin source (`s` in the TUI), add `--geyser-plugin-config plugins/footer-dump/config.json`.

**2. Build ledger-dump**, once:

```bash
cargo build --release -p ledger-dump
```

**3. Make sure the validator set file exists for the current epoch.** Epoch 0 is already in `fixtures/local`.

Then in `huginn-evm`:

```bash
cd app && cargo build --release && cd ..
./app/target/release/huginn-evm
```

Make the terminal at least 150 × 34 characters.

## The run (mock proofs, ~10 s)

Press `e`, type a message (e.g. `gm from solana`), then `enter` to save it. Press `enter` again to start. The four stages fill in, and the card underneath tells the story in plain sentences. At the end it says whether the contract trusts your message.

To show what's under the hood, press `d` at any time: every step, the exact command it ran, and its output. What each step shows there:

| # | Step | What you should see |
|---|---|---|
| 1 | Check tools | ✓ for solana, forge, cast, anvil, ledger-dump, validator, footers. `!` for Docker/circuits is fine in mock mode |
| 2 | Send memo on Solana | `$ solana transfer … --with-memo "huginn:31337:greeter:gm from solana"`, then the signature |
| 3 | Wait for finality | `finalized in slot N` |
| 4 | Fetch certificate + shreds | `certs for N and N-1 verify (… from the shreds' footers)`, `computed block_id` equal to `certified block_id`, `MATCH` |
| 5 | Prove with SP1 | 1 validator, 32 shreds; ~5.1M cycles (3.9M finality, 0.6M inclusion); the program key |
| 6 | Start EVM chain | anvil, chain 31337 |
| 7 | Deploy light client | the `SolanaLightClient` address |
| 8 | Submit proof | `verified Solana slot N` and the block_id; ~105k gas |
| 9 | Ask the contract | `isVerified(signature) = true` |
| 10 | Deploy Solana-controlled app | `SolanaGreeter` deployed with your Solana key as controller; greeting `"gm"` |
| 11 | Run the Solana command | the same proof handed to `SolanaGreeter.execute`; greeting becomes your memo |
| 12 | Forged proof rejected | the real output is accepted; the memo with one character changed gets `rejected: InvalidProof` |

In the details view, the three boxes at the top fill in as you go:
- **Solana:** tx, slot, finalized, memo;
- **proof:** mode, cycles, time, program key;
- **EVM:** client, gas, verified, greeting.

`d` (or `esc`) goes back.

## Talking points

- **Nothing is trusted but Solana's validators.** The proof checks:
  - their BLS signatures;
  - that they hold over 80% of the stake (fast path);
  - the block_id rebuilt from the shreds;
  - the transaction's bytes inside the block.
- **No special infrastructure.** The certificate that finalized block N is inside block N+1's footer. So the shreds are all Huginn needs: no plugin and no paid feed.
- **Practice vs real.** A practice proof runs the same program in SP1 but makes no proof. Press `m` for `real (STARK)`, ~5 minutes on a laptop. Real chains take `real (Groth16)`: ~260 bytes, ~270k gas to verify on Base.
- **A Solana wallet controls an EVM contract.** `SolanaGreeter` acts only on transactions your Solana key signed. The memo names the chain and the contract (`huginn:31337:greeter:…`), so it can't be replayed elsewhere, and each transaction runs once.
- **Inclusion is not success.** A failed Solana transaction is in its block too. Step 3 says if it failed; the contract sees the raw transaction.
- **Today's trust gap.** The client's owner stores each epoch's validator set hash. Proving the set handover from one epoch to the next is planned.

## Without the TUI

```bash
./app/target/release/huginn-evm run --memo "gm from solana"
```

The same steps, printed one after another.
