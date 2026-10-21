//! The npm cache's HTTP client: registry metadata and tarballs, over
//! reqwest. Retries transient failures; a 404 and a 304 are answers, not
//! errors.

use std::time::Duration;

use simien_npm_cache::{
    DownloadError, NpmCacheHttpClient, NpmCacheHttpClientBytesResponse, NpmCacheHttpClientResponse,
};
use simien_npmrc::RegistryConfig;
use url::Url;

const ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct HttpClient(reqwest::Client);

impl HttpClient {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("simien/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self(client))
    }

    async fn get_once(
        &self,
        url: &Url,
        maybe_auth: Option<&str>,
        maybe_etag: Option<&str>,
    ) -> Result<NpmCacheHttpClientResponse, DownloadError> {
        let mut request = self.0.get(url.clone());
        if let Some(auth) = maybe_auth {
            request = request.header(reqwest::header::AUTHORIZATION, auth);
        }
        if let Some(etag) = maybe_etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(|err| DownloadError {
            status_code: None,
            error: simien_error::JsErrorBox::generic(format!("{url}: {err}")),
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(NpmCacheHttpClientResponse::NotFound);
        }
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(NpmCacheHttpClientResponse::NotModified);
        }
        if !status.is_success() {
            return Err(DownloadError {
                status_code: Some(status.as_u16()),
                error: simien_error::JsErrorBox::generic(format!("{url}: HTTP {status}")),
            });
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(|err| DownloadError {
            status_code: Some(status.as_u16()),
            error: simien_error::JsErrorBox::generic(format!("{url}: {err}")),
        })?;
        Ok(NpmCacheHttpClientResponse::Bytes(
            NpmCacheHttpClientBytesResponse {
                bytes: bytes.to_vec(),
                etag,
            },
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl NpmCacheHttpClient for HttpClient {
    async fn download_with_retries_on_any_tokio_runtime(
        &self,
        url: Url,
        maybe_auth: Option<String>,
        maybe_etag: Option<String>,
        _maybe_registry_config: Option<&RegistryConfig>,
    ) -> Result<NpmCacheHttpClientResponse, DownloadError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .get_once(&url, maybe_auth.as_deref(), maybe_etag.as_deref())
                .await
            {
                // Client errors (4xx) won't change on a retry.
                Err(err)
                    if attempt < ATTEMPTS
                        && !err
                            .status_code
                            .is_some_and(|code| (400..500).contains(&code)) =>
                {
                    tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt))).await;
                }
                result => return result,
            }
        }
    }
}
