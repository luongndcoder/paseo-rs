//! Request handlers that turn WebSocket requests into wire responses. They hold
//! the policy the TS session applies on top of the file modules: `maxBytes`,
//! binary streaming negotiation, and the `directory_suggestions_request` search
//! configuration for workspace versus home searches.

use crate::error::ExplorerError;
use crate::explorer::{
    emit_file_transfer_frames, get_downloadable_file_info, list_directory_entries,
    read_explorer_file, ListDirectoryParams, ReadFileParams,
};
use crate::path_utils::home_dir;
use crate::suggestions::{
    search_directory_entries, BlankQueryBehavior, PathFormat, PathQueryPolicy, SearchOptions,
    WORKSPACE_SEARCH_HIDDEN_DIRECTORIES,
};
use crate::token_store::{DownloadTokenInput, DownloadTokenStore};
use crate::wire::{
    DirectorySuggestionsRequest, DirectorySuggestionsResponse, DirectorySuggestionsResponsePayload,
    ExplorerMode, FileDownloadTokenRequest, FileDownloadTokenResponse,
    FileDownloadTokenResponsePayload, FileExplorerRequest, FileExplorerResponse,
    FileExplorerResponsePayload, SuggestionKind,
};

pub enum FileExplorerOutcome {
    /// Send this JSON message.
    Response(Box<FileExplorerResponse>),
    /// The file went out as binary frames; nothing else to send.
    Streamed,
}

/// Handle `file_explorer_request`. `emit_binary` is `Some` when the client
/// accepts binary frames and the connection has a binary channel.
pub fn handle_file_explorer_request(
    request: &FileExplorerRequest,
    emit_binary: Option<&mut dyn FnMut(Vec<u8>) -> Result<(), String>>,
) -> FileExplorerOutcome {
    let requested_path = request.path.clone().unwrap_or_else(|| ".".to_string());
    let cwd = request.cwd.trim();
    let respond = |path: String, directory, file, error: Option<String>, cwd: String| {
        FileExplorerOutcome::Response(Box::new(FileExplorerResponse::new(
            FileExplorerResponsePayload {
                cwd,
                path,
                mode: request.mode,
                directory,
                file,
                error,
                request_id: request.request_id.clone(),
            },
        )))
    };
    if cwd.is_empty() {
        return respond(
            requested_path,
            None,
            None,
            Some("cwd is required".into()),
            request.cwd.clone(),
        );
    }
    let outcome = (|| -> Result<FileExplorerOutcome, ExplorerError> {
        match request.mode {
            ExplorerMode::List => {
                let directory = list_directory_entries(ListDirectoryParams {
                    root: cwd,
                    relative_path: &requested_path,
                })?;
                Ok(respond(
                    directory.path.clone(),
                    Some(directory),
                    None,
                    None,
                    cwd.to_string(),
                ))
            }
            ExplorerMode::File => {
                let params = ReadFileParams {
                    root: cwd,
                    relative_path: &requested_path,
                };
                if let Some(max) = request.max_bytes {
                    if get_downloadable_file_info(params.clone())?.size > max {
                        return Err(ExplorerError::message("File is too large to display"));
                    }
                }
                match (request.accept_binary.unwrap_or(false), emit_binary) {
                    (true, Some(emit)) => {
                        emit_file_transfer_frames(&request.request_id, params, emit)?;
                        Ok(FileExplorerOutcome::Streamed)
                    }
                    _ => {
                        let file = read_explorer_file(params)?;
                        Ok(respond(
                            file.path.clone(),
                            None,
                            Some(file),
                            None,
                            cwd.to_string(),
                        ))
                    }
                }
            }
        }
    })();
    outcome.unwrap_or_else(|e| {
        respond(
            requested_path,
            None,
            None,
            Some(e.to_string()),
            cwd.to_string(),
        )
    })
}

pub fn handle_file_download_token_request(
    request: &FileDownloadTokenRequest,
    store: &mut DownloadTokenStore,
) -> FileDownloadTokenResponse {
    let cwd = request.cwd.trim();
    let failure = |cwd: String, path: String, error: String| {
        FileDownloadTokenResponse::new(FileDownloadTokenResponsePayload {
            cwd,
            path,
            token: None,
            file_name: None,
            mime_type: None,
            size: None,
            error: Some(error),
            request_id: request.request_id.clone(),
        })
    };
    if cwd.is_empty() {
        return failure(
            request.cwd.clone(),
            request.path.clone(),
            "cwd is required".into(),
        );
    }
    match get_downloadable_file_info(ReadFileParams {
        root: cwd,
        relative_path: &request.path,
    }) {
        Ok(info) => {
            let entry = store.issue_token(DownloadTokenInput {
                path: info.path.clone(),
                absolute_path: info.absolute_path,
                file_name: info.file_name,
                mime_type: info.mime_type,
                size: info.size,
            });
            FileDownloadTokenResponse::new(FileDownloadTokenResponsePayload {
                cwd: cwd.to_string(),
                path: info.path,
                token: Some(entry.token),
                file_name: Some(entry.file_name),
                mime_type: Some(entry.mime_type),
                size: Some(entry.size),
                error: None,
                request_id: request.request_id.clone(),
            })
        }
        Err(e) => failure(cwd.to_string(), request.path.clone(), e.to_string()),
    }
}

/// Handle `directory_suggestions_request`. With a `cwd` it searches that
/// workspace (relative paths, slashes policy, gitignore respected, allowlisted
/// hidden directories). Without one it searches the home directory (absolute
/// paths, rooted policy, `~` alias, `~/Library` excluded on macOS).
pub fn handle_directory_suggestions_request(
    request: &DirectorySuggestionsRequest,
) -> DirectorySuggestionsResponse {
    let workspace = request
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let home = home_dir();
    let mut options = SearchOptions::new(
        match workspace {
            Some(cwd) => crate::path_utils::expand_user_path(cwd)
                .to_string_lossy()
                .into_owned(),
            None => home.to_string_lossy().into_owned(),
        },
        request.query.clone(),
        if workspace.is_some() {
            PathFormat::Relative
        } else {
            PathFormat::Absolute
        },
    );
    options.path_query_policy = Some(if workspace.is_some() {
        PathQueryPolicy::Slashes
    } else {
        PathQueryPolicy::Rooted
    });
    options.blank_query_behavior = Some(if workspace.is_some() {
        BlankQueryBehavior::Children
    } else {
        BlankQueryBehavior::None
    });
    if workspace.is_none() {
        options.root_aliases = vec!["~".to_string()];
        options.confident_result_scan_threshold = Some(5_000);
        if cfg!(target_os = "macos") {
            // readdir on TCC-protected folders under ~/Library can block forever.
            options.excluded_discovery_paths =
                vec![home.join("Library").to_string_lossy().into_owned()];
        }
    } else {
        options.traversable_hidden_directory_names = WORKSPACE_SEARCH_HIDDEN_DIRECTORIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        options.respect_git_ignore = true;
    }
    options.include_files = request.include_files;
    options.include_directories = request.include_directories;
    options.match_mode = request.match_mode;
    options.limit = request.limit;
    let entries = search_directory_entries(&options);
    let directories = entries
        .iter()
        .filter(|e| e.kind == SuggestionKind::Directory)
        .map(|e| e.path.clone())
        .collect();
    DirectorySuggestionsResponse::new(DirectorySuggestionsResponsePayload {
        directories,
        entries,
        error: None,
        request_id: request.request_id.clone(),
    })
}
