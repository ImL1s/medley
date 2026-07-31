//! Upload destination config and archive-restore metadata shared by the
//! always-on upload queue and session restore paths.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Method for uploading to object storage.
#[derive(Clone)]
pub enum UploadMethod {
    Direct {
        service_account_key: Option<String>,
    },
    Proxy {
        proxy_base_url: String,
        user_token: String,
        deployment_key: Option<String>,
        alpha_test_key: Option<String>,
    },
    S3 {
        bucket: String,
        region: String,
        credentials_file: Option<String>,
        credentials_content: Option<String>,
        endpoint_url: Option<String>,
    },
}

impl std::fmt::Debug for UploadMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct {
                service_account_key,
            } => f
                .debug_struct("Direct")
                .field(
                    "service_account_key_present",
                    &service_account_key.is_some(),
                )
                .finish(),
            Self::Proxy {
                user_token,
                deployment_key,
                alpha_test_key,
                ..
            } => f
                .debug_struct("Proxy")
                .field("user_token_present", &!user_token.is_empty())
                .field("deployment_key_present", &deployment_key.is_some())
                .field("alpha_test_key_present", &alpha_test_key.is_some())
                .finish(),
            Self::S3 {
                credentials_file,
                credentials_content,
                endpoint_url,
                ..
            } => f
                .debug_struct("S3")
                .field("credentials_file_present", &credentials_file.is_some())
                .field(
                    "credentials_content_present",
                    &credentials_content.is_some(),
                )
                .field("endpoint_url_present", &endpoint_url.is_some())
                .finish(),
        }
    }
}

/// Configuration for object-storage export.
#[derive(Clone)]
pub struct TraceExportConfig {
    pub bucket_url: Option<String>,
    pub service_account_key: Option<String>,
    pub upload_method: UploadMethod,
    pub prefix_dir: Option<String>,
    pub gcs_prefix: Option<String>,
    pub absolute_paths: bool,
    pub archive_name_override: Option<String>,
}

impl std::fmt::Debug for TraceExportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceExportConfig")
            .field("bucket_url_present", &self.bucket_url.is_some())
            .field(
                "service_account_key_present",
                &self.service_account_key.is_some(),
            )
            .field("upload_method", &self.upload_method)
            .field("prefix_dir_present", &self.prefix_dir.is_some())
            .field("gcs_prefix_present", &self.gcs_prefix.is_some())
            .field("absolute_paths", &self.absolute_paths)
            .field(
                "archive_name_override_present",
                &self.archive_name_override.is_some(),
            )
            .finish()
    }
}

#[cfg(test)]
mod credential_debug_tests {
    use super::*;

    #[test]
    fn upload_config_debug_redacts_all_secret_carriers() {
        let sentinels = [
            "bucket-SENTINEL",
            "service-SENTINEL",
            "token-SENTINEL",
            "deployment-SENTINEL",
            "alpha-SENTINEL",
            "prefix-SENTINEL",
        ];
        let config = TraceExportConfig {
            bucket_url: Some(sentinels[0].into()),
            service_account_key: Some(sentinels[1].into()),
            upload_method: UploadMethod::Proxy {
                proxy_base_url: "https://proxy.example/secret?key=SENTINEL".into(),
                user_token: sentinels[2].into(),
                deployment_key: Some(sentinels[3].into()),
                alpha_test_key: Some(sentinels[4].into()),
            },
            prefix_dir: Some(sentinels[5].into()),
            gcs_prefix: Some("gcs-SENTINEL".into()),
            absolute_paths: false,
            archive_name_override: Some("archive-SENTINEL".into()),
        };
        let debug = format!("{config:?}");
        for sentinel in
            sentinels
                .into_iter()
                .chain(["proxy.example", "gcs-SENTINEL", "archive-SENTINEL"])
        {
            assert!(
                !debug.contains(sentinel),
                "debug leaked {sentinel:?}: {debug}"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobCompression {
    #[default]
    None,
    Zstd,
}

pub const SKIP_DIR_NAMES: &[&str] = &[
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".env",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".output",
    ".cache",
    ".parcel-cache",
    ".turbo",
    "vendor",
    "bower_components",
    ".tox",
    ".nox",
    ".eggs",
    ".idea",
    ".vscode",
    ".gradle",
    ".dart_tool",
    "coverage",
    ".nyc_output",
    "htmlcov",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];

pub fn skip_dir_set() -> &'static std::collections::HashSet<&'static str> {
    use std::collections::HashSet;
    use std::sync::LazyLock;
    static SET: LazyLock<HashSet<&str>> =
        LazyLock::new(|| SKIP_DIR_NAMES.iter().copied().collect());
    &SET
}

pub const SKIP_FILE_PATTERNS: &[&str] = &[
    "*.egg-info",
    "*.pyc",
    "*.pyo",
    "*.o",
    "*.so",
    "*.dylib",
    "*.class",
    "*.jar",
    ".DS_Store",
    "Thumbs.db",
    "*.swp",
    "*.swo",
    "*~",
    "*.iml",
];

pub fn default_untracked_exclude_globs() -> Vec<String> {
    let mut globs: Vec<String> = SKIP_DIR_NAMES.iter().map(|d| format!("{d}/")).collect();
    globs.extend(SKIP_FILE_PATTERNS.iter().map(|p| p.to_string()));
    globs
}

pub fn default_excludes_as_gitignore() -> String {
    default_untracked_exclude_globs().join("\n")
}

pub const ARCHIVE_SCHEMA_VERSION: &str = "v2";
pub const ARCHIVE_SCHEMA_VERSION_V3: &str = "v3";
pub const DEDUP_GCS_PREFIX: &str = "repo_changes_dedup";
pub const DEDUP_PATCH_SUBDIR: &str = "patches";
pub const DEDUP_BLOB_SUBDIR: &str = "blobs";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchReference {
    #[serde(rename = "type")]
    pub ref_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReference {
    #[serde(rename = "type")]
    pub ref_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExcludedContent {
    pub path: String,
    pub reason: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_archive_url: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub patch_references: HashMap<String, PatchReference>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub file_references: HashMap<String, FileReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ExcludedContent>,
}
