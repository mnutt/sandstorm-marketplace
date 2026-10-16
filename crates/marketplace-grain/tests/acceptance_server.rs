//! Local-only HTTP adapter for browser acceptance tests. All admin operations go
//! through the production WebSession RPC implementation and a disposable registry.
//! This is an ignored test binary, never part of the packaged application.
mod common;
use anyhow::{Result, bail};
use axum::{
    Router,
    body::{Bytes, to_bytes},
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use capnp::capability::Promise;
use marketplace_grain::{
    grain_capnp::ui_view, registry::Registry, rpc::View, submit_capnp::SubmissionState,
    util_capnp::byte_stream, web_session_capnp::web_session as web,
};
use std::{
    cell::{Cell, RefCell},
    io::Write,
    rc::Rc,
};
use tokio::sync::{mpsc, oneshot};

struct Incoming {
    method: String,
    path: String,
    body: Bytes,
    reply: oneshot::Sender<Response>,
}
async fn http(State(sender): State<mpsc::Sender<Incoming>>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let body = match to_bytes(request.into_body(), 16 * 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => return response(400, "text/plain", err.to_string().into_bytes(), None),
    };
    let (reply, receive) = oneshot::channel();
    if sender
        .send(Incoming {
            method,
            path,
            body,
            reply,
        })
        .await
        .is_err()
    {
        return response(503, "text/plain", b"test server stopped".to_vec(), None);
    }
    receive
        .await
        .unwrap_or_else(|_| response(503, "text/plain", b"test server stopped".to_vec(), None))
}
fn response(status: u16, mime: &str, body: Vec<u8>, filename: Option<String>) -> Response {
    let mut response = (axum::http::StatusCode::from_u16(status).unwrap(), body).into_response();
    response
        .headers_mut()
        .insert("content-type", mime.parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    if let Some(name) = filename {
        response.headers_mut().insert(
            "content-disposition",
            format!("attachment; filename=\"{name}\"").parse().unwrap(),
        );
    }
    response
}
#[derive(Clone, Default)]
struct Collector {
    bytes: Rc<RefCell<Vec<u8>>>,
    done: Rc<Cell<bool>>,
}
impl byte_stream::Server for Collector {
    fn expect_size(
        &mut self,
        _: byte_stream::ExpectSizeParams,
        _: byte_stream::ExpectSizeResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }
    fn write(&mut self, params: byte_stream::WriteParams) -> Promise<(), capnp::Error> {
        self.bytes
            .borrow_mut()
            .extend_from_slice(params.get().unwrap().get_data().unwrap());
        Promise::ok(())
    }
    fn done(
        &mut self,
        _: byte_stream::DoneParams,
        _: byte_stream::DoneResults,
    ) -> Promise<(), capnp::Error> {
        self.done.set(true);
        Promise::ok(())
    }
}
struct Fixture {
    _storage: tempfile::TempDir,
    registry: Registry,
    admin: web::Client,
    reviewer: web::Client,
    submitter: web::Client,
    fail_queue: bool,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let storage = tempfile::tempdir()?;
        let registry = Registry::new(storage.path().to_owned())?;
        registry.publish()?;
        let view: ui_view::Client = capnp_rpc::new_client(View {
            registry: registry.clone(),
            platform: Rc::new(RefCell::new(None)),
        });
        let mut fixture = Self {
            admin: common::session(&view, &[true, true, false], false).await?,
            reviewer: common::session(&view, &[false, true, false], false).await?,
            submitter: common::session(&view, &[false, false, true], true).await?,
            _storage: storage,
            registry,
            fail_queue: false,
        };
        for version in 1..=3 {
            let id = fixture.submit(version, false).await?;
            if version < 3 {
                common::post(&fixture.admin, &format!("approve/{id}"), b"").await?;
            }
        }
        fixture.submit(4, true).await?;
        Ok(fixture)
    }
    async fn submit(&mut self, version: u32, embargo: bool) -> Result<String> {
        let data = common::package(version);
        let uploaded = common::post(&self.submitter, "upload", &data).await?;
        if !matches!(uploaded.get()?.which()?, web::response::NoContent(_)) {
            bail!("fixture upload failed");
        }
        let mut temp = self.registry.temporary()?;
        temp.write_all(&data)?;
        let id = self.registry.upload(temp.path())?;
        let state = if embargo {
            SubmissionState::Review
        } else {
            SubmissionState::Publish
        };
        common::post(
            &self.submitter,
            "status",
            &common::request(&id, Some(state), 1),
        )
        .await?;
        Ok(id)
    }
    async fn serve(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
        port: u16,
    ) -> Result<Response> {
        if path == "/health" {
            return Ok(response(200, "text/plain", b"ok".to_vec(), None));
        }
        if path == "/__test/malformed.spk" {
            return Ok(response(
                200,
                "application/octet-stream",
                common::package_with_description(5, true),
                None,
            ));
        }
        // The Sandstorm shell owns installation; verify the browser opens its
        // expected URL without implementing an installer in the test adapter.
        if path.starts_with("/install/") || path == "/apps" {
            return Ok(response(
                200,
                "text/html",
                b"Sandstorm installer (platform stub)".to_vec(),
                None,
            ));
        }
        if method == "POST" {
            match path {
                "/__test/reset" => {
                    *self = Self::new().await?;
                    return Ok(response(204, "text/plain", vec![], None));
                }
                "/__test/submit" => {
                    let version: u32 = std::str::from_utf8(body)?.parse()?;
                    let id = self.submit(version, false).await?;
                    return Ok(response(200, "text/plain", id.into_bytes(), None));
                }
                "/__test/new-app" => {
                    let key = ed25519_dalek::SigningKey::from_bytes(&[43; 32]);
                    let bytes = common::package_with_key(1, false, &key, "Unreviewed app");
                    let mut file = self.registry.temporary()?;
                    file.write_all(&bytes)?;
                    let id = self.registry.upload(file.path())?;
                    self.registry.set_state(&id, SubmissionState::Publish, 1)?;
                    self.registry.publish()?;
                    return Ok(response(200, "text/plain", id.into_bytes(), None));
                }
                "/__test/fail-next-queue" => {
                    self.fail_queue = true;
                    return Ok(response(204, "text/plain", vec![], None));
                }
                "/__test/legacy-invalid" => {
                    let id = common::seed_legacy_invalid(&self.registry, 99)?;
                    return Ok(response(200, "text/plain", id.into_bytes(), None));
                }
                _ => {}
            }
        }
        if let Some(path) = path.strip_prefix("/public/").or_else(|| {
            path.strip_prefix('/').filter(|p| {
                !["admin/", "reviewer/", "submitter/"]
                    .iter()
                    .any(|prefix| p.starts_with(prefix))
            })
        }) {
            let path = if path.is_empty() { "index.html" } else { path };
            if path.split('/').any(|part| part == ".." || part == ".") {
                bail!("invalid path");
            }
            let file = self.registry.root.join("www").join(path);
            let file = if file.is_dir() {
                file.join("index.html")
            } else {
                file
            };
            let mime = match file.extension().and_then(|ext| ext.to_str()) {
                Some("json") => "application/json",
                Some("js") => "text/javascript",
                Some("css") => "text/css",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("woff2") => "font/woff2",
                _ => "text/html",
            };
            return Ok(match std::fs::read(file) {
                Ok(bytes) => response(200, mime, bytes, None),
                Err(_) => response(404, "text/plain", b"not found".to_vec(), None),
            });
        }
        let (client, path) = if let Some(path) = path.strip_prefix("/admin/") {
            (&self.admin, path)
        } else if let Some(path) = path.strip_prefix("/reviewer/") {
            (&self.reviewer, path)
        } else if let Some(path) = path.strip_prefix("/submitter/") {
            (&self.submitter, path)
        } else {
            return Ok(response(404, "text/plain", vec![], None));
        };
        // Sandstorm provides this platform URL; only this platform service is stubbed.
        if path == "public-id" {
            return Ok(response(
                200,
                "application/json",
                serde_json::to_vec(
                    &serde_json::json!({"autoUrl": format!("http://127.0.0.1:{port}/public/")}),
                )?,
                None,
            ));
        }
        if path == "admin" && self.fail_queue {
            self.fail_queue = false;
            return Ok(response(
                503,
                "text/plain",
                b"Injected queue failure".to_vec(),
                None,
            ));
        }
        let collector = Collector::default();
        let result = if method == "GET" {
            let mut request = client.get_request();
            request.get().set_path(path);
            request
                .get()
                .init_context()
                .set_response_stream(capnp_rpc::new_client(collector.clone()));
            request.send().promise.await?
        } else {
            common::post(client, path, body).await?
        };
        match result.get()?.which()? {
            web::response::Content(content) => {
                let mime = content.get_mime_type()?.to_str()?.to_owned();
                let filename = match content.get_disposition().which()? {
                    web::response::content::disposition::Download(name) => {
                        Some(name?.to_str()?.to_owned())
                    }
                    _ => None,
                };
                let bytes = match content.get_body().which()? {
                    web::response::content::body::Bytes(bytes) => bytes?.to_vec(),
                    web::response::content::body::Stream(handle) => {
                        let _handle = handle?;
                        tokio::time::timeout(std::time::Duration::from_secs(10), async {
                            while !collector.done.get() {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await?;
                        collector.bytes.borrow().clone()
                    }
                };
                Ok(response(200, &mime, bytes, filename))
            }
            web::response::NoContent(_) => Ok(response(204, "text/plain", vec![], None)),
            web::response::ClientError(error) => Ok(response(
                400,
                "text/plain",
                error.get_description_html()?.as_bytes().to_vec(),
                None,
            )),
            _ => bail!("unsupported test response"),
        }
    }
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "HTTP adapter for Playwright; run through npm run test:acceptance"]
async fn serve() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let port = std::env::var("MARKETPLACE_TEST_PORT")
                .unwrap_or_else(|_| "9940".into())
                .parse::<u16>()?;
            let (sender, mut requests) = mpsc::channel::<Incoming>(32);
            let listener =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
            let server = tokio::spawn(async move {
                axum::serve(listener, Router::new().fallback(http).with_state(sender)).await
            });
            let mut fixture = Fixture::new().await?;
            println!("Acceptance server ready on http://127.0.0.1:{port}");
            while let Some(request) = requests.recv().await {
                let result = fixture
                    .serve(&request.method, &request.path, &request.body, port)
                    .await;
                let output = result.unwrap_or_else(|err| {
                    response(500, "text/plain", format!("{err:#}").into_bytes(), None)
                });
                let _ = request.reply.send(output);
            }
            server.abort();
            Ok(())
        })
        .await
}
