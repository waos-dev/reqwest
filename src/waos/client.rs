use std::convert::TryInto;
use std::time::Duration;
use std::{fmt, future::Future, sync::Arc};

use bytes::Bytes;
use http::header::{
    Entry, ACCEPT, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, REFERER,
    TRANSFER_ENCODING, USER_AGENT,
};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use url::Url;

use super::host::{self, Answer, Timeouts};
use super::{Body, Deadline, Request, RequestBuilder, Response};
#[cfg(feature = "cookies")]
use crate::cookie;
use crate::redirect::{self, ActionKind};
use crate::IntoUrl;

/// An HTTP Client for WaOS.
///
/// Uses WaOS's `wasi:http` client to make requests. The `Client` holds
/// configuration that applies to all requests. To configure a `Client`,
/// use `Client::builder()`.
#[derive(Clone)]
pub struct Client {
    config: Arc<Config>,
}

/// A `ClientBuilder` can be used to create a `Client` with custom configuration.
pub struct ClientBuilder {
    config: Config,
}

/// One request as it is sent: the first as the app made it, each later one
/// as a redirect made it of the one before.
struct Hop {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Bytes>,
}

impl Client {
    /// Constructs a new `Client`.
    ///
    /// # Panics
    ///
    /// This method panics if the default configuration cannot be built, which
    /// it always can. Use `Client::builder()` to handle a failure as an
    /// `Error` instead.
    pub fn new() -> Self {
        Client::builder().build().expect("Client::new()")
    }

    /// Creates a `ClientBuilder` to configure a `Client`.
    ///
    /// This is the same as `ClientBuilder::new()`.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Convenience method to make a `GET` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// Convenience method to make a `POST` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Convenience method to make a `PUT` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// Convenience method to make a `PATCH` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// Convenience method to make a `DELETE` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    /// Convenience method to make a `HEAD` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    /// Start building a `Request` with the `Method` and `Url`.
    ///
    /// Returns a `RequestBuilder`, which will allow setting headers and
    /// request body before sending.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        let req = url.into_url().map(move |url| Request::new(method, url));
        RequestBuilder::new(self.clone(), req)
    }

    /// Executes a `Request`.
    ///
    /// A `Request` can be built manually with `Request::new()` or obtained
    /// from a RequestBuilder with `RequestBuilder::build()`.
    ///
    /// You should prefer to use the `RequestBuilder` and
    /// `RequestBuilder::send()`.
    ///
    /// # Errors
    ///
    /// This method fails if there was an error while sending request,
    /// redirect loop was detected or redirect limit was exhausted.
    pub fn execute(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, crate::Error>> {
        let client = self.clone();
        async move { client.execute_request(request).await }
    }

    pub(super) async fn execute_request(&self, request: Request) -> crate::Result<Response> {
        let (method, url, mut headers, body, timeout) = request.pieces();
        self.check_scheme(&url)?;
        self.add_default_headers(&mut headers);
        let first = Hop {
            method,
            url: url.clone(),
            headers,
            body: body.map(Body::into_bytes),
        };
        let deadline = Deadline::after(timeout.or(self.config.timeout));
        match deadline.limit(self.answered(first, deadline)).await {
            Ok(answered) => answered,
            Err(timed_out) => Err(crate::error::request(timed_out).with_url(url)),
        }
    }

    fn check_scheme(&self, url: &Url) -> crate::Result<()> {
        let allowed = match url.scheme() {
            "https" => true,
            "http" => !self.config.https_only,
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(crate::error::url_bad_scheme(url.clone()))
        }
    }

    /// Adds the client's default headers to `headers`, without overwriting
    /// what the request set.
    fn add_default_headers(&self, headers: &mut HeaderMap) {
        for (key, value) in &self.config.headers {
            if let Entry::Vacant(entry) = headers.entry(key) {
                entry.insert(value.clone());
            }
        }
    }

    /// The response to `hop`, or to the last of the redirects it leads
    /// through, which the client's policy decides one by one.
    async fn answered(&self, mut hop: Hop, deadline: Deadline) -> crate::Result<Response> {
        let mut previous = Vec::new();
        loop {
            let answer = self.sent(&hop).await?;
            match self.redirected(&hop, &answer, &mut previous)? {
                Some(next) => hop = next,
                None => {
                    let Answer {
                        status,
                        headers,
                        body,
                    } = answer;
                    return Ok(Response::new(status, headers, body, deadline, hop.url));
                }
            }
        }
    }

    /// Sends `hop` once, with the cookies the store has for its URL, and
    /// keeps the cookies its response sets.
    async fn sent(&self, hop: &Hop) -> crate::Result<Answer> {
        #[cfg(feature = "cookies")]
        let with_cookies = self.with_cookies(hop);
        #[cfg(feature = "cookies")]
        let headers = with_cookies.as_ref().unwrap_or(&hop.headers);
        #[cfg(not(feature = "cookies"))]
        let headers = &hop.headers;

        let timeouts = self.config.timeouts;
        let answer = host::send(&hop.method, &hop.url, headers, hop.body.clone(), timeouts)
            .await
            .map_err(|error| error.if_no_url(|| hop.url.clone()))?;

        #[cfg(feature = "cookies")]
        if let Some(store) = &self.config.cookie_store {
            let mut set = answer.headers.get_all(http::header::SET_COOKIE).iter();
            store.set_cookies(&mut set, &hop.url);
        }
        Ok(answer)
    }

    /// `hop`'s headers with the store's cookies for its URL; none where the
    /// request names its own cookies, or the store has none to add.
    #[cfg(feature = "cookies")]
    fn with_cookies(&self, hop: &Hop) -> Option<HeaderMap> {
        let store = self.config.cookie_store.as_ref()?;
        if hop.headers.contains_key(http::header::COOKIE) {
            return None;
        }
        let mut headers = hop.headers.clone();
        crate::util::add_cookie_header(&mut headers, &**store, &hop.url);
        Some(headers)
    }

    /// The request `answer` redirects `hop` to, if it redirects and the
    /// client's policy follows; none where `answer` is the response to give.
    /// `previous` are the URLs asked before, to which `hop`'s is added.
    fn redirected(
        &self,
        hop: &Hop,
        answer: &Answer,
        previous: &mut Vec<Url>,
    ) -> crate::Result<Option<Hop>> {
        let Some(mut next) = hop.redirected_by(answer) else {
            return Ok(None);
        };
        previous.push(hop.url.clone());
        match self
            .config
            .redirect_policy
            .check(answer.status, &next.url, previous)
        {
            ActionKind::Follow => {}
            ActionKind::Stop => return Ok(None),
            ActionKind::Error(error) => {
                return Err(crate::error::redirect(error, hop.url.clone()));
            }
        }
        if next.url.scheme() != "http" && next.url.scheme() != "https" {
            return Err(crate::error::url_bad_scheme(next.url));
        }
        if self.config.https_only && next.url.scheme() != "https" {
            return Err(crate::error::redirect(
                crate::error::url_bad_scheme(next.url.clone()),
                next.url,
            ));
        }
        redirect::remove_sensitive_headers(&mut next.headers, &next.url, previous);
        if self.config.referer {
            if let Some(referer) = redirect::make_referer(&next.url, &hop.url) {
                next.headers.insert(REFERER, referer);
            }
        }
        Ok(Some(next))
    }
}

impl Hop {
    /// The request `answer` redirects this one to, where it is a redirect
    /// with a location: at that location, as a `GET` without the body where
    /// the status makes it one (a 303 of anything but a `HEAD`, a 301 or a
    /// 302 of a `POST`), the same request otherwise.
    fn redirected_by(&self, answer: &Answer) -> Option<Hop> {
        let as_get = match answer.status {
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => self.method == Method::POST,
            StatusCode::SEE_OTHER => true,
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => false,
            _ => return None,
        };
        let location = answer.headers.get(LOCATION)?.to_str().ok()?;
        let mut url = self.url.join(location).ok()?;
        url.set_fragment(None);

        let mut next = Hop {
            method: self.method.clone(),
            url,
            headers: self.headers.clone(),
            body: self.body.clone(),
        };
        if as_get {
            if next.method != Method::HEAD {
                next.method = Method::GET;
            }
            next.body = None;
            for header in [
                CONTENT_TYPE,
                CONTENT_LENGTH,
                CONTENT_ENCODING,
                TRANSFER_ENCODING,
            ] {
                next.headers.remove(header);
            }
        }
        Some(next)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("Client");
        self.config.fmt_fields(&mut builder);
        builder.finish()
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("ClientBuilder");
        self.config.fmt_fields(&mut builder);
        builder.finish()
    }
}

// ===== impl ClientBuilder =====

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`.
    ///
    /// This is the same as `Client::builder()`.
    pub fn new() -> Self {
        ClientBuilder {
            config: Config::default(),
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    pub fn build(mut self) -> Result<Client, crate::Error> {
        if let Some(err) = self.config.error.take() {
            return Err(err);
        }

        Ok(Client {
            config: Arc::new(self.config),
        })
    }

    /// Sets the `User-Agent` header to be used by this client.
    pub fn user_agent<V>(mut self, value: V) -> ClientBuilder
    where
        V: TryInto<HeaderValue>,
        V::Error: Into<http::Error>,
    {
        match value.try_into() {
            Ok(value) => {
                self.config.headers.insert(USER_AGENT, value);
            }
            Err(e) => {
                self.config.error = Some(crate::error::builder(e.into()));
            }
        }
        self
    }

    /// Sets the default headers for every request.
    pub fn default_headers(mut self, headers: HeaderMap) -> ClientBuilder {
        for (key, value) in headers.iter() {
            self.config.headers.insert(key, value.clone());
        }
        self
    }

    /// Enable a persistent cookie store for the client.
    ///
    /// Cookies received in responses will be preserved and included in
    /// additional requests.
    ///
    /// By default, no cookie store is used. Enabling the cookie store
    /// with `cookie_store(true)` will set the store to a default implementation.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cookies")))]
    pub fn cookie_store(mut self, enable: bool) -> ClientBuilder {
        if enable {
            self.cookie_provider(Arc::new(cookie::Jar::default()))
        } else {
            self.config.cookie_store = None;
            self
        }
    }

    /// Set the persistent cookie store for the client.
    ///
    /// Cookies received in responses will be passed to this store, and
    /// additional requests will query this store for cookies.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cookies")))]
    pub fn cookie_provider<C: cookie::CookieStore + 'static>(
        mut self,
        cookie_store: Arc<C>,
    ) -> ClientBuilder {
        self.config.cookie_store = Some(cookie_store as _);
        self
    }

    /// Set a `RedirectPolicy` for this client.
    ///
    /// Default will follow redirects up to a maximum of 10.
    pub fn redirect(mut self, policy: redirect::Policy) -> ClientBuilder {
        self.config.redirect_policy = policy;
        self
    }

    /// Enable or disable automatic setting of the `Referer` header.
    ///
    /// Default is `true`.
    pub fn referer(mut self, enable: bool) -> ClientBuilder {
        self.config.referer = enable;
        self
    }

    /// Restrict the Client to be used with HTTPS only requests.
    ///
    /// Defaults to false.
    pub fn https_only(mut self, enabled: bool) -> ClientBuilder {
        self.config.https_only = enabled;
        self
    }

    /// Enables a total request timeout.
    ///
    /// The timeout is applied from when the request starts connecting until the
    /// response body has finished. Also considered a total deadline.
    ///
    /// Default is no timeout.
    pub fn timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.timeout = Some(timeout);
        self
    }

    /// Enables a read timeout.
    ///
    /// The timeout applies to the wait for the response to begin and then to
    /// each wait for more of its body. WaOS counts the first of the two
    /// together with the connect timeout, which is its own ten minutes where
    /// `connect_timeout` sets none: set both to bound the wait for a response
    /// to begin.
    ///
    /// Default is no timeout.
    pub fn read_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.timeouts.read = Some(timeout);
        self
    }

    /// Set a timeout for only the connect phase of a `Client`.
    ///
    /// Default is `None`.
    pub fn connect_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.timeouts.connect = Some(timeout);
        self
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

struct Config {
    headers: HeaderMap,
    redirect_policy: redirect::Policy,
    referer: bool,
    https_only: bool,
    #[cfg(feature = "cookies")]
    cookie_store: Option<Arc<dyn cookie::CookieStore>>,
    /// The whole of a request, until its response's body has ended.
    timeout: Option<Duration>,
    timeouts: Timeouts,
    error: Option<crate::Error>,
}

impl Default for Config {
    fn default() -> Config {
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        Config {
            headers,
            redirect_policy: redirect::Policy::default(),
            referer: true,
            https_only: false,
            #[cfg(feature = "cookies")]
            cookie_store: None,
            timeout: None,
            timeouts: Timeouts::default(),
            error: None,
        }
    }
}

impl Config {
    fn fmt_fields(&self, f: &mut fmt::DebugStruct<'_, '_>) {
        f.field("default_headers", &self.headers);
        #[cfg(feature = "cookies")]
        if self.cookie_store.is_some() {
            f.field("cookie_store", &true);
        }
        if !self.redirect_policy.is_default() {
            f.field("redirect_policy", &self.redirect_policy);
        }
        if self.referer {
            f.field("referer", &true);
        }
        if self.https_only {
            f.field("https_only", &true);
        }
        if let Some(timeout) = &self.timeout {
            f.field("timeout", timeout);
        }
        if let Some(timeout) = &self.timeouts.read {
            f.field("read_timeout", timeout);
        }
        if let Some(timeout) = &self.timeouts.connect {
            f.field("connect_timeout", timeout);
        }
    }
}
