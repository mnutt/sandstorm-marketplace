use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use marketplace_model::app_index::{AppDetailsForMarket, AppIndexForMarket, LoadedAppIndex};
use reqwest::Client;
use tokio::fs;

#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    SyncRemote {
        #[arg(long, default_value = "https://app-index.sandstorm.io")]
        base_url: String,
        #[arg(long, default_value = "data/app-index")]
        output_dir: PathBuf,
        #[arg(long, default_value_t = true)]
        include_experimental: bool,
        /// Also download referenced SPKs and build an empty-grain import archive.
        #[arg(long)]
        tarball: Option<PathBuf>,
        /// Maximum total catalog releases; approved first, then experimental.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        limit: Option<u32>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,marketplace_sync=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::SyncRemote {
            base_url,
            output_dir,
            include_experimental,
            tarball,
            limit,
        } => {
            sync_remote(&base_url, &output_dir, include_experimental, limit).await?;
            if let Some(path) = tarball {
                create_import(&base_url, &output_dir, include_experimental, &path).await?;
            }
        }
    }
    Ok(())
}

fn limit_index(index: &mut AppIndexForMarket, remaining: &mut Option<usize>) {
    index
        .apps
        .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.app_id.cmp(&b.app_id)));
    if let Some(remaining) = remaining {
        index.apps.truncate(*remaining);
        *remaining -= index.apps.len();
    }
}

#[tracing::instrument(skip_all, err(Debug))]
async fn sync_remote(
    base_url: &str,
    output_dir: &Path,
    include_experimental: bool,
    limit: Option<u32>,
) -> Result<()> {
    let started = std::time::Instant::now();
    tracing::info!(?limit, include_experimental, "Starting catalog sync");
    let client = Client::builder()
        .user_agent("sandstorm-marketplace-sync/0.1")
        .build()?;

    fs::create_dir_all(output_dir.join("apps")).await?;
    fs::create_dir_all(output_dir.join("images")).await?;
    fs::create_dir_all(output_dir.join("packages")).await?;
    if include_experimental {
        fs::create_dir_all(output_dir.join("experimental")).await?;
    }

    let index_url = format!("{}/apps/index.json", base_url.trim_end_matches('/'));
    let index_bytes = fetch_bytes(&client, &index_url).await?;
    let mut remaining = limit.map(|n| n as usize);
    let mut index: AppIndexForMarket = serde_json::from_slice(&index_bytes)?;
    limit_index(&mut index, &mut remaining);
    tracing::info!(
        apps = index.apps.len(),
        "Approved index fetched and selected"
    );
    fs::write(
        output_dir.join("apps/index.json"),
        serde_json::to_vec(&index)?,
    )
    .await?;

    let loaded = LoadedAppIndex::load(output_dir)?;
    let mut image_ids: BTreeSet<String> = loaded
        .index
        .image_ids()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();

    for (position, app) in loaded.index.apps.iter().enumerate() {
        tracing::info!(current = position + 1, total = loaded.index.apps.len(), app_id = %app.app_id, "Fetching app details");
        let details_url = format!(
            "{}/apps/{}.json",
            base_url.trim_end_matches('/'),
            app.app_id
        );
        let details_bytes = fetch_bytes(&client, &details_url).await?;
        let details_path = output_dir.join("apps").join(format!("{}.json", app.app_id));
        fs::write(details_path, &details_bytes).await?;

        let details: AppDetailsForMarket = serde_json::from_slice(&details_bytes)?;
        image_ids.extend(details.image_ids().map(ToOwned::to_owned));
    }

    if include_experimental {
        let experimental_url =
            format!("{}/experimental/index.json", base_url.trim_end_matches('/'));
        let mut experimental: AppIndexForMarket = if remaining == Some(0) {
            AppIndexForMarket { apps: Vec::new() }
        } else {
            serde_json::from_slice(&fetch_bytes(&client, &experimental_url).await?)?
        };
        limit_index(&mut experimental, &mut remaining);
        tracing::info!(
            apps = experimental.apps.len(),
            "Experimental index selected"
        );
        fs::write(
            output_dir.join("experimental/index.json"),
            serde_json::to_vec(&experimental)?,
        )
        .await?;
        image_ids.extend(experimental.image_ids().into_iter().map(ToOwned::to_owned));
        for app in &experimental.apps {
            let bytes = fetch_bytes(
                &client,
                &format!(
                    "{}/experimental/{}.json",
                    base_url.trim_end_matches('/'),
                    app.app_id
                ),
            )
            .await?;
            let details: AppDetailsForMarket = serde_json::from_slice(&bytes)?;
            image_ids.extend(details.image_ids().map(ToOwned::to_owned));
            fs::write(
                output_dir
                    .join("experimental")
                    .join(format!("{}.json", app.app_id)),
                bytes,
            )
            .await?;
        }
    }

    for (position, image_id) in image_ids.iter().enumerate() {
        tracing::info!(
            current = position + 1,
            total = image_ids.len(),
            image_id,
            "Fetching image"
        );
        let image_url = format!("{}/images/{}", base_url.trim_end_matches('/'), image_id);
        let image_bytes = fetch_bytes(&client, &image_url).await?;
        fs::write(output_dir.join("images").join(image_id), image_bytes).await?;
    }

    tracing::info!(apps = loaded.index.apps.len(), images = image_ids.len(), output = %output_dir.display(), elapsed_ms = started.elapsed().as_millis() as u64, "Catalog sync complete");

    Ok(())
}

async fn fetch_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}

#[tracing::instrument(skip_all, err(Debug))]
async fn create_import(
    base_url: &str,
    catalog: &Path,
    experimental: bool,
    destination: &Path,
) -> Result<()> {
    let started = std::time::Instant::now();
    tracing::info!(output = %destination.display(), "Building import tarball");
    use marketplace_model::app_index::AppIndexForMarket;
    use std::io::Write;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).await?;
    let output = tempfile::NamedTempFile::new_in(parent)?;
    let mut archive = tar::Builder::new(output);
    let mut ids = BTreeSet::new();
    for name in ["apps/index.json", "experimental/index.json"] {
        if name.starts_with("experimental/") && !experimental {
            continue;
        }
        let path = catalog.join(name);
        let bytes = fs::read(&path).await?;
        let index: AppIndexForMarket = serde_json::from_slice(&bytes)?;
        for app in index.apps {
            ensure!(
                app.package_id.len() == 32
                    && app
                        .package_id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid package ID in catalog"
            );
            ensure!(ids.insert(app.package_id), "duplicate package in catalog");
        }
        archive.append_path_with_name(path, name)?;
    }
    ensure!(!ids.is_empty(), "catalog contains no packages");
    let client = Client::builder()
        .user_agent("sandstorm-marketplace-sync/0.1")
        .build()?;
    let mut total = 0u64;
    for (number, id) in ids.iter().enumerate() {
        let download_started = std::time::Instant::now();
        let mut last_progress = download_started;
        tracing::info!(current = number + 1, total = ids.len(), package_id = %id, "Downloading SPK");
        let mut response = client
            .get(format!("{}/packages/{id}", base_url.trim_end_matches('/')))
            .send()
            .await?
            .error_for_status()?;
        let expected_bytes = response.content_length();
        tracing::info!(package_id = %id, ?expected_bytes, "SPK response received");
        let mut file = tempfile::tempfile()?;
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await? {
            size += chunk.len() as u64;
            total += chunk.len() as u64;
            ensure!(size <= 1024 * 1024 * 1024, "SPK exceeds 1 GiB");
            ensure!(
                total < 16 * 1024 * 1024 * 1024 - 64 * 1024 * 1024,
                "import exceeds 16 GiB limit"
            );
            file.write_all(&chunk)?;
            if last_progress.elapsed() >= std::time::Duration::from_secs(5) {
                tracing::info!(package_id = %id, bytes = size, ?expected_bytes, elapsed_secs = download_started.elapsed().as_secs(), "SPK download progress");
                last_progress = std::time::Instant::now();
            }
        }
        std::io::Seek::rewind(&mut file)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, format!("packages/{id}"), file)?;
        tracing::info!(package_id = %id, bytes = size, elapsed_ms = download_started.elapsed().as_millis() as u64, "SPK downloaded and archived");
    }
    tracing::info!(bytes = total, "Finalizing tarball");
    let output = archive.into_inner()?;
    output.as_file().sync_all()?;
    output.persist(destination)?;
    tracing::info!(output = %destination.display(), packages = ids.len(), bytes = total, elapsed_ms = started.elapsed().as_millis() as u64, "Import tarball ready to upload");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[tokio::test]
    async fn limited_sync_only_fetches_selected_details() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = |id: &str| serde_json::json!({"appId": id, "name": id, "version": "1", "versionNumber": 1, "packageId": "0123456789abcdef0123456789abcdef", "isOpenSource": true, "author": null});
        let index = serde_json::json!({"apps": [app("z"), app("a"), app("b")]});
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let server = std::thread::spawn(move || {
            for (path, body) in [
                ("/apps/index.json", index.to_string()),
                ("/apps/a.json", "{}".to_owned()),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut request = [0; 4096];
                let n = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..n]).starts_with(&format!("GET {path} "))
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        sync_remote(&url, dir.path(), true, Some(1)).await?;
        server.join().unwrap();
        let approved = LoadedAppIndex::load(dir.path())?;
        assert_eq!(approved.index.apps.len(), 1);
        assert_eq!(approved.index.apps[0].app_id, "a");
        let experimental: AppIndexForMarket =
            serde_json::from_slice(&fs::read(dir.path().join("experimental/index.json")).await?)?;
        assert!(experimental.apps.is_empty());
        // The limit is shared across indexes, not applied separately to each.
        let mut remaining = Some(2);
        let mut main = approved.index.clone();
        limit_index(&mut main, &mut remaining);
        let mut pending = approved.index;
        limit_index(&mut pending, &mut remaining);
        assert_eq!(pending.apps.len(), 1);
        assert_eq!(remaining, Some(0));
        assert!(Cli::try_parse_from(["sync", "sync-remote", "--limit=0"]).is_err());
        assert!(Cli::try_parse_from(["sync", "sync-remote", "--limit=10"]).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn mirror_fetches_pending_descriptions_and_images() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = |version, image| serde_json::json!({"appId":"test", "name":"Test", "version":"1", "versionNumber":version, "packageId":format!("{version:032x}"), "isOpenSource":true, "author":null, "imageId": image});
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let responses = [
            ("/apps/index.json", serde_json::json!({"apps":[app(1, "")]}).to_string()),
            ("/apps/test.json", "{}".into()),
            ("/experimental/index.json", serde_json::json!({"apps":[app(2, "beta.svg")]}).to_string()),
            ("/experimental/test.json", serde_json::json!({"description":"Pending description", "screenshots":[{"imageId":"shot.png", "width":1, "height":1}]}).to_string()),
            ("/images/beta.svg", "<svg/>".into()),
            ("/images/shot.png", "fixture".into()),
        ];
        let server = std::thread::spawn(move || {
            for (path, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut request = [0; 4096];
                let n = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..n]).starts_with(&format!("GET {path} "))
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        sync_remote(&url, dir.path(), true, None).await?;
        server.join().unwrap();
        let details: AppDetailsForMarket =
            serde_json::from_slice(&fs::read(dir.path().join("experimental/test.json")).await?)?;
        assert_eq!(details.description, "Pending description");
        assert!(dir.path().join("images/beta.svg").is_file());
        assert!(dir.path().join("images/shot.png").is_file());
        Ok(())
    }

    #[tokio::test]
    async fn import_tarball_downloads_packages_and_finishes_atomically() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = dir.path().join("catalog");
        fs::create_dir_all(catalog.join("apps")).await?;
        let id = "0123456789abcdef0123456789abcdef";
        let index = serde_json::json!({"apps": [{"appId": "test", "name": "Test", "version": "1", "versionNumber": 1, "packageId": id, "isOpenSource": true, "author": null}]});
        fs::write(catalog.join("apps/index.json"), serde_json::to_vec(&index)?).await?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let server = std::thread::spawn(move || {
            for status in ["200 OK", "404 Not Found"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut request = [0; 4096];
                let n = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..n])
                        .starts_with(&format!("GET /packages/{id} "))
                );
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture"
                )
                .unwrap();
            }
        });
        let output = dir.path().join("import.tar");
        create_import(&url, &catalog, false, &output).await?;
        let original = fs::read(&output).await?;
        let mut archive = tar::Archive::new(original.as_slice());
        let mut entries = archive.entries()?;
        assert_eq!(
            entries.next().unwrap()?.path()?.as_ref(),
            Path::new("apps/index.json")
        );
        let mut package = entries.next().unwrap()?;
        assert_eq!(
            package.path()?.as_ref(),
            Path::new(&format!("packages/{id}"))
        );
        let mut bytes = Vec::new();
        package.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"fixture");
        assert!(entries.next().is_none());
        assert!(create_import(&url, &catalog, false, &output).await.is_err());
        assert_eq!(fs::read(output).await?, original);
        server.join().unwrap();
        Ok(())
    }
}
