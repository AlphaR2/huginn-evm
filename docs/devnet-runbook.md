# Runbook: Solana devnet → Base Sepolia, all on one VPS

You end with a Solana devnet transaction proven by a real Groth16 proof and verified on-chain by Succinct's SP1 verifier on Base Sepolia. Everything runs on one rented machine; your laptop only pushes code and reads the result. Nothing here is audited: testnets only, keys that hold nothing of value.

**Not yet tested on a VPS.** The pieces were tested separately: the devnet flow end to end with practice proofs and a local EVM chain, a real STARK proof of a devnet transaction, and the deploy scripts on anvil. A real Groth16 proof and the Base Sepolia deploy have never run. Where this guide guesses, it says so.

## 0. On your laptop: get the code to the VPS

Push both repos (`huginn` and `huginn-evm`) to GitHub, or copy them (step 4 shows both ways). Their latest commits must include the `--cluster devnet` mode and `shred-repair`.

## 1. Rent the machine

Ubuntu 24.04, **32 GB RAM or more, 8 vCPUs or more, 150 GB disk** (SP1's Groth16 circuits alone are ~14 GB; the Rust builds ~12 GB). Hourly billing is fine: you need it for a few hours and then delete it. Open only SSH (port 22). Nothing needs to listen: the repair client only makes outbound UDP.

## 2. Install the tools (on the VPS)

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev libudev-dev clang libclang-dev protobuf-compiler git curl tmux jq docker.io
sudo usermod -aG docker $USER && newgrp docker
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source ~/.cargo/env
```

```bash
curl -L https://sp1up.succinct.xyz | bash && source ~/.bashrc && sp1up --version v6.10.0
```

```bash
curl -L https://foundry.paradigm.xyz | bash && source ~/.bashrc && foundryup
```

```bash
sh -c "$(curl -sSfL https://release.anza.xyz/v4.4.0/install)"
echo 'export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
```

Check: `cargo prove --version`, `forge --version`, `solana --version` (4.4.0), `docker info`.

## 3. Keys (generate them on the VPS; they never leave it)

**Solana devnet key** (pays the memo; devnet SOL is free):

```bash
solana-keygen new --no-bip39-passphrase
solana config set --url devnet
solana airdrop 2        # rate-limited; if it fails use https://faucet.solana.com with the address from `solana address`
solana balance
```

**Base Sepolia deployer key:**

```bash
cast wallet new
```

Copy the address and private key somewhere safe. Send the address a little Base Sepolia ETH from a faucet (about 0.01 ETH is plenty: a deploy plus submit costs well under 3 million gas).

## 4. The code

Clone (the two repos must sit side by side):

```bash
cd ~ && git clone https://github.com/AlphaR2/huginn && git clone https://github.com/AlphaR2/huginn-evm
```

Or copy from your laptop (run there, in the folder holding both repos):

```bash
rsync -az --exclude target --exclude .local-ledger --exclude lib --exclude out --exclude cache --exclude broadcast huginn huginn-evm root@<vps-ip>:~/
```

## 5. Build

```bash
cd ~/huginn && cargo build --release -p ledger-dump -p fetch -p shred-repair
```

```bash
cd ~/huginn-evm && forge install foundry-rs/forge-std --no-git
cd app && cargo build --release
```

Both take several minutes (the second also compiles the SP1 program). The SP1 program's key (`huginn-evm vkey`) depends on the exact compiled program, so build on this machine and deploy from this same build.

## 6. Configure

```bash
cd ~/huginn-evm && cp .env.example .env && chmod 600 .env
```

Edit `.env` and set exactly these (leave the rest):

```
RPC_URL=https://sepolia.base.org
PRIVATE_KEY=0x<the key from `cast wallet new`>
SP1_VERIFIER=0xb69f2584CBcFf99a58C4e7002E8b89Af54a6f4e2
```

`SP1_VERIFIER` is Succinct's SP1 **v6.1.0** Groth16 verifier on Base Sepolia, taken from `succinctlabs/sp1-contracts` (`contracts/deployments/84532.json`, `V6_1_0_SP1_VERIFIER_GROTH16`). Check it yourself before trusting it:

```bash
cast code 0xb69f2584CBcFf99a58C4e7002E8b89Af54a6f4e2 --rpc-url https://sepolia.base.org | head -c 40
cast call 0xb69f2584CBcFf99a58C4e7002E8b89Af54a6f4e2 "VERSION()(string)" --rpc-url https://sepolia.base.org
```

The first must print bytecode (not `0x`), the second must say `v6.1.0`. Shred version, epoch and validator set are **not** configured: the app reads them from devnet at deploy time. (The gateway at `0x397A5f7f3dBd538f23DE225B51f532c34448dA9B` also exists; this guide uses the version-specific verifier, which avoids relying on the gateway having a route for v6.1.0.)

Before you start, check you are not at the end of a Solana epoch (an epoch lasts about 2 days, and the validator set the contract holds is for one epoch):

```bash
solana epoch-info --url devnet
```

If it is within a few hours of the end, wait.

## 7. Run

Use tmux so an SSH drop doesn't kill a 20-minute proof:

```bash
tmux new -s huginn
cd ~/huginn-evm
./app/target/release/huginn-evm run doctor --cluster devnet --mode groth16
```

Every line must be ✓. The doctor shows your Solana key and balance, your Base Sepolia deployer and balance, the verifier, and Docker. Fix anything marked ✗, then the whole flow in the TUI:

```bash
./app/target/release/huginn-evm --cluster devnet --mode groth16
```

Press `e` to type a message (it ends up in a public contract: no secrets), then `enter`. Steps, and what each shows as data:

1. **Check tools / EVM chain**: versions, balances, chain ids.
2. **Deploy light client**: the devnet epoch and validator set read from public RPC (count, total stake, hash), the program key, the Base Sepolia deploy transactions with Basescan links, and the values the contract reports back (compare them).
3. **Deploy the Solana-controlled app**: your Solana key and the contract it controls.
4. **Send**: the devnet signature, memo and explorer link.
5. **Finalize** and **Fetch**: the slot, which peer served the shreds, the signer bitmap, signed stake %, the certificate and block id (rebuilt vs certified).
6. **Prove**: cycles, proving time, program key, and what the proof commits to, decoded.
7. **Submit, check, command, forged**: Base Sepolia transactions, gas, values read back, the forged copy refused.

Expect the Groth16 proof to be the long step (tens of minutes on first use: the circuits and a Docker image download once). I have not measured it on a VPS.

Headless alternative, with a log:

```bash
./app/target/release/huginn-evm run --cluster devnet --mode groth16 --memo "gm from devnet" | tee run.log
```

If a step fails you can resume: `huginn-evm run --cluster devnet --mode groth16 prove submit check command forged` re-runs just those (state is in `proofs/state.json`).

## 8. Check the result without trusting the app

`proofs/receipt.md` holds every value and the commands to check them. Copy it to your laptop:

```bash
scp root@<vps-ip>:huginn-evm/proofs/receipt.md .
```

What a skeptic can do from it:

- **Solana:** open the signature on `https://explorer.solana.com/tx/<signature>?cluster=devnet`.
- **Validator set:** run `cargo run --release -p fetch -- validator-set --cluster devnet` in the huginn repo during the same epoch; the commitment must equal the receipt's "validator set hash" and the light client's `validatorSetHash(epoch)`.
- **Base Sepolia:** open the light client and transaction links (`sepolia.basescan.org`), and run the `cast call` lines (`isVerified`, `transactionSlot`, `programVKey`).
- **The proof really was checked by a real verifier:** the light client's `verifier()` is `0xb69f…f4e2` (Succinct's), not a stand-in; and a one-byte change to the output makes `verify` revert (step "forged").
- The proof's first four bytes (the receipt's "selector") should equal the first four bytes of `cast call 0xb69f…f4e2 "VERIFIER_HASH()(bytes32)"`. If they differ, the proof was made for a different SP1 version.

## 9. Clean up

Delete the VPS in your provider's console. The EVM key was a throwaway; do not reuse it. The contracts stay on Base Sepolia.

## If something goes wrong

| Symptom | Cause and fix |
|---|---|
| `no answer from … after 6 tries` | That peer doesn't hold the slot or is overloaded. The tool tries up to 8 peers by itself; rerun the `fetch` step. |
| `none of the first 8 peers could serve this` | The block is too old for peers' ledgers, or UDP is blocked by the provider. Send a new transaction (`run send finalize fetch …`). |
| `no footer within 24 blocks … carries its certificate` | The network is slow to certify, or the block was never given its own certificate (a finalized descendant finalizes it). Not supported yet. Send a new transaction. |
| `the light client holds epoch X's validator set, but this transaction is in epoch Y` | The epoch changed since deploy. Deploy again (`run deploy greeter send …`) or propose epoch Y's set with `script/ProposeValidatorSet.s.sol` and wait `SET_DELAY`. |
| `chain 84532 accepts only Groth16 proofs` | You ran with `--mode mock` or `compressed`. Use `--mode groth16`. |
| docker not running / permission denied | `sudo systemctl start docker`; make sure your user is in the `docker` group (`newgrp docker`). |
| Killed during the proof | Out of memory. Use a machine with 32 GB or more. |
| `InvalidProof` on submit | The verifier version doesn't match the proof (compare selectors, step 8) or the program key changed since deploy: deploy again from this same build. |
