//! Reevo API client for AgentFS filesystem operations.
//! Uses ureq (purely synchronous, no background threads) to avoid TLS
//! destruction panics when used from an LD_PRELOAD shim at process exit.

use std::io::Read;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ReevoClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
    user_id: Option<String>,
    org_id: Option<String>,
}

// -- API response types --

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
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .redirects(3)
            .build();

        Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            user_id: user_id.map(|s| s.to_string()),
            org_id: org_id.map(|s| s.to_string()),
        }
    }

    fn add_headers(&self, req: ureq::Request) -> ureq::Request {
        let mut req = req.set("Content-Type", "application/json");
        if !self.token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", self.token));
        }
        if let Some(ref user_id) = self.user_id {
            req = req.set("x-reevo-user-id", user_id);
        }
        if let Some(ref org_id) = self.org_id {
            req = req.set("x-reevo-org-id", org_id);
        }
        req
    }

    fn fs_url(&self, namespace: &str, scope: &str, path: &str) -> String {
        let clean_path = path.trim_start_matches('/');
        format!(
            "{}/api/v2/fs/{}/{}/{}",
            self.base_url, namespace, scope, clean_path
        )
    }

    /// Read a file's raw bytes.
    ///
    /// Follows redirects (the backend returns 302 → presigned S3 URL for large
    /// files). ureq strips the `Authorization` header across hosts by default,
    /// so the bearer token is not leaked to S3.
    pub fn read_file(
        &self,
        namespace: &str,
        scope: &str,
        path: &str,
    ) -> Result<Vec<u8>, ApiError> {
        let url = self.fs_url(namespace, scope, path);
        let req = self.add_headers(self.agent.get(&url));
        match req.call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader()
                    .read_to_end(&mut buf)
                    .map_err(|e| ApiError::Network(e.to_string()))?;
                Ok(buf)
            }
            Err(ureq::Error::Status(404, _)) => Err(ApiError::NotFound),
            Err(ureq::Error::Status(403, _)) => Err(ApiError::Forbidden),
            Err(ureq::Error::Status(415, _)) => Err(ApiError::Forbidden),
            Err(ureq::Error::Status(400, resp)) => Err(ApiError::BadRequest(
                resp.into_string().unwrap_or_default(),
            )),
            Err(e) => Err(ApiError::Network(e.to_string())),
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
        let req = self.add_headers(self.agent.put(&url));
        match req.send_json(ureq::json!(&body)) {
            Ok(resp) => resp.into_json::<WriteFileResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            Err(ureq::Error::Status(403, _)) => Err(ApiError::Forbidden),
            Err(ureq::Error::Status(400, resp)) => Err(ApiError::BadRequest(
                resp.into_string().unwrap_or_default(),
            )),
            Err(e) => Err(ApiError::Network(e.to_string())),
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
        let req = self.add_headers(self.agent.delete(&url));
        match req.call() {
            Ok(resp) => resp.into_json::<DeleteFileResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            Err(ureq::Error::Status(404, _)) => Err(ApiError::NotFound),
            Err(ureq::Error::Status(403, _)) => Err(ApiError::Forbidden),
            Err(e) => Err(ApiError::Network(e.to_string())),
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
        let req = self.add_headers(self.agent.post(&url));
        match req.send_json(ureq::json!(&body)) {
            Ok(resp) => resp.into_json::<ListDirectoryResponse>()
                .map_err(|e| ApiError::Network(e.to_string())),
            Err(ureq::Error::Status(404, _)) => Err(ApiError::NotFound),
            Err(ureq::Error::Status(403, _)) => Err(ApiError::Forbidden),
            Err(e) => Err(ApiError::Network(e.to_string())),
        }
    }
}
