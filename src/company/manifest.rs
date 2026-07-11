//! Manifest loading, discovery, and validation.
//!
//! [`CompanyManifest::from_path`] parses a manifest file and validates it,
//! returning every problem at once in prosumer language. [`discover`] locates
//! the manifest inside a company directory, preferring `company.toml` over the
//! legacy `agents.toml`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{OpenCompanyError, Result};

use super::types::{
    BRAIN_MODES, CompanyManifest, KNOWN_CHANNELS, POLICY_MODES, TIERS, TOOL_PROVIDERS,
};

/// Preferred manifest filename.
pub const MANIFEST_FILE: &str = "company.toml";

/// Legacy manifest filename, accepted unchanged with a deprecation note.
pub const LEGACY_MANIFEST_FILE: &str = "agents.toml";

/// A located manifest file and whether it uses the legacy filename.
#[derive(Clone, Debug)]
pub struct Located {
    /// Path to the manifest file.
    pub path: PathBuf,
    /// True when the file is the legacy `agents.toml`.
    pub legacy: bool,
}

/// Locates the manifest inside a directory (or accepts a direct file path),
/// preferring `company.toml` over `agents.toml`.
pub fn discover(input: &Path) -> Result<Located> {
    if input.is_file() {
        let legacy = input.file_name().and_then(|n| n.to_str()) == Some(LEGACY_MANIFEST_FILE);
        return Ok(Located {
            path: input.to_path_buf(),
            legacy,
        });
    }

    let preferred = input.join(MANIFEST_FILE);
    if preferred.is_file() {
        return Ok(Located {
            path: preferred,
            legacy: false,
        });
    }

    let legacy = input.join(LEGACY_MANIFEST_FILE);
    if legacy.is_file() {
        return Ok(Located {
            path: legacy,
            legacy: true,
        });
    }

    Err(OpenCompanyError::MissingManifest(input.to_path_buf()))
}

impl CompanyManifest {
    /// Reads, parses, and validates a manifest from `path`.
    ///
    /// `path` may be a manifest file or a directory containing one. Validation
    /// collects every problem and reports them together.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let located = discover(path.as_ref())?;
        Self::from_file(&located.path)
    }

    /// Reads, parses, and validates a specific manifest file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).map_err(|source| OpenCompanyError::ManifestRead {
                path: path.to_path_buf(),
                source,
            })?;

        let manifest: CompanyManifest = toml::from_str(&text).map_err(|err| {
            OpenCompanyError::ManifestParse(path.to_path_buf(), err.message().to_string())
        })?;

        let problems = manifest.validate();
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(OpenCompanyError::ManifestInvalid {
                path: path.to_path_buf(),
                problems,
            })
        }
    }

    /// Returns every validation problem in prosumer language. An empty vector
    /// means the manifest is valid.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.company.name.trim().is_empty() {
            problems.push("`[company].name` cannot be empty — give your company a name.".into());
        }

        // Roster: ids must be snake_case and unique; tiers and budgets sane.
        let mut seen = std::collections::HashSet::new();
        for (index, agent) in self.agents.iter().enumerate() {
            let label = if agent.id.is_empty() {
                format!("agent #{}", index + 1)
            } else {
                format!("agent `{}`", agent.id)
            };

            if agent.id.trim().is_empty() {
                problems.push(format!("{label} is missing an `id`."));
            } else if !is_snake_case(&agent.id) {
                problems.push(format!(
                    "{label} has an invalid `id` — use snake_case (lowercase letters, digits, and underscores, starting with a letter)."
                ));
            } else if !seen.insert(agent.id.as_str()) {
                problems.push(format!(
                    "agent `id` `{}` is used more than once — ids must be unique.",
                    agent.id
                ));
            }

            if agent.role.trim().is_empty() {
                problems.push(format!("{label} is missing a `role`."));
            }

            if let Some(tier) = &agent.tier
                && !TIERS.contains(&tier.as_str())
            {
                problems.push(one_of(&format!("{label} `tier`"), TIERS, tier));
            }

            if let Some(budget) = agent.budget_usd_daily
                && budget < 0.0
            {
                problems.push(format!(
                    "{label} `budget_usd_daily` cannot be negative — you wrote `{budget}`."
                ));
            }
        }

        if !BRAIN_MODES.contains(&self.brain.mode.as_str()) {
            problems.push(one_of("`[brain].mode`", BRAIN_MODES, &self.brain.mode));
        }

        if !TOOL_PROVIDERS.contains(&self.tools.provider.as_str()) {
            problems.push(one_of(
                "`[tools].provider`",
                TOOL_PROVIDERS,
                &self.tools.provider,
            ));
        }

        if !POLICY_MODES.contains(&self.policy.mode.as_str()) {
            problems.push(one_of("`[policy].mode`", POLICY_MODES, &self.policy.mode));
        }

        if let Some(under) = self.policy.auto_approve_under_usd
            && under < 0.0
        {
            problems.push(format!(
                "`[policy].auto_approve_under_usd` cannot be negative — you wrote `{under}`."
            ));
        }

        for name in self.channels.keys() {
            if !KNOWN_CHANNELS.contains(&name.as_str()) {
                problems.push(format!(
                    "`[channels.{name}]` is not a channel OpenCompany knows — expected one of {}.",
                    join_backticked(KNOWN_CHANNELS)
                ));
            }
        }

        if self.place.discoverable && self.company.handle.is_none() {
            problems.push(
                "`[place].discoverable` is true but `[company].handle` is not set — a public company needs a @handle.".into(),
            );
        }

        for skill in &self.place.skills {
            if parse_usd(&skill.price_usd).is_none() {
                problems.push(format!(
                    "skill `{}` has an invalid `price_usd` `{}` — use a decimal string like \"25.00\".",
                    skill.id, skill.price_usd
                ));
            }
        }

        if let Some(monthly) = self.budget.monthly_usd
            && monthly < 0.0
        {
            problems.push(format!(
                "`[budget].monthly_usd` cannot be negative — you wrote `{monthly}`."
            ));
        }

        for (index, schedule) in self.schedules.iter().enumerate() {
            let fields = schedule.cron.split_whitespace().count();
            if fields != 5 {
                problems.push(format!(
                    "schedule #{} has an invalid `cron` `{}` — a schedule needs 5 fields (minute hour day month weekday).",
                    index + 1,
                    schedule.cron
                ));
            }
        }

        problems
    }

    /// Renders a human-readable summary of the effective configuration, used by
    /// `opencompany check` and the example boot banner.
    pub fn effective_summary(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Company:  {}", self.company.name);
        if let Some(output) = &self.company.output {
            let _ = writeln!(out, "Output:   {output}");
        }
        if let Some(role) = &self.company.human_role {
            let _ = writeln!(out, "You own:  {role}");
        }
        let _ = writeln!(out, "Brain:    {}", self.brain.mode);
        let _ = writeln!(out, "Policy:   {}", self.policy.mode);
        let _ = writeln!(out, "Tools:    {}", self.tools.provider);
        if let Some(monthly) = self.budget.monthly_usd {
            let _ = writeln!(out, "Budget:   ${monthly:.2}/month");
        }
        let _ = writeln!(
            out,
            "Discover: {}",
            if self.place.discoverable {
                "public"
            } else {
                "private"
            }
        );

        let _ = writeln!(out, "\nRoster ({}):", self.agents.len());
        for agent in &self.agents {
            let tier = agent.tier.as_deref().unwrap_or("—");
            let _ = writeln!(out, "  • {:<20} {}  [tier: {}]", agent.id, agent.role, tier);
        }

        if !self.channels.is_empty() {
            let names: Vec<&str> = self.channels.keys().map(String::as_str).collect();
            let _ = writeln!(out, "\nChannels: {}", names.join(", "));
        }
        if !self.schedules.is_empty() {
            let _ = writeln!(out, "\nSchedules ({}):", self.schedules.len());
            for schedule in &self.schedules {
                let _ = writeln!(out, "  • {}  →  {}", schedule.cron, schedule.prompt);
            }
        }

        out
    }
}

/// True when `id` is non-empty, starts with a lowercase letter, and contains
/// only lowercase letters, digits, and underscores.
fn is_snake_case(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parses a decimal USD string, rejecting anything non-numeric or negative.
fn parse_usd(value: &str) -> Option<f64> {
    match value.trim().parse::<f64>() {
        Ok(amount) if amount >= 0.0 && amount.is_finite() => Some(amount),
        _ => None,
    }
}

/// Builds a "must be one of … — you wrote `x`" message.
fn one_of(field: &str, allowed: &[&str], actual: &str) -> String {
    format!(
        "{field} must be one of {} — you wrote `{actual}`.",
        allowed.join(", ")
    )
}

fn join_backticked(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> CompanyManifest {
        toml::from_str(text).expect("valid toml")
    }

    #[test]
    fn bare_agents_toml_is_valid() {
        let manifest = parse(
            r#"
            [company]
            name = "Agentic Marketing Agency"
            output = "Campaigns across every channel"
            human_role = "Campaign review and sign-off"

            [[agent]]
            id = "copywriter"
            role = "Copywriter"
            description = "Write ads."
            "#,
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn defaults_are_prosumer_safe() {
        let manifest = parse("[company]\nname = \"Solo\"\n");
        assert_eq!(manifest.brain.mode, "hosted");
        assert_eq!(manifest.tools.provider, "openhuman");
        assert_eq!(manifest.policy.mode, "supervised");
        assert!(!manifest.place.discoverable);
        assert_eq!(
            manifest.policy.always_approve,
            vec!["payment.send", "filing.submit", "external.publish"]
        );
    }

    #[test]
    fn rejects_bad_policy_mode_in_prosumer_language() {
        let manifest = parse("[company]\nname = \"X\"\n[policy]\nmode = \"supervized\"\n");
        let problems = manifest.validate();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("`[policy].mode`"));
        assert!(problems[0].contains("readonly, supervised, full"));
        assert!(problems[0].contains("supervized"));
    }

    #[test]
    fn rejects_non_snake_case_and_duplicate_ids() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            [[agent]]
            id = "BadId"
            role = "A"
            [[agent]]
            id = "dup"
            role = "B"
            [[agent]]
            id = "dup"
            role = "C"
            "#,
        );
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("snake_case")));
        assert!(problems.iter().any(|p| p.contains("more than once")));
    }

    #[test]
    fn rejects_unknown_channel_and_bad_tier() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            [[agent]]
            id = "a"
            role = "A"
            tier = "genius"
            [channels.telepathy]
            enabled = true
            "#,
        );
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("telepathy")));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`tier`") && p.contains("genius"))
        );
    }

    #[test]
    fn public_company_requires_handle() {
        let manifest = parse("[company]\nname = \"X\"\n[place]\ndiscoverable = true\n");
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("@handle")));
    }

    #[test]
    fn rejects_bad_skill_price_and_cron() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            handle = "x"
            [place]
            discoverable = true
            skills = [{ id = "seo.audit", price_usd = "free" }]
            [[schedule]]
            cron = "every monday"
            prompt = "review"
            "#,
        );
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("price_usd")));
        assert!(problems.iter().any(|p| p.contains("5 fields")));
    }

    #[test]
    fn effective_summary_lists_roster() {
        let manifest = parse(
            r#"
            [company]
            name = "Agentic Marketing Agency"
            [[agent]]
            id = "copywriter"
            role = "Copywriter"
            "#,
        );
        let summary = manifest.effective_summary();
        assert!(summary.contains("Agentic Marketing Agency"));
        assert!(summary.contains("copywriter"));
        assert!(summary.contains("Roster (1)"));
    }

    #[test]
    fn discover_prefers_company_toml() {
        let dir = std::env::temp_dir().join(format!("oc-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LEGACY_MANIFEST_FILE), "[company]\nname=\"L\"\n").unwrap();
        let located = discover(&dir).unwrap();
        assert!(located.legacy);
        std::fs::write(dir.join(MANIFEST_FILE), "[company]\nname=\"C\"\n").unwrap();
        let located = discover(&dir).unwrap();
        assert!(!located.legacy);
        std::fs::remove_dir_all(&dir).ok();
    }
}
