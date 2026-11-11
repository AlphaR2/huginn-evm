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
const ANVIL_CHAIN_ID: u64 = 31337;
/// Who the memo transfer pays: a fixed throwaway address on the local chain.
const RECIPIENT: &str = "7eEm8iDQomGBvWzz159ATKbvG3aZfaBfTz6LvnDERmRR";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepId {
    Doctor,
    Anvil,
    Deploy,
    Greeter,
    Send,
    Finalize,
    Fetch,
    Prove,
    Submit,
    Check,
    Command,
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
    /// The order a full run takes. The EVM contracts come first: the memo names the
    /// greeter's address and next nonce, so the greeter must exist before the memo is sent.
    pub const ALL: [StepId; 12] = [
        StepId::Doctor,
        StepId::Anvil,
        StepId::Deploy,
        StepId::Greeter,
        StepId::Send,
        StepId::Finalize,
        StepId::Fetch,
        StepId::Prove,
        StepId::Submit,
        StepId::Check,
        StepId::Command,
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
            StepId::Greeter => "greeter",
            StepId::Command => "command",
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
            StepId::Greeter => "Deploy Solana-controlled app",
            StepId::Command => "Run the Solana command",
            StepId::Forged => "Forged proof rejected",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            StepId::Doctor => "Looks for every tool the flow needs (Solana CLI and validator, ledger-dump, Foundry, Docker for Groth16) and says what is missing.",
            StepId::Send => "Sends a small SOL transfer with the memo huginn:<chain id>:<greeter address>:<next nonce>:<your text> on the local Alpenglow chain, signed by the Solana CLI's default keypair.",
            StepId::Finalize => "Polls the validator until the transaction's block is finalized, and notes whether the transaction itself succeeded (inclusion is not success).",
            StepId::Fetch => "ledger-dump reads the block's shreds from the validator's ledger and the certificate that finalized it (in the next block's footer, or from the plugin), checks both, and rebuilds the block_id.",
            StepId::Prove => "Runs Huginn's SP1 program: checks the BLS certificate and the stake threshold, rebuilds the block_id, finds the transaction, and proves all of it.",
            StepId::Anvil => "Uses the EVM chain at RPC_URL; starts a local anvil if it is the default and nothing is running.",
            StepId::Deploy => "Computes Huginn's program key, then forge runs script/Deploy.s.sol: SolanaLightClient, pinned to that key, this cluster's shred version, and epoch 0's validator set hash.",
            StepId::Submit => "forge runs script/Submit.s.sol: the contract checks the proof, then records the block and the transaction.",
            StepId::Check => "cast asks the contract isVerified(signature): what any other contract on this chain can now rely on.",
            StepId::Greeter => "forge runs script/DeployGreeter.s.sol: SolanaGreeter, a contract whose greeting only your Solana CLI key can change (SolanaControlled).",
            StepId::Command => "forge runs script/Execute.s.sol: the same proof goes to SolanaGreeter.execute, which checks the controller signed it, the memo names this chain and this greeter, and its nonce is the next one. The greeting becomes your memo.",
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

/// Which Solana cluster the flow reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cluster {
    /// `solana-test-validator --alpenglow` on this machine; shreds from its ledger.
    Local,
    /// Public devnet; shreds from validators over the repair protocol (no node needed).
    Devnet,
}

impl Cluster {
    pub fn as_str(self) -> &'static str {
        match self {
            Cluster::Local => "local",
            Cluster::Devnet => "devnet",
        }
    }

    pub fn parse(s: &str) -> Result<Cluster> {
        Ok(match s {
            "local" => Cluster::Local,
            "devnet" => Cluster::Devnet,
            _ => bail!("cluster is local or devnet"),
        })
    }

    fn default_rpc(self) -> &'static str {
        match self {
            Cluster::Local => "http://127.0.0.1:8899",
            Cluster::Devnet => "https://api.devnet.solana.com",
        }
    }
}

/// One piece of data a step found or did, shown as it happens and kept in the receipt.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Fact {
    pub step: String,
    pub label: String,
    pub value: String,
    /// Where to look it up (an explorer), if there is one.
    pub link: Option<String>,
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
    pub cluster: Cluster,
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
            huginn_dir: huginn_dir
                .canonicalize()
                .with_context(|| format!("huginn checkout at {}", huginn_dir.display()))?,
            evm_dir,
            solana_rpc: std::env::var("SOLANA_RPC")
                .unwrap_or_else(|_| Cluster::Local.default_rpc().into()),
            cluster: Cluster::Local,
            memo: "hello from solana".into(),
            mode: ProofMode::Mock,
            certs: CertSource::Shreds,
            env,
        })
    }

    /// Reads this cluster; its public RPC unless `SOLANA_RPC` says otherwise.
    pub fn set_cluster(&mut self, c: Cluster) {
        self.cluster = c;
        self.solana_rpc = std::env::var("SOLANA_RPC").unwrap_or_else(|_| c.default_rpc().into());
    }

    /// A link that shows the Solana transaction in a block explorer.
    pub fn solana_tx_link(&self, sig: &str) -> String {
        match self.cluster {
            Cluster::Devnet => format!("https://explorer.solana.com/tx/{sig}?cluster=devnet"),
            Cluster::Local => format!(
                "https://explorer.solana.com/tx/{sig}?cluster=custom&customUrl={}",
                self.solana_rpc
            ),
        }
    }

    pub fn evm_rpc(&self) -> String {
        self.env
            .get("RPC_URL")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:8545".into())
    }

    /// Whether the RPC URL points at this machine: only used to decide whether to start
    /// anvil. Never used to pick a key (an SSH tunnel to a real chain is also "localhost").
    fn rpc_is_this_machine(&self) -> bool {
        let rpc = self.evm_rpc();
        rpc.contains("127.0.0.1") || rpc.contains("localhost")
    }

    /// `PRIVATE_KEY` from `.env`; with none set, anvil's public test key, but only when the
    /// chain itself says it is anvil (chain id 31337).
    fn private_key(&self, chain_id: u64) -> Result<String> {
        match self.env.get("PRIVATE_KEY").filter(|v| !v.is_empty()) {
            Some(k) => Ok(k.clone()),
            None if chain_id == ANVIL_CHAIN_ID => Ok(ANVIL_KEY.into()),
            None => bail!("set PRIVATE_KEY in .env: chain {chain_id} is not a local anvil chain"),
        }
    }
}

/// What the flow has found so far. The TUI's top bar shows it. Saved to
/// `proofs/state.json` after every step, so steps can also be run one at a time.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    pub greeter: Option<String>,
    pub greeting: Option<String>,
    pub forged_rejected: Option<bool>,
    /// The epoch and validator set hash the light client was deployed with (devnet).
    pub epoch: Option<u64>,
    pub set_hash: Option<String>,
    pub facts: Vec<Fact>,
}

pub enum Event {
    Started(StepId),
    Log(StepId, String),
    Done(StepId, Result<String, String>, Duration),
    State(Box<State>),
    /// A piece of data a step just found.
    Fact(Fact),
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
        let state = std::fs::read_to_string(state_file(&settings))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Runner {
            settings,
            state,
            events,
        }
    }

    /// Runs one step and reports it. Returns whether it succeeded.
    pub async fn run(&mut self, id: StepId) -> bool {
        self.state.facts.retain(|f| f.step != id.name());
        let _ = self.events.send(Event::Started(id));
        let t = Instant::now();
        let result = self.step(id).await.map_err(|e| format!("{e:#}"));
        let ok = result.is_ok();
        if let Ok(json) = serde_json::to_string_pretty(&self.state) {
            let _ = std::fs::write(state_file(&self.settings), json + "\n");
        }
        let _ = self.events.send(Event::State(Box::new(self.state.clone())));
        let _ = self.events.send(Event::Done(id, result, t.elapsed()));
        self.write_receipt();
        ok
    }

    /// Records a piece of data: shown live, kept in `proofs/state.json` and the receipt.
    fn fact(&mut self, id: StepId, label: &str, value: impl Into<String>) {
        self.fact_link(id, label, value, None);
    }

    fn fact_link(
        &mut self,
        id: StepId,
        label: &str,
        value: impl Into<String>,
        link: Option<String>,
    ) {
        let f = Fact {
            step: id.name().into(),
            label: label.into(),
            value: value.into(),
            link,
        };
        self.state
            .facts
            .retain(|x| !(x.step == f.step && x.label == f.label));
        self.state.facts.push(f.clone());
        let _ = self.events.send(Event::Fact(f));
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
            StepId::Greeter => self.greeter().await,
            StepId::Command => self.command().await,
            StepId::Forged => self.forged().await,
        }
    }

    // ---- Solana ---------------------------------------------------------------------

    fn is_devnet(&self) -> bool {
        self.settings.cluster == Cluster::Devnet
    }

    /// Where this run's cluster configuration lives: `proofs/<cluster>/<cluster>/`
    /// (cluster_info.json, validator_set_<epoch>.json). Never the repo's tracked fixtures.
    fn sets_dir(&self) -> PathBuf {
        let c = self.settings.cluster.as_str();
        self.settings.evm_dir.join("proofs").join(c).join(c)
    }

    fn chain(&self) -> u64 {
        self.state.chain_id.unwrap_or(ANVIL_CHAIN_ID)
    }

    /// `cast call`, result trimmed, "28090 [2.8e4]" cut to "28090".
    async fn read(&self, id: StepId, addr: &str, sig: &str, args: &[&str]) -> Result<String> {
        let rpc = self.settings.evm_rpc();
        let mut a = vec!["call", addr, sig];
        a.extend(args);
        a.extend(["--rpc-url", rpc.as_str()]);
        let out = self.cmd(id, "cast", &a, None, &[], true).await?;
        let out = out.trim();
        Ok(out
            .split(" [")
            .next()
            .unwrap_or(out)
            .trim_matches('"')
            .to_string())
    }

    async fn doctor(&mut self) -> Result<String> {
        let id = StepId::Doctor;
        let devnet = self.is_devnet();
        let mut missing = Vec::new();
        let mut warnings = Vec::new();
        self.fact(
            id,
            "Solana cluster",
            format!(
                "{} · {}",
                self.settings.cluster.as_str(),
                self.settings.solana_rpc
            ),
        );
        self.fact(id, "EVM RPC", self.settings.evm_rpc());
        self.fact(id, "proof mode", self.settings.mode.as_str());
        for (tool, args, why) in [
            ("solana", vec!["--version"], "Solana CLI: sends the memo"),
            ("forge", vec!["--version"], "Foundry: deploys and submits"),
            ("cast", vec!["--version"], "Foundry: asks the contract"),
            ("anvil", vec!["--version"], "Foundry: local EVM chain"),
        ] {
            match self.cmd(id, tool, &args, None, &[], true).await {
                Ok(out) => {
                    let v = out.lines().next().unwrap_or("").trim().to_string();
                    self.log(id, format!("✓ {tool:<8} {v}"));
                    self.fact(id, tool, v);
                }
                Err(_) => {
                    self.log(id, format!("✗ {tool:<8} not found ({why})"));
                    missing.push(tool);
                }
            }
        }
        for (name, path, how) in [
            (
                "ledger-dump",
                self.ledger_dump(),
                "cargo build --release -p ledger-dump",
            ),
            (
                "shred-repair",
                self.settings.huginn_dir.join("target/release/shred-repair"),
                "cargo build --release -p shred-repair",
            ),
            (
                "fetch",
                self.settings.huginn_dir.join("target/release/fetch"),
                "cargo build --release -p fetch",
            ),
        ] {
            if name != "ledger-dump" && !devnet {
                continue;
            }
            if path.exists() {
                self.log(id, format!("✓ {name} {}", path.display()));
            } else {
                self.log(id, format!("✗ {name} not built: in huginn run `{how}`"));
                missing.push(name);
            }
        }
        match self.rpc("getHealth", "[]").await {
            Ok(_) => self.log(
                id,
                format!("✓ Solana RPC {} is up", self.settings.solana_rpc),
            ),
            Err(_) => {
                self.log(
                    id,
                    format!("✗ Solana RPC not reachable at {}", self.settings.solana_rpc),
                );
                if !devnet {
                    self.log(
                        id,
                        "    in huginn: solana-test-validator --alpenglow --ledger .local-ledger",
                    );
                }
                missing.push("Solana RPC");
            }
        }
        if let Ok(v) = self.rpc("getVersion", "[]").await {
            self.fact(
                id,
                "Solana node version",
                v["solana-core"].as_str().unwrap_or("?").to_string(),
            );
        }
        if let Ok(v) = self.rpc("getEpochInfo", "[]").await {
            self.fact(
                id,
                "Solana epoch / slot",
                format!(
                    "{} / {}",
                    v["epoch"],
                    commas(v["absoluteSlot"].as_u64().unwrap_or(0))
                ),
            );
        }
        if devnet {
            if let Ok(addr) = self.cmd(id, "solana", &["address"], None, &[], true).await {
                let addr = addr.trim().to_string();
                self.fact(id, "Solana key", addr.clone());
                match self.rpc("getBalance", &format!(r#"["{addr}"]"#)).await {
                    Ok(v) => {
                        let lamports = v["value"].as_u64().unwrap_or(0);
                        self.fact(
                            id,
                            "Solana balance",
                            format!("{} SOL ({lamports} lamports)", lamports as f64 / 1e9),
                        );
                        if lamports < 20_000_000 {
                            self.log(id, format!("✗ {addr} holds {} SOL on devnet; the memo transfer needs about 0.011", lamports as f64 / 1e9));
                            self.log(id, "    fix: solana airdrop 2 --url devnet   (or https://faucet.solana.com)");
                            missing.push("devnet SOL");
                        }
                    }
                    Err(e) => {
                        warnings.push(Box::leak(format!("balance: {e}").into_boxed_str()) as &str)
                    }
                }
            }
        } else {
            let footers = self
                .settings
                .huginn_dir
                .join("fixtures/local/footers.jsonl");
            match (self.settings.certs, footers.exists()) {
                (CertSource::Shreds, _) => self.log(
                    id,
                    "✓ certificates come from the shreds' footers (no plugin needed)",
                ),
                (CertSource::Plugin, true) => {
                    self.log(id, "✓ footer-dump plugin output found (certificates)")
                }
                (CertSource::Plugin, false) => {
                    self.log(id, "✗ no footers.jsonl: start the validator with the footer-dump plugin, or use certs from shreds");
                    missing.push("footers");
                }
            }
        }

        // The EVM side: a real chain needs a key, funds, a verifier and a real proof mode.
        let evm = self.settings.evm_rpc();
        match self
            .cmd(
                id,
                "cast",
                &["chain-id", "--rpc-url", &evm],
                None,
                &[],
                true,
            )
            .await
        {
            Ok(c) => {
                let chain: u64 = c.trim().parse().unwrap_or(0);
                self.fact(
                    id,
                    "EVM chain id",
                    format!("{chain} ({})", chain_label(chain)),
                );
                if chain != ANVIL_CHAIN_ID {
                    match self.settings.private_key(chain) {
                        Ok(key) => {
                            let env = [("PRIVATE_KEY".to_string(), key)];
                            let dir = self.settings.evm_dir.clone();
                            match self
                                .cmd(
                                    id,
                                    "forge",
                                    &["script", "script/Whoami.s.sol", "--rpc-url", &evm],
                                    Some(&dir),
                                    &env,
                                    true,
                                )
                                .await
                            {
                                Ok(out) => {
                                    let get = |k: &str| {
                                        out.lines()
                                            .find_map(|l| l.trim().strip_prefix(k))
                                            .map(|v| v.trim().to_string())
                                    };
                                    let who = get("deployer:").unwrap_or_default();
                                    let wei: u128 = get("balance wei:")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0);
                                    self.fact_link(
                                        id,
                                        "EVM deployer",
                                        who.clone(),
                                        evm_addr_link(chain, &who),
                                    );
                                    self.fact(
                                        id,
                                        "EVM balance",
                                        format!("{} ETH ({wei} wei)", wei as f64 / 1e18),
                                    );
                                    if wei == 0 {
                                        self.log(
                                            id,
                                            format!("✗ {who} has no funds on chain {chain}"),
                                        );
                                        self.log(
                                            id,
                                            "    fix: send it a little testnet ETH for gas",
                                        );
                                        missing.push("EVM funds");
                                    }
                                }
                                Err(e) => {
                                    self.log(id, format!("✗ could not read the deployer: {e}"));
                                    missing.push("deployer");
                                }
                            }
                        }
                        Err(e) => {
                            self.log(id, format!("✗ {e}"));
                            missing.push("PRIVATE_KEY");
                        }
                    }
                    match self
                        .settings
                        .env
                        .get("SP1_VERIFIER")
                        .filter(|v| !v.is_empty())
                    {
                        Some(v) => {
                            let code = self
                                .cmd(id, "cast", &["code", v, "--rpc-url", &evm], None, &[], true)
                                .await
                                .unwrap_or_default();
                            if code.trim().len() > 2 {
                                self.fact_link(
                                    id,
                                    "SP1 verifier",
                                    v.clone(),
                                    evm_addr_link(chain, v),
                                );
                            } else {
                                self.log(
                                    id,
                                    format!("✗ SP1_VERIFIER {v} has no code on chain {chain}"),
                                );
                                missing.push("SP1_VERIFIER");
                            }
                        }
                        None => {
                            self.log(id, "✗ SP1_VERIFIER is empty: a real chain needs Succinct's verifier address");
                            self.log(id, "    fix: see docs/deploy.md (Base Sepolia: 0xb69f2584CBcFf99a58C4e7002E8b89Af54a6f4e2)");
                            missing.push("SP1_VERIFIER");
                        }
                    }
                    if self.settings.mode != ProofMode::Groth16 {
                        self.log(id, format!("✗ proof mode is {}: only a Groth16 proof is accepted on chain {chain}", self.settings.mode.as_str()));
                        self.log(id, "    fix: --mode groth16 (or press m until groth16)");
                        missing.push("proof mode");
                    }
                }
            }
            Err(_) => {
                if self.settings.rpc_is_this_machine() {
                    self.log(
                        id,
                        "! no EVM chain at the default RPC yet: the next step starts a local anvil",
                    );
                } else {
                    self.log(id, format!("✗ EVM RPC {evm} is not reachable"));
                    missing.push("EVM RPC");
                }
            }
        }

        if self
            .cmd(id, "docker", &["info"], None, &[], true)
            .await
            .is_ok()
        {
            self.log(id, "✓ docker   running (needed only for groth16)");
            self.fact(id, "docker", "running");
        } else if self.settings.mode == ProofMode::Groth16 {
            self.log(id, "✗ docker   not running: a groth16 proof needs it");
            self.log(
                id,
                "    fix: start Docker (on a VPS: sudo systemctl start docker)",
            );
            missing.push("docker");
        } else {
            self.log(id, "! docker   not running: only groth16 proofs need it");
            warnings.push("docker");
        }
        let circuits = home().join(".sp1/circuits/groth16/v6.1.0/.complete");
        if circuits.exists() {
            self.log(id, "✓ SP1 Groth16 circuit files installed");
        } else {
            self.log(id, "! SP1 Groth16 circuit files not installed: the first groth16 proof downloads them (~14 GB)");
            warnings.push("circuits");
        }
        ensure!(missing.is_empty(), "missing: {}", missing.join(", "));
        Ok(if warnings.is_empty() {
            "all tools found".into()
        } else {
            format!("ready (groth16 needs: {})", warnings.join(", "))
        })
    }

    async fn send(&mut self) -> Result<String> {
        let id = StepId::Send;
        let rpc = self.settings.solana_rpc.clone();
        // Addressed to the greeter (SolanaControlled's memo format): its prefix names the
        // EVM chain and its address, then comes the next nonce.
        let greeter = self
            .state
            .greeter
            .clone()
            .context("deploy the greeter first: the memo names its address")?;
        let evm = self.settings.evm_rpc();
        let prefix = unquote(
            &self
                .cmd(
                    id,
                    "cast",
                    &["call", &greeter, "memoPrefix()(string)", "--rpc-url", &evm],
                    None,
                    &[],
                    true,
                )
                .await?,
        );
        let nonce: u64 = self
            .cmd(
                id,
                "cast",
                &["call", &greeter, "nonce()(uint64)", "--rpc-url", &evm],
                None,
                &[],
                true,
            )
            .await?
            .trim()
            .parse()?;
        let memo = format!("{prefix}{}:{}", nonce + 1, self.settings.memo);
        let payer = self
            .cmd(id, "solana", &["address"], None, &[], true)
            .await
            .map(|a| a.trim().to_string())
            .unwrap_or_default();
        let out = self
            .cmd(
                id,
                "solana",
                &[
                    "transfer",
                    RECIPIENT,
                    "0.01",
                    "--with-memo",
                    &memo,
                    "--allow-unfunded-recipient",
                    "--url",
                    &rpc,
                    "--output",
                    "json",
                ],
                None,
                &[],
                false,
            )
            .await?;
        let sig = serde_json::from_str::<serde_json::Value>(&out)?["signature"]
            .as_str()
            .context("no signature in the CLI's output")?
            .to_string();
        // A new transaction: forget the previous one's results, keep the contracts.
        let kept: Vec<Fact> = self
            .state
            .facts
            .iter()
            .filter(|f| ["doctor", "anvil", "deploy", "greeter"].contains(&f.step.as_str()))
            .cloned()
            .collect();
        self.state = State {
            signature: Some(sig.clone()),
            chain_id: self.state.chain_id,
            program_vkey: self.state.program_vkey.clone(),
            client: self.state.client.clone(),
            greeter: self.state.greeter.clone(),
            greeting: self.state.greeting.clone(),
            epoch: self.state.epoch,
            set_hash: self.state.set_hash.clone(),
            facts: kept,
            ..State::default()
        };
        self.fact(id, "paid by", payer);
        self.fact(
            id,
            "to",
            format!("{RECIPIENT} (0.01 SOL, a throwaway address)"),
        );
        self.fact(id, "memo (full)", memo);
        self.fact(
            id,
            "memo names",
            format!(
                "EVM chain {}, greeter {greeter}, nonce {}",
                self.chain(),
                nonce + 1
            ),
        );
        let link = self.settings.solana_tx_link(&sig);
        self.fact_link(id, "signature", sig.clone(), Some(link));
        Ok(format!("signature {sig}"))
    }

    async fn finalize(&mut self) -> Result<String> {
        let id = StepId::Finalize;
        let sig = self
            .state
            .signature
            .clone()
            .context("send a transaction first")?;
        self.log(
            id,
            format!("getSignatureStatuses [{sig}]  (every second, up to 90 s)"),
        );
        let started = Instant::now();
        let mut last = String::new();
        for _ in 0..90 {
            let v = self
                .rpc("getSignatureStatuses", &format!("[[\"{sig}\"]]"))
                .await?;
            let status = &v["value"][0];
            if !status.is_null() {
                let level = status["confirmationStatus"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string();
                if level != last {
                    self.log(id, format!("  {level} in slot {}", status["slot"]));
                    self.fact(
                        id,
                        &level,
                        format!(
                            "slot {} · after {:.1}s",
                            commas(status["slot"].as_u64().unwrap_or(0)),
                            started.elapsed().as_secs_f64()
                        ),
                    );
                    last = level.clone();
                }
                if level == "finalized" {
                    let slot = status["slot"].as_u64().context("slot")?;
                    let ok = status["err"].is_null();
                    self.state.slot = Some(slot);
                    self.state.tx_ok = Some(ok);
                    self.fact(id, "slot", commas(slot));
                    self.fact(id, "epoch", format!("{} (slot / 432,000)", slot / 432_000));
                    self.fact(
                        id,
                        "transaction result",
                        if ok {
                            "succeeded".to_string()
                        } else {
                            format!(
                                "FAILED: {} (still in the block: inclusion is not success)",
                                status["err"]
                            )
                        },
                    );
                    let params = format!(
                        r#"["{sig}",{{"commitment":"finalized","maxSupportedTransactionVersion":1,"encoding":"json"}}]"#
                    );
                    if let Ok(t) = self.rpc("getTransaction", &params).await {
                        if let Some(fee) = t["meta"]["fee"].as_u64() {
                            self.fact(id, "fee", format!("{fee} lamports"));
                        }
                        self.fact(
                            id,
                            "transaction version",
                            t["version"].to_string().trim_matches('"').to_string(),
                        );
                        if let Some(bt) = t["blockTime"].as_i64() {
                            self.fact(id, "block time (unix)", bt.to_string());
                        }
                    }
                    if !ok {
                        self.log(
                            id,
                            format!(
                                "  the transaction FAILED on Solana: {}. It is still in the block.",
                                status["err"]
                            ),
                        );
                    }
                    return Ok(format!(
                        "slot {slot} finalized{}",
                        if ok { "" } else { " (tx failed)" }
                    ));
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("not finalized after 90 s")
    }

    async fn fetch(&mut self) -> Result<String> {
        let id = StepId::Fetch;
        let slot = self.state.slot.context("wait for finality first")?;
        let ledger_out = if self.is_devnet() {
            self.fetch_devnet(slot).await?
        } else {
            self.fetch_local(slot).await?
        };
        let file = self
            .settings
            .evm_dir
            .join(format!("proofs/block_{slot}.json"));
        ensure!(file.exists(), "ledger-dump wrote no {}", file.display());
        self.state.block_file = Some(file.clone());
        if let Some(line) = ledger_out.lines().find(|l| l.contains("data +")) {
            self.state.shreds = Some(
                line.split(':')
                    .nth(1)
                    .unwrap_or("")
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .into(),
            );
        }
        for (key, label) in [
            ("computed block_id", "block_id rebuilt from shreds"),
            ("certified block_id", "block_id in the certificate"),
        ] {
            if let Some(v) = ledger_out.lines().find_map(|l| l.trim().strip_prefix(key)) {
                self.fact(id, label, v.trim().to_string());
            }
        }
        self.fetch_facts(&file)?;
        Ok(format!(
            "block_id rebuilt and certified · {}",
            self.state.shreds.clone().unwrap_or_default()
        ))
    }

    /// Local chain: shreds from the validator's own ledger.
    async fn fetch_local(&mut self, slot: u64) -> Result<String> {
        let id = StepId::Fetch;
        let out_dir = self.settings.evm_dir.join("proofs");
        let dump = self.ledger_dump();
        let slot_s = slot.to_string();
        let out_s = out_dir.display().to_string();
        let huginn = self.settings.huginn_dir.clone();
        let certs = self.settings.certs.as_str();
        let epoch = slot / 432_000;
        if let Some(held) = self.state.epoch {
            ensure!(held == epoch, "the light client holds epoch {held}'s validator set, but this transaction is in epoch {epoch}: deploy again");
        }
        if !self
            .sets_dir()
            .join(format!("validator_set_{epoch}.json"))
            .exists()
        {
            self.fetch_set(id).await?;
        }
        let sets_s = self.sets_dir().display().to_string();
        self.fact(
            id,
            "shreds from",
            "the local validator's ledger (RocksDB, read-only)",
        );
        // The certificate for slot S travels in a later block's footer, so it can take a
        // few slots to appear.
        let mut last_err = None;
        for attempt in 0..10 {
            match self
                .cmd(
                    id,
                    dump.to_str().unwrap(),
                    &[
                        "--slot", &slot_s, "--certs", certs, "--sets", &sets_s, "--out", &out_s,
                    ],
                    Some(&huginn),
                    &[],
                    attempt > 0,
                )
                .await
            {
                Ok(out) => return Ok(out),
                Err(e) => {
                    if attempt == 0 {
                        self.log(
                            id,
                            "  certificate not captured yet; retrying for up to 20 s",
                        );
                    }
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        Err(last_err.unwrap())
    }

    /// Devnet: shreds from public validators over the repair protocol, the validator set
    /// from public RPC. No node, no paid feed.
    async fn fetch_devnet(&mut self, slot: u64) -> Result<String> {
        let id = StepId::Fetch;
        let evm = self.settings.evm_dir.clone();
        let huginn = self.settings.huginn_dir.clone();
        let epoch = slot / 432_000;
        let sets = self.sets_dir();
        if let Some(held) = self.state.epoch {
            ensure!(
                held == epoch,
                "the light client holds epoch {held}'s validator set, but this transaction is in epoch {epoch}: propose epoch {epoch}'s set (script/ProposeValidatorSet.s.sol), wait SET_DELAY, then run again"
            );
        }
        if !sets.join(format!("validator_set_{epoch}.json")).exists() {
            self.fetch_set(id).await?;
        }
        ensure!(
            sets.join(format!("validator_set_{epoch}.json")).exists(),
            "no validator set for epoch {epoch}: public RPC serves only the current epoch's stakes, and this transaction's epoch has ended"
        );
        let shreds_dir = evm.join("proofs/shreds");
        let _ = std::fs::remove_dir_all(&shreds_dir);
        let repair = huginn.join("target/release/shred-repair");
        let slot_s = slot.to_string();
        let dir_s = shreds_dir.display().to_string();
        let rpc = self.settings.solana_rpc.clone();
        self.fact(
            id,
            "shreds from",
            "public devnet validators, Agave repair protocol (shred-repair)",
        );
        let out = self
            .cmd(
                id,
                repair.to_str().unwrap(),
                &["--rpc", &rpc, "fetch", "--slot", &slot_s, "--out", &dir_s],
                Some(&huginn),
                &[],
                false,
            )
            .await?;
        for l in out.lines() {
            if let Some(v) = l.strip_prefix("served by ") {
                self.fact(id, "served by (peer, identity)", v.trim().to_string());
            } else if l.starts_with("slot ") && l.contains("data shreds") {
                let (k, v) = l.split_once(':').unwrap_or(("", l));
                self.fact(id, &format!("fetched {}", k.trim()), v.trim().to_string());
            }
        }
        let dump = self.ledger_dump();
        let sets_s = sets.display().to_string();
        let out_s = evm.join("proofs").display().to_string();
        self.cmd(
            id,
            dump.to_str().unwrap(),
            &[
                "--shreds", &dir_s, "--slot", &slot_s, "--sets", &sets_s, "--out", &out_s,
            ],
            Some(&huginn),
            &[],
            false,
        )
        .await
    }

    /// The current epoch's ranked BLS validator set and shred version, from the cluster's RPC.
    /// Returns (epoch, ranked, total stake, commitment, shred version).
    async fn fetch_set(&mut self, id: StepId) -> Result<(u64, u64, u64, String, u64)> {
        let huginn = self.settings.huginn_dir.clone();
        let fetch = huginn.join("target/release/fetch");
        let root = self
            .settings
            .evm_dir
            .join("proofs")
            .join(self.settings.cluster.as_str())
            .display()
            .to_string();
        let rpc = self.settings.solana_rpc.clone();
        let cluster = self.settings.cluster.as_str();
        self.cmd(
            id,
            fetch.to_str().unwrap(),
            &[
                "--rpc",
                &rpc,
                "--out",
                &root,
                "cluster-info",
                "--cluster",
                cluster,
            ],
            Some(&huginn),
            &[],
            false,
        )
        .await?;
        let out = self
            .cmd(
                id,
                fetch.to_str().unwrap(),
                &[
                    "--rpc",
                    &rpc,
                    "--out",
                    &root,
                    "validator-set",
                    "--cluster",
                    cluster,
                ],
                Some(&huginn),
                &[],
                false,
            )
            .await?;
        // The vote accounts as they are now can include validators that registered a BLS key
        // after the epoch began; the real certificates were signed without them. Check a recent
        // block's certificate and drop them, before anything is deployed with this set's hash.
        if self.settings.cluster == Cluster::Devnet {
            self.reconcile_set(id)
                .await
                .context("the validator set could not be matched to real certificates; not deploying with a set that may be wrong")?;
        }
        // "epoch 1202: 4366 vote accounts, 26 with BLS key, 22 ranked, total stake N, commitment HEX -> file"
        let line = out
            .lines()
            .find(|l| l.starts_with("epoch "))
            .context("fetch printed no epoch line")?;
        let num = |after: &str| -> Option<u64> {
            line.split(after)
                .nth(1)?
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        };
        let epoch: u64 = line
            .trim_start_matches("epoch ")
            .split(':')
            .next()
            .unwrap_or("")
            .parse()
            .context("epoch")?;
        let ranked = line
            .split(", ")
            .find(|p| p.ends_with(" ranked"))
            .and_then(|p| p.split(' ').next()?.parse().ok())
            .context("ranked count")?;
        let stake = num("total stake ").context("total stake")?;
        let commitment = line
            .split("commitment ")
            .nth(1)
            .and_then(|c| c.split_whitespace().next())
            .context("commitment")?
            .to_string();
        let info: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
            self.sets_dir().join("cluster_info.json"),
        )?)?;
        let shred_version = info["shred_version"].as_u64().context("shred_version")?;
        // The set file is the truth: reconciling may have changed it since `fetch` printed.
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
            self.sets_dir().join(format!("validator_set_{epoch}.json")),
        )?)?;
        let ranked = file["validator_set"]["entries"]
            .as_array()
            .map(|a| a.len() as u64)
            .unwrap_or(ranked);
        let stake = file["validator_set"]["total_stake"]
            .as_u64()
            .unwrap_or(stake);
        let commitment = file["commitment"]
            .as_str()
            .map(str::to_string)
            .unwrap_or(commitment);
        Ok((epoch, ranked, stake, commitment, shred_version))
    }

    /// Fetches a few recently finalized blocks with their certificates, and has ledger-dump
    /// make the saved set agree with all of them (see `ledger-dump --fix-set`). One block can
    /// leave the choice open; several settle it.
    async fn reconcile_set(&mut self, id: StepId) -> Result<()> {
        const BLOCKS: u64 = 4;
        let huginn = self.settings.huginn_dir.clone();
        let newest = self
            .rpc("getSlot", r#"[{"commitment":"finalized"}]"#)
            .await?["result"]
            .as_u64()
            .context("finalized slot")?
            .saturating_sub(80);
        let dir = self.settings.evm_dir.join("proofs/probe");
        let _ = std::fs::remove_dir_all(&dir);
        let dir_s = dir.display().to_string();
        let rpc = self.settings.solana_rpc.clone();
        let repair = huginn.join("target/release/shred-repair");
        let slots: Vec<u64> = (0..BLOCKS).map(|i| newest - i * 45).collect();
        self.log(
            id,
            format!(
                "checking the validator set against the certificates of {BLOCKS} recent blocks (slots {} to {})",
                slots[slots.len() - 1],
                slots[0]
            ),
        );
        for slot in &slots {
            let slot_s = slot.to_string();
            self.cmd(
                id,
                repair.to_str().unwrap(),
                &["--rpc", &rpc, "fetch", "--slot", &slot_s, "--out", &dir_s],
                Some(&huginn),
                &[],
                true,
            )
            .await?;
        }
        let sets = self.sets_dir().display().to_string();
        let list = slots
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let newest_s = newest.to_string();
        self.cmd(
            id,
            self.ledger_dump().to_str().unwrap(),
            &[
                "--shreds",
                &dir_s,
                "--slot",
                &newest_s,
                "--fix-slots",
                &list,
                "--sets",
                &sets,
                "--fix-set",
            ],
            Some(&huginn),
            &[],
            false,
        )
        .await?;
        Ok(())
    }

    /// Everything worth showing about the fetched block, read from the fixture ledger-dump wrote.
    fn fetch_facts(&mut self, file: &Path) -> Result<()> {
        let id = StepId::Fetch;
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file)?)?;
        let shreds = v["data_shreds"].as_array().map(|a| a.len()).unwrap_or(0);
        self.fact(id, "block", commas(v["slot"].as_u64().unwrap_or(0)));
        self.fact(id, "epoch", v["epoch"].to_string());
        self.fact(id, "shred version", v["shred_version"].to_string());
        self.fact(
            id,
            "data shreds",
            format!("{shreds} in {} FEC sets of 32", shreds / 32),
        );
        let parent_id = v["parent_block_id"]
            .as_str()
            .or_else(|| v["parent_block_final_cert"]["block_id"].as_str())
            .unwrap_or("?");
        self.fact(
            id,
            "parent",
            format!(
                "slot {} · block_id {parent_id}",
                commas(v["parent_slot"].as_u64().unwrap_or(0))
            ),
        );
        let cert = &v["block_final_cert"];
        let fast = cert["notar_aggregate"].is_null();
        self.fact(
            id,
            "certificate for",
            format!(
                "slot {} · block_id {}",
                commas(cert["slot"].as_u64().unwrap_or(0)),
                cert["block_id"].as_str().unwrap_or("?")
            ),
        );
        self.fact(
            id,
            "certificate path",
            if fast {
                "fast: one round, needs 80% of stake".to_string()
            } else {
                "slow: Finalize + Notarize, each needs 60% of stake".to_string()
            },
        );
        let entries = v["validator_set"]["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let total: u64 = entries.iter().filter_map(|e| e["stake"].as_u64()).sum();
        let bitmap = hex::decode(cert["final_aggregate"]["bitmap"].as_str().unwrap_or(""))
            .unwrap_or_default();
        let ranks = signer_ranks(&bitmap);
        let signed: u64 = ranks
            .iter()
            .filter_map(|r| entries.get(*r))
            .filter_map(|e| e["stake"].as_u64())
            .sum();
        self.fact(
            id,
            "validator set",
            format!(
                "{} ranked validators · total stake {}",
                entries.len(),
                commas(total)
            ),
        );
        self.fact(
            id,
            "signers",
            format!("{} of {} ranked validators", ranks.len(), entries.len()),
        );
        self.fact(
            id,
            "signed stake",
            format!(
                "{} of {} = {}",
                commas(signed),
                commas(total),
                pct(signed, total)
            ),
        );
        self.fact(
            id,
            "signer bitmap",
            cert["final_aggregate"]["bitmap"]
                .as_str()
                .unwrap_or("?")
                .to_string(),
        );
        self.fact(
            id,
            "BLS signature (96 bytes)",
            cert["final_aggregate"]["signature"]
                .as_str()
                .unwrap_or("?")
                .to_string(),
        );
        Ok(())
    }

    async fn prove(&mut self) -> Result<String> {
        let id = StepId::Prove;
        let block = self
            .state
            .block_file
            .clone()
            .context("fetch the block first")?;
        let sig = self
            .state
            .signature
            .clone()
            .context("send a transaction first")?;
        let mode = self.settings.mode;
        ensure!(
            self.chain() == ANVIL_CHAIN_ID || mode == ProofMode::Groth16,
            "chain {} accepts only Groth16 proofs; the {} proof would be refused on-chain. Use --mode groth16",
            self.chain(),
            mode.as_str()
        );
        // SP1 picks its prover from the environment.
        std::env::set_var(
            "SP1_PROVER",
            if mode == ProofMode::Mock {
                "mock"
            } else {
                "cpu"
            },
        );
        self.log(
            id,
            format!(
                "huginn_prover::tx_input_from_block_file({}, {sig})",
                block.file_name().unwrap().to_string_lossy()
            ),
        );
        let input = tx_input_from_block_file(&block, &sig)?;
        self.log(
            id,
            format!(
                "  {} validator(s), {} shreds of the tx's component",
                input.finality.validators.len(),
                input.inclusion.shreds.len()
            ),
        );
        self.fact(
            id,
            "validators in the set",
            input.finality.validators.len().to_string(),
        );
        self.fact(
            id,
            "shreds proven for inclusion",
            format!(
                "{} (the transaction's component)",
                input.inclusion.shreds.len()
            ),
        );
        let huginn = Huginn::from_env().await;
        self.log(id, "huginn.execute(..)  (run the program, count cycles)");
        let e = huginn.execute(&input).await?;
        self.log(
            id,
            format!(
                "  finality {} cycles, inclusion {} cycles, total {}",
                e.finality_cycles, e.inclusion_cycles, e.total_cycles
            ),
        );
        self.fact(id, "cycles: certificate", commas(e.finality_cycles));
        self.fact(id, "cycles: inclusion", commas(e.inclusion_cycles));
        self.fact(id, "cycles: total", commas(e.total_cycles));
        self.state.cycles = Some(e.total_cycles);

        let sp1_mode = match mode {
            ProofMode::Compressed => Mode::Compressed,
            ProofMode::Mock | ProofMode::Groth16 => Mode::Groth16,
        };
        self.log(
            id,
            format!(
                "huginn.prove(.., {})  {}",
                mode.as_str(),
                match mode {
                    ProofMode::Mock => "(SP1 mock prover: no real proof)",
                    ProofMode::Compressed => "(real STARK, ~5 min)",
                    ProofMode::Groth16 => "(real Groth16, ~10+ min, Docker)",
                }
            ),
        );
        let proven = huginn.prove(&input, sp1_mode).await?;
        self.log(
            id,
            format!(
                "  verified; public values = native output ({} bytes)",
                proven.public_values.len()
            ),
        );
        self.log(id, format!("  program key {}", proven.program_vkey));
        if let Some(deployed) = &self.state.program_vkey {
            ensure!(deployed == &proven.program_vkey, "the light client was deployed for program {deployed}, this proof is for {}: run deploy again", proven.program_vkey);
        }
        let file = self.settings.evm_dir.join("proofs/proof.json");
        let mut summary = proven.summary(&sig);
        summary["mode"] = mode.as_str().into();
        std::fs::write(&file, serde_json::to_string_pretty(&summary)? + "\n")?;
        self.log(id, format!("  wrote {}", file.display()));
        self.state.prove_secs = Some(proven.prove_secs);
        self.state.proof_mode = Some(mode);
        self.state.proof_file = Some(file.clone());

        self.fact(
            id,
            "proof mode",
            match mode {
                ProofMode::Mock => "practice (SP1 mock prover: proves nothing)".to_string(),
                ProofMode::Compressed => "real STARK (checked on this machine only)".to_string(),
                ProofMode::Groth16 => {
                    "real Groth16 (what a chain's SP1 verifier checks)".to_string()
                }
            },
        );
        self.fact(id, "proving time", format!("{:.1}s", proven.prove_secs));
        self.fact(id, "program key", proven.program_vkey.clone());
        let proof_hex = summary["proof"].as_str().unwrap_or("");
        if proof_hex.is_empty() {
            self.fact(id, "proof bytes", "none (practice mode)");
        } else {
            self.fact(
                id,
                "proof bytes",
                format!(
                    "{} · selector 0x{}",
                    proof_hex.len() / 2,
                    &proof_hex[..8.min(proof_hex.len())]
                ),
            );
        }
        let pv = hex::decode(summary["public_values"].as_str().unwrap_or(""))?;
        self.fact(id, "public values", format!("{} bytes", pv.len()));
        if let Ok(o) = decode_pv(&pv) {
            self.fact(
                id,
                "proved: validator set hash",
                format!("0x{}", hex::encode(o.set_hash)),
            );
            self.fact(id, "proved: shred version", o.shred_version.to_string());
            self.fact(id, "proved: slot", commas(o.slot));
            self.fact(
                id,
                "proved: block_id",
                format!("0x{}", hex::encode(o.block_id)),
            );
            self.fact(
                id,
                "proved: finality path",
                if o.slow {
                    "slow (60% + 60%)"
                } else {
                    "fast (80%)"
                },
            );
            self.fact(
                id,
                "proved: stake",
                format!(
                    "{} of {} = {}",
                    commas(o.signed),
                    commas(o.total),
                    pct(o.signed, o.total)
                ),
            );
            self.fact(
                id,
                "proved: position",
                format!(
                    "component starts at shred {} · entry {} · transaction {}",
                    o.comp, o.entry, o.tx_index
                ),
            );
            let contains = parse_signature(&sig)
                .map(|s| o.tx.windows(64).any(|w| w == s.as_slice()))
                .unwrap_or(false);
            self.fact(
                id,
                "proved: transaction",
                format!(
                    "{} bytes · contains your signature: {}",
                    o.tx.len(),
                    if contains { "yes ✓" } else { "NO" }
                ),
            );
            self.fact(id, "public values (hex)", format!("0x{}", hex::encode(&pv)));
        }
        Ok(format!(
            "{} proof in {:.1}s, {} cycles",
            mode.as_str(),
            proven.prove_secs,
            e.total_cycles
        ))
    }

    // ---- EVM ------------------------------------------------------------------------

    async fn anvil(&mut self) -> Result<String> {
        let id = StepId::Anvil;
        let rpc = self.settings.evm_rpc();
        if let Ok(out) = self
            .cmd(
                id,
                "cast",
                &["chain-id", "--rpc-url", &rpc],
                None,
                &[],
                true,
            )
            .await
        {
            self.log(id, format!("$ cast chain-id --rpc-url {rpc}"));
            let chain: u64 = out.trim().parse()?;
            self.state.chain_id = Some(chain);
            self.fact(id, "EVM chain", format!("{chain} ({})", chain_label(chain)));
            self.fact(id, "RPC", rpc.clone());
            if let Ok(b) = self
                .cmd(
                    id,
                    "cast",
                    &["block-number", "--rpc-url", &rpc],
                    None,
                    &[],
                    true,
                )
                .await
            {
                self.fact(id, "latest block", b.trim().to_string());
            }
            return Ok(format!("chain {chain} at {rpc}"));
        }
        ensure!(self.settings.rpc_is_this_machine(), "no chain at {rpc}");
        self.log(
            id,
            "$ anvil --silent   (nothing at the default RPC; starting one)",
        );
        // Left running after huginn-evm exits, so later commands reach the same chain.
        // Stop it with `pkill anvil`.
        Command::new("anvil")
            .arg("--silent")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.log(
            id,
            "  it keeps running after huginn-evm exits (stop it: pkill anvil)",
        );
        for _ in 0..50 {
            if let Ok(out) = self
                .cmd(
                    id,
                    "cast",
                    &["chain-id", "--rpc-url", &rpc],
                    None,
                    &[],
                    true,
                )
                .await
            {
                let chain: u64 = out.trim().parse()?;
                self.state.chain_id = Some(chain);
                self.fact(
                    id,
                    "EVM chain",
                    format!("{chain} ({}), started just now", chain_label(chain)),
                );
                self.fact(id, "RPC", rpc.clone());
                return Ok(format!("anvil started, chain {chain}"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("anvil did not start")
    }

    /// The EVM chain's id, asked from the chain itself.
    async fn chain_id(&self, id: StepId) -> Result<u64> {
        let rpc = self.settings.evm_rpc();
        let out = self
            .cmd(
                id,
                "cast",
                &["chain-id", "--rpc-url", &rpc],
                None,
                &[],
                true,
            )
            .await
            .with_context(|| format!("no EVM chain at {rpc}"))?;
        Ok(out.trim().parse()?)
    }

    /// The environment forge scripts read. The key travels here, never on a command line.
    async fn forge_env(&self, id: StepId) -> Result<Vec<(String, String)>> {
        let mut env: Vec<(String, String)> = self
            .settings
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let chain = self.chain_id(id).await?;
        env.push(("PRIVATE_KEY".into(), self.settings.private_key(chain)?));
        env.push(("RPC_URL".into(), self.settings.evm_rpc()));
        if let Some(vkey) = &self.state.program_vkey {
            env.push(("PROGRAM_VKEY".into(), vkey.clone()));
        }
        if let Some(client) = &self.state.client {
            env.push(("HUGINN_CLIENT".into(), client.clone()));
        }
        // Devnet: the light client starts from the cluster's current epoch.
        if let (Some(epoch), Some(hash)) = (self.state.epoch, &self.state.set_hash) {
            env.push(("EPOCH".into(), epoch.to_string()));
            env.push(("VALIDATOR_SET_HASH".into(), hash.clone()));
        }
        env.push(("PROOF_FILE".into(), "proofs/proof.json".into()));
        Ok(env)
    }

    async fn deploy(&mut self) -> Result<String> {
        let id = StepId::Deploy;
        let chain = self.chain_id(id).await?;
        // The light client starts from the cluster's current epoch and its validator set, read
        // from the live chain (not from .env, not from the repo's fixtures).
        self.log(
            id,
            format!(
                "reading the current epoch's validator set from {}",
                self.settings.solana_rpc
            ),
        );
        let (epoch, ranked, stake, hash, shred) = self.fetch_set(id).await?;
        self.state.epoch = Some(epoch);
        self.state.set_hash = Some(format!("0x{hash}"));
        let shred_version = Some(shred);
        self.fact(id, "Solana epoch", epoch.to_string());
        self.fact(
            id,
            "validators ranked",
            format!("{ranked} (with a BLS key, by stake)"),
        );
        self.fact(id, "total stake", commas(stake));
        self.fact(id, "validator set hash", format!("0x{hash}"));
        self.fact(id, "shred version", shred.to_string());
        // The program key depends only on the program, not on a proof: compute it now.
        self.log(
            id,
            "huginn.program_vkey()  (the SP1 program's fingerprint; no proof needed)",
        );
        std::env::set_var("SP1_PROVER", "mock");
        let vkey = Huginn::from_env().await.program_vkey().await?;
        self.log(id, format!("  {vkey}"));
        self.fact(id, "program key", vkey.clone());
        self.state.program_vkey = Some(vkey);
        let mut env = self.forge_env(id).await?;
        if let Some(sv) = shred_version {
            env.retain(|(k, _)| k != "SHRED_VERSION");
            env.push(("SHRED_VERSION".into(), sv.to_string()));
        }
        // A real Groth16 proof on anvil needs SP1's real verifier, not the stand-in.
        if self.settings.mode == ProofMode::Groth16 {
            env.push(("LOCAL_VERIFIER".into(), "groth16".into()));
        }
        let rpc = self.settings.evm_rpc();
        let dir = self.settings.evm_dir.clone();
        let out = self
            .forge(id, "script/Deploy.s.sol", &rpc, &dir, &env)
            .await?;
        let client = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("SolanaLightClient:"))
            .map(|a| a.trim().to_string())
            .context("no SolanaLightClient address in forge's output")?;
        self.state.client = Some(client.clone());
        self.state.submit_gas = None;
        self.state.verified = None;
        self.state.greeter = None;
        self.state.greeting = None;
        self.state.forged_rejected = None;
        self.fact_link(
            id,
            "SolanaLightClient",
            client.clone(),
            evm_addr_link(chain, &client),
        );
        self.broadcast_facts(id, "Deploy.s.sol", chain);
        // What the chain itself now says about the contract: compare with the values above.
        for (label, sig) in [
            ("on-chain: programVKey()", "programVKey()(bytes32)"),
            ("on-chain: shredVersion()", "shredVersion()(uint16)"),
            ("on-chain: slotsPerEpoch()", "slotsPerEpoch()(uint64)"),
            ("on-chain: verifier()", "verifier()(address)"),
            ("on-chain: setDelay() seconds", "setDelay()(uint64)"),
            ("on-chain: owner()", "owner()(address)"),
        ] {
            if let Ok(v) = self.read(id, &client, sig, &[]).await {
                self.fact(id, label, v);
            }
        }
        if let Some(epoch) = self.state.epoch {
            if let Ok(v) = self
                .read(
                    id,
                    &client,
                    "validatorSetHash(uint64)(bytes32)",
                    &[&epoch.to_string()],
                )
                .await
            {
                self.fact(id, &format!("on-chain: validatorSetHash({epoch})"), v);
            }
        }
        Ok(format!("SolanaLightClient at {client}"))
    }

    /// The transactions a forge script sent, from forge's own broadcast record.
    fn broadcast_facts(&mut self, id: StepId, script: &str, chain: u64) {
        let Some(v) = last_broadcast(&self.settings.evm_dir, script, chain) else {
            return;
        };
        let mut gas_total = 0u64;
        for r in v["receipts"].as_array().cloned().unwrap_or_default() {
            gas_total += r["gasUsed"]
                .as_str()
                .and_then(|g| u64::from_str_radix(g.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
        }
        for t in v["transactions"].as_array().cloned().unwrap_or_default() {
            let hash = t["hash"].as_str().unwrap_or("?").to_string();
            let what = t["contractName"]
                .as_str()
                .map(|n| format!("deploy {n}"))
                .or_else(|| t["function"].as_str().map(|f| f.to_string()))
                .unwrap_or_else(|| "transaction".into());
            self.fact_link(
                id,
                &format!("tx: {what}"),
                hash.clone(),
                evm_tx_link(chain, &hash),
            );
        }
        if gas_total > 0 {
            self.fact(id, "gas used (all txs)", commas(gas_total));
        }
    }

    async fn submit(&mut self) -> Result<String> {
        let id = StepId::Submit;
        ensure!(self.state.client.is_some(), "deploy first");
        ensure!(self.state.proof_file.is_some(), "prove first");
        let env = self.forge_env(id).await?;
        let rpc = self.settings.evm_rpc();
        let dir = self.settings.evm_dir.clone();
        let out = self
            .forge(id, "script/Submit.s.sol", &rpc, &dir, &env)
            .await?;
        ensure!(
            out.contains("verified Solana slot"),
            "the contract did not confirm the slot"
        );
        let chain = self.chain();
        let gas = last_gas(&dir, "Submit.s.sol", chain);
        self.state.submit_gas = gas;
        self.broadcast_facts(id, "Submit.s.sol", chain);
        if let Some(g) = gas {
            self.fact(id, "gas for the proof check + record", commas(g));
        }
        Ok(format!(
            "accepted{}",
            gas.map(|g| format!(", {g} gas")).unwrap_or_default()
        ))
    }

    async fn check(&mut self) -> Result<String> {
        let id = StepId::Check;
        let client = self.state.client.clone().context("deploy first")?;
        let sig = self
            .state
            .signature
            .clone()
            .context("send a transaction first")?;
        let sig_hex = format!("0x{}", hex::encode(parse_signature(&sig)?));
        let rpc = self.settings.evm_rpc();
        let out = self
            .cmd(
                id,
                "cast",
                &[
                    "call",
                    &client,
                    "isVerified(bytes)(bool)",
                    &sig_hex,
                    "--rpc-url",
                    &rpc,
                ],
                None,
                &[],
                false,
            )
            .await?;
        let verified = out.trim() == "true";
        self.state.verified = Some(verified);
        self.fact(id, "isVerified(signature)", out.trim().to_string());
        self.fact(id, "signature as bytes", sig_hex.clone());
        let key = self
            .cmd(id, "cast", &["keccak", &sig_hex], None, &[], true)
            .await?;
        if let Ok(slot) = self
            .read(
                id,
                &client,
                "transactionSlot(bytes32)(uint64)",
                &[key.trim()],
            )
            .await
        {
            self.fact(id, "transactionSlot(keccak(signature))", slot.clone());
            if let (Some(want), Ok(got)) = (self.state.slot, slot.parse::<u64>()) {
                self.fact(
                    id,
                    "matches the finalized slot",
                    if got == want { "yes ✓" } else { "NO" },
                );
            }
        }
        if let (Some(slot), Some(set)) = (self.state.slot, self.state.set_hash.clone().or(None)) {
            if let Ok(b) = self
                .read(
                    id,
                    &client,
                    "finalizedBlock(uint64,bytes32)(bytes32)",
                    &[&slot.to_string(), &set],
                )
                .await
            {
                self.fact(id, "finalizedBlock(slot, set)", b);
            }
        } else if let Some(slot) = self.state.slot {
            let epoch = slot / 432_000;
            if let Ok(set) = self
                .read(
                    id,
                    &client,
                    "validatorSetHash(uint64)(bytes32)",
                    &[&epoch.to_string()],
                )
                .await
            {
                if let Ok(b) = self
                    .read(
                        id,
                        &client,
                        "finalizedBlock(uint64,bytes32)(bytes32)",
                        &[&slot.to_string(), &set],
                    )
                    .await
                {
                    self.fact(id, "finalizedBlock(slot, set)", b);
                }
            }
        }
        ensure!(
            verified,
            "the contract says this transaction is not verified"
        );
        Ok("isVerified(signature) = true".into())
    }

    async fn forged(&mut self) -> Result<String> {
        let id = StepId::Forged;
        let client = self.state.client.clone().context("deploy first")?;
        let file = self.state.proof_file.clone().context("prove first")?;
        let rpc = self.settings.evm_rpc();
        let (pv, proof) = self.proof_bytes(id, &client, &file).await?;
        let proof_hex = format!("0x{}", hex::encode(&proof));

        self.log(id, "1) the real output with its proof:");
        self.cmd(
            id,
            "cast",
            &[
                "call",
                &client,
                "verify(bytes,bytes)",
                &format!("0x{}", hex::encode(&pv)),
                &proof_hex,
                "--rpc-url",
                &rpc,
            ],
            None,
            &[],
            false,
        )
        .await
        .context("the real proof should pass")?;
        self.log(id, "   accepted");
        self.fact(id, "real output + proof", "accepted by verify(bytes,bytes)");

        let mut forged = pv.clone();
        let last = forged.len() - 1;
        let before = forged[last] as char;
        forged[last] ^= 0x01;
        self.log(
            id,
            format!(
                "2) the same proof, memo's last character changed '{before}' -> '{}':",
                forged[last] as char
            ),
        );
        self.fact(
            id,
            "forged byte",
            format!(
                "offset {last} of {}: 0x{:02x} '{before}' → 0x{:02x} '{}'",
                pv.len(),
                pv[last],
                forged[last],
                forged[last] as char
            ),
        );
        match self
            .cmd(
                id,
                "cast",
                &[
                    "call",
                    &client,
                    "verify(bytes,bytes)",
                    &format!("0x{}", hex::encode(&forged)),
                    &proof_hex,
                    "--rpc-url",
                    &rpc,
                ],
                None,
                &[],
                false,
            )
            .await
        {
            Ok(_) => {
                self.state.forged_rejected = Some(false);
                bail!("the forged output was ACCEPTED")
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let reason = msg
                    .split("execution reverted: ")
                    .nth(1)
                    .map(|r| r.split([',', '|']).next().unwrap_or(r).trim().to_string());
                let reason = reason.unwrap_or(msg);
                self.log(id, format!("   rejected: {reason}"));
                self.fact(
                    id,
                    "forged output + same proof",
                    format!("REJECTED: {reason}"),
                );
                self.state.forged_rejected = Some(true);
                Ok("forged memo rejected, real one accepted".into())
            }
        }
    }

    async fn greeter(&mut self) -> Result<String> {
        let id = StepId::Greeter;
        ensure!(self.state.client.is_some(), "deploy the light client first");
        let address = self
            .cmd(id, "solana", &["address"], None, &[], false)
            .await?;
        let key = bs58::decode(address.trim()).into_vec()?;
        ensure!(key.len() == 32, "solana address is not 32 bytes");
        self.log(
            id,
            format!("  controller = your Solana key, 0x{}", hex::encode(&key)),
        );
        self.fact(id, "controller (Solana key)", address.trim().to_string());
        self.fact(
            id,
            "controller as bytes32",
            format!("0x{}", hex::encode(&key)),
        );
        let mut env = self.forge_env(id).await?;
        env.push((
            "SOLANA_CONTROLLER".into(),
            format!("0x{}", hex::encode(&key)),
        ));
        let rpc = self.settings.evm_rpc();
        let dir = self.settings.evm_dir.clone();
        let out = self
            .forge(id, "script/DeployGreeter.s.sol", &rpc, &dir, &env)
            .await?;
        let greeter = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("SolanaGreeter:"))
            .map(|a| a.trim().to_string())
            .context("no SolanaGreeter address in forge's output")?;
        let greeting = self
            .cmd(
                id,
                "cast",
                &["call", &greeter, "greeting()(string)", "--rpc-url", &rpc],
                None,
                &[],
                false,
            )
            .await?;
        self.state.greeter = Some(greeter.clone());
        self.state.greeting = Some(unquote(&greeting));
        let chain = self.chain();
        self.fact_link(
            id,
            "SolanaGreeter",
            greeter.clone(),
            evm_addr_link(chain, &greeter),
        );
        self.broadcast_facts(id, "DeployGreeter.s.sol", chain);
        self.fact(id, "greeting now", greeting.trim().to_string());
        if let Ok(p) = self.read(id, &greeter, "memoPrefix()(string)", &[]).await {
            self.fact(id, "memo must start with", p);
        }
        if let Ok(n) = self.read(id, &greeter, "nonce()(uint64)", &[]).await {
            self.fact(id, "nonce (next memo uses +1)", n);
        }
        Ok(format!(
            "SolanaGreeter at {greeter}, greeting {}",
            greeting.trim()
        ))
    }

    async fn command(&mut self) -> Result<String> {
        let id = StepId::Command;
        let greeter = self
            .state
            .greeter
            .clone()
            .context("deploy the greeter first")?;
        let client = self.state.client.clone().context("deploy first")?;
        let file = self.state.proof_file.clone().context("prove first")?;
        let rpc = self.settings.evm_rpc();
        let (pv, proof) = self.proof_bytes(id, &client, &file).await?;
        let mut env = self.forge_env(id).await?;
        env.push(("GREETER".into(), greeter.clone()));
        env.push(("PUBLIC_VALUES".into(), format!("0x{}", hex::encode(&pv))));
        env.push(("PROOF".into(), format!("0x{}", hex::encode(&proof))));
        let dir = self.settings.evm_dir.clone();
        self.forge(id, "script/Execute.s.sol", &rpc, &dir, &env)
            .await?;
        let greeting = self
            .cmd(
                id,
                "cast",
                &["call", &greeter, "greeting()(string)", "--rpc-url", &rpc],
                None,
                &[],
                false,
            )
            .await?;
        let greeting = unquote(&greeting);
        self.state.greeting = Some(greeting.clone());
        let chain = self.chain();
        self.broadcast_facts(id, "Execute.s.sol", chain);
        self.fact(id, "greeting() now", greeting.clone());
        if let Ok(n) = self.read(id, &greeter, "nonce()(uint64)", &[]).await {
            self.fact(id, "nonce() now", n);
        }
        ensure!(
            greeting == self.settings.memo,
            "greeting is {greeting:?}, the memo said {:?}",
            self.settings.memo
        );
        Ok(format!("greeting is now \"{greeting}\""))
    }

    // ---- helpers --------------------------------------------------------------------

    /// The proof's public values and the bytes the verifier takes. A mock or compressed
    /// proof has no onchain bytes, so the local stand-in verifier's "proof" is used.
    async fn proof_bytes(
        &self,
        id: StepId,
        client: &str,
        file: &Path,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let rpc = self.settings.evm_rpc();
        let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file)?)?;
        let pv = hex::decode(summary["public_values"].as_str().context("public_values")?)?;
        let mut proof = hex::decode(summary["proof"].as_str().unwrap_or(""))?;
        if proof.is_empty() {
            let verifier = self
                .cmd(
                    id,
                    "cast",
                    &["call", client, "verifier()(address)", "--rpc-url", &rpc],
                    None,
                    &[],
                    true,
                )
                .await?;
            let vkey = self
                .cmd(
                    id,
                    "cast",
                    &["call", client, "programVKey()(bytes32)", "--rpc-url", &rpc],
                    None,
                    &[],
                    true,
                )
                .await?;
            let p = self
                .cmd(
                    id,
                    "cast",
                    &[
                        "call",
                        verifier.trim(),
                        "proofFor(bytes32,bytes)(bytes)",
                        vkey.trim(),
                        &format!("0x{}", hex::encode(&pv)),
                        "--rpc-url",
                        &rpc,
                    ],
                    None,
                    &[],
                    true,
                )
                .await?;
            proof = hex::decode(p.trim().trim_start_matches("0x"))?;
        }
        Ok((pv, proof))
    }

    /// `forge script ... --broadcast`, showing only the script's own logs and the result
    /// (forge's full output is in the error if it fails).
    async fn forge(
        &self,
        id: StepId,
        script: &str,
        rpc: &str,
        dir: &Path,
        env: &[(String, String)],
    ) -> Result<String> {
        self.log(
            id,
            format!("$ forge script {script} --rpc-url {rpc} --broadcast"),
        );
        let out = self
            .cmd(
                id,
                "forge",
                &["script", script, "--rpc-url", rpc, "--broadcast"],
                Some(dir),
                env,
                true,
            )
            .await?;
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

    async fn rpc(&self, method: &str, params: &str) -> Result<serde_json::Value> {
        let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#);
        let out = Command::new("curl")
            .args([
                "-s",
                "--max-time",
                "5",
                &self.settings.solana_rpc,
                "-H",
                "content-type: application/json",
                "-d",
                &body,
            ])
            .output()
            .await?;
        ensure!(
            out.status.success(),
            "RPC {} unreachable",
            self.settings.solana_rpc
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        if let Some(e) = v.get("error") {
            bail!("RPC error: {e}");
        }
        Ok(v["result"].clone())
    }

    /// Runs a command, streams its output into the step's log, returns stdout. `quiet`
    /// hides the command line and its output (for checks and polls).
    async fn cmd(
        &self,
        id: StepId,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        env: &[(String, String)],
        quiet: bool,
    ) -> Result<String> {
        if !quiet {
            let shown: Vec<String> = args
                .iter()
                .map(|a| {
                    if a.chars().count() > 42 {
                        format!("{}…", a.chars().take(40).collect::<String>())
                    } else {
                        a.to_string()
                    }
                })
                .collect();
            let name = Path::new(program)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(program.into());
            self.log(id, format!("$ {name} {}", shown.join(" ")));
        }
        let mut c = Command::new(program);
        c.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
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
            if let Some(l) = stderr
                .lines()
                .chain(stdout.lines())
                .find(|l| l.contains("execution reverted"))
            {
                bail!("{}", l.trim().trim_start_matches("Error: "));
            }
            let tail: Vec<&str> = stderr
                .lines()
                .chain(stdout.lines())
                .filter(|l| !l.trim().is_empty())
                .collect();
            bail!(
                "{} failed: {}",
                Path::new(program).file_name().unwrap().to_string_lossy(),
                tail.iter()
                    .rev()
                    .take(2)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        Ok(stdout)
    }
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

fn state_file(settings: &Settings) -> PathBuf {
    settings.evm_dir.join("proofs/state.json")
}

pub fn send_state(runner: &Runner) {
    let _ = runner
        .events
        .send(Event::State(Box::new(runner.state.clone())));
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Long lines (hex return values) cut to fit a screen.
fn clip(s: &str) -> String {
    if s.chars().count() > 150 {
        format!(
            "{}… ({} chars)",
            s.chars().take(140).collect::<String>(),
            s.chars().count()
        )
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
    let extra = [
        ".foundry/bin",
        ".sp1/bin",
        ".local/share/solana/install/active_release/bin",
        ".cargo/bin",
    ]
    .map(|p| home.join(p).display().to_string());
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{path}:{}", extra.join(":")));
    // Homebrew and Docker Desktop.
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!(
            "{path}:/opt/homebrew/bin:/usr/local/bin:{}",
            home.join(".docker/bin").display()
        ),
    );
}

// ---- data helpers -----------------------------------------------------------------------

/// The Huginn program's public values (`HuginnOutput.sol` documents the layout).
pub struct Pv {
    pub set_hash: [u8; 32],
    pub shred_version: u16,
    pub slot: u64,
    pub block_id: [u8; 32],
    pub slow: bool,
    pub signed: u64,
    pub total: u64,
    pub comp: u32,
    pub entry: u32,
    pub tx_index: u32,
    pub tx: Vec<u8>,
}

pub fn decode_pv(b: &[u8]) -> Result<Pv> {
    ensure!(
        b.len() >= 106 && b[0] == 1,
        "unexpected public values ({} bytes)",
        b.len()
    );
    let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
    let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
    let tx_len = u16::from_be_bytes([b[104], b[105]]) as usize;
    ensure!(
        b.len() == 106 + tx_len,
        "public values length {} does not match transaction length {tx_len}",
        b.len()
    );
    Ok(Pv {
        set_hash: b[1..33].try_into().unwrap(),
        shred_version: u16::from_be_bytes([b[33], b[34]]),
        slot: u64_at(35),
        block_id: b[43..75].try_into().unwrap(),
        slow: b[75] == 1,
        signed: u64_at(76),
        total: u64_at(84),
        comp: u32_at(92),
        entry: u32_at(96),
        tx_index: u32_at(100),
        tx: b[106..].to_vec(),
    })
}

/// 519635459 -> "519,635,459".
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "n/a".into();
    }
    format!("{:.2}%", part as f64 * 100.0 / whole as f64)
}

/// Signer ranks from a signer-store bitmap: `version u8 | bits u16 LE | payload`, LSB first.
pub fn signer_ranks(bitmap: &[u8]) -> Vec<usize> {
    let [0, lo, hi, payload @ ..] = bitmap else {
        return vec![];
    };
    let bits = u16::from_le_bytes([*lo, *hi]) as usize;
    (0..bits)
        .filter(|&i| payload.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1))
        .collect()
}

pub fn chain_label(chain: u64) -> &'static str {
    match chain {
        31337 => "local anvil",
        84532 => "Base Sepolia",
        8453 => "Base",
        11155111 => "Ethereum Sepolia",
        1 => "Ethereum",
        _ => "custom chain",
    }
}

fn explorer(chain: u64) -> Option<&'static str> {
    match chain {
        84532 => Some("https://sepolia.basescan.org"),
        8453 => Some("https://basescan.org"),
        11155111 => Some("https://sepolia.etherscan.io"),
        1 => Some("https://etherscan.io"),
        _ => None,
    }
}

pub fn evm_tx_link(chain: u64, hash: &str) -> Option<String> {
    explorer(chain).map(|e| format!("{e}/tx/{hash}"))
}

pub fn evm_addr_link(chain: u64, addr: &str) -> Option<String> {
    explorer(chain).map(|e| format!("{e}/address/{addr}"))
}

/// forge's record of what its last broadcast sent.
fn last_broadcast(dir: &Path, script: &str, chain: u64) -> Option<serde_json::Value> {
    serde_json::from_str(
        &std::fs::read_to_string(dir.join(format!("broadcast/{script}/{chain}/run-latest.json")))
            .ok()?,
    )
    .ok()
}

// ---- the receipt ------------------------------------------------------------------------

impl Runner {
    /// `proofs/receipt.md`: every piece of data the run produced, in order, plus commands to
    /// check it without trusting this program. Rewritten after every step.
    fn write_receipt(&self) {
        let s = &self.state;
        let mut md = String::new();
        md.push_str("# Huginn receipt\n\n");
        md.push_str(&format!(
            "Solana {} → EVM chain {} ({}). Proof mode: {}.\n\n",
            self.settings.cluster.as_str(),
            s.chain_id
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            s.chain_id.map(chain_label).unwrap_or("?"),
            s.proof_mode
                .map(|m| m.as_str())
                .unwrap_or(self.settings.mode.as_str()),
        ));
        match s.proof_mode {
            Some(ProofMode::Groth16) => md.push_str("A real Groth16 proof, checked by the chain's SP1 verifier.\n\n"),
            Some(ProofMode::Compressed) => md.push_str("A real STARK proof, checked on the proving machine only. The contract used a local stand-in verifier.\n\n"),
            Some(ProofMode::Mock) => md.push_str("**Practice run: nothing was proven.** The stand-in verifier accepts anyone's \"proof\".\n\n"),
            None => {}
        }
        for id in StepId::ALL {
            let facts: Vec<&Fact> = s.facts.iter().filter(|f| f.step == id.name()).collect();
            if facts.is_empty() {
                continue;
            }
            md.push_str(&format!("## {}\n\n", id.title()));
            for f in facts {
                match &f.link {
                    Some(l) => md.push_str(&format!("- **{}**: `{}`  \n  {l}\n", f.label, f.value)),
                    None => md.push_str(&format!("- **{}**: `{}`\n", f.label, f.value)),
                }
            }
            md.push('\n');
        }

        md.push_str("## Check it yourself\n\nNone of these use this program.\n\n```bash\n");
        if let Some(sig) = &s.signature {
            md.push_str("# 1. The Solana transaction exists and is finalized\n");
            md.push_str(&format!(
                "solana confirm -v {sig} --url {}\n",
                self.settings.solana_rpc
            ));
            md.push_str(&format!(
                "# or open: {}\n\n",
                self.settings.solana_tx_link(sig)
            ));
        }
        if self.is_devnet() {
            md.push_str("# 2. The validator set the proof was checked against (compare with 'validator set hash')\n");
            md.push_str("cargo run --release -p fetch -- validator-set --cluster devnet   # in the huginn repo, during the same epoch\n\n");
        }
        if let (Some(client), Some(sig)) = (&s.client, &s.signature) {
            let rpc = self.settings.evm_rpc();
            if let Ok(bytes) = parse_signature(sig) {
                let sig_hex = format!("0x{}", hex::encode(bytes));
                md.push_str("# 3. The contract recorded it\n");
                md.push_str(&format!(
                    "cast call {client} \"isVerified(bytes)(bool)\" {sig_hex} --rpc-url {rpc}\n"
                ));
                md.push_str(&format!("cast call {client} \"transactionSlot(bytes32)(uint64)\" $(cast keccak {sig_hex}) --rpc-url {rpc}\n"));
                md.push_str(&format!("cast call {client} \"programVKey()(bytes32)\" --rpc-url {rpc}   # must equal 'program key'\n"));
            }
        }
        if let Some(g) = &s.greeter {
            md.push_str(&format!(
                "cast call {g} \"greeting()(string)\" --rpc-url {}\n",
                self.settings.evm_rpc()
            ));
        }
        md.push_str("```\n");
        let _ = std::fs::write(self.settings.evm_dir.join("proofs/receipt.md"), md);
    }
}
