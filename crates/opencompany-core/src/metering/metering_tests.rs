use super::*;

#[test]
fn roster_uses_role_then_overlay_name() {
    let agents = vec![
        Agent {
            provider: None,
            global: false,
            id: "strategy".into(),
            role: "Strategy desk".into(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            skills: None,
            delegates_to: vec![],
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
        },
        Agent {
            provider: None,
            global: false,
            id: "creative".into(),
            role: "Creative studio".into(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            skills: None,
            delegates_to: vec![],
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
        },
    ];
    let overlay = vec![OverlayAgent {
        provider: None,
        id: "creative".into(),
        name: "Creative studio (renamed)".into(),
        role: "Creative".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    }];
    let map = roster_display_names(&agents, &overlay);
    assert_eq!(map.get("strategy").unwrap(), "Strategy desk");
    // Overlay name overrides the manifest role for the same id.
    assert_eq!(map.get("creative").unwrap(), "Creative studio (renamed)");
}
