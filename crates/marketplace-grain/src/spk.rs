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
    verify_in(path, &std::env::temp_dir())
}

pub fn verify_in(path: &Path, scratch: &Path) -> Result<Builder<capnp::message::HeapAllocator>> {
    verify_in_cancellable(path, scratch, &std::sync::atomic::AtomicBool::new(false))
}
pub(crate) fn verify_in_cancellable(
    path: &Path,
    scratch: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Builder<capnp::message::HeapAllocator>> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= SIZE_LIMIT,
        "SPK exceeds 1 GiB upload limit"
    );
    let mut package_hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        crate::work::check_cancelled(cancelled)?;
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
    // A Sandstorm grain's /tmp is a separate 16 MiB tmpfs. Registry callers
    // supply /var/tmp so decompression uses persistent storage instead.
    let mut archive = tempfile::tempfile_in(scratch).with_context(|| {
        format!(
            "creating SPK verification scratch file in {}",
            scratch.display()
        )
    })?;
    tracing::info!(scratch = %scratch.display(), "Decompressing and checking SPK archive");
    let mut hash = Sha512::new();
    let mut size = 0u64;
    loop {
        crate::work::check_cancelled(cancelled)?;
        let n = decoder.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        ensure!(size <= SIZE_LIMIT, "SPK exceeds 1 GiB decompressed limit");
        hash.update(&buffer[..n]);
        archive
            .write_all(&buffer[..n])
            .context("writing decompressed SPK to verification scratch storage")?;
    }
    ensure!(
        hash.finalize().as_slice() == &signed_hash[64..],
        "SPK content does not match signature"
    );
    let app = ids::canonical_app(&public_key, &package)?;
    tracing::info!(
        decompressed_bytes = size,
        "SPK archive hash verified; reading manifest"
    );
    archive.rewind()?;
    let archive_message = read_archive(&mut archive, size)?;
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

// SDK archives are flat messages and can exceed the Rust stream reader's fixed
// 511-segment cap. Read their framing with explicit segment and total-size bounds;
// normal Cap'n Proto pointer validation and traversal limits still apply.
fn read_archive(
    input: &mut impl Read,
    size: u64,
) -> Result<capnp::message::Reader<serialize::OwnedSegments>> {
    ensure!(size <= SIZE_LIMIT, "archive too large");
    let mut word = [0u8; 4];
    input.read_exact(&mut word)?;
    let count = u64::from(u32::from_le_bytes(word)) + 1;
    ensure!(count <= 65536, "archive has too many segments");
    let table_bytes = (count + 2) / 2 * 8;
    ensure!(table_bytes <= size, "truncated archive segment table");
    let mut lengths = serialize::SegmentLengthsBuilder::with_capacity(count as usize);
    for _ in 0..count {
        input.read_exact(&mut word)?;
        lengths.try_push_segment(u32::from_le_bytes(word) as usize)?;
        ensure!(
            lengths.total_words() as u64 <= (size - table_bytes) / 8,
            "archive segment lengths exceed archive size"
        );
    }
    if count % 2 == 0 {
        input.read_exact(&mut word)?;
    }
    ensure!(
        lengths.total_words() as u64 * 8 == size - table_bytes,
        "archive size does not match segment lengths"
    );
    let mut segments = lengths.into_owned_segments();
    input.read_exact(&mut segments)?;
    Ok(capnp::message::Reader::new(
        segments,
        limits(SIZE_LIMIT as usize / 8),
    ))
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
    let mut message = Message::from_reader(signature)
        .context("parsing OpenPGP author proof (binary or ASCII armor)")?
        .0
        .decompress()?;
    let mut content = Vec::new();
    message.by_ref().take(4097).read_to_end(&mut content)?;
    ensure!(content.len() <= 4096, "PGP identity message too large");
    ensure!(
        std::str::from_utf8(&content)?.trim()
            == format!("I am the author of the Sandstorm.io app with the following ID: {app_id}"),
        "PGP signature binds a different app"
    );
    for key in SignedPublicKey::from_reader_many(keyring)
        .context("parsing OpenPGP author keyring (binary or ASCII armor)")?
        .0
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pgp::{
        armor::Dearmor,
        composed::{Deserializable, SignedPublicKey},
    };
    const PROOF: &[u8] = include_bytes!("../tests/fixtures/pgp/keybase-proof.asc");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/pgp/keybase-public-key.pgp");
    const APP: &str = "zxent02jw0z9ckkagz1uscdxesaf1f1wgzdunx2wrcj0jt9uf9rh";
    const FINGERPRINT: &str = "7116FC1150C0C96EF251B9A89324AC3921F3DE70";

    #[test]
    fn archive_reader_accepts_many_segments_and_bounds_lengths() -> Result<()> {
        use capnp::message::{AllocationStrategy, HeapAllocator};
        let allocator = HeapAllocator::new()
            .first_segment_words(1)
            .allocation_strategy(AllocationStrategy::FixedSize);
        let mut archive = Builder::new(allocator);
        let mut files = archive.init_root::<p::archive::Builder>().init_files(600);
        for n in 0..600 {
            let mut file = files.reborrow().get(n);
            file.set_name(format!("file-{n}").as_str());
            file.set_regular(b"test");
        }
        let mut bytes = Vec::new();
        serialize::write_message(&mut bytes, &archive)?;
        assert!(u32::from_le_bytes(bytes[..4].try_into()?) >= 512);
        let message = read_archive(&mut bytes.as_slice(), bytes.len() as u64)?;
        assert_eq!(
            message.get_root::<p::archive::Reader>()?.get_files()?.len(),
            600
        );
        let mut oversized = bytes.clone();
        oversized[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_archive(&mut oversized.as_slice(), oversized.len() as u64).is_err());
        let mut too_many = bytes.clone();
        too_many[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_archive(&mut too_many.as_slice(), too_many.len() as u64).is_err());
        assert!(
            read_archive(
                &mut bytes[..bytes.len() - 8].as_ref(),
                (bytes.len() - 8) as u64
            )
            .is_err()
        );
        bytes.extend_from_slice(&[0; 8]);
        assert!(read_archive(&mut bytes.as_slice(), bytes.len() as u64).is_err());
        Ok(())
    }

    #[test]
    fn verifies_armored_and_binary_author_proofs_and_keys() -> Result<()> {
        let mut binary = Vec::new();
        Dearmor::new(PROOF).read_to_end(&mut binary)?;
        let key = SignedPublicKey::from_bytes(KEY)?;
        let armored_key = key.to_armored_bytes(Default::default())?;
        for proof in [PROOF, binary.as_slice()] {
            for key in [KEY, armored_key.as_slice()] {
                assert_eq!(verify_pgp(APP, proof, key)?, FINGERPRINT);
                assert!(verify_pgp("another-app", proof, key).is_err());
            }
        }
        // The format change must not bypass signature verification.
        let mut changed = binary;
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(verify_pgp(APP, &changed, KEY).is_err());
        Ok(())
    }
}
