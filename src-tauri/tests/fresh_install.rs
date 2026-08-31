//! What a brand-new install does, over HTTP, through the launch path itself.
//!
//! The unit tests in `local.rs` assert the same decision, but they assert it
//! against `LocalHosts` as a value. This file is deliberately one level out: it
//! makes a data directory that has never been used, boots it exactly the way
//! `lib::run` boots the application — `LocalHosts::load(data_dir)`, nothing
//! else — and then only ever asks the resulting host questions over its own
//! socket, the way the console does.
//!
//! That distinction is the whole point of the file. The thing being verified is
//! not "the flag is set", it is "a person who double-clicks a fresh install
//! reaches onboarding, and a person who already has a company does not". Both
//! of those are answers the console reads out of `/spec` at boot
//! (`ConnectionConsole` enters its setup phase on `setup_complete === false`
//! and on nothing else), so `/spec` is what this asks.
//!
//! It also walks the rest of the way: completing setup with the same starter
//! template the host used to seed silently, then relaunching over the root that
//! leaves behind. A change that made the wizard open but left an operator
//! unable to finish it, or made them finish it once per launch, would pass a
//! narrower test than this one.

use std::path::Path;

use opencompany_desktop_lib::local::LocalHosts;

/// The template the desktop used to seed without asking. It has to still be
/// offered, or "the wizard opens instead" would be a downgrade rather than a
/// choice.
const STARTER_TEMPLATE: &str = "agentic_marketing_agency";

#[tokio::test]
async fn a_fresh_install_opens_the_wizard_and_can_complete_it() {
    let data_dir = tempfile::tempdir().expect("tempdir");

    // Launch, as `lib::run` performs it. A fresh install differs from every
    // other launch in one way only: this directory is empty.
    let hosts = LocalHosts::load(data_dir.path().to_path_buf()).await;
    let default = hosts
        .default_instance()
        .expect("a fresh data dir still starts its default instance");
    let base = default
        .base_url
        .clone()
        .expect("a started instance reports its address");

    assert!(
        default.companies.is_empty(),
        "nobody has chosen a company yet, so the host must not have one: {:?}",
        default.companies
    );
    assert_eq!(
        spec(&base).await["setup_complete"],
        serde_json::json!(false),
        "this is the field the console consults, and `false` is what opens the wizard"
    );

    // The wizard's own read, made with no session at all. A first run has no
    // account to present, so a setup surface that needed one could never be
    // reached on the install that needs it most.
    let offer = reqwest::get(format!("{base}/api/v1/setup"))
        .await
        .expect("the setup route answers");
    assert_eq!(
        offer.status(),
        200,
        "an unconfigured host must serve its wizard to an anonymous caller"
    );
    let offer: serde_json::Value = offer.json().await.expect("a setup document");
    let templates = offer["templates"]
        .as_array()
        .expect("the wizard offers a template catalog");
    assert!(
        templates.iter().any(|t| t["id"] == STARTER_TEMPLATE),
        "the company the host used to seed silently must still be one click away"
    );

    // Completing it, choosing that same starter company. This is the path the
    // seed used to take on the operator's behalf, now taken by the operator.
    let applied = reqwest::Client::new()
        .post(format!("{base}/api/v1/setup"))
        .json(&serde_json::json!({ "fields": {}, "template": STARTER_TEMPLATE }))
        .send()
        .await
        .expect("the setup route accepts a completed wizard");
    assert_eq!(
        applied.status(),
        200,
        "a wizard that opens and cannot be finished is worse than one that never opened"
    );

    let listed = companies(&base).await;
    assert_eq!(
        listed.len(),
        1,
        "completing setup seeds exactly the company that was chosen: {listed:?}"
    );
    // The company that was chosen, not merely *a* company: an apply that
    // ignored the template, or seeded a different one, would satisfy the count
    // above and nothing else here. Provenance rather than the id, because the
    // id follows the company's name and the name is the operator's to change.
    assert_eq!(
        provenance(&base, &listed[0]).await.as_deref(),
        Some(STARTER_TEMPLATE),
        "the seeded company must be the template the wizard was told to use"
    );
    assert_eq!(
        spec(&base).await["setup_complete"],
        serde_json::json!(true),
        "and the wizard must not open again over the same root"
    );

    // The second launch, which is the one that used to go wrong quietly in the
    // other direction: an install with a company must never be walked through
    // setup again. Dropped first so the root's lock is released.
    let chosen = listed;
    drop(hosts);
    let relaunched = relaunch(data_dir.path()).await;
    let default = relaunched
        .default_instance()
        .expect("the same root comes back");
    let base = default.base_url.clone().expect("running");

    assert_eq!(
        default.companies.len(),
        1,
        "the company completing setup left behind is adopted, not re-created"
    );
    assert_eq!(
        companies(&base).await,
        chosen,
        "and it is the same company, not a second one"
    );
    assert_eq!(
        spec(&base).await["setup_complete"],
        serde_json::json!(true),
        "a used install goes straight to its console"
    );
}

/// The handshake the console makes before anything else.
async fn spec(base: &str) -> serde_json::Value {
    reqwest::get(format!("{base}/spec"))
        .await
        .expect("the reported address answers")
        .json()
        .await
        .expect("a spec document")
}

/// The company ids this host serves, as the console lists them.
async fn companies(base: &str) -> Vec<String> {
    let listed: serde_json::Value = reqwest::get(format!("{base}/api/v1/companies"))
        .await
        .expect("the companies route answers")
        .json()
        .await
        .expect("a company list");
    listed
        .as_array()
        .expect("an array")
        .iter()
        .map(|company| {
            company["id"]
                .as_str()
                .expect("every company has an id")
                .to_string()
        })
        .collect()
}

/// Which template a company was seeded from, as the host reports it.
async fn provenance(base: &str, id: &str) -> Option<String> {
    let listed: serde_json::Value = reqwest::get(format!("{base}/api/v1/companies"))
        .await
        .expect("the companies route answers")
        .json()
        .await
        .expect("a company list");
    listed
        .as_array()
        .expect("an array")
        .iter()
        .find(|company| company["id"] == id)
        .and_then(|company| company["template_provenance"]["source_id"].as_str())
        .map(str::to_string)
}

/// Loads the roster again, retrying while the previous run's `flock` clears.
///
/// The same asynchronous release `local.rs`'s own relaunch helpers wait out: the
/// lock belongs to the open file description, and this binary spawns
/// subprocesses that inherit a copy.
async fn relaunch(data_dir: &Path) -> LocalHosts {
    for _ in 0..200 {
        let hosts = LocalHosts::load(data_dir.to_path_buf()).await;
        if hosts
            .default_instance()
            .is_some_and(|instance| instance.running)
        {
            return hosts;
        }
        drop(hosts);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("a released root must become takeable");
}
