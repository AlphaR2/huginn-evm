//! Makes the proof an EVM chain needs, with huginn as a library:
//!
//!   huginn-evm-relayer vkey
//!   huginn-evm-relayer prove --block block_46718.json --signature 4F39ig6… --mode groth16
//!
//! The proof file is what `script/Submit.s.sol` sends to `SolanaLightClient`. Where proving
//! runs follows SP1's environment (`SP1_PROVER=cpu|mock`); Groth16 on the CPU needs Docker.

use {
    anyhow::Result,
    clap::{Parser, Subcommand},
    huginn_prover::{tx_input_from_block_file, Huginn, Mode},
};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the Huginn program key (PROGRAM_VKEY in .env).
    Vkey,
    /// Prove one transaction and write the proof file.
    Prove {
        /// Block file from huginn's `ledger-dump`.
        #[arg(long)]
        block: String,
        /// Transaction signature, base58 or hex.
        #[arg(long)]
        signature: String,
        /// groth16 for a real chain; compressed works with the anvil stand-in verifier.
        #[arg(long, default_value = "groth16")]
        mode: Mode,
        #[arg(long, default_value = "../proofs/proof.json")]
        out: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let huginn = Huginn::from_env().await;
    match cli.command {
        Command::Vkey => println!("{}", huginn.program_vkey().await?),
        Command::Prove { block, signature, mode, out } => {
            let input = tx_input_from_block_file(&block, &signature)?;
            let proven = huginn.prove(&input, mode).await?;
            std::fs::write(&out, serde_json::to_string_pretty(&proven.summary(&signature))? + "\n")?;
            println!("{} proof in {:.0}s -> {out}", mode.as_str(), proven.prove_secs);
        }
    }
    Ok(())
}
