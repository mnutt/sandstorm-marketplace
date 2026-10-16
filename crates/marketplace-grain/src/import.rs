//! Empty-grain bootstrap from a public catalog. Never extracts arbitrary tar paths.
use crate::{
    ids, package_capnp as p,
    registry::{Registry, atomic_write},
    spk, submit_capnp as s,
};
use anyhow::{Context, Result, ensure};
use marketplace_model::app_index::AppIndexForMarket;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::Path,
};

pub const IMPORT_LIMIT: u64 = 16 * 1024 * 1024 * 1024;
impl Registry {
    pub fn import_catalog(&self, path: &Path) -> Result<()> {
        self.import_catalog_cancellable(path, &std::sync::atomic::AtomicBool::new(false))
    }
    #[tracing::instrument(name = "import_catalog", skip_all, err(Debug))]
    pub(crate) fn import_catalog_cancellable(
        &self,
        path: &Path,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        tracing::info!(bytes = fs::metadata(path)?.len(), "Starting catalog import");
        ensure!(
            self.package_ids()?.is_empty(),
            "import is only available in an empty grain"
        );
        ensure!(
            fs::metadata(path)?.len() <= IMPORT_LIMIT,
            "import exceeds 16 GiB"
        );
        let work = tempfile::tempdir_in(self.root.join("tmp"))?;
        let mut approved = None;
        let mut experimental = None;
        let mut packages = BTreeMap::new();
        let mut names = BTreeSet::new();
        let mut total = 0u64;
        let mut archive = tar::Archive::new(File::open(path)?);
        for entry in archive.entries()? {
            crate::work::check_cancelled(cancelled)?;
            let mut entry = entry?;
            ensure!(
                entry.header().entry_type().is_file(),
                "import only accepts regular files"
            );
            let name = entry
                .path()?
                .to_str()
                .context("invalid archive filename")?
                .to_owned();
            ensure!(
                names.len() < 10002 && names.insert(name.clone()),
                "duplicate or excessive archive entries"
            );
            let size = entry.size();
            total = total.checked_add(size).context("archive size overflow")?;
            ensure!(total <= IMPORT_LIMIT, "import exceeds 16 GiB");
            if name == "apps/index.json" || name == "experimental/index.json" {
                ensure!(size <= 16 * 1024 * 1024, "catalog index too large");
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                let index: AppIndexForMarket = serde_json::from_slice(&bytes)?;
                if name == "apps/index.json" {
                    approved = Some(index);
                } else {
                    experimental = Some(index);
                }
            } else if let Some(id) = name.strip_prefix("packages/") {
                ensure!(
                    id.len() == 32
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "invalid package filename"
                );
                ensure!(size <= spk::SIZE_LIMIT, "SPK exceeds 1 GiB");
                let path = work.path().join(id);
                tracing::info!(
                    package_id = id,
                    bytes = size,
                    "Reading SPK from import archive"
                );
                std::io::copy(&mut entry, &mut File::create(&path)?)?;
                packages.insert(id.to_owned(), path);
            } else {
                anyhow::bail!("unexpected archive entry: {name}");
            }
        }
        let approved = approved.context("missing apps/index.json")?;
        let experimental = experimental.unwrap_or(AppIndexForMarket { apps: Vec::new() });
        let staged = Registry::new(work.path().join("registry"))?;
        tracing::info!(
            approved = approved.apps.len(),
            experimental = experimental.apps.len(),
            "Archive read; verifying catalog packages"
        );
        let mut listed = BTreeSet::new();
        // Upload everything before assigning published states: catalog order must not
        // make importing an older approved release alongside a newer pending one fail.
        let mut states = Vec::new();
        for (index, is_approved) in [(approved, true), (experimental, false)] {
            let mut apps = BTreeSet::new();
            for app in index.apps {
                ensure!(apps.insert(app.app_id.clone()), "duplicate app in catalog");
                ensure!(
                    listed.insert(app.package_id.clone()),
                    "package listed more than once"
                );
                let file = packages
                    .get(&app.package_id)
                    .context("catalog references a missing SPK")?;
                tracing::info!(package_id = %app.package_id, current = listed.len(), total = packages.len(), "Verifying import package");
                let id = staged.upload_cancellable(file, cancelled)?;
                ensure!(id == app.package_id, "SPK hash does not match catalog");
                let message = staged.metadata(&id)?;
                let info = message.get_root::<p::verified_info::Reader>()?;
                ensure!(
                    ids::app_id(&ids::app_bytes(info.get_app_id()?)) == app.app_id
                        && info.get_version() == app.version_number,
                    "SPK identity/version does not match catalog"
                );
                if let Some(author) = app.author {
                    atomic_write(
                        &staged.package_dir(&id)?.join("catalog-author.json"),
                        &serde_json::to_vec(&author)?,
                    )?;
                }
                states.push((id, is_approved));
            }
        }
        ensure!(!listed.is_empty(), "catalog contains no packages");
        ensure!(
            listed.len() == packages.len(),
            "archive contains unlisted packages"
        );
        for (id, approved) in states {
            staged.mutate(&id, |mut status| {
                status.set_request_state(s::SubmissionState::Publish);
                if approved {
                    status.set_approved("");
                } else {
                    status.set_pending(());
                }
                Ok(())
            })?;
        }
        // Validate all rendering before making any package visible in the live registry.
        tracing::info!("Validating publication in temporary storage");
        staged.publish()?;
        crate::work::check_cancelled(cancelled)?;
        ensure!(self.package_ids()?.is_empty(), "grain is no longer empty");
        // POSIX rename atomically replaces the empty destination directory. A nonempty
        // destination (including an incomplete upload) fails without replacing it.
        tracing::info!(packages = listed.len(), "Committing imported packages");
        fs::rename(staged.root.join("packages"), self.root.join("packages"))?;
        self.publish()?;
        tracing::info!(
            packages = listed.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Catalog import complete"
        );
        Ok(())
    }
}
