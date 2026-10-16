//! Convert verified package metadata without reading or writing registry files.
use crate::{ids, package_capnp as p, registry::atomic_write};
use anyhow::{Context, Result, bail};
use marketplace_model::app_index::{AppDetailsForMarket, AppSummary, Screenshot};
use sha2::Digest;
use std::path::Path;

pub(crate) struct Entry<'a> {
    pub app: AppSummary,
    pub details: AppDetailsForMarket,
    pub fingerprint: &'a str,
    assets: Vec<Asset<'a>>,
}
struct Asset<'a> {
    name: String,
    data: &'a [u8],
}
impl Entry<'_> {
    pub fn write_assets(&self, catalog: &Path) -> Result<()> {
        for asset in &self.assets {
            let path = catalog.join("images").join(&asset.name);
            if !path.exists() {
                atomic_write(&path, asset.data)?;
            }
        }
        Ok(())
    }
}
pub(crate) fn parse<'a>(info: p::verified_info::Reader<'a>, published: i64) -> Result<Entry<'a>> {
    let meta = info.get_metadata()?;
    let icons = meta.get_icons();
    let mut assets = Vec::new();
    let mut collect_image = |data: &'a [u8], extension| -> String {
        let hash = blake2::Blake2b::<blake2::digest::consts::U16>::digest(data);
        let name = format!("{}.{}", hex::encode(hash), extension);
        assets.push(Asset {
            name: name.clone(),
            data,
        });
        name
    };
    let mut image_id = String::new();
    if icons.has_market() || icons.has_app_grid() {
        let icon = if icons.has_market() {
            icons.get_market()?
        } else {
            icons.get_app_grid()?
        };
        image_id = match icon.which()? {
            p::metadata::icon::Svg(svg) => collect_image(svg?.to_str()?.as_bytes(), "svg"),
            p::metadata::icon::Png(png) => collect_image(
                if png.has_dpi1x() {
                    png.get_dpi1x()?
                } else {
                    png.get_dpi2x()?
                },
                "png",
            ),
            _ => String::new(),
        };
    }
    let mut screenshots = Vec::new();
    for shot in meta.get_screenshots()? {
        let image_id = match shot.which()? {
            p::metadata::screenshot::Png(png) => collect_image(png?, "png"),
            p::metadata::screenshot::Jpeg(jpeg) => collect_image(jpeg?, "jpeg"),
            _ => bail!("unknown screenshot format"),
        };
        screenshots.push(Screenshot {
            image_id,
            width: shot.get_width(),
            height: shot.get_height(),
        });
    }
    let (is_open_source, license) = match meta.get_license().which()? {
        p::metadata::license::OpenSource(value) => (
            true,
            LICENSES
                .get(value? as usize)
                .unwrap_or(&"Unknown")
                .to_string(),
        ),
        p::metadata::license::Proprietary(_) => (false, "Proprietary".to_string()),
        p::metadata::license::PublicDomain(_) => (false, "Public Domain".to_string()),
        _ => (false, String::new()),
    };
    Ok(Entry {
        app: AppSummary {
            app_id: ids::app_id(&ids::app_bytes(info.get_app_id()?)),
            package_id: hex::encode(ids::package_bytes(info.get_package_id()?)),
            name: info.get_title()?.get_default_text()?.to_str()?.to_string(),
            version: info
                .get_marketing_version()?
                .get_default_text()?
                .to_str()?
                .to_string(),
            version_number: info.get_version(),
            image_id,
            web_link: meta.get_website()?.to_str()?.to_string(),
            code_link: meta.get_code_url()?.to_str()?.to_string(),
            is_open_source,
            categories: meta
                .get_categories()?
                .iter()
                .map(|c| c.map(|v| CATEGORIES[v as usize].to_string()))
                .collect::<std::result::Result<_, _>>()?,
            author: None,
            upstream_author: meta
                .get_author()
                .get_upstream_author()?
                .to_str()?
                .to_string(),
            short_description: meta
                .get_short_description()?
                .get_default_text()?
                .to_str()?
                .to_string(),
            created_at: chrono::DateTime::from_timestamp(published, 0)
                .context("invalid publish date")?
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        },
        details: AppDetailsForMarket {
            description: meta
                .get_description()?
                .get_default_text()?
                .to_str()?
                .to_string(),
            screenshots,
            license,
        },
        fingerprint: info.get_author_pgp_key_fingerprint()?.to_str()?,
        assets,
    })
}
const CATEGORIES: &[&str] = &[
    "Other",
    "Productivity",
    "Communications",
    "Social",
    "Web Publishing",
    "Office",
    "DevTools",
    "Science",
    "Media",
    "Games",
    "Graphics",
];
const LICENSES: &[&str] = &[
    "Invalid",
    "MIT License",
    "Apache License v2",
    "GNU GPL v3",
    "GNU AGPL v3",
    "BSD 3-Clause",
    "BSD 2-Clause",
    "GNU GPL v2",
    "GNU LGPL v2.1",
    "GNU LGPL v3",
    "ISC License",
    "Artistic License v2",
    "Python License v2",
    "PHP License v3",
    "Mozilla Public License v2",
    "CDDL",
    "Eclipse Public License",
    "Common Public Attribution License",
    "Zlib/libpng License",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_assets_are_only_written_when_publishing() -> Result<()> {
        let mut message = capnp::message::Builder::new_default();
        let mut info = message.init_root::<p::verified_info::Builder>();
        let mut metadata = info.reborrow().init_metadata();
        metadata
            .reborrow()
            .init_icons()
            .init_market()
            .set_svg("<svg/>");
        let mut shot = metadata.init_screenshots(1).get(0);
        shot.set_png(b"screenshot");
        shot.set_width(640);
        shot.set_height(480);
        let entry = parse(message.get_root_as_reader()?, 0)?;
        let output = tempfile::tempdir()?;
        std::fs::create_dir(output.path().join("images"))?;
        assert_eq!(std::fs::read_dir(output.path().join("images"))?.count(), 0);
        assert_eq!(entry.details.screenshots[0].width, 640);
        entry.write_assets(output.path())?;
        assert_eq!(
            std::fs::read(output.path().join("images").join(&entry.app.image_id))?,
            b"<svg/>"
        );
        assert_eq!(
            std::fs::read(
                output
                    .path()
                    .join("images")
                    .join(&entry.details.screenshots[0].image_id)
            )?,
            b"screenshot"
        );
        Ok(())
    }
}
