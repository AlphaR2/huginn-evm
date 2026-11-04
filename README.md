# hugin-evm

Use Solana transactions inside EVM contracts (Base first, any EVM chain the same way), with
proofs made by [huginn](../huginn).

huginn does the Solana side: it proves *"this transaction is in a block that Solana's
Alpenglow validators finalized"*, as one SP1 proof with a fixed-layout output. This repo is
one **destination** for those proofs:

```
huginn (dependency)                      hugin-evm (this repo)
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

A single screen runs the whole flow:
1. checks your tools;
2. sends a memo on the local Solana chain and waits for it to finalize;
3. fetches the block's certificate and shreds;
4. proves it with SP1;
5. starts anvil, deploys the light client and submits the proof;
6. asks the contract `isVerified`;
7. deploys a contract your Solana key controls, and runs your memo as its command;
8. shows that a forged proof is rejected.

Each step shows the exact command it ran and that command's output.

| Key | Does |
|---|---|
| `enter` | run every step |
| `r` / `n` | run the selected step / from it to the end |
| `m` | proof mode: `mock` (seconds), `compressed` (real STARK, ~5 min), `groth16` (real, for real chains) |
| `s` | certificate source: `shreds` (the next block's footer; no plugin needed) or `plugin` (footer-dump) |
| `e` | edit the memo |
| `↑↓`, `J/K` | select a step, scroll its log |

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
.env.example                every setting, explained
```

## Setup

You need:
- [Foundry](https://getfoundry.sh), Rust, the SP1 toolchain (`sp1up`) and `protoc`;
- the Solana CLI (Agave 4.4, for `solana-test-validator --alpenglow`);
- huginn checked out next to this repo (`../huginn`), with its local Alpenglow validator
  running and `ledger-dump` built (see huginn's README). The footer-dump plugin is optional:
  certificates are read from the shreds by default. Or switch `app/Cargo.toml` to huginn's
  git dependency;
- Docker, only for Groth16 proofs made on your own machine.

```bash
cp .env.example .env          # optional on anvil: an empty PRIVATE_KEY uses anvil's test key
forge install foundry-rs/forge-std --no-git
forge test -vv
```

## Run it on a real chain (Base Sepolia, Base, ...)

1. Set `RPC_URL`, `PRIVATE_KEY` and `SP1_VERIFIER` (Succinct's gateway on that chain) in `.env`.
2. Set `SHRED_VERSION`, `EPOCH`, `VALIDATOR_SET_HASH` for the Solana cluster you follow.
3. `./app/target/release/huginn-evm run --mode groth16`, or press `m` until groth16 in the TUI.

## Status (2026-11-04)

- 19 Foundry tests pass on the real proven memo transfer from huginn's local chain.
- The full flow runs on fresh transactions in ~8 s with mock proofs: send, finalize, fetch,
  prove, deploy, submit, `isVerified` true, forged memo rejected (`InvalidProof`).
- Gas on anvil, with the stand-in verifier:
  - deploy: 1,081,264;
  - first `submit`: **105,825**, including the 21k base cost.

  The real Groth16 check adds roughly 270k, so expect ~380k per submit on a real chain.
- Waiting on a real Groth16 proof from huginn. When one exists, put its summary in
  `test/fixtures/` and point a test at Succinct's verifier.

## Using it from your contract

The easiest way is to inherit `SolanaControlled` (see `src/examples/SolanaGreeter.sol`) and implement `_execute(command)`. Or use the pieces directly:

```solidity
import {SolanaLightClient} from "hugin-evm/SolanaLightClient.sol";
import {HuginnOutput} from "hugin-evm/HuginnOutput.sol";
import {SolanaTx} from "hugin-evm/SolanaTx.sol";

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
- `SolanaLightClient`'s **owner stores each epoch's validator set hash** (write-once per
  epoch). Until set rotation is proven, the owner is trusted for that.
- **Inclusion is not success.** Failed Solana transactions are in blocks too. Check what your
  contract needs from the transaction itself.
