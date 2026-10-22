use crate::audit::{Actor, Event};
use crate::database::{Change, Database, decode, encode};
use crate::{app_index_capnp as a, ids, package_capnp as p, spk, submit_capnp as s};
use anyhow::{Context, Result, bail, ensure};
use capnp::{
    message::{Builder, Reader},
    serialize, serialize_packed,
};
use ed25519_dalek::{Signature, VerifyingKey};
use marketplace_model::app_index::{
    AppDetailsForMarket, AppIndexForMarket, AppSummary, AuthorIdentity, Deprecation,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSnapshot {
    pub releases: Vec<ReviewRelease>,
    pub can_approve: bool,
    pub busy: bool,
    pub keybase_connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications: Option<crate::notifications::Settings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_keys: Option<crate::replacements::Settings>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRelease {
    pub current: bool,
    pub pinned_package_id: String,
    pub public_download: bool,
    pub public_page: bool,
    pub received_at: Option<String>,
    pub published_at: i64,
    pub app: AppSummary,
    pub details: AppDetailsForMarket,
    pub request_state: &'static str,
    pub review: &'static str,
    pub reason: String,
    pub validation_error: Option<String>,
    pub contact_email: String,
    pub fingerprint: String,
}

pub type Message = Reader<serialize::OwnedSegments>;
// One operation's view of persisted releases. Never retained between requests.
struct Release {
    id: String,
    app_id: String,
    version: u32,
    pin: String,
    ever_approved: bool,
    key_error: Option<String>,
    metadata: Message,
    status: Message,
}
impl Release {
    fn pinned(&self) -> bool {
        self.pin == self.id
    }
}

fn selected_releases(releases: &[Release]) -> Result<BTreeMap<&str, &Release>> {
    let mut selected = BTreeMap::<&str, &Release>::new();
    for release in releases {
        if release.key_error.is_some() {
            continue;
        }
        let status = release.status.get_root::<s::submission_status::Reader>()?;
        if status.get_request_state()? != s::SubmissionState::Publish
            || !matches!(status.which()?, s::submission_status::Approved(_))
        {
            continue;
        }
        if selected
            .get(release.app_id.as_str())
            .is_none_or(|old| release.pinned() || (!old.pinned() && release.version > old.version))
        {
            selected.insert(&release.app_id, release);
        }
    }
    Ok(selected)
}

#[derive(Clone)]
pub struct Registry {
    pub root: PathBuf,
    operation: Arc<tokio::sync::Mutex<()>>,
    pub(crate) notification_busy: Arc<AtomicBool>,
    pub(crate) actor: Actor,
    pub(crate) database: Arc<Database>,
}
pub(crate) struct PreparedUpload {
    rules_revision: i64,
    file: tempfile::NamedTempFile,
    verified: Builder<capnp::message::HeapAllocator>,
}
pub(crate) type Operation = tokio::sync::OwnedMutexGuard<()>;
impl Registry {
    pub fn new(root: PathBuf) -> Result<Self> {
        for directory in ["packages", "tmp"] {
            fs::create_dir_all(root.join(directory))?;
        }
        let database = Arc::new(Database::open(&root)?);
        let registry = Self {
            root,
            operation: Arc::new(tokio::sync::Mutex::new(())),
            notification_busy: Arc::new(AtomicBool::new(false)),
            actor: Actor::default(),
            database,
        };
        Ok(registry)
    }
    pub fn is_busy(&self) -> bool {
        self.operation.try_lock().is_err()
    }
    pub(crate) async fn begin_operation(&self) -> Operation {
        // FIFO, cancellation-safe waiting without blocking the capability RPC loop.
        // The owned guard can stay with a blocking worker until cleanup completes.
        self.operation.clone().lock_owned().await
    }
    pub fn temporary(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::NamedTempFile::new_in(self.root.join("tmp"))?)
    }
    pub fn package_dir(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid package ID"
        );
        Ok(self.root.join("packages").join(id))
    }
    pub fn metadata(&self, id: &str) -> Result<Message> {
        self.database.metadata(id)
    }
    pub fn status(&self, id: &str) -> Result<Message> {
        self.database.status(id)
    }
    pub fn package_ids(&self) -> Result<Vec<String>> {
        self.database.ids()
    }
    pub fn upload(&self, upload: &Path) -> Result<String> {
        self.upload_cancellable(upload, &AtomicBool::new(false))
    }
    #[tracing::instrument(name = "upload", skip_all, err(Debug))]
    pub(crate) fn upload_cancellable(
        &self,
        upload: &Path,
        cancelled: &AtomicBool,
    ) -> Result<String> {
        let prepared = self.prepare_upload(upload, cancelled)?;
        self.commit_upload(prepared, cancelled)
    }
    pub(crate) fn prepare_upload(
        &self,
        upload: &Path,
        cancelled: &AtomicBool,
    ) -> Result<PreparedUpload> {
        crate::work::check_cancelled(cancelled)?;
        // Own the exact bytes that are verified and eventually installed.
        let mut file = self.temporary()?;
        std::io::copy(&mut File::open(upload)?, &mut file)?;
        file.as_file().sync_all()?;
        crate::work::check_cancelled(cancelled)?;
        tracing::info!(bytes = file.as_file().metadata()?.len(), "Verifying SPK");
        let rules = self.signing_key_rules()?;
        let verified =
            spk::verify_with_rules(file.path(), &self.root.join("tmp"), cancelled, &rules)?;
        crate::work::check_cancelled(cancelled)?;
        let info = verified.get_root_as_reader::<p::verified_info::Reader>()?;
        let metadata = info.get_metadata()?;
        ensure!(
            !metadata
                .get_author()
                .get_contact_email()?
                .to_str()?
                .is_empty(),
            "contact email required"
        );
        ensure!(
            !metadata.get_categories()?.is_empty(),
            "at least one category required"
        );
        let short = metadata.get_short_description()?.get_default_text()?;
        ensure!(
            !short.is_empty() && short.len() < 25,
            "short description must contain 1–24 bytes"
        );
        // Exercise the same metadata/image conversion used by publication before
        // committing the package. A valid signature does not imply valid text or enums.
        crate::catalog::parse(info, 0).context("invalid marketplace metadata")?;
        Ok(PreparedUpload {
            file,
            verified,
            rules_revision: rules.revision,
        })
    }
    // Caller holds the registry gate; all state-dependent checks happen here.
    pub(crate) fn commit_upload(
        &self,
        prepared: PreparedUpload,
        cancelled: &AtomicBool,
    ) -> Result<String> {
        crate::work::check_cancelled(cancelled)?;
        let PreparedUpload {
            file,
            verified,
            rules_revision,
        } = prepared;
        ensure!(
            self.signing_key_rules()?.revision == rules_revision,
            "signing-key rules changed during verification; please submit again"
        );
        let info = verified.get_root_as_reader::<p::verified_info::Reader>()?;
        let id = hex::encode(ids::package_bytes(info.get_package_id()?));
        let dest = self.package_dir(&id)?;
        if self.database.contains(&id)? {
            tracing::info!(package_id = %id, "Package already stored");
            return Ok(id);
        }
        let app = ids::app_bytes(info.get_app_id()?);
        // Check all previously published releases, including withdrawn versions.
        for previous in self.package_ids()? {
            let status = self.status(&previous)?;
            if status
                .get_root::<s::submission_status::Reader>()?
                .get_publish_date()
                == 0
            {
                continue;
            }
            let old = self.metadata(&previous)?;
            let old = old.get_root::<p::verified_info::Reader>()?;
            if ids::app_bytes(old.get_app_id()?) == app {
                ensure!(
                    info.get_version() > old.get_version(),
                    "appVersion must exceed the previously published version"
                );
            }
        }
        fs::create_dir_all(&dest)?;
        let received_at = Some(chrono::Utc::now().to_rfc3339());
        let mut status = Builder::new_default();
        status.init_root::<s::submission_status::Builder>();
        // Install the immutable file before the database commit. A crash can leave
        // an unreferenced file, which a verified retry safely replaces.
        file.persist(dest.join("spk"))?;
        File::open(&dest)?.sync_all()?;
        File::open(self.root.join("packages"))?.sync_all()?;
        self.audit_commit(
            vec![Change::Release {
                id: id.clone(),
                metadata: encode(&verified)?,
                status: encode(&status)?,
                received_at,
                ever_approved: false,
            }],
            vec![Event::new(
                &ids::app_id(&app),
                Some((&id, info.get_version())),
                "submitted",
                self.actor.clone(),
                json!({"signatureVerified": true}),
            )],
        )?;
        tracing::info!(package_id = %id, version = info.get_version(), "SPK verified and stored");
        Ok(id)
    }
    // An approver can request publication without possessing the author's key.
    // Preserve review decisions and signed-request sequence numbers on imports.
    pub(crate) fn request_import_publication(&self, id: &str) -> Result<()> {
        self.audit_publication()?;
        ensure!(
            self.key_error(id)?.is_none(),
            "package is blocked by signing-key rules"
        );
        self.mutate_audited(id, "importPublicationRequested", |mut status| {
            status.set_request_state(s::SubmissionState::Publish);
            Ok(())
        })?;
        self.publish()
    }
    pub fn submission(&self, body: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            body.len() >= 64 && body.len() <= 65536,
            "invalid submission request size"
        );
        let (signed, signature) = body.split_at(body.len() - 64);
        let mut input = signed;
        let request = serialize_packed::read_message(&mut input, spk::limits(4096))?;
        ensure!(input.is_empty(), "trailing request bytes");
        let request = request.get_root::<s::submission_request::Reader>()?;
        let id = hex::encode(ids::package_bytes(request.get_package_id()?));
        let mut response = Builder::new_default();
        if self.database.contains(&id)? {
            let message = self.metadata(&id)?;
            let info = message.get_root::<p::verified_info::Reader>()?;
            ensure!(
                self.key_error(&id)?.is_none(),
                "package is blocked by signing-key rules"
            );
            let key = self
                .signing_key_rules()?
                .current_key(&ids::app_bytes(info.get_app_id()?))?;
            VerifyingKey::from_bytes(&key)?
                .verify_strict(signed, &Signature::from_slice(signature)?)?;
            if let s::submission_request::SetState(change) = request.which()? {
                let mut actor = self.actor.clone();
                actor.kind = "appSigningKey".into();
                actor.signing_key = Some(hex::encode(key));
                self.with_actor(actor).set_state(
                    &id,
                    change.get_new_state()?,
                    change.get_sequence_number(),
                )?;
                // Also retries a failed publication on an identical authenticated request.
                self.publish()?;
            }
            response.set_root(
                self.status(&id)?
                    .get_root::<s::submission_status::Reader>()?,
            )?;
        } else {
            response
                .init_root::<s::submission_status::Builder>()
                .set_not_uploaded(());
        }
        let mut output = vec![0];
        serialize_packed::write_message(&mut output, &response)?;
        Ok(output)
    }
    pub fn set_state(&self, id: &str, state: s::SubmissionState, sequence: u64) -> Result<()> {
        self.audit_publication()?;
        tracing::info!(
            package_id = id,
            ?state,
            sequence,
            "Applying publication request"
        );
        self.mutate_audited(id, "authorStateChanged", |mut status| {
            // Permit exact retries, but don't let a later no-op request leave an
            // earlier change replayable after it.
            if sequence < status.reborrow().get_next_sequence_number() {
                ensure!(
                    status.reborrow().get_request_state()? == state
                        && sequence.checked_add(1)
                            == Some(status.reborrow().get_next_sequence_number()),
                    "stale submission sequence number"
                );
                return Ok(());
            }
            status.set_request_state(state);
            status.set_next_sequence_number(sequence.checked_add(1).context("sequence overflow")?);
            Ok(())
        })
    }
    #[tracing::instrument(skip_all, fields(package_id = id, action = action), err(Debug))]
    pub fn review(&self, id: &str, action: &str, reason: &str) -> Result<()> {
        self.audit_publication()?;
        ensure!(
            serde_json::to_vec(reason)?.len() <= 8192,
            "review reason exceeds 8 KiB of JSON text"
        );
        tracing::info!("Applying review decision");
        if action == "approve" || action == "rollback" {
            ensure!(
                self.key_error(id)?.is_none(),
                "package is blocked by signing-key rules"
            );
            let metadata = self.metadata(id)?;
            crate::catalog::parse(metadata.get_root()?, 0)
                .context("invalid marketplace metadata")?;
        }
        if action == "rollback" || action == "latest" {
            let metadata = self.metadata(id)?;
            let info = metadata.get_root::<p::verified_info::Reader>()?;
            let app_id = ids::app_id(&ids::app_bytes(info.get_app_id()?));
            if action == "rollback" {
                let status = self.status(id)?;
                let status = status.get_root::<s::submission_status::Reader>()?;
                ensure!(
                    status.get_request_state()? == s::SubmissionState::Publish
                        && matches!(status.which()?, s::submission_status::Approved(_)),
                    "only approved releases requested for publication can be selected"
                );
                let releases = self.releases()?;
                let selected = selected_releases(&releases)?;
                let current = selected
                    .get(app_id.as_str())
                    .context("app has no published release")?;
                ensure!(
                    current.pin == id || info.get_version() < current.version,
                    "rollback must select an older release than the current public version"
                );
            }
            let previous = self.release_pin(&app_id)?;
            let next = if action == "rollback" { id } else { "" };
            if previous != next {
                self.audit_commit(
                    vec![Change::Pin(app_id.clone(), next.into())],
                    vec![Event::new(
                        &app_id,
                        Some((id, info.get_version())),
                        if action == "rollback" {
                            "rolledBack"
                        } else {
                            "rollbackCleared"
                        },
                        self.actor.clone(),
                        json!({"previous": previous, "next": next}),
                    )],
                )?;
            }
            return self.publish();
        }
        ensure!(
            action != "reject" || !reason.trim().is_empty(),
            "a rejection reason is required"
        );
        self.mutate_audited(
            id,
            match action {
                "approve" => "approved",
                "reject" => "rejected",
                "unapprove" => "approvalRevoked",
                _ => action,
            },
            |mut status| {
                match action {
                    "approve" => status.set_approved(""),
                    "unapprove" => status.set_pending(()),
                    "reject" => status.set_needs_update(reason),
                    _ => bail!("unknown review action"),
                }
                Ok(())
            },
        )?;
        self.publish()
    }
    pub(crate) fn mutate(
        &self,
        id: &str,
        change: impl FnOnce(s::submission_status::Builder<'_>) -> Result<()>,
    ) -> Result<()> {
        self.mutate_audited(id, "importedState", change)
    }
    fn mutate_audited(
        &self,
        id: &str,
        action: &str,
        change: impl FnOnce(s::submission_status::Builder<'_>) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            self.package_dir(id)?.join("spk").exists(),
            "package not uploaded"
        );
        let old = self.status(id)?;
        let previous_status = old.get_root::<s::submission_status::Reader>()?;
        let mut message = Builder::new_default();
        message.set_root(previous_status)?;
        {
            let mut status = message.get_root::<s::submission_status::Builder>()?;
            change(status.reborrow())?;
            if status.reborrow().get_publish_date() == 0
                && status.reborrow().get_request_state()? == s::SubmissionState::Publish
                && matches!(
                    status.reborrow().which()?,
                    s::submission_status::Approved(_)
                )
            {
                status.set_publish_date(chrono::Utc::now().timestamp());
            }
        }
        let next_status = message.get_root_as_reader::<s::submission_status::Reader>()?;
        let previous = audit_status(previous_status)?;
        let next = audit_status(next_status)?;
        let sequence_advanced =
            previous_status.get_next_sequence_number() != next_status.get_next_sequence_number();
        let events = if previous != next {
            vec![self.package_event(id, action, json!({"previous": previous, "next": next}))?]
        } else if action == "authorStateChanged" && sequence_advanced {
            // Fresh author intent is audited; exact retries leave the sequence alone.
            vec![self.package_event(
                id,
                "resubmitted",
                json!({
                    "previous": previous, "next": next,
                    "sequenceNumber": (next_status.get_next_sequence_number() - 1).to_string(),
                    "note": "Fresh author request; submission state unchanged."
                }),
            )?]
        } else {
            Vec::new()
        };
        let mut bytes = Vec::new();
        serialize::write_message(&mut bytes, &message)?;
        self.audit_commit(vec![Change::Status(id.into(), bytes)], events)?;
        Ok(())
    }
    // A pin only applies while its release is approved and the author permits publication.
    // Otherwise use the newest eligible release, without overriding author withdrawal.
    fn release_pin(&self, app_id: &str) -> Result<String> {
        self.database.read(|db| {
            Ok(db
                .query_row(
                    "SELECT pinned_package_id FROM apps WHERE app_id=?",
                    [app_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or_default())
        })
    }
    fn releases(&self) -> Result<Vec<Release>> {
        let mut pins = BTreeMap::new();
        let mut releases = Vec::new();
        for id in self.package_ids()? {
            let metadata = self.metadata(&id)?;
            let info = metadata.get_root::<p::verified_info::Reader>()?;
            let app_id = ids::app_id(&ids::app_bytes(info.get_app_id()?));
            let pin = match pins.entry(app_id.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(self.release_pin(&app_id)?)
                }
            };
            let status = self.status(&id)?;
            let state = status.get_root::<s::submission_status::Reader>()?;
            let ever_approved = state.get_publish_date() != 0
                || matches!(state.which()?, s::submission_status::Approved(_))
                || self.database.read(|db| {
                    Ok(db.query_row(
                        "SELECT ever_approved FROM releases WHERE package_id=?",
                        [&id],
                        |r| r.get::<_, bool>(0),
                    )?)
                })?;
            releases.push(Release {
                key_error: self.key_error(&id)?,
                status,
                ever_approved,
                version: info.get_version(),
                id,
                app_id,
                pin: pin.clone(),
                metadata,
            });
        }
        Ok(releases)
    }

    fn deprecations(&self) -> Result<BTreeMap<String, Deprecation>> {
        self.database.deprecations()
    }

    /// App-level policy, independent of package approval and rollback selection.
    pub fn set_deprecation(&self, app_id: &str, value: Option<Deprecation>) -> Result<()> {
        self.audit_publication()?;
        ids::parse_app_id(app_id)?;
        let releases = self.releases()?;
        ensure!(releases.iter().any(|r| r.app_id == app_id), "unknown app");
        let mut deprecations = self.deprecations()?;
        let previous = serde_json::to_value(deprecations.get(app_id))?;
        if let Some(value) = value {
            if let Some(replacement) = &value.replacement_app_id {
                ids::parse_app_id(replacement)?;
                ensure!(replacement != app_id, "an app cannot replace itself");
                ensure!(
                    selected_releases(&releases)?.contains_key(replacement.as_str()),
                    "replacement must have an approved public release"
                );
                // Follow the existing chain to prevent circular replacement links.
                let mut next = Some(replacement.as_str());
                let mut seen = std::collections::BTreeSet::from([app_id]);
                while let Some(id) = next {
                    ensure!(seen.insert(id), "replacement would create a cycle");
                    next = deprecations
                        .get(id)
                        .and_then(|d| d.replacement_app_id.as_deref());
                }
            }
            deprecations.insert(app_id.to_owned(), value);
        } else {
            deprecations.remove(app_id);
        }
        let next = serde_json::to_value(deprecations.get(app_id))?;
        if previous != next {
            self.audit_commit(
                vec![Change::Deprecations(deprecations)],
                vec![Event::new(
                    app_id,
                    None,
                    if next.is_null() {
                        "restored"
                    } else {
                        "deprecated"
                    },
                    self.actor.clone(),
                    json!({"previous": previous, "next": next}),
                )],
            )?;
        }
        self.publish()
    }

    fn description_overrides(&self) -> Result<BTreeMap<String, String>> {
        self.database.read(|db| Ok(db.prepare("SELECT app_id,description_override FROM apps WHERE description_override IS NOT NULL")?.query_map([], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?))
    }
    #[tracing::instrument(skip_all, err(Debug))]
    pub fn publish(&self) -> Result<()> {
        self.database
            .read(|db| crate::replacements::remove_blocked_downloads(db, &self.root))?;
        self.audit_publication()?;
        let started = std::time::Instant::now();
        let releases = self.releases()?;
        let selected = selected_releases(&releases)?;
        let previously_approved: std::collections::BTreeSet<_> = releases
            .iter()
            .filter(|release| release.ever_approved)
            .map(|release| release.app_id.as_str())
            .collect();
        let mut newest_approved = BTreeMap::<&str, u32>::new();
        for release in &releases {
            if release.key_error.is_some() {
                continue;
            }
            let status = release.status.get_root::<s::submission_status::Reader>()?;
            if status.get_request_state()? == s::SubmissionState::Publish
                && matches!(status.which()?, s::submission_status::Approved(_))
            {
                let version = newest_approved.entry(&release.app_id).or_default();
                *version = (*version).max(release.version);
            }
        }
        let overrides = self.description_overrides()?;
        let deprecations = self.deprecations()?;
        tracing::info!(packages = releases.len(), "Rebuilding public catalog");
        // Rebuildable scratch data: the publisher flushes the completed public
        // tree before installing it. No intermediate per-file barriers are needed.
        let catalog = self.root.join("catalog");
        let previous_publication = self.installed_directory();
        if catalog.exists() {
            fs::remove_dir_all(&catalog)?;
        }
        for directory in ["apps", "experimental", "images", "packages"] {
            fs::create_dir_all(catalog.join(directory))?;
        }
        let mut approved = BTreeMap::<String, (AppSummary, AppDetailsForMarket)>::new();
        let mut pending = BTreeMap::<String, (AppSummary, AppDetailsForMarket)>::new();
        for release in &releases {
            if release.key_error.is_some() {
                continue;
            }
            let id = &release.id;
            let status = release.status.get_root::<s::submission_status::Reader>()?;
            if status.reborrow().get_request_state()? != s::SubmissionState::Publish {
                continue;
            }
            let entries = match status.reborrow().which()? {
                s::submission_status::Approved(_) => &mut approved,
                s::submission_status::Pending(()) => {
                    // First submissions stay entirely private. Pending updates must
                    // also be newer than approved public releases (including pins).
                    if !previously_approved.contains(release.app_id.as_str())
                        || newest_approved
                            .get(release.app_id.as_str())
                            .is_some_and(|version| release.version <= *version)
                    {
                        continue;
                    }
                    &mut pending
                }
                _ => continue,
            };
            let entry =
                self.catalog_entry(release.metadata.get_root()?, status.get_publish_date())?;
            entry.write_assets(&catalog, &previous_publication)?;
            let (mut app, details) = (entry.app, entry.details);
            app.deprecation = deprecations.get(&app.app_id).cloned();
            fs::hard_link(
                self.package_dir(&id)?.join("spk"),
                catalog.join("packages").join(&id),
            )?;
            if matches!(status.which()?, s::submission_status::Approved(_))
                && selected.get(app.app_id.as_str()).map(|release| &release.id) != Some(id)
            {
                continue;
            }
            if entries
                .get(&app.app_id)
                .is_none_or(|(old, _)| old.version_number < app.version_number)
            {
                entries.insert(app.app_id.clone(), (app, details));
            }
        }
        for (directory, entries) in [("apps", approved), ("experimental", pending)] {
            let mut apps = Vec::new();
            for (_, (mut app, details)) in entries {
                if let Some(description) = overrides.get(&app.app_id) {
                    app.short_description = description.clone();
                }
                fs::write(
                    &catalog.join(directory).join(format!("{}.json", app.app_id)),
                    &serde_json::to_vec(&details)?,
                )?;
                apps.push(app);
            }
            apps.sort_by(|a, b| a.name.cmp(&b.name));
            fs::write(
                &catalog.join(directory).join("index.json"),
                &serde_json::to_vec(&AppIndexForMarket { apps })?,
            )?;
        }
        let generation = self.prepare_publication(&catalog)?;
        if let Err(error) = marketplace_web::publish::publish_generation(
            &catalog,
            &self.root.join("www"),
            generation,
        ) {
            self.database.transaction(|db| {
                db.execute(
                    "UPDATE publication_generations SET last_error=? WHERE generation_id=?",
                    rusqlite::params![format!("{error:#}"), generation],
                )?;
                Ok(())
            })?;
            return Err(error);
        }
        self.audit_publication()?;
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Public site published"
        );
        Ok(())
    }
    pub fn preview(&self, id: &str) -> Result<String> {
        ensure!(
            self.package_dir(id)?.join("spk").is_file(),
            "package not uploaded"
        );
        let metadata = self.metadata(id)?;
        let status = self.status(id)?;
        let published = status
            .get_root::<s::submission_status::Reader>()?
            .get_publish_date();
        let mut entry = self.catalog_entry(metadata.get_root()?, published)?;
        entry.app.deprecation = self.deprecations()?.remove(&entry.app.app_id);
        if let Some(description) = self.description_overrides()?.get(&entry.app.app_id) {
            entry.app.short_description = description.clone();
        }
        if published == 0 {
            entry.app.created_at.clear();
        }
        marketplace_web::render_preview(
            &entry.app,
            &entry.details,
            &entry.image_data_urls(),
            self.root.join("www/packages").join(id).is_file(),
        )
    }

    pub fn queue(&self) -> Result<Vec<ReviewRelease>> {
        let mut queue = Vec::new();
        let deprecations = self.deprecations()?;
        let releases = self.releases()?;
        let selected = selected_releases(&releases)?;
        for release in &releases {
            let id = &release.id;
            let info = release.metadata.get_root::<p::verified_info::Reader>()?;
            let status = release.status.get_root::<s::submission_status::Reader>()?;
            let app_id = release.app_id.clone();
            let entry = self
                .catalog_entry(info, status.get_publish_date())
                .map(|entry| (entry.app, entry.details));
            let validation_error = release
                .key_error
                .clone()
                .or_else(|| entry.as_ref().err().map(|err| format!("{err:#}")));
            // Keep releases with malformed stored metadata
            // visible so an approver can reject them and restore publication.
            let (mut app, details) = entry.unwrap_or_else(|_| {
                (
                    AppSummary {
                        deprecation: None,
                        app_id,
                        package_id: id.clone(),
                        name: format!("Invalid package {}", &id[..8]),
                        version: String::new(),
                        version_number: info.get_version(),
                        image_id: String::new(),
                        web_link: String::new(),
                        code_link: String::new(),
                        is_open_source: false,
                        categories: Vec::new(),
                        author: None,
                        upstream_author: String::new(),
                        short_description: "Invalid metadata".into(),
                        created_at: String::new(),
                    },
                    AppDetailsForMarket {
                        description: String::new(),
                        change_log: String::new(),
                        screenshots: Vec::new(),
                        license: String::new(),
                    },
                )
            });
            app.deprecation = deprecations.get(&app.app_id).cloned();
            let (review, reason) = match status.reborrow().which()? {
                s::submission_status::Approved(_) => ("approved", ""),
                s::submission_status::NeedsUpdate(reason) => ("needsUpdate", reason?.to_str()?),
                _ => ("pending", ""),
            };
            let current = selected.get(app.app_id.as_str()).map(|release| &release.id) == Some(id);
            let received_at = self.database.read(|db| {
                Ok(db.query_row(
                    "SELECT received_at FROM releases WHERE package_id=?",
                    [id],
                    |r| r.get(0),
                )?)
            })?;
            queue.push(ReviewRelease {
                current,
                pinned_package_id: release.pin.clone(),
                public_download: self.root.join("www/packages").join(&id).is_file(),
                // Experimental-only pages also have HTML, but no approved app details.
                public_page: self
                    .installed_directory()
                    .join("apps")
                    .join(format!("{}.json", app.app_id))
                    .is_file()
                    && self
                        .installed_directory()
                        .join("app")
                        .join(&app.app_id)
                        .join("index.html")
                        .is_file(),
                received_at,
                published_at: status.get_publish_date(),
                app,
                details,
                request_state: match status.get_request_state()? {
                    s::SubmissionState::Publish => "publish",
                    s::SubmissionState::Review => "review",
                    s::SubmissionState::Ignore => "ignore",
                },
                review,
                reason: reason.to_owned(),
                validation_error,
                contact_email: info
                    .get_metadata()?
                    .get_author()
                    .get_contact_email()
                    .ok()
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_owned(),
                fingerprint: info
                    .get_author_pgp_key_fingerprint()
                    .ok()
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_owned(),
            });
        }
        Ok(queue)
    }
    fn catalog_entry<'a>(
        &self,
        info: p::verified_info::Reader<'a>,
        published: i64,
    ) -> Result<crate::catalog::Entry<'a>> {
        let mut entry = crate::catalog::parse(info, published)?;
        entry.app.author = self.author_profile(entry.fingerprint)?;
        if entry.app.author.is_none() {
            // Imported profiles describe this exact package, not a verified Keybase
            // identity. Keep them separate so a live lookup always takes precedence.
            if let Some(profile) = self.database.imported_author(&entry.app.package_id)? {
                entry.app.author = Some(serde_json::from_str(&profile)?);
            }
        }
        Ok(entry)
    }
    fn author_profile(&self, fingerprint: &str) -> Result<Option<AuthorIdentity>> {
        Ok(if let Some(bytes) = self.database.profile(fingerprint)? {
            ensure!(
                fingerprint.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid PGP fingerprint"
            );
            let profile = decode(&bytes)?;
            let p = profile.get_root::<a::keybase_identity::Reader>()?;
            let first = |list: capnp::text_list::Reader<'_>| -> Result<String> {
                Ok(if list.is_empty() {
                    String::new()
                } else {
                    list.get(0)?.to_str()?.to_string()
                })
            };
            Some(AuthorIdentity {
                name: p.get_name()?.to_str()?.to_string(),
                keybase_username: p.get_keybase_handle()?.to_str()?.to_string(),
                picture: p.get_picture()?.to_str()?.to_string(),
                github_username: first(p.get_github_handles()?)?,
                twitter_username: first(p.get_twitter_handles()?)?,
                hackernews_username: first(p.get_hackernews_handles()?)?,
                reddit_username: first(p.get_reddit_handles()?)?,
            })
        } else {
            None
        })
    }
}
pub(crate) fn audit_status(status: s::submission_status::Reader<'_>) -> Result<serde_json::Value> {
    let (review, reason) = match status.which()? {
        s::submission_status::Approved(_) => ("approved", ""),
        s::submission_status::Pending(()) => ("pending", ""),
        s::submission_status::NeedsUpdate(reason) => ("needsUpdate", reason?.to_str()?),
        _ => ("unknown", ""),
    };
    let request_state = match status.get_request_state()? {
        s::SubmissionState::Ignore => "ignore",
        s::SubmissionState::Review => "review",
        s::SubmissionState::Publish => "publish",
    };
    Ok(json!({"requestState": request_state,
        "review": review, "reason": reason}))
}

impl Registry {
    pub(crate) fn package_event(
        &self,
        id: &str,
        action: &str,
        details: serde_json::Value,
    ) -> Result<Event> {
        let metadata = self.metadata(id)?;
        let info = metadata.get_root::<p::verified_info::Reader>()?;
        Ok(Event::new(
            &ids::app_id(&ids::app_bytes(info.get_app_id()?)),
            Some((id, info.get_version())),
            action,
            self.actor.clone(),
            details,
        ))
    }
    fn installed_directory(&self) -> PathBuf {
        let www = self.root.join("www");
        if !www.exists() && self.root.join("www.publish-previous").exists() {
            self.root.join("www.publish-previous")
        } else {
            www
        }
    }
    fn public_states(&self, www: &Path) -> Result<BTreeMap<String, PublicVisibility>> {
        let mut states = BTreeMap::new();
        for id in self.package_ids()? {
            let visibility = PublicVisibility {
                download: www.join("packages").join(&id).is_file(),
                ..Default::default()
            };
            states.insert(id, visibility);
        }
        for (name, experimental) in [("apps", false), ("experimental", true)] {
            let path = www.join(name).join("index.json");
            if path.exists() {
                let index: AppIndexForMarket = serde_json::from_slice(&fs::read(path)?)?;
                for app in index.apps {
                    if app.deprecation.is_some() {
                        continue;
                    }
                    if let Some(visibility) = states.get_mut(&app.package_id) {
                        if experimental {
                            visibility.experimental_listing = true;
                        } else {
                            visibility.approved_listing = true;
                        }
                    }
                }
            }
        }
        Ok(states)
    }
    fn prepare_publication(&self, catalog: &Path) -> Result<i64> {
        let items = self.public_states(catalog)?;
        self.database.transaction(|db| {
            db.execute("UPDATE publication_generations SET status='failed' WHERE status='prepared'", [])?;
            db.execute("INSERT INTO publication_generations(source_revision,status,created_at) SELECT state_revision,'prepared',? FROM registry_state", [chrono::Utc::now().to_rfc3339()])?;
            let generation = db.last_insert_rowid();
            for (id,visibility) in items {
                db.execute("INSERT INTO publication_items VALUES(?,?,?)", rusqlite::params![generation,id,serde_json::to_string(&visibility)?])?;
            }
            db.execute("INSERT INTO publication_causes
                SELECT ?,a.app_id,e.event_number FROM apps a JOIN audit_events e
                ON e.app_id=a.app_id AND e.event_number=(SELECT event_number FROM audit_events
                    WHERE app_id=a.app_id AND action NOT IN ('publicationChanged','submitted','resubmitted')
                    ORDER BY event_number DESC LIMIT 1)", [generation])?;
            // Retain generation summaries and audit records, only current/pending
            // generations need their full package and cause snapshots.
            db.execute("DELETE FROM publication_items WHERE generation_id != ? AND generation_id != COALESCE((SELECT installed_generation_id FROM registry_state),0)", [generation])?;
            db.execute("DELETE FROM publication_causes WHERE generation_id != ? AND generation_id != COALESCE((SELECT installed_generation_id FROM registry_state),0)", [generation])?;
            Ok(generation)
        })
    }
    fn audit_publication(&self) -> Result<()> {
        let www = self.installed_directory();
        if !www.exists() {
            return Ok(());
        }
        let generation = fs::read_to_string(www.join("generation.txt"))?
            .parse::<i64>()
            .context("invalid installed generation")?;
        let observed = self.public_states(&www)?;
        let mut changes = Vec::new();
        let mut events = Vec::new();
        for (id, next) in observed {
            let previous = self.database.visibility(&id)?;
            if previous == next {
                continue;
            }
            let mut event = self.package_event(&id, "publicationChanged", json!(null))?;
            let cause = self.database.publication_cause(generation, &event.app_id)?;
            event.actor = Actor::default();
            event.details = json!({"previous": previous, "next": next,
                "generation": generation,
                "latestDecision": cause.as_ref().map(|event| event.number),
                "decisionActor": cause.as_ref().map(|event| &event.actor),
                "note": "Visibility observed after publication or during recovery; timestamp is the observation time. Linked decision is publication context, not proof of sole causation."});
            changes.push(Change::Visibility(id, next));
            events.push(event);
        }
        self.database.transaction(|db| {
            for change in changes {
                crate::database::apply(db, change)?;
            }
            crate::database::append_events(db, events)?;
            ensure!(db.execute("UPDATE publication_generations SET status='installed',installed_at=COALESCE(installed_at,?),last_error=NULL WHERE generation_id=?", rusqlite::params![chrono::Utc::now().to_rfc3339(),generation])? == 1,"unknown installed generation");
            db.execute("UPDATE publication_generations SET status='superseded' WHERE status='installed' AND generation_id != ?",[generation])?;
            db.execute("UPDATE registry_state SET installed_generation_id=?",[generation])?;
            Ok(())
        })
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicVisibility {
    pub(crate) approved_listing: bool,
    pub(crate) experimental_listing: bool,
    pub(crate) download: bool,
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use std::io::Write;

    fn prepared(registry: &Registry, version: u32) -> Result<PreparedUpload> {
        let mut verified = Builder::new_default();
        let mut info = verified.init_root::<p::verified_info::Builder>();
        ids::set_app(info.reborrow().init_app_id(), &[42; 32]);
        ids::set_package(info.reborrow().init_package_id(), &[version as u8; 16]);
        info.set_version(version);
        let mut file = registry.temporary()?;
        file.write_all(b"already verified package")?;
        Ok(PreparedUpload {
            file,
            verified,
            rules_revision: registry.signing_key_rules()?.revision,
        })
    }

    #[test]
    fn upload_cannot_commit_after_rules_change() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().to_owned())?;
        let upload = prepared(&registry, 1)?;
        registry.database.transaction(|db| {
            db.execute(
                "UPDATE registry_state SET key_rules_revision=key_rules_revision+1",
                [],
            )?;
            Ok(())
        })?;
        assert!(
            registry
                .commit_upload(upload, &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("rules changed")
        );
        assert!(registry.package_ids()?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn upload_commit_rechecks_duplicates_versions_and_cancellation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().to_owned())?;
        let cancelled = AtomicBool::new(false);
        let older = prepared(&registry, 1)?;
        let newer = prepared(&registry, 2)?;
        let duplicate = prepared(&registry, 2)?;
        let _operation = registry.begin_operation().await;
        let id = registry.commit_upload(newer, &cancelled)?;
        registry.mutate(&id, |mut status| {
            status.set_publish_date(1);
            Ok(())
        })?;
        assert_eq!(registry.commit_upload(duplicate, &cancelled)?, id);
        assert!(
            registry
                .commit_upload(older, &cancelled)
                .unwrap_err()
                .to_string()
                .contains("appVersion must exceed")
        );
        let next = prepared(&registry, 3)?;
        assert!(
            registry
                .commit_upload(next, &AtomicBool::new(true))
                .is_err()
        );
        assert_eq!(registry.package_ids()?, vec![id]);
        let events = registry.audit_page(&ids::app_id(&[42; 32]), None)?;
        assert_eq!(
            events
                .events
                .iter()
                .filter(|event| event.action == "submitted")
                .count(),
            1
        );
        Ok(())
    }
}
