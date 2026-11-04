//! The steps of the Solana -> proof -> EVM flow. The TUI and the `run` subcommand both drive
//! these; each step logs the exact command or call it makes, so what you see on screen is
//! what you'd type yourself.

use {
    anyhow::{anyhow, bail, ensure, Context, Result},
    huginn_prover::{parse_signature, tx_input_from_block_file, Huginn, Mode},
    std::{
        collections::HashMap,
        path::{Path, PathBuf},
        process::Stdio,
        time::{Duration, Instant},
    },
    tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::Command,
        sync::mpsc::UnboundedSender,
    },
};

/// anvil's first default account: a public test key, only ever used on a local chain.
const ANVIL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// Who the memo transfer pays: a fixed throwaway address on the local chain.
const RECIPIENT: &str = "7eEm8iDQomGBvWzz159ATKbvG3aZfaBfTz6LvnDERmRR";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepId {
    Doctor,
    Send,
    Finalize,
    Fetch,
    Prove,
    Anvil,
    Deploy,
    Submit,
    Check,
    Forged,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Setup,
    Solana,
    Proof,
    Evm,
}

impl StepId {
    pub const ALL: [StepId; 10] = [
        StepId::Doctor,
        StepId::Send,
        StepId::Finalize,
        StepId::Fetch,
        StepId::Prove,
        StepId::Anvil,
        StepId::Deploy,
        StepId::Submit,
        StepId::Check,
        StepId::Forged,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StepId::Doctor => "doctor",
            StepId::Send => "send",
            StepId::Finalize => "finalize",
            StepId::Fetch => "fetch",
            StepId::Prove => "prove",
            StepId::Anvil => "anvil",
            StepId::Deploy => "deploy",
            StepId::Submit => "submit",
            StepId::Check => "check",
            StepId::Forged => "forged",
        }
    }

    pub fn from_name(s: &str) -> Option<StepId> {
        StepId::ALL.into_iter().find(|s2| s2.name() == s)
    }

    pub fn title(self) -> &'static str {
        match self {
            StepId::Doctor => "Check tools",
            StepId::Send => "Send memo on Solana",
            StepId::Finalize => "Wait for finality",
            StepId::Fetch => "Fetch certificate + shreds",
            StepId::Prove => "Prove with SP1",
            StepId::Anvil => "Start EVM chain",
            StepId::Deploy => "Deploy light client",
            StepId::Submit => "Submit proof",
            StepId::Check => "Ask the contract",
            StepId::Forged => "Forged proof rejected",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            StepId::Doctor => "Looks for every tool the flow needs (Solana CLI and validator, ledger-dump, Foundry, Docker for Groth16) and says what is missing.",
            StepId::Send => "Sends a small SOL transfer with your memo on the local Alpenglow chain, using the Solana CLI's default keypair.",
            StepId::Finalize => "Polls the validator until the transaction's block is finalized, and notes whether the transaction itself succeeded (inclusion is not success).",
            StepId::Fetch => "ledger-dump reads the block's shreds from the validator's ledger and the certificate that finalized it (in the next block's footer, or from the plugin), checks both, and rebuilds the block_id.",
            StepId::Prove => "Runs Huginn's SP1 program: checks the BLS certificate and the stake threshold, rebuilds the block_id, finds the transaction, and proves all of it.",
            StepId::Anvil => "Uses the EVM chain at RPC_URL; starts a local anvil if it is the default and nothing is running.",
            StepId::Deploy => "forge runs script/Deploy.s.sol: SolanaLightClient, pinned to Huginn's program key, this cluster's shred version, and epoch 0's validator set hash.",
            StepId::Submit => "forge runs script/Submit.s.sol: the contract checks the proof, then records the block and the transaction.",
            StepId::Check => "cast asks the contract isVerified(signature): what any other contract on this chain can now rely on.",
            StepId::Forged => "Changes one byte of the memo in the proven output and asks the contract to verify it with the same proof. It must refuse.",
        }
    }

    pub fn side(self) -> Side {
        match self {
            StepId::Doctor => Side::Setup,
            StepId::Send | StepId::Finalize | StepId::Fetch => Side::Solana,
            StepId::Prove => Side::Proof,
            _ => Side::Evm,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProofMode {
    /// SP1's mock prover: runs the program for real, makes no proof. Seconds.
    Mock,
    /// A real STARK proof (~5 min on a laptop). On anvil the stand-in verifier accepts it.
    Compressed,
    /// A real Groth16 proof: what a real chain's SP1 verifier takes. Needs Docker + circuits.
    Groth16,
}

impl ProofMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProofMode::Mock => "mock",
            ProofMode::Compressed => "compressed",
            ProofMode::Groth16 => "groth16",
        }
    }

    pub fn next(self) -> ProofMode {
        match self {
            ProofMode::Mock => ProofMode::Compressed,
            ProofMode::Compressed => ProofMode::Groth16,
            ProofMode::Groth16 => ProofMode::Mock,
        }
    }

    pub fn parse(s: &str) -> Result<ProofMode> {
        Ok(match s {
            "mock" => ProofMode::Mock,
            "compressed" => ProofMode::Compressed,
            "groth16" => ProofMode::Groth16,
            _ => bail!("proof mode is mock, compressed or groth16"),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertSource {
    Shreds,
    Plugin,
}

impl CertSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CertSource::Shreds => "shreds",
            CertSource::Plugin => "plugin",
        }
    }

    pub fn next(self) -> CertSource {
        match self {
            CertSource::Shreds => CertSource::Plugin,
            CertSource::Plugin => CertSource::Shreds,
        }
    }

    pub fn parse(s: &str) -> Result<CertSource> {
        Ok(match s {
            "shreds" => CertSource::Shreds,
            "plugin" => CertSource::Plugin,
            _ => bail!("certificate source is shreds or plugin"),
        })
    }
}

#[derive(Clone)]
pub struct Settings {
    /// The huginn checkout (ledger-dump, the validator's ledger and footers).
    pub huginn_dir: PathBuf,
    /// This repo (forge scripts, proofs/).
    pub evm_dir: PathBuf,
    pub solana_rpc: String,
    pub memo: String,
    pub mode: ProofMode,
    /// Where certificates come from: the shreds' own footers (default) or the footer-dump
    /// plugin's output. Shreds always come from the validator's ledger.
    pub certs: CertSource,
    /// `.env` merged over `.env.example`.
    pub env: HashMap<String, String>,
}

impl Settings {
    pub fn load(evm_dir: PathBuf, huginn_dir: Option<PathBuf>) -> Result<Settings> {
        let mut env = HashMap::new();
        for file in [".env.example", ".env"] {
            if let Ok(iter) = dotenvy::from_path_iter(evm_dir.join(file)) {
                for item in iter {
                    let (k, v) = item?;
                    env.insert(k, v);
                }
            }
        }
        let huginn_dir = huginn_dir.unwrap_or_else(|| evm_dir.join("../huginn"));
        Ok(Settings {
            huginn_dir: huginn_dir.canonicalize().with_context(|| format!("huginn checkout at {}", huginn_dir.display()))?,
            evm_dir,
            solana_rpc: std::env::var("SOLANA_RPC").unwrap_or_else(|_| "http://127.0.0.1:8899".into()),
            memo: "hello from solana".into(),
            mode: ProofMode::Mock,
            certs: CertSource::Shreds,
            env,
        })
    }

    pub fn evm_rpc(&self) -> String {
        self.env.get("RPC_URL").filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| "http://127.0.0.1:8545".into())
    }

    fn is_local_evm(&self) -> bool {
        let rpc = self.evm_rpc();
        rpc.contains("127.0.0.1") || rpc.contains("localhost")
    }

    fn private_key(&self) -> Option<String> {
        match self.env.get("PRIVATE_KEY").filter(|v| !v.is_empty()) {
            Some(k) => Some(k.clone()),
            None if self.is_local_evm() => Some(ANVIL_KEY.into()),
            None => None,
        }
    }
}

/// What the flow has found so far. The TUI's top bar shows it. Saved to
/// `proofs/state.json` after every step, so steps can also be run one at a time.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub signature: Option<String>,
    pub slot: Option<u64>,
    pub tx_ok: Option<bool>,
    pub block_file: Option<PathBuf>,
    pub shreds: Option<String>,
    pub cycles: Option<u64>,
    pub prove_secs: Option<f64>,
    pub proof_mode: Option<ProofMode>,
    pub proof_file: Option<PathBuf>,
    pub program_vkey: Option<String>,
    pub chain_id: Option<u64>,
    pub client: Option<String>,
    pub submit_gas: Option<u64>,
    pub verified: Option<bool>,
    pub forged_rejected: Option<bool>,
}

pub enum Event {
    Started(StepId),
    Log(StepId, String),
    Done(StepId, Result<String, String>, Duration),
    State(Box<State>),
    /// A run (one step or several) is over.
    Finished,
}

pub struct Runner {
    pub settings: Settings,
    pub state: State,
    events: UnboundedSender<Event>,
}

impl Runner {
    pub fn new(settings: Settings, events: UnboundedSender<Event>) -> Runner {
        let state = std::fs::read_to_string(state_file(&settings)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Runner { settings, state, events }
    }

    /// Runs one step and reports it. Returns whether it succeeded.
    pub async fn run(&mut self, id: StepId) -> bool {
        let _ = self.events.send(Event::Started(id));
        let t = Instant::now();
        let result = self.step(id).await.map_err(|e| format!("{e:#}"));
        let ok = result.is_ok();
        if let Ok(json) = serde_json::to_string_pretty(&self.state) {
            let _ = std::fs::write(state_file(&self.settings), json + "\n");
        }
        let _ = self.events.send(Event::State(Box::new(self.state.clone())));
        let _ = self.events.send(Event::Done(id, result, t.elapsed()));
        ok
    }

    pub fn finished(&self) {
        let _ = self.events.send(Event::Finished);
    }

    fn log(&self, id: StepId, line: impl Into<String>) {
        let _ = self.events.send(Event::Log(id, line.into()));
    }

    async fn step(&mut self, id: StepId) -> Result<String> {
        match id {
            StepId::Doctor => self.doctor().await,
            StepId::Send => self.send().await,
            StepId::Finalize => self.finalize().await,
            StepId::Fetch => self.fetch().await,
            StepId::Prove => self.prove().await,
            StepId::Anvil => self.anvil().await,
            StepId::Deploy => self.deploy().await,
            StepId::Submit => self.submit().await,
            StepId::Check => self.check().await,
            StepId::Forged => self.forged().await,
        }
    }

    // ---- Solana ---------------------------------------------------------------------

    async fn doctor(&mut self) -> Result<String> {
        let id = StepId::Doctor;
        let mut missing = Vec::new();
        let mut warnings = Vec::new();
        for (tool, args, why) in [
            ("solana", vec!["--version"], "Solana CLI: sends the memo"),
            ("forge", vec!["--version"], "Foundry: deploys and submits"),
            ("cast", vec!["--version"], "Foundry: asks the contract"),
            ("anvil", vec!["--version"], "Foundry: local EVM chain"),
        ] {
            match self.cmd(id, tool, &args, None, &[], true).await {
                Ok(out) => self.log(id, format!("✓ {tool:<8} {}", out.lines().next().unwrap_or("").trim())),
                Err(_) => {
                    self.log(id, format!("✗ {tool:<8} not found ({why})"));
                    missing.push(tool);
                }
            }
        }
        let dump = self.ledger_dump();
        if dump.exists() {
            self.log(id, format!("✓ ledger-dump {}", dump.display()));
        } else {
            self.log(id, "✗ ledger-dump not built: in huginn run `cargo build --release -p ledger-dump`");
            missing.push("ledger-dump");
        }
        match self.rpc("getHealth", "[]").await {
            Ok(_) => self.log(id, format!("✓ validator {} is up", self.settings.solana_rpc)),
            Err(_) => {
                self.log(id, format!("✗ validator not reachable at {}", self.settings.solana_rpc));
                self.log(id, "    in huginn: solana-test-validator --alpenglow --ledger .local-ledger --geyser-plugin-config plugins/footer-dump/config.json");
                missing.push("validator");
            }
        }
        let footers = self.settings.huginn_dir.join("fixtures/local/footers.jsonl");
        match (self.settings.certs, footers.exists()) {
            (CertSource::Shreds, _) => self.log(id, "✓ certificates come from the shreds' footers (no plugin needed)"),
            (CertSource::Plugin, true) => self.log(id, "✓ footer-dump plugin output found (certificates)"),
            (CertSource::Plugin, false) => {
                self.log(id, "✗ no footers.jsonl: start the validator with the footer-dump plugin, or use certs from shreds");
                missing.push("footers");
            }
        }
        if self.cmd(id, "docker", &["info"], None, &[], true).await.is_ok() {
            self.log(id, "✓ docker   running (needed only for groth16)");
        } else {
            self.log(id, "! docker   not running: only groth16 proofs need it");
            warnings.push("docker");
        }
        let circuits = home().join(".sp1/circuits/groth16/v6.1.0/.complete");
        if circuits.exists() {
            self.log(id, "✓ SP1 Groth16 circuit files installed");
        } else {
            self.log(id, "! SP1 Groth16 circuit files not installed: only groth16 proofs need them");
            warnings.push("circuits");
        }
        ensure!(missing.is_empty(), "missing: {}", missing.join(", "));
        Ok(if warnings.is_empty() { "all tools found".into() } else { format!("ready (groth16 needs: {})", warnings.join(", ")) })
    }

    async fn send(&mut self) -> Result<String> {
        let id = StepId::Send;
        let rpc = self.settings.solana_rpc.clone();
        let memo = self.settings.memo.clone();
        let out = self
            .cmd(
                id,
                "solana",
                &["transfer", RECIPIENT, "0.01", "--with-memo", &memo, "--allow-unfunded-recipient", "--url", &rpc, "--output", "json"],
                None,
                &[],
                false,
            )
            .await?;
        let sig = serde_json::from_str::<serde_json::Value>(&out)?["signature"]
            .as_str()
            .context("no signature in the CLI's output")?
            .to_string();
        self.state = State { signature: Some(sig.clone()), ..State::default() };
        Ok(format!("signature {sig}"))
    }

    async fn finalize(&mut self) -> Result<String> {
        let id = StepId::Finalize;
        let sig = self.state.signature.clone().context("send a transaction first")?;
        self.log(id, format!("getSignatureStatuses [{sig}]  (every second, up to 60 s)"));
        let mut last = String::new();
        for _ in 0..60 {
            let v = self.rpc("getSignatureStatuses", &format!("[[\"{sig}\"]]")).await?;
            let status = &v["value"][0];
            if !status.is_null() {
                let level = status["confirmationStatus"].as_str().unwrap_or("?").to_string();
                if level != last {
                    self.log(id, format!("  {level} in slot {}", status["slot"]));
                    last = level.clone();
                }
                if level == "finalized" {
                    let slot = status["slot"].as_u64().context("slot")?;
                    let ok = status["err"].is_null();
                    self.state.slot = Some(slot);
                    self.state.tx_ok = Some(ok);
                    if !ok {
                        self.log(id, format!("  the transaction FAILED on Solana: {}. It is still in the block.", status["err"]));
                    }
                    return Ok(format!("slot {slot} finalized{}", if ok { "" } else { " (tx failed)" }));
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("not finalized after 60 s")
    }

    async fn fetch(&mut self) -> Result<String> {
        let id = StepId::Fetch;
        let slot = self.state.slot.context("wait for finality first")?;
        let out_dir = self.settings.evm_dir.join("proofs");
        let dump = self.ledger_dump();
        let slot_s = slot.to_string();
        let out_s = out_dir.display().to_string();
        let huginn = self.settings.huginn_dir.clone();
        let certs = self.settings.certs.as_str();
        // The certificate for slot S travels in a later block's footer, so it can take a
        // few slots to appear.
        let mut last_err = None;
        for attempt in 0..10 {
            match self.cmd(id, dump.to_str().unwrap(), &["--slot", &slot_s, "--certs", certs, "--out", &out_s], Some(&huginn), &[], attempt > 0).await {
                Ok(out) => {
                    if let Some(line) = out.lines().find(|l| l.contains("data +")) {
                        self.state.shreds = Some(line.split(':').nth(1).unwrap_or("").split(',').next().unwrap_or("").trim().into());
                    }
                    let file = out_dir.join(format!("block_{slot}.json"));
                    ensure!(file.exists(), "ledger-dump wrote no {}", file.display());
                    self.state.block_file = Some(file);
                    return Ok(format!("block_id rebuilt and certified · {}", self.state.shreds.clone().unwrap_or_default()));
                }
                Err(e) => {
                    if attempt == 0 {
                        self.log(id, "  certificate not captured yet; retrying for up to 20 s");
                    }
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        Err(last_err.unwrap())
    }

    async fn prove(&mut self) -> Result<String> {
        let id = StepId::Prove;
        let block = self.state.block_file.clone().context("fetch the block first")?;
        let sig = self.state.signature.clone().context("send a transaction first")?;
        let mode = self.settings.mode;
        // SP1 picks its prover from the environment.
        std::env::set_var("SP1_PROVER", if mode == ProofMode::Mock { "mock" } else { "cpu" });
        self.log(id, format!("huginn_prover::tx_input_from_block_file({}, {sig})", block.file_name().unwrap().to_string_lossy()));
        let input = tx_input_from_block_file(&block, &sig)?;
        self.log(id, format!("  {} validator(s), {} shreds of the tx's component", input.finality.validators.len(), input.inclusion.shreds.len()));
        let huginn = Huginn::from_env().await;
        self.log(id, "huginn.execute(..)  (run the program, count cycles)");
        let e = huginn.execute(&input).await?;
        self.log(id, format!("  finality {} cycles, inclusion {} cycles, total {}", e.finality_cycles, e.inclusion_cycles, e.total_cycles));
        self.state.cycles = Some(e.total_cycles);

        let sp1_mode = match mode {
            ProofMode::Compressed => Mode::Compressed,
            ProofMode::Mock | ProofMode::Groth16 => Mode::Groth16,
        };
        self.log(id, format!("huginn.prove(.., {})  {}", mode.as_str(), match mode {
            ProofMode::Mock => "(SP1 mock prover: no real proof)",
            ProofMode::Compressed => "(real STARK, ~5 min)",
            ProofMode::Groth16 => "(real Groth16, ~10+ min, Docker)",
        }));
        let proven = huginn.prove(&input, sp1_mode).await?;
        self.log(id, format!("  verified; public values = native output ({} bytes)", proven.public_values.len()));
        self.log(id, format!("  program key {}", proven.program_vkey));
        let file = self.settings.evm_dir.join("proofs/proof.json");
        let mut summary = proven.summary(&sig);
        summary["mode"] = mode.as_str().into();
        std::fs::write(&file, serde_json::to_string_pretty(&summary)? + "\n")?;
        self.log(id, format!("  wrote {}", file.display()));
        self.state.prove_secs = Some(proven.prove_secs);
        self.state.proof_mode = Some(mode);
        self.state.proof_file = Some(file);
        self.state.program_vkey = Some(proven.program_vkey);
        Ok(format!("{} proof in {:.1}s, {} cycles", mode.as_str(), proven.prove_secs, e.total_cycles))
    }

    // ---- EVM ------------------------------------------------------------------------

    async fn anvil(&mut self) -> Result<String> {
        let id = StepId::Anvil;
        let rpc = self.settings.evm_rpc();
        if let Ok(out) = self.cmd(id, "cast", &["chain-id", "--rpc-url", &rpc], None, &[], true).await {
            self.log(id, format!("$ cast chain-id --rpc-url {rpc}"));
            let chain: u64 = out.trim().parse()?;
            self.state.chain_id = Some(chain);
            return Ok(format!("chain {chain} at {rpc}"));
        }
        ensure!(self.settings.is_local_evm(), "no chain at {rpc}");
        self.log(id, "$ anvil --silent   (nothing at the default RPC; starting one)");
        // Left running after huginn-evm exits, so later commands reach the same chain.
        // Stop it with `pkill anvil`.
        Command::new("anvil").arg("--silent").stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        self.log(id, "  it keeps running after huginn-evm exits (stop it: pkill anvil)");
        for _ in 0..50 {
            if let Ok(out) = self.cmd(id, "cast", &["chain-id", "--rpc-url", &rpc], None, &[], true).await {
                let chain: u64 = out.trim().parse()?;
                self.state.chain_id = Some(chain);
                return Ok(format!("anvil started, chain {chain}"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("anvil did not start")
    }

    fn forge_env(&self) -> Result<Vec<(String, String)>> {
        let mut env: Vec<(String, String)> = self.settings.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        env.push(("PRIVATE_KEY".into(), self.settings.private_key().context("set PRIVATE_KEY in .env for a non-local chain")?));
        env.push(("RPC_URL".into(), self.settings.evm_rpc()));
        if let Some(vkey) = &self.state.program_vkey {
            env.push(("PROGRAM_VKEY".into(), vkey.clone()));
        }
        if let Some(client) = &self.state.client {
            env.push(("HUGINN_CLIENT".into(), client.clone()));
        }
        env.push(("PROOF_FILE".into(), "proofs/proof.json".into()));
        Ok(env)
    }

    async fn deploy(&mut self) -> Result<String> {
        let id = StepId::Deploy;
        let env = self.forge_env()?;
        let rpc = self.settings.evm_rpc();
        let dir = self.settings.evm_dir.clone();
        let out = self.forge(id, "script/Deploy.s.sol", &rpc, &dir, &env).await?;
        let client = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("SolanaLightClient:"))
            .map(|a| a.trim().to_string())
            .context("no SolanaLightClient address in forge's output")?;
        self.state.client = Some(client.clone());
        self.state.verified = None;
        self.state.forged_rejected = None;
        Ok(format!("SolanaLightClient at {client}"))
    }

    async fn submit(&mut self) -> Result<String> {
        let id = StepId::Submit;
        ensure!(self.state.client.is_some(), "deploy first");
        ensure!(self.state.proof_file.is_some(), "prove first");
        if self.state.proof_mode == Some(ProofMode::Groth16) && self.settings.env.get("SP1_VERIFIER").is_none_or(|v| v.is_empty()) {
            bail!("a groth16 proof needs SP1's real verifier: set SP1_VERIFIER (anvil's stand-in only takes mock/compressed)");
        }
        let env = self.forge_env()?;
        let rpc = self.settings.evm_rpc();
        let dir = self.settings.evm_dir.clone();
        let out = self.forge(id, "script/Submit.s.sol", &rpc, &dir, &env).await?;
        ensure!(out.contains("verified Solana slot"), "the contract did not confirm the slot");
        let gas = self.chain_id_for_broadcast().await.and_then(|chain| last_gas(&dir, "Submit.s.sol", chain));
        self.state.submit_gas = gas;
        Ok(format!("accepted{}", gas.map(|g| format!(", {g} gas")).unwrap_or_default()))
    }

    async fn check(&mut self) -> Result<String> {
        let id = StepId::Check;
        let client = self.state.client.clone().context("deploy first")?;
        let sig = self.state.signature.clone().context("send a transaction first")?;
        let sig_hex = format!("0x{}", hex::encode(parse_signature(&sig)?));
        let rpc = self.settings.evm_rpc();
        let out = self.cmd(id, "cast", &["call", &client, "isVerified(bytes)(bool)", &sig_hex, "--rpc-url", &rpc], None, &[], false).await?;
        let verified = out.trim() == "true";
        self.state.verified = Some(verified);
        ensure!(verified, "the contract says this transaction is not verified");
        Ok("isVerified(signature) = true".into())
    }

    async fn forged(&mut self) -> Result<String> {
        let id = StepId::Forged;
        let client = self.state.client.clone().context("deploy first")?;
        let file = self.state.proof_file.clone().context("prove first")?;
        let rpc = self.settings.evm_rpc();
        let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
        let pv = hex::decode(summary["public_values"].as_str().context("public_values")?)?;
        let mut proof = hex::decode(summary["proof"].as_str().unwrap_or(""))?;
        if proof.is_empty() {
            // The local stand-in verifier's "proof" for the real output.
            let verifier = self.cmd(id, "cast", &["call", &client, "verifier()(address)", "--rpc-url", &rpc], None, &[], false).await?;
            let vkey = self.cmd(id, "cast", &["call", &client, "programVKey()(bytes32)", "--rpc-url", &rpc], None, &[], true).await?;
            let p = self
                .cmd(id, "cast", &["call", verifier.trim(), "proofFor(bytes32,bytes)(bytes)", vkey.trim(), &format!("0x{}", hex::encode(&pv)), "--rpc-url", &rpc], None, &[], true)
                .await?;
            proof = hex::decode(p.trim().trim_start_matches("0x"))?;
        }
        let proof_hex = format!("0x{}", hex::encode(&proof));

        self.log(id, "1) the real output with its proof:");
        self.cmd(id, "cast", &["call", &client, "verify(bytes,bytes)", &format!("0x{}", hex::encode(&pv)), &proof_hex, "--rpc-url", &rpc], None, &[], false)
            .await
            .context("the real proof should pass")?;
        self.log(id, "   accepted");

        let mut forged = pv.clone();
        let last = forged.len() - 1;
        let before = forged[last] as char;
        forged[last] ^= 0x01;
        self.log(id, format!("2) the same proof, memo's last character changed '{before}' -> '{}':", forged[last] as char));
        match self.cmd(id, "cast", &["call", &client, "verify(bytes,bytes)", &format!("0x{}", hex::encode(&forged)), &proof_hex, "--rpc-url", &rpc], None, &[], false).await {
            Ok(_) => {
                self.state.forged_rejected = Some(false);
                bail!("the forged output was ACCEPTED")
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let reason = msg.split("execution reverted: ").nth(1).map(|r| r.split([',', '|']).next().unwrap_or(r).trim().to_string());
                self.log(id, format!("   rejected: {}", reason.unwrap_or(msg)));
                self.state.forged_rejected = Some(true);
                Ok("forged memo rejected, real one accepted".into())
            }
        }
    }

    // ---- helpers --------------------------------------------------------------------

    /// `forge script ... --broadcast`, showing only the script's own logs and the result
    /// (forge's full output is in the error if it fails).
    async fn forge(&self, id: StepId, script: &str, rpc: &str, dir: &Path, env: &[(String, String)]) -> Result<String> {
        self.log(id, format!("$ forge script {script} --rpc-url {rpc} --broadcast"));
        let out = self.cmd(id, "forge", &["script", script, "--rpc-url", rpc, "--broadcast"], Some(dir), env, true).await?;
        let mut in_logs = false;
        for line in out.lines() {
            let t = line.trim();
            if t.starts_with("== ") || t.starts_with("##") {
                in_logs = t == "== Logs ==";
                continue;
            }
            if (in_logs && !t.is_empty()) || t.starts_with("ONCHAIN EXECUTION") {
                self.log(id, format!("  {t}"));
            }
        }
        Ok(out)
    }

    fn ledger_dump(&self) -> PathBuf {
        self.settings.huginn_dir.join("target/release/ledger-dump")
    }

    async fn chain_id_for_broadcast(&self) -> Option<u64> {
        self.state.chain_id
    }

    async fn rpc(&self, method: &str, params: &str) -> Result<serde_json::Value> {
        let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#);
        let out = Command::new("curl")
            .args(["-s", "--max-time", "5", &self.settings.solana_rpc, "-H", "content-type: application/json", "-d", &body])
            .output()
            .await?;
        ensure!(out.status.success(), "RPC {} unreachable", self.settings.solana_rpc);
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        if let Some(e) = v.get("error") {
            bail!("RPC error: {e}");
        }
        Ok(v["result"].clone())
    }

    /// Runs a command, streams its output into the step's log, returns stdout. `quiet`
    /// hides the command line and its output (for checks and polls).
    async fn cmd(&self, id: StepId, program: &str, args: &[&str], cwd: Option<&Path>, env: &[(String, String)], quiet: bool) -> Result<String> {
        if !quiet {
            let shown: Vec<String> = args.iter().map(|a| if a.chars().count() > 42 { format!("{}…", a.chars().take(40).collect::<String>()) } else { a.to_string() }).collect();
            let name = Path::new(program).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(program.into());
            self.log(id, format!("$ {name} {}", shown.join(" ")));
        }
        let mut c = Command::new(program);
        c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
        if let Some(dir) = cwd {
            c.current_dir(dir);
        }
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.spawn().map_err(|e| anyhow!("{program}: {e}"))?;
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = out.next_line(), if !out_done => match line? {
                    Some(l) => { if !quiet { self.log(id, format!("  {}", clip(&l))); } stdout.push_str(&l); stdout.push('\n'); }
                    None => out_done = true,
                },
                line = err.next_line(), if !err_done => match line? {
                    Some(l) => { if !quiet { self.log(id, format!("  {}", clip(&l))); } stderr.push_str(&l); stderr.push('\n'); }
                    None => err_done = true,
                },
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            if let Some(l) = stderr.lines().chain(stdout.lines()).find(|l| l.contains("execution reverted")) {
                bail!("{}", l.trim().trim_start_matches("Error: "));
            }
            let tail: Vec<&str> = stderr.lines().chain(stdout.lines()).filter(|l| !l.trim().is_empty()).collect();
            bail!("{} failed: {}", Path::new(program).file_name().unwrap().to_string_lossy(), tail.iter().rev().take(2).rev().cloned().collect::<Vec<_>>().join(" | "));
        }
        Ok(stdout)
    }
}

fn state_file(settings: &Settings) -> PathBuf {
    settings.evm_dir.join("proofs/state.json")
}

pub fn send_state(runner: &Runner) {
    let _ = runner.events.send(Event::State(Box::new(runner.state.clone())));
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Long lines (hex return values) cut to fit a screen.
fn clip(s: &str) -> String {
    if s.chars().count() > 150 {
        format!("{}… ({} chars)", s.chars().take(140).collect::<String>(), s.chars().count())
    } else {
        s.to_string()
    }
}

fn last_gas(dir: &Path, script: &str, chain: u64) -> Option<u64> {
    let f = dir.join(format!("broadcast/{script}/{chain}/run-latest.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).ok()?).ok()?;
    let g = v["receipts"].as_array()?.last()?["gasUsed"].as_str()?;
    u64::from_str_radix(g.trim_start_matches("0x"), 16).ok()
}

/// Tools installed by sp1up, foundryup and the Solana installer aren't always on PATH
/// (e.g. when launched from an IDE).
pub fn extend_path() {
    let home = home();
    let extra = [".foundry/bin", ".sp1/bin", ".local/share/solana/install/active_release/bin", ".cargo/bin"]
        .map(|p| home.join(p).display().to_string());
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{path}:{}", extra.join(":")));
    // Homebrew and Docker Desktop.
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{path}:/opt/homebrew/bin:/usr/local/bin:{}", home.join(".docker/bin").display()));
}
