//! Video generation module. Hosts the shared [`VideoGenClient`] and the
//! `image_to_video` and `reference_to_video` tools, which generate videos via
//! the xAI Video Generation API and save them to the local filesystem so the
//! model can reference them in code (e.g. `<video src="videos/hero.mp4">`).
//!
//! Architecture follows the same pattern as `image_gen`:
//!
//! - [`VideoGenConfig`] is built from session credentials by the host and
//!   injected into the tool registry.
//! - When `Enabled`, a [`VideoGenClient`] is constructed once and injected
//!   into `Resources`. The tools read it at runtime via `resources.require()`.
//! - When `Disabled`, the tools are not registered so the model never sees them.
//!
//! The generated video is written to `<session_folder>/videos/<n>.mp4`
//! where `<n>` is a session-scoped counter (1, 2, 3, ... — 1 token each).
//! The tools return the absolute path so the model can copy or move the
//! video into the project working directory when it needs a persistent asset.
//!
//! Video generation is asynchronous:
//! 1. POST to `/v1/videos/generations` → receive a `request_id`
//! 2. Poll GET `/v1/videos/{request_id}` until status is `"done"`
//! 3. Download video bytes from the API URL, or an optional presigned GET URL

use base64::Engine as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::Deserialize;

use crate::attribution::{SharedAttributionCallback, ToolConsumer};
use crate::types::SharedApiKeyProvider;

use crate::types::output::{MediaGenOutput, ToolOutput};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::SessionFolder;
use crate::types::tool::{ToolKind, ToolNamespace};

const XAI_VIDEO_MODEL: &str = "grok-imagine-video-1.5";
const VIDEO_START_TIMEOUT_SECS: u64 = 60;
const VIDEO_GEN_TIMEOUT_SECS: u64 = 300;
const VIDEO_POLL_INTERVAL_SECS: u64 = 5;
const VIDEO_POLL_REQUEST_TIMEOUT_SECS: u64 = 30;
const VIDEO_DOWNLOAD_TIMEOUT_SECS: u64 = 120;
const DEFAULT_ZDR_VIDEO_PRESIGN_EXPIRES_SECS: u64 = 900;
/// Presign at request start; must survive generation poll + local download.
const MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS: u64 =
    VIDEO_GEN_TIMEOUT_SECS + VIDEO_DOWNLOAD_TIMEOUT_SECS + 60;
const DEFAULT_ZDR_VIDEO_KEY_PREFIX: &str = "grok-videos/";
const ZDR_VIDEO_CONTENT_TYPE: &str = "video/mp4";
const DEFAULT_VIDEO_DIR: &str = "videos";
const DEFAULT_RESOLUTION: &str = "480p";
const DEFAULT_IMAGINE_VIDEO_DURATION_SECS: u32 = 6;
const MAX_R2V_REFERENCE_IMAGES: usize = 7;
const MAX_R2V_REFERENCE_VOICES: usize = 3;
const MIN_R2V_DURATION_SECS: u32 = 1;
const MAX_R2V_DURATION_SECS: u32 = 15;
const VALID_IMAGINE_VIDEO_ASPECT_RATIOS: &[&str] =
    &["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3"];
const VALID_VIDEO_RESOLUTIONS: &[&str] = &["480p", "720p"];
const IMAGINE_VIDEO_DURATIONS_SECS: &[u32] = &[6, 10];

pub use xai_grok_tools_api::slash_commands::{
    IMAGE_TO_VIDEO_TOOL_NAME, IMAGINE_VIDEO_COMMAND_NAME, imagine_video_instruction,
    imagine_video_usage_message,
};

pub const REFERENCE_TO_VIDEO_TOOL_NAME: &str = "reference_to_video";

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct S3AccessCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl S3AccessCredentials {
    fn is_valid(&self) -> bool {
        !self.access_key_id.trim().is_empty() && !self.secret_access_key.trim().is_empty()
    }

    fn to_static(&self) -> xai_file_utils::s3::S3StaticCredentials {
        xai_file_utils::s3::S3StaticCredentials {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
        }
    }
}

impl std::fmt::Debug for S3AccessCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3AccessCredentials")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct ZdrVideoOutputS3Config {
    pub bucket: String,
    pub endpoint: String,
    pub region: String,
    #[serde(default = "default_zdr_video_key_prefix")]
    pub key_prefix: String,
    #[serde(default = "default_zdr_video_presign_expires_secs")]
    pub expires_secs: u64,
    pub read_write: S3AccessCredentials,
    #[serde(default)]
    pub read_only: Option<S3AccessCredentials>,
}

fn default_zdr_video_key_prefix() -> String {
    DEFAULT_ZDR_VIDEO_KEY_PREFIX.to_owned()
}

fn default_zdr_video_presign_expires_secs() -> u64 {
    DEFAULT_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
}

impl ZdrVideoOutputS3Config {
    pub fn is_valid(&self) -> bool {
        !self.bucket.trim().is_empty()
            && !self.endpoint.trim().is_empty()
            && !self.region.trim().is_empty()
            && self.read_write.is_valid()
    }
}

impl std::fmt::Debug for ZdrVideoOutputS3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZdrVideoOutputS3Config")
            .field("bucket_present", &!self.bucket.is_empty())
            .field("endpoint_present", &!self.endpoint.is_empty())
            .field("region_present", &!self.region.is_empty())
            .field("key_prefix_present", &!self.key_prefix.is_empty())
            .field("expires_secs", &self.expires_secs)
            .field("read_write", &self.read_write)
            .field("read_only", &self.read_only.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// HTTP client for xAI Video Generation API. Cloned per-request; shares `Arc` state.
#[derive(Clone)]
pub struct VideoGenClient {
    http: reqwest::Client,
    download_http: reqwest::Client,
    base_url: String,
    writer: super::storage::SessionFileWriter,
    zdr_video_output_s3: Option<ZdrVideoOutputS3Config>,
    api_key_provider: Option<SharedApiKeyProvider>,
    /// Optional 401-attribution hook. Hosts wire this so a 401 from the
    /// Video Generation API emits an `auth_401_attribution` event with
    /// `consumer` of `"VideoGen.start"` (start request) or
    /// `"VideoGen.poll"` (poll request) for unified auth-failure telemetry.
    attribution_callback: Option<SharedAttributionCallback>,
    /// When `true`, the user is on a tier the Imagine server zero-limits
    /// (free / X Basic). The video tools short-circuit before any HTTP call
    /// and return the SuperGrok upsell prose. See [`VideoGenClient::is_tier_restricted`].
    tier_restricted: bool,
    /// See [`VideoGenConfig::Enabled`]'s `zdr_restricted`.
    zdr_restricted: bool,
}

impl VideoGenClient {
    pub fn new(
        config: &VideoGenConfig,
        api_key_provider: Option<SharedApiKeyProvider>,
    ) -> Result<Self, xai_tool_runtime::ToolError> {
        let VideoGenConfig::Enabled {
            api_key,
            base_url,
            extra_headers,
            zdr_video_output_s3,
            tier_restricted,
            zdr_restricted,
        } = config
        else {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "Cannot create VideoGenClient from disabled config",
            ));
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Always bake the static api_key as the default Authorization header.
        // The dynamic provider overrides per-request; this is the fallback.
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| {
                xai_tool_runtime::ToolError::invalid_arguments("Invalid API key for header.")
            })?,
        );

        extra_headers.into_iter().try_for_each(|(key, value)| {
            let header_name =
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
                    xai_tool_runtime::ToolError::invalid_arguments("Invalid extra header name.")
                })?;
            let header_value = HeaderValue::from_str(value).map_err(|_| {
                xai_tool_runtime::ToolError::invalid_arguments("Invalid extra header value.")
            })?;
            headers.insert(header_name, header_value);
            Ok::<(), xai_tool_runtime::ToolError>(())
        })?;

        let http = xai_grok_extra_ca::with_extra_root_certificates(
            reqwest::Client::builder().default_headers(headers),
        )
        .build()
        .map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Failed to build HTTP client.")
        })?;

        let download_http = xai_grok_extra_ca::with_extra_root_certificates(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(VIDEO_DOWNLOAD_TIMEOUT_SECS)),
        )
        .build()
        .map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Failed to build download client.")
        })?;

        Ok(Self {
            http,
            download_http,
            base_url: base_url.clone(),
            writer: super::storage::SessionFileWriter::new(DEFAULT_VIDEO_DIR, "mp4"),
            zdr_video_output_s3: zdr_video_output_s3
                .as_ref()
                .map(|c| (**c).clone())
                .filter(ZdrVideoOutputS3Config::is_valid),
            api_key_provider,
            attribution_callback: None,
            tier_restricted: *tier_restricted,
            zdr_restricted: *zdr_restricted,
        })
    }

    /// Whether the current user's tier (free / X Basic) is zero-limited on
    /// Imagine server-side. The video tools use this to short-circuit with the
    /// SuperGrok upsell instead of issuing a doomed request.
    pub(crate) fn is_tier_restricted(&self) -> bool {
        self.tier_restricted
    }

    /// See [`VideoGenConfig::Enabled`]'s `zdr_restricted`.
    pub(crate) fn is_zdr_restricted(&self) -> bool {
        self.zdr_restricted
    }

    /// Wire a 401-attribution callback into this client. Idempotent;
    /// safe to call before or after the first request.
    pub fn with_attribution_callback(
        mut self,
        callback: Option<SharedAttributionCallback>,
    ) -> Self {
        self.attribution_callback = callback;
        self
    }

    async fn current_bearer(&self) -> Option<String> {
        crate::types::api_key_provider::resolve_bearer(self.api_key_provider.as_ref()).await
    }

    async fn compare_sent_credential(
        &self,
        sent_bearer: Option<&str>,
    ) -> xai_grok_auth::CredentialComparison {
        crate::types::api_key_provider::compare_sent_bearer(
            self.api_key_provider.as_ref(),
            sent_bearer,
        )
        .await
    }

    fn record_401_attribution(
        &self,
        consumer: ToolConsumer,
        comparison: xai_grok_auth::CredentialComparison,
    ) {
        crate::attribution::emit_401(self.attribution_callback.as_ref(), consumer, comparison);
    }

    pub async fn generate_with_images(
        &self,
        model: &'static str,
        prompt: &str,
        duration: Option<u32>,
        aspect_ratio: Option<&str>,
        resolution: &str,
        image: Option<String>,
        reference_images: Vec<String>,
        reference_voices: Vec<String>,
    ) -> Result<VideoOutcome, xai_tool_runtime::ToolError> {
        let start_url = format!("{}/videos/generations", self.base_url.trim_end_matches('/'));

        let presigned = match &self.zdr_video_output_s3 {
            Some(config) => Some(self.presign_zdr_output_urls(config).await?),
            None => None,
        };

        let payload = GenerateVideoPayload {
            model,
            prompt,
            image: image.map(|url| VideoImageUrl { url }),
            duration,
            aspect_ratio,
            resolution,
            reference_images: reference_images
                .into_iter()
                .map(|url| VideoImageUrl { url })
                .collect(),
            reference_audios: reference_voices
                .into_iter()
                .map(|voice_id| VideoVoiceId { voice_id })
                .collect(),
            output: presigned.as_ref().map(|urls| VideoOutput {
                upload_url: urls.upload_url.clone(),
            }),
        };

        let sent_bearer = self.current_bearer().await;
        let mut req = self
            .http
            .post(&start_url)
            .timeout(std::time::Duration::from_secs(VIDEO_START_TIMEOUT_SECS))
            .json(&payload);
        if let Some(ref key) = sent_bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let request = req.build().map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments(
                "Failed to build video generation request.",
            )
        })?;
        let sent_bearer = crate::types::api_key_provider::request_credential(&request);
        let response = self.http.execute(request).await.map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Video generation API transport failed.")
        })?;

        let status = response.status();
        if crate::types::api_key_provider::is_auth_failure(status) {
            let comparison = self.compare_sent_credential(sent_bearer.as_deref()).await;
            self.record_401_attribution(ToolConsumer::VideoGenStart, comparison);
        }
        if !status.is_success() {
            tracing::warn!(http_status = %status, "Video generation API request failed");
            return Err(xai_tool_runtime::ToolError::new(
                xai_tool_runtime::ToolErrorKind::Custom,
                format!("Video generation failed (HTTP {status})."),
            )
            .with_details(serde_json::json!({"code": "http_failure", "status": status.as_u16()})));
        }

        let body = response.text().await.map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments(
                "Failed to read video generation start response.",
            )
        })?;

        let start_resp: VideoGenStartResponse = serde_json::from_str(&body).map_err(|_| {
            tracing::warn!("Video generation API returned an invalid start response");
            xai_tool_runtime::ToolError::invalid_arguments(
                "Video generation API returned an invalid start response.",
            )
        })?;

        let request_id = start_resp.request_id;
        if request_id.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "No request_id received from the video generation API.",
            ));
        }

        tracing::info!("Video generation started, polling for completion");

        let poll_url = format!(
            "{}/videos/{}",
            self.base_url.trim_end_matches('/'),
            request_id
        );
        let poll_timeout = std::time::Duration::from_secs(VIDEO_POLL_REQUEST_TIMEOUT_SECS);
        let poll_interval = std::time::Duration::from_secs(VIDEO_POLL_INTERVAL_SECS);
        let deadline = std::time::Duration::from_secs(VIDEO_GEN_TIMEOUT_SECS);
        let started = tokio::time::Instant::now();

        loop {
            tokio::time::sleep(poll_interval).await;

            if started.elapsed() >= deadline {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Video generation did not complete within {}s.",
                    VIDEO_GEN_TIMEOUT_SECS
                )));
            }

            let poll_sent_bearer = self.current_bearer().await;
            let mut poll_req = self.http.get(&poll_url).timeout(poll_timeout);
            if let Some(ref key) = poll_sent_bearer {
                poll_req = poll_req.header(AUTHORIZATION, format!("Bearer {key}"));
            }
            let poll_request = poll_req.build().map_err(|_| {
                xai_tool_runtime::ToolError::invalid_arguments(
                    "Failed to build video poll request.",
                )
            })?;
            let poll_sent_bearer =
                crate::types::api_key_provider::request_credential(&poll_request);
            let poll_response = self.http.execute(poll_request).await.map_err(|_| {
                xai_tool_runtime::ToolError::invalid_arguments("Video poll transport failed.")
            })?;

            let poll_status = poll_response.status();
            if crate::types::api_key_provider::is_auth_failure(poll_status) {
                let poll_comparison = self
                    .compare_sent_credential(poll_sent_bearer.as_deref())
                    .await;
                self.record_401_attribution(ToolConsumer::VideoGenPoll, poll_comparison);
            }
            if !poll_status.is_success() && poll_status.as_u16() != 202 {
                return Err(xai_tool_runtime::ToolError::new(
                    xai_tool_runtime::ToolErrorKind::Custom,
                    format!("Video poll failed (HTTP {poll_status})."),
                )
                .with_details(
                    serde_json::json!({"code": "http_failure", "status": poll_status.as_u16()}),
                ));
            }

            let poll_body = poll_response.text().await.map_err(|_| {
                xai_tool_runtime::ToolError::invalid_arguments(
                    "Failed to read video poll response.",
                )
            })?;

            let poll_data: VideoGenPollResponse =
                serde_json::from_str(&poll_body).map_err(|_| {
                    tracing::warn!("Video poll API returned an invalid response");
                    xai_tool_runtime::ToolError::invalid_arguments(
                        "Video poll API returned an invalid response.",
                    )
                })?;

            match poll_data.status.as_str() {
                "done" => {
                    let video_url = poll_data.video.and_then(|v| v.url).unwrap_or_default();
                    tracing::info!(
                        elapsed_secs = started.elapsed().as_secs(),
                        "Video generation completed"
                    );
                    return match presigned {
                        Some(urls) => self.finish_zdr_video(urls).await,
                        None if video_url.is_empty() => {
                            Err(xai_tool_runtime::ToolError::invalid_arguments(
                                "Video generation completed but no download URL was returned.",
                            ))
                        }
                        None => self
                            .download_video(&video_url)
                            .await
                            .map(VideoOutcome::Bytes),
                    };
                }
                "failed" => {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "Video generation failed on the server.",
                    ));
                }
                "expired" => {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "Video generation request expired.",
                    ));
                }
                _ => {
                    tracing::debug!(
                        status_class = "pending_or_unknown",
                        elapsed_secs = started.elapsed().as_secs(),
                        "Video generation still in progress"
                    );
                }
            }
        }
    }

    /// Download video bytes from a pre-signed temporary URL (no auth headers).
    async fn download_video(&self, url: &str) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        let response = self.download_http.get(url).send().await.map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Video download transport failed.")
        })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(xai_tool_runtime::ToolError::new(
                xai_tool_runtime::ToolErrorKind::Custom,
                format!("Video download failed (HTTP {status})"),
            )
            .with_details(serde_json::json!({"code": "http_failure", "status": status.as_u16()})));
        }

        response.bytes().await.map(|b| b.to_vec()).map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Failed to read video bytes.")
        })
    }

    async fn finish_zdr_video(
        &self,
        urls: ZdrPresignedUrls,
    ) -> Result<VideoOutcome, xai_tool_runtime::ToolError> {
        let config = self.zdr_video_output_s3.as_ref().ok_or_else(|| {
            xai_tool_runtime::ToolError::invalid_arguments(
                "Presigned video output config missing after presign",
            )
        })?;

        // A presigned GET means the client should download locally. If that
        // fails after the provider already uploaded the object, preserve the
        // successful generation as a credential-free S3 object reference.
        if let Some(get_url) = urls.get_url.as_deref() {
            return match self.download_video(get_url).await {
                Ok(bytes) => Ok(VideoOutcome::Bytes(bytes)),
                Err(_) => {
                    tracing::warn!("Presigned video download failed; returning object reference");
                    Ok(zdr_uploaded_reference_outcome(config, &urls))
                }
            };
        }

        // No pre-minted GET URL — retry presign (may succeed now that the
        // object exists) and attempt a local download. A presigned URL is never
        // returned to the model; upload-success fallback uses only bucket/key.
        match self.presign_and_download(config, &urls).await {
            Ok(bytes) => Ok(VideoOutcome::Bytes(bytes)),
            Err(_) => {
                tracing::warn!("Post-upload video download failed; returning object reference");
                Ok(zdr_uploaded_reference_outcome(config, &urls))
            }
        }
    }

    async fn presign_zdr_output_urls(
        &self,
        config: &ZdrVideoOutputS3Config,
    ) -> Result<ZdrPresignedUrls, xai_tool_runtime::ToolError> {
        let object_key = zdr_video_object_key(&config.key_prefix);
        let expires_in =
            std::time::Duration::from_secs(zdr_presign_expires_secs(config.expires_secs));
        let endpoint = Some(config.endpoint.as_str());

        let upload_url = xai_file_utils::s3::presign_put_url(
            &config.region,
            endpoint,
            &config.read_write.to_static(),
            &config.bucket,
            &object_key,
            ZDR_VIDEO_CONTENT_TYPE,
            expires_in,
        )
        .await
        .map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Failed to presign video upload URL.")
        })?;

        ensure_http_url(&upload_url, "Presigned video upload URL is invalid.")?;

        let get_url = match self
            .presign_zdr_get_url(config, &object_key, expires_in)
            .await
        {
            Ok(url) => Some(url),
            Err(_) => {
                tracing::warn!(
                    "Video GET presign failed before generation; will retry download after upload completes"
                );
                None
            }
        };

        Ok(ZdrPresignedUrls {
            object_key,
            upload_url,
            get_url,
            expires_in,
        })
    }

    /// Re-presign a GET URL after generation and attempt a local download.
    async fn presign_and_download(
        &self,
        config: &ZdrVideoOutputS3Config,
        urls: &ZdrPresignedUrls,
    ) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        let get_url = self
            .presign_zdr_get_url(config, &urls.object_key, urls.expires_in)
            .await?;
        tracing::info!("Post-upload video GET presign succeeded, attempting download");
        self.download_video(&get_url).await
    }

    async fn presign_zdr_get_url(
        &self,
        config: &ZdrVideoOutputS3Config,
        object_key: &str,
        expires_in: std::time::Duration,
    ) -> Result<String, xai_tool_runtime::ToolError> {
        let endpoint = Some(config.endpoint.as_str());
        let (creds, creds_source) = zdr_get_credentials(config);
        let url = xai_file_utils::s3::presign_get_url(
            &config.region,
            endpoint,
            &creds.to_static(),
            &config.bucket,
            object_key,
            expires_in,
        )
        .await
        .map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to presign video GET URL ({creds_source})."
            ))
        })?;

        ensure_http_url(&url, "Presigned video download URL is invalid.")?;
        Ok(url)
    }
}

fn zdr_presign_expires_secs(configured: u64) -> u64 {
    configured.max(MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS)
}

fn zdr_get_credentials(config: &ZdrVideoOutputS3Config) -> (&S3AccessCredentials, &'static str) {
    if let Some(read_only) = config.read_only.as_ref() {
        if read_only.is_valid() {
            return (read_only, "read_only");
        }
        tracing::warn!(
            "tools.zdr_video_output_s3.read_only is incomplete; falling back to read_write for GET presign"
        );
    }
    (&config.read_write, "read_write")
}

fn zdr_video_object_key(prefix: &str) -> String {
    let prefix = prefix.trim();
    let object_id = uuid::Uuid::new_v4();
    if prefix.is_empty() {
        format!("{object_id}.mp4")
    } else {
        let normalized = if prefix.ends_with('/') {
            prefix.to_owned()
        } else {
            format!("{prefix}/")
        };
        format!("{normalized}{object_id}.mp4")
    }
}

/// Build a stable remote reference from non-secret S3 coordinates only.
/// Presigned URLs, endpoints, and credentials must never reach model output.
fn zdr_object_reference(bucket: &str, object_key: &str) -> String {
    let mut reference = url::Url::parse("s3://configured/").expect("valid static S3 URL");
    if reference.set_host(Some(bucket.trim())).is_err() {
        return "s3://configured/object".to_string();
    }
    if let Ok(mut segments) = reference.path_segments_mut() {
        segments.clear();
        for segment in object_key.split('/').filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    reference.to_string()
}

fn zdr_uploaded_reference_outcome(
    config: &ZdrVideoOutputS3Config,
    urls: &ZdrPresignedUrls,
) -> VideoOutcome {
    VideoOutcome::UploadedReference(zdr_object_reference(&config.bucket, &urls.object_key))
}

fn is_http_url(raw: &str) -> bool {
    url::Url::parse(raw)
        .map(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn ensure_http_url(raw: &str, safe_error: &'static str) -> Result<(), xai_tool_runtime::ToolError> {
    if is_http_url(raw) {
        Ok(())
    } else {
        Err(xai_tool_runtime::ToolError::invalid_arguments(safe_error))
    }
}

/// Session-level configuration. Same shape as [`ImageGenConfig`].
///
/// [`ImageGenConfig`]: super::image_gen::ImageGenConfig
#[derive(Clone, Default)]
pub enum VideoGenConfig {
    #[default]
    Disabled,
    Enabled {
        api_key: String,
        base_url: String,
        extra_headers: indexmap::IndexMap<String, String>,
        zdr_video_output_s3: Option<Box<ZdrVideoOutputS3Config>>,
        /// `true` when the user is on a tier the Imagine server zero-limits
        /// (free / X Basic). The video tools stay advertised but short-circuit
        /// at call time with the SuperGrok upsell prose. Set by the host from
        /// the subscription tier; always `false` for team / API-key / workspace.
        tier_restricted: bool,
        /// `true` when `tools.disable_zdr_incompatible_tools` is set with no
        /// valid `[tools.zdr_video_output_s3]` bucket. The video tools stay
        /// advertised but fail at call time with [`ZDR_RESTRICTED_MESSAGE`]
        /// instead of being silently dropped.
        zdr_restricted: bool,
    },
}

impl std::fmt::Debug for VideoGenConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("VideoGenConfig::Disabled"),
            Self::Enabled {
                api_key,
                base_url,
                extra_headers,
                zdr_video_output_s3,
                tier_restricted,
                zdr_restricted,
            } => f
                .debug_struct("VideoGenConfig::Enabled")
                .field("api_key_present", &!api_key.is_empty())
                .field("base_url_present", &!base_url.is_empty())
                .field("extra_headers_present", &!extra_headers.is_empty())
                .field(
                    "zdr_video_output_s3_present",
                    &zdr_video_output_s3.is_some(),
                )
                .field("tier_restricted", tier_restricted)
                .field("zdr_restricted", zdr_restricted)
                .finish(),
        }
    }
}

impl VideoGenConfig {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    /// Stamp [`super::image_gen::SESSION_ID_HEADER`] onto `extra_headers`.
    /// A caller-provided value is never overwritten. No-op when `Disabled`.
    pub fn stamp_session_id_header(&mut self, session_id: &str) {
        if let Self::Enabled { extra_headers, .. } = self {
            extra_headers
                .entry(super::image_gen::SESSION_ID_HEADER.to_string())
                .or_insert_with(|| session_id.to_string());
        }
    }
}

/// Prose returned to the model (as a normal, successful tool result) when a
/// free / X Basic user calls a video tool. The model relays it to the user;
/// the deliberate `/imagine-video` slash command shows the SuperGrok upsell
/// modal instead.
pub(crate) const TIER_RESTRICTED_UPSELL: &str = "Video generation is a SuperGrok feature and isn't available on the free or X Basic tier. Let the user know they can unlock image and video generation by upgrading to SuperGrok: https://grok.com/supergrok?referrer=grok-build. Do not retry this tool.";

/// Error for video tool calls in a ZDR session with no output bucket.
/// A verbatim tool *error* (unlike the [`TIER_RESTRICTED_UPSELL`] prose):
/// paraphrasing a privacy-adjacent message risks distortion.
pub(crate) const ZDR_RESTRICTED_MESSAGE: &str = "Video generation tools are unavailable under zero data retention (ZDR). To re-enable, either supply a user-hosted storage bucket (see https://docs.x.ai/build/settings/zdr-video-storage) or turn off /privacy mode to disable ZDR for all Grok Build requests (including code). Restart Grok after changing the config for it to take effect. Relay this message to the user verbatim; do not retry this tool.";

/// The [`ZDR_RESTRICTED_MESSAGE`] as a structured tool error, with a stable
/// details code for log/trace filtering.
fn zdr_restricted_error() -> xai_tool_runtime::ToolError {
    xai_tool_runtime::ToolError::new(
        xai_tool_runtime::ToolErrorKind::Custom,
        ZDR_RESTRICTED_MESSAGE,
    )
    .with_details(serde_json::json!({"code": "zdr_output_storage_required"}))
}

fn default_resolution_name() -> String {
    DEFAULT_RESOLUTION.to_owned()
}

pub enum VideoOutcome {
    Bytes(Vec<u8>),
    UploadedReference(String),
}

#[derive(serde::Serialize)]
struct GenerateVideoPayload<'a> {
    model: &'static str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<VideoImageUrl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<&'a str>,
    resolution: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reference_images: Vec<VideoImageUrl>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reference_audios: Vec<VideoVoiceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<VideoOutput>,
}

#[derive(serde::Serialize)]
struct VideoImageUrl {
    url: String,
}

#[derive(serde::Serialize)]
struct VideoVoiceId {
    voice_id: String,
}

#[derive(serde::Serialize)]
struct VideoOutput {
    upload_url: String,
}

struct ZdrPresignedUrls {
    object_key: String,
    upload_url: String,
    get_url: Option<String>,
    /// Cached TTL for re-presigning after generation completes.
    expires_in: std::time::Duration,
}

#[derive(Debug, serde::Deserialize)]
struct VideoGenStartResponse {
    #[serde(default)]
    request_id: String,
}

#[derive(serde::Deserialize)]
struct VideoGenPollResponse {
    #[serde(default)]
    status: String,
    video: Option<VideoGenVideoInfo>,
}

#[derive(serde::Deserialize)]
struct VideoGenVideoInfo {
    url: Option<String>,
}

async fn resolve_image_reference(value: &str) -> Result<String, xai_tool_runtime::ToolError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "image reference must not be empty",
        ));
    }

    if value.starts_with("data:image/") {
        let comma = value.find(',').ok_or_else(|| {
            xai_tool_runtime::ToolError::invalid_arguments("malformed data URL in image reference")
        })?;
        if !value[..comma].contains(";base64") {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "image references only support base64 data URLs",
            ));
        }
        return Ok(value.to_owned());
    }

    if value.starts_with("https://") {
        return Ok(value.to_owned());
    }

    let raw_bytes = tokio::fs::read(value).await.map_err(|_| {
        xai_tool_runtime::ToolError::invalid_arguments("Image reference is not readable.")
    })?;
    if raw_bytes.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "image reference contained no data",
        ));
    }

    let (_w, _h, mime) =
        crate::util::image_validate::validate_image_bytes(&raw_bytes).map_err(|_| {
            xai_tool_runtime::ToolError::invalid_arguments("Image reference is invalid.")
        })?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

fn validate_one_of(
    field: &str,
    value: &str,
    allowed: &[&str],
) -> Result<(), xai_tool_runtime::ToolError> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
        "`{field}` must be one of: {}. Got {value}.",
        allowed.join(", ")
    )))
}

fn validate_imagine_duration(duration: Option<u32>) -> Result<(), xai_tool_runtime::ToolError> {
    if let Some(secs) = duration
        && !IMAGINE_VIDEO_DURATIONS_SECS.contains(&secs)
    {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "`duration` must be either 6 or 10 seconds. Got {secs}."
        )));
    }
    Ok(())
}

fn validate_r2v_duration(duration: Option<u32>) -> Result<(), xai_tool_runtime::ToolError> {
    if let Some(secs) = duration
        && !(MIN_R2V_DURATION_SECS..=MAX_R2V_DURATION_SECS).contains(&secs)
    {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "`duration` must be between {MIN_R2V_DURATION_SECS} and {MAX_R2V_DURATION_SECS} seconds. Got {secs}."
        )));
    }
    Ok(())
}

fn duration_from_json<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Input {
        Int(u32),
        Str(String),
    }

    match <Option<Input> as serde::Deserialize>::deserialize(deserializer)? {
        Some(Input::Int(value)) => Ok(Some(value)),
        Some(Input::Str(value)) => value
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom("duration must be a whole number of seconds")),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ImageToVideoInput {
    #[serde(default)]
    #[schemars(
        description = "Optional prompt to guide the video generation model. If omitted, a natural animation applies automatically."
    )]
    pub prompt: Option<String>,

    #[schemars(
        description = "Source image to animate. Provide an absolute filesystem path, HTTPS URL, or `data:image/...;base64,...` URL."
    )]
    pub image: String,

    #[serde(
        default,
        deserialize_with = "duration_from_json",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(
        description = "Duration of the video generation, either 6 or 10 seconds. Default to 6 unless the user requests longer."
    )]
    pub duration: Option<u32>,

    #[serde(default = "default_resolution_name")]
    #[schemars(
        description = "Resolution name of the video generation, only specify it when user asks for a specific resolution, either 480p or 720p. Defaults to 480p unless the user specifically requests for higher quality."
    )]
    pub resolution_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ReferenceToVideoInput {
    #[schemars(
        description = "Prompt to guide the video generation model. Describe the desired video."
    )]
    pub prompt: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(
        description = "Reference images, up to 7 entries; the images are used as style/content references for the generated video (people, objects, clothing, settings). Each entry may be an absolute filesystem path, HTTPS URL, or `data:image/...;base64,...` URL. Reference them in the prompt as `<IMAGE_0>`, `<IMAGE_1>`, ... May be empty when `voices` is provided."
    )]
    pub images: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(
        description = "Optional preset voices the subject(s) speak in, up to 3 entries, each a voice identifier from the built-in roster (e.g. \"ara\", \"eve\", \"leo\", \"rex\"; same voices as the xAI text-to-speech API; an unknown identifier fails with the list of available voices). Reference them in the prompt as `<AUDIO_0>`, `<AUDIO_1>`, `<AUDIO_2>`. Usable alongside `images` or on their own."
    )]
    pub voices: Vec<String>,

    #[schemars(
        description = "Aspect ratio of the generated video, decide it based on the user's request. 1:1 for square (icons, profiles), 16:9 for wide (landscapes, cinematic), 9:16 for tall (phone wallpapers, stories), 4:3 or 3:2 for horizontal photos, 3:4 or 2:3 for vertical (portraits, posters)."
    )]
    pub aspect_ratio: String,

    #[serde(
        default,
        deserialize_with = "duration_from_json",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(description = "Duration of the video in seconds, between 1 and 15. Defaults to 6.")]
    pub duration: Option<u32>,

    #[serde(default = "default_resolution_name")]
    #[schemars(
        description = "Resolution name of the video generation, only specify it when user asks for a specific resolution, either 480p or 720p. Defaults to 480p."
    )]
    pub resolution_name: String,
}

/// Acquire the shared [`VideoGenClient`] and session folder from tool
/// resources. Shared by all video-generation tools so the acquisition logic
/// lives in one place.
async fn acquire_video_client(
    ctx: &xai_tool_runtime::ToolCallContext,
) -> Result<(VideoGenClient, std::path::PathBuf), xai_tool_runtime::ToolError> {
    use crate::types::tool_metadata::shared_resources;
    let resources = shared_resources(ctx)?;
    let res = resources.lock().await;
    let client = res.require::<VideoGenClient>()?.clone();
    let session_folder = res.require::<SessionFolder>()?.0.clone();
    Ok((client, session_folder))
}

/// Persist generated video bytes to the session folder and return the absolute
/// path. Shared by all video-generation tools so the save + logging logic lives
/// in one place.
async fn save_video_bytes(
    client: &VideoGenClient,
    session_folder: &std::path::Path,
    video_bytes: &[u8],
) -> Result<std::path::PathBuf, xai_tool_runtime::ToolError> {
    let absolute_path = client
        .writer
        .save(session_folder, video_bytes, None)
        .await
        .map_err(|e| xai_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;

    tracing::info!(
        path = %absolute_path.display(),
        bytes = video_bytes.len(),
        "video saved to disk"
    );

    Ok(absolute_path)
}

async fn media_output_from_outcome(
    client: &VideoGenClient,
    session_folder: &std::path::Path,
    outcome: VideoOutcome,
) -> Result<MediaGenOutput, xai_tool_runtime::ToolError> {
    match outcome {
        VideoOutcome::Bytes(bytes) => {
            let path = save_video_bytes(client, session_folder, &bytes).await?;
            Ok(MediaGenOutput::new(path))
        }
        VideoOutcome::UploadedReference(reference) => Ok(MediaGenOutput::uploaded(reference)),
    }
}

#[derive(Debug, Default)]
pub struct ImageToVideoTool;

impl crate::types::tool_metadata::ToolMetadata for ImageToVideoTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ImageToVideo
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Generate a video from a single source image; returns the saved video's absolute path. When telling the user where it was saved, refer to it by its short session-relative path (e.g. `videos/1.mp4`) rather than the absolute path, so it renders as a clickable link that opens the video. Provide `image` for the image to animate and optionally a `prompt` to guide the animation. Use this tool when the user provides an image and wants it animated, turned into a video, or used as the first frame. Example: image_to_video(image="/Users/me/photo.jpg", prompt="gentle camera push-in with wind moving the hair", duration=6, resolution_name="480p")"##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ImageToVideoTool {
    type Args = ImageToVideoInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(IMAGE_TO_VIDEO_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            IMAGE_TO_VIDEO_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.image_to_video",
        skip_all,
        fields(prompt_len = input.prompt.as_deref().unwrap_or("").len(), duration = ?input.duration, resolution = %input.resolution_name)
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ImageToVideoInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        validate_imagine_duration(input.duration)?;
        validate_one_of(
            "resolution_name",
            &input.resolution_name,
            VALID_VIDEO_RESOLUTIONS,
        )?;
        let image = resolve_image_reference(&input.image).await?;
        let prompt = input.prompt.unwrap_or_default();

        let (client, session_folder) = acquire_video_client(&ctx).await?;

        // Free / X Basic users are zero-limited on Imagine server-side; return
        // the upsell prose instead of a doomed request.
        if client.is_tier_restricted() {
            return Ok(ToolOutput::Text(TIER_RESTRICTED_UPSELL.into()));
        }
        if client.is_zdr_restricted() {
            return Err(zdr_restricted_error());
        }

        let outcome = client
            .generate_with_images(
                XAI_VIDEO_MODEL,
                &prompt,
                Some(
                    input
                        .duration
                        .unwrap_or(DEFAULT_IMAGINE_VIDEO_DURATION_SECS),
                ),
                None,
                &input.resolution_name,
                Some(image),
                Vec::new(),
                Vec::new(),
            )
            .await?;

        let media = media_output_from_outcome(&client, &session_folder, outcome).await?;

        Ok(ToolOutput::ImageToVideo(media))
    }
}

#[derive(Debug, Default)]
pub struct ReferenceToVideoTool;

impl crate::types::tool_metadata::ToolMetadata for ReferenceToVideoTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ReferenceToVideo
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Generate a video from reference images and/or preset voices, guided by a required text prompt; returns the saved video's absolute path. When telling the user where it was saved, refer to it by its short session-relative path (e.g. `videos/1.mp4`) rather than the absolute path, so it renders as a clickable link that opens the video. Provide up to 7 `images` (style/content references: people, objects, clothing, settings) and/or up to 3 `voices` (preset voice identifiers the subjects speak in); at least one of either is required. Tag references in the prompt as `<IMAGE_0>`, `<IMAGE_1>`, ... and `<AUDIO_0>`, `<AUDIO_1>`, ... Use this tool when the user wants a video referencing existing images without locking the first frame, or wants a speaking subject with a specific voice. Example: reference_to_video(prompt="The person from <IMAGE_0> presents the product from <IMAGE_1>, speaking with the voice from <AUDIO_0>", images=["/Users/me/host.jpg", "/Users/me/product.jpg"], voices=["eve"], aspect_ratio="16:9", duration=10, resolution_name="480p")"##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ReferenceToVideoTool {
    type Args = ReferenceToVideoInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(REFERENCE_TO_VIDEO_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            REFERENCE_TO_VIDEO_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.reference_to_video",
        skip_all,
        fields(prompt_len = input.prompt.len(), num_images = input.images.len(), num_voices = input.voices.len(), aspect_ratio = %input.aspect_ratio, duration = ?input.duration, resolution = %input.resolution_name)
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ReferenceToVideoInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        if input.prompt.trim().is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "`prompt` must not be empty.",
            ));
        }
        if input.images.is_empty() && input.voices.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "Provide at least one reference: `images` (up to 7) and/or `voices` (up to 3).",
            ));
        }
        if input.images.len() > MAX_R2V_REFERENCE_IMAGES {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "`images` must contain at most {MAX_R2V_REFERENCE_IMAGES} image references."
            )));
        }
        if input.voices.len() > MAX_R2V_REFERENCE_VOICES {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "`voices` must contain at most {MAX_R2V_REFERENCE_VOICES} preset voices."
            )));
        }
        if input.voices.iter().any(|v| v.trim().is_empty()) {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "`voices` entries must be non-empty voice identifiers (e.g. \"ara\").",
            ));
        }
        validate_r2v_duration(input.duration)?;
        validate_one_of(
            "aspect_ratio",
            &input.aspect_ratio,
            VALID_IMAGINE_VIDEO_ASPECT_RATIOS,
        )?;
        validate_one_of(
            "resolution_name",
            &input.resolution_name,
            VALID_VIDEO_RESOLUTIONS,
        )?;

        let mut reference_images = Vec::with_capacity(input.images.len());
        for image in &input.images {
            reference_images.push(resolve_image_reference(image).await?);
        }

        let (client, session_folder) = acquire_video_client(&ctx).await?;

        // Free / X Basic users are zero-limited on Imagine server-side; return
        // the upsell prose instead of a doomed request.
        if client.is_tier_restricted() {
            return Ok(ToolOutput::Text(TIER_RESTRICTED_UPSELL.into()));
        }
        if client.is_zdr_restricted() {
            return Err(zdr_restricted_error());
        }

        let outcome = client
            .generate_with_images(
                XAI_VIDEO_MODEL,
                &input.prompt,
                Some(
                    input
                        .duration
                        .unwrap_or(DEFAULT_IMAGINE_VIDEO_DURATION_SECS),
                ),
                Some(&input.aspect_ratio),
                &input.resolution_name,
                None,
                reference_images,
                input.voices,
            )
            .await?;

        let media = media_output_from_outcome(&client, &session_folder, outcome).await?;

        Ok(ToolOutput::ReferenceToVideo(media))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool_metadata::test_ctx_with_call_id;

    fn assert_no_secret_windows(rendered: &str, secret: &str) {
        assert!(!rendered.contains(secret));
        for window in secret.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !rendered.contains(window),
                "leaked sentinel window: {window}"
            );
        }
    }

    #[test]
    fn config_debug_is_presence_only() {
        let secret = "GB002-video-config-secret-0123456789abcdef";
        let storage = ZdrVideoOutputS3Config {
            bucket: secret.to_owned(),
            endpoint: format!("https://user:{secret}@example.test/?token={secret}"),
            region: secret.to_owned(),
            key_prefix: secret.to_owned(),
            expires_secs: 900,
            read_write: S3AccessCredentials {
                access_key_id: secret.to_owned(),
                secret_access_key: secret.to_owned(),
            },
            read_only: None,
        };
        assert_no_secret_windows(&format!("{storage:?}"), secret);

        let config = VideoGenConfig::Enabled {
            api_key: secret.to_owned(),
            base_url: format!("https://user:{secret}@example.test/?token={secret}"),
            extra_headers: indexmap::IndexMap::from([(
                "Authorization".to_owned(),
                secret.to_owned(),
            )]),
            zdr_video_output_s3: Some(Box::new(storage)),
            tier_restricted: false,
            zdr_restricted: false,
        };
        assert_no_secret_windows(&format!("{config:?}"), secret);
    }

    #[test]
    fn invalid_presigned_url_error_does_not_echo_url() {
        let secret = "GB002-zqxv-token-0123456789abcdef";
        let invalid_url = format!("ftp://user:{secret}@example.test/?token={secret}");
        for message in [
            "Presigned video upload URL is invalid.",
            "Presigned video download URL is invalid.",
        ] {
            let error = ensure_http_url(&invalid_url, message)
                .unwrap_err()
                .to_string();
            assert_eq!(error, message);
            assert_no_secret_windows(&error, secret);
        }
    }

    #[test]
    fn invalid_credential_header_error_is_fixed_and_secret_free() {
        let secret = "GB002-zqxv-header-0123456789abcdef";
        let config = VideoGenConfig::Enabled {
            api_key: format!("{secret}\n"),
            base_url: "https://example.test".to_owned(),
            extra_headers: indexmap::IndexMap::new(),
            zdr_video_output_s3: None,
            tier_restricted: false,
            zdr_restricted: false,
        };
        let error = match VideoGenClient::new(&config, None) {
            Ok(_) => panic!("invalid credential header must fail"),
            Err(error) => error.to_string(),
        };
        assert_eq!(error, "Invalid API key for header.");
        assert_no_secret_windows(&error, secret);
    }

    #[tokio::test]
    async fn unreadable_image_reference_error_is_fixed_and_secret_free() {
        let secret = "GB002-zqxv-image-ref-0123456789abcdef";
        let error = resolve_image_reference(&format!("/missing/{secret}"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Image reference is not readable.");
        assert_no_secret_windows(&error, secret);
    }

    #[test]
    fn image_to_video_name_and_description() {
        let tool = ImageToVideoTool;
        assert_eq!(
            xai_tool_runtime::Tool::id(&tool).as_str(),
            IMAGE_TO_VIDEO_TOOL_NAME
        );
        let desc = crate::types::tool_metadata::ToolMetadata::description_template(&tool);
        assert!(desc.contains("single source image"));
        assert!(desc.contains("image_to_video"));
    }

    #[test]
    fn reference_to_video_name_and_description() {
        let tool = ReferenceToVideoTool;
        assert_eq!(
            xai_tool_runtime::Tool::id(&tool).as_str(),
            REFERENCE_TO_VIDEO_TOOL_NAME
        );
        let desc = crate::types::tool_metadata::ToolMetadata::description_template(&tool);
        assert!(desc.contains("reference images and/or preset voices"));
        assert!(desc.contains("reference_to_video"));
        assert!(desc.contains("<AUDIO_0>"));
    }

    #[test]
    fn image_to_video_defaults_match_toolbox() {
        let input: ImageToVideoInput =
            serde_json::from_str(r#"{"image":"/tmp/source.jpg"}"#).unwrap();
        assert_eq!(input.prompt, None);
        assert_eq!(input.duration, None);
        assert_eq!(input.resolution_name, DEFAULT_RESOLUTION);
    }

    #[test]
    fn reference_to_video_input_deserializes() {
        let input: ReferenceToVideoInput = serde_json::from_str(
            r#"{"prompt":"blend these","images":["/tmp/a.jpg","/tmp/b.jpg"],"aspect_ratio":"16:9","duration":"10"}"#,
        )
        .unwrap();
        assert_eq!(input.prompt, "blend these");
        assert_eq!(input.images.len(), 2);
        assert!(input.voices.is_empty());
        assert_eq!(input.aspect_ratio, "16:9");
        assert_eq!(input.duration, Some(10));
        assert_eq!(input.resolution_name, DEFAULT_RESOLUTION);
    }

    #[test]
    fn reference_to_video_input_deserializes_voices() {
        let input: ReferenceToVideoInput = serde_json::from_str(
            r#"{"prompt":"the subject speaks","voices":["ara","eve"],"aspect_ratio":"16:9"}"#,
        )
        .unwrap();
        assert!(input.images.is_empty());
        assert_eq!(input.voices, vec!["ara", "eve"]);
    }

    #[test]
    fn imagine_duration_validation_allows_only_toolbox_values() {
        assert!(validate_imagine_duration(None).is_ok());
        assert!(validate_imagine_duration(Some(6)).is_ok());
        assert!(validate_imagine_duration(Some(10)).is_ok());
        assert!(validate_imagine_duration(Some(8)).is_err());
    }

    #[test]
    fn image_and_reference_payload_fields_are_serialized() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "animate",
            image: Some(VideoImageUrl {
                url: "data:image/png;base64,a".to_owned(),
            }),
            duration: Some(6),
            aspect_ratio: None,
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            reference_audios: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["image"]["url"], "data:image/png;base64,a");
        assert!(json.get("aspect_ratio").is_none());
        assert!(json.get("output").is_none());

        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "blend",
            image: None,
            duration: Some(6),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: vec![
                VideoImageUrl {
                    url: "data:image/png;base64,a".to_owned(),
                },
                VideoImageUrl {
                    url: "data:image/png;base64,b".to_owned(),
                },
            ],
            reference_audios: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["reference_images"].as_array().unwrap().len(), 2);
        assert_eq!(json["aspect_ratio"], "16:9");
    }

    #[test]
    fn output_upload_url_serialized_when_present() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "animate",
            image: None,
            duration: Some(6),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            reference_audios: Vec::new(),
            output: Some(VideoOutput {
                upload_url: "https://bucket.example.com/signed-put".to_owned(),
            }),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json["output"]["upload_url"],
            "https://bucket.example.com/signed-put"
        );
    }

    #[test]
    fn zdr_presign_expires_secs_clamps_below_minimum() {
        // Below minimum → clamped up.
        assert_eq!(
            zdr_presign_expires_secs(60),
            MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
        );
        assert_eq!(
            zdr_presign_expires_secs(0),
            MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
        );
        // At or above minimum → passthrough.
        assert_eq!(
            zdr_presign_expires_secs(MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS),
            MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
        );
        let large = MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS + 600;
        assert_eq!(zdr_presign_expires_secs(large), large);
    }

    #[test]
    fn zdr_select_get_credentials() {
        let rw = S3AccessCredentials {
            access_key_id: "rw".into(),
            secret_access_key: "rw-secret".into(),
        };
        let mut config = ZdrVideoOutputS3Config {
            bucket: "b".into(),
            endpoint: "https://s3.example.com".into(),
            region: "us-east-1".into(),
            key_prefix: String::new(),
            expires_secs: DEFAULT_ZDR_VIDEO_PRESIGN_EXPIRES_SECS,
            read_write: rw.clone(),
            read_only: None,
        };

        let (creds, source) = zdr_get_credentials(&config);
        assert_eq!((source, creds.access_key_id.as_str()), ("read_write", "rw"));

        config.read_only = Some(S3AccessCredentials {
            access_key_id: "ro".into(),
            secret_access_key: "ro-secret".into(),
        });
        let (creds, source) = zdr_get_credentials(&config);
        assert_eq!((source, creds.access_key_id.as_str()), ("read_only", "ro"));

        config.read_only = Some(S3AccessCredentials {
            access_key_id: "   ".into(),
            secret_access_key: String::new(),
        });
        let (creds, source) = zdr_get_credentials(&config);
        assert_eq!((source, creds.access_key_id.as_str()), ("read_write", "rw"));
    }

    #[test]
    fn zdr_video_output_s3_config_deserializes() {
        let cfg: ZdrVideoOutputS3Config = serde_json::from_value(serde_json::json!({
            "bucket": "team-videos",
            "endpoint": "https://s3.example.com",
            "region": "us-east-1",
            "read_write": {
                "access_key_id": "AKIATEST",
                "secret_access_key": "secret",
            },
        }))
        .unwrap();
        assert!(cfg.is_valid());
    }

    #[test]
    fn zdr_video_object_key_normalizes_prefix() {
        // No prefix → bare UUID.mp4.
        let key = zdr_video_object_key("");
        assert!(key.ends_with(".mp4"), "key must end with .mp4: {key}");
        assert!(!key.starts_with('/'), "bare key must not start with /");

        // Prefix with trailing slash → preserved.
        let key = zdr_video_object_key("team/videos/");
        assert!(
            key.starts_with("team/videos/"),
            "prefix must be preserved: {key}"
        );
        assert!(key.ends_with(".mp4"));

        // Prefix without trailing slash → slash appended.
        let key = zdr_video_object_key("team/videos");
        assert!(
            key.starts_with("team/videos/"),
            "trailing / must be added: {key}"
        );

        // Whitespace-only prefix → treated as empty.
        let key = zdr_video_object_key("   ");
        assert!(
            !key.contains(' '),
            "whitespace prefix must be trimmed: {key}"
        );
        assert!(key.ends_with(".mp4"));

        // Two calls produce different keys (UUID uniqueness).
        let a = zdr_video_object_key("v/");
        let b = zdr_video_object_key("v/");
        assert_ne!(a, b, "object keys must be unique across calls");
    }

    #[test]
    fn zdr_upload_fallback_reference_excludes_credentials_and_presigned_urls() {
        const ACCESS_SECRET: &str = "GB002-video-access-secret-0123456789";
        const SIGNING_SECRET: &str = "GB002-video-signing-secret-9876543210";
        const QUERY_SECRET: &str = "GB002-video-query-secret-2468135790";
        let config = ZdrVideoOutputS3Config {
            bucket: "team-videos".into(),
            endpoint: format!("https://user:{QUERY_SECRET}@s3.example.test/?token={QUERY_SECRET}"),
            region: "us-east-1".into(),
            key_prefix: "generated/session".into(),
            expires_secs: 600,
            read_write: S3AccessCredentials {
                access_key_id: ACCESS_SECRET.into(),
                secret_access_key: SIGNING_SECRET.into(),
            },
            read_only: None,
        };
        let urls = ZdrPresignedUrls {
            object_key: "generated/session/video.mp4".into(),
            upload_url: format!("https://upload.example.test/?signature={QUERY_SECRET}"),
            get_url: Some(format!(
                "https://download.example.test/?signature={QUERY_SECRET}"
            )),
            expires_in: std::time::Duration::from_secs(600),
        };
        let VideoOutcome::UploadedReference(reference) =
            zdr_uploaded_reference_outcome(&config, &urls)
        else {
            panic!("download failure must preserve upload success")
        };

        assert_eq!(reference, "s3://team-videos/generated/session/video.mp4");
        for secret in [ACCESS_SECRET, SIGNING_SECRET, QUERY_SECRET] {
            assert!(!reference.contains(secret));
            for window in secret.as_bytes().windows(8) {
                assert!(!reference.contains(std::str::from_utf8(window).unwrap()));
            }
        }
        assert!(!reference.contains('?'));
    }

    #[test]
    fn is_http_url_validates_scheme() {
        assert!(is_http_url("https://bucket.example.com/signed?token=abc"));
        assert!(is_http_url("http://localhost:9000/test"));
        assert!(!is_http_url("ftp://files.example.com/video.mp4"));
        assert!(!is_http_url("file:///tmp/video.mp4"));
        assert!(!is_http_url("not-a-url"));
        assert!(!is_http_url(""));
    }

    #[tokio::test]
    async fn image_to_video_rejects_bad_duration() {
        let tool = ImageToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ImageToVideoInput {
                prompt: None,
                image: "/tmp/source.jpg".into(),
                duration: Some(8),
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected invalid duration error");
        assert!(err.to_string().contains("either 6 or 10"));
    }

    #[tokio::test]
    async fn reference_to_video_rejects_bad_aspect_ratio() {
        let tool = ReferenceToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ReferenceToVideoInput {
                prompt: "blend".into(),
                images: vec!["/tmp/a.jpg".into(), "/tmp/b.jpg".into()],
                voices: Vec::new(),
                aspect_ratio: "21:9".into(),
                duration: None,
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected aspect ratio error");
        assert!(err.to_string().contains("aspect_ratio"));
    }

    #[tokio::test]
    async fn image_to_video_rejects_bad_resolution() {
        let tool = ImageToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ImageToVideoInput {
                prompt: None,
                image: "/tmp/source.jpg".into(),
                duration: None,
                resolution_name: "1080p".into(),
            },
        )
        .await
        .expect_err("Expected resolution error");
        assert!(err.to_string().contains("resolution_name"));
    }

    #[tokio::test]
    async fn reference_to_video_rejects_no_references() {
        let tool = ReferenceToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ReferenceToVideoInput {
                prompt: "blend".into(),
                images: Vec::new(),
                voices: Vec::new(),
                aspect_ratio: "16:9".into(),
                duration: None,
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected missing references error");
        assert!(err.to_string().contains("at least one reference"));
    }

    #[tokio::test]
    async fn reference_to_video_rejects_too_many_voices() {
        let tool = ReferenceToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ReferenceToVideoInput {
                prompt: "speak".into(),
                images: Vec::new(),
                voices: vec!["ara".into(), "eve".into(), "leo".into(), "rex".into()],
                aspect_ratio: "16:9".into(),
                duration: None,
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected voice count error");
        assert!(err.to_string().contains("at most 3 preset voices"));
    }

    #[tokio::test]
    async fn reference_to_video_rejects_out_of_range_duration() {
        let tool = ReferenceToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ReferenceToVideoInput {
                prompt: "speak".into(),
                images: Vec::new(),
                voices: vec!["ara".into()],
                aspect_ratio: "16:9".into(),
                duration: Some(16),
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected duration error");
        assert!(err.to_string().contains("between 1 and 15"));
    }

    #[test]
    fn non_numeric_duration_parse_error_is_range_agnostic() {
        let err =
            serde_json::from_str::<ImageToVideoInput>(r#"{"image":"/tmp/a.jpg","duration":"abc"}"#)
                .expect_err("expected parse error");
        assert!(err.to_string().contains("whole number of seconds"));

        let err = serde_json::from_str::<ReferenceToVideoInput>(
            r#"{"prompt":"x","voices":["ara"],"aspect_ratio":"16:9","duration":"abc"}"#,
        )
        .expect_err("expected parse error");
        assert!(err.to_string().contains("whole number of seconds"));
    }

    #[test]
    fn r2v_duration_validation_accepts_full_range() {
        assert!(validate_r2v_duration(None).is_ok());
        assert!(validate_r2v_duration(Some(1)).is_ok());
        assert!(validate_r2v_duration(Some(15)).is_ok());
        assert!(validate_r2v_duration(Some(0)).is_err());
        assert!(validate_r2v_duration(Some(16)).is_err());
    }

    #[test]
    fn reference_audios_serialized_as_voice_ids() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "the subject speaks",
            image: None,
            duration: Some(10),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            reference_audios: vec![
                VideoVoiceId {
                    voice_id: "ara".to_owned(),
                },
                VideoVoiceId {
                    voice_id: "eve".to_owned(),
                },
            ],
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["reference_audios"][0]["voice_id"], "ara");
        assert_eq!(json["reference_audios"][1]["voice_id"], "eve");

        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "no voices",
            image: None,
            duration: Some(6),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            reference_audios: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("reference_audios").is_none());
    }

    #[tokio::test]
    async fn provider_error_body_never_reaches_video_generation_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let secret = "video-secret-0123456789";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/videos/generations"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_string(format!("provider echoed {secret} and {}", &secret[4..16])),
            )
            .mount(&server)
            .await;
        let config = VideoGenConfig::Enabled {
            api_key: secret.into(),
            base_url: server.uri(),
            extra_headers: indexmap::IndexMap::new(),
            zdr_video_output_s3: None,
            tier_restricted: false,
            zdr_restricted: false,
        };
        let client = VideoGenClient::new(&config, None).unwrap();
        let result = client
            .generate_with_images(
                XAI_VIDEO_MODEL,
                "prompt",
                Some(6),
                Some("16:9"),
                "480p",
                None,
                Vec::new(),
                Vec::new(),
            )
            .await;
        let error = match result {
            Ok(_) => panic!("provider failure must not succeed"),
            Err(error) => error.to_string(),
        };
        assert!(!error.contains(secret));
        for window in secret.as_bytes().windows(8) {
            assert!(!error.contains(std::str::from_utf8(window).unwrap()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn provider_request_id_never_reaches_video_generation_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let secret = "video_request_SENTINEL_0123456789abcdef";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/videos/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "request_id": secret,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/videos/{secret}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "failed",
            })))
            .mount(&server)
            .await;
        let config = VideoGenConfig::Enabled {
            api_key: secret.into(),
            base_url: server.uri(),
            extra_headers: indexmap::IndexMap::new(),
            zdr_video_output_s3: None,
            tier_restricted: false,
            zdr_restricted: false,
        };
        let client = VideoGenClient::new(&config, None).unwrap();
        let outcome = client
            .generate_with_images(
                XAI_VIDEO_MODEL,
                "prompt",
                Some(6),
                Some("16:9"),
                "480p",
                None,
                Vec::new(),
                Vec::new(),
            )
            .await;
        let error = match outcome {
            Ok(_) => panic!("provider failure must not succeed"),
            Err(error) => error.to_string(),
        };
        assert_no_secret_windows(&error, secret);
    }

    #[test]
    fn omitted_duration_is_dropped_from_wire_payload() {
        // Regression: an unset `duration` must not be serialized at all
        // (no `null`, no synthetic default) so the server's default applies.
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "test",
            image: None,
            duration: None,
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            reference_audios: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            json.get("duration").is_none(),
            "duration must be omitted, got: {json:?}"
        );
    }

    #[test]
    fn explicit_duration_is_present_on_wire() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_MODEL,
            prompt: "test",
            image: None,
            duration: Some(12),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            reference_audios: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json.get("duration"), Some(&serde_json::Value::from(12)));
    }
}
