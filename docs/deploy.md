# Deploying to a real chain

For Base Sepolia first, then Base or any other EVM chain. Nothing here is audited; use a testnet until the open items in huginn's [`docs/security.md`](https://github.com/AlphaR2/huginn/blob/main/docs/security.md) are closed.

## What you need first

| Need | How |
|---|---|
| A funded deployer key | A fresh key, not one used elsewhere. Put it in `.env` as `PRIVATE_KEY` (never commit `.env`; it is gitignored). Fund it with testnet ETH. |
| The SP1 verifier on that chain | Succinct's `SP1VerifierGateway` for this chain, from [sp1-contracts `deployments/`](https://github.com/succinctlabs/sp1-contracts). Confirm it routes SP1 circuit **v6.1.0**. Set `SP1_VERIFIER`. |
| The program key | `huginn-evm vkey`. It changes whenever the SP1 program changes, so rebuild the program first. |
| The Solana cluster values | `SHRED_VERSION`, `SLOTS_PER_EPOCH`, and the first `EPOCH` and its `VALIDATOR_SET_HASH` (`fetch validator-set` prints it). Check the hash yourself against the cluster; it is the root of trust. |
| A Groth16 proof | Needs 32 GB RAM or more: [huginn `docs/run.md`](https://github.com/AlphaR2/huginn/blob/main/docs/run.md) part 2. STARK proofs are checked locally only and a real chain will refuse them. |
| An owner that is a multisig | Set it right after deploy with `transferOwnership`, then `acceptOwnership` from the multisig. |

## Steps

1. **Fill `.env`** from `.env.example`. Keep `SET_DELAY` unset (one day) or longer; it must stay shorter than a Solana epoch.
2. **Preflight** (read-only):
   ```bash
   EXPECT_CHAIN_ID=84532 forge script script/Preflight.s.sol --rpc-url $RPC_URL
   ```
3. **Deploy** the light client:
   ```bash
   forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast
   ```
   Write the printed `SolanaLightClient` address into `HUGINN_CLIENT`. Check the constructor values on the block explorer against your `.env`.
4. **Hand over ownership** to the multisig (`transferOwnership`, then `acceptOwnership` from it).
5. **Deploy your contract** (for the demo app: `script/DeployGreeter.s.sol`). A `SolanaControlled` contract's memo names its own address, so deploy it before sending the Solana memo.
6. **Send the Solana memo**, produce the Groth16 proof, then **submit** (`script/Submit.s.sol`) or call `execute` on your contract with the proof.
7. **Each new epoch**, propose that epoch's set: `script/ProposeValidatorSet.s.sol` (`HUGINN_CLIENT`, `EPOCH`, `VALIDATOR_SET_HASH`). It counts after `SET_DELAY`, so propose well before the epoch starts.

## After deploying

- Watch `ValidatorSetProposed` and `OwnerChanged` events on the client.
- Verify the contract source on the explorer.
- Record the addresses, the program key and the SP1 version you deployed with. A new program means a new deployment, never an upgrade.
