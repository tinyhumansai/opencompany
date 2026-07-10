use serde::Serialize;

/// Runtime integration status for an inherited module.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeModuleStatus {
    /// Module name.
    pub name: &'static str,
    /// Whether the module is compiled into this build.
    pub enabled: bool,
    /// Intended role in OpenCompany.
    pub role: &'static str,
    /// Local source location.
    pub path: &'static str,
}

impl RuntimeModuleStatus {
    /// Returns the status of all inherited runtime modules.
    pub fn all() -> Vec<Self> {
        vec![
            Self {
                name: "tinyagents",
                enabled: cfg!(feature = "tinyagents"),
                role: "agent harness, graphs, registry, and RLM runtime",
                path: "vendor/tinyagents",
            },
            Self {
                name: "openhuman",
                enabled: true,
                role: "OpenHuman checkout launched through Cargo",
                path: "vendor/openhuman",
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_known_modules_are_reported() {
        let names: Vec<_> = RuntimeModuleStatus::all()
            .into_iter()
            .map(|module| module.name)
            .collect();

        assert_eq!(names, vec!["tinyagents", "openhuman"]);
    }
}
