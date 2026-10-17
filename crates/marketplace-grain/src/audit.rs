//! Private, per-app history. Only the open 100-event chunk is rewritten.
//! A short-lived redo journal couples registry writes with their audit records.
use crate::{
    ids,
    registry::{Registry, atomic_write},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs::{self, File},
    path::PathBuf,
};

const CHUNK: u64 = 100;
const PAGE: u64 = 50;
const MAX_EVENT: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub kind: String,
    pub identity_id: Option<String>,
    pub display_name: String,
    pub signing_key: Option<String>,
}
impl Default for Actor {
    fn default() -> Self {
        Self {
            kind: "system".into(),
            identity_id: None,
            display_name: "Marketplace system".into(),
            signing_key: None,
        }
    }
}
impl Actor {
    pub fn session(user: crate::identity_capnp::user_info::Reader<'_>, api: bool) -> Result<Self> {
        let id = user.get_identity_id()?;
        ensure!(
            id.is_empty() || id.len() == 32,
            "invalid Sandstorm identity ID"
        );
        Ok(Self {
            kind: if id.is_empty() {
                "unidentifiedSession"
            } else {
                "sandstormIdentity"
            }
            .into(),
            identity_id: (!id.is_empty()).then(|| hex::encode(id)),
            display_name: if id.is_empty() {
                if api {
                    "Unidentified API session"
                } else {
                    "Unidentified web session"
                }
                .into()
            } else {
                user.get_display_name()?
                    .get_default_text()?
                    .to_str()?
                    .chars()
                    .take(256)
                    .collect()
            },
            signing_key: None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub number: u64,
    pub timestamp: String,
    pub app_id: String,
    pub package_id: Option<String>,
    pub version: Option<u32>,
    pub action: String,
    pub actor: Actor,
    pub details: Value,
}
impl Event {
    pub fn new(
        app: &str,
        package: Option<(&str, u32)>,
        action: &str,
        actor: Actor,
        details: Value,
    ) -> Self {
        Self {
            number: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_id: app.into(),
            package_id: package.map(|p| p.0.into()),
            version: package.map(|p| p.1),
            action: action.into(),
            actor,
            details,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub events: Vec<Event>,
    pub before: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Transaction {
    pub writes: Vec<(PathBuf, Vec<u8>)>,
    pub moves: Vec<(PathBuf, PathBuf)>,
    #[serde(default)]
    pub cleanup: Vec<PathBuf>,
}
fn read_head(path: &std::path::Path) -> Result<u64> {
    match fs::read(path.join("head.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}
fn chunk_path(path: &std::path::Path, number: u64) -> PathBuf {
    path.join(format!("{:012}.json", (number - 1) / CHUNK))
}
impl Registry {
    pub fn with_actor(&self, actor: Actor) -> Self {
        let mut registry = self.clone();
        registry.actor = actor;
        registry
    }
    pub(crate) fn recover_audit(&self) -> Result<()> {
        let _guard = self.audit_lock.lock().unwrap();
        self.recover_audit_locked()
    }
    fn recover_audit_locked(&self) -> Result<()> {
        let path = self.root.join("audit-transaction.json");
        match fs::read(&path) {
            Ok(bytes) => self.apply_audit_transaction(&serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("reading audit recovery journal"),
        }
    }
    // Caller holds audit_lock and the complete transaction is already durable.
    // Both recovery and normal commits apply exactly the same idempotent writes.
    fn apply_audit_transaction(&self, transaction: &Transaction) -> Result<()> {
        for (source, destination) in &transaction.moves {
            if source.exists() {
                fs::rename(source, destination)?;
                File::open(destination.parent().context("missing destination parent")?)?
                    .sync_all()?;
                File::open(source.parent().context("missing source parent")?)?.sync_all()?;
            } else {
                ensure!(
                    destination.exists(),
                    "audit recovery is missing staged data"
                );
            }
        }
        for (destination, bytes) in &transaction.writes {
            fs::create_dir_all(destination.parent().context("missing parent")?)?;
            atomic_write(destination, bytes)?;
            let mut parent = destination.parent();
            while let Some(directory) = parent {
                File::open(directory)?.sync_all()?;
                if directory == self.root {
                    break;
                }
                parent = directory.parent();
            }
        }
        for directory in &transaction.cleanup {
            if directory.exists() {
                fs::remove_dir_all(directory)?;
            }
            File::open(directory.parent().context("cleanup parent")?)?.sync_all()?;
        }
        // Persist newly-created app directories before discarding recovery data.
        if self.root.join("audit").exists() {
            File::open(self.root.join("audit"))?.sync_all()?;
        }
        File::open(&self.root)?.sync_all()?;
        fs::remove_file(self.root.join("audit-transaction.json"))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub(crate) fn audit_commit(
        &self,
        mut transaction: Transaction,
        events: Vec<Event>,
    ) -> Result<()> {
        let _guard = self.audit_lock.lock().unwrap();
        self.recover_audit_locked()?;
        let mut grouped = BTreeMap::<String, Vec<Event>>::new();
        for event in events {
            ids::parse_app_id(&event.app_id)?;
            grouped.entry(event.app_id.clone()).or_default().push(event);
        }
        for (app, events) in grouped {
            let directory = self.root.join("audit").join(&app);
            let mut head = read_head(&directory)?;
            let mut chunks = BTreeMap::<PathBuf, Vec<Event>>::new();
            let mut latest_decision = None;
            for mut event in events {
                head = head.checked_add(1).context("audit event number overflow")?;
                event.number = head;
                let bytes = serde_json::to_vec(&event)?;
                ensure!(
                    bytes.len() <= MAX_EVENT,
                    "audit event exceeds 32 KiB; shorten the reason"
                );
                let path = chunk_path(&directory, head);
                let chunk = match chunks.entry(path) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let existing = if (head - 1) % CHUNK == 0 {
                            Vec::new()
                        } else {
                            serde_json::from_slice(&fs::read(entry.key())?)?
                        };
                        entry.insert(existing)
                    }
                };
                // Retain the latest state-changing decision as publication context.
                if !matches!(
                    event.action.as_str(),
                    "publicationChanged" | "baseline" | "submitted" | "resubmitted"
                ) {
                    latest_decision = Some(bytes);
                }
                chunk.push(event);
            }
            if let Some(bytes) = latest_decision {
                transaction
                    .writes
                    .push((directory.join("cause.json"), bytes));
            }
            for (path, events) in chunks {
                transaction
                    .writes
                    .push((path, serde_json::to_vec(&events)?));
            }
            transaction
                .writes
                .push((directory.join("head.json"), serde_json::to_vec(&head)?));
        }
        atomic_write(
            &self.root.join("audit-transaction.json"),
            &serde_json::to_vec(&transaction)?,
        )?;
        self.apply_audit_transaction(&transaction)
    }
    pub fn audit_page(&self, app: &str, before: Option<u64>) -> Result<Page> {
        ids::parse_app_id(app)?;
        let _guard = self.audit_lock.lock().unwrap();
        // Reads never expose a partially applied audit transaction.
        self.recover_audit_locked()?;
        let path = self.root.join("audit").join(app);
        let head = read_head(&path)?;
        let end = before
            .map(|n| n.saturating_sub(1).min(head))
            .unwrap_or(head);
        let start = end.saturating_sub(PAGE - 1).max(1);
        let mut events = Vec::new();
        if end != 0 {
            let mut number = end;
            loop {
                let chunk: Vec<Event> =
                    serde_json::from_slice(&fs::read(chunk_path(&path, number))?)?;
                events.extend(
                    chunk
                        .into_iter()
                        .rev()
                        .filter(|event| event.number >= start && event.number <= end),
                );
                let first = ((number - 1) / CHUNK) * CHUNK + 1;
                if first <= start {
                    break;
                }
                number = first - 1;
            }
        }
        Ok(Page {
            events,
            before: (start > 1 && end != 0).then_some(start),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn event(app: &str) -> Event {
        Event::new(
            app,
            None,
            "approved",
            Actor::default(),
            json!({"reason": "test"}),
        )
    }
    #[test]
    fn thousands_of_events_paginate_without_scanning_old_chunks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let registry = Registry::new(temp.path().to_owned())?;
        let app = ids::app_id(&[1; 32]);
        registry.audit_commit(
            Transaction::default(),
            (0..10003).map(|_| event(&app)).collect(),
        )?;
        let first = registry.audit_page(&app, None)?;
        assert_eq!(first.events.len(), 50);
        assert_eq!(first.events[0].number, 10003);
        assert_eq!(first.events[49].number, 9954);
        // New arrivals don't move the cursor for the next page.
        registry.audit_commit(Transaction::default(), vec![event(&app)])?;
        let mut before = first.before;
        let mut numbers: Vec<_> = first.events.iter().map(|e| e.number).collect();
        while let Some(cursor) = before {
            let page = registry.audit_page(&app, Some(cursor))?;
            assert!(page.events.len() <= 50);
            numbers.extend(page.events.iter().map(|e| e.number));
            before = page.before;
        }
        assert_eq!(numbers, (1..=10003).rev().collect::<Vec<_>>());
        assert!(registry.audit_page(&app, Some(0))?.events.is_empty());
        assert!(registry.audit_page("../packages", None).is_err());
        // A damaged ancient chunk cannot affect reads/appends of the latest page.
        fs::write(
            chunk_path(&temp.path().join("audit").join(&app), 1),
            "damaged",
        )?;
        registry.audit_commit(Transaction::default(), vec![event(&app)])?;
        assert_eq!(registry.audit_page(&app, None)?.events[0].number, 10005);
        assert!(registry.audit_page(&app, Some(51)).is_err());
        let other = ids::app_id(&[2; 32]);
        assert!(registry.audit_page(&other, None)?.events.is_empty());
        Ok(())
    }
    #[test]
    fn interrupted_state_and_history_commit_recovers_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let registry = Registry::new(temp.path().to_owned())?;
        let app = ids::app_id(&[1; 32]);
        let blocked = chunk_path(&temp.path().join("audit").join(&app), 1);
        fs::create_dir_all(&blocked)?;
        let state = temp.path().join("decision");
        assert!(
            registry
                .audit_commit(
                    Transaction {
                        writes: vec![(state.clone(), b"approved".to_vec())],
                        ..Default::default()
                    },
                    vec![event(&app)]
                )
                .is_err()
        );
        assert_eq!(fs::read(&state)?, b"approved");
        assert!(temp.path().join("audit-transaction.json").exists());
        fs::remove_dir(blocked)?;
        let recovered = Registry::new(temp.path().to_owned())?;
        assert_eq!(recovered.audit_page(&app, None)?.events.len(), 1);
        assert!(!temp.path().join("audit-transaction.json").exists());
        recovered.recover_audit()?;
        assert_eq!(recovered.audit_page(&app, None)?.events.len(), 1);
        Ok(())
    }
    #[test]
    fn oversized_event_cannot_change_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let registry = Registry::new(temp.path().to_owned())?;
        let app = ids::app_id(&[1; 32]);
        let mut event = event(&app);
        event.details = json!("x".repeat(MAX_EVENT));
        let state = temp.path().join("decision");
        assert!(
            registry
                .audit_commit(
                    Transaction {
                        writes: vec![(state.clone(), b"approved".to_vec())],
                        ..Default::default()
                    },
                    vec![event]
                )
                .is_err()
        );
        assert!(!state.exists());
        assert!(registry.audit_page(&app, None)?.events.is_empty());
        Ok(())
    }
    #[test]
    fn recovery_tolerates_already_committed_rename() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let registry = Registry::new(temp.path().to_owned())?;
        let staging = temp.path().join("staging");
        fs::create_dir(&staging)?;
        let source = staging.join("staged");
        let destination = temp.path().join("committed");
        fs::write(&source, "package")?;
        let tx = Transaction {
            moves: vec![(source.clone(), destination.clone())],
            cleanup: vec![staging.clone()],
            ..Default::default()
        };
        atomic_write(
            &temp.path().join("audit-transaction.json"),
            &serde_json::to_vec(&tx)?,
        )?;
        fs::rename(source, &destination)?;
        registry.recover_audit()?;
        assert_eq!(fs::read_to_string(destination)?, "package");
        assert!(!staging.exists());
        Ok(())
    }
}
