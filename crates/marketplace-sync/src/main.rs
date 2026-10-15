use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use marketplace_model::app_index::{AppDetailsForMarket, LoadedAppIndex};
use reqwest::Client;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::fs;

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, default_value = "sqlite://marketplace.db")]
    database_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    SyncFixtures {
        #[arg(long, default_value = "fixtures/app-index")]
        fixture_root: PathBuf,
    },
    SyncRemote {
        #[arg(long, default_value = "https://app-index.sandstorm.io")]
        base_url: String,
        #[arg(long, default_value = "data/app-index")]
        output_dir: PathBuf,
        #[arg(long, default_value_t = true)]
        include_experimental: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::SyncFixtures { fixture_root } => {
            sync_fixtures(&cli.database_url, &fixture_root).await?
        }
        Command::SyncRemote {
            base_url,
            output_dir,
            include_experimental,
        } => sync_remote(&base_url, &output_dir, include_experimental).await?,
    }
    Ok(())
}

async fn sync_fixtures(database_url: &str, fixture_root: &PathBuf) -> Result<()> {
    let loaded = LoadedAppIndex::load(fixture_root)?;
    let options = database_url
        .parse::<SqliteConnectOptions>()?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new().connect_with(options).await?;

    sqlx::query(
        r#"
        create table if not exists apps (
            app_id text primary key,
            name text not null,
            version text not null,
            package_id text not null,
            short_description text not null,
            created_at text not null,
            is_open_source integer not null
        )
        "#,
    )
    .execute(&pool)
    .await?;

    for app in &loaded.index.apps {
        sqlx::query(
            r#"
            insert into apps (app_id, name, version, package_id, short_description, created_at, is_open_source)
            values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            on conflict(app_id) do update set
              name = excluded.name,
              version = excluded.version,
              package_id = excluded.package_id,
              short_description = excluded.short_description,
              created_at = excluded.created_at,
              is_open_source = excluded.is_open_source
            "#,
        )
        .bind(&app.app_id)
        .bind(&app.name)
        .bind(&app.version)
        .bind(&app.package_id)
        .bind(&app.short_description)
        .bind(&app.created_at)
        .bind(app.is_open_source as i64)
        .execute(&pool)
        .await?;
    }

    println!(
        "synced {} apps into {}",
        loaded.index.apps.len(),
        database_url
    );
    Ok(())
}

async fn sync_remote(base_url: &str, output_dir: &Path, include_experimental: bool) -> Result<()> {
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
    fs::write(output_dir.join("apps/index.json"), &index_bytes).await?;

    let loaded = LoadedAppIndex::load(output_dir)?;
    let mut image_ids: BTreeSet<String> = loaded
        .index
        .image_ids()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();

    for app in &loaded.index.apps {
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

    for image_id in &image_ids {
        let image_url = format!("{}/images/{}", base_url.trim_end_matches('/'), image_id);
        let image_bytes = fetch_bytes(&client, &image_url).await?;
        fs::write(output_dir.join("images").join(image_id), image_bytes).await?;
    }

    if include_experimental {
        let experimental_url =
            format!("{}/experimental/index.json", base_url.trim_end_matches('/'));
        let experimental_bytes = fetch_bytes(&client, &experimental_url).await?;
        fs::write(
            output_dir.join("experimental/index.json"),
            experimental_bytes,
        )
        .await?;
    }

    println!(
        "mirrored {} apps and {} images from {} into {}",
        loaded.index.apps.len(),
        image_ids.len(),
        base_url,
        output_dir.display()
    );

    Ok(())
}

async fn fetch_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}
