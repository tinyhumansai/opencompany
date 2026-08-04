//! One-shot boot migration off the legacy doubled home layout.
//!
//! Until the default home lost its `companies` leaf,
//! [`resolve_home`](crate::store::resolve_home) returned
//! `$HOME/.opencompany/companies` and [`Bundle`](crate::store::Bundle) appended a
//! `companies/` of its own, so a default local install's bundles sit one level
//! too deep:
//!
//! ```text
//! ~/.opencompany/companies/companies/<slug>/   ← legacy (doubled)
//! ~/.opencompany/companies/<slug>/             ← canonical
//! ```
//!
//! A local sqlite install is orphaned the same way: `serve` hands the resolved
//! home to [`open_storage`](crate::store::open_storage), so its database sits at
//! `~/.opencompany/companies/opencompany.db` rather than beside the workspace.
//!
//! Correcting the resolver without moving that data would silently orphan every
//! local company: the operator opens the console and their companies are gone,
//! which is worse than the wart itself. So `serve`, `export`, and `import` all
//! call [`migrate_legacy_nest_announced`] against the resolved home before
//! touching it. Running it for `export` and `import` too is not thoroughness for
//! its own sake: otherwise an un-migrated install's first post-upgrade command
//! fails to find its bundles.
//!
//! The migration is a detect-and-move:
//!
//! - No `<home>/companies/companies` directory is a no-op. A hosted tenant, whose
//!   home resolves to `/data`, takes this branch on every boot: two `stat`s that
//!   find nothing.
//! - A nest holding a `company.toml` or `meta.json` at its top level *is* a real
//!   bundle whose slug happens to be `companies`, and is left alone.
//! - Each entry is renamed up one level when the destination is free.
//! - A destination that already exists is **skipped with a warning naming both
//!   paths**, never merged: a user who ran the app both ways has two copies of
//!   one company, and interleaving two event logs and two signing keys is
//!   unrecoverable. Picking a winner is the same bet with the loser deleted.
//! - The nest directory is removed only once emptied, so a crash mid-loop simply
//!   resumes on the next boot. Idempotent by construction.
//! - The database and its `-wal`/`-shm` siblings resume the same way: the set is
//!   detected from *any* surviving member, not from the database alone, so a run
//!   that moved the database and then died is finished by the next boot rather
//!   than being mistaken for a completed one.
//! - Only **regular files** are ever treated as the database. A company slugged
//!   `opencompany.db` owns the directory at that exact path, and moving it would
//!   delete the company.
//! - A source another process moved first is a success, not a failure: `serve`
//!   and a hand-run `export` against one home both migrate, and neither should
//!   abort because the other won the race.
//!
//! Moves are announced on stderr rather than through `warn!`: the default
//! `EnvFilter` drops warnings unless `RUST_LOG` is set, which would make the
//! announcement exactly as invisible as the bug it reports.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::OpenCompanyError;

/// The local sqlite database — named by
/// [`open_storage`](crate::store::open_storage) — and its write-ahead-log
/// siblings.
///
/// These travel as a set: leaving a `-wal` behind either strands committed
/// transactions or pairs a moved database with a stale log. The whole slice is
/// also the *detector*, not just the payload — a run that moved the database and
/// then died is recognised by whichever member it left behind, which is what
/// makes the move resumable without a migration journal.
const LEGACY_SQLITE_FILES: &[&str] =
    &["opencompany.db", "opencompany.db-wal", "opencompany.db-shm"];

/// Marker files that prove a directory is a company bundle rather than a nest of
/// them. Their presence at `<home>/companies/companies` means the operator has a
/// real company slugged `companies`, which must not be dissolved.
const BUNDLE_MARKERS: &[&str] = &["company.toml", "meta.json"];

/// What a migrated path holds, so the operator message names it accurately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relocated {
    /// A company bundle directory.
    Company,
    /// The local sqlite database or one of its `-wal`/`-shm` siblings.
    Database,
}

/// A same-named entry that already existed at its destination. Both copies are
/// left exactly where they are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision {
    /// What the two copies hold.
    pub what: Relocated,
    /// The copy still sitting in the legacy nested layout.
    pub legacy: PathBuf,
    /// The copy already at the canonical location.
    pub destination: PathBuf,
}

/// What [`migrate_legacy_nest`] did, so the caller can report it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NestMigration {
    /// Paths moved up one level, in their new canonical locations.
    pub moved: Vec<(Relocated, PathBuf)>,
    /// Entries skipped because the destination was occupied.
    pub collisions: Vec<Collision>,
}

impl NestMigration {
    /// True when nothing moved and nothing collided: the ordinary case on every
    /// boot after the first, and on every hosted boot.
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.collisions.is_empty()
    }

    /// Operator-facing lines describing the migration, in report order. Empty
    /// when [`is_empty`](Self::is_empty), so a settled install stays silent.
    pub fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (what, destination) in &self.moved {
            let noun = match what {
                Relocated::Company => "company bundle",
                Relocated::Database => "local database file",
            };
            lines.push(format!(
                "moved {noun} up out of the legacy nested layout: {}",
                destination.display()
            ));
        }
        for collision in &self.collisions {
            let advice = match collision.what {
                Relocated::Company => {
                    "Two copies of one company cannot be merged: interleaving two \
                     event logs and two signing keys is not recoverable. Keep one \
                     by hand and move or delete the other."
                }
                Relocated::Database => {
                    "Two databases hold two separate histories. Keep one by hand \
                     and move or delete the other, including its -wal and -shm \
                     siblings."
                }
            };
            lines.push(format!(
                "left both copies in place: {} already exists, so the legacy copy \
                 stays at {}. {advice}",
                collision.destination.display(),
                collision.legacy.display(),
            ));
        }
        lines
    }
}

/// Moves a legacy `<home>/companies/companies/<slug>` install up one level, plus
/// any local sqlite database orphaned beside it, and announces what moved on
/// stderr.
///
/// This is the boot entry point; [`migrate_legacy_nest`] is the silent core the
/// tests drive.
pub fn migrate_legacy_nest_announced(home: &Path) -> Result<NestMigration> {
    let migration = migrate_legacy_nest(home)?;
    for line in migration.report() {
        eprintln!("opencompany: {line}");
    }
    Ok(migration)
}

/// Migrates the legacy doubled layout under `home`. See the [module docs](self)
/// for the rules; idempotent, and a no-op when there is no nest.
pub fn migrate_legacy_nest(home: &Path) -> Result<NestMigration> {
    let companies = home.join("companies");
    let mut migration = NestMigration::default();
    migrate_bundles(&companies, &mut migration)?;
    migrate_sqlite(home, &companies, &mut migration)?;
    Ok(migration)
}

/// Renames every `<companies>/companies/<slug>` up into `<companies>/<slug>`.
fn migrate_bundles(companies: &Path, migration: &mut NestMigration) -> Result<()> {
    let nest = companies.join("companies");
    // The hosted no-op: one `stat` that finds nothing.
    if !nest.is_dir() {
        return Ok(());
    }
    // A real bundle that happens to be slugged `companies`. Dissolving it would
    // scatter its event log and keys across the companies directory.
    if BUNDLE_MARKERS
        .iter()
        .any(|marker| exists(&nest.join(marker)))
    {
        return Ok(());
    }

    let mut names = read_dir_names(&nest)?;
    names.sort();
    for name in names {
        let legacy = nest.join(&name);
        let destination = companies.join(&name);
        if exists(&destination) {
            migration.collisions.push(Collision {
                what: Relocated::Company,
                legacy,
                destination,
            });
            continue;
        }
        if rename_or_already_moved(&legacy, &destination)? {
            migration.moved.push((Relocated::Company, destination));
        }
    }

    // Only once emptied. A skipped collision keeps the nest, and the next boot
    // picks up where this one stopped.
    if read_dir_names(&nest)?.is_empty() {
        // A failure here leaves an empty directory, not lost data, and the next
        // boot retries. Never worth aborting a boot for.
        let _ = std::fs::remove_dir(&nest);
    }
    Ok(())
}

/// Moves a local sqlite database orphaned at `<home>/companies/opencompany.db`
/// beside the workspace at `<home>/opencompany.db`, with its `-wal`/`-shm`
/// siblings.
///
/// A hosted tenant never has one: it resolves its home to the data root, so its
/// database is written at `<root>/opencompany.db` to begin with.
fn migrate_sqlite(home: &Path, companies: &Path, migration: &mut NestMigration) -> Result<()> {
    // **Regular files only.** A company whose slug happens to be
    // `opencompany.db` has its canonical bundle at exactly
    // `<home>/companies/opencompany.db`, and a bare existence check accepts that
    // directory. Moving it up would delete the company from every bundle lookup
    // and hand sqlite a directory to open. `slug` permits `.`, so this is a name
    // an operator can really have.
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    for name in LEGACY_SQLITE_FILES {
        let legacy = companies.join(name);
        if is_regular_file(&legacy)? {
            moves.push((legacy, home.join(name)));
        }
    }
    // Detection deliberately does **not** hinge on the database file alone. A
    // previous run that moved `opencompany.db` and then died would leave the
    // `-wal` behind, and keying off the database would read that as "already
    // migrated" — pairing a relocated database with a stranded write-ahead log,
    // which loses committed transactions. Any surviving member of the set is a
    // migration to resume, so the next boot finishes what the last one started.
    if moves.is_empty() {
        return Ok(());
    }
    // An occupied destination among the files still to move means two databases,
    // i.e. two histories. Skip the whole set and say so.
    //
    // A resumed half-move is not this case: there the database is already at its
    // destination and is no longer among `moves`, so only the stranded siblings
    // are checked and they find their destinations free. The one shape that
    // cannot be told apart — a legacy `-wal` with no legacy `.db`, beside an
    // unrelated canonical database — is not one sqlite produces.
    if let Some((legacy, destination)) = moves.iter().find(|(_, dest)| exists(dest)) {
        migration.collisions.push(Collision {
            what: Relocated::Database,
            legacy: legacy.clone(),
            destination: destination.clone(),
        });
        return Ok(());
    }
    for (legacy, destination) in moves {
        if rename_or_already_moved(&legacy, &destination)? {
            migration.moved.push((Relocated::Database, destination));
        }
    }
    Ok(())
}

/// Existence that a dangling symlink still counts as occupying: plain
/// [`Path::exists`] follows the link and reports `false`, and a rename onto a
/// dangling symlink would silently replace it.
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Directory entry names, or an empty vec when the directory is gone.
fn read_dir_names(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(OpenCompanyError::StoreIo {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    entries
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|source| OpenCompanyError::StoreIo {
                    path: dir.to_path_buf(),
                    source,
                })
        })
        .collect()
}

/// Whether `path` is a regular file, following no symlink. Absent is `false`;
/// an unreadable path is an error rather than a silent `false`.
fn is_regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(OpenCompanyError::StoreIo {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Renames within one directory tree, returning whether *this* call performed
/// the move.
///
/// A source that vanished between enumeration and rename, whose destination now
/// exists, is another process having migrated it first — `Ok(false)`, not a
/// failure. Running `opencompany export` against a home a `serve` process is
/// booting is ordinary, and both commands migrate; aborting one of them on a
/// `NotFound` that means "already done" would be a boot failure with nothing
/// wrong behind it.
///
/// Every other failure aborts rather than continuing with half an install: a
/// runtime that silently comes up missing companies is the exact symptom this
/// migration exists to prevent. A `NotFound` with the destination *also* absent
/// is such a failure (a missing destination parent, say) and propagates.
fn rename_or_already_moved(from: &Path, to: &Path) -> Result<bool> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound && exists(to) => Ok(false),
        Err(source) => Err(OpenCompanyError::StoreIo {
            path: from.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A scratch home that cleans itself up, named per test so parallel runs
    /// never share a tree.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oc-migrate-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch home");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Creates a bundle directory holding one marker file.
        fn bundle(&self, relative: &str) -> PathBuf {
            let dir = self.0.join(relative);
            std::fs::create_dir_all(&dir).expect("bundle dir");
            std::fs::write(dir.join("company.toml"), "[company]\n").expect("manifest");
            dir
        }

        fn write(&self, relative: &str, body: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
            std::fs::write(&path, body).expect("file");
            path
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_nest_is_a_no_op() {
        // The hosted boot, and every local boot after the first.
        let home = TempHome::new("absent");
        home.bundle("companies/acme");

        let migration = migrate_legacy_nest(home.path()).expect("no nest to migrate");

        assert!(migration.is_empty());
        assert!(migration.report().is_empty(), "a no-op says nothing");
        assert!(home.path().join("companies/acme/company.toml").exists());
    }

    #[test]
    fn nested_bundles_move_up_one_level() {
        let home = TempHome::new("move");
        home.bundle("companies/companies/acme");
        home.bundle("companies/companies/globex");

        let migration = migrate_legacy_nest(home.path()).expect("migrates");

        assert!(home.path().join("companies/acme/company.toml").exists());
        assert!(home.path().join("companies/globex/company.toml").exists());
        // The emptied nest is removed, so the layout is genuinely canonical.
        assert!(!home.path().join("companies/companies").exists());
        assert_eq!(migration.moved.len(), 2);
        assert!(migration.collisions.is_empty());
        assert_eq!(migration.report().len(), 2);
    }

    #[test]
    fn re_running_is_silent() {
        let home = TempHome::new("idempotent");
        home.bundle("companies/companies/acme");

        migrate_legacy_nest(home.path()).expect("first run migrates");
        let second = migrate_legacy_nest(home.path()).expect("second run is a no-op");

        assert!(second.is_empty(), "{second:?}");
        assert!(home.path().join("companies/acme/company.toml").exists());
    }

    #[test]
    fn a_collision_keeps_both_copies_and_names_both_paths() {
        // The user who ran the app both ways. Two event logs and two signing
        // keys cannot be merged, so neither copy may be touched.
        let home = TempHome::new("collision");
        home.bundle("companies/acme");
        home.write("companies/acme/events.jsonl", "canonical\n");
        home.bundle("companies/companies/acme");
        home.write("companies/companies/acme/events.jsonl", "legacy\n");

        let migration = migrate_legacy_nest(home.path()).expect("migrates");

        assert!(migration.moved.is_empty());
        assert_eq!(migration.collisions.len(), 1);
        assert_eq!(migration.collisions[0].what, Relocated::Company);
        // Both copies survive with their own event logs.
        assert_eq!(
            std::fs::read_to_string(home.path().join("companies/acme/events.jsonl")).unwrap(),
            "canonical\n"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("companies/companies/acme/events.jsonl"))
                .unwrap(),
            "legacy\n"
        );
        // The nest survives too, since it is not empty.
        assert!(home.path().join("companies/companies").is_dir());
        // Both paths are named, so the operator can act without guessing.
        let report = migration.report().join("\n");
        assert!(
            report.contains(&home.path().join("companies/acme").display().to_string()),
            "{report}"
        );
        assert!(
            report.contains(
                &home
                    .path()
                    .join("companies/companies/acme")
                    .display()
                    .to_string()
            ),
            "{report}"
        );
    }

    #[test]
    fn a_collision_does_not_block_the_other_bundles() {
        let home = TempHome::new("partial");
        home.bundle("companies/acme");
        home.bundle("companies/companies/acme");
        home.bundle("companies/companies/globex");

        let migration = migrate_legacy_nest(home.path()).expect("migrates");

        assert_eq!(migration.moved.len(), 1);
        assert_eq!(migration.collisions.len(), 1);
        assert!(home.path().join("companies/globex/company.toml").exists());
        assert!(home.path().join("companies/companies/acme").is_dir());
    }

    #[test]
    fn a_company_genuinely_slugged_companies_is_untouched() {
        // `<home>/companies/companies` holding a manifest is a bundle, not a
        // nest. This guard is also what proves a hosted tenant is never
        // dissolved.
        let home = TempHome::new("real-slug");
        home.bundle("companies/companies");
        home.write("companies/companies/events.jsonl", "real bundle\n");

        let migration = migrate_legacy_nest(home.path()).expect("leaves the bundle alone");

        assert!(migration.is_empty());
        assert!(
            home.path()
                .join("companies/companies/company.toml")
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("companies/companies/events.jsonl")).unwrap(),
            "real bundle\n"
        );
    }

    #[test]
    fn a_meta_json_alone_also_marks_a_real_bundle() {
        // A bundle whose manifest is not materialized yet still has `meta.json`;
        // that is not a nest either.
        let home = TempHome::new("meta-marker");
        home.write("companies/companies/meta.json", "{}\n");

        let migration = migrate_legacy_nest(home.path()).expect("leaves the bundle alone");

        assert!(migration.is_empty());
        assert!(home.path().join("companies/companies/meta.json").exists());
    }

    #[test]
    fn an_orphaned_sqlite_database_moves_with_its_siblings() {
        // `serve` passed the resolved home to `open_storage`, so the legacy
        // default's database landed inside the bundle home.
        let home = TempHome::new("sqlite");
        home.bundle("companies/companies/acme");
        home.write("companies/opencompany.db", "db");
        home.write("companies/opencompany.db-wal", "wal");
        home.write("companies/opencompany.db-shm", "shm");

        let migration = migrate_legacy_nest(home.path()).expect("migrates");

        for name in LEGACY_SQLITE_FILES {
            assert!(home.path().join(name).exists(), "{name} moved up");
            assert!(
                !home.path().join("companies").join(name).exists(),
                "{name} left behind"
            );
        }
        assert_eq!(
            migration
                .moved
                .iter()
                .filter(|(what, _)| *what == Relocated::Database)
                .count(),
            3
        );
    }

    #[test]
    fn a_database_without_a_write_ahead_log_still_moves() {
        let home = TempHome::new("sqlite-clean");
        home.write("companies/opencompany.db", "db");

        migrate_legacy_nest(home.path()).expect("migrates");

        assert!(home.path().join("opencompany.db").exists());
        assert!(!home.path().join("companies/opencompany.db").exists());
    }

    #[test]
    fn a_database_at_the_destination_is_never_overwritten() {
        // Two databases means two histories. Overwriting one destroys an
        // install, so both are kept and the operator is told.
        let home = TempHome::new("sqlite-collision");
        home.write("companies/opencompany.db", "legacy");
        home.write("opencompany.db", "canonical");

        let migration = migrate_legacy_nest(home.path()).expect("migrates");

        assert!(migration.moved.is_empty());
        assert_eq!(migration.collisions.len(), 1);
        assert_eq!(migration.collisions[0].what, Relocated::Database);
        assert_eq!(
            std::fs::read_to_string(home.path().join("opencompany.db")).unwrap(),
            "canonical"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("companies/opencompany.db")).unwrap(),
            "legacy"
        );
    }

    #[test]
    fn a_wal_at_the_destination_blocks_the_whole_database_move() {
        // Half a move pairs a moved database with a stranded log, so a single
        // occupied sibling stops the set.
        let home = TempHome::new("wal-collision");
        home.write("companies/opencompany.db", "legacy");
        home.write("companies/opencompany.db-wal", "legacy wal");
        home.write("opencompany.db-wal", "canonical wal");

        let migration = migrate_legacy_nest(home.path()).expect("migrates");

        assert!(migration.moved.is_empty());
        assert_eq!(migration.collisions.len(), 1);
        assert!(!home.path().join("opencompany.db").exists());
        assert!(home.path().join("companies/opencompany.db").exists());
    }

    #[test]
    fn a_company_slugged_like_the_database_is_never_moved_as_one() {
        // `slug` permits `.`, so a company really can be called
        // `opencompany.db` — and its canonical bundle is at exactly the path the
        // legacy database would occupy. Moving that directory up would delete the
        // company from every bundle lookup and hand sqlite a directory to open.
        let home = TempHome::new("db-named-company");
        home.bundle("companies/opencompany.db");
        home.write("companies/opencompany.db/events.jsonl", "a real company\n");

        let migration = migrate_legacy_nest(home.path()).expect("leaves the bundle alone");

        assert!(migration.is_empty(), "{migration:?}");
        assert!(
            home.path()
                .join("companies/opencompany.db/company.toml")
                .exists()
        );
        assert!(
            !home.path().join("opencompany.db").exists(),
            "the bundle must not be relocated as a database"
        );
    }

    #[test]
    fn a_half_moved_database_set_is_finished_on_the_next_run() {
        // A run that moved `opencompany.db` and then died leaves the sidecars
        // behind. Keying detection off the database alone would call that
        // finished, pairing a relocated database with a stranded write-ahead log
        // — losing whatever the log still held.
        let home = TempHome::new("half-moved-db");
        home.write("opencompany.db", "already moved");
        home.write("companies/opencompany.db-wal", "stranded wal");
        home.write("companies/opencompany.db-shm", "stranded shm");

        let migration = migrate_legacy_nest(home.path()).expect("resumes");

        assert_eq!(migration.moved.len(), 2, "{migration:?}");
        assert!(migration.collisions.is_empty(), "{migration:?}");
        assert_eq!(
            std::fs::read_to_string(home.path().join("opencompany.db-wal")).unwrap(),
            "stranded wal"
        );
        assert!(home.path().join("opencompany.db-shm").exists());
        assert!(!home.path().join("companies/opencompany.db-wal").exists());
        // The already-moved database is untouched.
        assert_eq!(
            std::fs::read_to_string(home.path().join("opencompany.db")).unwrap(),
            "already moved"
        );
    }

    #[test]
    fn a_source_another_process_already_moved_is_not_a_failure() {
        // `serve` booting and a hand-run `export` against one home both migrate.
        // Losing that race must not abort the loser with a NotFound that means
        // "already done".
        let home = TempHome::new("raced");
        let destination = home.write("companies/acme/company.toml", "[company]\n");
        let vanished = home.path().join("companies/companies/acme/company.toml");

        assert!(
            !rename_or_already_moved(&vanished, &destination).expect("tolerated"),
            "a vanished source whose destination now exists did not move here",
        );

        // A NotFound with the destination *also* absent is a real failure and
        // still propagates, so this tolerance cannot mask a broken migration.
        let nowhere = home.path().join("companies/companies/globex/company.toml");
        assert!(rename_or_already_moved(&vanished, &nowhere).is_err());
    }

    #[test]
    fn an_empty_home_migrates_nothing() {
        let home = TempHome::new("empty");

        let migration = migrate_legacy_nest(home.path()).expect("nothing to do");

        assert!(migration.is_empty());
    }
}
