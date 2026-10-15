use capnp::{message::Builder, serialize, serialize_packed, traits::HasTypeId};
use ed25519_dalek::{Signer, SigningKey};
use marketplace_grain::{
    api_session_capnp::api_session,
    grain_capnp::{session_context, ui_view},
    ids, package_capnp as p,
    registry::Registry,
    rpc::{View, cast},
    spk, submit_capnp as s,
    util_capnp::byte_stream,
    web_session_capnp::web_session as web,
};
use sha2::{Digest, Sha512};
use std::{cell::RefCell, io::Write, rc::Rc};

fn key() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}
fn package(version: u32) -> Vec<u8> {
    let mut manifest = Builder::new_default();
    {
        let mut m = manifest.init_root::<p::manifest::Builder>();
        m.reborrow()
            .init_app_title()
            .set_default_text("Rust test app");
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
            .set_default_text("A signed integration-test package.");
        metadata
            .reborrow()
            .init_categories(1)
            .set(0, p::Category::DeveloperTools);
        metadata
            .init_license()
            .set_open_source(p::OpenSourceLicense::Apache2);
    }
    let mut manifest_bytes = Vec::new();
    serialize::write_message(&mut manifest_bytes, &manifest).unwrap();
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
    let mut signed = key().sign(&hash).to_bytes().to_vec();
    signed.extend_from_slice(&hash);
    let mut signature = Builder::new_default();
    let mut root = signature.init_root::<p::signature::Builder>();
    root.set_public_key(key().verifying_key().as_bytes());
    root.set_signature(&signed);
    let mut encoder =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
    serialize::write_message(&mut encoder, &signature).unwrap();
    encoder.write_all(&archive_bytes).unwrap();
    let mut bytes = spk::MAGIC.to_vec();
    bytes.extend_from_slice(&encoder.finish().unwrap());
    bytes
}
fn request(id: &str, state: Option<s::SubmissionState>, sequence: u64) -> Vec<u8> {
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
fn response_state(bytes: &[u8]) -> s::SubmissionState {
    assert_eq!(bytes[0], 0);
    let message = serialize_packed::read_message(&bytes[1..], spk::limits(4096)).unwrap();
    message
        .get_root::<s::submission_status::Reader>()
        .unwrap()
        .get_request_state()
        .unwrap()
}
fn upload(registry: &Registry, version: u32) -> String {
    let mut file = registry.temporary().unwrap();
    file.write_all(&package(version)).unwrap();
    registry.upload(file.path()).unwrap()
}

#[test]
fn registry_lifecycle_and_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    registry.publish()?;
    let id = upload(&registry, 1);
    let app = ids::app_id(key().verifying_key().as_bytes());
    let download = dir.path().join("www/packages").join(&id);
    let page = dir.path().join("www/app").join(&app).join("index.html");
    let response = registry.submission(&request(&id, Some(s::SubmissionState::Review), 1))?;
    assert_eq!(response_state(&response), s::SubmissionState::Review);
    registry.review(&id, "approve", "")?;
    assert!(!download.exists());
    assert!(!page.exists());
    registry.submission(&request(&id, Some(s::SubmissionState::Publish), 2))?;
    assert!(download.exists());
    assert!(page.exists());
    assert_eq!(std::fs::read(&download)?, package(1));
    assert!(std::fs::read_to_string(&page)?.contains("Rust test app"));
    // Real legacy status/metadata messages survive restart.
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    assert!(page.exists());
    let mut tampered = request(&id, Some(s::SubmissionState::Ignore), 3);
    *tampered.last_mut().unwrap() ^= 1;
    assert!(reopened.submission(&tampered).is_err());
    assert!(download.exists());
    assert!(
        reopened
            .submission(&request(&id, Some(s::SubmissionState::Ignore), 1))
            .is_err()
    );
    reopened.submission(&request(&id, Some(s::SubmissionState::Ignore), 3))?;
    assert!(!download.exists());
    assert!(!page.exists());
    // Idempotent retry and status check are accepted without republishing.
    reopened.submission(&request(&id, Some(s::SubmissionState::Ignore), 3))?;
    assert_eq!(
        response_state(&reopened.submission(&request(&id, None, 0))?),
        s::SubmissionState::Ignore
    );
    // A newer version enters the experimental catalog, then becomes approved.
    let next = upload(&registry, 2);
    registry.submission(&request(&next, Some(s::SubmissionState::Publish), 1))?;
    assert!(dir.path().join("www/packages").join(&next).exists());
    assert!(!page.exists());
    assert!(
        std::fs::read_to_string(dir.path().join("www/experimental/index.json"))?.contains(&next)
    );
    registry.review(&next, "approve", "")?;
    assert!(page.exists());
    registry.review(&next, "reject", "Needs changes")?;
    assert!(!page.exists());
    assert!(!dir.path().join("www/packages").join(next).exists());
    Ok(())
}

#[test]
fn rejects_corrupt_packages_and_invalid_ids() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let registry = Registry::new(temp.path().to_owned())?;
    for bytes in [b"not an SPK".to_vec(), package(1)[..32].to_vec(), {
        let mut bytes = package(1);
        let n = bytes.len() / 2;
        bytes[n] ^= 1;
        bytes
    }] {
        let mut file = registry.temporary()?;
        file.write_all(&bytes)?;
        assert!(registry.upload(file.path()).is_err());
    }
    assert!(registry.package_ids()?.is_empty());
    for id in ["../secret", "/etc/passwd", "bad"] {
        assert!(registry.package_dir(id).is_err());
    }
    assert_eq!(
        ids::parse_app_id(&ids::app_id(key().verifying_key().as_bytes()))?,
        key().verifying_key().to_bytes()
    );
    Ok(())
}
struct Context;
impl session_context::Server for Context {}
async fn session(view: &ui_view::Client, bits: &[bool], api: bool) -> capnp::Result<web::Client> {
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
    open.get().set_context(capnp_rpc::new_client(Context));
    Ok(cast(open.send().promise.await?.get()?.get_session()?))
}
async fn post(
    client: &web::Client,
    path: &str,
    body: &[u8],
) -> capnp::Result<capnp::capability::Response<web::response::Owned>> {
    let mut call = client.post_request();
    call.get().set_path(path);
    call.get().init_content().set_content(body);
    call.send().promise.await
}

#[tokio::test(flavor = "current_thread")]
async fn api_permissions_and_streaming_order() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let registry = Registry::new(temp.path().to_owned())?;
    let view: ui_view::Client = capnp_rpc::new_client(View {
        registry: registry.clone(),
        platform: Rc::new(RefCell::new(None)),
    });
    assert!(session(&view, &[false, true, false], true).await.is_err());
    assert!(session(&view, &[false, false, true], false).await.is_err());
    let api = session(&view, &[false, false, true], true).await?;
    let reviewer = session(&view, &[false, true, false], false).await?;
    let admin = session(&view, &[true, true, false], false).await?;
    assert!(matches!(
        post(&reviewer, "reindex", b"").await?.get()?.which()?,
        web::response::ClientError(_)
    ));
    let data = package(1);
    let mut start = api.post_streaming_request();
    start.get().set_path("upload");
    start.get().set_expected_size(data.len() as u64);
    let stream = start.send().promise.await?.get()?.get_stream()?;
    // Sandstorm asks for the response before it has finished sending the body.
    let response = stream.get_response_request().send().promise;
    let bytes: byte_stream::Client = cast(stream);
    for chunk in data.chunks(97) {
        let mut write = bytes.write_request();
        write.get().set_data(chunk);
        write.send().await?;
    }
    bytes.done_request().send().promise.await?;
    assert!(matches!(
        response.await?.get()?.which()?,
        web::response::NoContent(_)
    ));
    let id = registry.package_ids()?.remove(0);
    let signed = request(&id, Some(s::SubmissionState::Publish), 1);
    let response = post(&api, "status", &signed).await?;
    let body = match response.get()?.which()? {
        web::response::Content(c) => match c.get_body().which()? {
            web::response::content::body::Bytes(bytes) => bytes?,
            _ => panic!("expected bytes"),
        },
        _ => panic!("expected signed status reply"),
    };
    assert_eq!(response_state(body), s::SubmissionState::Publish);
    assert!(matches!(
        post(&admin, &format!("approve/{id}"), b"")
            .await?
            .get()?
            .which()?,
        web::response::NoContent(_)
    ));
    assert!(temp.path().join("www/index.html").exists());
    assert!(matches!(
        post(&api, "reindex", b"").await?.get()?.which()?,
        web::response::ClientError(_)
    ));
    let mut start = api.post_streaming_request();
    start.get().set_path("upload");
    start.get().set_expected_size(10);
    let stream = start.send().promise.await?.get()?.get_stream()?;
    let bytes: byte_stream::Client = cast(stream.clone());
    assert!(bytes.done_request().send().promise.await.is_err());
    assert!(matches!(
        stream
            .get_response_request()
            .send()
            .promise
            .await?
            .get()?
            .which()?,
        web::response::ClientError(_)
    ));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn packaged_entry_point_exposes_rpc_on_fd_three() -> anyhow::Result<()> {
    use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
    use futures::AsyncReadExt;
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    use tokio_util::compat::TokioAsyncReadCompatExt;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let (parent, child) = std::os::unix::net::UnixStream::pair()?;
            let fd = child.as_raw_fd();
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_marketplace-grain"));
            command.arg("--init").env("MARKETPLACE_ROOT", dir.path());
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let _child = Child(command.spawn()?);
            drop(child);
            parent.set_nonblocking(true)?;
            let (read, write) = tokio::net::UnixStream::from_std(parent)?.compat().split();
            let network = twoparty::VatNetwork::new(read, write, Side::Server, Default::default());
            let mut rpc = RpcSystem::new(Box::new(network), None);
            let view: ui_view::Client = rpc.bootstrap(Side::Client);
            let task = tokio::task::spawn_local(rpc);
            let work = async {
                let info = view.get_view_info_request().send().promise.await?;
                assert_eq!(info.get()?.get_permissions()?.len(), 3);
                let api = session(&view, &[false, false, true], true).await?;
                let admin = session(&view, &[true, true, false], false).await?;
                let bytes = package(1);
                assert!(matches!(
                    post(&api, "upload", &bytes).await?.get()?.which()?,
                    web::response::NoContent(_)
                ));
                let id = Registry::new(dir.path().to_owned())?
                    .package_ids()?
                    .remove(0);
                post(
                    &api,
                    "status",
                    &request(&id, Some(s::SubmissionState::Publish), 1),
                )
                .await?;
                assert!(matches!(
                    post(&admin, &format!("approve/{id}"), b"")
                        .await?
                        .get()?
                        .which()?,
                    web::response::NoContent(_)
                ));
                let index = std::fs::read_to_string(dir.path().join("www/apps/index.json"))?;
                assert!(index.contains(&id));
                Ok::<_, anyhow::Error>(())
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), work).await?;
            task.abort();
            result
        })
        .await
}
