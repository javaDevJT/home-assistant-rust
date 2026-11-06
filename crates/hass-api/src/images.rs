//! Authenticated uploads and public managed-image file responses.

use crate::{AppState, http::ApiError};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use hass_core::{Error, Result};
use hass_image_upload::{ImageManager, ImageUpdate};
use serde_json::{Map, Value};
use std::sync::Arc;
use tower::ServiceExt;
use tower_http::services::ServeFile;

pub(crate) const MAX_UPLOAD_BODY: usize = hass_image_upload::MAX_SIZE + 64 * 1024;

pub(crate) async fn upload(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> std::result::Result<Json<Value>, ApiError> {
    let manager = state
        .features
        .images
        .clone()
        .ok_or_else(|| ApiError::not_found("Image upload is unavailable"))?;
    let mut upload = None;
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        if field.name() != Some("file") || upload.is_some() {
            return Err(ApiError::invalid("Expected one image file field"));
        }
        let name = field
            .file_name()
            .ok_or_else(|| ApiError::invalid("file must be an uploaded file"))?
            .to_owned();
        let content_type = field
            .content_type()
            .ok_or_else(|| ApiError::invalid("Image content type is required"))?
            .to_owned();
        let bytes = field.bytes().await.map_err(multipart_error)?;
        if bytes.len() > hass_image_upload::MAX_SIZE {
            return Err(ApiError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: "Image upload is too large".into(),
            });
        }
        upload = Some((name, content_type, bytes));
    }
    let (name, content_type, bytes) =
        upload.ok_or_else(|| ApiError::invalid("An image file is required"))?;
    let image = tokio::task::spawn_blocking(move || manager.create(name, content_type, &bytes))
        .await
        .map_err(|_| ApiError::internal())??;
    Ok(Json(
        serde_json::to_value(image).map_err(|_| ApiError::internal())?,
    ))
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    ApiError {
        status: error.status(),
        message: "Invalid multipart image upload".into(),
    }
}

pub(crate) async fn serve(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
    mut request: Request,
) -> std::result::Result<Response, ApiError> {
    let manager = state
        .features
        .images
        .clone()
        .ok_or_else(|| ApiError::not_found("Image not found"))?;
    let image = tokio::task::spawn_blocking(move || manager.prepare_file(&id, &filename))
        .await
        .map_err(|_| ApiError::internal())??;
    let etag = HeaderValue::from_str(&image.etag).map_err(|_| ApiError::internal())?;
    if request.headers().contains_key(header::IF_MATCH) {
        if !etag_matches(request.headers(), header::IF_MATCH, &image.etag, false) {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::PRECONDITION_FAILED;
            response.headers_mut().insert(header::ETAG, etag);
            return Ok(response);
        }
        request.headers_mut().remove(header::IF_UNMODIFIED_SINCE);
    }
    let not_modified = if request.headers().contains_key(header::IF_NONE_MATCH) {
        let matches = etag_matches(request.headers(), header::IF_NONE_MATCH, &image.etag, true);
        request.headers_mut().remove(header::IF_MODIFIED_SINCE);
        if matches {
            // Preconditions precede range parsing. The file service still
            // evaluates If-Unmodified-Since before we produce a 304.
            request.headers_mut().remove(header::RANGE);
        }
        matches
    } else {
        false
    };
    let response = match ServeFile::new(image.path).oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let mut response = response.map(Body::new);
    if not_modified && response.status().is_success() {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.body_mut() = Body::empty();
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().remove(header::CONTENT_RANGE);
    }
    if response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        response.headers_mut().insert(header::ETAG, etag);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&image.metadata.content_type)
                .map_err(|_| ApiError::internal())?,
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(hass_image_upload::CACHE_CONTROL_HEADER_VALUE),
        );
    }
    Ok(response)
}

fn etag_matches(
    headers: &axum::http::HeaderMap,
    field: axum::http::HeaderName,
    current: &str,
    weak: bool,
) -> bool {
    headers
        .get_all(field)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|tag| {
            let tag = tag.trim();
            tag == "*"
                || if weak {
                    tag.strip_prefix("W/").unwrap_or(tag) == current
                } else {
                    tag == current
                }
        })
}

pub(crate) async fn handle_ws(manager: ImageManager, command: &Value) -> Result<Value> {
    let command = command.clone();
    tokio::task::spawn_blocking(move || {
        let object = command
            .as_object()
            .ok_or_else(|| Error::InvalidInput("Image command must be an object".into()))?;
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        let mut fields: Map<String, Value> = object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        match kind {
            "image/list" => {
                require_empty(&fields)?;
                serde_json::to_value(manager.list()?)
                    .map_err(|_| Error::Service("Cannot serialize images".into()))
            }
            "image/update" => {
                let id = take_id(&mut fields)?;
                let name = fields
                    .remove("name")
                    .map(|name| {
                        name.as_str().map(str::to_owned).ok_or_else(|| {
                            Error::InvalidInput("Image name must be a string".into())
                        })
                    })
                    .transpose()?;
                require_empty(&fields)?;
                serde_json::to_value(manager.update(&id, ImageUpdate { name })?)
                    .map_err(|_| Error::Service("Cannot serialize image".into()))
            }
            "image/delete" => {
                let id = take_id(&mut fields)?;
                require_empty(&fields)?;
                manager.delete(&id)?;
                Ok(Value::Null)
            }
            "image/create" => Err(Error::Service(
                "Upload images through /api/image/upload".into(),
            )),
            _ => Err(Error::InvalidInput("Unsupported image command".into())),
        }
    })
    .await
    .map_err(|_| Error::Service("Image task failed".into()))?
}

fn take_id(fields: &mut Map<String, Value>) -> Result<String> {
    fields
        .remove("image_id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| Error::InvalidInput("image_id must be a string".into()))
}

fn require_empty(fields: &Map<String, Value>) -> Result<()> {
    if fields.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "Unexpected image command fields".into(),
        ))
    }
}
