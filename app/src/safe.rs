//! Small, pure helpers that keep secrets out of logs, files and `ps`, and keep chain math
//! honest. Kept apart from `steps.rs` so each one has a unit test.

use {
    anyhow::{anyhow, bail, ensure, Result},
    serde_json::{json, Value},
    std::path::Path,
};

/// Scrubs RPC URLs (down to the host) and key-looking values from any text we are about to
/// show, log or write. Every fact, log line, state file and receipt goes through this.
pub fn redact(text: &str) -> String {
    redact::redact_text(text)
}

/// A URL that carries nothing secret: no user info, no query, no path beyond `/`.
/// Public endpoints and localhost qualify; a keyed provider URL does not.
pub fn is_plain_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    // Anything beyond a bare host[:port] (a path, a query, a fragment, user info) may carry a
    // key: `https://host?api-key=K` has no `/` but is keyed all the same.
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    !rest.is_empty() && !rest.contains(['/', '?', '#', '@'])
}

/// Pulls `--rpc-url <url>` and `--rpc <url>` out of a command's arguments. The URL moves to
/// environment variables instead, because command-line arguments are visible to every user
/// through `ps`:
///   - `--rpc-url` sets `ETH_RPC_URL` (read by `cast`) and `FOUNDRY_ETH_RPC_URL` (Foundry's
///     config value, read by `forge script`). `forge script` ignores `ETH_RPC_URL`: with only
///     that set it runs on an empty in-memory chain (id 31337, every balance 0) instead of
///     the real one, without any error (seen 14 Nov 2026).
///   - `--rpc` sets `SOLANA_RPC` (read by the huginn tools).
pub fn split_rpc_args(args: &[&str]) -> (Vec<String>, Vec<(String, String)>) {
    let (mut kept, mut env) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let vars: &[&str] = match args[i] {
            "--rpc-url" => &["ETH_RPC_URL", "FOUNDRY_ETH_RPC_URL"],
            "--rpc" => &["SOLANA_RPC"],
            _ => &[],
        };
        match (vars.is_empty(), args.get(i + 1)) {
            (false, Some(url)) => {
                env.extend(vars.iter().map(|v| (v.to_string(), url.to_string())));
                i += 2;
            }
            _ => {
                kept.push(args[i].to_string());
                i += 1;
            }
        }
    }
    (kept, env)
}

/// The only variables a child process inherits besides what the step passes explicitly.
/// Everything else (`PRIVATE_KEY`, `NETWORK_PRIVATE_KEY`, other tools' API keys, …) stays out.
pub fn base_env() -> Vec<(String, String)> {
    const KEEP: [&str; 14] = [
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "TERM",
        "SSL_CERT_FILE", "SSL_CERT_DIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "RUST_LOG",
    ];
    std::env::vars()
        .filter(|(k, _)| KEEP.contains(&k.as_str()) || k.starts_with("FOUNDRY_") || k.starts_with("LC_"))
        .collect()
}

/// Which SP1 prover to use. Mock proofs are for tests; real proofs follow the user's own
/// `SP1_PROVER` (`cpu`, `cuda`, `network`), defaulting to `cpu`. We never override a choice.
pub fn sp1_prover(real_proof: bool, user: Option<&str>) -> Result<String> {
    if !real_proof {
        return Ok("mock".into());
    }
    match user.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok("cpu".into()),
        Some("mock") => bail!("SP1_PROVER=mock cannot make a real proof; unset it or use cpu/cuda/network"),
        Some(p @ ("cpu" | "cuda" | "network")) => Ok(p.into()),
        Some(other) => bail!("SP1_PROVER={other} is not one of cpu, cuda, network"),
    }
}

/// `getEpochSchedule`, with the rules Solana uses to turn a slot into an epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochSchedule {
    pub slots_per_epoch: u64,
    pub warmup: bool,
    pub first_normal_epoch: u64,
    pub first_normal_slot: u64,
}

const MINIMUM_SLOTS_PER_EPOCH: u64 = 32;

impl EpochSchedule {
    pub fn from_rpc(v: &Value) -> Result<Self> {
        let n = |k: &str| v[k].as_u64().ok_or_else(|| anyhow!("getEpochSchedule: no {k}"));
        let s = EpochSchedule {
            slots_per_epoch: n("slotsPerEpoch")?,
            warmup: v["warmup"].as_bool().unwrap_or(false),
            first_normal_epoch: n("firstNormalEpoch")?,
            first_normal_slot: n("firstNormalSlot")?,
        };
        ensure!(s.slots_per_epoch > 0, "getEpochSchedule: slotsPerEpoch is 0");
        Ok(s)
    }

    /// The epoch a slot belongs to, including the short warmup epochs some clusters start with.
    pub fn epoch_of(&self, slot: u64) -> u64 {
        if slot < self.first_normal_slot {
            // Warmup epoch lengths double: 32, 64, 128, ... (Solana's `EpochSchedule`).
            let n = slot + MINIMUM_SLOTS_PER_EPOCH + 1;
            (63 - n.next_power_of_two().leading_zeros() as u64)
                .saturating_sub(MINIMUM_SLOTS_PER_EPOCH.trailing_zeros() as u64 + 1)
                .min(self.first_normal_epoch)
        } else {
            self.first_normal_epoch + (slot - self.first_normal_slot) / self.slots_per_epoch
        }
    }

    /// The epoch the light client will compute for `slot`. The contract follows Solana's
    /// EpochSchedule (warmup included, deployed with `WARMUP`), so this is `epoch_of`.
    pub fn check_contract_agrees(&self, slot: u64) -> Result<u64> {
        Ok(self.epoch_of(slot))
    }
}

/// Default `SET_DELAY` off anvil: two hours, a small slice of a ~1-day epoch.
pub const DEFAULT_SET_DELAY_SECS: u64 = 2 * 3600;
/// A proposed set may wait at most this fraction of an epoch (1/8 ≈ 3 h of a 25.6 h epoch).
pub const MAX_DELAY_FRACTION: f64 = 0.125;

/// Average milliseconds per slot from `getRecentPerformanceSamples`.
pub fn ms_per_slot(samples: &Value) -> Result<f64> {
    let (mut secs, mut slots) = (0u64, 0u64);
    for s in samples.as_array().ok_or_else(|| anyhow!("getRecentPerformanceSamples: not a list"))? {
        secs += s["samplePeriodSecs"].as_u64().unwrap_or(0);
        slots += s["numSlots"].as_u64().unwrap_or(0);
    }
    ensure!(slots > 0, "getRecentPerformanceSamples: no slots");
    Ok(secs as f64 * 1000.0 / slots as f64)
}

/// Refuses a set delay too long for the epoch: a set proposed at the start of an epoch must
/// count well before the epoch ends. Returns the epoch length in hours, for display.
pub fn check_set_delay(delay_secs: u64, slots_per_epoch: u64, ms_per_slot: f64) -> Result<f64> {
    let epoch_secs = slots_per_epoch as f64 * ms_per_slot / 1000.0;
    ensure!(
        delay_secs as f64 <= epoch_secs * MAX_DELAY_FRACTION,
        "SET_DELAY is {delay_secs} s, but an epoch is only about {:.1} h here: a proposed set must count within {:.0}% of an epoch ({:.0} s). Lower SET_DELAY (default {DEFAULT_SET_DELAY_SECS} s).",
        epoch_secs / 3600.0,
        MAX_DELAY_FRACTION * 100.0,
        epoch_secs * MAX_DELAY_FRACTION
    );
    Ok(epoch_secs / 3600.0)
}

/// True only if `check-set` passed for exactly this set file: same epoch, same commitment,
/// `pass == true`. A set file that merely exists proves nothing.
pub fn set_is_verified(dir: &Path, epoch: u64) -> bool {
    let read = |name: String| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(dir.join(name)).ok()?).ok()
    };
    let (Some(set), Some(rep)) = (
        read(format!("validator_set_{epoch}.json")),
        read(format!("check_set_{epoch}.json")),
    ) else {
        return false;
    };
    set["validator_set"]["epoch"].as_u64() == Some(epoch)
        && rep["epoch"].as_u64() == Some(epoch)
        && rep["pass"] == json!(true)
        && !set["commitment"].is_null()
        && rep["commitment"] == set["commitment"]
}

/// A short-lived `solana` CLI config file, for RPC URLs that carry a key. The `solana` CLI
/// only takes a URL as `--url` (visible in `ps`) or from a config file, so for a keyed URL we
/// write a private (0600) config, point `--config` at it, and delete it when this is dropped.
pub struct SolanaConfig(std::path::PathBuf);

impl SolanaConfig {
    pub fn create(url: &str, keypair_path: &str) -> Result<Self> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let path = std::env::temp_dir().join(format!("huginn-solana-{}-{:x}.yml", std::process::id(), nonce()));
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        // JSON strings are valid YAML, and quote the values safely. The CLI needs every field
        // its config has: with `websocket_url` missing it can't parse the file and silently
        // uses its defaults (mainnet), seen 14 Nov 2026. An empty websocket_url means "derive
        // it from json_rpc_url". Callers still check the result with `rpc_url_matches`.
        writeln!(
            f,
            "json_rpc_url: {}\nwebsocket_url: \"\"\nkeypair_path: {}\naddress_labels: {{}}\ncommitment: confirmed",
            json!(url),
            json!(keypair_path)
        )?;
        Ok(SolanaConfig(path))
    }
    pub fn path(&self) -> String {
        self.0.display().to_string()
    }
}

/// Whether `solana config get --config <file>` output shows exactly this RPC URL, i.e. the
/// CLI really read our file instead of falling back to its defaults.
pub fn rpc_url_matches(config_get_output: &str, url: &str) -> bool {
    config_get_output
        .lines()
        .find_map(|l| l.strip_prefix("RPC URL:"))
        .is_some_and(|v| v.trim() == url)
}

impl Drop for SolanaConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn nonce() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// One JSON-RPC call over HTTP, in-process. Errors name the host only.
pub fn http_rpc(url: &str, method: &str, params: Value, timeout_secs: u64) -> Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp: Value = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .post(url)
        .send_json(body)
        .map_err(|e| anyhow!("RPC {method} to {} failed: {}", redact::redact_url(url), redact(&e.to_string())))?
        .into_json()
        .map_err(|e| anyhow!("RPC {method} from {}: bad JSON: {e}", redact::redact_url(url)))?;
    if let Some(e) = resp.get("error") {
        bail!("RPC {method} error: {}", redact(&e.to_string()));
    }
    Ok(resp["result"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_hides_keys_in_urls_and_text() {
        let r = redact("fetch via https://mainnet.helius-rpc.com/?api-key=SECRETVALUE failed");
        assert!(!r.contains("SECRETVALUE"), "{r}");
        assert!(r.contains("mainnet.helius-rpc.com"));
        assert!(!redact("--rpc-url https://eth.example/v2/KEYINPATH").contains("KEYINPATH"));
        assert_eq!(redact("http://127.0.0.1:8545"), "http://127.0.0.1:8545");
    }

    #[test]
    fn plain_urls_are_the_keyless_ones() {
        assert!(is_plain_url("http://127.0.0.1:8899"));
        assert!(is_plain_url("https://api.devnet.solana.com"));
        assert!(!is_plain_url("https://x.helius-rpc.com/?api-key=K"));
        assert!(!is_plain_url("https://rpc.example/v2/KEY"));
        assert!(!is_plain_url("https://user:pw@rpc.example"));
        assert!(is_plain_url("https://api.devnet.solana.com/"));
        // A key in a query with no path slash is still a key.
        assert!(!is_plain_url("https://rpc.example?api-key=K"));
        assert!(!is_plain_url("https://rpc.example#K"));
        assert!(!is_plain_url("https://"));
    }

    #[test]
    fn rpc_urls_move_from_arguments_to_environment() {
        let (args, env) = split_rpc_args(&["call", "0xabc", "f()", "--rpc-url", "https://e.io/KEY"]);
        assert_eq!(args, ["call", "0xabc", "f()"]);
        let url = "https://e.io/KEY".to_string();
        assert_eq!(
            env,
            [("ETH_RPC_URL".to_string(), url.clone()), ("FOUNDRY_ETH_RPC_URL".to_string(), url)]
        );
        let (args, env) = split_rpc_args(&["--rpc", "https://s.io/K", "--out", "x", "fetch"]);
        assert_eq!(args, ["--out", "x", "fetch"]);
        assert_eq!(env[0].0, "SOLANA_RPC");
        // A trailing flag with no value is left alone, not swallowed.
        assert_eq!(split_rpc_args(&["a", "--rpc-url"]).0, ["a", "--rpc-url"]);
    }

    #[test]
    fn solana_config_is_private_and_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let c = SolanaConfig::create("https://x.io/?api-key=K", "/home/u/id.json").unwrap();
        let p = std::path::PathBuf::from(c.path());
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("json_rpc_url: \"https://x.io/?api-key=K\""), "{text}");
        assert!(text.contains("websocket_url: \"\""), "{text}");
        drop(c);
        assert!(!p.exists(), "config file must be removed");
    }

    #[test]
    fn rpc_url_check_catches_a_fallback() {
        let ok = "Config File: /tmp/c.yml\nRPC URL: https://d.io/?api-key=K \nKeypair Path: /k\n";
        assert!(rpc_url_matches(ok, "https://d.io/?api-key=K"));
        let fell_back = "RPC URL: https://api.mainnet-beta.solana.com \n";
        assert!(!rpc_url_matches(fell_back, "https://d.io/?api-key=K"));
        assert!(!rpc_url_matches("", "https://d.io/"));
    }

    #[test]
    fn child_environment_drops_secrets() {
        std::env::set_var("PRIVATE_KEY", "should-not-pass");
        std::env::set_var("NETWORK_PRIVATE_KEY", "should-not-pass");
        std::env::set_var("FOUNDRY_PROFILE", "ok");
        let e = base_env();
        assert!(e.iter().all(|(k, _)| k != "PRIVATE_KEY" && k != "NETWORK_PRIVATE_KEY"));
        assert!(e.iter().any(|(k, _)| k == "FOUNDRY_PROFILE"));
    }

    #[test]
    fn prover_choice_is_never_forced() {
        assert_eq!(sp1_prover(false, Some("network")).unwrap(), "mock");
        assert_eq!(sp1_prover(true, None).unwrap(), "cpu");
        assert_eq!(sp1_prover(true, Some("network")).unwrap(), "network");
        assert_eq!(sp1_prover(true, Some("cuda")).unwrap(), "cuda");
        assert!(sp1_prover(true, Some("mock")).is_err());
        assert!(sp1_prover(true, Some("gpu?")).is_err());
    }

    fn mainnet() -> EpochSchedule {
        EpochSchedule { slots_per_epoch: 432_000, warmup: false, first_normal_epoch: 0, first_normal_slot: 0 }
    }

    #[test]
    fn epoch_math_after_warmup() {
        let s = mainnet();
        assert_eq!(s.epoch_of(0), 0);
        assert_eq!(s.epoch_of(431_999), 0);
        assert_eq!(s.epoch_of(432_000), 1);
        assert_eq!(s.epoch_of(521_088_299), 1206); // the devnet slot in the fixtures
        assert_eq!(s.check_contract_agrees(521_088_299).unwrap(), 1206);
    }

    #[test]
    fn epoch_math_with_warmup_matches_solana() {
        // Solana: slots_per_epoch 8192, warmup -> epochs of 32, 64, ..., first normal epoch 8.
        let s = EpochSchedule { slots_per_epoch: 8192, warmup: true, first_normal_epoch: 8, first_normal_slot: 8160 };
        assert_eq!(s.epoch_of(0), 0);
        assert_eq!(s.epoch_of(31), 0);
        assert_eq!(s.epoch_of(32), 1);
        assert_eq!(s.epoch_of(95), 1);
        assert_eq!(s.epoch_of(96), 2);
        assert_eq!(s.epoch_of(8159), 7);
        assert_eq!(s.epoch_of(8160), 8);
        assert_eq!(s.epoch_of(8160 + 8192), 9);
        // The contract follows warmup too (deployed with WARMUP=true).
        assert_eq!(s.check_contract_agrees(100).unwrap(), 2); // 96..=223 is epoch 2
    }

    #[test]
    fn schedule_parses_rpc_shape() {
        let v = json!({"slotsPerEpoch": 432000, "leaderScheduleSlotOffset": 432000, "warmup": false, "firstNormalEpoch": 0, "firstNormalSlot": 0});
        assert_eq!(EpochSchedule::from_rpc(&v).unwrap(), mainnet());
        assert!(EpochSchedule::from_rpc(&json!({"slotsPerEpoch": 0, "firstNormalEpoch": 0, "firstNormalSlot": 0})).is_err());
        assert!(EpochSchedule::from_rpc(&json!({})).is_err());
    }

    #[test]
    fn a_set_counts_only_with_a_matching_passing_check() {
        let dir = std::env::temp_dir().join(format!("huginn-set-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = |n: &str, v: Value| std::fs::write(dir.join(n), v.to_string()).unwrap();
        w("validator_set_5.json", json!({"commitment": "aa", "validator_set": {"epoch": 5}}));
        assert!(!set_is_verified(&dir, 5), "no report yet");
        w("check_set_5.json", json!({"epoch": 5, "pass": true, "commitment": "aa"}));
        assert!(set_is_verified(&dir, 5));
        w("check_set_5.json", json!({"epoch": 5, "pass": false, "commitment": "aa"}));
        assert!(!set_is_verified(&dir, 5), "a failed check");
        w("check_set_5.json", json!({"epoch": 5, "pass": true, "commitment": "bb"}));
        assert!(!set_is_verified(&dir, 5), "report is for a different set");
        w("validator_set_5.json", json!({"commitment": "aa", "validator_set": {"epoch": 4}}));
        w("check_set_5.json", json!({"epoch": 5, "pass": true, "commitment": "aa"}));
        assert!(!set_is_verified(&dir, 5), "file belongs to another epoch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_delay_must_fit_the_epoch() {
        let ms = ms_per_slot(&json!([{"numSlots": 280, "samplePeriodSecs": 60}, {"numSlots": 284, "samplePeriodSecs": 60}])).unwrap();
        assert!((ms - 212.77).abs() < 0.1, "{ms}");
        // 432,000 slots at ~213 ms ≈ 25.5 h: two hours is fine, one day is not.
        let h = check_set_delay(DEFAULT_SET_DELAY_SECS, 432_000, ms).unwrap();
        assert!((h - 25.5).abs() < 0.2, "{h}");
        assert!(check_set_delay(86_400, 432_000, ms).is_err());
        assert!(check_set_delay(0, 432_000, ms).is_ok());
        assert!(ms_per_slot(&json!([])).is_err());
    }
}
