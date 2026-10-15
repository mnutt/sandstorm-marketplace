use anyhow::{Context, Result, ensure};
use capnp::{message::Builder, serialize};
use ed25519_dalek::{Signer, SigningKey};
use marketplace_grain::{ids, package_capnp as p, spk};
use rand::RngCore;
use sha2::{Digest, Sha512};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process::Command,
};

const APP_VERSION: u32 = 2;
const MARKETING_VERSION: &str = "0.2.0";

fn identity(root: &Path) -> Result<SigningKey> {
    let dir = root.join(".local");
    fs::create_dir_all(&dir)?;
    let keyring = dir.join("marketplace.keyring");
    let id_file = dir.join("app-id");
    ensure!(
        keyring.exists() == id_file.exists(),
        "Signing identity is incomplete; restore .local/marketplace.keyring and .local/app-id before packaging"
    );
    if keyring.exists() {
        let message = serialize::read_message(File::open(&keyring)?, spk::limits(1024))?;
        let key = message.get_root::<p::key_file::Reader>()?;
        let private = key.get_private_key()?;
        ensure!(private.len() == 64, "invalid private key length");
        let signing = SigningKey::from_bytes(private[..32].try_into()?);
        ensure!(
            key.get_public_key()? == signing.verifying_key().as_bytes()
                && &private[32..] == signing.verifying_key().as_bytes(),
            "inconsistent signing key"
        );
        ensure!(
            fs::read_to_string(id_file)?.trim() == ids::app_id(signing.verifying_key().as_bytes()),
            "app ID does not match signing key"
        );
        Ok(signing)
    } else {
        let mut seed = [0; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let key = SigningKey::from_bytes(&seed);
        let mut private = seed.to_vec();
        private.extend_from_slice(key.verifying_key().as_bytes());
        let mut message = Builder::new_default();
        let mut value = message.init_root::<p::key_file::Builder>();
        value.set_public_key(key.verifying_key().as_bytes());
        value.set_private_key(&private);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(keyring)?;
        serialize::write_message(&mut file, &message)?;
        file.sync_all()?;
        fs::write(
            id_file,
            format!("{}\n", ids::app_id(key.verifying_key().as_bytes())),
        )?;
        Ok(key)
    }
}
fn manifest(email: &str) -> Result<Vec<u8>> {
    let mut message = Builder::new_default();
    let mut m = message.init_root::<p::manifest::Builder>();
    m.reborrow()
        .init_app_title()
        .set_default_text("Sandstorm Marketplace");
    m.set_app_version(APP_VERSION);
    m.reborrow()
        .init_app_marketing_version()
        .set_default_text(MARKETING_VERSION);
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
pub fn package(root: &Path, email: &str, debug: bool) -> Result<()> {
    ensure!(
        email.contains('@') && !email.chars().any(char::is_whitespace),
        "invalid contact email"
    );
    super::build(debug)?;
    let signing = identity(root)?;
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
    fs::write(stage.join("sandstorm-manifest"), manifest(email)?)?;
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
    let mut artifact = tempfile::NamedTempFile::new_in(root.join("build"))?;
    artifact.write_all(spk::MAGIC)?;
    let mut encoder =
        lzma_rust2::XzWriter::new(artifact.as_file_mut(), lzma_rust2::XzOptions::default())?;
    serialize::write_message(&mut encoder, &signature)?;
    encoder.write_all(&bytes)?;
    encoder.finish()?;
    artifact.as_file().sync_all()?;
    spk::verify(artifact.path())?;
    artifact.persist(root.join("build/marketplace.spk"))?;
    let destination = work.join("stage");
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    fs::rename(staged.path(), destination)?;
    println!(
        "Built build/marketplace.spk (app ID {})",
        ids::app_id(signing.verifying_key().as_bytes())
    );
    Ok(())
}
