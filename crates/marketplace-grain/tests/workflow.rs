use marketplace_grain::{
    grain_capnp::ui_view,
    ids,
    registry::Registry,
    rpc::{View, cast},
    submit_capnp as s,
    util_capnp::byte_stream,
    web_session_capnp::web_session as web,
};
use std::{cell::RefCell, io::Write, rc::Rc};

mod common;
use common::*;

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
    // Stored submission state and metadata survive restart.
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
    assert!(std::fs::read_to_string(&page)?.contains("No approved public release"));
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
fn rollback_preserves_history_and_respects_author_and_review_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let first = upload(&registry, 1);
    registry.submission(&request(&first, Some(s::SubmissionState::Publish), 1))?;
    registry.review(&first, "approve", "")?;
    let second = upload(&registry, 2);
    registry.submission(&request(&second, Some(s::SubmissionState::Publish), 1))?;
    registry.review(&second, "approve", "")?;
    let public_id = || -> anyhow::Result<String> {
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("www/apps/index.json"))?)?;
        Ok(index["apps"][0]["packageId"].as_str().unwrap().to_owned())
    };
    assert_eq!(public_id()?, second);
    registry.review(&first, "rollback", "")?;
    assert_eq!(public_id()?, first);
    registry.review(&first, "rollback", "")?; // Retry after a lost response or failed publication.
    assert!(dir.path().join("www/packages").join(&second).exists());
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    assert_eq!(public_id()?, first);
    let queue = serde_json::to_value(reopened.queue()?)?;
    let first_row = queue
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["app"]["packageId"] == first)
        .unwrap();
    assert_eq!(first_row["current"], true);
    assert_eq!(first_row["pinnedPackageId"], first);
    assert!(first_row["receivedAt"].is_string());
    assert!(reopened.review(&second, "rollback", "").is_err());
    let third = upload(&reopened, 3);
    reopened.submission(&request(&third, Some(s::SubmissionState::Publish), 1))?;
    assert!(reopened.review(&third, "rollback", "").is_err());
    reopened.review(&third, "approve", "")?;
    assert_eq!(public_id()?, first); // Explicit selection survives new approvals.
    reopened.review(&third, "latest", "")?;
    assert_eq!(public_id()?, third);
    reopened.review(&first, "rollback", "")?;
    reopened.submission(&request(&first, Some(s::SubmissionState::Ignore), 2))?;
    assert_eq!(public_id()?, third);
    assert!(!dir.path().join("www/packages").join(&first).exists());
    assert!(reopened.review(&first, "rollback", "").is_err());
    reopened.submission(&request(&first, Some(s::SubmissionState::Publish), 3))?;
    assert_eq!(public_id()?, first);
    assert!(reopened.review(&first, "reject", "  ").is_err());
    reopened.review(&first, "reject", "Broken release")?;
    assert_eq!(public_id()?, third);
    assert!(reopened.review(&first, "rollback", "").is_err());
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
    for action in ["rollback", "latest"] {
        for client in [&reviewer, &api] {
            assert!(matches!(
                post(client, &format!("{action}/{id}"), b"")
                    .await?
                    .get()?
                    .which()?,
                web::response::ClientError(_)
            ));
        }
    }
    let mut status = reviewer.get_request();
    status.get().set_path("admin");
    let response = status.send().promise.await?;
    if let web::response::Content(content) = response.get()?.which()? {
        if let web::response::content::body::Bytes(bytes) = content.get_body().which()? {
            let status: serde_json::Value = serde_json::from_slice(bytes?)?;
            assert_eq!(status["canApprove"], false);
        } else {
            panic!("expected status JSON");
        }
    } else {
        panic!("expected status response");
    }

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
    process_entry_point(false).await
}
#[tokio::test(flavor = "current_thread")]
async fn failed_startup_publication_keeps_admin_available_for_recovery() -> anyhow::Result<()> {
    process_entry_point(true).await
}
async fn process_entry_point(recovery: bool) -> anyhow::Result<()> {
    use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
    use futures::AsyncReadExt;
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    use tokio_util::compat::TokioAsyncReadCompatExt;
    struct Wake;
    impl marketplace_grain::util_capnp::handle::Server for Wake {}
    struct Platform;
    impl marketplace_grain::grain_capnp::sandstorm_api::Server<capnp::any_pointer::Owned> for Platform {
        fn stay_awake(
            self: std::rc::Rc<Self>,
            _: marketplace_grain::grain_capnp::sandstorm_api::StayAwakeParams<
                capnp::any_pointer::Owned,
            >,
            mut results: marketplace_grain::grain_capnp::sandstorm_api::StayAwakeResults<
                capnp::any_pointer::Owned,
            >,
        ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
            results.get().set_handle(capnp_rpc::new_client(Wake));
            capnp::capability::Promise::ok(())
        }
    }
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
            let broken = if recovery {
                let registry = Registry::new(dir.path().to_owned())?;
                registry.publish()?;
                Some(seed_invalid_metadata(&registry, 99)?)
            } else {
                None
            };
            let (parent, child) = std::os::unix::net::UnixStream::pair()?;
            let fd = child.as_raw_fd();
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_marketplace-grain"));
            command
                .arg("--init")
                .env("MARKETPLACE_ROOT", dir.path())
                // SPK verification must use registry scratch storage, even when
                // the process-wide temporary directory is unavailable.
                .env("TMPDIR", dir.path().join("unavailable-system-tmp"));
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
            let platform: marketplace_grain::grain_capnp::sandstorm_api::Client<
                capnp::any_pointer::Owned,
            > = capnp_rpc::new_client(Platform);
            let mut rpc = RpcSystem::new(Box::new(network), Some(platform.client));
            let view: ui_view::Client = rpc.bootstrap(Side::Client);
            let task = tokio::task::spawn_local(rpc);
            let work = async {
                let info = view.get_view_info_request().send().promise.await?;
                assert_eq!(info.get()?.get_permissions()?.len(), 3);
                let api = session(&view, &[false, false, true], true).await?;
                let admin = session(&view, &[true, true, false], false).await?;
                if let Some(id) = &broken {
                    let mut queue = admin.get_request();
                    queue.get().set_path("admin");
                    assert!(matches!(
                        queue.send().promise.await?.get()?.which()?,
                        web::response::Content(_)
                    ));
                    assert!(matches!(
                        post(&admin, &format!("reject/{id}"), b"Invalid metadata")
                            .await?
                            .get()?
                            .which()?,
                        web::response::NoContent(_)
                    ));
                }
                let bytes = package(1);
                assert!(matches!(
                    post(&api, "upload", &bytes).await?.get()?.which()?,
                    web::response::NoContent(_)
                ));
                let id = Registry::new(dir.path().to_owned())?
                    .package_ids()?
                    .into_iter()
                    .find(|id| Some(id) != broken.as_ref())
                    .unwrap();
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

fn import_bundle(corrupt: bool) -> anyhow::Result<Vec<u8>> {
    let source = tempfile::tempdir()?;
    let registry = Registry::new(source.path().to_owned())?;
    let first = upload(&registry, 1);
    registry.set_state(&first, s::SubmissionState::Publish, 1)?;
    registry.review(&first, "approve", "")?;
    let second = upload(&registry, 2);
    registry.set_state(&second, s::SubmissionState::Publish, 1)?;
    registry.publish()?;
    let mut archive = tar::Builder::new(Vec::new());
    for name in ["apps/index.json", "experimental/index.json"] {
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(source.path().join("catalog").join(name))?)?;
        index["apps"][0]["author"] = serde_json::json!({"name": if name == "apps/index.json" { "Imported Packager" } else { "  " }, "keybaseUsername": "packager"});
        let bytes = serde_json::to_vec(&index)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, name, bytes.as_slice())?;
    }
    for (i, id) in [first, second].into_iter().enumerate() {
        let mut bytes = std::fs::read(registry.package_dir(&id)?.join("spk"))?;
        if corrupt && i == 1 {
            bytes[0] ^= 1;
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, format!("packages/{id}"), bytes.as_slice())?;
    }
    Ok(archive.into_inner()?)
}

#[test]
fn catalog_import_is_atomic_and_empty_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    registry.publish()?;
    let before = std::fs::read(dir.path().join("www/apps/index.json"))?;
    let mut file = registry.temporary()?;
    file.write_all(&import_bundle(true)?)?;
    assert!(registry.import_catalog(file.path()).is_err());
    assert!(registry.package_ids()?.is_empty());
    assert_eq!(
        before,
        std::fs::read(dir.path().join("www/apps/index.json"))?
    );
    // Symlinks must never be extracted, even if their target is inside the archive.
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    archive.append_link(&mut header, "packages/link", "/etc/passwd")?;
    let mut link = registry.temporary()?;
    link.write_all(&archive.into_inner()?)?;
    assert!(registry.import_catalog(link.path()).is_err());
    let mut valid = registry.temporary()?;
    valid.write_all(&import_bundle(false)?)?;
    registry.import_catalog(valid.path())?;
    assert_eq!(registry.package_ids()?.len(), 2);
    let main: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("www/apps/index.json"))?)?;
    let experimental: serde_json::Value = serde_json::from_slice(&std::fs::read(
        dir.path().join("www/experimental/index.json"),
    )?)?;
    assert_eq!(main["apps"][0]["versionNumber"], 1);
    assert_eq!(experimental["apps"][0]["versionNumber"], 2);
    assert_eq!(main["apps"][0]["author"]["name"], "Imported Packager");
    assert_eq!(
        experimental["apps"][0]["author"]["keybaseUsername"],
        "packager"
    );
    assert!(registry.import_catalog(valid.path()).is_err());
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    assert_eq!(reopened.package_ids()?.len(), 2);
    let app = main["apps"][0]["appId"].as_str().unwrap();
    let html = std::fs::read_to_string(dir.path().join("www/app").join(app).join("index.html"))?;
    assert!(html.contains("class=\"author-name\">Imported Packager</span>"));
    assert!(html.contains("class=\"author-name\">packager</span>"));
    let pending = experimental["apps"][0]["packageId"].as_str().unwrap();
    assert!(
        reopened
            .preview(pending)?
            .contains("class=\"author-name\">packager</span>")
    );
    // Imported display profiles apply only to the exact imported release.
    let next = upload(&reopened, 3);
    assert!(
        reopened
            .preview(&next)?
            .contains("class=\"author-name\">Unknown</span>")
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn catalog_import_permissions_and_streaming_race() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let view: ui_view::Client = capnp_rpc::new_client(View {
        registry: registry.clone(),
        platform: Rc::new(RefCell::new(None)),
    });
    let admin = session(&view, &[true, true, false], false).await?;
    let reviewer = session(&view, &[false, true, false], false).await?;
    let api = session(&view, &[false, false, true], true).await?;
    let bundle = import_bundle(false)?;
    for client in [&reviewer, &api] {
        assert!(matches!(
            post(client, "import", &bundle).await?.get()?.which()?,
            web::response::ClientError(_)
        ));
        let mut call = client.post_streaming_request();
        call.get().set_path("import");
        assert!(call.send().promise.await.is_err());
    }
    let mut call = admin.post_streaming_request();
    call.get().set_path("import");
    call.get().set_expected_size(bundle.len() as u64);
    let stream = call.send().promise.await?.get()?.get_stream()?;
    let response = stream.get_response_request().send().promise;
    let bytes: byte_stream::Client = cast(stream);
    let mut write = bytes.write_request();
    write.get().set_data(&bundle);
    write.send().await?;
    // Another upload wins while this import's body is in flight.
    upload(&registry, 3);
    assert!(bytes.done_request().send().promise.await.is_err());
    assert!(matches!(
        response.await?.get()?.which()?,
        web::response::ClientError(_)
    ));
    assert_eq!(registry.package_ids()?.len(), 1);
    assert!(matches!(
        post(&admin, "import", &bundle).await?.get()?.which()?,
        web::response::ClientError(_)
    ));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn streamed_catalog_import_disables_admin_import() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let view: ui_view::Client = capnp_rpc::new_client(View {
        registry: registry.clone(),
        platform: Rc::new(RefCell::new(None)),
    });
    let admin = session(&view, &[true, true, false], false).await?;
    let bundle = import_bundle(false)?;
    let mut call = admin.post_streaming_request();
    call.get().set_path("import");
    call.get().set_expected_size(bundle.len() as u64);
    let stream = call.send().promise.await?.get()?.get_stream()?;
    let response = stream.get_response_request().send().promise;
    let bytes: byte_stream::Client = cast(stream);
    for chunk in bundle.chunks(1024) {
        let mut write = bytes.write_request();
        write.get().set_data(chunk);
        write.send().await?;
    }
    bytes.done_request().send().promise.await?;
    assert!(matches!(
        response.await?.get()?.which()?,
        web::response::NoContent(_)
    ));
    assert_eq!(registry.package_ids()?.len(), 2);
    let mut status = admin.get_request();
    status.get().set_path("admin");
    let response = status.send().promise.await?;
    let web::response::Content(content) = response.get()?.which()? else {
        panic!("expected JSON");
    };
    let web::response::content::body::Bytes(body) = content.get_body().which()? else {
        panic!("expected bytes");
    };
    let snapshot: serde_json::Value = serde_json::from_slice(body?)?;
    assert_eq!(snapshot["canApprove"], true);
    assert_eq!(snapshot["busy"], false);
    assert_eq!(snapshot["releases"].as_array().unwrap().len(), 2);
    Ok(())
}

/// Exercise a real synced tarball without installing or modifying a live grain.
#[test]
#[ignore = "set MARKETPLACE_IMPORT_TARBALL to a synced catalog tarball"]
fn real_catalog_tarball_import() -> anyhow::Result<()> {
    let path = std::env::var_os("MARKETPLACE_IMPORT_TARBALL")
        .ok_or_else(|| anyhow::anyhow!("set MARKETPLACE_IMPORT_TARBALL"))?;
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    registry.import_catalog(std::path::Path::new(&path))?;
    let ids = registry.package_ids()?;
    assert!(!ids.is_empty());
    for id in &ids {
        assert!(dir.path().join("www/packages").join(id).is_file());
    }
    println!(
        "Verified and published {} packages from the real catalog tarball",
        ids.len()
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn reviewers_can_stream_private_packages_but_submitters_cannot() -> anyhow::Result<()> {
    struct Sink {
        bytes: Rc<RefCell<Vec<u8>>>,
        done: Rc<std::cell::Cell<bool>>,
        expected: Rc<std::cell::Cell<u64>>,
    }
    impl byte_stream::Server for Sink {
        fn expect_size(
            self: std::rc::Rc<Self>,
            params: byte_stream::ExpectSizeParams,
            _: byte_stream::ExpectSizeResults,
        ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
            self.expected.set(params.get().unwrap().get_size());
            capnp::capability::Promise::ok(())
        }
        fn write(
            self: std::rc::Rc<Self>,
            params: byte_stream::WriteParams,
        ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
            self.bytes
                .borrow_mut()
                .extend_from_slice(params.get().unwrap().get_data().unwrap());
            capnp::capability::Promise::ok(())
        }
        fn done(
            self: std::rc::Rc<Self>,
            _: byte_stream::DoneParams,
            _: byte_stream::DoneResults,
        ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
            self.done.set(true);
            capnp::capability::Promise::ok(())
        }
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().to_owned())?;
            let id = upload(&registry, 1);
            registry.submission(&request(&id, Some(s::SubmissionState::Review), 1))?;
            assert!(!dir.path().join("www/packages").join(&id).exists());
            assert_eq!(registry.queue()?[0].public_download, false);
            let view = capnp_rpc::new_client(View {
                registry: registry.clone(),
                platform: Rc::new(RefCell::new(None)),
            });
            let reviewer = session(&view, &[false, true, false], false).await?;
            let submitter = session(&view, &[false, false, true], true).await?;
            for (client, path) in [
                (&submitter, format!("download/{id}")),
                (&reviewer, "download/../../marketplace.sqlite".to_owned()),
                (&reviewer, format!("download/{}", "0".repeat(32))),
            ] {
                let mut get = client.get_request();
                get.get().set_path(&path);
                assert!(matches!(
                    get.send().promise.await?.get()?.which()?,
                    web::response::ClientError(_)
                ));
            }
            let bytes = Rc::new(RefCell::new(Vec::new()));
            let done = Rc::new(std::cell::Cell::new(false));
            let expected = Rc::new(std::cell::Cell::new(0));
            let mut get = reviewer.get_request();
            get.get().set_path(&format!("download/{id}"));
            get.get()
                .init_context()
                .set_response_stream(capnp_rpc::new_client(Sink {
                    bytes: bytes.clone(),
                    done: done.clone(),
                    expected: expected.clone(),
                }));
            let response = get.send().promise.await?;
            let content = match response.get()?.which()? {
                web::response::Content(content) => content,
                _ => panic!("expected download"),
            };
            assert_eq!(
                content.get_mime_type()?.to_str()?,
                "application/octet-stream"
            );
            match content.get_disposition().which()? {
                web::response::content::disposition::Download(name) => {
                    assert_eq!(name?.to_str()?, format!("{id}.spk"))
                }
                _ => panic!("expected attachment"),
            }
            let _handle = match content.get_body().which()? {
                web::response::content::body::Stream(handle) => handle?,
                _ => panic!("expected stream"),
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !done.get() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_eq!(*bytes.borrow(), package(1));
            assert_eq!(expected.get(), bytes.borrow().len() as u64);
            assert!(!dir.path().join("www/packages").join(&id).exists());
            Ok::<_, anyhow::Error>(())
        })
        .await
}

#[test]
fn invalid_metadata_is_rejected_before_commit_and_stored_entries_remain_reviewable()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let mut file = registry.temporary()?;
    file.write_all(&package_with_description(1, true))?;
    assert!(
        registry
            .upload(file.path())
            .unwrap_err()
            .to_string()
            .contains("invalid marketplace metadata")
    );
    assert!(registry.package_ids()?.is_empty());
    let bad = seed_invalid_metadata(&registry, 1)?;
    let queue = serde_json::to_value(registry.queue()?)?;
    assert!(
        queue[0]["validationError"]
            .as_str()
            .unwrap()
            .contains("utf-8")
    );
    assert!(registry.review(&bad, "approve", "").is_err());
    registry.review(&bad, "reject", "Invalid description")?;
    registry.publish()?;
    assert!(dir.path().join("www/index.html").exists());
    Ok(())
}

#[test]
fn experimental_updates_require_approval_history_and_stay_author_controlled() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let first = upload(&registry, 1);
    let next = upload(&registry, 2);
    registry.submission(&request(&first, Some(s::SubmissionState::Publish), 1))?;
    registry.submission(&request(&next, Some(s::SubmissionState::Publish), 1))?;
    let experimental = || -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::from_slice(&std::fs::read(
            dir.path().join("www/experimental/index.json"),
        )?)?)
    };
    assert_eq!(experimental()?["apps"].as_array().unwrap().len(), 0);
    for id in [&first, &next] {
        assert!(!dir.path().join("www/packages").join(id).exists());
    }
    assert_eq!(std::fs::read_dir(dir.path().join("www/images"))?.count(), 0);
    assert!(registry.preview(&next)?.contains("Private listing preview"));
    // Approving an embargoed release counts, but does not publish that release.
    registry.submission(&request(&first, Some(s::SubmissionState::Review), 2))?;
    registry.review(&first, "approve", "")?;
    assert_eq!(experimental()?["apps"][0]["packageId"], next);
    // An experimental page alone must not advertise an approved public page.
    let app_id = ids::app_id(key().verifying_key().as_bytes());
    assert!(
        dir.path()
            .join("www/app")
            .join(&app_id)
            .join("index.html")
            .is_file()
    );
    assert!(registry.queue()?.iter().all(|release| !release.public_page));
    registry.review(&first, "reject", "Withdrawn approval")?;
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    assert_eq!(experimental()?["apps"][0]["packageId"], next);
    assert!(!dir.path().join("www/packages").join(&first).exists());
    registry.submission(&request(&next, Some(s::SubmissionState::Review), 2))?;
    assert!(experimental()?["apps"].as_array().unwrap().is_empty());
    registry.submission(&request(&next, Some(s::SubmissionState::Publish), 3))?;
    registry.review(&next, "reject", "Not ready")?;
    assert!(experimental()?["apps"].as_array().unwrap().is_empty());
    registry.review(&next, "unapprove", "")?;
    assert_eq!(experimental()?["apps"][0]["packageId"], next);
    registry.submission(&request(&next, Some(s::SubmissionState::Ignore), 4))?;
    assert!(experimental()?["apps"].as_array().unwrap().is_empty());
    // A stale pending version must not bypass an intentional rollback.
    registry.submission(&request(&next, Some(s::SubmissionState::Publish), 5))?;
    let third = upload(&registry, 3);
    registry.submission(&request(&third, Some(s::SubmissionState::Publish), 1))?;
    registry.review(&third, "approve", "")?;
    registry.submission(&request(&first, Some(s::SubmissionState::Publish), 3))?;
    registry.review(&first, "approve", "")?;
    registry.review(&first, "rollback", "")?;
    assert!(experimental()?["apps"].as_array().unwrap().is_empty());
    assert!(!dir.path().join("www/packages").join(&next).exists());
    Ok(())
}

#[test]
fn publication_and_import_when_sandbox_blocks_directory_exchange() -> anyhow::Result<()> {
    const CHILD: &str = "MARKETPLACE_TEST_RENAMEAT2_ERRNO";
    let Ok(errno) = std::env::var(CHILD) else {
        // Seccomp is irreversible and thread-local: use a child test process so
        // the rest of the test runner retains its normal syscall permissions.
        for errno in [libc::ENOSYS, libc::EINVAL] {
            let result = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "publication_and_import_when_sandbox_blocks_directory_exchange",
                    "--nocapture",
                ])
                .env(CHILD, errno.to_string())
                .output()?;
            anyhow::ensure!(
                result.status.success(),
                "sandbox publication test failed: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        return Ok(());
    };
    let errno: u32 = errno.parse()?;
    // Load seccomp_data.nr; reject renameat2, allow everything else. ENOSYS
    // reproduces Sandstorm's allowlist; glibc may expose it as EINVAL.
    let filter = [
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: libc::SYS_renameat2 as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | errno,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut _,
    };
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    // Includes replacing an existing site, publishing approved and experimental
    // releases, rejecting/withdrawing SPKs, and rebuilding after a restart.
    catalog_import_is_atomic_and_empty_only()?;
    registry_lifecycle_and_restart()?;
    rollback_preserves_history_and_respects_author_and_review_state()?;
    Ok(())
}

#[test]
fn deprecation_hides_discovery_preserves_downloads_and_survives_new_releases() -> anyhow::Result<()>
{
    use marketplace_model::app_index::Deprecation;
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let first = upload(&registry, 1);
    registry.set_state(&first, s::SubmissionState::Publish, 1)?;
    registry.review(&first, "approve", "")?;
    let pending = upload(&registry, 3);
    registry.set_state(&pending, s::SubmissionState::Publish, 1)?;
    let private = upload(&registry, 4);
    let app = ids::app_id(key().verifying_key().as_bytes());
    let other_key = ed25519_dalek::SigningKey::from_bytes(&[43; 32]);
    let replacement = ids::app_id(other_key.verifying_key().as_bytes());
    let mut file = registry.temporary()?;
    file.write_all(&package_with_key(1, false, &other_key, "Replacement"))?;
    let other = registry.upload(file.path())?;
    let value = || {
        Some(Deprecation {
            replacement_app_id: Some(replacement.clone()),
        })
    };
    assert!(registry.set_deprecation(&app, value()).is_err());
    registry.set_state(&other, s::SubmissionState::Publish, 1)?;
    registry.review(&other, "approve", "")?;
    assert!(
        registry
            .set_deprecation(
                &app,
                Some(Deprecation {
                    replacement_app_id: Some(app.clone())
                })
            )
            .is_err()
    );
    registry.set_deprecation(&app, value())?;
    assert!(std::fs::read_to_string(dir.path().join("catalog/apps/index.json"))?.contains(&app));
    marketplace_web::publish::publish(&dir.path().join("catalog"), &dir.path().join("standalone"))?;
    assert!(
        dir.path()
            .join("standalone/app")
            .join(&app)
            .join("index.html")
            .exists()
    );
    assert!(
        registry
            .set_deprecation(
                &replacement,
                Some(Deprecation {
                    replacement_app_id: Some(app.clone())
                })
            )
            .is_err()
    );
    let check = || -> anyhow::Result<()> {
        for index in ["apps/index.json", "experimental/index.json"] {
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("www").join(index))?)?;
            assert!(
                json["apps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["appId"] != app)
            );
        }
        let home = std::fs::read_to_string(dir.path().join("www/index.html"))?;
        assert!(!home.contains(&format!("/app/{app}")));
        let page =
            std::fs::read_to_string(dir.path().join("www/app").join(&app).join("index.html"))?;
        assert!(page.contains("This app is deprecated."));
        assert!(page.contains(&format!("href=\"/app/{replacement}\"")));
        for package in [&first, &pending] {
            assert!(dir.path().join("www/packages").join(package).is_file());
        }
        assert!(!dir.path().join("www/packages").join(&private).exists());
        Ok(())
    };
    check()?;
    registry.review(&pending, "approve", "")?;
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    check()?;
    assert!(
        reopened
            .queue()?
            .iter()
            .filter(|r| r.app.app_id == app)
            .all(|r| r.app.deprecation.is_some())
    );
    reopened.set_deprecation(&app, None)?;
    assert!(std::fs::read_to_string(dir.path().join("www/apps/index.json"))?.contains(&app));
    assert!(
        !std::fs::read_to_string(dir.path().join("www/app").join(&app).join("index.html"))?
            .contains("This app is deprecated.")
    );
    Ok(())
}

#[test]
fn audit_tracks_actors_retries_failed_publication_and_recovery() -> anyhow::Result<()> {
    use marketplace_grain::audit::Actor;
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let app = ids::app_id(key().verifying_key().as_bytes());
    let actor = Actor {
        kind: "sandstormIdentity".into(),
        identity_id: Some("01".repeat(32)),
        display_name: "Reviewer Alice".into(),
        signing_key: None,
    };
    let reviewer = registry.with_actor(actor.clone());
    let id = upload(&registry, 1);
    let events = registry.audit_page(&app, None)?.events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].action, "submitted");
    // Re-upload is a retry, not another submission.
    assert_eq!(upload(&registry, 1), id);
    assert_eq!(registry.audit_page(&app, None)?.events.len(), 1);
    let signed = request(&id, Some(s::SubmissionState::Publish), 1);
    registry.submission(&signed)?;
    let events = registry.audit_page(&app, None)?.events;
    assert_eq!(events[0].actor.kind, "appSigningKey");
    assert_eq!(
        events[0].actor.signing_key.as_deref(),
        Some(hex::encode(key().verifying_key().as_bytes()).as_str())
    );
    assert!(!events.iter().any(|e| e.action == "publicationChanged"));
    let before = events.len();
    registry.submission(&signed)?;
    assert_eq!(registry.audit_page(&app, None)?.events.len(), before);
    // Fail rendering after saving approval.
    std::fs::remove_dir_all(dir.path().join("catalog"))?;
    std::fs::write(dir.path().join("catalog"), "blocked")?;
    assert!(reviewer.review(&id, "approve", "").is_err());
    let events = registry.audit_page(&app, None)?.events;
    assert_eq!(events[0].action, "approved");
    assert_eq!(events[0].actor, actor);
    assert!(!events.iter().any(|e| e.action == "publicationChanged"));
    let decision = events[0].number;
    std::fs::remove_file(dir.path().join("catalog"))?;
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    let events = reopened.audit_page(&app, None)?.events;
    assert_eq!(events[0].action, "publicationChanged");
    assert_eq!(events[0].details["latestDecision"], decision);
    assert_eq!(
        events[0].details["decisionActor"]["displayName"],
        "Reviewer Alice"
    );
    assert_eq!(events[0].details["next"]["download"], true);
    let count = events.len();
    reviewer.review(&id, "approve", "")?;
    reopened.publish()?;
    assert_eq!(reopened.audit_page(&app, None)?.events.len(), count);
    reviewer.review(&id, "unapprove", "")?;
    let events = reopened.audit_page(&app, None)?.events;
    assert!(
        events
            .iter()
            .any(|e| e.action == "approvalRevoked" && e.actor == actor)
    );
    assert_eq!(events[0].details["next"]["approvedListing"], false);
    reviewer.review(&id, "reject", "Please fix this.")?;
    let events = reopened.audit_page(&app, None)?.events;
    assert!(
        events
            .iter()
            .any(|e| e.action == "rejected" && e.details["next"]["reason"] == "Please fix this.")
    );
    assert!(!dir.path().join("www/audit").exists());
    Ok(())
}

#[test]
fn audit_imports_do_not_invent_historical_actors() -> anyhow::Result<()> {
    let app = ids::app_id(key().verifying_key().as_bytes());
    let imported = tempfile::tempdir()?;
    let registry = Registry::new(imported.path().to_owned())?;
    let mut file = registry.temporary()?;
    file.write_all(&import_bundle(false)?)?;
    registry.import_catalog(file.path())?;
    let events = registry.audit_page(&app, None)?.events;
    assert!(events.iter().any(|e| e.action == "imported"));
    assert!(events.iter().any(|e| e.action == "publicationChanged"));
    assert!(!events.iter().any(|e| e.action == "approved"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn audit_rpc_identity_and_access() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let registry = Registry::new(temp.path().to_owned())?;
    let view: ui_view::Client = capnp_rpc::new_client(View {
        registry: registry.clone(),
        platform: Rc::new(RefCell::new(None)),
    });
    let api = session_with_identity(
        &view,
        &[false, false, true],
        true,
        Some((&[9; 32], "Uploader")),
    )
    .await?;
    let admin = session_with_identity(
        &view,
        &[true, true, false],
        false,
        Some((&[7; 32], "Approver")),
    )
    .await?;
    let reviewer = session(&view, &[false, true, false], false).await?;
    post(&api, "upload", &package(1)).await?;
    let id = registry.package_ids()?.remove(0);
    let app = ids::app_id(key().verifying_key().as_bytes());
    let events = registry.audit_page(&app, None)?.events;
    assert_eq!(events[0].actor.display_name, "Uploader");
    assert_eq!(events[0].actor.identity_id, Some("09".repeat(32)));
    assert!(events[0].actor.signing_key.is_none());
    post(
        &api,
        "status",
        &request(&id, Some(s::SubmissionState::Review), 1),
    )
    .await?;
    let event = registry.audit_page(&app, None)?.events.remove(0);
    assert_eq!(event.actor.kind, "appSigningKey");
    assert_eq!(event.actor.identity_id, Some("09".repeat(32)));
    post(&admin, &format!("approve/{id}"), b"").await?;
    let event = registry.audit_page(&app, None)?.events.remove(0);
    assert_eq!(event.actor.display_name, "Approver");
    assert_eq!(event.actor.identity_id, Some("07".repeat(32)));
    for (client, allowed) in [(&admin, true), (&reviewer, true), (&api, false)] {
        let mut get = client.get_request();
        get.get().set_path(&format!("audit/{app}"));
        assert_eq!(
            matches!(
                get.send().promise.await?.get()?.which()?,
                web::response::Content(_)
            ),
            allowed
        );
    }
    for path in [
        "audit/../packages".into(),
        format!("audit/{app}/invalid"),
        format!("audit/{app}/1/2"),
    ] {
        let mut get = admin.get_request();
        get.get().set_path(&path);
        assert!(matches!(
            get.send().promise.await?.get()?.which()?,
            web::response::ClientError(_)
        ));
    }
    Ok(())
}

#[test]
fn audit_recovers_visibility_installed_before_history_was_written() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    let app = ids::app_id(key().verifying_key().as_bytes());
    let id = upload(&registry, 1);
    registry.submission(&request(&id, Some(s::SubmissionState::Publish), 1))?;
    // Save approval but block the following catalog build.
    std::fs::remove_dir_all(dir.path().join("catalog"))?;
    std::fs::write(dir.path().join("catalog"), "blocked")?;
    assert!(registry.review(&id, "approve", "").is_err());
    let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
    db.execute_batch("CREATE TRIGGER interrupt_visibility BEFORE UPDATE OF public_visibility ON releases BEGIN SELECT RAISE(FAIL,'simulated crash after site swap'); END;")?;
    std::fs::remove_file(dir.path().join("catalog"))?;
    assert!(registry.publish().is_err());
    assert!(dir.path().join("www/packages").join(&id).is_file());
    db.execute_batch("DROP TRIGGER interrupt_visibility;")?;
    let recovered = Registry::new(dir.path().to_owned())?;
    // A later decision first reconciles the installed generation, then records itself.
    recovered.review(&id, "reject", "Revoke after recovery")?;
    let events = recovered.audit_page(&app, None)?.events;
    let publication: Vec<_> = events
        .iter()
        .filter(|e| e.action == "publicationChanged")
        .collect();
    assert_eq!(publication.len(), 2);
    assert_eq!(publication[0].details["next"]["download"], false);
    assert_eq!(publication[1].details["next"]["download"], true);
    assert!(
        publication[1].number
            < events
                .iter()
                .find(|e| e.action == "rejected")
                .unwrap()
                .number
    );
    recovered.publish()?;
    assert_eq!(recovered.audit_page(&app, None)?.events.len(), events.len());
    Ok(())
}

#[test]
fn import_retries_ignore_unreferenced_files() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().to_owned())?;
    std::fs::create_dir(dir.path().join("packages/incomplete"))?;
    let mut file = registry.temporary()?;
    file.write_all(&import_bundle(false)?)?;
    registry.import_catalog(file.path())?;
    assert!(
        !Registry::new(dir.path().to_owned())?
            .package_ids()?
            .is_empty()
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn audit_new_submission_after_restart_records_the_submitter() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let old = Registry::new(temp.path().to_owned())?;
    let previous = upload(&old, 331);
    drop(old);
    let registry = Registry::new(temp.path().to_owned())?;
    let app = ids::app_id(key().verifying_key().as_bytes());
    let view: ui_view::Client = capnp_rpc::new_client(View {
        registry: registry.clone(),
        platform: Rc::new(RefCell::new(None)),
    });
    let api = session(&view, &[false, false, true], true).await?;
    let data = package(332);
    let mut start = api.post_streaming_request();
    start.get().set_path("upload");
    start.get().set_expected_size(data.len() as u64);
    let stream = start.send().promise.await?.get()?.get_stream()?;
    let response = stream.get_response_request().send().promise;
    let bytes: byte_stream::Client = cast(stream);
    let mut write = bytes.write_request();
    write.get().set_data(&data);
    write.send().await?;
    bytes.done_request().send().promise.await?;
    assert!(matches!(
        response.await?.get()?.which()?,
        web::response::NoContent(_)
    ));
    let current = registry
        .package_ids()?
        .into_iter()
        .find(|id| id != &previous)
        .unwrap();
    post(
        &api,
        "status",
        &request(&current, Some(s::SubmissionState::Publish), 1),
    )
    .await?;
    let events = Registry::new(temp.path().to_owned())?
        .audit_page(&app, None)?
        .events;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].action, "authorStateChanged");
    assert_eq!(events[0].actor.kind, "appSigningKey");
    assert_eq!(events[1].action, "submitted");
    assert_eq!(events[1].version, Some(332));
    assert_eq!(events[1].actor.kind, "unidentifiedSession");
    assert_eq!(events[2].action, "submitted");
    assert_eq!(events[2].version, Some(331));
    Ok(())
}

#[test]
fn audit_resubmissions_record_fresh_intent_but_not_status_checks_or_retries() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let registry = Registry::new(temp.path().to_owned())?;
    let actor = marketplace_grain::audit::Actor {
        kind: "sandstormIdentity".into(),
        identity_id: Some("09".repeat(32)),
        display_name: "Author".into(),
        signing_key: None,
    };
    let author = registry.with_actor(actor.clone());
    let app = ids::app_id(key().verifying_key().as_bytes());
    let id = upload(&registry, 1);
    author.submission(&request(&id, Some(s::SubmissionState::Publish), 1))?;
    registry.review(&id, "approve", "")?;
    let before = registry.audit_page(&app, None)?.events.len();
    let db = rusqlite::Connection::open(temp.path().join("marketplace.sqlite"))?;
    let cause = || -> anyhow::Result<i64> {
        Ok(db.query_row("SELECT max(event_number) FROM audit_events WHERE app_id=? AND action NOT IN ('publicationChanged','submitted','resubmitted')", [&app], |r| r.get(0))?)
    };
    let original_cause = cause()?;
    let resubmit = request(&id, Some(s::SubmissionState::Publish), 2);
    author.submission(&resubmit)?;
    let events = registry.audit_page(&app, None)?.events;
    assert_eq!(events.len(), before + 1);
    assert_eq!(events[0].action, "resubmitted");
    assert_eq!(events[0].version, Some(1));
    assert_eq!(events[0].package_id.as_deref(), Some(id.as_str()));
    assert_eq!(events[0].actor.kind, "appSigningKey");
    assert_eq!(events[0].actor.identity_id, actor.identity_id);
    assert_eq!(events[0].actor.display_name, "Author");
    assert_eq!(
        events[0].actor.signing_key,
        Some(hex::encode(key().verifying_key().as_bytes()))
    );
    assert_eq!(events[0].details["previous"], events[0].details["next"]);
    assert_eq!(events[0].details["next"]["review"], "approved");
    assert_eq!(events[0].details["sequenceNumber"], "2");
    // A no-op must not displace the decision associated with public visibility.
    assert_eq!(cause()?, original_cause);
    let reopened = Registry::new(temp.path().to_owned())?;
    reopened.submission(&resubmit)?;
    reopened.submission(&request(&id, None, 0))?;
    assert_eq!(reopened.audit_page(&app, None)?.events.len(), before + 1);
    assert!(
        reopened
            .submission(&request(&id, Some(s::SubmissionState::Publish), 1))
            .is_err()
    );
    let mut forged = request(&id, Some(s::SubmissionState::Publish), 3);
    *forged.last_mut().unwrap() ^= 1;
    assert!(reopened.submission(&forged).is_err());
    assert_eq!(reopened.audit_page(&app, None)?.events.len(), before + 1);
    // Repeated review/embargo and withdrawal requests also record fresh intent.
    for (state, sequence) in [
        (s::SubmissionState::Review, 3),
        (s::SubmissionState::Ignore, 5),
    ] {
        reopened.submission(&request(&id, Some(state), sequence))?;
        reopened.submission(&request(&id, Some(state), sequence + 1))?;
        let events = reopened.audit_page(&app, None)?.events;
        assert_eq!(events[0].action, "resubmitted");
        let count = events.len();
        reopened.submission(&request(&id, Some(state), sequence + 1))?;
        assert_eq!(reopened.audit_page(&app, None)?.events.len(), count);
    }
    Ok(())
}
