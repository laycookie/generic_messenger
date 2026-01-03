use std::{
    error::Error,
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

use facet::Facet;
use surf::{RequestBuilder, StatusCode};
use tracing::{error, info};

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
    let json = facet_format_json::from_str(&json_stringified)?;

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

    // Create a file at the specified path
    match fs::create_dir_all(&path) {
        Ok(_) => info!("Directory created successfully: {:?}", path),
        Err(e) => error!("Failed to create directory: {}", e),
    }

    let mut file = File::create(&file_path)?;

    // Copy the content from the response to the file
    let bytes = res.body_bytes().await?;
    file.write_all(&bytes)?;

    Ok(file_path)
}
