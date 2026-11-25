//! One request sent through WaOS's `wasi:http` client, and its response read:
//! the `http` crate's types made into WASI's and back, and WASI's error code
//! kept as the source of the `Error` an app sees.
//!
//! The invariant: nothing started here outlives the request. A request body
//! is written by the same future that awaits the send, never by a task of
//! its own, so a wait on the app's thread (`wasdk::block_on`) that sends a
//! request is held by nothing once the request is answered. The transmission
//! future `Request::new` answers is dropped unread and the trailers are left
//! at none: both report on a send that has not begun, and awaiting either
//! before `client::send` waits forever.

use std::error::Error as StdError;
use std::fmt;
use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue, CONTENT_LENGTH};
use http::{HeaderMap, Method, StatusCode};
use url::{Position, Url};
use wasdk::http::client;
use wasdk::http::types::{
    self, ErrorCode, Fields, Method as WasiMethod, Request as WasiRequest, RequestOptions, Scheme,
    Trailers,
};
use wasdk::{wit_future, wit_stream, FutureReader, FutureWriter, StreamReader, StreamResult};

/// The most bytes one read of a response body asks for.
const CHUNK_SIZE: usize = 64 * 1024;

/// The timeouts WASI takes with a request, each none where the client has none.
#[derive(Clone, Copy, Default)]
pub(super) struct Timeouts {
    /// For the connection to be made.
    pub(super) connect: Option<Duration>,
    /// For the response to begin, and then between two reads of its body.
    pub(super) read: Option<Duration>,
}

/// A response as WaOS answered it, its body not read yet.
pub(super) struct Answer {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: Incoming,
}

/// A response's body, read as it arrives.
pub(super) struct Incoming {
    contents: StreamReader<u8>,
    /// How the body ended, which WASI says with its trailers; none once read.
    ending: Option<FutureReader<Result<Option<Trailers>, ErrorCode>>>,
    /// Held while the body is read, so WaOS knows the body is wanted; dropped
    /// with the body.
    _reading: FutureWriter<Result<(), ErrorCode>>,
}

/// Why WaOS failed a request: WASI's error code.
#[derive(Debug)]
pub(crate) struct HostError(ErrorCode);

/// Sends one request and answers its response, following no redirect.
pub(super) async fn send(
    method: &Method,
    url: &Url,
    headers: &HeaderMap,
    body: Option<Bytes>,
    timeouts: Timeouts,
) -> crate::Result<Answer> {
    let body = body.filter(|body| !body.is_empty());
    let fields = fields_of(headers)?;
    if !headers.contains_key(CONTENT_LENGTH) && (body.is_some() || carries_content(method)) {
        let length = body.as_ref().map_or(0, Bytes::len);
        append(&fields, CONTENT_LENGTH.as_str(), length.to_string().as_bytes())?;
    }
    let (contents, written) = match body {
        Some(body) => {
            let (writer, contents) = wit_stream::new::<u8>();
            (Some(contents), Some((writer, body)))
        }
        None => (None, None),
    };
    let (_none, trailers) = wit_future::new(|| Ok(None));
    let options = options_of(timeouts)?;
    let (request, _transmission) = WasiRequest::new(fields, contents, trailers, Some(options));
    address(&request, method, url)?;

    let write = async move {
        if let Some((mut writer, body)) = written {
            // What a write leaves unwritten, WaOS stopped reading: the send says why.
            writer.write_all(body.to_vec()).await;
        }
    };
    let response = both(client::send(request), write)
        .await
        .map_err(|code| crate::error::request(HostError(code)))?;
    answer_of(response)
}

/// Awaits `first` and `second` together, and answers what `first` did once
/// both are done.
async fn both<T>(first: impl Future<Output = T>, second: impl Future<Output = ()>) -> T {
    let mut first = pin!(first);
    let mut second = pin!(second);
    let mut answer = None;
    let mut second_done = false;
    poll_fn(|context| {
        if answer.is_none() {
            if let Poll::Ready(done) = first.as_mut().poll(context) {
                answer = Some(done);
            }
        }
        if !second_done {
            second_done = second.as_mut().poll(context).is_ready();
        }
        match answer.take_if(|_| second_done) {
            Some(answer) => Poll::Ready(answer),
            None => Poll::Pending,
        }
    })
    .await
}

/// Whether a request of `method` says how long its content is even with none.
fn carries_content(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::PATCH)
}

/// `headers` as WASI's fields, each value as it lies.
fn fields_of(headers: &HeaderMap) -> crate::Result<Fields> {
    let fields = Fields::new();
    for (name, value) in headers {
        append(&fields, name.as_str(), value.as_bytes())?;
    }
    Ok(fields)
}

/// Adds a header to `fields`. WaOS refuses the ones it sets itself, `Host`
/// and those of the connection among them.
fn append(fields: &Fields, name: &str, value: &[u8]) -> crate::Result<()> {
    fields.append(name, value).map_err(|refused| {
        crate::error::builder(format!("WaOS refuses the header `{name}`: {refused:?}"))
    })
}

/// WASI's request options for `timeouts`, each set only where the client has
/// one, so that none leaves WaOS's own.
fn options_of(timeouts: Timeouts) -> crate::Result<RequestOptions> {
    let options = RequestOptions::new();
    let nanos = |timeout: Duration| u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
    let refused =
        |refused| crate::error::builder(format!("WaOS refuses a timeout: {refused:?}"));
    if let Some(timeout) = timeouts.connect {
        options
            .set_connect_timeout(Some(nanos(timeout)))
            .map_err(refused)?;
    }
    if let Some(timeout) = timeouts.read {
        options
            .set_first_byte_timeout(Some(nanos(timeout)))
            .map_err(refused)?;
        options
            .set_between_bytes_timeout(Some(nanos(timeout)))
            .map_err(refused)?;
    }
    Ok(options)
}

/// Sets on `request` what it asks and where: the method, and `url`'s scheme,
/// host with its port, and path with its query.
fn address(request: &WasiRequest, method: &Method, url: &Url) -> crate::Result<()> {
    let scheme = match url.scheme() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => return Err(crate::error::url_bad_scheme(url.clone())),
    };
    let authority = &url[Position::BeforeHost..Position::AfterPort];
    let path = &url[Position::BeforePath..Position::AfterQuery];
    request
        .set_method(&wasi_method(method))
        .and_then(|()| request.set_scheme(Some(&scheme)))
        .and_then(|()| request.set_authority(Some(authority)))
        .and_then(|()| request.set_path_with_query(Some(path)))
        .map_err(|()| crate::error::url_invalid_uri(url.clone()))
}

fn wasi_method(method: &Method) -> WasiMethod {
    match method.as_str() {
        "GET" => WasiMethod::Get,
        "HEAD" => WasiMethod::Head,
        "POST" => WasiMethod::Post,
        "PUT" => WasiMethod::Put,
        "DELETE" => WasiMethod::Delete,
        "CONNECT" => WasiMethod::Connect,
        "OPTIONS" => WasiMethod::Options,
        "TRACE" => WasiMethod::Trace,
        "PATCH" => WasiMethod::Patch,
        other => WasiMethod::Other(other.to_owned()),
    }
}

/// WASI's response as the client reads it: its status and headers now, its
/// body as it arrives.
fn answer_of(response: types::Response) -> crate::Result<Answer> {
    let status = StatusCode::from_u16(response.get_status_code()).map_err(crate::error::decode)?;
    let headers = headers_of(&response.get_headers())?;
    let (reading, read) = wit_future::new(|| Ok(()));
    let (contents, ending) = types::Response::consume_body(response, read);
    Ok(Answer {
        status,
        headers,
        body: Incoming {
            contents,
            ending: Some(ending),
            _reading: reading,
        },
    })
}

/// `fields` as the `http` crate's headers, each name and value as it came.
fn headers_of(fields: &Fields) -> crate::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in fields.copy_all() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(crate::error::decode)?;
        let value = HeaderValue::from_bytes(&value).map_err(crate::error::decode)?;
        headers.append(name, value);
    }
    Ok(headers)
}

impl Incoming {
    /// The next bytes of the body; none once it has ended whole. A body cut
    /// short is an error, by WASI's word of how it ended.
    pub(super) async fn chunk(&mut self) -> crate::Result<Option<Bytes>> {
        if self.ending.is_none() {
            return Ok(None);
        }
        loop {
            let (read, bytes) = self.contents.read(Vec::with_capacity(CHUNK_SIZE)).await;
            match read {
                StreamResult::Complete(0) => continue,
                StreamResult::Complete(_) => return Ok(Some(bytes.into())),
                StreamResult::Dropped | StreamResult::Cancelled => break,
            }
        }
        match self.ending.take() {
            Some(ending) => match ending.await {
                Ok(_trailers) => Ok(None),
                Err(code) => Err(crate::error::body(HostError(code))),
            },
            None => Ok(None),
        }
    }
}

impl HostError {
    /// Whether the connection was never made: the name not found, the
    /// address not reached or refused, or TLS not agreed.
    pub(crate) fn is_connect(&self) -> bool {
        matches!(
            self.0,
            ErrorCode::DnsTimeout
                | ErrorCode::DnsError(_)
                | ErrorCode::DestinationNotFound
                | ErrorCode::DestinationUnavailable
                | ErrorCode::DestinationIpProhibited
                | ErrorCode::DestinationIpUnroutable
                | ErrorCode::ConnectionRefused
                | ErrorCode::ConnectionTimeout
                | ErrorCode::ConnectionLimitReached
                | ErrorCode::TlsProtocolError
                | ErrorCode::TlsCertificateError
                | ErrorCode::TlsAlertReceived(_)
        )
    }

    pub(crate) fn is_dns(&self) -> bool {
        matches!(self.0, ErrorCode::DnsTimeout | ErrorCode::DnsError(_))
    }

    pub(crate) fn is_timeout(&self) -> bool {
        matches!(
            self.0,
            ErrorCode::DnsTimeout
                | ErrorCode::ConnectionTimeout
                | ErrorCode::ConnectionReadTimeout
                | ErrorCode::ConnectionWriteTimeout
                | ErrorCode::HttpResponseTimeout
        )
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "WaOS answered {:?}", self.0)
    }
}

impl StdError for HostError {}
