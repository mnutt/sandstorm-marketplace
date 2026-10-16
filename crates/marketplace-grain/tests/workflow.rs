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
            &mut self,
            _: marketplace_grain::grain_capnp::sandstorm_api::StayAwakeParams<
                capnp::any_pointer::Owned,
            >,
            mut results: marketplace_grain::grain_capnp::sandstorm_api::StayAwakeResults<
                capnp::any_pointer::Owned,
            >,
        ) -> capnp::capability::Promise<(), capnp::Error> {
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
                Some(seed_legacy_invalid(&registry, 99)?)
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
        archive.append_path_with_name(source.path().join("catalog").join(name), name)?;
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
    assert!(registry.import_catalog(valid.path()).is_err());
    let reopened = Registry::new(dir.path().to_owned())?;
    reopened.publish()?;
    assert_eq!(reopened.package_ids()?.len(), 2);
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
            &mut self,
            params: byte_stream::ExpectSizeParams,
            _: byte_stream::ExpectSizeResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            self.expected.set(params.get().unwrap().get_size());
            capnp::capability::Promise::ok(())
        }
        fn write(
            &mut self,
            params: byte_stream::WriteParams,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            self.bytes
                .borrow_mut()
                .extend_from_slice(params.get().unwrap().get_data().unwrap());
            capnp::capability::Promise::ok(())
        }
        fn done(
            &mut self,
            _: byte_stream::DoneParams,
            _: byte_stream::DoneResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
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
                (&reviewer, "download/../../keybase-token".to_owned()),
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
fn invalid_metadata_is_rejected_before_commit_and_legacy_entries_remain_reviewable()
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
    let bad = seed_legacy_invalid(&registry, 1)?;
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
