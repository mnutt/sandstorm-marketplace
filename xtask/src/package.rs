use anyhow::{Context, Result, ensure};
use capnp::{message::Builder, serialize};
use ed25519_dalek::{Signer, SigningKey};
use marketplace_grain::{ids, package_capnp as p, spk};
use sha2::{Digest, Sha512};
use std::{
    fs::{self, File},
    io::{BufReader, Write},
    num::NonZeroU64,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

pub const APP_ID: &str = "3theazdrcs30r9qysmh6ks0vndm3z4xkuk6k9qmu506h7mamvqg0";
pub const APP_VERSION: u32 = 9;
pub const MARKETING_VERSION: &str = "0.4.3";

pub struct Release {
    pub test_key: bool,
    pub keyring: Option<PathBuf>,
    pub app_version: u32,
    pub marketing_version: String,
}
impl Default for Release {
    fn default() -> Self {
        Self {
            test_key: false,
            keyring: None,
            app_version: APP_VERSION,
            marketing_version: MARKETING_VERSION.into(),
        }
    }
}
fn signing_key(release: &Release) -> Result<SigningKey> {
    if release.test_key {
        // Intentionally public and stable. Never access the production keyring.
        Ok(SigningKey::from_bytes(&[0x54; 32]))
    } else {
        let keyring = match &release.keyring {
            Some(path) => path.clone(),
            None => {
                PathBuf::from(std::env::var_os("HOME").context("HOME is unset; supply --keyring")?)
                    .join(".sandstorm/sandstorm-keyring")
            }
        };
        identity(&keyring, APP_ID)
    }
}

fn identity(keyring: &Path, app_id: &str) -> Result<SigningKey> {
    let expected = ids::parse_app_id(app_id)?;
    let mut file = BufReader::new(File::open(keyring).with_context(|| {
        format!(
            "opening signing keyring {} (override with --keyring)",
            keyring.display()
        )
    })?);
    let mut found = None;
    // SDK keyrings concatenate KeyFile messages. Select by public identity,
    // never by position, and never generate a replacement for a missing key.
    while let Some(message) = serialize::try_read_message(&mut file, spk::limits(1024))? {
        let key = message.get_root::<p::key_file::Reader>()?;
        if key.get_public_key()? != expected {
            continue;
        }
        let private = key.get_private_key()?;
        ensure!(
            private.len() == 64,
            "invalid private key length for marketplace app"
        );
        let signing = SigningKey::from_bytes(private[..32].try_into()?);
        ensure!(
            signing.verifying_key().as_bytes() == &expected && private[32..] == expected,
            "inconsistent marketplace signing key"
        );
        found = Some(signing);
    }
    found.with_context(|| format!("marketplace app key {app_id} not found in {}; restore/import the existing key or supply --keyring", keyring.display()))
}
fn manifest(email: &str, release: &Release) -> Result<Vec<u8>> {
    let mut message = Builder::new_default();
    let mut m = message.init_root::<p::manifest::Builder>();
    m.reborrow()
        .init_app_title()
        .set_default_text(if release.test_key {
            "Sandstorm Marketplace (Test)"
        } else {
            "Sandstorm Marketplace"
        });
    m.set_app_version(release.app_version);
    m.reborrow()
        .init_app_marketing_version()
        .set_default_text(&release.marketing_version);
    let mut action = m.reborrow().init_actions(1).get(0);
    action
        .reborrow()
        .init_title()
        .set_default_text("New marketplace");
    let mut argv = action.init_command().init_argv(2);
    argv.set(0, "/marketplace-grain");
    argv.set(1, "--init");
    m.reborrow()
        .init_continue_command()
        .init_argv(1)
        .set(0, "/marketplace-grain");
    let mut metadata = m.init_metadata();
    metadata.reborrow().init_author().set_contact_email(email);
    metadata
        .reborrow()
        .init_categories(1)
        .set(0, p::Category::DeveloperTools);
    metadata
        .reborrow()
        .init_license()
        .set_open_source(p::OpenSourceLicense::Apache2);
    metadata
        .reborrow()
        .init_short_description()
        .set_default_text("App marketplace");
    metadata.init_description().set_default_text("Host an independent Sandstorm app marketplace. Accept signed SPK submissions, review releases, and publish a static catalog and website.");
    let mut bytes = Vec::new();
    serialize::write_message(&mut bytes, &message)?;
    Ok(bytes)
}
fn files(
    path: &Path,
    mut list: capnp::struct_list::Builder<'_, p::archive::file::Owned>,
) -> Result<()> {
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for (i, entry) in entries.iter().enumerate() {
        let mut file = list.reborrow().get(i as u32);
        file.set_name(
            entry
                .file_name()
                .to_str()
                .context("non-UTF8 package filename")?,
        );
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let count = fs::read_dir(entry.path())?.count();
            files(&entry.path(), file.init_directory(count.try_into()?))?;
        } else {
            ensure!(
                entry.file_type()?.is_file(),
                "unexpected special file in package"
            );
            let bytes = fs::read(entry.path())?;
            if metadata.permissions().mode() & 0o111 != 0 {
                file.set_executable(&bytes);
            } else {
                file.set_regular(&bytes);
            }
        }
    }
    Ok(())
}
fn copy(stage: &Path, source: &Path, destination: &Path) -> Result<()> {
    let destination = stage.join(destination.strip_prefix("/").unwrap_or(destination));
    fs::create_dir_all(destination.parent().unwrap())?;
    fs::copy(source, destination)?;
    Ok(())
}
fn stage(root: &Path, email: &str, debug: bool, release: &Release) -> Result<tempfile::TempDir> {
    ensure!(
        email.contains('@') && !email.chars().any(char::is_whitespace),
        "invalid contact email"
    );
    super::build(debug)?;
    let work = root.join("build/package");
    fs::create_dir_all(&work)?;
    let staged = tempfile::tempdir_in(&work)?;
    let stage = staged.path();
    let binary = root.join(format!(
        "target/{}/marketplace-grain",
        if debug { "debug" } else { "release" }
    ));
    copy(stage, &binary, Path::new("marketplace-grain"))?;
    let libraries = super::output(Command::new("ldd").arg(&binary))?;
    let libraries = String::from_utf8(libraries.stdout)?;
    ensure!(
        !libraries.contains("not found"),
        "missing runtime library: {libraries}"
    );
    for library in libraries.split_whitespace().filter(|s| s.starts_with('/')) {
        copy(stage, Path::new(library), Path::new(library))?;
    }
    for dir in ["tmp", "var", "dev", "proc", "etc"] {
        fs::create_dir_all(stage.join(dir))?;
    }
    fs::set_permissions(stage.join("tmp"), fs::Permissions::from_mode(0o1777))?;
    copy(stage, &root.join("LICENSE"), Path::new("LICENSE"))?;
    let manifest_bytes = manifest(email, release)?;
    fs::write(stage.join("sandstorm-manifest"), &manifest_bytes)?;
    // Use canonical schema text; Rust's Debug formatter differs for Void fields.
    // Dev and release share the same binary manifest.
    let mut decode = Command::new(std::env::var_os("CAPNP").unwrap_or_else(|| "capnp".into()))
        .arg("decode")
        .arg("-I")
        .arg(root.join("crates/marketplace-grain/schema"))
        .arg(root.join("crates/marketplace-grain/schema/sandstorm/package.capnp"))
        .arg("Manifest")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    decode
        .stdin
        .take()
        .context("manifest decoder stdin")?
        .write_all(&manifest_bytes)?;
    let decoded = decode.wait_with_output()?;
    ensure!(
        decoded.status.success(),
        "failed to format the development manifest"
    );
    let manifest_text = String::from_utf8(decoded.stdout)?;
    fs::write(
        work.join("manifest.capnp"),
        format!(
            "# Generated by cargo xtask; edit xtask/src/package.rs instead.\n@0xbba9c7625d8af6b1;\nusing Package = import \"/sandstorm/package.capnp\";\nconst manifest :Package.Manifest = {manifest_text};\n"
        ),
    )?;
    Ok(staged)
}

fn install_stage(root: &Path, staged: tempfile::TempDir) -> Result<()> {
    let destination = root.join("build/package/stage");
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    fs::rename(staged.path(), destination)?;
    Ok(())
}

pub fn prepare_dev(root: &Path) -> Result<()> {
    let staged = stage(
        root,
        "maintainer@example.invalid",
        true,
        &Release::default(),
    )?;
    install_stage(root, staged)?;
    println!("Ready. Run: spk dev -I crates/marketplace-grain/schema");
    Ok(())
}

pub fn package(root: &Path, email: &str, debug: bool, release: &Release) -> Result<()> {
    let started = Instant::now();
    let signing = signing_key(release)?;
    let staged = stage(root, email, debug, release)?;
    println!("Build and staging finished in {:.2?}", started.elapsed());
    let archive_started = Instant::now();
    let stage = staged.path();
    let mut archive = Builder::new_default();
    let count = fs::read_dir(stage)?.count();
    files(
        stage,
        archive
            .init_root::<p::archive::Builder>()
            .init_files(count.try_into()?),
    )?;
    let mut bytes = Vec::new();
    serialize::write_message(&mut bytes, &archive)?;
    let hash = Sha512::digest(&bytes);
    let mut signed = signing.sign(&hash).to_bytes().to_vec();
    signed.extend_from_slice(&hash);
    let mut signature = Builder::new_default();
    let mut value = signature.init_root::<p::signature::Builder>();
    value.set_public_key(signing.verifying_key().as_bytes());
    value.set_signature(&signed);
    println!(
        "Archive and signing finished in {:.2?} ({} bytes)",
        archive_started.elapsed(),
        bytes.len()
    );
    let mut artifact = tempfile::NamedTempFile::new_in(root.join("build"))?;
    artifact.write_all(spk::MAGIC)?;
    let mut options = lzma_rust2::XzOptions::default();
    // Independent blocks allow parallel compression without lowering the preset.
    // Three dictionary lengths per block retain useful compression context;
    // cap workers to limit concurrent encoder memory on large hosts.
    let block_size = u64::from(options.lzma_options.dict_size) * 3;
    options.set_block_size(NonZeroU64::new(block_size));
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(4)) as u32;
    println!("Compressing XZ with {workers} workers and {block_size}-byte blocks");
    let compression_started = Instant::now();
    let mut encoder = lzma_rust2::XzWriterMt::new(artifact.as_file_mut(), options, workers)?;
    serialize::write_message(&mut encoder, &signature)?;
    encoder.write_all(&bytes)?;
    encoder.finish()?;
    println!(
        "XZ compression finished in {:.2?} ({} bytes)",
        compression_started.elapsed(),
        artifact.as_file().metadata()?.len()
    );
    artifact.as_file().sync_all()?;
    let verification_started = Instant::now();
    spk::verify(artifact.path())?;
    println!(
        "SPK verification finished in {:.2?}",
        verification_started.elapsed()
    );
    artifact.persist(root.join("build/marketplace.spk"))?;
    install_stage(root, staged)?;
    println!(
        "Built build/marketplace.spk (app ID {}) in {:.2?}",
        ids::app_id(signing.verifying_key().as_bytes()),
        started.elapsed()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key_record(seed: u8) -> Result<(SigningKey, Vec<u8>)> {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let public = signing.verifying_key();
        let mut private = signing.to_bytes().to_vec();
        private.extend_from_slice(public.as_bytes());
        let mut message = Builder::new_default();
        let mut record = message.init_root::<p::key_file::Builder>();
        record.set_public_key(public.as_bytes());
        record.set_private_key(&private);
        let mut bytes = Vec::new();
        serialize::write_message(&mut bytes, &message)?;
        Ok((signing, bytes))
    }

    #[test]
    fn shared_keyring_selects_by_app_id_without_modifying_keys() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("keyring");
        let (first, mut bytes) = key_record(1)?;
        let (second, other) = key_record(2)?;
        bytes.extend(other);
        fs::write(&path, &bytes)?;
        for expected in [first, second] {
            let id = ids::app_id(expected.verifying_key().as_bytes());
            let selected = identity(&path, &id)?;
            assert_eq!(selected.verifying_key(), expected.verifying_key());
        }
        assert!(identity(&path, APP_ID).is_err());
        assert_eq!(fs::read(&path)?, bytes);
        let missing = root.path().join("absent");
        assert!(identity(&missing, APP_ID).is_err());
        assert!(!missing.exists());
        Ok(())
    }

    #[test]
    fn shared_keyring_rejects_inconsistent_keys_and_truncated_records() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("keyring");
        let (signing, bytes) = key_record(3)?;
        let id = ids::app_id(signing.verifying_key().as_bytes());
        let reader = serialize::read_message(bytes.as_slice(), spk::limits(1024))?;
        for offset in [0, 32] {
            let mut message = Builder::new_default();
            message.set_root(reader.get_root::<p::key_file::Reader>()?)?;
            message
                .get_root::<p::key_file::Builder>()?
                .get_private_key()?[offset] ^= 1;
            serialize::write_message(File::create(&path)?, &message)?;
            assert!(identity(&path, &id).is_err());
        }
        let mut truncated = bytes.clone();
        truncated.extend_from_slice(&bytes[..bytes.len() - 1]);
        fs::write(&path, truncated)?;
        assert!(identity(&path, &id).is_err());
        Ok(())
    }

    #[test]
    fn production_identity_matches_sdk_package_definition() {
        assert!(
            include_str!("../../sandstorm-pkgdef.capnp").contains(&format!("id = \"{APP_ID}\""))
        );
    }

    #[test]
    fn ci_test_identity_is_stable_and_does_not_touch_production_keys() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join(".local"))?;
        fs::write(
            root.path().join(".local/marketplace.keyring"),
            b"do not read or replace",
        )?;
        let release = Release {
            test_key: true,
            keyring: Some(root.path().join(".local/marketplace.keyring")),
            app_version: 123,
            marketing_version: "v1.2.3 (test)".into(),
        };
        let first = signing_key(&release)?;
        let second = signing_key(&release)?;
        ensure!(
            first.verifying_key() == second.verifying_key(),
            "test identity changed"
        );
        ensure!(
            fs::read(release.keyring.as_ref().unwrap())? == b"do not read or replace",
            "production key changed"
        );
        let bytes = manifest("ci@example.invalid", &release)?;
        let message = serialize::read_message(&mut bytes.as_slice(), spk::limits(4096))?;
        let manifest = message.get_root::<p::manifest::Reader>()?;
        ensure!(
            manifest.get_app_version() == 123,
            "version override ignored"
        );
        ensure!(
            manifest.get_app_title()?.get_default_text()?.to_str()?
                == "Sandstorm Marketplace (Test)",
            "test build is unlabeled"
        );
        ensure!(
            manifest
                .get_app_marketing_version()?
                .get_default_text()?
                .to_str()?
                == "v1.2.3 (test)",
            "marketing version override ignored"
        );
        Ok(())
    }
}
