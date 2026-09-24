use std::{path::PathBuf, time::Duration};

use hyper::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};

/// Represents an API token for Gofile
///
/// This is a type-safe wrapper around it that allows you to do operations with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gofile {
    token: String,
    folder_id: String,
}

#[derive(Deserialize)]
struct ResponseData {
    id: String,
    name: String,
    servers: Vec<String>,
    #[serde(rename = "downloadPage")]
    download_url: String,
}

impl ResponseData {
    #[allow(dead_code)]
    fn direct_download_url(&self) -> String {
        let server = self.servers.first().map(String::as_str).unwrap_or_default();
        format!("https://{}.gofile.io/download/web/{}/{}", server, self.id, self.name)
    }
}

#[derive(Deserialize)]
struct UploadResponse {
    data: ResponseData,
}

impl Gofile {
    pub async fn upload(&self, client: &reqwest::Client, file: PathBuf) -> anyhow::Result<String> {
        let form = reqwest::multipart::Form::new()
            .file("file", file)
            .await?
            .text("folderId", self.folder_id.clone());

        let response = client
            .post("https://upload.gofile.io/uploadfile")
            .multipart(form)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .timeout(Duration::from_secs(86400)) // emulate "no timeout"
            .send()
            .await?
            .error_for_status()?;

        let json: UploadResponse = response.json().await?;
        Ok(json.data.download_url)
    }
}
