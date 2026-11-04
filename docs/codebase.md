# huginn-evm codebase guide

What every file does, in the order it's worth reading. huginn has its own guide (`../huginn/docs/codebase.md`) for the Solana side and the proof.

---

## 1. The whole thing in one picture

```
SOLANA (local validator)              huginn (dependency)                    EVM chain (anvil / Base)
────────────────────────              ───────────────────                    ────────────────────────
your memo transaction                 huginn-prover:                         SolanaLightClient
   │ finalized by validators            checks certificate + shreds            verify(output, proof)
   ▼                                    inside SP1, outputs                    ├─ SP1 verifier says valid
ledger (shreds)  ── ledger-dump ──▶     "block X final, tx T in it"  ──▶       ├─ right program, cluster,
                    (fixture)           + a proof                              │  validator set
                                                                               └─ remembers block + tx
                                                                                      │
                                                                               SolanaGreeter
                                                                               (only your Solana key)
```

This repo is the right-hand column, plus the app (`app/`) that drives all three columns from one screen.

**What the contracts trust.** The SP1 proof guarantees four things:
- the BLS signatures;
- that the signers hold enough stake;
- that the block_id was rebuilt from the shreds;
- that the transaction's bytes are in the block.

The contracts add three checks: the proof is from **our** program, from **this** Solana cluster, and against **the stored** validator set for that epoch.

**What you must trust today.** The owner stores each epoch's validator set hash.

---

## 2. Suggested reading order

1. `src/HuginnOutput.sol`: what a proof *says* (60 lines).
2. `src/SolanaLightClient.sol`: how a contract *accepts* it.
3. `src/SolanaTx.sol`: how a contract reads the Solana transaction inside it.
4. `src/SolanaControlled.sol` and `src/examples/SolanaGreeter.sol`: a contract a Solana key controls.
5. `test/`: the attacks the contracts refuse.
6. `script/`: deploy and submit from `.env`.
7. `app/src/steps.rs`: every step of the flow, as code.
8. `app/src/tui.rs`: the screen.

---

## 3. Top-level files

| File | What it's for |
|---|---|
| `README.md` | What this is, the TUI, setup, running on a real chain |
| `.env.example` | Every setting, explained. Copy it to `.env` (git-ignored). An empty `PRIVATE_KEY` uses anvil's public test key, only on a chain whose id is 31337 |
| `foundry.toml` | solc 0.8.28; tests may read `test/fixtures`, scripts may read `proofs/`; one lint switched off (reverting inside a parser loop is intended) |
| `proofs/` | Where the app writes block fixtures, `proof.json` and `state.json`. Git-ignored |
| `lib/` | `forge-std`, installed with `forge install foundry-rs/forge-std --no-git`. Git-ignored |
| `docs/demo.md` | The demo for the video, step by step |

---

## 4. `src/`: the contracts

### `ISP1Verifier.sol`
One function: `verifyProof(programVKey, publicValues, proof)`. Succinct deploys this on each chain; it reverts unless the proof is valid **for that program** and **for exactly those output bytes**.

### `HuginnOutput.sol`: reading what a proof says
The proof's output is plain bytes at fixed offsets, the same layout huginn's `TxOutput::encode` writes. The full table is at the top of the file:

| Offset | Field |
|---|---|
| 0 | version |
| 1 | validator set hash |
| 33 | shred version |
| 35 | slot |
| 43 | block_id |
| 75 | path |
| 76 | signed stake |
| 84 | total stake |
| 92 | where the transaction sits in the block (three numbers) |
| 104 | transaction length |
| 106 | the transaction's raw bytes |

`decode` checks the version byte and that the length is exact, then reads the fields. It's not Solidity ABI on purpose: any chain can read this layout.

### `SolanaLightClient.sol`: the contract apps call
- **Fixed at deploy:** the SP1 verifier, our program key, this cluster's shred version, and slots per epoch.
- **Stored:** each epoch's validator set hash (owner-only, write-once per epoch), every finalized block seen (`slot → block_id`), and every transaction submitted (`keccak(signature) → slot`).
- **`verify(output, proof)`** writes nothing. It:
  1. asks the SP1 verifier whether the proof is valid;
  2. decodes the output;
  3. checks the shred version;
  4. checks the set hash against the one stored for `slot / slotsPerEpoch`;
  5. refuses a block_id that differs from one already submitted for that slot (`ConflictingBlock`: that would mean Solana itself broke);
  6. returns the decoded output.
- **`submit(output, proof)`** runs `verify`, then records the block and the transaction, so `isVerified(signature)` works later without a proof.
- `setOwner(0)` is refused: without an owner, nobody could store the next epoch's set.

### `SolanaTx.sol`: reading a Solana transaction
The Solidity twin of huginn's `tx.rs`. `parse(raw)` returns signatures, header, account keys, blockhash, instructions and v0 lookups. Helpers:
- `signer(i)` and `isSigner(key)`: who signed;
- `programId(ix)`: which program an instruction called;
- `account(ix, j)`: which accounts it used;
- `firstSignature(raw)`: the transaction's id.

Every read is bounds-checked. Lengths must use the shortest encoding, and every byte must be used. A count is checked against the bytes left *before* any memory is allocated, so junk input reverts cleanly instead of burning gas.

### `SolanaControlled.sol`: contracts a Solana key controls
An abstract base. The controller sends a Solana transaction with a memo: `huginn:<chain id>:<contract address>:<nonce>:<command>`. Anyone can relay its proof to `execute`, which runs the command if all of these hold:
- the proof is valid (via the light client);
- the controller signed the transaction;
- the memo names **this chain** and **this contract's address** (any letter case), so it can't run on another chain or another deployment;
- the nonce is exactly `nonce + 1`, so commands run once each and in the order the controller numbered them. A relayer can delay a command, never reorder it.

`memoPrefix()` returns `huginn:<chain id>:<address>:`, which is what tools put before the nonce. An empty command uses up its nonce and runs nothing: that's how the controller cancels a command it signed but nobody relayed.

Your contract implements `_execute(command)`. Note in the file: a *failed* Solana transaction still counts, because authority comes from the signature, not from success. Don't build commands that depend on the Solana side succeeding.

### `examples/SolanaGreeter.sol`
The smallest `SolanaControlled`: a `greeting` that only the controller's memos can change.

### `mocks/BindingVerifier.sol`
**Not a verifier.** A stand-in for local chains until a Groth16 proof exists. Its "proof" is a hash of (program key, output), so changing the output without "re-proving" fails, just like the real thing. But anyone can make such a proof, so `Deploy.s.sol` refuses to deploy it anywhere but anvil (chain 31337).

---

## 5. `test/`: what the contracts refuse

39 tests (plus 5 Groth16 tests skipped until a real proof exists): `forge test -vv`.

| File | Covers |
|---|---|
| `SolanaLightClient.t.sol` | the **real** output of our proven memo transfer decodes field by field. Refused: one changed memo byte, another program's proof, another cluster, an unknown epoch, a wrong set, conflicting blocks, a non-owner storing sets, rewriting a set, a zero owner, bad framing. Plus a gas measurement |
| `SolanaTx.t.sol` | the real transaction parsed exactly (0.5 SOL transfer + memo). Refused: every possible truncation, trailing bytes, non-canonical lengths, unknown versions. v0 lookups parse. 256 random inputs never panic |
| `SolanaGreeter.t.sol` | transactions built byte by byte. The controller changes the greeting. Refused: a stranger, a replay, commands out of order, malformed nonces, a memo for another chain, a memo for another deployment, a forged command. The address works in any letter case; an empty command cancels; a 200-byte command works |
| `Adversarial.t.sol` | an outside review's attacks (huginn's `docs/adversarial-report.md`). `test_FIXED_*` assert a fixed attack now fails; `test_POC_*` still show an open one (the owner's power over validator sets) |
| `fixtures/` | copies of huginn's proven output and the transaction's RPC copy |

---

## 6. `script/`: Foundry scripts driven by `.env`

| Script | Does |
|---|---|
| `Deploy.s.sol` | deploys `SolanaLightClient`; on anvil with no `SP1_VERIFIER`, first deploys the stand-in verifier |
| `Submit.s.sol` | reads `PROOF_FILE` (`proofs/proof.json`) and calls `submit`. A proof with no on-chain bytes (practice/STARK) only works on anvil |
| `DeployGreeter.s.sol` | deploys `SolanaGreeter` for `SOLANA_CONTROLLER` (a Solana public key as 32 bytes hex) |
| `Execute.s.sol` | relays a proof to `GREETER`'s `execute` (`PUBLIC_VALUES`, `PROOF`) |

Every script reads `PRIVATE_KEY` from the environment, never from a command-line argument (other users of the machine can see those).

---

## 7. `app/`: `huginn-evm`, the TUI and the steps

A Rust crate that depends on huginn's `huginn-prover`. It has its own workspace, because the SP1 SDK is large.

### `src/main.rs`: the entry point
- `huginn-evm` opens the TUI.
- `huginn-evm run [steps…]` runs steps without the screen, printing each step's log.
- `huginn-evm vkey` prints the program key.
- Flags: `--memo`, `--mode mock|compressed|groth16` (`mock` is what the TUI calls "practice"), `--certs shreds|plugin`, `--huginn <path>`.

It must run from this repo, because it reads `.env` and `script/`.

### `src/steps.rs`: the flow, as code
- **`StepId`**: the 12 steps, each with a name, a title and a one-line explanation:
  doctor → anvil → deploy → greeter → send → finalize → fetch → prove → submit → check → command → forged.
  The contracts come first because the memo names the greeter's address and next nonce.
- **`Settings`**: `.env.example` with `.env` on top, plus memo, proof mode and certificate source.
- **`State`**: what the flow has found so far (signature, slot, proof file, contract addresses, gas, greeting). It's saved to `proofs/state.json` after every step, so `run submit` works on its own later.
- **`Runner::run(step)`** runs one step and reports it as events (`Started`, `Log`, `Done`, `State`, `Finished`). The TUI and `run` both listen to the same events.
- **The steps themselves** mostly run the tools you would run by hand: `solana transfer`, `ledger-dump`, `forge script`, `cast call`. That way each logged command is copyable. Only `deploy` (the program key) and `prove` call the library directly (`huginn_prover::Huginn`).
- **Keys:** `PRIVATE_KEY` from `.env`. With none set, anvil's public test key, but only if the chain answers `cast chain-id` with 31337. The key reaches forge through the environment.
- **Helpers:**
  - `cmd` runs a command and streams its output into the log;
  - `forge` keeps only the script's own logs;
  - `proof_bytes` gets the bytes the verifier takes (the stand-in's when the proof has none);
  - `extend_path` finds tools installed by sp1up, foundryup and the Solana installer.

### `src/tui.rs`: the screen
Two views of the same run:
- **Guided (default):** your message, five stages (Set up, Send, Finalize, Prove, Verify) mapped onto the 12 steps by `STAGES`, and plain sentences.
  - `doing_text` says what's happening now;
  - `done_text` says what happened;
  - `failure_help` says what's wrong and the fix;
  - one result card at the end, which says what was really shown: a practice run proves nothing, a STARK is checked on your machine, only Groth16 is checked by the contract.
- **Details (`d`):** every step with its exact command and output, and the three boxes (Solana, proof, EVM).
- **Help (`?`):** the keys and the two settings (`m` proof, `s` signature source).

The run happens on a background task; the screen redraws every 80 ms from the events it receives. *Tests:* `snapshot` and `guided_moments` render the screen into text (`cargo test --release -- --nocapture guided`), which is how layout changes are checked without a terminal.

---

## 8. Words you'll meet

| Word | Meaning |
|---|---|
| **public values / output** | the bytes a proof commits to: what it proves (`HuginnOutput`) |
| **program key (vkey)** | the fingerprint of huginn's SP1 program. Any code change gives a new one, and contracts pin the one they trust |
| **stand-in verifier** | `BindingVerifier`, for anvil only |
| **practice proof** | SP1's mock mode: every check runs, but no proof is made, so nothing is proven |
| **controller** | the Solana public key a `SolanaControlled` contract obeys |
| **target** | a contract's name in memos (`greeter`) |
| **shred version** | a number that tells Solana clusters apart (28090 = the local validator) |
