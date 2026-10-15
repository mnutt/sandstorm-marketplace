//! SPK verification in Rust; no unpacked application code is ever executed.
use crate::{ids, package_capnp as p};
use anyhow::{Context, Result, bail, ensure};
use capnp::{
    message::{Builder, ReaderOptions},
    serialize,
};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, Write},
    path::Path,
};

pub const SIZE_LIMIT: u64 = 1 << 30;
pub const MAGIC: &[u8; 8] = b"\x8f\xc6\xcd\xef\x45\x1a\xea\x96";
pub fn limits(words: usize) -> ReaderOptions {
    ReaderOptions {
        traversal_limit_in_words: Some(words),
        nesting_limit: 64,
    }
}

pub fn verify(path: &Path) -> Result<Builder<capnp::message::HeapAllocator>> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= SIZE_LIMIT,
        "SPK exceeds 1 GiB upload limit"
    );
    let mut package_hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        package_hash.update(&buffer[..n]);
    }
    let package: [u8; 16] = package_hash.finalize()[..16].try_into()?;
    file.rewind()?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "not an SPK (bad magic number)");
    let mut decoder = lzma_rust2::XzReader::new(BufReader::new(file), true);
    let signature_message = serialize::read_message(&mut decoder, limits(1024))?;
    let signature = signature_message.get_root::<p::signature::Reader>()?;
    let public_key: [u8; 32] = signature
        .get_public_key()?
        .try_into()
        .context("invalid signing key length")?;
    let signed_hash = signature.get_signature()?;
    ensure!(signed_hash.len() == 128, "invalid SPK signature length");
    VerifyingKey::from_bytes(&public_key)?.verify_strict(
        &signed_hash[64..],
        &Signature::from_slice(&signed_hash[..64])?,
    )?;
    let mut archive = tempfile::tempfile()?;
    let mut hash = Sha512::new();
    let mut size = 0u64;
    loop {
        let n = decoder.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        ensure!(size <= SIZE_LIMIT, "SPK exceeds 1 GiB decompressed limit");
        hash.update(&buffer[..n]);
        archive.write_all(&buffer[..n])?;
    }
    ensure!(
        hash.finalize().as_slice() == &signed_hash[64..],
        "SPK content does not match signature"
    );
    let app = ids::canonical_app(&public_key, &package)?;
    archive.rewind()?;
    let archive_message = serialize::read_message(&mut archive, limits(SIZE_LIMIT as usize / 8))?;
    ensure!(archive.stream_position()? == size, "trailing archive data");
    let mut manifest_bytes = None;
    for entry in archive_message
        .get_root::<p::archive::Reader>()?
        .get_files()?
    {
        if entry.get_name()?.to_str()? == "sandstorm-manifest" {
            ensure!(manifest_bytes.is_none(), "duplicate manifest");
            match entry.which()? {
                p::archive::file::Regular(data) => manifest_bytes = Some(data?),
                _ => bail!("manifest is not a regular file"),
            }
        }
    }
    let mut bytes = manifest_bytes.context("SPK contains no manifest")?;
    let message = serialize::read_message_from_flat_slice(
        &mut bytes,
        limits(p::manifest::SIZE_LIMIT_IN_WORDS as usize),
    )?;
    ensure!(bytes.is_empty(), "trailing manifest data");
    let manifest = message.get_root::<p::manifest::Reader>()?;
    let metadata = manifest.get_metadata()?;
    for link in [metadata.get_website()?, metadata.get_code_url()?] {
        let link = link.to_str()?;
        ensure!(
            link.is_empty() || link.starts_with("https://") || link.starts_with("http://"),
            "website and source links must be HTTP URLs"
        );
    }
    let mut verified = Builder::new_default();
    {
        let mut info = verified.init_root::<p::verified_info::Builder>();
        ids::set_app(info.reborrow().init_app_id(), &app);
        ids::set_package(info.reborrow().init_package_id(), &package);
        info.set_title(manifest.get_app_title()?)?;
        info.set_version(manifest.get_app_version());
        info.set_marketing_version(manifest.get_app_marketing_version()?)?;
        info.set_metadata(metadata)?;
        let author = metadata.get_author();
        if author.has_pgp_signature() {
            ensure!(
                metadata.has_pgp_keyring(),
                "PGP signature requires a keyring"
            );
            let fingerprint = verify_pgp(
                &ids::app_id(&app),
                author.get_pgp_signature()?,
                metadata.get_pgp_keyring()?,
            )?;
            info.set_author_pgp_key_fingerprint(fingerprint.as_str());
        }
    }
    Ok(verified)
}

fn verify_pgp(app_id: &str, signature: &[u8], keyring: &[u8]) -> Result<String> {
    use pgp::{
        composed::{Deserializable, Message, SignedPublicKey},
        types::KeyDetails,
    };
    ensure!(
        signature.len() <= 1 << 20 && keyring.len() <= 4 << 20,
        "PGP identity data too large"
    );
    let mut message = Message::from_bytes(signature)?.decompress()?;
    let mut content = Vec::new();
    message.by_ref().take(4097).read_to_end(&mut content)?;
    ensure!(content.len() <= 4096, "PGP identity message too large");
    ensure!(
        std::str::from_utf8(&content)?.trim()
            == format!("I am the author of the Sandstorm.io app with the following ID: {app_id}"),
        "PGP signature binds a different app"
    );
    for key in SignedPublicKey::from_bytes_many(keyring)? {
        let key = key?;
        key.verify()?;
        if message.verify(&key).is_ok()
            || key
                .public_subkeys
                .iter()
                .any(|subkey| message.verify(subkey).is_ok())
        {
            return Ok(hex::encode_upper(key.fingerprint().as_bytes()));
        }
    }
    bail!("invalid PGP author signature")
}
