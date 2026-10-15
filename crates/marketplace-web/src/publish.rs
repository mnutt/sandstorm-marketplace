//! Build outside the public tree, then swap directories with Linux renameat2.
//! A generation contains real directories and hard links, never symlinks.
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use askama::Template;
use marketplace_model::app_index::LoadedAppIndex;

use crate::{HomeTemplate, build_tag_chips, listing, render_app};

pub fn publish(source: &Path, output: &Path) -> Result<()> {
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
    let stage = output.with_extension("publish-stage");
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir(&stage)?;
    let result = build(&source, &stage).and_then(|()| commit(&stage, &output));
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    result
}

fn build(source: &Path, stage: &Path) -> Result<()> {
    let index = LoadedAppIndex::load(source)?;
    // Copy only the public catalog, never private metadata or submission state.
    for directory in ["apps", "experimental", "images", "packages"] {
        let path = source.join(directory);
        if path.exists() {
            copy_flat_directory(&path, &stage.join(directory))?;
        }
    }
    let mut known = std::collections::BTreeSet::new();
    for app in &index.index.apps {
        safe_name(&app.app_id)?;
        safe_name(&app.package_id)?;
        ensure!(known.insert(&app.app_id), "duplicate app ID");
        ensure!(
            stage.join("packages").join(&app.package_id).is_file(),
            "missing package {}",
            app.package_id
        );
        if !app.image_id.is_empty() {
            require_image(stage, &app.image_id)?;
        }
        for screenshot in index.load_details(&app.app_id)?.screenshots {
            require_image(stage, &screenshot.image_id)?;
        }
        let directory = stage.join("app").join(&app.app_id);
        fs::create_dir_all(&directory)?;
        write(
            &directory.join("index.html"),
            render_app(&index, app, "/packages")?,
        )?;
    }
    let template = HomeTemplate {
        tags: build_tag_chips(&index, None, ""),
        apps: index
            .index
            .apps
            .iter()
            .map(|app| listing(app, "/packages"))
            .collect(),
        search: String::new(),
    };
    write(
        &stage.join("marketplace.js"),
        include_str!("../static/marketplace.js"),
    )?;
    write(&stage.join("index.html"), template.render()?)?;
    if stage.join("app").exists() {
        fs::File::open(stage.join("app"))?.sync_all()?;
    }
    fs::File::open(stage)?.sync_all()?;
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
        fs::File::open(target)?.sync_all()?;
    }
    fs::File::open(dest)?.sync_all()?;
    Ok(())
}

fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    fs::write(path, contents)?;
    fs::File::open(path)?.sync_all()?;
    fs::File::open(path.parent().context("file parent")?)?.sync_all()?;
    Ok(())
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
            bail!(
                "atomic publication failed: {}",
                std::io::Error::last_os_error()
            );
        }
    } else {
        fs::rename(stage, output)?;
    }
    fs::File::open(output.parent().unwrap())?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "marketplace-publish-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publication_is_complete_and_failure_preserves_previous_generation() -> Result<()> {
        let temp = Temp::new();
        let source = temp.0.join("source");
        fs::create_dir(&source)?;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/app-index");
        for dir in ["apps", "images", "packages"] {
            copy_flat_directory(&fixtures.join(dir), &source.join(dir))?;
        }
        let output = temp.0.join("www");
        publish(&source, &output)?;
        let original = fs::read(output.join("index.html"))?;
        assert!(String::from_utf8_lossy(&original).contains("Etherpad"));
        assert!(output.join("packages/pkg-etherpad-0001").is_file());
        let app_id = LoadedAppIndex::load(&source)?.index.apps[0].app_id.clone();
        assert!(output.join(format!("app/{app_id}/index.html")).is_file());
        fs::remove_file(source.join("packages/pkg-etherpad-0001"))?;
        assert!(publish(&source, &output).is_err());
        assert_eq!(fs::read(output.join("index.html"))?, original);
        // Withdrawal creates a new generation with no stale app pages or SPKs.
        for dir in ["apps", "images", "packages"] {
            fs::remove_dir_all(source.join(dir))?;
            fs::create_dir(source.join(dir))?;
        }
        fs::write(source.join("apps/index.json"), r#"{"apps":[]}"#)?;
        publish(&source, &output)?;
        assert!(!output.join("app").exists());
        assert!(!output.join("packages/pkg-etherpad-0001").exists());
        Ok(())
    }

    #[test]
    fn rejects_path_traversal_and_symlinks() -> Result<()> {
        for name in ["", ".", "..", "../secret", "a/b", "/etc/passwd"] {
            assert!(safe_name(name).is_err());
        }
        let temp = Temp::new();
        let source = temp.0.join("source");
        fs::create_dir(&source)?;
        std::os::unix::fs::symlink("/etc/passwd", source.join("secret"))?;
        assert!(copy_flat_directory(&source, &temp.0.join("out")).is_err());
        Ok(())
    }
}
