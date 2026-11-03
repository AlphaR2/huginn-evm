# hugin-evm

Use Solana transactions inside EVM contracts (Base first, any EVM chain the same way), with
proofs made by [huginn](../huginn).

huginn does the Solana side: it proves *"this transaction is in a block that Solana's
Alpenglow validators finalized"*, as one SP1 proof with a fixed-layout output. This repo is
one **destination** for those proofs:

```
huginn (dependency)                      hugin-evm (this repo)
────────────────────                     ──────────────────────────────────────────────
huginn-prover crate  ──▶ relayer/ ──▶ proof file ──▶ script/Submit.s.sol ──▶ SolanaLightClient
  (SP1 program,          makes the                     sends it to          checks it, stores
   Alpenglow checks)     proof                         your chain           block + tx; your
                                                                            contracts read it
```

## Layout

```
src/SolanaLightClient.sol   verify(publicValues, proof) / submit(...): the contract apps call
src/HuginnOutput.sol        reads the proof's output (same offsets as huginn's TxOutput::encode)
src/SolanaTx.sol            reads a Solana transaction: signers, programs, accounts, data
src/ISP1Verifier.sol        Succinct's verifier interface
src/mocks/BindingVerifier   local stand-in verifier (anvil only, never a real chain)
script/Deploy.s.sol         deploy SolanaLightClient with the settings in .env
script/Submit.s.sol         submit the relayer's proof file
relayer/                    Rust CLI on top of huginn-prover: `vkey`, `prove`
test/                       Foundry tests on the real proven memo transfer from huginn's local chain
.env.example                every setting, explained
```

## Setup

You need: [Foundry](https://getfoundry.sh), Rust, the SP1 toolchain (`sp1up`) and `protoc`
for the relayer. The relayer also needs Docker for Groth16 proofs on your own machine. huginn
must be checked out next to this repo (`../huginn`), or switch `relayer/Cargo.toml` to its
git dependency.

```bash
cp .env.example .env          # fill in your chain
forge install foundry-rs/forge-std --no-git
forge test -vv
```

## Run it locally (anvil, no real verifier)

```bash
anvil                                                   # terminal 1
forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast   # set HUGINN_CLIENT from the output
cp test/fixtures/real-1-validator-fast.compressed.json proofs/proof.json
forge script script/Submit.s.sol --rpc-url $RPC_URL --broadcast
```

## Run it on a real chain (Base Sepolia, Base, ...)

1. Set `RPC_URL`, `PRIVATE_KEY` and `SP1_VERIFIER` (Succinct's gateway on that chain) in `.env`.
2. `cd relayer && cargo run --release -- vkey`, then put the result in `PROGRAM_VKEY`.
3. Set `SHRED_VERSION`, `EPOCH`, `VALIDATOR_SET_HASH` for the Solana cluster you follow.
4. Deploy: `forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast`.
5. Prove: `cd relayer && cargo run --release -- prove --block <ledger-dump file> --signature <tx> --mode groth16`.
6. Submit: `forge script script/Submit.s.sol --rpc-url $RPC_URL --broadcast`.

## Using it from your contract

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
