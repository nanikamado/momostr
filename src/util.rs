use reqwest::header;
use serde::Serialize;
use std::time::Duration;
use tokio::time::Sleep;
use tracing::error;

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

#[derive(Debug)]
pub struct RateLimiter {
    count: u32,
    last_reset: tokio::time::Instant,
    count_max: u32,
    duration: Duration,
}

impl RateLimiter {
    pub fn new(count_max: u32, duration: Duration) -> Self {
        Self {
            count: 0,
            last_reset: tokio::time::Instant::now(),
            count_max,
            duration,
        }
    }

    pub fn wait(&mut self) -> Sleep {
        if self.count >= self.count_max {
            self.count = 0;
            let now = tokio::time::Instant::now();
            let wait_until = self.last_reset + self.duration;
            self.last_reset = now;
            if wait_until > now {
                error!("rate limit reached");
                tokio::time::sleep_until(wait_until)
            } else {
                tokio::time::sleep(Duration::ZERO)
            }
        } else {
            self.count += 1;
            tokio::time::sleep(Duration::ZERO)
        }
    }
}
