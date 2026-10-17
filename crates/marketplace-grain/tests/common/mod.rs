#![allow(dead_code)]
use capnp::{message::Builder, serialize, serialize_packed, traits::HasTypeId};
use ed25519_dalek::{Signer, SigningKey};
use marketplace_grain::{
    api_session_capnp::api_session,
    grain_capnp::{session_context, ui_view},
    ids, package_capnp as p,
    registry::Registry,
    rpc::cast,
    spk, submit_capnp as s,
    web_session_capnp::web_session as web,
};
use sha2::{Digest, Sha512};
use std::io::Write;

pub fn key() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}
pub fn package(version: u32) -> Vec<u8> {
    package_with_description(version, false)
}
pub fn package_with_description(version: u32, malformed: bool) -> Vec<u8> {
    package_with_key(version, malformed, &key(), "Rust test app")
}
pub fn package_with_key(
    version: u32,
    malformed: bool,
    signing: &SigningKey,
    title: &str,
) -> Vec<u8> {
    let mut manifest = Builder::new_default();
    {
        let mut m = manifest.init_root::<p::manifest::Builder>();
        m.reborrow().init_app_title().set_default_text(title);
        m.set_app_version(version);
        m.reborrow()
            .init_app_marketing_version()
            .set_default_text("1.0");
        let mut metadata = m.init_metadata();
        metadata
            .reborrow()
            .init_author()
            .set_contact_email("test@example.invalid");
        metadata
            .reborrow()
            .init_short_description()
            .set_default_text("Test package");
        metadata
            .reborrow()
            .init_description()
            .set_default_text(&format!(
                "A signed integration-test package. Description for build {version}.\n\n- **Features** with `code` and [documentation](https://example.org).\n\n<script>window.descriptionInjected = true</script>\n\n![tracker](/description-track) [unsafe](javascript:alert(1))"
            ));
        if version > 1 {
            let mut changelog =
                format!("## Build {version}\n\n- **Improved** sync for build {version}.\n");
            if version >= 3 {
                for old in 1..=15 {
                    changelog.push_str(&format!(
                        "\n## Earlier release {old}\n\n- Historical change {old}.\n"
                    ));
                }
            }
            if version >= 3 {
                changelog.push_str(
                    r#"
<script>window.changelogInjected = true</script>

<img src="/changelog-track" onerror="window.changelogInjected = true">

![tracker](/changelog-track)

[bad](javascript:alert(1)) [encoded](jav&#x61;script:alert(1)) [data](data:text/html,test)

[quote](https://example.org/"onclick="alert(1))

<svg onload="window.changelogInjected = true"></svg>
"#,
                );
            }
            metadata
                .reborrow()
                .init_change_log()
                .set_default_text(&changelog);
        }
        metadata.reborrow().init_categories(1).set(
            0,
            if version == 3 {
                p::Category::Productivity
            } else {
                p::Category::DeveloperTools
            },
        );
        metadata.reborrow().init_icons().init_market().set_svg(&format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\"><text x=\"5\" y=\"30\">{version}</text></svg>"));
        use base64::Engine;
        let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=").unwrap();
        let mut shot = metadata.reborrow().init_screenshots(1).get(0);
        shot.set_png(&png);
        shot.set_width(1);
        shot.set_height(1);
        metadata
            .init_license()
            .set_open_source(p::OpenSourceLicense::Apache2);
    }
    let mut manifest_bytes = Vec::new();
    serialize::write_message(&mut manifest_bytes, &manifest).unwrap();
    if malformed {
        let marker = b"A signed integration-test package.";
        let offset = manifest_bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap();
        manifest_bytes[offset] = 0xff;
    }
    let mut archive = Builder::new_default();
    let mut file = archive
        .init_root::<p::archive::Builder>()
        .init_files(1)
        .get(0);
    file.set_name("sandstorm-manifest");
    file.set_regular(&manifest_bytes);
    let mut archive_bytes = Vec::new();
    serialize::write_message(&mut archive_bytes, &archive).unwrap();
    let hash = Sha512::digest(&archive_bytes);
    let mut signed = signing.sign(&hash).to_bytes().to_vec();
    signed.extend_from_slice(&hash);
    let mut signature = Builder::new_default();
    let mut root = signature.init_root::<p::signature::Builder>();
    root.set_public_key(signing.verifying_key().as_bytes());
    root.set_signature(&signed);
    let mut encoder =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
    serialize::write_message(&mut encoder, &signature).unwrap();
    encoder.write_all(&archive_bytes).unwrap();
    let mut bytes = spk::MAGIC.to_vec();
    bytes.extend_from_slice(&encoder.finish().unwrap());
    bytes
}
pub fn request(id: &str, state: Option<s::SubmissionState>, sequence: u64) -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut root = message.init_root::<s::submission_request::Builder>();
    ids::set_package(
        root.reborrow().init_package_id(),
        &hex::decode(id).unwrap().try_into().unwrap(),
    );
    if let Some(state) = state {
        let mut change = root.init_set_state();
        change.set_new_state(state);
        change.set_sequence_number(sequence);
    } else {
        root.set_check_status(());
    }
    let mut bytes = Vec::new();
    serialize_packed::write_message(&mut bytes, &message).unwrap();
    bytes.extend_from_slice(&key().sign(&bytes).to_bytes());
    bytes
}
pub fn response_state(bytes: &[u8]) -> s::SubmissionState {
    assert_eq!(bytes[0], 0);
    let message = serialize_packed::read_message(&bytes[1..], spk::limits(4096)).unwrap();
    message
        .get_root::<s::submission_status::Reader>()
        .unwrap()
        .get_request_state()
        .unwrap()
}
pub fn upload(registry: &Registry, version: u32) -> String {
    let mut file = registry.temporary().unwrap();
    file.write_all(&package(version)).unwrap();
    registry.upload(file.path()).unwrap()
}

struct Context;
impl session_context::Server for Context {}
pub async fn session(
    view: &ui_view::Client,
    bits: &[bool],
    api: bool,
) -> capnp::Result<web::Client> {
    session_with_identity(view, bits, api, None).await
}
pub async fn session_with_identity(
    view: &ui_view::Client,
    bits: &[bool],
    api: bool,
    identity: Option<(&[u8], &str)>,
) -> capnp::Result<web::Client> {
    let mut open = view.new_session_request();
    open.get().set_session_type(if api {
        api_session::Client::TYPE_ID
    } else {
        web::Client::TYPE_ID
    });
    let mut permissions = open
        .get()
        .init_user_info()
        .init_permissions(bits.len() as u32);
    for (i, &bit) in bits.iter().enumerate() {
        permissions.set(i as u32, bit);
    }
    if let Some((id, name)) = identity {
        let mut user = open.get().get_user_info()?;
        user.set_identity_id(id);
        user.init_display_name().set_default_text(name);
    }
    open.get().set_context(capnp_rpc::new_client(Context));
    Ok(cast(open.send().promise.await?.get()?.get_session()?))
}
pub async fn post(
    client: &web::Client,
    path: &str,
    body: &[u8],
) -> capnp::Result<capnp::capability::Response<web::response::Owned>> {
    let mut call = client.post_request();
    call.get().set_path(path);
    call.get().init_content().set_content(body);
    call.send().promise.await
}

/// Inject malformed stored metadata to exercise administrative recovery.
pub fn seed_invalid_metadata(registry: &Registry, version: u32) -> anyhow::Result<String> {
    let bytes = package_with_description(version, true);
    let mut file = registry.temporary()?;
    file.write_all(&bytes)?;
    let metadata = spk::verify_in(file.path(), &registry.root.join("tmp"))?;
    let info = metadata.get_root_as_reader::<p::verified_info::Reader>()?;
    let id = hex::encode(ids::package_bytes(info.get_package_id()?));
    let dir = registry.package_dir(&id)?;
    std::fs::create_dir(&dir)?;
    std::fs::write(dir.join("spk"), bytes)?;
    let mut metadata_bytes = Vec::new();
    serialize::write_message(&mut metadata_bytes, &metadata)?;
    let db = rusqlite::Connection::open(registry.root.join("marketplace.sqlite"))?;
    let app = ids::app_id(&ids::app_bytes(info.get_app_id()?));
    db.execute("INSERT OR IGNORE INTO apps(app_id) VALUES(?)", [&app])?;
    db.execute("INSERT INTO releases(package_id,app_id,build_number,verified_metadata,request_state,review_state,review_reason,first_published_at,ever_approved,next_sequence_number) VALUES(?,?,?,?,?,'pending','',0,1,zeroblob(8))",rusqlite::params![id,app,version,metadata_bytes,u16::from(s::SubmissionState::Publish)])?;
    Ok(id)
}
