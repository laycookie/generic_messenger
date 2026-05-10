use std::{error::Error, path::PathBuf};

use futures::io::AsyncWriteExt;

use facet::Facet;
use messenger_interface::types::{CACHE_IMGS_DIR, CacheCategory};
use surf::{RequestBuilder, StatusCode};
use tracing::error;

use crate::api_types::SNOWFLAKE;

const DISCORD_CDN: &str = "https://cdn.discordapp.com";
const DISCORD_CACHE: &str = "discord";

/// Build a Discord CDN image URL.
///
/// `kind` is the CDN path segment (e.g. `"avatars"`, `"icons"`, `"channel-icons"`).
pub fn cdn_image_url(kind: &str, id: SNOWFLAKE, hash: &str) -> String {
    format!("{DISCORD_CDN}/{kind}/{id}/{hash}.webp?size=80&quality=lossless")
}

/// Build the local cache directory for a Discord image.
pub fn cache_img_dir(category: CacheCategory, id: SNOWFLAKE) -> PathBuf {
    [CACHE_IMGS_DIR, category.as_str(), DISCORD_CACHE, &id.to_string()]
        .iter()
        .collect()
}

/// Download and cache a Discord CDN image, returning the local path.
pub async fn cache_cdn_image(
    cdn_kind: &str,
    cache_category: CacheCategory,
    id: SNOWFLAKE,
    hash: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let url = cdn_image_url(cdn_kind, id, hash);
    let dir = cache_img_dir(cache_category, id);
    let filename = format!("{hash}.webp");
    cache_download(url, dir, filename).await
}

pub async fn http_request<T: for<'a> Facet<'a>>(
    mut req: RequestBuilder,
    headers: Vec<(&str, String)>,
) -> Result<T, Box<dyn Error + Sync + Send>> {
    for (key, value) in headers {
        req = req.header(key, value);
    }

    let mut res = req.send().await?;

    let status = res.status();
    if StatusCode::Ok != status {
        // Ussualy a result of an outdated token
        error!("Failed to fetch http with status code: {status:?}");
        return Err(
            surf::Error::from_str(status, "Failed to fetch data from http endpoint.").into(),
        );
    }

    let json_stringified = res.body_string().await?;
    let json = facet_json::from_str(&json_stringified)?;

    Ok(json)
}

pub async fn cache_download(
    url: impl Into<String>,
    path: PathBuf,
    file_name: impl Into<String>,
) -> Result<PathBuf, Box<dyn Error>> {
    let file_path = path.join(file_name.into());
    if file_path.exists() {
        return Ok(file_path);
    };

    let url = url.into();
    let req = surf::get(&url);
    let mut res = req.send().await?;

    let StatusCode::Ok = res.status() else {
        return Err(format!("Failed to download file. Status: {}", res.status()).into());
    };

    async_fs::create_dir_all(&path).await?;

    let mut file = async_fs::File::create(&file_path).await?;

    // Copy the content from the response to the file
    let bytes = res.body_bytes().await?;
    file.write_all(&bytes).await?;

    Ok(file_path)
}
