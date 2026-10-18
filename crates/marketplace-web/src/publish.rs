//! Build outside the public tree, then exchange directories where supported.
//! Sandstorm falls back to recoverable ordinary renames.
//! A generation contains real directories and hard links, never symlinks.
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use askama::Template;
use marketplace_model::app_index::{AppDetailsForMarket, AppIndexForMarket, LoadedAppIndex};

use crate::{HomeTemplate, build_tag_chips, listing, render_app};

pub fn publish(source: &Path, output: &Path) -> Result<()> {
    publish_with_package_base(source, output, None)
}

// Remote mirrors do not contain SPKs. Only preview may use an upstream URL;
// production publication always requires a self-contained package tree.
pub(crate) fn publish_with_package_base(
    source: &Path,
    output: &Path,
    package_base: Option<&str>,
) -> Result<()> {
    publish_generation_inner(source, output, package_base, None)
}

/// Include only a public generation number, atomically installed with the site.
pub fn publish_generation(source: &Path, output: &Path, generation: i64) -> Result<()> {
    ensure!(generation > 0, "invalid publication generation");
    publish_generation_inner(source, output, None, Some(generation))
}

fn publish_generation_inner(
    source: &Path,
    output: &Path,
    package_base: Option<&str>,
    generation: Option<i64>,
) -> Result<()> {
    let source = source.canonicalize()?;
    let parent = output
        .parent()
        .context("output must have a parent directory")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    fs::create_dir_all(parent)?;
    let output = parent
        .canonicalize()?
        .join(output.file_name().context("output must have a name")?);
    ensure!(
        !output.starts_with(&source) && !source.starts_with(&output),
        "source and output must be separate trees"
    );
    ensure!(!output.is_symlink(), "output must not be a symlink");
    // Only one publisher may run per destination. The native grain serializes
    // mutations; this lock also protects manual CLI invocations.
    let lock_path = output.with_extension("publish.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another publisher holds the output lock"
    );
    // Recover before rendering: a failed new build must not leave a site missing
    // after an interrupted fallback swap.
    recover_publication(&output)?;
    let stage = output.with_extension("publish-stage");
    ensure!(
        !stage.is_symlink(),
        "publication stage must not be a symlink"
    );
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir(&stage)?;
    let started = std::time::Instant::now();
    let result = build(&source, &stage, package_base).and_then(|()| {
        if let Some(generation) = generation {
            fs::write(&stage.join("generation.txt"), generation.to_string())?;
        }
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Public stage written"
        );
        let flushing = std::time::Instant::now();
        sync_tree(&stage)?;
        tracing::info!(
            elapsed_ms = flushing.elapsed().as_millis() as u64,
            "Public stage flushed"
        );
        commit(&stage, &output)
    });
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    result
}

fn build(source: &Path, stage: &Path, package_base: Option<&str>) -> Result<()> {
    let index = LoadedAppIndex::load(source)?;
    // Copy only the public catalog, never private metadata or submission state.
    for directory in ["apps", "experimental", "images", "packages"] {
        let path = source.join(directory);
        if path.exists() {
            copy_flat_directory(&path, &stage.join(directory))?;
        }
    }
    let experimental_path = source.join("experimental/index.json");
    let experimental: AppIndexForMarket = if experimental_path.exists() {
        serde_json::from_slice(&fs::read(experimental_path)?)?
    } else {
        AppIndexForMarket { apps: Vec::new() }
    };
    let mut merged: std::collections::BTreeMap<_, _> = index
        .index
        .apps
        .iter()
        .map(|app| (app.app_id.clone(), app.clone()))
        .collect();
    ensure!(merged.len() == index.index.apps.len(), "duplicate app ID");
    let mut seen = std::collections::BTreeSet::new();
    let mut updates = LoadedAppIndex {
        root: source.to_owned(),
        index: AppIndexForMarket { apps: Vec::new() },
    };
    let mut experimental_ids = std::collections::BTreeSet::new();
    for app in experimental.apps {
        ensure!(
            seen.insert(app.app_id.clone()),
            "duplicate experimental app ID"
        );
        if merged
            .get(&app.app_id)
            .is_none_or(|stable| app.version_number > stable.version_number)
        {
            experimental_ids.insert(app.app_id.clone());
            updates.index.apps.push(app.clone());
            merged.insert(app.app_id.clone(), app);
        }
    }
    updates.index.apps.sort_by(|a, b| a.name.cmp(&b.name));
    let base = package_base.unwrap_or("/packages");
    for app in merged.values() {
        safe_name(&app.app_id)?;
        let stable = index
            .index
            .apps
            .iter()
            .find(|stable| stable.app_id == app.app_id);
        let approved_page = if let Some(stable) = stable {
            let details = index.load_details(&stable.app_id)?;
            validate_listing(stage, stable, &details, package_base.is_some())?;
            render_app(&index, stable, &details, base, false)?
        } else {
            unavailable_page("No approved public release")
        };
        let page = if experimental_ids.contains(&app.app_id) {
            let details: AppDetailsForMarket = serde_json::from_slice(&fs::read(
                source
                    .join("experimental")
                    .join(format!("{}.json", app.app_id)),
            )?)?;
            validate_listing(stage, app, &details, package_base.is_some())?;
            with_experimental(
                approved_page,
                render_app(&updates, app, &details, base, true)?,
            )?
        } else {
            with_experimental(approved_page, unavailable_page("No pending update"))?
        };
        let directory = stage.join("app").join(&app.app_id);
        fs::create_dir_all(&directory)?;
        fs::write(&directory.join("index.html"), page)?;
    }
    // Index files were hard-linked from the source. Unlink before rewriting so
    // filtering discovery cannot mutate the catalog used to build direct pages.
    for directory in ["apps", "experimental"] {
        let path = stage.join(directory).join("index.json");
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    // Render direct pages first, then remove deprecated apps from discovery.
    let mut index = index;
    index.index.apps.retain(|app| app.deprecation.is_none());
    updates.index.apps.retain(|app| app.deprecation.is_none());
    fs::write(
        &stage.join("apps/index.json"),
        serde_json::to_vec(&index.index)?,
    )?;
    fs::create_dir_all(stage.join("experimental"))?;
    fs::write(
        &stage.join("experimental/index.json"),
        serde_json::to_vec(&updates.index)?,
    )?;
    fs::write(
        &stage.join("marketplace.js"),
        include_str!("../static/marketplace.js"),
    )?;
    fs::write(
        &stage.join("index.html"),
        with_experimental(
            home(&index, None, base, false)?,
            home(&updates, None, base, true)?,
        )?,
    )?;
    let categories: std::collections::BTreeSet<_> = index
        .index
        .apps
        .iter()
        .chain(&updates.index.apps)
        .flat_map(|app| &app.categories)
        .collect();
    for name in categories {
        ensure!(
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']),
            "invalid category name"
        );
        let directory = stage.join("genre").join(&name);
        fs::create_dir_all(&directory)?;
        fs::write(
            &directory.join("index.html"),
            with_experimental(
                home(&index, Some(name), base, false)?,
                home(&updates, Some(name), base, true)?,
            )?,
        )?;
    }
    fs::create_dir(stage.join("assets"))?;
    for asset in crate::assets::ASSETS {
        fs::write(&stage.join("assets").join(asset.name), asset.bytes)?;
    }
    Ok(())
}

fn unavailable_page(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Marketplace</title><link rel=\"stylesheet\" href=\"/assets/marketplace.css\"></head><body><main><h1>{message}</h1><a href=\"/\">Back to all apps</a></main><script src=\"/marketplace.js\" defer></script></body></html>"
    )
}

fn home(
    index: &LoadedAppIndex,
    tag: Option<&str>,
    base: &str,
    experimental: bool,
) -> Result<String> {
    Ok(HomeTemplate {
        show_welcome: tag.is_none(),
        search_action: crate::home_href(tag, ""),
        tags: build_tag_chips(index, tag, ""),
        apps: index
            .index
            .apps
            .iter()
            .filter(|app| {
                tag.is_none_or(|tag| app.categories.iter().any(|category| category == tag))
            })
            .map(|app| listing(app, base))
            .collect(),
        search: String::new(),
        experimental,
    }
    .render()?)
}

// Both variants are rendered by the same templates. The browser selects the
// opt-in body before binding search/install/lightbox behavior; no JSON renderer
// or separate experimental routes are needed.
fn with_experimental(stable: String, experimental: String) -> Result<String> {
    let body = experimental
        .split_once("<body>")
        .context("missing template body")?
        .1
        .rsplit_once("</body>")
        .context("missing template body end")?
        .0;
    Ok(stable.replace(
        "</body>",
        &format!("<template id=\"experimental-page\">{body}</template></body>"),
    ))
}

fn validate_listing(
    stage: &Path,
    app: &marketplace_model::app_index::AppSummary,
    details: &AppDetailsForMarket,
    remote_packages: bool,
) -> Result<()> {
    safe_name(&app.app_id)?;
    safe_name(&app.package_id)?;
    ensure!(
        remote_packages || stage.join("packages").join(&app.package_id).is_file(),
        "missing package {}",
        app.package_id
    );
    if !app.image_id.is_empty() {
        require_image(stage, &app.image_id)?;
    }
    for shot in &details.screenshots {
        require_image(stage, &shot.image_id)?;
    }
    Ok(())
}

fn safe_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid catalog filename: {value}"
    );
    Ok(())
}

fn require_image(root: &Path, name: &str) -> Result<()> {
    safe_name(name)?;
    ensure!(
        root.join("images").join(name).is_file(),
        "missing image {name}"
    );
    Ok(())
}

fn copy_flat_directory(source: &Path, dest: &Path) -> Result<()> {
    ensure!(
        !source.is_symlink(),
        "catalog directory must not be a symlink"
    );
    fs::create_dir(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        safe_name(name.to_str().context("non-UTF8 filename")?)?;
        ensure!(
            entry.file_type()?.is_file(),
            "catalog contains a non-regular file"
        );
        let target = dest.join(name);
        // Package files are immutable; hard links avoid duplicating SPKs in the grain.
        if fs::hard_link(entry.path(), &target).is_err() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// Flush only after all writes are complete. sync_data covers file contents and
// metadata needed to read them, without forcing unrelated inode changes (such as
// hard-link counts) to disk for every immutable image or SPK. Directory fsyncs
// persist the names, once per directory, children before parents. This also
// handles caller-provided catalogs whose files have not already been flushed.
fn sync_tree(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            ensure!(kind.is_file(), "publication contains a non-regular file");
            fs::File::open(entry.path())?.sync_data()?;
        }
    }
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn sync_parent(output: &Path) -> Result<()> {
    fs::File::open(output.parent().context("publication parent")?)?.sync_all()?;
    Ok(())
}

// All callers hold the publication lock. If only the previous generation exists,
// a swap was interrupted before the new site became visible; restore the old one.
// If both exist, the new site committed and the previous generation is obsolete.
fn recover_publication(output: &Path) -> Result<()> {
    let previous = output.with_extension("publish-previous");
    ensure!(
        !previous.is_symlink(),
        "previous publication must not be a symlink"
    );
    if previous.exists() {
        if output.exists() {
            fs::remove_dir_all(&previous)?;
        } else {
            fs::rename(&previous, output)?;
        }
        sync_parent(output)?;
    }
    Ok(())
}

fn commit_with_renames(stage: &Path, output: &Path) -> Result<()> {
    let previous = output.with_extension("publish-previous");
    fs::rename(output, &previous).context("saving previous publication")?;
    // Persist the recovery point before replacing it. Readers can briefly see a
    // missing site between these renames, but never a partially built generation.
    let result = sync_parent(output)
        .and_then(|()| fs::rename(stage, output).context("installing new publication"));
    if let Err(error) = result {
        recover_publication(output)
            .context(format!("{error:#}; restoring previous publication"))?;
        return Err(error);
    }
    sync_parent(output)?;
    recover_publication(output)
}

fn commit(stage: &Path, output: &Path) -> Result<()> {
    if output.exists() {
        let from = CString::new(stage.as_os_str().as_bytes())?;
        let to = CString::new(output.as_os_str().as_bytes())?;
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            // Sandstorm can deny renameat2 with ENOSYS; glibc translates that to
            // EINVAL for exchange flags. Filesystems may also lack exchange support.
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
            ) {
                return commit_with_renames(stage, output);
            }
            return Err(error).context("atomic publication failed");
        }
    } else {
        fs::rename(stage, output)?;
    }
    sync_parent(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_is_complete_and_failure_preserves_previous_generation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/app-index");
        for dir in ["apps", "images", "packages"] {
            copy_flat_directory(&fixtures.join(dir), &source.join(dir))?;
        }
        let output = temp.path().join("www");
        let source_index = fs::read(source.join("apps/index.json"))?;
        publish_generation(&source, &output, 1)?;
        assert_eq!(fs::read_to_string(output.join("generation.txt"))?, "1");
        assert_eq!(fs::read(source.join("apps/index.json"))?, source_index);
        let original = fs::read(output.join("index.html"))?;
        assert!(String::from_utf8_lossy(&original).contains("Etherpad"));
        assert!(String::from_utf8_lossy(&original).contains("/assets/marketplace.css"));
        for asset in crate::assets::ASSETS {
            assert_eq!(
                fs::read(output.join("assets").join(asset.name))?,
                asset.bytes
            );
        }
        assert!(output.join("packages/pkg-etherpad-0001").is_file());
        let category = fs::read_to_string(output.join("genre/Collaboration/index.html"))?;
        assert!(category.contains("<h2 title=\"Etherpad\">"));
        assert!(!category.contains("<h2 title=\"Wekan\">"));
        assert!(category.contains("action=\"/genre/Collaboration\""));
        assert!(category.contains("id=\"welcome\" hidden"));
        assert!(!category.contains("?tag="));
        let app_id = LoadedAppIndex::load(&source)?.index.apps[0].app_id.clone();
        assert!(output.join(format!("app/{app_id}/index.html")).is_file());
        fs::remove_file(source.join("packages/pkg-etherpad-0001"))?;
        assert!(publish_generation(&source, &output, 2).is_err());
        assert_eq!(fs::read(output.join("index.html"))?, original);
        assert_eq!(fs::read_to_string(output.join("generation.txt"))?, "1");
        // Withdrawal creates a new generation with no stale app pages or SPKs.
        for dir in ["apps", "images", "packages"] {
            fs::remove_dir_all(source.join(dir))?;
            fs::create_dir(source.join(dir))?;
        }
        fs::write(source.join("apps/index.json"), r#"{"apps":[]}"#)?;
        publish_generation(&source, &output, 3)?;
        assert_eq!(fs::read_to_string(output.join("generation.txt"))?, "3");
        assert!(!output.join("app").exists());
        assert!(!output.join("genre").exists());
        assert!(!output.join("packages/pkg-etherpad-0001").exists());
        Ok(())
    }

    #[test]
    fn fallback_swap_rolls_back_on_failure_and_recovers_after_interruption() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("www");
        let previous = output.with_extension("publish-previous");
        let stage = output.with_extension("publish-stage");
        fs::create_dir(&output)?;
        fs::write(output.join("index.html"), "old")?;
        // A failure installing the new generation restores the previous site.
        assert!(commit_with_renames(&stage, &output).is_err());
        assert_eq!(fs::read_to_string(output.join("index.html"))?, "old");
        assert!(!previous.exists());

        fs::create_dir(&stage)?;
        fs::write(stage.join("index.html"), "new")?;
        fs::rename(&output, &previous)?;
        // Simulate stopping between the two renames. Recovery must happen even
        // when the next build fails to render.
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        assert!(publish(&source, &output).is_err());
        assert_eq!(fs::read_to_string(output.join("index.html"))?, "old");
        assert!(!previous.exists());

        fs::create_dir(&stage)?;
        fs::write(stage.join("index.html"), "new")?;
        commit_with_renames(&stage, &output)?;
        assert_eq!(fs::read_to_string(output.join("index.html"))?, "new");
        assert!(!previous.exists());
        // Stopping after installation preserves the new site on recovery.
        fs::create_dir(&previous)?;
        fs::write(previous.join("index.html"), "old")?;
        recover_publication(&output)?;
        assert_eq!(fs::read_to_string(output.join("index.html"))?, "new");
        assert!(!previous.exists());
        Ok(())
    }

    #[test]
    fn remote_preview_uses_static_pages_without_local_packages() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/app-index");
        for dir in ["apps", "images"] {
            copy_flat_directory(&fixtures.join(dir), &source.join(dir))?;
        }
        let output = temp.path().join("preview");
        assert!(publish(&source, &output).is_err());
        publish_with_package_base(&source, &output, Some("https://market.example/packages"))?;
        let home = fs::read_to_string(output.join("index.html"))?;
        assert!(home.contains("https://market.example/packages/pkg-etherpad-0001"));
        let category = fs::read_to_string(output.join("genre/Collaboration/index.html"))?;
        assert!(category.contains("Etherpad"));
        assert!(!category.contains("<h2 title=\"Wekan\">"));
        let id = LoadedAppIndex::load(&source)?.index.apps[0].app_id.clone();
        let detail = fs::read_to_string(output.join(format!("app/{id}/index.html")))?;
        assert!(detail.contains("https://market.example/packages/"));
        Ok(())
    }

    #[test]
    fn rejects_path_traversal_and_symlinks() -> Result<()> {
        for name in ["", ".", "..", "../secret", "a/b", "/etc/passwd"] {
            assert!(safe_name(name).is_err());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        std::os::unix::fs::symlink("/etc/passwd", source.join("secret"))?;
        assert!(copy_flat_directory(&source, &temp.path().join("out")).is_err());
        Ok(())
    }
}
