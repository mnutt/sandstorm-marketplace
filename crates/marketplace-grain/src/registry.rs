use crate::audit::{Actor, Event, Transaction};
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
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSnapshot {
    pub releases: Vec<ReviewRelease>,
    pub can_approve: bool,
    pub busy: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRelease {
    pub current: bool,
    pub pinned_package_id: String,
    pub public_download: bool,
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
    busy: Arc<AtomicBool>,
    pub(crate) actor: Actor,
    pub(crate) audit_lock: Arc<std::sync::Mutex<()>>,
}
pub(crate) struct Operation(Arc<AtomicBool>);
impl Drop for Operation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Registry {
    pub fn new(root: PathBuf) -> Result<Self> {
        for directory in ["packages", "apps", "keybase", "tmp", "release-pins"] {
            fs::create_dir_all(root.join(directory))?;
        }
        let registry = Self {
            root,
            busy: Arc::new(AtomicBool::new(false)),
            actor: Actor::default(),
            audit_lock: Arc::new(std::sync::Mutex::new(())),
        };
        registry.recover_audit()?;
        registry.audit_baseline()?;
        Ok(registry)
    }
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
    pub(crate) fn begin_operation(&self) -> Result<Operation> {
        ensure!(
            self.busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "another registry operation is in progress; try again when it finishes"
        );
        Ok(Operation(self.busy.clone()))
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
        read_message(&self.package_dir(id)?.join("metadata"))
    }
    pub fn status(&self, id: &str) -> Result<Message> {
        read_message(&self.package_dir(id)?.join("status"))
    }
    pub fn package_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.root.join("packages"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("spk").is_file() {
                let id = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid package filename"))?;
                self.package_dir(&id)?;
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
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
        self.recover_audit()?;
        crate::work::check_cancelled(cancelled)?;
        let started = std::time::Instant::now();
        tracing::info!(bytes = fs::metadata(upload)?.len(), "Verifying SPK");
        let verified = spk::verify_in_cancellable(upload, &self.root.join("tmp"), cancelled)?;
        crate::work::check_cancelled(cancelled)?;
        let info = verified.get_root_as_reader::<p::verified_info::Reader>()?;
        let id = hex::encode(ids::package_bytes(info.get_package_id()?));
        let dest = self.package_dir(&id)?;
        if dest.join("spk").exists() {
            tracing::info!(package_id = %id, "Package already stored");
            return Ok(id);
        }
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
        write_message(&dest.join("metadata"), &verified)?;
        atomic_write(
            &dest.join("received-at"),
            chrono::Utc::now().to_rfc3339().as_bytes(),
        )?;
        let mut status = Builder::new_default();
        status.init_root::<s::submission_status::Builder>();
        write_message(&dest.join("status"), &status)?;
        // SPK existence is the commit marker. Metadata and initial status precede it.
        let mut staged = tempfile::NamedTempFile::new_in(&dest)?;
        std::io::copy(&mut File::open(upload)?, &mut staged)?;
        staged.as_file().sync_all()?;
        let (_, staged_path) = staged.keep()?;
        File::open(&dest)?.sync_all()?;
        self.audit_commit(
            Transaction {
                moves: vec![(staged_path, dest.join("spk"))],
                writes: vec![(dest.join("audit-tracked"), Vec::new())],
                ..Default::default()
            },
            vec![Event::new(
                &ids::app_id(&app),
                Some((&id, info.get_version())),
                "submitted",
                self.actor.clone(),
                json!({"signatureVerified": true}),
            )],
        )?;
        tracing::info!(package_id = %id, version = info.get_version(), elapsed_ms = started.elapsed().as_millis() as u64, "SPK verified and stored");
        Ok(id)
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
        if self.package_dir(&id)?.join("spk").exists() {
            let message = self.metadata(&id)?;
            let info = message.get_root::<p::verified_info::Reader>()?;
            let key = ids::current_key(&ids::app_bytes(info.get_app_id()?))?;
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
        self.recover_audit()?;
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
        self.recover_audit()?;
        self.audit_publication()?;
        ensure!(
            serde_json::to_vec(reason)?.len() <= 8192,
            "review reason exceeds 8 KiB of JSON text"
        );
        tracing::info!("Applying review decision");
        if action == "approve" || action == "rollback" {
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
                    Transaction {
                        writes: vec![(
                            self.root.join("release-pins").join(&app_id),
                            next.as_bytes().to_vec(),
                        )],
                        ..Default::default()
                    },
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
        // Preserve approval history even if an embargoed approval is later revoked.
        if matches!(previous_status.which()?, s::submission_status::Approved(_)) {
            self.remember_approval(id)?;
        }
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
        self.audit_commit(
            Transaction {
                writes: vec![(self.package_dir(id)?.join("status"), bytes)],
                ..Default::default()
            },
            events,
        )?;
        if matches!(next_status.which()?, s::submission_status::Approved(_)) {
            self.remember_approval(id)?;
        }
        Ok(())
    }
    fn remember_approval(&self, id: &str) -> Result<()> {
        let marker = self.package_dir(id)?.join("ever-approved");
        if !marker.exists() {
            atomic_write(&marker, b"")?;
        }
        Ok(())
    }
    // A pin only applies while its release is approved and the author permits publication.
    // Otherwise use the newest eligible release, without overriding author withdrawal.
    fn release_pin(&self, app_id: &str) -> Result<String> {
        match fs::read_to_string(self.root.join("release-pins").join(app_id)) {
            Ok(value) => Ok(value),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err.into()),
        }
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
                || self.package_dir(&id)?.join("ever-approved").is_file();
            releases.push(Release {
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
        match fs::read(self.root.join("deprecations.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// App-level policy, independent of package approval and rollback selection.
    pub fn set_deprecation(&self, app_id: &str, value: Option<Deprecation>) -> Result<()> {
        self.recover_audit()?;
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
                Transaction {
                    writes: vec![(
                        self.root.join("deprecations.json"),
                        serde_json::to_vec(&deprecations)?,
                    )],
                    ..Default::default()
                },
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
        let path = self.root.join("descriptions");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let message = read_message(&path)?;
        message
            .get_root::<a::short_description_overrides::Reader>()?
            .get_items()?
            .iter()
            .map(|item| {
                Ok((
                    item.get_app_id()?.to_str()?.to_owned(),
                    item.get_short_description()?.to_str()?.to_owned(),
                ))
            })
            .collect()
    }
    #[tracing::instrument(skip_all, err(Debug))]
    pub fn publish(&self) -> Result<()> {
        self.recover_audit()?;
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
        let catalog = self.root.join("catalog");
        if catalog.exists() {
            fs::remove_dir_all(&catalog)?;
        }
        for directory in ["apps", "experimental", "images", "packages"] {
            fs::create_dir_all(catalog.join(directory))?;
        }
        let mut approved = BTreeMap::<String, (AppSummary, AppDetailsForMarket)>::new();
        let mut pending = BTreeMap::<String, (AppSummary, AppDetailsForMarket)>::new();
        for release in &releases {
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
            entry.write_assets(&catalog)?;
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
                atomic_write(
                    &catalog.join(directory).join(format!("{}.json", app.app_id)),
                    &serde_json::to_vec(&details)?,
                )?;
                apps.push(app);
            }
            apps.sort_by(|a, b| a.name.cmp(&b.name));
            atomic_write(
                &catalog.join(directory).join("index.json"),
                &serde_json::to_vec(&AppIndexForMarket { apps })?,
            )?;
        }
        marketplace_web::publish::publish(&catalog, &self.root.join("www"))?;
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
            let validation_error = entry.as_ref().err().map(|err| format!("{err:#}"));
            // Older builds may have stored malformed metadata. Keep these releases
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
            let received_at = fs::read_to_string(self.package_dir(&id)?.join("received-at")).ok();
            queue.push(ReviewRelease {
                current,
                pinned_package_id: release.pin.clone(),
                public_download: self.root.join("www/packages").join(&id).is_file(),
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
            let path = self
                .package_dir(&entry.app.package_id)?
                .join("catalog-author.json");
            match fs::read(path) {
                Ok(bytes) => entry.app.author = Some(serde_json::from_slice(&bytes)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(entry)
    }
    fn author_profile(&self, fingerprint: &str) -> Result<Option<AuthorIdentity>> {
        Ok(
            if !fingerprint.is_empty() && self.root.join("keybase").join(fingerprint).exists() {
                ensure!(
                    fingerprint.bytes().all(|c| c.is_ascii_hexdigit()),
                    "invalid PGP fingerprint"
                );
                let profile = read_message(&self.root.join("keybase").join(fingerprint))?;
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
            },
        )
    }
}
pub fn read_message(path: &Path) -> Result<Message> {
    Ok(serialize::read_message(
        File::open(path).with_context(|| format!("opening registry file {}", path.display()))?,
        spk::limits(16 * 1024 * 1024),
    )?)
}
pub fn write_message(path: &Path, message: &Builder<capnp::message::HeapAllocator>) -> Result<()> {
    let mut bytes = Vec::new();
    serialize::write_message(&mut bytes, message)?;
    atomic_write(path, &bytes)
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn audit_status(status: s::submission_status::Reader<'_>) -> Result<serde_json::Value> {
    let (review, reason) = match status.which()? {
        s::submission_status::Approved(_) => ("approved", ""),
        s::submission_status::Pending(()) => ("pending", ""),
        s::submission_status::NeedsUpdate(reason) => ("needsUpdate", reason?.to_str()?),
        _ => ("unknown", ""),
    };
    // Legacy rejection reasons were unbounded; preserve a bounded excerpt plus a
    // digest so changes beyond the excerpt remain distinguishable.
    use sha2::{Digest, Sha256};
    let excerpt: String = if serde_json::to_vec(reason)?.len() <= 8192 {
        reason.to_owned()
    } else {
        reason.chars().take(1024).collect()
    };
    let request_state = match status.get_request_state()? {
        s::SubmissionState::Ignore => "ignore",
        s::SubmissionState::Review => "review",
        s::SubmissionState::Publish => "publish",
    };
    Ok(json!({"requestState": request_state,
        "review": review, "reason": excerpt, "reasonTruncated": excerpt.len() < reason.len(),
        "reasonSha256": hex::encode(Sha256::digest(reason.as_bytes()))}))
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
    fn audit_baseline(&self) -> Result<()> {
        // Per-package markers also distinguish packages imported into an existing empty grain.
        let untracked: Vec<_> = self
            .package_ids()?
            .into_iter()
            .filter(|id| {
                !self
                    .root
                    .join("packages")
                    .join(id)
                    .join("audit-tracked")
                    .exists()
            })
            .collect();
        if untracked.is_empty() {
            return Ok(());
        }
        let public = self.public_states()?;
        let deprecations = self.deprecations()?;
        for id in untracked {
            let marker = self.package_dir(&id)?.join("audit-tracked");
            let visibility = public.get(&id).copied().unwrap_or_default();
            let mut event = self.package_event(&id, "baseline", json!(null))?;
            event.details = json!({
                "note": "Existing release when audit tracking began; historical actors and dates are unknown.",
                "state": audit_status(self.status(&id)?.get_root()?)?,
                "publicVisibility": visibility,
                "rollbackSelection": self.release_pin(&event.app_id)?,
                "deprecation": deprecations.get(&event.app_id)
            });
            self.audit_commit(
                Transaction {
                    writes: vec![
                        (marker, Vec::new()),
                        (
                            self.package_dir(&id)?.join("audit-public.json"),
                            serde_json::to_vec(&visibility)?,
                        ),
                    ],
                    ..Default::default()
                },
                vec![event],
            )?;
        }
        Ok(())
    }
    fn public_states(&self) -> Result<BTreeMap<String, PublicVisibility>> {
        // During an interrupted fallback swap the saved previous generation is
        // still the committed publication; the publisher will restore it.
        let mut www = self.root.join("www");
        if !www.exists() && self.root.join("www.publish-previous").exists() {
            www = self.root.join("www.publish-previous");
        }
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
    fn audit_publication(&self) -> Result<()> {
        // Compare installed output, never desired approval state. Reconcile before
        // a new decision too, preserving the original cause after a failed response.
        for (id, next) in self.public_states()? {
            let path = self.package_dir(&id)?.join("audit-public.json");
            let previous = if path.exists() {
                serde_json::from_slice(&fs::read(&path)?)?
            } else {
                PublicVisibility::default()
            };
            if previous == next {
                continue;
            }
            let mut event = self.package_event(&id, "publicationChanged", json!(null))?;
            let cause_path = self
                .root
                .join("audit")
                .join(&event.app_id)
                .join("cause.json");
            let cause = if cause_path.exists() {
                Some(serde_json::from_slice::<Event>(&fs::read(cause_path)?)?)
            } else {
                None
            };
            event.actor = Actor::default();
            event.details = json!({"previous": previous, "next": next,
                "latestDecision": cause.as_ref().map(|event| event.number),
                "decisionActor": cause.as_ref().map(|event| &event.actor),
                "note": "Visibility observed after publication or during recovery; timestamp is the observation time. Linked decision is the most recent app decision, not proof of sole causation."});
            self.audit_commit(
                Transaction {
                    writes: vec![(path, serde_json::to_vec(&next)?)],
                    ..Default::default()
                },
                vec![event],
            )?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicVisibility {
    approved_listing: bool,
    experimental_listing: bool,
    download: bool,
}
