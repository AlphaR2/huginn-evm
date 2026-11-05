# huginn-evm

Use Solana transactions inside EVM contracts (Base first, any EVM chain the same way), with
proofs made by [huginn](https://github.com/AlphaR2/huginn).

huginn does the Solana side: it proves *"this transaction is in a block that Solana's
Alpenglow validators finalized"*, as one SP1 proof with a fixed-layout output. This repo is
one **destination** for those proofs:

```
huginn (dependency)                      huginn-evm (this repo)
────────────────────                     ──────────────────────────────────────────────
huginn-prover crate  ──▶ app/ (huginn-evm) ──▶ script/Deploy, Submit ──▶ SolanaLightClient
  (SP1 program,          TUI + subcommands:      forge sends to             checks the proof,
   Alpenglow checks)     send, fetch, prove      your chain                 stores block + tx;
                                                                            your contracts read it
```

## The TUI

```bash
cd app && cargo build --release && cd ..
./app/target/release/huginn-evm
```

Type a message, press enter, and watch it travel through five stages:

```
✓ Set up ─── ✓ Send ─── ✓ Finalize ─── ⠋ Prove ─── ○ Verify
```

Underneath, plain sentences say what's happening and what happened. A result card at the end says what was really shown: a practice run proves nothing, a real STARK is checked on your machine, and only a Groth16 proof is checked by the contract.

| Key | Does |
|---|---|
| `enter` | send your message and run everything |
| `e` | write a new message |
| `d` | details: every step, the exact command it ran, and its output (`enter`/`r`/`n` run steps from there) |
| `?` | help, including the two settings: |
| `m` | proof: `practice` (seconds, no real proof), `real (STARK)` (~5 min), `real (Groth16)` (for real chains) |
| `s` | where the validators' signatures come from: the next block's shreds (default, no plugin), or the footer-dump plugin |

The same steps run without the TUI (scripts, CI):

```bash
./app/target/release/huginn-evm run                       # all
./app/target/release/huginn-evm run send finalize fetch   # some, in order
./app/target/release/huginn-evm run --mode compressed --memo "gm"
./app/target/release/huginn-evm vkey                      # PROGRAM_VKEY
```

The flow's state is saved to `proofs/state.json` after every step, so steps can be run one at a time.

## Layout

```
app/                        huginn-evm: TUI (src/tui.rs), the steps (src/steps.rs), subcommands (src/main.rs)
src/SolanaLightClient.sol   verify(publicValues, proof) / submit(...): the contract apps call
src/HuginnOutput.sol        reads the proof's output (same offsets as huginn's TxOutput::encode)
src/SolanaTx.sol            reads a Solana transaction: signers, programs, accounts, data
src/SolanaControlled.sol    base for contracts a Solana key controls (memo commands, no replay)
src/examples/SolanaGreeter  the smallest one: a greeting only a Solana key can change
src/ISP1Verifier.sol        Succinct's verifier interface
src/mocks/BindingVerifier   local stand-in verifier (anvil only, never a real chain)
script/Deploy.s.sol         deploy SolanaLightClient with the settings in .env
script/Submit.s.sol         submit proofs/proof.json
script/DeployGreeter.s.sol  deploy SolanaGreeter for a Solana key
test/                       Foundry tests on the real proven memo transfer from huginn's local chain
docs/demo.md                the demo, step by step, with what you should see
docs/codebase.md            every file explained, in reading order
.env.example                every setting, explained
```

## Setup

You need:
- [Foundry](https://getfoundry.sh), Rust, the SP1 toolchain (`sp1up`) and `protoc`;
- the Solana CLI (Agave 4.4, for `solana-test-validator --alpenglow`);
- huginn checked out next to this repo (`../huginn`; step by step in huginn's
  [`docs/run.md`](https://github.com/AlphaR2/huginn/blob/main/docs/run.md)), with its local Alpenglow validator
  running and `ledger-dump` built (see huginn's README). The footer-dump plugin is optional:
  certificates are read from the shreds by default. Or switch `app/Cargo.toml` to huginn's
  git dependency;
- Docker, only for Groth16 proofs made on your own machine.

```bash
cp .env.example .env          # optional on anvil: an empty PRIVATE_KEY uses anvil's test key (chain 31337 only)
forge install foundry-rs/forge-std --no-git
forge test -vv
```

## Run it on a real chain (Base Sepolia, Base, ...)

1. Set `RPC_URL`, `PRIVATE_KEY` and `SP1_VERIFIER` (Succinct's gateway on that chain) in `.env`.
2. Set `SHRED_VERSION`, `EPOCH`, `VALIDATOR_SET_HASH` for the Solana cluster you follow.
3. `./app/target/release/huginn-evm run --mode groth16`, or press `m` until groth16 in the TUI.

## Status (2026-11-04)

- 49 Foundry tests pass: the real proven memo transfer from huginn's local chain, the
  transaction parser (legacy, v0 and v1), the Solana-controlled greeter, and the outside review's attacks
  (`test/Adversarial.t.sol`: fixed ones assert the attack fails, open ones still show it).
- The full TUI flow runs on fresh transactions in ~10 s with practice proofs, and end to end
  with a real STARK proof (~5 min).
  - The steps: send, finalize, fetch, prove, deploy, submit, `isVerified` true.
  - The greeter takes your memo as its greeting.
  - The forged memo is rejected (`InvalidProof`).
- Gas on anvil, with the stand-in verifier:
  - deploy: 1,081,264;
  - first `submit`: **105,825**, including the 21k base cost, measured before the
    delayed-set check was added. That check adds about 4.5k (one more storage read).

  The real Groth16 check adds roughly 270k, so expect ~380k per submit on a real chain.
- **Waiting on a real Groth16 proof.** SP1's real verifier is in `src/sp1/`, and `test/Groth16.t.sol`
  checks a proof against it (skipped until `test/fixtures/real-1-validator-fast.groth16.json`
  exists). Making one needs ≥32 GB RAM: see huginn's `docs/run.md` (VPS steps).

## Using it from your contract

The easiest way is to inherit `SolanaControlled` (see `src/examples/SolanaGreeter.sol`) and implement `_execute(command)`. The controller then sends a Solana memo

```
huginn:<chain id>:<your contract's address>:<nonce>:<command>
```

where `<nonce>` is the contract's `nonce() + 1` (`memoPrefix()` returns everything before the nonce). Commands run once each, in order, only on that chain and that deployment. An empty command cancels a signed command that was never relayed.

**A failed Solana transaction still runs its command.** The proof shows the transaction is in a finalized block, not that it succeeded. That's right for "the controller signed this", and wrong for anything that depends on the Solana side happening (a payment, a burn): don't build those on `SolanaControlled`.

Or use the pieces directly:

```solidity
import {SolanaLightClient} from "huginn-evm/SolanaLightClient.sol";
import {HuginnOutput} from "huginn-evm/HuginnOutput.sol";
import {SolanaTx} from "huginn-evm/SolanaTx.sol";

function act(bytes calldata output, bytes calldata proof) external {
    HuginnOutput.Output memory o = huginn.verify(output, proof);   // final + included, or revert
    SolanaTx.Transaction memory t = SolanaTx.parse(o.transaction);
    require(SolanaTx.isSigner(t, MY_SOLANA_KEY), "not signed by my Solana key");
    // ... your logic
}
```

## Trust

- The SP1 proof guarantees the Alpenglow signatures, the stake threshold, and that the
  transaction is in the block (see huginn's `docs/codebase.md`).
- `SolanaLightClient`'s **owner stores each epoch's validator set hash**. Until set rotation
  is proven in SP1, the owner is trusted for that: an owner who proposes a set whose keys it
  holds can prove anything. Make the owner a multisig. What limits the owner today:
  - a proposed set only counts after `setDelay` (default one day off anvil). Until then
    anyone can see it (`pendingSet`, the `ValidatorSetProposed` event), and the owner can
    replace or cancel it, so a typo is fixed before anything relies on it. Watch those
    events if you put value behind a client;
  - a set already in force can be superseded the same way, never instantly;
  - ownership moves in two steps (`transferOwnership`, then `acceptOwnership`).

  Propose a set with `script/ProposeValidatorSet.s.sol` (`HUGINN_CLIENT`, `EPOCH`,
  `VALIDATOR_SET_HASH`).
- **Inclusion is not success.** Failed Solana transactions are in blocks too. Check what your
  contract needs from the transaction itself.
