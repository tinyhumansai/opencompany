//! The hosts this machine runs, as a set rather than as a singleton.
//!
//! [`crate::embedded`] knows how to start *one* host over *one* data root, and
//! deliberately says nothing about which roots exist. This module is the layer
//! above: the roster of local instances an operator has asked for, where each
//! one keeps its data, and which of them are listening right now.
//!
//! ## Why a roster on disk
//!
//! An instance is only interesting because it survives a quit. The port does
//! not (it is ephemeral by design — see `embedded.rs`), and neither does the
//! process, so the durable thing about "the Acme instance" is its data root and
//! the name someone gave it. That has to be written down somewhere the next
//! launch reads, and `<data-dir>/instances.json` is it.
//!
//! ## Why the first instance is the data root itself
//!
//! Every install that predates this module keeps its company under
//! `<data-dir>` directly. Moving it under `<data-dir>/instances/default/`
//! would be a migration whose failure mode is "my company is gone", to buy
//! nothing but symmetry. So the default instance's root *is* the data dir, and
//! only instances created after it get a subdirectory.
//!
//! ## Failure is a state, not an error
//!
//! A root can be held by another process — a second window, or an
//! `opencompany serve` in a terminal. That instance simply does not start, and
//! the rest do; the console renders the reason on its row. Refusing to launch
//! because one of N roots is busy would make the multi-instance case *worse*
//! than the single one it replaces.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::embedded::{self, EmbeddedHost, FirstRun};

/// The id of the instance rooted at the data dir itself.
pub const DEFAULT_INSTANCE_ID: &str = "default";

/// What the console calls that instance before anyone renames it.
pub const DEFAULT_INSTANCE_LABEL: &str = "This computer";

/// The roster file, under the data dir.
const ROSTER_FILE: &str = "instances.json";

/// Where new instances put their data, under the data dir.
const INSTANCES_DIR: &str = "instances";

/// One instance's durable record. No port, no pid: neither survives a quit.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RosterEntry {
    id: String,
    label: String,
    /// The data root, relative to the data dir. `None` means the data dir
    /// itself, which is what the default instance has always used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    /// Whether to start this instance on launch. An operator who stopped an
    /// instance means it to stay stopped across a relaunch — otherwise the
    /// stop button is undone by every quit.
    #[serde(default = "yes")]
    autostart: bool,
    /// Set only when this application provisions the root. A path shaped like
    /// one of ours is not proof that the application owns its contents.
    #[serde(default, skip_serializing_if = "is_false")]
    desktop_created: bool,
}

fn yes() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Roster {
    #[serde(default)]
    instances: Vec<RosterEntry>,
}

/// One instance as the console sees it: what it is, and where it got to.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalInstanceInfo {
    pub id: String,
    pub label: String,
    pub data_dir: String,
    pub running: bool,
    /// Present exactly when `running`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The host's own durable identity, which is what the console keys its
    /// connection row on — the address changes every launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub companies: Vec<String>,
    /// Why it is not running, in the operator's words. Most often the root
    /// being held by another process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Instance {
    entry: RosterEntry,
    host: Option<EmbeddedHost>,
    error: Option<String>,
}

impl Instance {
    fn info(&self, data_dir: &Path) -> LocalInstanceInfo {
        LocalInstanceInfo {
            id: self.entry.id.clone(),
            label: self.entry.label.clone(),
            data_dir: root_of(data_dir, &self.entry).display().to_string(),
            running: self.host.is_some(),
            base_url: self.host.as_ref().map(EmbeddedHost::base_url),
            // Read off disk when nothing is running, because the console
            // prunes its remembered profiles against this. A stopped instance
            // that reported no identity would be indistinguishable from a data
            // root this application no longer serves, and the console would
            // forget its connection id — which is what every browser-local key
            // for that instance is scoped by.
            instance_id: match self.host.as_ref() {
                Some(host) => Some(host.instance_id().to_string()),
                None => opencompany::app::instance::peek(&root_of(data_dir, &self.entry)),
            },
            companies: self
                .host
                .as_ref()
                .map(|host| host.companies().to_vec())
                .unwrap_or_default(),
            error: self.error.clone(),
        }
    }
}

/// Resolves an entry's data root against the application's data dir.
fn root_of(data_dir: &Path, entry: &RosterEntry) -> PathBuf {
    match entry.root.as_deref() {
        None => data_dir.to_path_buf(),
        Some(relative) => data_dir.join(relative),
    }
}

/// Every local instance, and the ones currently listening.
pub struct LocalHosts {
    data_dir: PathBuf,
    instances: Vec<Instance>,
}

impl LocalHosts {
    /// Reads the roster and starts everything marked for autostart.
    ///
    /// Never fails as a whole: an unreadable roster falls back to the single
    /// default instance, and an instance that cannot start becomes a row
    /// carrying its reason. A desktop that refused to open because one data
    /// root was busy is the bug this shape exists to avoid.
    pub async fn load(data_dir: PathBuf) -> Self {
        let roster = read_roster(&data_dir);
        let mut hosts = Self {
            data_dir,
            instances: roster
                .instances
                .into_iter()
                .map(|entry| Instance {
                    entry,
                    host: None,
                    error: None,
                })
                .collect(),
        };
        for index in 0..hosts.instances.len() {
            if hosts.instances[index].entry.autostart {
                hosts.start_at(index).await;
            }
        }
        hosts
    }

    /// The roster, in listing order.
    pub fn list(&self) -> Vec<LocalInstanceInfo> {
        self.instances
            .iter()
            .map(|instance| instance.info(&self.data_dir))
            .collect()
    }

    /// The instance rooted at the data dir itself, when it is running.
    ///
    /// What `oc_embedded` answers with, so a console (or a shell) that predates
    /// the roster keeps seeing exactly what it saw before.
    pub fn default_instance(&self) -> Option<LocalInstanceInfo> {
        self.instances
            .iter()
            .find(|instance| instance.entry.id == DEFAULT_INSTANCE_ID)
            .filter(|instance| instance.host.is_some())
            .map(|instance| instance.info(&self.data_dir))
    }

    /// Adds an instance over a fresh data root and starts it.
    ///
    /// The root is derived from the id, never from the label: a label is free
    /// text an operator retypes, and a renamed instance must not become a
    /// second empty one.
    pub async fn create(&mut self, label: &str) -> Result<LocalInstanceInfo, String> {
        let label = label.trim();
        if label.is_empty() {
            return Err("an instance needs a name".to_string());
        }
        let id = self.mint_id(label);
        let entry = RosterEntry {
            root: Some(format!("{INSTANCES_DIR}/{id}")),
            id,
            label: label.to_string(),
            autostart: true,
            desktop_created: true,
        };
        self.instances.push(Instance {
            entry,
            host: None,
            error: None,
        });
        let index = self.instances.len() - 1;
        self.start_at(index).await;
        self.persist();
        let instance = &self.instances[index];
        match &instance.error {
            // A create that could not start is reported as an error rather than
            // as a stopped row: the operator asked for a running instance and
            // nothing on screen would otherwise say why they did not get one.
            Some(error) => Err(error.clone()),
            None => Ok(instance.info(&self.data_dir)),
        }
    }

    /// Starts a stopped instance, or reports why it will not start.
    pub async fn start(&mut self, id: &str) -> Result<LocalInstanceInfo, String> {
        let index = self.index_of(id)?;
        if self.instances[index].host.is_none() {
            self.start_at(index).await;
        }
        // Autostart follows the operator's last explicit choice, so a started
        // instance comes back on the next launch.
        self.instances[index].entry.autostart = true;
        self.persist();
        let instance = &self.instances[index];
        match &instance.error {
            Some(error) => Err(error.clone()),
            None => Ok(instance.info(&self.data_dir)),
        }
    }

    /// Stops an instance, freeing its port and its data root.
    ///
    /// The row stays: an instance is its data, and stopping is not forgetting.
    pub fn stop(&mut self, id: &str) -> Result<LocalInstanceInfo, String> {
        let index = self.index_of(id)?;
        // Dropping the host aborts its server task and releases the root lock.
        self.instances[index].host = None;
        self.instances[index].error = None;
        self.instances[index].entry.autostart = false;
        self.persist();
        Ok(self.instances[index].info(&self.data_dir))
    }

    /// Removes an instance from the roster, leaving its data on disk.
    ///
    /// Deliberately not a delete. The roster is a list of things to run; the
    /// data root is someone's company. Removing the first is reversible by
    /// re-adding the root, and destroying the second is not reversible at all,
    /// so this does only the reversible half.
    ///
    /// The default instance cannot be removed: its root is the data dir, so a
    /// roster without it is a roster this application cannot rebuild.
    pub fn forget(&mut self, id: &str) -> Result<(), String> {
        if id == DEFAULT_INSTANCE_ID {
            return Err("the instance on this computer cannot be removed".to_string());
        }
        let index = self.index_of(id)?;
        self.instances.remove(index);
        self.persist();
        Ok(())
    }

    /// Permanently removes a desktop-created instance and its data root.
    ///
    /// The default instance is deliberately excluded: its root is the
    /// application's data directory, which also owns the roster and every
    /// desktop-created instance below it. Only roots minted under
    /// `instances/` are eligible for recursive deletion.
    pub async fn delete(&mut self, id: &str) -> Result<(), String> {
        if id == DEFAULT_INSTANCE_ID {
            return Err("the instance on this computer cannot be deleted".to_string());
        }
        let index = self.index_of(id)?;
        let Some(relative_root) = self.instances[index].entry.root.as_deref() else {
            return Err("only desktop-created instances can be deleted".to_string());
        };
        if relative_root != format!("{INSTANCES_DIR}/{id}") {
            return Err("only desktop-created instances can be deleted".to_string());
        }
        if !self.instances[index].entry.desktop_created {
            return Err("only desktop-created instances can be deleted".to_string());
        }
        let root = self.data_dir.join(relative_root);

        // Release the server and root lock, then durably disable autostart
        // before removing anything beneath it. If the final roster write
        // fails, the stopped row remains safe to retry: a relaunch cannot
        // recreate an empty root over data that was already deleted.
        self.instances[index].host = None;
        let was_autostart = self.instances[index].entry.autostart;
        self.instances[index].entry.autostart = false;
        if let Err(error) = self.try_persist() {
            self.instances[index].entry.autostart = was_autostart;
            return Err(error);
        }
        match tokio::fs::remove_dir_all(&root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("could not delete {}: {error}", root.display()));
            }
        }
        let removed = self.instances.remove(index);
        if let Err(error) = self.try_persist() {
            self.instances.insert(index, removed);
            return Err(error);
        }
        Ok(())
    }

    /// Renames an instance. Its id, and therefore its data root, is untouched.
    pub fn rename(&mut self, id: &str, label: &str) -> Result<LocalInstanceInfo, String> {
        let label = label.trim();
        if label.is_empty() {
            return Err("an instance needs a name".to_string());
        }
        let index = self.index_of(id)?;
        self.instances[index].entry.label = label.to_string();
        self.persist();
        Ok(self.instances[index].info(&self.data_dir))
    }

    fn index_of(&self, id: &str) -> Result<usize, String> {
        self.instances
            .iter()
            .position(|instance| instance.entry.id == id)
            .ok_or_else(|| format!("no such instance: {id}"))
    }

    async fn start_at(&mut self, index: usize) {
        let entry = &self.instances[index].entry;
        let root = root_of(&self.data_dir, entry);
        // Every instance adopts what its root already holds and seeds nothing
        // — the one at the data root included, which used to be the exception.
        //
        // That exception was an ordering artifact rather than a decision. #632
        // seeded a starter company because a double-clicked application had no
        // other way in: there was no setup wizard yet, and an empty registry is
        // a login form addressing a company that does not exist. The wizard
        // arrived later and this arm was never revisited — so the one install
        // that most needs onboarding became the one install that could never
        // reach it. Seeding is not merely a head start: it makes `/spec` report
        // `setup_complete` (`stamp || !registry.is_empty()`), `ConnectionConsole`
        // enters its setup phase from that field and nothing else, and no
        // settings link re-opens the wizard. One silent answer, permanently.
        //
        // Note what does **not** change, because it is the whole risk of
        // touching this: an install that already has a company still boots
        // straight into it, with no wizard. Seeding was only ever the fallback
        // half of `desktop::bootstrap_companies`, reached when adoption came
        // back empty — so the arm dropped here could only ever fire on a root
        // holding no companies at all, which on the default instance means a
        // genuinely fresh install. Everything else is the adopt half, which
        // stays.
        //
        // #632's guarantee survives too, as a guarantee about *reachability*
        // rather than about seeding: the wizard is anonymous on loopback while
        // the registry is empty (`server::setup::authorize`), its model step is
        // skippable onto a curated roster, and finishing it seeds a template
        // through `desktop::seed_company`. Still enterable with no terminal, no
        // mail server and no credential — it asks first, which is the point.
        let first_run = FirstRun::RunSetupWizard;
        match embedded::start_with(root, first_run).await {
            Ok(host) => {
                tracing::info!(
                    id = %self.instances[index].entry.id,
                    address = %host.address(),
                    "local instance listening"
                );
                self.instances[index].host = Some(host);
                self.instances[index].error = None;
            }
            Err(error) => {
                tracing::warn!(
                    id = %self.instances[index].entry.id,
                    %error,
                    "local instance did not start"
                );
                self.instances[index].host = None;
                self.instances[index].error = Some(error.to_string());
            }
        }
    }

    /// A filesystem-safe id that no existing instance already uses.
    fn mint_id(&self, label: &str) -> String {
        let taken: HashSet<&str> = self
            .instances
            .iter()
            .map(|instance| instance.entry.id.as_str())
            .collect();
        let base = slugify(label);
        if !taken.contains(base.as_str()) {
            return base;
        }
        // Suffix rather than fail: two instances called "Acme" is an ordinary
        // thing to want, and the label is what the operator reads anyway.
        for n in 2.. {
            let candidate = format!("{base}-{n}");
            if !taken.contains(candidate.as_str()) {
                return candidate;
            }
        }
        unreachable!("an unbounded range always yields a free id")
    }

    fn persist(&self) {
        if let Err(error) = self.try_persist() {
            let path = self.data_dir.join(ROSTER_FILE);
            // Not fatal for reversible state changes: the instances running
            // right now keep running, and the roster stays at its last
            // successful write. Permanent deletion uses `try_persist`
            // directly because it cannot safely suppress this failure.
            tracing::error!(%error, path = %path.display(), "could not write the instance roster");
        }
    }

    fn try_persist(&self) -> Result<(), String> {
        self.try_persist_with(|| Ok(()))
    }

    fn try_persist_with<F>(&self, before_replace: F) -> Result<(), String>
    where
        F: FnOnce() -> std::io::Result<()>,
    {
        let roster = Roster {
            instances: self
                .instances
                .iter()
                .map(|instance| instance.entry.clone())
                .collect(),
        };
        let path = self.data_dir.join(ROSTER_FILE);
        let write = std::fs::create_dir_all(&self.data_dir).and_then(|()| {
            let body = serde_json::to_vec_pretty(&roster)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let mut temporary = tempfile::Builder::new()
                .prefix(".instances.json.")
                .tempfile_in(&self.data_dir)?;
            temporary.as_file_mut().write_all(&body)?;
            temporary.as_file().sync_all()?;
            before_replace()?;
            temporary.persist(&path).map_err(|error| error.error)?;
            #[cfg(unix)]
            std::fs::File::open(&self.data_dir)?.sync_all()?;
            Ok(())
        });
        write.map_err(|error| {
            format!(
                "could not write the instance roster at {}: {error}",
                path.display()
            )
        })
    }
}

/// Reads the roster, or invents the one every install has always had.
fn read_roster(data_dir: &Path) -> Roster {
    let path = data_dir.join(ROSTER_FILE);
    let parsed = std::fs::read(&path)
        .ok()
        .and_then(|body| serde_json::from_slice::<Roster>(&body).ok());
    let mut roster = parsed.unwrap_or_else(|| Roster {
        instances: Vec::new(),
    });

    // Drop anything a hand-edit or a partial write left unusable, and anything
    // whose root escapes the data dir. The roster is a plain file in a
    // directory an operator can open, and a `root` of `../../..` would point a
    // host — and its lock — at somewhere this application never chose.
    roster.instances.retain(|entry| {
        !entry.id.is_empty()
            && entry.id == slugify(&entry.id)
            && entry.root.as_deref().is_none_or(is_contained)
    });
    // By id across the whole file, not `dedup_by`, which only compares
    // neighbours. A roster holding `acme`, `other`, `acme` would keep both
    // `acme` rows, and two entries sharing an id share a *root*: the second
    // cannot start because the first holds its lock, so it becomes a permanent
    // failed row — and `index_of` resolves the first, so `rename`, `stop` and
    // `forget` could never reach it. Retaining the first occurrence keeps
    // listing order.
    let mut seen = HashSet::new();
    roster
        .instances
        .retain(|entry| seen.insert(entry.id.clone()));

    // The default is always present, and always first: it is the root every
    // pre-roster install already keeps its company in, and the one the console
    // opens on.
    if !roster
        .instances
        .iter()
        .any(|entry| entry.id == DEFAULT_INSTANCE_ID)
    {
        roster.instances.insert(
            0,
            RosterEntry {
                id: DEFAULT_INSTANCE_ID.to_string(),
                label: DEFAULT_INSTANCE_LABEL.to_string(),
                root: None,
                autostart: true,
                desktop_created: false,
            },
        );
    }
    roster
}

/// Whether a recorded root stays under the data dir.
fn is_contained(root: &str) -> bool {
    let path = Path::new(root);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// A lowercase, filesystem- and URL-safe form of a label.
///
/// Deliberately strict rather than clever: the result becomes a directory name
/// under the data dir, so everything outside `[a-z0-9-]` is folded to `-`
/// rather than transliterated. A label with nothing usable in it — every
/// non-Latin script, for instance — falls back to a fixed stem, and
/// [`LocalHosts::mint_id`] makes it unique.
fn slugify(label: &str) -> String {
    let mut out = String::new();
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return "instance".to_string();
    }
    // Long enough to stay readable, short enough to stay inside every
    // filesystem's component limit once the data dir is prepended.
    trimmed.chars().take(48).collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_label_becomes_a_safe_directory_name() {
        assert_eq!(slugify("Acme Corp"), "acme-corp");
        assert_eq!(slugify("  Acme/../etc  "), "acme-etc");
        assert_eq!(slugify("../../escape"), "escape");
        assert_eq!(slugify("日本語"), "instance");
        assert_eq!(slugify("A"), "a");
    }

    #[test]
    fn a_root_that_escapes_the_data_dir_is_refused() {
        assert!(is_contained("instances/acme"));
        assert!(!is_contained("../elsewhere"));
        assert!(!is_contained("/etc"));
        assert!(!is_contained("instances/../../elsewhere"));
    }

    /// A fresh install has exactly the instance it has always had, rooted where
    /// it has always been rooted.
    #[tokio::test]
    async fn a_fresh_data_dir_holds_one_instance_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = LocalHosts::load(dir.path().to_path_buf()).await;

        let listed = hosts.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, DEFAULT_INSTANCE_ID);
        assert!(listed[0].running, "{:?}", listed[0].error);
        assert_eq!(listed[0].data_dir, dir.path().display().to_string());
        assert!(hosts.default_instance().is_some());
    }

    /// The point of the whole module: two instances, two roots, two ports.
    #[tokio::test]
    async fn a_second_instance_gets_its_own_root_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;

        let created = hosts.create("Acme Corp").await.expect("it starts");
        assert_eq!(created.id, "acme-corp");
        assert!(created.running);
        assert_eq!(
            created.data_dir,
            dir.path().join("instances/acme-corp").display().to_string()
        );

        let listed = hosts.list();
        assert_eq!(listed.len(), 2);
        let ports: HashSet<_> = listed
            .iter()
            .map(|instance| instance.base_url.clone().expect("running"))
            .collect();
        assert_eq!(ports.len(), 2, "each instance binds its own port");
        // Two roots are two hosts, and the console tells them apart by this.
        assert_ne!(listed[0].instance_id, listed[1].instance_id);
    }

    /// The roster is what makes an instance a thing rather than a session.
    #[tokio::test]
    async fn instances_come_back_after_a_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let created = hosts.create("Acme").await.expect("it starts");
        let identity = created.instance_id.clone();
        // Quitting: every host is dropped, releasing its root and its port.
        drop(hosts);

        let relaunched = relaunch(dir.path()).await;
        let listed = relaunched.list();
        assert_eq!(listed.len(), 2);
        let acme = listed.iter().find(|i| i.id == "acme").expect("remembered");
        assert!(acme.running, "{:?}", acme.error);
        // The same data root is the same host, whatever port it landed on.
        assert_eq!(acme.instance_id, identity);
    }

    #[tokio::test]
    async fn a_stopped_instance_stays_stopped_across_a_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");

        let stopped = hosts.stop("acme").expect("a known instance");
        assert!(!stopped.running);
        assert!(stopped.base_url.is_none());
        drop(hosts);

        let relaunched = relaunch_until(dir.path(), default_is_running).await;
        let acme = relaunched
            .list()
            .into_iter()
            .find(|i| i.id == "acme")
            .expect("still rostered");
        assert!(!acme.running, "the stop button must survive a quit");
    }

    #[tokio::test]
    async fn a_stopped_instance_can_be_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let first = hosts.create("Acme").await.expect("it starts");
        hosts.stop("acme").expect("a known instance");

        let restarted = start_when_free(&mut hosts, "acme").await;
        assert!(restarted.running);
        assert_eq!(
            restarted.instance_id, first.instance_id,
            "the same root is the same host"
        );
    }

    /// The onboarding guarantee: a created instance opens the setup wizard.
    ///
    /// Asserted over HTTP on `/spec`, because that is the only thing the
    /// console actually consults — `ConnectionConsole` enters its `setup`
    /// phase on `setup_complete === false` and nothing else. And the field is
    /// computed as `stamp || !registry.is_empty()`, so "did it seed a company"
    /// and "does the wizard open" are the same question asked twice. Asserting
    /// an empty company list would prove only the first half.
    #[tokio::test]
    async fn a_created_instance_opens_the_setup_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let created = hosts.create("Acme").await.expect("it starts");

        assert!(
            created.companies.is_empty(),
            "an instance an operator asked for must not be handed a company they did not choose"
        );
        let spec: serde_json::Value = reqwest::get(format!(
            "{}/spec",
            created.base_url.as_deref().expect("running")
        ))
        .await
        .expect("the reported address answers")
        .json()
        .await
        .expect("a spec document");
        assert_eq!(
            spec["setup_complete"],
            serde_json::json!(false),
            "a fresh root must report setup as outstanding, or the wizard never opens: {spec}"
        );
    }

    /// And the instance at the data root gets the same guarantee, which is the
    /// half that used to be missing.
    ///
    /// The pair matters more than either alone: these two hosts made exactly
    /// one decision differently, and it was the decision that made onboarding
    /// unreachable on the only install most operators will ever have. Asserted
    /// over `/spec` for the reason the sibling test gives — `setup_complete` is
    /// the whole of what the console consults, so an empty company list would
    /// prove only half of it.
    #[tokio::test]
    async fn the_instance_at_the_data_root_opens_the_setup_wizard_too() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let default = hosts.default_instance().expect("it starts");

        assert!(
            default.companies.is_empty(),
            "a fresh install must not be handed a company nobody chose"
        );
        let spec: serde_json::Value = reqwest::get(format!(
            "{}/spec",
            default.base_url.as_deref().expect("running")
        ))
        .await
        .expect("the reported address answers")
        .json()
        .await
        .expect("a spec document");
        assert_eq!(
            spec["setup_complete"],
            serde_json::json!(false),
            "the wizard opens on the default instance, or nothing does: {spec}"
        );
    }

    /// The migration guarantee, and the only thing standing between this change
    /// and "my company is gone".
    ///
    /// Not seeding a *fresh* root must not mean ignoring a *used* one. Every
    /// install that has ever been opened keeps its company under the data dir
    /// itself (see the module header), and that company has to come back
    /// without a wizard in front of it — the operator set this machine up long
    /// ago and has nothing left to decide.
    #[tokio::test]
    async fn a_data_root_that_already_holds_a_company_boots_straight_into_it() {
        let dir = tempfile::tempdir().unwrap();
        // What a used install looks like on disk: a bundle under the data dir,
        // put there by an older launch that seeded, or by a completed wizard.
        let existing = seed_a_company_into(dir.path()).await;
        assert_eq!(existing.len(), 1, "the fixture writes one company");

        let hosts = relaunch(dir.path()).await;
        let default = hosts.default_instance().expect("it starts");

        assert_eq!(
            default.companies, existing,
            "a root with a company in it must adopt it, not ignore it"
        );
        let spec: serde_json::Value = reqwest::get(format!(
            "{}/spec",
            default.base_url.as_deref().expect("running")
        ))
        .await
        .expect("the reported address answers")
        .json()
        .await
        .expect("a spec document");
        assert_eq!(
            spec["setup_complete"],
            serde_json::json!(true),
            "an install that is already set up must never be walked through setup: {spec}"
        );
    }

    /// Skipping the *seed* must not skip the *adopt*.
    ///
    /// A company the wizard writes into a created instance's root is a bundle
    /// on disk and nothing else. If `RunSetupWizard` meant "register nothing",
    /// the instance would come back from every relaunch serving an empty
    /// registry — and, worse, reporting setup outstanding again, so the
    /// operator would be walked through the wizard once per launch.
    #[tokio::test]
    async fn a_created_instance_keeps_the_company_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");
        drop(hosts);

        // What completing the wizard leaves behind: a bundle in that root, put
        // there by the same seeding path the default instance uses.
        let root = dir.path().join("instances/acme");
        let seeded = seed_a_company_into(&root).await;

        let relaunched = relaunch(dir.path()).await;
        let acme = relaunched
            .list()
            .into_iter()
            .find(|instance| instance.id == "acme")
            .expect("still rostered");
        assert_eq!(
            acme.companies, seeded,
            "a created instance must adopt what its root already holds"
        );
    }

    /// Writes a company into `root` the way completing setup does, and returns
    /// its id. Uses a seeding host so the bundle is a real one.
    async fn seed_a_company_into(root: &Path) -> Vec<String> {
        take_root(root).await.companies().to_vec()
    }

    /// Starts a seeding host over `root`, retrying while a released `flock`
    /// clears.
    ///
    /// Every take in this module needs this, not just the ones that look like
    /// a relaunch. `flock` belongs to the open file description, so a
    /// subprocess spawned anywhere in this test binary between `fork` and
    /// `exec` holds a copy of the lock — and the suite spawns `git` constantly.
    /// A bare `expect` on a root released microseconds earlier therefore fails
    /// a few runs in five, and the failure reads as "the roster is broken"
    /// rather than "the kernel had not caught up".
    async fn take_root(root: &Path) -> EmbeddedHost {
        let mut last = None;
        for _ in 0..200 {
            match embedded::start(root.to_path_buf()).await {
                Ok(host) => return host,
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("a released root must become takeable: {last:?}");
    }

    #[tokio::test]
    async fn two_instances_may_share_a_label() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let first = hosts.create("Acme").await.expect("it starts");
        let second = hosts.create("Acme").await.expect("it starts");

        assert_eq!(first.id, "acme");
        assert_eq!(second.id, "acme-2");
        assert_ne!(first.data_dir, second.data_dir);
    }

    #[tokio::test]
    async fn forgetting_keeps_the_data_and_refuses_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");
        let root = dir.path().join("instances/acme");
        assert!(root.exists());

        assert!(
            hosts.forget(DEFAULT_INSTANCE_ID).is_err(),
            "the root instance is not removable"
        );
        hosts.forget("acme").expect("a known instance");

        assert_eq!(hosts.list().len(), 1);
        assert!(root.exists(), "forgetting is not deleting");
    }

    #[tokio::test]
    async fn deleting_removes_a_created_instance_and_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");
        let root = dir.path().join("instances/acme");
        std::fs::write(root.join("company-data"), "valuable").unwrap();

        hosts.delete("acme").await.expect("a created instance");

        assert_eq!(hosts.list().len(), 1);
        assert!(!root.exists(), "delete removes the instance data root");
        assert!(
            hosts.delete(DEFAULT_INSTANCE_ID).await.is_err(),
            "the application data root is never recursively deleted"
        );
    }

    #[tokio::test]
    async fn deleting_keeps_data_when_the_roster_cannot_be_updated() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");
        let root = dir.path().join("instances/acme");
        std::fs::write(root.join("company-data"), "valuable").unwrap();

        let roster = dir.path().join(ROSTER_FILE);
        let saved_roster = dir.path().join("instances.saved.json");
        std::fs::rename(&roster, &saved_roster).unwrap();
        std::fs::create_dir(&roster).unwrap();

        let error = hosts
            .delete("acme")
            .await
            .expect_err("an unwritable roster must fail deletion");

        assert!(error.contains("could not write the instance roster"));
        assert!(root.join("company-data").exists(), "data stays retryable");
        assert!(hosts.list().iter().any(|instance| instance.id == "acme"));

        std::fs::remove_dir(&roster).unwrap();
        std::fs::rename(&saved_roster, &roster).unwrap();
    }

    #[tokio::test]
    async fn a_failed_atomic_roster_replace_keeps_the_previous_roster() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");
        let roster = dir.path().join(ROSTER_FILE);
        let before = std::fs::read(&roster).unwrap();
        hosts.instances[1].entry.label = "Changed only in memory".to_string();

        let error = hosts
            .try_persist_with(|| Err(std::io::Error::other("injected before replace")))
            .expect_err("the injected replacement failure must be reported");

        assert!(error.contains("injected before replace"));
        assert_eq!(
            std::fs::read(&roster).unwrap(),
            before,
            "a failed replacement must not truncate or change the live roster"
        );
    }

    #[tokio::test]
    async fn deleting_refuses_a_hand_written_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(ROSTER_FILE),
            r#"{"instances":[{"id":"kept","label":"Kept","root":"instances/kept"}]}"#,
        )
        .unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let root = dir.path().join("instances/kept");
        assert!(root.exists());

        assert!(hosts.delete("kept").await.is_err());
        assert!(
            root.exists(),
            "delete only owns roots it minted under instances"
        );
    }

    #[tokio::test]
    async fn renaming_keeps_the_root_and_therefore_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let created = hosts.create("Acme").await.expect("it starts");

        let renamed = hosts.rename("acme", "Acme Holdings").expect("known");
        assert_eq!(renamed.label, "Acme Holdings");
        assert_eq!(renamed.id, created.id);
        assert_eq!(renamed.data_dir, created.data_dir);
        assert_eq!(renamed.instance_id, created.instance_id);
    }

    /// A busy root is a row with a reason on it, not a launch that fails.
    #[tokio::test]
    async fn an_instance_whose_root_is_held_is_reported_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        hosts.create("Acme").await.expect("it starts");
        drop(hosts);

        // Something else takes one of the two roots — a second window, or an
        // `opencompany serve` in a terminal. Retried, because a root released a
        // moment ago is not takeable a moment later: see `take_root`.
        let squatter = take_root(&dir.path().join("instances/acme")).await;

        let relaunched = relaunch_until(dir.path(), default_is_running).await;
        let listed = relaunched.list();
        assert_eq!(listed.len(), 2, "every instance still has a row");
        let acme = listed.iter().find(|i| i.id == "acme").unwrap();
        assert!(!acme.running);
        assert!(acme.error.is_some(), "the row says why");
        let default = listed.iter().find(|i| i.id == DEFAULT_INSTANCE_ID).unwrap();
        assert!(default.running, "one busy root must not stop the others");
        drop(squatter);
    }

    /// A stopped instance still says who it is.
    ///
    /// The console prunes its remembered connections against this list, and
    /// `removeConnection` forgets the persisted profile — so an instance that
    /// went quiet about its identity while stopped would have its connection id
    /// dropped, and with it the tour state, last-read channel and mail draft
    /// scoped to that id. Stopping is not forgetting, at either end.
    #[tokio::test]
    async fn a_stopped_instance_still_reports_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let running = hosts.create("Acme").await.expect("it starts");
        let identity = running.instance_id.clone();
        assert!(identity.is_some());

        let stopped = hosts.stop("acme").expect("a known instance");
        assert!(!stopped.running);
        assert!(stopped.base_url.is_none(), "nothing is listening");
        assert_eq!(
            stopped.instance_id, identity,
            "a stopped instance is the same host, and must still be recognisable as it"
        );
    }

    /// Duplicate ids are dropped wherever they sit, not only side by side.
    ///
    /// `dedup_by` compares neighbours, so the entry between the two `acme` rows
    /// is enough to defeat it. Two rows sharing an id share a root: the second
    /// never starts because the first holds the lock, and `index_of` resolves
    /// only the first, so `stop`, `rename` and `forget` cannot reach the other.
    #[tokio::test]
    async fn a_roster_repeating_an_id_out_of_order_keeps_one_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(ROSTER_FILE),
            r#"{"instances":[
                {"id":"acme","label":"Acme","root":"instances/acme"},
                {"id":"other","label":"Other","root":"instances/other"},
                {"id":"acme","label":"Acme again","root":"instances/acme"}
            ]}"#,
        )
        .unwrap();

        let hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let listed = hosts.list();
        let acme: Vec<_> = listed.iter().filter(|i| i.id == "acme").collect();
        assert_eq!(acme.len(), 1, "one row per id: {listed:?}");
        // The first occurrence, so listing order is what the file said.
        assert_eq!(acme[0].label, "Acme");
        assert!(listed.iter().any(|i| i.id == "other"), "{listed:?}");
        assert!(
            listed.iter().all(|i| i.running),
            "no row may be left holding a root another row already took: {listed:?}"
        );
    }

    /// A hand-edited roster cannot point a host outside the data dir.
    #[tokio::test]
    async fn a_roster_naming_an_escaping_root_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(ROSTER_FILE),
            r#"{"instances":[{"id":"evil","label":"Evil","root":"../../elsewhere"}]}"#,
        )
        .unwrap();

        let hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        let listed = hosts.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, DEFAULT_INSTANCE_ID);
    }

    /// An unreadable roster degrades to the install everyone already had.
    #[tokio::test]
    async fn a_corrupt_roster_falls_back_to_the_default_instance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ROSTER_FILE), "{ not json").unwrap();

        let hosts = LocalHosts::load(dir.path().to_path_buf()).await;
        assert_eq!(hosts.list().len(), 1);
        assert!(hosts.default_instance().is_some());
    }

    /// Loads over `root`, retrying while a just-released `flock` clears.
    ///
    /// See `embedded::test::stopping_a_host_frees_its_root_and_its_port` for
    /// why the release is not instantaneous. The condition is passed in
    /// because "settled" differs per test: one of these deliberately relaunches
    /// into a root something else is holding, where waiting for everything to
    /// run would wait forever.
    async fn relaunch_until(
        root: &Path,
        settled: impl Fn(&[LocalInstanceInfo]) -> bool,
    ) -> LocalHosts {
        for _ in 0..200 {
            let hosts = LocalHosts::load(root.to_path_buf()).await;
            if settled(&hosts.list()) {
                return hosts;
            }
            drop(hosts);
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("released roots must become takeable");
    }

    /// The condition for a relaunch where some instance is expected *not* to
    /// come back — only the root instance has to have taken its root.
    fn default_is_running(listed: &[LocalInstanceInfo]) -> bool {
        listed
            .iter()
            .any(|instance| instance.id == DEFAULT_INSTANCE_ID && instance.running)
    }

    /// The ordinary relaunch: every rostered instance that wants to run, runs.
    async fn relaunch(root: &Path) -> LocalHosts {
        relaunch_until(root, |listed| {
            listed.iter().all(|instance| instance.running)
        })
        .await
    }

    /// Starts `id`, retrying for the same reason `relaunch` does.
    async fn start_when_free(hosts: &mut LocalHosts, id: &str) -> LocalInstanceInfo {
        let mut last = None;
        for _ in 0..200 {
            match hosts.start(id).await {
                Ok(info) => return info,
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("a released root must become takeable: {last:?}");
    }
}
