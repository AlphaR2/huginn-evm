//! `huginn-evm`: prove Solana transactions to EVM chains with huginn.
//!
//!   huginn-evm                      the TUI: the whole flow, step by step
//!   huginn-evm run                  the same steps without the TUI (all, or some by name)
//!   huginn-evm run send finalize --memo "gm"
//!   huginn-evm vkey                 the Huginn program key (PROGRAM_VKEY in .env)
//!
//! Run it from this repo (it reads .env and the forge scripts here). huginn is expected
//! next to it (../huginn), or pass --huginn.

mod safe;
mod steps;
mod tui;

use {
    anyhow::{bail, Result},
    clap::{Parser, Subcommand},
    std::path::PathBuf,
    steps::{Event, ProofMode, Runner, Settings, StepId},
};

#[derive(Parser)]
#[command(about = "Prove Solana transactions to EVM chains with huginn")]
struct Cli {
    /// The huginn checkout (default: ../huginn next to this repo).
    #[arg(long, global = true)]
    huginn: Option<PathBuf>,
    /// Memo text for the Solana transaction.
    #[arg(long, global = true, default_value = "hello from solana")]
    memo: String,
    /// Proof mode: mock (seconds), compressed (real STARK), groth16 (real, for real chains).
    #[arg(long, global = true, default_value = "mock")]
    mode: String,
    /// Where certificates come from: shreds (the next block's footer) or plugin (footer-dump).
    #[arg(long, global = true, default_value = "shreds")]
    certs: String,
    /// The Solana cluster: local (solana-test-validator --alpenglow) or devnet (shreds from
    /// public validators over the repair protocol, no node needed).
    #[arg(long, global = true, default_value = "local")]
    cluster: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run steps without the TUI: all of them, or the ones named (doctor send finalize fetch
    /// prove anvil deploy submit check forged), in order.
    Run { steps: Vec<String> },
    /// Print the Huginn program key.
    Vkey,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {}", safe::redact(&format!("{e:#}")));
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    steps::extend_path();
    let cli = Cli::parse();
    let evm_dir = std::env::current_dir()?;
    if !evm_dir.join("script/Deploy.s.sol").exists() {
        bail!("run huginn-evm from the huginn-evm repo (it needs .env and script/)");
    }
    let mut settings = Settings::load(evm_dir, cli.huginn)?;
    settings.memo = cli.memo;
    settings.mode = ProofMode::parse(&cli.mode)?;
    settings.certs = steps::CertSource::parse(&cli.certs)?;
    settings.set_cluster(steps::Cluster::parse(&cli.cluster)?);

    match cli.command {
        None => tui::run(settings),
        Some(Command::Vkey) => tokio::runtime::Runtime::new()?.block_on(async {
            println!(
                "{}",
                huginn_prover::Huginn::from_env()
                    .await
                    .program_vkey()
                    .await?
            );
            Ok(())
        }),
        Some(Command::Run { steps }) => {
            let ids: Vec<StepId> = if steps.is_empty() {
                StepId::ALL.to_vec()
            } else {
                steps
                    .iter()
                    .map(|s| {
                        StepId::from_name(s).ok_or_else(|| anyhow::anyhow!("unknown step {s}"))
                    })
                    .collect::<Result<_>>()?
            };
            tokio::runtime::Runtime::new()?.block_on(headless(settings, ids))
        }
    }
}

/// Prints each step's log as it happens.
async fn headless(settings: Settings, ids: Vec<StepId>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let mut failed = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Started(id) => println!("\n▶ {}  {}", id.name(), id.title()),
                Event::Log(_, line) => println!("  {line}"),
                Event::Done(id, Ok(summary), took) => {
                    println!("✓ {}  {summary}  ({:.1}s)", id.name(), took.as_secs_f64())
                }
                Event::Done(id, Err(e), _) => {
                    println!("✗ {}  {e}", id.name());
                    failed = true;
                }
                Event::State(_) => {}
                Event::Fact(f) => {
                    println!("    {:<30} {}", f.label, f.value);
                    if let Some(l) = f.link {
                        println!("    {:<30} {l}", "");
                    }
                }
                Event::Finished => break,
            }
        }
        failed
    });
    let mut runner = Runner::new(settings, tx);
    for id in ids {
        if !runner.run(id).await {
            break;
        }
    }
    runner.finished();
    if printer.await? {
        bail!("a step failed");
    }
    Ok(())
}
