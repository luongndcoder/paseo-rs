//! Wire JSON shapes. Field names and null/omitted semantics follow
//! `packages/protocol/src/messages.ts`: `null` fields are always present,
//! `undefined` fields are omitted.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExplorerEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExplorerFileKind {
    Text,
    Image,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorerEncoding {
    #[serde(rename = "utf-8")]
    Utf8,
    #[serde(rename = "base64")]
    Base64,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileExplorerEntry {
    pub name: String,
    pub path: String,
    pub kind: ExplorerEntryKind,
    pub size: u64,
    pub modified_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileExplorerDirectory {
    pub path: String,
    pub entries: Vec<FileExplorerEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileExplorerFile {
    pub path: String,
    pub kind: ExplorerFileKind,
    pub encoding: ExplorerEncoding,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mime_type: Option<String>,
    pub size: u64,
    pub modified_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ExplorerFileVersion {
    Ready {
        cwd: String,
        path: String,
        size: u64,
        modified_at: String,
        revision: String,
    },
    Missing {
        cwd: String,
        path: String,
    },
    Error {
        cwd: String,
        path: String,
        error: String,
    },
}

/// `FileWriteResultSchema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ExplorerFileWriteResult {
    Written {
        modified_at: String,
        size: u64,
        revision: String,
    },
    Conflict {
        version: ExplorerFileVersion,
    },
    Error {
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExplorerMode {
    List,
    File,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileExplorerRequest {
    pub cwd: String,
    #[serde(default)]
    pub path: Option<String>,
    pub mode: ExplorerMode,
    pub request_id: String,
    #[serde(default)]
    pub accept_binary: Option<bool>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileExplorerResponsePayload {
    pub cwd: String,
    pub path: String,
    pub mode: ExplorerMode,
    pub directory: Option<FileExplorerDirectory>,
    pub file: Option<FileExplorerFile>,
    pub error: Option<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileExplorerResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub payload: FileExplorerResponsePayload,
}

impl FileExplorerResponse {
    pub fn new(payload: FileExplorerResponsePayload) -> Self {
        Self {
            kind: "file_explorer_response",
            payload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectorySuggestionEntry {
    pub path: String,
    pub kind: SuggestionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    Fuzzy,
    Suffix,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySuggestionsRequest {
    pub query: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub include_files: Option<bool>,
    #[serde(default)]
    pub include_directories: Option<bool>,
    #[serde(default)]
    pub match_mode: Option<MatchMode>,
    #[serde(default)]
    pub limit: Option<i64>,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySuggestionsResponsePayload {
    pub directories: Vec<String>,
    pub entries: Vec<DirectorySuggestionEntry>,
    pub error: Option<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DirectorySuggestionsResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub payload: DirectorySuggestionsResponsePayload,
}

impl DirectorySuggestionsResponse {
    pub fn new(payload: DirectorySuggestionsResponsePayload) -> Self {
        Self {
            kind: "directory_suggestions_response",
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDownloadTokenRequest {
    pub cwd: String,
    pub path: String,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDownloadTokenResponsePayload {
    pub cwd: String,
    pub path: String,
    pub token: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub error: Option<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileDownloadTokenResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub payload: FileDownloadTokenResponsePayload,
}

impl FileDownloadTokenResponse {
    pub fn new(payload: FileDownloadTokenResponsePayload) -> Self {
        Self {
            kind: "file_download_token_response",
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUploadRequest {
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
    pub modified_at: String,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadedFileAttachment {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUploadResponsePayload {
    pub request_id: String,
    pub file: Option<UploadedFileAttachment>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileUploadResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub payload: FileUploadResponsePayload,
}

impl FileUploadResponse {
    pub fn new(payload: FileUploadResponsePayload) -> Self {
        Self {
            kind: "file.upload.response",
            payload,
        }
    }
}

/// `fs.file.write.response` payload wrapper.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileWriteResponsePayload {
    pub result: ExplorerFileWriteResult,
    pub request_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explorer_file_omits_undefined_fields() {
        let file = FileExplorerFile {
            path: "a".into(),
            kind: ExplorerFileKind::Binary,
            encoding: ExplorerEncoding::None,
            content: None,
            mime_type: Some("application/octet-stream".into()),
            size: 5,
            modified_at: "2026-01-01T00:00:00.000Z".into(),
            revision: Some("1:2:5:3".into()),
        };
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            json!({"path":"a","kind":"binary","encoding":"none","mimeType":"application/octet-stream","size":5,"modifiedAt":"2026-01-01T00:00:00.000Z","revision":"1:2:5:3"})
        );
    }

    #[test]
    fn version_and_write_result_tags() {
        let v = ExplorerFileVersion::Missing {
            cwd: "/w".into(),
            path: "x".into(),
        };
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            json!({"status":"missing","cwd":"/w","path":"x"})
        );
        let r = ExplorerFileWriteResult::Written {
            modified_at: "m".into(),
            size: 1,
            revision: "r".into(),
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"status":"written","modifiedAt":"m","size":1,"revision":"r"})
        );
        let c = ExplorerFileWriteResult::Conflict { version: v };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({"status":"conflict","version":{"status":"missing","cwd":"/w","path":"x"}})
        );
    }

    #[test]
    fn requests_deserialize_from_wire_json() {
        let req: FileExplorerRequest = serde_json::from_value(json!({
            "type":"file_explorer_request","cwd":"/w","mode":"file","requestId":"r1",
            "acceptBinary":true,"maxBytes":10
        }))
        .unwrap();
        assert_eq!(req.mode, ExplorerMode::File);
        assert_eq!(req.accept_binary, Some(true));
        let req: DirectorySuggestionsRequest = serde_json::from_value(json!({
            "type":"directory_suggestions_request","query":"q","requestId":"r","matchMode":"suffix","limit":5
        }))
        .unwrap();
        assert_eq!(req.match_mode, Some(MatchMode::Suffix));
    }

    #[test]
    fn response_envelopes_carry_type_and_nulls() {
        let resp = FileExplorerResponse::new(FileExplorerResponsePayload {
            cwd: "/w".into(),
            path: ".".into(),
            mode: ExplorerMode::List,
            directory: None,
            file: None,
            error: Some("boom".into()),
            request_id: "r".into(),
        });
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({"type":"file_explorer_response","payload":{"cwd":"/w","path":".","mode":"list","directory":null,"file":null,"error":"boom","requestId":"r"}})
        );
        let up = FileUploadResponse::new(FileUploadResponsePayload {
            request_id: "r".into(),
            file: None,
            error: Some("e".into()),
        });
        assert_eq!(
            serde_json::to_value(&up).unwrap(),
            json!({"type":"file.upload.response","payload":{"requestId":"r","file":null,"error":"e"}})
        );
    }
}
