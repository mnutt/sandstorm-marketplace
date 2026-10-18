mod common;
use anyhow::Result;
use common::*;
use marketplace_grain::{ids, registry::Registry, submit_capnp as s};
use std::{fs, io::Write};

#[test]
fn unreferenced_spk_is_replaced_by_verified_upload() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().into())?;
    let bytes = package(1);
    let mut file = registry.temporary()?;
    file.write_all(&bytes)?;
    let verified = marketplace_grain::spk::verify(file.path())?;
    let info =
        verified.get_root_as_reader::<marketplace_grain::package_capnp::verified_info::Reader>()?;
    let id = hex::encode(ids::package_bytes(info.get_package_id()?));
    fs::create_dir(registry.package_dir(&id)?)?;
    fs::write(
        registry.package_dir(&id)?.join("spk"),
        b"interrupted upload",
    )?;
    assert!(registry.package_ids()?.is_empty());
    assert_eq!(registry.upload(file.path())?, id);
    assert_eq!(fs::read(registry.package_dir(&id)?.join("spk"))?, bytes);
    assert_eq!(
        registry
            .audit_page(&ids::app_id(key().verifying_key().as_bytes()), None)?
            .events[0]
            .action,
        "submitted"
    );
    Ok(())
}

#[test]
fn sqlite_initialization_restart_and_schema_version() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().into())?;
    let id = upload(&registry, 1);
    registry.set_state(&id, s::SubmissionState::Review, u64::MAX - 2)?;
    drop(registry);
    let registry = Registry::new(dir.path().into())?;
    assert_eq!(registry.package_ids()?, vec![id.clone()]);
    assert_eq!(
        registry
            .status(&id)?
            .get_root::<s::submission_status::Reader>()?
            .get_next_sequence_number(),
        u64::MAX - 1
    );
    registry.set_state(&id, s::SubmissionState::Ignore, u64::MAX - 1)?;
    assert_eq!(
        registry
            .status(&id)?
            .get_root::<s::submission_status::Reader>()?
            .get_next_sequence_number(),
        u64::MAX
    );
    assert!(
        registry
            .set_state(&id, s::SubmissionState::Publish, u64::MAX)
            .is_err()
    );
    let app = ids::app_id(key().verifying_key().as_bytes());
    assert_eq!(registry.audit_page(&app, None)?.events.len(), 3);
    let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        3
    );
    db.execute_batch("PRAGMA user_version=4")?;
    assert!(Registry::new(dir.path().into()).is_err());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        4
    );
    Ok(())
}

#[test]
fn signing_rules_migrate_from_version_two_without_resetting_state() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
    db.execute_batch(include_str!("../src/schema.sql"))?;
    db.execute_batch(include_str!("../src/notifications.sql"))?;
    db.execute_batch("PRAGMA user_version=2; UPDATE registry_state SET state_revision=42; UPDATE notification_settings SET recipient='admin@example.org'")?;
    let registry = Registry::new(dir.path().into())?;
    let mut rules = registry.signing_key_rules()?.rules;
    let mut expected = marketplace_grain::replacements::bundled()?.rules;
    rules.sort_by(|a, b| a.original.cmp(&b.original));
    expected.sort_by(|a, b| a.original.cmp(&b.original));
    assert_eq!(rules, expected);
    assert_eq!(
        db.query_row("SELECT state_revision FROM registry_state", [], |r| r
            .get::<_, i64>(0))?,
        42
    );
    assert_eq!(
        db.query_row("SELECT recipient FROM notification_settings", [], |r| {
            r.get::<_, String>(0)
        })?,
        "admin@example.org"
    );
    Ok(())
}
