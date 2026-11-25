//! The client on WaOS, where an app is a WASI component: requests go through
//! WaOS's `wasi:http` client (`host`), which makes the connection, speaks TLS
//! against the system's trust store and goes through the system's proxy.
//!
//! WaOS sends one request and answers one response, so what a client does
//! around that is done here (`client`): default headers, the cookie store,
//! redirects by the client's `redirect::Policy`, and the request's whole
//! timeout. What only the host could do and WASI has no word for is absent
//! from this client, never taken and ignored: a proxy of the app's own, TLS
//! settings, the HTTP version, the connection pool, a local address.
//!
//! Everything is awaited on the app's one thread: there is no runtime and no
//! thread, and a future here is not `Send`.

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;
use std::time::{Duration, Instant};

mod body;
mod client;
mod host;
mod request;
mod response;

pub use self::body::Body;
pub use self::client::{Client, ClientBuilder};
pub(crate) use self::host::HostError;
pub use self::request::{Request, RequestBuilder};
pub use self::response::Response;

use crate::error::TimedOut;

/// When a request's timeout ends; never, for a request with none.
#[derive(Clone, Copy)]
struct Deadline(Option<Instant>);

impl Deadline {
    /// The end of `timeout` counted from now.
    fn after(timeout: Option<Duration>) -> Deadline {
        Deadline(timeout.map(|timeout| Instant::now() + timeout))
    }

    /// What `work` answers, unless the deadline comes first: `work` is
    /// dropped then, which gives up what it waited for.
    async fn limit<T>(self, work: impl Future<Output = T>) -> Result<T, TimedOut> {
        let Some(end) = self.0 else {
            return Ok(work.await);
        };
        let left = end.saturating_duration_since(Instant::now());
        let nanos = u64::try_from(left.as_nanos()).unwrap_or(u64::MAX);
        let mut work = pin!(work);
        let mut late = pin!(wasdk::monotonic_clock::wait_for(nanos));
        poll_fn(|context| {
            if let Poll::Ready(done) = work.as_mut().poll(context) {
                return Poll::Ready(Ok(done));
            }
            late.as_mut().poll(context).map(|()| Err(TimedOut))
        })
        .await
    }
}
