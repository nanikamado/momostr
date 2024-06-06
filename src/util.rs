use reqwest::header;
use serde::Serialize;
use std::time::Duration;

#[derive(Serialize)]
pub struct Merge<T1, T2> {
    #[serde(flatten)]
    pub f1: T1,
    #[serde(flatten)]
    pub f2: T2,
}

pub async fn get_media_type(url: &str, client: &reqwest::Client) -> Option<String> {
    let r = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let a = r.headers().get(header::CONTENT_TYPE)?.to_str().ok()?;
    if a.starts_with("image/") || a.starts_with("video/") || a.starts_with("audio/") {
        Some(a.to_string())
    } else {
        None
    }
}
