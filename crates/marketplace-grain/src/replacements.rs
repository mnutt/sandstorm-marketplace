//! Signing authority changes. SPKs remain immutable; identity and eligibility are revalidated.
use crate::{audit::Event, database, ids, package_capnp as p, registry::Registry, spk};
use anyhow::{Context, Result, ensure};
use capnp::message::{Builder, HeapAllocator};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicBool,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    pub original: String,
    pub replacement: Option<String>,
    // None retains old signatures; Some([]) revokes every package signed by this key.
    pub allowed_packages: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub revision: i64,
    pub rules: Vec<Rule>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub mode: UpdateMode,
    pub revision: i64,
    pub rule: Rule,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateMode {
    Create,
    Edit,
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.rules.len() <= 256, "too many signing-key rules");
        let mut originals = BTreeSet::new();
        let mut replacements = BTreeSet::new();
        for rule in &self.rules {
            ids::parse_app_id(&rule.original)?;
            ensure!(originals.insert(&rule.original), "duplicate original key");
            if let Some(replacement) = &rule.replacement {
                ids::parse_app_id(replacement)?;
                ensure!(replacement != &rule.original, "a key cannot replace itself");
                ensure!(
                    replacements.insert(replacement),
                    "replacement key already belongs to another app"
                );
            }
            if let Some(packages) = &rule.allowed_packages {
                ensure!(packages.len() <= 256, "too many allowed packages");
                let mut seen = BTreeSet::new();
                for package in packages {
                    ensure!(
                        package.len() == 32
                            && package
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "invalid allowed package ID"
                    );
                    ensure!(seen.insert(package), "duplicate allowed package ID");
                }
            }
        }
        for rule in &self.rules {
            self.canonical(&rule.original)?;
        }
        Ok(())
    }
    pub fn canonical(&self, signing: &str) -> Result<String> {
        let mut id = signing.to_owned();
        for _ in 0..=self.rules.len() {
            match self
                .rules
                .iter()
                .find(|r| r.replacement.as_deref() == Some(&id))
            {
                Some(rule) => id = rule.original.clone(),
                None => return Ok(id),
            }
        }
        anyhow::bail!("cyclic signing-key replacement rules")
    }
    pub fn canonical_app(&self, signing: &[u8; 32], package: &[u8; 16]) -> Result<[u8; 32]> {
        let signing = ids::app_id(signing);
        if let Some(allowed) = self
            .rules
            .iter()
            .find(|rule| rule.original == signing)
            .and_then(|rule| rule.allowed_packages.as_ref())
        {
            ensure!(
                allowed.contains(&hex::encode(package)),
                "package signed with a revoked key"
            );
        }
        ids::parse_app_id(&self.canonical(&signing)?)
    }
    pub fn current_key(&self, canonical: &[u8; 32]) -> Result<[u8; 32]> {
        let mut id = ids::app_id(canonical);
        for _ in 0..=self.rules.len() {
            match self.rules.iter().find(|r| r.original == id) {
                Some(rule) => match &rule.replacement {
                    Some(replacement) => id = replacement.clone(),
                    None if rule.allowed_packages.is_none() => return ids::parse_app_id(&id),
                    None => anyhow::bail!("app signing key is revoked without a replacement"),
                },
                None => return ids::parse_app_id(&id),
            }
        }
        anyhow::bail!("cyclic signing-key replacement rules")
    }
}
pub fn bundled() -> Result<Settings> {
    let mut rules = Vec::new();
    for rule in crate::appid_replacements_capnp::APP_ID_REPLACEMENT_LIST.get()? {
        rules.push(Rule {
            original: rule.get_original()?.to_str()?.into(),
            replacement: if rule.has_replacement() {
                Some(rule.get_replacement()?.to_str()?.into())
            } else {
                None
            },
            allowed_packages: if rule.has_revoke_except_package_ids() {
                Some(
                    rule.get_revoke_except_package_ids()?
                        .iter()
                        .map(|v| Ok(v?.to_str()?.to_owned()))
                        .collect::<Result<_>>()?,
                )
            } else {
                None
            },
        });
    }
    let settings = Settings { revision: 0, rules };
    settings.validate()?;
    Ok(settings)
}
pub(crate) fn write_rules(db: &Connection, settings: &Settings) -> Result<()> {
    db.execute("DELETE FROM signing_key_rules", [])?;
    for rule in &settings.rules {
        db.execute(
            "INSERT INTO signing_key_rules(original,replacement,allowed_packages_json) VALUES(?,?,?)",
            params![
                rule.original,
                rule.replacement,
                rule.allowed_packages
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
            ],
        )?;
    }
    db.execute(
        "UPDATE registry_state SET key_rules_revision=?",
        [settings.revision],
    )?;
    Ok(())
}
pub(crate) fn read_rules(db: &Connection) -> Result<Settings> {
    let revision = db.query_row("SELECT key_rules_revision FROM registry_state", [], |r| {
        r.get(0)
    })?;
    let mut statement = db.prepare(
        "SELECT original,replacement,allowed_packages_json FROM signing_key_rules ORDER BY original",
    )?;
    let mut rows = statement.query([])?;
    let mut rules = Vec::new();
    while let Some(row) = rows.next()? {
        let allowed: Option<String> = row.get(2)?;
        rules.push(Rule {
            original: row.get(0)?,
            replacement: row.get(1)?,
            allowed_packages: allowed
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
        });
    }
    Ok(Settings { revision, rules })
}
struct CheckedPackage {
    id: String,
    metadata: Builder<HeapAllocator>,
    error: Option<String>,
}
pub(crate) struct PreparedUpdate {
    before: Settings,
    after: Settings,
    rule: Rule,
    package_ids: Vec<String>,
    packages: Vec<CheckedPackage>,
}
impl Registry {
    pub fn signing_key_rules(&self) -> Result<Settings> {
        self.database.read(read_rules)
    }
    pub(crate) fn prepare_key_rule(
        &self,
        update: Update,
        cancelled: &AtomicBool,
    ) -> Result<PreparedUpdate> {
        let before = self.signing_key_rules()?;
        ensure!(
            before.revision == update.revision,
            "signing-key rules changed; refresh and try again"
        );
        let existing = before
            .rules
            .iter()
            .find(|rule| rule.original == update.rule.original);
        match update.mode {
            UpdateMode::Create => ensure!(
                existing.is_none(),
                "a rule already exists for this key; use Edit"
            ),
            UpdateMode::Edit => ensure!(
                existing.is_some(),
                "rule no longer exists; refresh and try again"
            ),
        }
        let changed = existing != Some(&update.rule);
        let mut affected_apps = BTreeSet::new();
        if existing.and_then(|rule| rule.allowed_packages.as_ref())
            != update.rule.allowed_packages.as_ref()
        {
            affected_apps.insert(before.canonical(&update.rule.original)?);
        }
        let mut after = before.clone();
        if let Some(existing) = after
            .rules
            .iter_mut()
            .find(|r| r.original == update.rule.original)
        {
            ensure!(
                existing.replacement.is_none() || existing.replacement == update.rule.replacement,
                "existing key mappings cannot be changed; rotate again by adding a rule for the current replacement key"
            );
            *existing = update.rule.clone();
        } else {
            after.rules.push(update.rule.clone());
        }
        if changed {
            after.revision = before
                .revision
                .checked_add(1)
                .context("rules revision overflow")?;
        }
        after.validate()?;
        if let Some(replacement) = &update.rule.replacement {
            let original = before.canonical(replacement)?;
            if after.canonical(replacement)? != original {
                affected_apps.insert(original);
            }
        }
        // Capture every ID for the commit-time race check, but only open SPKs
        // whose canonical identity or signing-key eligibility can change.
        let rows = self.database.read(|db| {
            Ok(db
                .prepare("SELECT package_id,app_id FROM releases ORDER BY package_id")?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let package_ids = rows.iter().map(|(id, _)| id.clone()).collect();
        let mut packages = Vec::new();
        for (id, app) in rows {
            if !affected_apps.contains(&app) {
                continue;
            }
            crate::work::check_cancelled(cancelled)?;
            let result = spk::verify_with_rules(
                &self.package_dir(&id)?.join("spk"),
                &self.root.join("tmp"),
                cancelled,
                &after,
            );
            crate::work::check_cancelled(cancelled)?;
            let (metadata, error) = match result {
                Ok(message) => (message, None),
                Err(error) => {
                    // Infrastructure failures must abort, rather than revoke packages.
                    if error.chain().any(|cause| cause.is::<std::io::Error>()) {
                        return Err(error).context("could not revalidate stored package");
                    }
                    // Keep blocked releases in the same canonical history too. This
                    // metadata remains ineligible until a later verification succeeds.
                    let previous = self.metadata(&id)?;
                    let info = previous.get_root::<p::verified_info::Reader>()?;
                    let app = after.canonical(&ids::app_id(&ids::app_bytes(info.get_app_id()?)))?;
                    let mut message = Builder::new_default();
                    message.set_root(info)?;
                    ids::set_app(
                        message
                            .get_root::<p::verified_info::Builder>()?
                            .init_app_id(),
                        &ids::parse_app_id(&app)?,
                    );
                    (
                        message,
                        Some(format!("{error:#}").chars().take(1000).collect()),
                    )
                }
            };
            packages.push(CheckedPackage {
                id,
                metadata,
                error,
            });
        }
        Ok(PreparedUpdate {
            before,
            after,
            rule: update.rule,
            package_ids,
            packages,
        })
    }
    pub(crate) fn commit_key_rule(
        &self,
        prepared: PreparedUpdate,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        crate::work::check_cancelled(cancelled)?;
        ensure!(
            self.signing_key_rules()?.revision == prepared.before.revision,
            "signing-key rules changed; refresh and try again"
        );
        ensure!(
            self.package_ids()? == prepared.package_ids,
            "packages changed during revalidation; try again"
        );
        if prepared.before.revision == prepared.after.revision {
            return self
                .publish()
                .context("Rule unchanged, but public site rebuild failed; try saving again");
        }
        self.database.transaction(|db| {
            let mut events = Vec::new();
            let mut remaps = BTreeMap::new();
            // A single rule can join two identity chains. Include history from
            // rules created before any package was uploaded, too.
            if let Some(replacement) = &prepared.rule.replacement {
                let old = prepared.before.canonical(replacement)?;
                let target = prepared.after.canonical(replacement)?;
                if old != target {
                    remaps.insert(old, target);
                }
            }
            for (old, target) in &remaps {
                merge_history(db, old, target)?;
            }
            for package in prepared.packages {
                let old_error: Option<String> = db.query_row(
                    "SELECT key_error FROM releases WHERE package_id=?",
                    [&package.id], |row| row.get(0),
                )?;
                let info = package.metadata.get_root_as_reader::<p::verified_info::Reader>()?;
                let app = ids::app_id(&ids::app_bytes(info.get_app_id()?));
                let build = info.get_version();
                db.execute(
                    "UPDATE releases SET app_id=?,verified_metadata=?,key_error=? WHERE package_id=?",
                    params![app, database::encode(&package.metadata)?, package.error, package.id],
                )?;
                if package.error != old_error {
                    events.push(Event::new(
                        &app, Some((&package.id, build)), "signingKeyValidationChanged",
                        self.actor.clone(), serde_json::json!({"error": package.error}),
                    ));
                }
            }
            write_rules(db, &prepared.after)?;
            let canonical = prepared.after.canonical(&prepared.rule.original)?;
            events.push(Event::new(
                &canonical, None, "signingKeyRuleChanged", self.actor.clone(),
                serde_json::json!({
                    "previous": prepared.before.rules.iter().find(|rule| rule.original == prepared.rule.original),
                    "next": prepared.rule,
                    "remappedApps": remaps,
                }),
            ));
            database::append_events(db, events)?;
            db.execute("UPDATE registry_state SET state_revision=state_revision+1", [])?;
            // Remove public hard links durably before committing revocation. If
            // this fails, roll back the rule rather than claim it took effect.
            remove_blocked_downloads(db, &self.root)?;
            Ok(())
        })?;
        self.publish().context("Signing-key rule saved, but public site rebuild failed; reopen the rule and save again to rebuild")
    }
    pub(crate) fn key_error(&self, id: &str) -> Result<Option<String>> {
        self.database.read(|db| {
            Ok(db.query_row(
                "SELECT key_error FROM releases WHERE package_id=?",
                [id],
                |r| r.get(0),
            )?)
        })
    }
}

// Also clean recovery trees so a failed/fallback publication cannot restore a
// revoked download. Original private SPKs remain intact for review and restoration.
pub(crate) fn remove_blocked_downloads(db: &Connection, root: &std::path::Path) -> Result<()> {
    use std::{fs, io::ErrorKind};
    let ids = db
        .prepare("SELECT package_id FROM releases WHERE key_error IS NOT NULL")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.is_empty() {
        return Ok(());
    }
    for tree in ["www", "www.publish-previous", "www.publish-stage"] {
        let directory = root.join(tree).join("packages");
        match fs::metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        for id in &ids {
            match fs::remove_file(directory.join(id)) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("removing revoked public download"),
            }
        }
        // Sync even if a prior attempt unlinked the files but failed to sync.
        fs::File::open(&directory)?.sync_all()?;
    }
    Ok(())
}

// Called inside the rule-update transaction, before changing release identities.
fn merge_history(db: &Connection, old: &str, target: &str) -> Result<()> {
    database::ensure_app(db, old)?;
    database::ensure_app(db, target)?;
    // Never silently discard app-level decisions when identities merge.
    let configured: bool = db.query_row(
        "SELECT pinned_package_id IS NOT NULL OR deprecated=1 OR description_override IS NOT NULL
         FROM apps WHERE app_id=?",
        [old],
        |row| row.get(0),
    )?;
    ensure!(
        !configured,
        "app {old} has a rollback, deprecation, or description override; clear it before merging signing identities"
    );
    let referenced: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM apps WHERE replacement_app_id=?)",
        [old],
        |row| row.get(0),
    )?;
    ensure!(
        !referenced,
        "app {old} is a deprecation destination; update those deprecation links before merging signing identities"
    );
    // Preserve original history and its URLs; copy it into the canonical history
    // with provenance. Existing audit/publication references remain valid.
    let offset: i64 = db.query_row(
        "SELECT next_event_number-1 FROM apps WHERE app_id=?",
        [target],
        |row| row.get(0),
    )?;
    let count: i64 = db.query_row(
        "SELECT next_event_number-1 FROM apps WHERE app_id=?",
        [old],
        |row| row.get(0),
    )?;
    db.execute(
        "INSERT INTO audit_events
            (app_id,event_number,timestamp,package_id,build_number,action,actor_json,details_json)
         SELECT ?,event_number+?,timestamp,package_id,build_number,action,actor_json,
            json_set(CASE WHEN json_type(details_json)='object' THEN details_json
                          ELSE json_object('previousDetails',json(details_json)) END,
                     '$.previousAppId',app_id,'$.previousEventNumber',event_number)
         FROM audit_events WHERE app_id=?",
        params![target, offset, old],
    )?;
    db.execute(
        "UPDATE apps SET next_event_number=next_event_number+? WHERE app_id=?",
        params![count, target],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_preserves_history_before_any_packages_exist() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().into())?;
        let cancelled = AtomicBool::new(false);
        let a = ids::app_id(&[1; 32]);
        let b = ids::app_id(&[2; 32]);
        let c = ids::app_id(&[3; 32]);
        for (revision, original, replacement) in [(0, &b, &c), (1, &a, &b), (2, &a, &b)] {
            let update = Update {
                mode: if revision < 2 {
                    UpdateMode::Create
                } else {
                    UpdateMode::Edit
                },
                revision,
                rule: Rule {
                    original: original.clone(),
                    replacement: Some(replacement.clone()),
                    allowed_packages: None,
                },
            };
            registry.commit_key_rule(registry.prepare_key_rule(update, &cancelled)?, &cancelled)?;
        }
        let events = registry.audit_page(&a, None)?.events;
        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.details["previousAppId"] == b)
                .count(),
            1
        );
        assert_eq!(registry.audit_page(&b, None)?.events.len(), 1);
        Ok(())
    }

    #[test]
    fn stale_and_cancelled_updates_do_not_commit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().into())?;
        let update = || Update {
            mode: UpdateMode::Create,
            revision: 0,
            rule: Rule {
                original: ids::app_id(&[1; 32]),
                replacement: Some(ids::app_id(&[2; 32])),
                allowed_packages: None,
            },
        };
        let cancelled = AtomicBool::new(false);
        let first = registry.prepare_key_rule(update(), &cancelled)?;
        let stale = registry.prepare_key_rule(update(), &cancelled)?;
        let aborted = registry.prepare_key_rule(update(), &cancelled)?;
        assert!(
            registry
                .commit_key_rule(aborted, &AtomicBool::new(true))
                .is_err()
        );
        assert_eq!(registry.signing_key_rules()?.revision, 0);
        registry.commit_key_rule(first, &cancelled)?;
        assert!(registry.commit_key_rule(stale, &cancelled).is_err());
        assert_eq!(registry.signing_key_rules()?.revision, 1);
        Ok(())
    }
}
