//! Request handling: the guard checks, routing, and each endpoint.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use http::header::{
    ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE,
};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;
use tpe_app::jobs::Action;

use crate::config::Limits;
use crate::error::{ApiError, PathIssue};
use crate::guard::Guard;
use crate::paths;
use crate::routes::{self, Matched, OPENAPI, Route};
use crate::sse;
use crate::store::{FileId, Store, log};

/// Every response body: whole (JSON) or streamed (events, output files).
pub type Body = UnsyncBoxBody<Bytes, std::io::Error>;

/// Everything a request needs.
pub struct App {
    pub guard: Guard,
    pub store: Arc<Store>,
    pub limits: Limits,
}

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

fn json(status: StatusCode, body: impl Into<Bytes>) -> Response<Body> {
    let mut response = Response::new(full(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

pub fn error_response(error: &ApiError) -> Response<Body> {
    let mut response = json(error.status, error.body());
    if error.status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

/// Headers on every response. No `Access-Control-Allow-*` header is ever
/// sent: the API is not readable from any other origin.
fn secure(headers: &mut HeaderMap) {
    let fixed: [(HeaderName, &str); 5] = [
        (CACHE_CONTROL, "no-store"),
        (HeaderName::from_static("x-content-type-options"), "nosniff"),
        (
            HeaderName::from_static("content-security-policy"),
            "default-src 'none'; frame-ancestors 'none'",
        ),
        (HeaderName::from_static("referrer-policy"), "no-referrer"),
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
    ];
    for (name, value) in fixed {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

/// The service function: every request goes through here.
pub async fn handle(
    app: Arc<App>,
    request: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let mut response =
        match tokio::time::timeout(app.limits.request_timeout, dispatch(&app, request)).await {
            Ok(response) => response,
            Err(_) => error_response(&ApiError::timeout()),
        };
    secure(response.headers_mut());
    Ok(response)
}

async fn dispatch(app: &App, request: Request<Incoming>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let checked = app
        .guard
        .check_host(&parts.uri, &parts.headers)
        .and_then(|()| app.guard.check_origin(&parts.headers));
    if let Err(error) = checked {
        log(format_args!(
            "refused a {} request: {}",
            parts.method, error.code
        ));
        return error_response(&error);
    }
    let matched = routes::resolve(parts.method.as_str(), parts.uri.path());
    if !matches!(matched, Matched::Found(Route::Health, _))
        && let Err(error) = app.guard.check_auth(&parts.headers)
    {
        log(format_args!(
            "refused a {} request: {}",
            parts.method, error.code
        ));
        return error_response(&error);
    }
    let result = match matched {
        Matched::NotFound => Err(ApiError::no_route()),
        Matched::WrongMethod(allowed) => {
            let mut response = error_response(&ApiError::method_not_allowed());
            if let Ok(value) = HeaderValue::from_str(&allowed.join(", ")) {
                response.headers_mut().insert(ALLOW, value);
            }
            return response;
        }
        Matched::Found(route, [first, second]) => match route {
            Route::Health => Ok(json(StatusCode::OK, &b"{\"ok\":true}"[..])),
            Route::Version => Ok(json(StatusCode::OK, version(app))),
            Route::OpenApi => Ok(json(StatusCode::OK, OPENAPI.as_bytes())),
            Route::ListJobs => Ok(json(StatusCode::OK, app.store.list_json())),
            Route::CreateJobs => create(app, &parts.headers, body).await,
            Route::GetJob => job_id(app, first)
                .and_then(|id| app.store.job_json(id))
                .map(|body| json(StatusCode::OK, body)),
            Route::DeleteJob => job_id(app, first)
                .and_then(|id| app.store.delete(id))
                .map(|()| {
                    let mut response = Response::new(full(Bytes::new()));
                    *response.status_mut() = StatusCode::NO_CONTENT;
                    response
                }),
            Route::CancelJob => job_id(app, first)
                .and_then(|id| app.store.cancel(id))
                .map(|body| json(StatusCode::OK, body)),
            Route::Events => job_id(app, first).and_then(|id| sse::events(app, id, &parts.headers)),
            Route::Output => match job_id(app, first) {
                Ok(id) => output(app, id, second).await,
                Err(error) => Err(error),
            },
        },
    };
    result.unwrap_or_else(|error| error_response(&error))
}

fn job_id(app: &App, text: &str) -> Result<usize, ApiError> {
    app.store.parse_id(text).ok_or_else(ApiError::job_not_found)
}

/// The version is the git tag (AGENTS.md): `PDFTEXTRACT_VERSION` when a
/// release build sets it, as `crates/tpe-app/bundle.sh` does for the app,
/// else the manifest's.
pub fn version_text() -> &'static str {
    option_env!("PDFTEXTRACT_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn version(app: &App) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "api": "v1",
        "name": "tpe-serve",
        "version": version_text(),
        "instance": app.store.instance(),
        "actions": ["text", "bibliography"],
    }))
    .unwrap_or_default()
}

/// The body of `POST /v1/jobs`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateJobs {
    action: ActionName,
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ActionName {
    Text,
    Bibliography,
}

/// Exactly one `Content-Type`, and it is `application/json` (parameters
/// such as `charset` allowed). A browser can send only `text/plain`,
/// `multipart/form-data` or `application/x-www-form-urlencoded` without a
/// CORS preflight, so this also keeps form posts out.
fn require_json(headers: &HeaderMap) -> Result<(), ApiError> {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let is_json = match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().is_ok_and(|text| {
            text.split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        }),
        _ => false,
    };
    if is_json {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "send the body as Content-Type: application/json",
        ))
    }
}

fn too_large(max: usize) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        format!("the request body is limited to {max} bytes"),
    )
}

/// Read the whole body, refusing more than `max` bytes whether or not a
/// `Content-Length` announced it.
async fn read_body(headers: &HeaderMap, body: Incoming, max: usize) -> Result<Bytes, ApiError> {
    let announced = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if announced.is_some_and(|length| length > max as u64) {
        return Err(too_large(max));
    }
    match Limited::new(body, max).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => Err(too_large(max)),
        Err(_) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the request body could not be read",
        )),
    }
}

/// An optional `Idempotency-Key`: 1 to 128 visible ASCII characters.
fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let invalid = || {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "Idempotency-Key must be one header of 1 to 128 visible ASCII characters",
        )
    };
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => {
            let bytes = value.as_bytes();
            if (1..=128).contains(&bytes.len()) && bytes.iter().all(|b| (0x21..=0x7e).contains(b)) {
                Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
            } else {
                Err(invalid())
            }
        }
        _ => Err(invalid()),
    }
}

async fn create(
    app: &App,
    headers: &HeaderMap,
    body: Incoming,
) -> Result<Response<Body>, ApiError> {
    require_json(headers)?;
    let key = idempotency_key(headers)?;
    let bytes = read_body(headers, body, app.limits.max_body_bytes).await?;
    let request: CreateJobs = serde_json::from_slice(&bytes).map_err(|error| {
        if matches!(
            error.classify(),
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof
        ) {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "malformed_json",
                "the request body is not valid JSON",
            )
        } else {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                format!("expected {{\"action\": \"text\" | \"bibliography\", \"paths\": [...]}}: {error}"),
            )
        }
    })?;
    let max = app.limits.max_paths_per_request;
    if request.paths.is_empty() || request.paths.len() > max {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            format!("paths must hold 1 to {max} absolute PDF paths"),
        ));
    }
    let action = match request.action {
        ActionName::Text => Action::Text,
        ActionName::Bibliography => Action::Bibliography,
    };
    // Resolving paths touches the file system: keep it off the async workers.
    let (sent, checked) = tokio::task::spawn_blocking(move || {
        let checked: Vec<_> = request
            .paths
            .iter()
            .map(|raw| paths::validate(raw))
            .collect();
        (request.paths, checked)
    })
    .await
    .map_err(|_| ApiError::internal("checking the paths failed"))?;
    let mut sources = Vec::with_capacity(checked.len());
    let mut details = Vec::new();
    for (index, result) in checked.into_iter().enumerate() {
        match result {
            Ok(source) => sources.push(source),
            Err(problem) => details.push(PathIssue {
                index,
                reason: problem.as_str(),
            }),
        }
    }
    if !details.is_empty() {
        let mut error = ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_path",
            "some paths were refused; nothing was queued",
        );
        error.details = details;
        return Err(error);
    }
    let (replayed, body) = app.store.submit(action, sent, sources, key)?;
    let status = if replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok(json(status, body))
}

/// Stream output `n` of job `id`: only a file this server's job wrote, and
/// only while its name still refers to that file (same device and inode,
/// not a symbolic link).
async fn output(app: &App, id: usize, n: &str) -> Result<Response<Body>, ApiError> {
    let not_found = || ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such output");
    if n.is_empty() || n.len() > 6 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err(not_found());
    }
    let n: usize = n.parse().map_err(|_| not_found())?;
    let (path, identity) = app.store.output(id, n)?;
    let gone = || {
        ApiError::new(
            StatusCode::GONE,
            "output_changed",
            "the file this job wrote has been moved, replaced or deleted",
        )
    };
    let identity = identity.ok_or_else(gone)?;
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .await
        .map_err(|_| gone())?;
    let meta = file.metadata().await.map_err(|_| gone())?;
    if !meta.is_file() || FileId::of(&meta) != identity {
        return Err(gone());
    }
    let length = meta.len();
    let content_type = if path.extension().is_some_and(|e| e == "json") {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    // One buffer per chunk, handed to the socket as it is: the file is
    // never held whole in memory.
    let chunks = futures::stream::unfold(Some(file.take(length)), |reader| async move {
        let mut reader = reader?;
        let mut buffer = BytesMut::with_capacity(64 * 1024);
        match reader.read_buf(&mut buffer).await {
            Ok(0) => None,
            Ok(_) => Some((Ok(Frame::data(buffer.freeze())), Some(reader))),
            Err(error) => Some((Err(error), None)),
        }
    });
    let mut response = Response::new(StreamBody::new(chunks).boxed_unsync());
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    Ok(response)
}
