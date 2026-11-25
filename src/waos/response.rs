use std::fmt;

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, StatusCode};
#[cfg(feature = "json")]
use serde::de::DeserializeOwned;
use url::Url;

use super::host::Incoming;
use super::Deadline;
#[cfg(feature = "cookies")]
use crate::cookie;

/// A Response to a submitted `Request`.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Incoming,
    /// When the request's timeout ends, which the reads of the body keep to.
    deadline: Deadline,
    // Boxed to save space (11 words to 1 word), and it's not accessed
    // frequently internally.
    url: Box<Url>,
}

impl Response {
    pub(super) fn new(
        status: StatusCode,
        headers: HeaderMap,
        body: Incoming,
        deadline: Deadline,
        url: Url,
    ) -> Response {
        Response {
            status,
            headers,
            body,
            deadline,
            url: Box::new(url),
        }
    }

    /// Get the `StatusCode` of this `Response`.
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Get the `Headers` of this `Response`.
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Get a mutable reference to the `Headers` of this `Response`.
    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// Get the content length of the response, if it is known.
    ///
    /// This is the length the server gave in `Content-Length`; a response
    /// sent in chunks gives none.
    pub fn content_length(&self) -> Option<u64> {
        self.headers
            .get(http::header::CONTENT_LENGTH)?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }

    /// Retrieve the cookies contained in the response.
    ///
    /// Note that invalid 'Set-Cookie' headers will be ignored.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cookies")))]
    pub fn cookies<'a>(&'a self) -> impl Iterator<Item = cookie::Cookie<'a>> + 'a {
        cookie::extract_response_cookies(&self.headers).filter_map(Result::ok)
    }

    /// Get the final `Url` of this `Response`.
    #[inline]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Try to deserialize the response body as JSON.
    ///
    /// # Optional
    ///
    /// This requires the optional `json` feature enabled.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub async fn json<T: DeserializeOwned>(self) -> crate::Result<T> {
        let full = self.bytes().await?;

        serde_json::from_slice(&full).map_err(crate::error::decode)
    }

    /// Get the full response text.
    ///
    /// The body is read as UTF-8, with any invalid sequence replaced by the
    /// replacement character.
    pub async fn text(self) -> crate::Result<String> {
        let full = self.bytes().await?;

        Ok(String::from_utf8_lossy(&full).into_owned())
    }

    /// Get the full response body as `Bytes`.
    pub async fn bytes(mut self) -> crate::Result<Bytes> {
        let mut full = BytesMut::new();
        while let Some(chunk) = self.chunk().await? {
            full.extend_from_slice(&chunk);
        }
        Ok(full.freeze())
    }

    /// Stream a chunk of the response body.
    ///
    /// When the response body has been exhausted, this will return `None`.
    ///
    /// # Example
    ///
    /// ```
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut res = reqwest::get("https://hyper.rs").await?;
    ///
    /// while let Some(chunk) = res.chunk().await? {
    ///     println!("Chunk: {chunk:?}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn chunk(&mut self) -> crate::Result<Option<Bytes>> {
        match self.deadline.limit(self.body.chunk()).await {
            Ok(chunk) => chunk,
            Err(timed_out) => Err(crate::error::body(timed_out)),
        }
        .map_err(|error| error.if_no_url(|| *self.url.clone()))
    }

    /// Convert the response into a `Stream` of `Bytes` from the body.
    ///
    /// # Optional
    ///
    /// This requires the optional `stream` feature to be enabled.
    #[cfg(feature = "stream")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
    pub fn bytes_stream(self) -> impl futures_core::Stream<Item = crate::Result<Bytes>> {
        futures_util::stream::unfold(self, |mut response| async move {
            let chunk = response.chunk().await.transpose()?;
            Some((chunk, response))
        })
    }

    // util methods

    /// Turn a response into an error if the server returned an error.
    pub fn error_for_status(self) -> crate::Result<Self> {
        let status = self.status();
        if status.is_client_error() || status.is_server_error() {
            Err(crate::error::status_code(*self.url, status))
        } else {
            Ok(self)
        }
    }

    /// Turn a reference to a response into an error if the server returned an error.
    pub fn error_for_status_ref(&self) -> crate::Result<&Self> {
        let status = self.status();
        if status.is_client_error() || status.is_server_error() {
            Err(crate::error::status_code(*self.url.clone(), status))
        } else {
            Ok(self)
        }
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Response")
            .field("url", &self.url().as_str())
            .field("status", &self.status())
            .field("headers", self.headers())
            .finish()
    }
}
