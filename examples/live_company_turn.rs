//! Live smoke: build one company agent on the embedded OpenHuman runtime and
//! run a single turn against the hosted TinyHumans inference endpoint.
//!
//! This is the end-to-end proof for issue #9 — a company agent whose cognition
//! runs on the real hosted brain, with the turn's token usage metered back.
//! It needs a live credential, so it is an example (run by hand), not a test.
//!
//! ```bash
//! TINYHUMANS_API_KEY=<jwt> \
//! OPENCOMPANY_INFERENCE_URL=https://staging-api.tinyhumans.ai/openai/v1 \
//! cargo run --example live_company_turn --features openhuman -- "Who are you?"
//! ```
//!
//! With no key set it prints how to supply one and exits non-zero, so it is
//! safe to invoke in any environment.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use opencompany::app::config::ProcessEnv;
use opencompany::company::CompanyManifest;
use opencompany::harness::provider::{HostedProvider, harness_inference_from_env};
use opencompany::harness::{HarnessDeps, HarnessPool};
use opencompany::ports::types::{CompanyId, CompanyRecord};
use opencompany::ports::usage::{UsageMeter, UsageSample};
use opencompany::store::{FsCompanyStore, FsContextStore};

/// A [`UsageMeter`] that keeps every sample in memory so the run can print the
/// turn's metered token/cost totals.
#[derive(Default)]
struct CapturingMeter {
    samples: Mutex<Vec<UsageSample>>,
}

#[async_trait]
impl UsageMeter for CapturingMeter {
    async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> opencompany::Result<()> {
        self.samples.lock().unwrap().push(sample.clone());
        Ok(())
    }
    async fn query(
        &self,
        _company: &CompanyId,
        _since_millis: u64,
    ) -> opencompany::Result<Vec<UsageSample>> {
        Ok(self.samples.lock().unwrap().clone())
    }
}

/// A one-agent company: the CEO of a tiny robotics firm.
const MANIFEST: &str = r#"
[company]
name = "Tiny Robotics"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Tiny Robotics. Speaks in the first person, crisp and factual."
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Introduce yourself in one sentence.".to_string());

    let (cfg, default_model) = harness_inference_from_env(&ProcessEnv).ok_or_else(|| {
        anyhow::anyhow!(
            "no inference credential — set TINYHUMANS_API_KEY (or OPENCOMPANY_INFERENCE_KEY), \
             optionally OPENCOMPANY_INFERENCE_URL / _MODEL"
        )
    })?;
    eprintln!(
        "[live] endpoint={}  default_model={}",
        cfg.base_url, default_model
    );

    let manifest: CompanyManifest = toml::from_str(MANIFEST)?;
    let record = CompanyRecord {
        id: CompanyId::new("demo"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
    };

    let dir = tempfile::tempdir()?;
    let meter = Arc::new(CapturingMeter::default());
    let deps = HarnessDeps {
        provider: Arc::new(HostedProvider::new(cfg)),
        provider_slug: "managed".to_string(),
        context: Arc::new(FsContextStore::new(dir.path())),
        store: Arc::new(FsCompanyStore::new(dir.path())),
        meter: Some(meter.clone()),
        workspace_root: dir.path().to_path_buf(),
        model_override: Some(default_model.clone()),
    };

    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await?;

    println!("── prompt → ceo ──\n{prompt}\n");
    let reply = pool.run(&record.id, "ceo", &prompt, &deps).await?;
    println!("── ceo reply ──\n{reply}\n");

    match meter.samples.lock().unwrap().last() {
        Some(s) => println!(
            "── metered usage ──\nprovider={}  in={}  out={}  cached={}  cost_usd={}",
            s.provider, s.input_tokens, s.output_tokens, s.cached_input_tokens, s.cost_usd
        ),
        None => println!("── metered usage ──\n(provider reported no usage)"),
    }

    Ok(())
}
