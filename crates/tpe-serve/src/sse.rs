//! `GET /v1/jobs/{id}/events`: a job's state as Server-Sent Events.
//!
//! Each event is `event: job`, `id: <seq>` and the job's JSON (the same
//! object `GET /v1/jobs/{id}` returns). A stream never queues: when it is
//! woken it sends the job's state as it is now, so a slow client skips
//! intermediate pages (as the app's window does) and its memory stays one
//! event. The stream ends after the job's final state; reconnecting with
//! `Last-Event-ID` resumes from there, and a job whose final state the
//! client has already seen answers 204, which tells a browser's
//! `EventSource` to stop reconnecting. A deleted job's stream ends with
//! `event: removed`. When nothing happens a comment line is sent every
//! `sse_keepalive`; nothing ever times a stream out.

use std::io::Write as _;
use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use http::{HeaderMap, Response, StatusCode};
use http_body_util::{BodyExt as _, StreamBody};
use hyper::body::Frame as BodyFrame;
use std::sync::Arc;
use tokio::sync::watch;

use crate::error::ApiError;
use crate::http::{App, Body, full};
use crate::store::{Frame, Store};

struct Stream {
    store: Arc<Store>,
    id: usize,
    changes: watch::Receiver<u64>,
    /// The last event id the client has.
    last: u64,
    keepalive: Duration,
    preamble: bool,
    done: bool,
}

pub fn events(app: &App, id: usize, headers: &HeaderMap) -> Result<Response<Body>, ApiError> {
    let last = match headers.get("last-event-id") {
        None => 0,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Last-Event-ID must be an event id this server sent",
                )
            })?,
    };
    let changes = app
        .store
        .subscribe(id)
        .ok_or_else(ApiError::job_not_found)?;
    match app.store.frame(id, last) {
        Frame::Removed => return Err(ApiError::job_not_found()),
        Frame::Unchanged { terminal: true } => {
            let mut response = Response::new(full(Bytes::new()));
            *response.status_mut() = StatusCode::NO_CONTENT;
            return Ok(response);
        }
        Frame::Unchanged { terminal: false } | Frame::Newer { .. } => {}
    }
    let stream = Stream {
        store: app.store.clone(),
        id,
        changes,
        last,
        keepalive: app.limits.sse_keepalive,
        preamble: true,
        done: false,
    };
    let body = StreamBody::new(futures::stream::unfold(stream, step)).boxed_unsync();
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

type Item = Result<BodyFrame<Bytes>, std::io::Error>;

/// The next chunk of the stream, or `None` once it is over.
async fn step(mut stream: Stream) -> Option<(Item, Stream)> {
    if stream.done {
        return None;
    }
    let mut out = Vec::new();
    if stream.preamble {
        out.extend_from_slice(b"retry: 2000\n\n");
        stream.preamble = false;
    }
    loop {
        // Mark the current value seen before reading the state, so a change
        // after the read always wakes `changed` below.
        stream.changes.borrow_and_update();
        match stream.store.frame(stream.id, stream.last) {
            Frame::Removed => {
                out.extend_from_slice(b"event: removed\ndata: {}\n\n");
                stream.done = true;
                break;
            }
            Frame::Newer {
                seq,
                json,
                terminal,
            } => {
                let _ = write!(out, "id: {seq}\nevent: job\ndata: ");
                out.extend_from_slice(&json);
                out.extend_from_slice(b"\n\n");
                stream.last = seq;
                stream.done = terminal;
                break;
            }
            Frame::Unchanged { terminal } => {
                if terminal {
                    stream.done = true;
                    break;
                }
                if !out.is_empty() {
                    break;
                }
                // A closed channel (the job was deleted) returns at once and
                // the next `frame` reports it.
                if tokio::time::timeout(stream.keepalive, stream.changes.changed())
                    .await
                    .is_err()
                {
                    out.extend_from_slice(b": keep-alive\n\n");
                    break;
                }
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some((Ok(BodyFrame::data(Bytes::from(out))), stream))
}
