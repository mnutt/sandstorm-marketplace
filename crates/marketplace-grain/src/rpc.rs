use crate::{
    api_session_capnp::api_session,
    grain_capnp::{sandstorm_api, ui_session, ui_view},
    hack_session_capnp::hack_session_context,
    registry::{AdminSnapshot, Registry},
    util_capnp::byte_stream,
    web_session_capnp::web_session as web,
};
use anyhow::{Context, Result, bail, ensure};
use capnp::{
    capability::{FromClientHook, Promise},
    traits::HasTypeId,
};
use std::{cell::RefCell, io::Write, rc::Rc};

pub type Platform = sandstorm_api::Client<capnp::any_pointer::Owned>;
pub type PlatformSlot = Rc<RefCell<Option<Platform>>>;
pub fn cast<A: FromClientHook, B: FromClientHook>(client: A) -> B {
    B::new(client.into_client_hook())
}
pub fn error(error: impl std::fmt::Display) -> capnp::Error {
    capnp::Error::failed(error.to_string())
}
fn promise(result: Result<()>) -> Promise<(), capnp::Error> {
    Promise::from_future(async { result.map_err(error) })
}

pub struct View {
    pub registry: Registry,
    pub platform: PlatformSlot,
}
impl ui_view::Server for View {
    fn get_view_info(
        &mut self,
        _: ui_view::GetViewInfoParams,
        mut results: ui_view::GetViewInfoResults,
    ) -> Promise<(), capnp::Error> {
        promise((|| {
            results.set(crate::app_index_capnp::APP_INDEX_VIEW_INFO.get()?)?;
            Ok(())
        })())
    }
    fn new_session(
        &mut self,
        params: ui_view::NewSessionParams,
        mut results: ui_view::NewSessionResults,
    ) -> Promise<(), capnp::Error> {
        promise((|| {
            let params = params.get()?;
            let permissions = params.get_user_info()?.get_permissions()?;
            let has = |i| i < permissions.len() && permissions.get(i);
            let api = params.get_session_type() == api_session::Client::TYPE_ID;
            ensure!(
                api || params.get_session_type() == web::Client::TYPE_ID,
                "unsupported session type"
            );
            ensure!(
                if api { has(2) } else { has(1) },
                "session permission denied"
            );
            let session = Session {
                registry: self.registry.clone(),
                api,
                can_approve: has(0),
                context: cast(params.get_context()?),
                platform: self.platform.clone(),
            };
            let client: api_session::Client = capnp_rpc::new_client(session);
            results.get().set_session(cast(client));
            Ok(())
        })())
    }
}
struct Session {
    registry: Registry,
    api: bool,
    can_approve: bool,
    context: hack_session_context::Client,
    platform: PlatformSlot,
}
impl ui_session::Server for Session {}
impl api_session::Server for Session {}
impl web::Server for Session {
    fn get(
        &mut self,
        params: web::GetParams,
        mut results: web::GetResults,
    ) -> Promise<(), capnp::Error> {
        let registry = self.registry.clone();
        let context = self.context.clone();
        let api = self.api;
        let can_approve = self.can_approve;
        Promise::from_future(async move {
            let request_path = params
                .get()
                .and_then(|p| p.get_path())
                .ok()
                .and_then(|p| p.to_str().ok())
                .unwrap_or("<invalid>")
                .to_owned();
            let action: Result<()> = async {
                ensure!(!api, "submission API does not expose administration");
                let path = params.get()?.get_path()?.to_str()?;
                match path {
                    "" => content(results.get(), "text/html; charset=utf-8", include_bytes!("review.html")),
                    "admin" => {
                        let snapshot = AdminSnapshot {
                            releases: registry.queue()?,
                            can_approve,
                            busy: registry.is_busy(),
                        };
                        content(results.get(), "application/json", &serde_json::to_vec(&snapshot)?);
                    }
                    "review.css" => content(results.get(), "text/css; charset=utf-8", include_bytes!("review.css")),
                    path if path.starts_with("assets/") => {
                        match marketplace_web::static_asset(&path[7..]) {
                            Some((mime, bytes)) => content(results.get(), mime, bytes),
                            None => not_found(results.get()),
                        }
                    }
                    "marketplace.js" => content(results.get(), "text/javascript; charset=utf-8", marketplace_web::marketplace_script()),
                    "preview.js" => content(results.get(), "text/javascript; charset=utf-8", include_bytes!("preview.js")),
                    path if path.starts_with("preview/") => content(results.get(), "text/html; charset=utf-8", registry.preview(&path[8..])?.as_bytes()),
                    "review.js" => content(results.get(), "text/javascript; charset=utf-8", include_bytes!("review.js")),
                    path if path.starts_with("download/") => {
                        let id = &path[9..];
                        let file_path = registry.package_dir(id)?.join("spk");
                        let file = tokio::fs::File::open(&file_path).await
                            .with_context(|| format!("opening submitted package {}", file_path.display()))?;
                        let size = file.metadata().await?.len();
                        let stream = params.get()?.get_context()?.get_response_stream()?;
                        let task = tokio::task::spawn_local(async move {
                            if let Err(err) = stream_package(file, size, stream).await {
                                tracing::warn!(error = ?err, "Package download failed");
                            }
                        });
                        let mut response = results.get().init_content();
                        response.set_mime_type("application/octet-stream");
                        response.reborrow().init_disposition().set_download(&format!("{id}.spk"));
                        response.init_body().set_stream(capnp_rpc::new_client(Download(task)));
                    }
                    "public-id" => {
                        let response = context.get_public_id_request().send().promise.await?; let response = response.get()?;
                        let body = serde_json::json!({"publicId": response.get_public_id()?.to_str()?, "hostname": response.get_hostname()?.to_str()?, "autoUrl": response.get_auto_url()?.to_str()?, "isDemoUser": response.get_is_demo_user()});
                        content(results.get(), "application/json", &serde_json::to_vec(&body)?);
                    }
                    "keybase-pb-descriptor" => content(results.get(), "text/plain", crate::keybase::descriptor()?.as_bytes()),
                    _ => not_found(results.get()),
                }
                Ok(())
            }.await;
            if let Err(err) = action {
                failure(
                    results.get(),
                    &err.context(format!("request {request_path}")),
                );
            }
            Ok(())
        })
    }
    fn post(
        &mut self,
        params: web::PostParams,
        mut results: web::PostResults,
    ) -> Promise<(), capnp::Error> {
        let registry = self.registry.clone();
        let api = self.api;
        let can_approve = self.can_approve;
        let context = self.context.clone();
        let platform = self.platform.clone();
        Promise::from_future(async move {
            let request_path = params
                .get()
                .and_then(|p| p.get_path())
                .ok()
                .and_then(|p| p.to_str().ok())
                .unwrap_or("<invalid>")
                .to_owned();
            let action: Result<()> = async {
                let params = params.get()?;
                let path = params.get_path()?.to_str()?;
                let data = params.get_content()?;
                ensure!(
                    !data.has_encoding() || data.get_encoding()?.is_empty(),
                    "encoded requests not accepted"
                );
                let body = data.get_content()?;
                let operation = registry.begin_operation()?;
                if api {
                    match path {
                        "upload" => {
                            ensure!(
                                body.len() as u64 <= crate::spk::SIZE_LIMIT,
                                "upload too large"
                            );
                            let mut temp = registry.temporary()?;
                            temp.write_all(body)?;
                            crate::work::upload(
                                registry.clone(),
                                platform.clone(),
                                operation,
                                temp,
                                false,
                            )
                            .await?;
                            results.get().init_no_content();
                        }
                        "status" => content(
                            results.get(),
                            "application/octet-stream",
                            &registry.submission(body)?,
                        ),
                        _ => not_found(results.get()),
                    }
                } else {
                    ensure!(can_approve, "approval permission denied");
                    if path == "import" {
                        ensure!(
                            registry.package_ids()?.is_empty(),
                            "import is only available in an empty grain"
                        );
                        ensure!(
                            body.len() as u64 <= crate::import::IMPORT_LIMIT,
                            "import too large"
                        );
                        let mut temp = registry.temporary()?;
                        temp.write_all(body)?;
                        crate::work::upload(
                            registry.clone(),
                            platform.clone(),
                            operation,
                            temp,
                            true,
                        )
                        .await?;
                        results.get().init_no_content();
                    } else if path == "reindex" {
                        registry.publish()?;
                        results.get().init_no_content();
                    } else if path == "keybase-pb-token" {
                        crate::keybase::claim(
                            &registry,
                            &context,
                            &platform,
                            std::str::from_utf8(body)?,
                        )
                        .await?;
                        results.get().init_no_content();
                    } else if let Some(fingerprint) = path.strip_prefix("keybase/") {
                        let profile =
                            crate::keybase::lookup(&registry, &platform, fingerprint).await?;
                        content(
                            results.get(),
                            "application/json",
                            &serde_json::to_vec(&profile)?,
                        );
                    } else if let Some((action, id)) = path.split_once('/') {
                        registry.review(id, action, std::str::from_utf8(body)?)?;
                        results.get().init_no_content();
                    } else {
                        not_found(results.get());
                    }
                }
                Ok(())
            }
            .await;
            if let Err(err) = action {
                failure(
                    results.get(),
                    &err.context(format!("request {request_path}")),
                );
            }
            Ok(())
        })
    }
    fn post_streaming(
        &mut self,
        params: web::PostStreamingParams,
        mut results: web::PostStreamingResults,
    ) -> Promise<(), capnp::Error> {
        let result: Result<()> = (|| {
            let params = params.get()?;
            let path = params.get_path()?.to_str()?;
            let import = !self.api && path == "import";
            if import {
                ensure!(self.can_approve, "approval permission denied");
                ensure!(
                    self.registry.package_ids()?.is_empty(),
                    "import is only available in an empty grain"
                );
            } else if !self.api || path != "upload" {
                bail!("unsupported streaming endpoint");
            }
            let limit = if import {
                crate::import::IMPORT_LIMIT
            } else {
                crate::spk::SIZE_LIMIT
            };
            ensure!(
                !params.has_encoding() || params.get_encoding()?.is_empty(),
                "encoded uploads not accepted"
            );
            let size = params.get_expected_size();
            ensure!(size <= limit, "upload too large");
            tracing::info!(import, expected_bytes = size, "Upload stream opened");
            let (sender, receiver) = futures::channel::oneshot::channel();
            let stream: web::request_stream::Client = capnp_rpc::new_client(Upload {
                registry: self.registry.clone(),
                platform: self.platform.clone(),
                file: Some(self.registry.temporary()?),
                count: 0,
                import,
                limit,
                started: std::time::Instant::now(),
                last_progress: std::time::Instant::now(),
                expected: (size != 0).then_some(size),
                sender: Some(sender),
                receiver: Some(receiver),
            });
            results.get().set_stream(stream);
            Ok(())
        })();
        // Sandstorm retries /status as a buffered post when streaming isn't supported.
        match result {
            Ok(()) => Promise::ok(()),
            Err(err) => Promise::err(capnp::Error::unimplemented(err.to_string())),
        }
    }
}
struct Upload {
    registry: Registry,
    platform: PlatformSlot,
    file: Option<tempfile::NamedTempFile>,
    count: u64,
    import: bool,
    limit: u64,
    started: std::time::Instant,
    last_progress: std::time::Instant,
    expected: Option<u64>,
    sender: Option<futures::channel::oneshot::Sender<std::result::Result<(), String>>>,
    receiver: Option<futures::channel::oneshot::Receiver<std::result::Result<(), String>>>,
}
impl byte_stream::Server for Upload {
    fn write(&mut self, params: byte_stream::WriteParams) -> Promise<(), capnp::Error> {
        promise((|| {
            let params = params.get()?;
            let data = params.get_data()?;
            let count = self
                .count
                .checked_add(data.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("upload too large"))?;
            ensure!(
                count <= self.expected.unwrap_or(self.limit),
                "upload exceeds expected size"
            );
            self.file
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("upload already finished"))?
                .write_all(data)?;
            self.count = count;
            if self.last_progress.elapsed() >= std::time::Duration::from_secs(5) {
                tracing::info!(import = self.import, bytes = count, expected_bytes = ?self.expected, elapsed_secs = self.started.elapsed().as_secs(), "Upload progress");
                self.last_progress = std::time::Instant::now();
            }
            Ok(())
        })())
    }
    fn expect_size(
        &mut self,
        params: byte_stream::ExpectSizeParams,
        _: byte_stream::ExpectSizeResults,
    ) -> Promise<(), capnp::Error> {
        promise((|| {
            let total = self
                .count
                .checked_add(params.get()?.get_size())
                .ok_or_else(|| anyhow::anyhow!("upload too large"))?;
            ensure!(
                total <= self.limit && self.expected.is_none_or(|v| v == total),
                "invalid expected size"
            );
            self.expected = Some(total);
            Ok(())
        })())
    }
    fn done(
        &mut self,
        _: byte_stream::DoneParams,
        _: byte_stream::DoneResults,
    ) -> Promise<(), capnp::Error> {
        let file = self.file.take();
        let registry = self.registry.clone();
        let platform = self.platform.clone();
        let import = self.import;
        let expected = self.expected;
        let count = self.count;
        let started = self.started;
        let sender = self.sender.take();
        Promise::from_future(async move {
            let result: Result<()> = async {
                let file = file.ok_or_else(|| anyhow::anyhow!("upload already finished"))?;
                ensure!(expected.is_none_or(|v| v == count), "incomplete upload");
                tracing::info!(
                    import,
                    bytes = count,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Upload received; processing"
                );
                let operation = registry.begin_operation()?;
                crate::work::upload(registry, platform, operation, file, import).await?;
                Ok(())
            }
            .await;
            if let Some(sender) = sender {
                let _ = sender.send(result.as_ref().map(|_| ()).map_err(|e| e.to_string()));
            }
            match &result {
                Ok(()) => tracing::info!(
                    import,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Upload processing complete"
                ),
                Err(err) => {
                    tracing::warn!(import, error = ?err, "Upload processing failed")
                }
            }
            result.map_err(error)
        })
    }
}
impl web::request_stream::Server for Upload {
    fn get_response(
        &mut self,
        _: web::request_stream::GetResponseParams,
        mut results: web::request_stream::GetResponseResults,
    ) -> Promise<(), capnp::Error> {
        let receiver = self.receiver.take();
        Promise::from_future(async move {
            let result = receiver
                .ok_or_else(|| error("getResponse already called"))?
                .await
                .map_err(error)?;
            match result {
                Ok(()) => {
                    results.get().init_no_content();
                }
                Err(err) => failure(results.get(), &anyhow::anyhow!(err)),
            }
            Ok(())
        })
    }
}
pub fn content(mut response: web::response::Builder<'_>, mime: &str, bytes: &[u8]) {
    let mut content = response.reborrow().init_content();
    content.set_mime_type(mime);
    content.init_body().set_bytes(bytes);
}
fn failure(mut response: web::response::Builder<'_>, err: &anyhow::Error) {
    tracing::warn!(error = ?err, "Request failed");
    let mut output = response.reborrow().init_client_error();
    output.set_status_code(web::response::ClientErrorCode::BadRequest);
    output.set_description_html(
        format!("{err:#}")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .as_str(),
    );
}
fn not_found(mut response: web::response::Builder<'_>) {
    let mut output = response.reborrow().init_client_error();
    output.set_status_code(web::response::ClientErrorCode::NotFound);
    output.set_description_html("Not found");
}

// Dropping the response handle cancels the transfer and closes the file.
struct Download(tokio::task::JoinHandle<()>);
impl crate::util_capnp::handle::Server for Download {}
impl Drop for Download {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn stream_package(
    mut file: tokio::fs::File,
    size: u64,
    stream: byte_stream::Client,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut expected = stream.expect_size_request();
    expected.get().set_size(size);
    expected.send().promise.await?;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let mut write = stream.write_request();
        write.get().set_data(&buffer[..count]);
        write.send().await?;
    }
    stream.done_request().send().promise.await?;
    Ok(())
}
