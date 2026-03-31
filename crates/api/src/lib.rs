//! Reevo API client for AgentFS filesystem operations.

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ReevoClient {
    client: Client,
    base_url: String,
    token: String,
    /// Reevo user ID (passed as x-reevo-user-id header)
    user_id: Option<String>,
    /// Reevo org ID (passed as x-reevo-org-id header)
    org_id: Option<String>,
}

// -- API response types --

#[derive(Debug, Deserialize)]
pub struct FileContentResponse {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct WriteFileRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct WriteFileResponse {
    pub success: bool,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteFileResponse {
    pub success: bool,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub is_directory: bool,
}

#[derive(Debug, Serialize)]
pub struct ListDirectoryRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct ListDirectoryResponse {
    pub path: String,
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    Forbidden,
    BadRequest(String),
    Network(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound => write!(f, "not found"),
            ApiError::Forbidden => write!(f, "forbidden"),
            ApiError::BadRequest(msg) => write!(f, "bad request: {msg}"),
            ApiError::Network(msg) => write!(f, "network error: {msg}"),
        }
    }
}

impl ReevoClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self::with_ids(base_url, token, None, None)
    }

    pub fn with_ids(
        base_url: &str,
        token: &str,
        user_id: Option<&str>,
        org_id: Option<&str>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            user_id: user_id.map(|s| s.to_string()),
            org_id: org_id.map(|s| s.to_string()),
        }
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !self.token.is_empty() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
            );
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(ref user_id) = self.user_id {
            headers.insert(
                "x-reevo-user-id",
                HeaderValue::from_str(user_id).unwrap(),
            );
        }
        if let Some(ref org_id) = self.org_id {
            headers.insert(
                "x-reevo-org-id",
                HeaderValue::from_str(org_id).unwrap(),
            );
        }
        headers
    }

    fn fs_url(&self, namespace: &str, scope: &str, path: &str) -> String {
        let clean_path = path.trim_start_matches('/');
        format!(
            "{}/api/v2/fs/{}/{}/{}",
            self.base_url, namespace, scope, clean_path
        )
    }

    /// Read a file's content.
    pub fn read_file(
        &self,
        namespace: &str,
        scope: &str,
        path: &str,
    ) -> Result<FileContentResponse, ApiError> {
        let url = self.fs_url(namespace, scope, path);
        let resp = self
            .client
            .get(&url)
            .headers(self.headers())
            .send()
            .map_err(|e| ApiError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => resp
                .json::<FileContentResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            404 => Err(ApiError::NotFound),
            403 => Err(ApiError::Forbidden),
            400 => Err(ApiError::BadRequest(
                resp.text().unwrap_or_default(),
            )),
            _ => Err(ApiError::Network(format!(
                "unexpected status: {}",
                resp.status()
            ))),
        }
    }

    /// Write (create or overwrite) a file.
    pub fn write_file(
        &self,
        namespace: &str,
        scope: &str,
        path: &str,
        content: &str,
    ) -> Result<WriteFileResponse, ApiError> {
        let url = self.fs_url(namespace, scope, path);
        let body = WriteFileRequest {
            content: content.to_string(),
        };
        let resp = self
            .client
            .put(&url)
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e| ApiError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => resp
                .json::<WriteFileResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            403 => Err(ApiError::Forbidden),
            400 => Err(ApiError::BadRequest(
                resp.text().unwrap_or_default(),
            )),
            _ => Err(ApiError::Network(format!(
                "unexpected status: {}",
                resp.status()
            ))),
        }
    }

    /// Delete a file or directory.
    pub fn delete_file(
        &self,
        namespace: &str,
        scope: &str,
        path: &str,
    ) -> Result<DeleteFileResponse, ApiError> {
        let url = self.fs_url(namespace, scope, path);
        let resp = self
            .client
            .delete(&url)
            .headers(self.headers())
            .send()
            .map_err(|e| ApiError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => resp
                .json::<DeleteFileResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            404 => Err(ApiError::NotFound),
            403 => Err(ApiError::Forbidden),
            _ => Err(ApiError::Network(format!(
                "unexpected status: {}",
                resp.status()
            ))),
        }
    }

    /// List directory contents.
    pub fn list_dir(
        &self,
        namespace: &str,
        scope: &str,
        path: &str,
    ) -> Result<ListDirectoryResponse, ApiError> {
        let url = format!(
            "{}/api/v2/fs/{}/{}/_list",
            self.base_url, namespace, scope
        );
        let body = ListDirectoryRequest {
            path: path.to_string(),
        };
        let resp = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e| ApiError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => resp
                .json::<ListDirectoryResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            404 => Err(ApiError::NotFound),
            403 => Err(ApiError::Forbidden),
            _ => Err(ApiError::Network(format!(
                "unexpected status: {}",
                resp.status()
            ))),
        }
    }
}
