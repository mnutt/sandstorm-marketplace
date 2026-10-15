//! Opt-in tests driven by `cargo xtask test-interop` with real SDK-generated SPKs.
use anyhow::{Result, ensure};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
};
use marketplace_grain::registry::Registry;
use std::{path::PathBuf, sync::Arc};

#[tokio::test(flavor = "current_thread")]
async fn real_spk_publish_client() -> Result<()> {
    let Some(fixture) = std::env::var_os("MARKETPLACE_INTEROP_FIXTURE") else {
        return Ok(());
    };
    let sdk = std::env::var_os("SANDSTORM_SPK")
        .ok_or_else(|| anyhow::anyhow!("missing SANDSTORM_SPK"))?;
    let fixture = PathBuf::from(fixture);
    let temp = tempfile::tempdir()?;
    let registry = Arc::new(Registry::new(temp.path().to_owned())?);
    registry.publish()?;
    let app = Router::new()
        .route(
            "/upload",
            post(
                |State(registry): State<Arc<Registry>>, bytes: Bytes| async move {
                    let result: Result<()> = (|| {
                        let mut file = registry.temporary()?;
                        std::io::Write::write_all(&mut file, &bytes)?;
                        registry.upload(file.path())?;
                        Ok(())
                    })();
                    match result {
                        Ok(()) => (StatusCode::NO_CONTENT, String::new()),
                        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()),
                    }
                },
            ),
        )
        .route(
            "/status",
            post(
                |State(registry): State<Arc<Registry>>, bytes: Bytes| async move {
                    match registry.submission(&bytes) {
                        Ok(body) => (StatusCode::OK, body),
                        Err(e) => (StatusCode::BAD_REQUEST, e.to_string().into_bytes()),
                    }
                },
            ),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(registry.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let webkey = format!("http://{}#interop-test", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let run = |flag: Option<&str>| {
        let mut command = tokio::process::Command::new(&sdk);
        command
            .arg("publish")
            .arg("-k")
            .arg(fixture.join("keyring"))
            .arg("--webkey")
            .arg(&webkey);
        if let Some(flag) = flag {
            command.arg(flag);
        }
        command.arg(fixture.join("test.spk"));
        async move {
            let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
                .await??;
            ensure!(
                output.status.success(),
                "spk publish failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Ok::<_, anyhow::Error>(())
        }
    };
    run(Some("--embargo")).await?;
    let id = registry.package_ids()?.remove(0);
    registry.review(&id, "approve", "")?;
    assert!(!temp.path().join("www/packages").join(&id).exists());
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    run(None).await?;
    assert!(temp.path().join("www/packages").join(&id).exists());
    run(Some("--status")).await?;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    run(Some("--remove")).await?;
    assert!(!temp.path().join("www/packages").join(&id).exists());
    server.abort();
    Ok(())
}
