use std::{
    convert::Infallible,
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    time::Duration,
};

use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Empty, StreamBody, combinators::UnsyncBoxBody};
use http_range::HttpRange;
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::project_cache::ProjectCache;

pub(crate) struct MediaServer(SocketAddr);

impl MediaServer {
    pub fn url(&self, id: Uuid) -> String {
        format!("http://{}/{id}", self.0)
    }
}

pub(crate) fn start(app: AppHandle) -> io::Result<MediaServer> {
    // Loopback only. The URL for each approved file is an unguessable,
    // process-local capability. Requests never accept filesystem paths.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    tauri::async_runtime::spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
            return;
        };
        while let Ok((stream, _)) = listener.accept().await {
            let app = app.clone();
            let host = addr.to_string();
            tauri::async_runtime::spawn(async move {
                let service = service_fn(|request| serve(request, &app, &host));
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(10))
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(MediaServer(addr))
}

type Body = UnsyncBoxBody<Bytes, io::Error>;

fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

async fn serve(
    request: Request<Incoming>,
    app: &AppHandle,
    host: &str,
) -> Result<Response<Body>, Infallible> {
    let mut response = Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            "Accept-Ranges, Content-Range",
        );
    let status = |code| {
        Response::builder()
            .status(code)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CACHE_CONTROL, "no-store")
            .body(empty())
            .unwrap()
    };
    if request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        != Some(host)
    {
        return Ok(status(StatusCode::NOT_FOUND));
    }
    let id = request
        .uri()
        .path()
        .strip_prefix('/')
        .and_then(|id| Uuid::parse_str(id).ok());
    let Some(path) = id.and_then(|id| app.state::<ProjectCache>().media_path(id)) else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    if request.method() == Method::OPTIONS {
        return Ok(response
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Range")
            .header("Access-Control-Allow-Private-Network", "true")
            .body(empty())
            .unwrap());
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Ok(status(StatusCode::METHOD_NOT_ALLOWED));
    }
    if !tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Ok(status(StatusCode::NOT_FOUND));
    }
    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let Ok(metadata) = file.metadata().await else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let length = metadata.len();
    response = response
        .header(
            header::CONTENT_TYPE,
            mime_guess::from_path(&path)
                .first_or_octet_stream()
                .as_ref(),
        )
        .header(header::ACCEPT_RANGES, "bytes");
    let mut bytes = length;
    if let Some(range) = request.headers().get(header::RANGE) {
        let ranges = range
            .to_str()
            .ok()
            .and_then(|range| HttpRange::parse(range, length).ok());
        let Some([range]) = ranges
            .as_deref()
            .filter(|ranges| ranges.len() == 1 && ranges[0].length > 0)
        else {
            return Ok(response
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                .body(empty())
                .unwrap());
        };
        if file.seek(io::SeekFrom::Start(range.start)).await.is_err() {
            return Ok(status(StatusCode::NOT_FOUND));
        }
        bytes = range.length;
        response = response.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{length}", range.start, range.start + bytes - 1),
        );
    }
    let body = if request.method() == Method::HEAD {
        empty()
    } else {
        StreamBody::new(
            ReaderStream::with_capacity(file.take(bytes), 64 * 1024).map_ok(Frame::data),
        )
        .boxed_unsync()
    };
    Ok(response
        .header(header::CONTENT_LENGTH, bytes)
        .body(body)
        .unwrap())
}
