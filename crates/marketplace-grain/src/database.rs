//! Authoritative metadata storage. SPKs and generated publications remain files.
use crate::{
    audit::{Event, Page},
    ids, package_capnp as p,
    registry::{Message, PublicVisibility},
    submit_capnp as s,
};
use anyhow::{Context, Result, ensure};
use capnp::{message::Builder, serialize};
use marketplace_model::app_index::Deprecation;
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::BTreeMap, path::Path, sync::Mutex};

pub(crate) struct Database(Mutex<Connection>);
pub(crate) enum Change {
    Release {
        id: String,
        metadata: Vec<u8>,
        status: Vec<u8>,
        received_at: Option<String>,
        ever_approved: bool,
    },
    Status(String, Vec<u8>),
    Pin(String, String),
    Deprecations(BTreeMap<String, Deprecation>),
    Visibility(String, PublicVisibility),
    ImportedAuthor(String, String),
}
impl Database {
    pub fn open(root: &Path) -> Result<Self> {
        let mut connection = Connection::open(root.join("marketplace.sqlite"))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            matches!(version, 0..=2),
            "marketplace database is newer than this program"
        );
        if version < 2 {
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Recheck after acquiring the writer lock in case another opener initialized it.
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            match version {
                0 => {
                    tx.execute_batch(include_str!("schema.sql"))?;
                    tx.execute_batch(include_str!("notifications.sql"))?;
                    tx.execute_batch("PRAGMA user_version=2;")?;
                }
                1 => {
                    tx.execute_batch(include_str!("notifications.sql"))?;
                    tx.execute_batch("PRAGMA user_version=2;")?;
                }
                2 => {}
                _ => anyhow::bail!("unsupported marketplace database version"),
            }
            tx.commit()?;
            std::fs::File::open(root)?.sync_all()?;
        }
        // Use SQLite's rollback journal: one serialized writer, short reads, and
        // straightforward grain backups without a second persistent WAL file.
        Ok(Self(Mutex::new(connection)))
    }
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
        f(&connection)
    }
    pub fn transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut connection = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn commit(&self, changes: Vec<Change>, events: Vec<Event>) -> Result<()> {
        self.transaction(|tx| {
            for change in changes {
                apply(tx, change)?;
            }
            append_events(tx, events)?;
            tx.execute(
                "UPDATE registry_state SET state_revision=state_revision+1",
                [],
            )?;
            Ok(())
        })
    }
    pub fn contains(&self, id: &str) -> Result<bool> {
        self.read(|db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM releases WHERE package_id=?)",
                [id],
                |r| r.get(0),
            )?)
        })
    }
    pub fn ids(&self) -> Result<Vec<String>> {
        self.read(|db| {
            Ok(db
                .prepare("SELECT package_id FROM releases ORDER BY package_id")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
    pub fn metadata(&self, id: &str) -> Result<Message> {
        self.read(|db| {
            decode(&db.query_row(
                "SELECT verified_metadata FROM releases WHERE package_id=?",
                [id],
                |r| r.get::<_, Vec<u8>>(0),
            )?)
        })
    }
    pub fn status(&self, id: &str) -> Result<Message> {
        self.read(|db| {
            let (request, review, reason, published, sequence): (u16,String,String,i64,Vec<u8>) = db.query_row(
                "SELECT request_state,review_state,review_reason,first_published_at,next_sequence_number FROM releases WHERE package_id=?", [id],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            let mut message = Builder::new_default();
            let mut status = message.init_root::<s::submission_status::Builder>();
            status.set_request_state(s::SubmissionState::try_from(request)?);
            match review.as_str() {
                "approved" => status.set_approved(&reason),
                "needsUpdate" => status.set_needs_update(&reason),
                "notUploaded" => status.set_not_uploaded(()),
                _ => status.set_pending(()),
            }
            status.set_publish_date(published);
            status.set_next_sequence_number(u64::from_be_bytes(sequence.try_into().map_err(|_| anyhow::anyhow!("invalid sequence"))?));
            decode(&encode(&message)?)
        })
    }
    pub fn page(&self, app: &str, before: Option<u64>) -> Result<Page> {
        ids::parse_app_id(app)?;
        self.read(|db| {
            let mut statement = db.prepare("SELECT event_number,timestamp,package_id,build_number,action,actor_json,details_json FROM audit_events WHERE app_id=? AND event_number < ? ORDER BY event_number DESC LIMIT 51")?;
            let mut events = statement.query_map(params![app, before.unwrap_or(i64::MAX as u64).min(i64::MAX as u64) as i64], |row| event_row(app,row))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let more = events.len() > 50;
            events.truncate(50);
            let before = if more { events.last().map(|e| e.number) } else { None };
            Ok(Page { events, before })
        })
    }
    pub fn publication_cause(&self, generation: i64, app: &str) -> Result<Option<Event>> {
        self.read(|db| Ok(db.query_row("SELECT e.event_number,e.timestamp,e.package_id,e.build_number,e.action,e.actor_json,e.details_json FROM audit_events e JOIN publication_causes c ON c.app_id=e.app_id AND c.event_number=e.event_number WHERE c.generation_id=? AND c.app_id=?", params![generation,app], |r| event_row(app,r)).optional()?))
    }
    pub fn deprecations(&self) -> Result<BTreeMap<String, Deprecation>> {
        self.read(|db| {
            Ok(db
                .prepare("SELECT app_id,replacement_app_id FROM apps WHERE deprecated=1")?
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        Deprecation {
                            replacement_app_id: r.get(1)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
    pub fn visibility(&self, id: &str) -> Result<PublicVisibility> {
        self.read(|db| {
            Ok(serde_json::from_str(&db.query_row(
                "SELECT public_visibility FROM releases WHERE package_id=?",
                [id],
                |r| r.get::<_, String>(0),
            )?)?)
        })
    }
    pub fn profile(&self, fingerprint: &str) -> Result<Option<Vec<u8>>> {
        self.read(|db| {
            Ok(db
                .query_row(
                    "SELECT profile FROM author_profiles WHERE fingerprint=?",
                    [fingerprint],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }
    pub fn imported_author(&self, id: &str) -> Result<Option<String>> {
        self.read(|db| {
            Ok(db
                .query_row(
                    "SELECT profile_json FROM release_author_profiles WHERE package_id=?",
                    [id],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }
}
pub(crate) fn encode(message: &Builder<capnp::message::HeapAllocator>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    serialize::write_message(&mut bytes, message)?;
    Ok(bytes)
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Message> {
    Ok(serialize::read_message(
        bytes,
        crate::spk::limits(16 * 1024 * 1024),
    )?)
}
pub(crate) fn ensure_app(db: &Connection, app: &str) -> Result<()> {
    ids::parse_app_id(app)?;
    db.execute("INSERT OR IGNORE INTO apps(app_id) VALUES(?)", [app])?;
    Ok(())
}
pub(crate) fn apply(db: &Connection, change: Change) -> Result<()> {
    match change {
        Change::Release {
            id,
            metadata,
            status,
            received_at,
            ever_approved,
        } => {
            let message = decode(&metadata)?;
            let info = message.get_root::<p::verified_info::Reader>()?;
            ensure!(
                hex::encode(ids::package_bytes(info.get_package_id()?)) == id,
                "metadata package ID mismatch"
            );
            let app = ids::app_id(&ids::app_bytes(info.get_app_id()?));
            ensure_app(db, &app)?;
            // Initial status is replaced below within the same transaction.
            db.execute("INSERT INTO releases(package_id,app_id,build_number,verified_metadata,received_at,request_state,review_state,review_reason,first_published_at,ever_approved,next_sequence_number) VALUES(?,?,?,?,?,0,'pending','',0,?,zeroblob(8))",params![id,app,info.get_version(),metadata,received_at,ever_approved])?;
            apply(db, Change::Status(id, status))?;
        }
        Change::Status(id, bytes) => {
            let message = decode(&bytes)?;
            let status = message.get_root::<s::submission_status::Reader>()?;
            let (review, reason) = match status.which()? {
                s::submission_status::Approved(v) => ("approved", v?.to_str()?),
                s::submission_status::NeedsUpdate(v) => ("needsUpdate", v?.to_str()?),
                s::submission_status::Pending(()) => ("pending", ""),
                s::submission_status::NotUploaded(()) => ("notUploaded", ""),
            };
            ensure!(db.execute("UPDATE releases SET request_state=?,review_state=?,review_reason=?,first_published_at=?,next_sequence_number=?,ever_approved=ever_approved OR ? WHERE package_id=?",params![u16::from(status.get_request_state()?),review,reason,status.get_publish_date(),status.get_next_sequence_number().to_be_bytes().as_slice(),review=="approved" || status.get_publish_date()!=0,id])? == 1,"unknown package");
        }
        Change::Pin(app, id) => {
            db.execute(
                "UPDATE apps SET pinned_package_id=? WHERE app_id=?",
                params![(!id.is_empty()).then_some(id), app],
            )?;
        }
        Change::Deprecations(values) => {
            db.execute("UPDATE apps SET deprecated=0,replacement_app_id=NULL", [])?;
            for (app, value) in values {
                ensure_app(db, &app)?;
                if let Some(id) = &value.replacement_app_id {
                    ensure_app(db, id)?;
                }
                db.execute(
                    "UPDATE apps SET deprecated=1,replacement_app_id=? WHERE app_id=?",
                    params![value.replacement_app_id, app],
                )?;
            }
        }
        Change::Visibility(id, value) => {
            db.execute(
                "UPDATE releases SET public_visibility=? WHERE package_id=?",
                params![serde_json::to_string(&value)?, id],
            )?;
        }
        Change::ImportedAuthor(id, value) => {
            db.execute(
                "INSERT OR REPLACE INTO release_author_profiles VALUES(?,?)",
                params![id, value],
            )?;
        }
    }
    Ok(())
}
pub(crate) fn insert_event(db: &Connection, event: &Event) -> Result<()> {
    ensure_app(db, &event.app_id)?;
    db.execute(
        "INSERT INTO audit_events VALUES(?,?,?,?,?,?,?,?)",
        params![
            event.app_id,
            i64::try_from(event.number)?,
            event.timestamp,
            event.package_id,
            event.version,
            event.action,
            serde_json::to_string(&event.actor)?,
            serde_json::to_string(&event.details)?
        ],
    )?;
    Ok(())
}
fn event_row(app: &str, row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    fn json<T: serde::de::DeserializeOwned>(
        row: &rusqlite::Row<'_>,
        i: usize,
    ) -> rusqlite::Result<T> {
        serde_json::from_str(&row.get::<_, String>(i)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
    }
    Ok(Event {
        number: row.get(0)?,
        timestamp: row.get(1)?,
        app_id: app.into(),
        package_id: row.get(2)?,
        version: row.get(3)?,
        action: row.get(4)?,
        actor: json(row, 5)?,
        details: json(row, 6)?,
    })
}

pub(crate) fn append_events(tx: &Connection, events: Vec<Event>) -> Result<()> {
    for mut event in events {
        ids::parse_app_id(&event.app_id)?;
        ensure_app(tx, &event.app_id)?;
        let number: i64 = tx.query_row(
            "SELECT next_event_number FROM apps WHERE app_id=?",
            [&event.app_id],
            |r| r.get(0),
        )?;
        event.number = number.try_into()?;
        ensure!(
            serde_json::to_vec(&event)?.len() <= 32 * 1024,
            "audit event exceeds 32 KiB; shorten the reason"
        );
        insert_event(tx, &event)?;
        crate::notifications::enqueue(tx, &event)?;
        tx.execute(
            "UPDATE apps SET next_event_number=? WHERE app_id=?",
            params![
                number.checked_add(1).context("audit number overflow")?,
                event.app_id
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Actor;
    #[test]
    fn indexed_pagination_is_stable_across_new_events_and_apps() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("packages"))?;
        let db = Database::open(dir.path())?;
        let app = ids::app_id(&[1; 32]);
        let event = || {
            Event::new(
                &app,
                None,
                "approved",
                Actor::default(),
                serde_json::json!({}),
            )
        };
        db.commit(vec![], (0..10003).map(|_| event()).collect())?;
        let first = db.page(&app, None)?;
        assert_eq!(first.events.len(), 50);
        assert_eq!(first.events[0].number, 10003);
        db.commit(vec![], vec![event()])?;
        let mut numbers: Vec<_> = first.events.iter().map(|e| e.number).collect();
        let mut cursor = first.before;
        while let Some(before) = cursor {
            let page = db.page(&app, Some(before))?;
            numbers.extend(page.events.iter().map(|e| e.number));
            cursor = page.before;
        }
        assert_eq!(numbers, (1..=10003).rev().collect::<Vec<_>>());
        assert!(db.page(&app, Some(0))?.events.is_empty());
        assert!(db.page(&ids::app_id(&[2; 32]), None)?.events.is_empty());
        db.read(|connection| {
            let plan:String=connection.query_row("EXPLAIN QUERY PLAN SELECT * FROM audit_events WHERE app_id=? AND event_number < ? ORDER BY event_number DESC LIMIT 51",params![app,10000],|r|r.get(3))?;
            assert!(plan.contains("SEARCH audit_events USING PRIMARY KEY"),"{plan}");
            Ok(())
        })?;
        Ok(())
    }
    #[test]
    fn failed_audit_insert_rolls_back_state_and_counter() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("packages"))?;
        let db = Database::open(dir.path())?;
        let app = ids::app_id(&[1; 32]);
        let event = Event::new(
            &app,
            None,
            "deprecated",
            Actor::default(),
            serde_json::json!("x".repeat(32768)),
        );
        assert!(
            db.commit(
                vec![Change::Deprecations(BTreeMap::from([(
                    app.clone(),
                    Deprecation {
                        replacement_app_id: None
                    }
                )]))],
                vec![event]
            )
            .is_err()
        );
        assert!(db.deprecations()?.is_empty());
        assert!(db.page(&app, None)?.events.is_empty());
        db.commit(
            vec![],
            vec![Event::new(
                &app,
                None,
                "approved",
                Actor::default(),
                serde_json::json!({}),
            )],
        )?;
        assert_eq!(db.page(&app, None)?.events[0].number, 1);
        Ok(())
    }
}
