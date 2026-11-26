# reqwest for WaOS

This fork adds a client for WaOS to reqwest 0.13.5: in a WaOS app (a WASI component, `target_os = "wasi"`), `reqwest::Client` sends its requests through WaOS's `wasi:http` client, where upstream picks its hyper client, which needs sockets, a runtime and threads a component has none of. The client is `src/waos/`; the redirect policy, the cookie jar and the error type are upstream's, opened to it. It is kept for whoever ports an app to WaOS, and is not offered upstream.

## Using it

```toml
[patch.crates-io]
reqwest = { git = "https://github.com/waos-dev/reqwest", branch = "waos" }
```

The fork asks for the published `wasdk` (0.8.0). An app that takes `wasdk` from WaOS's repository names it in the same `[patch.crates-io]`, so the fork builds with that one too.

An app's features stay as they are: those that configure the hyper client (`http2`, `rustls`, `native-tls`, `system-proxy`, `socks`, `charset`, `gzip` and the like) build and do nothing on WaOS.

Two things in the app's own code are WaOS's:

- **A request is awaited on the app's one thread.** Its future is not `Send`, as in reqwest's browser client, so it goes to a task of that thread (winit's fork has `winit::platform::waos::spawn_local`), not to `tokio::spawn`. A plain function gets an answer with `wasdk::block_on`.
- **The user allows the network.** WaOS's switches for the app decide: the network is on unless the user turns it off or names domains, and this computer and the local network are a switch of their own, off until the user turns it on. A request refused by a switch fails with `HttpRequestDenied` as its source.

## What it does

- `Client` and `ClientBuilder`: `user_agent`, `default_headers`, `cookie_store` and `cookie_provider` (feature `cookies`), `redirect`, `referer`, `https_only`, `timeout`, `connect_timeout`, `read_timeout`. `Accept: */*` is a default header, as upstream's.
- `Client::get`, `post`, `put`, `patch`, `delete`, `head`, `request`, `execute`; `reqwest::get`.
- `RequestBuilder`: `header`, `headers`, `basic_auth`, `bearer_auth`, `body`, `timeout`, `query`, `form`, `json` (each with its feature), `build`, `build_split`, `send`, `try_clone`. A user name and password in the URL become the `Authorization` header.
- `Response`: `status`, `headers`, `url`, `content_length`, `cookies`, `bytes`, `text`, `json`, `chunk`, `bytes_stream` (feature `stream`), `error_for_status`.
- **Redirects** are followed here, WaOS following none: by the client's `redirect::Policy`, ten at most by default, as upstream follows them. A 303, and a 301 or 302 of a `POST`, become a `GET` without the body; a 307 or 308 sends the body again. Credentials are dropped when the redirect leaves the origin, and `Referer` is set. `Response::url` is the last URL asked.
- **Cookies** set by a response are kept by the store and sent with each request to their site, a redirect's next request among them.
- **Timeouts.** `timeout` is the whole request until the body has ended, counted here. `connect_timeout` and `read_timeout` are WaOS's own to count; it allows a response to begin within the two together, and ten minutes for either left unset, so set both to bound that wait.
- **Errors** are reqwest's: `is_connect` (the name not found, the address refused or not reached, TLS not agreed), `is_dns`, `is_timeout`, `is_redirect`, `is_status`, `is_request`, `is_body`. A failure of WaOS's carries WASI's error code as its source.

## What it leaves out

What only the host could do, and `wasi:http` has no word for, is absent from the API: it is never taken and ignored.

- **A proxy of the app's own** (`ClientBuilder::proxy`, `no_proxy`, `reqwest::Proxy`): WaOS sends through the system's proxy.
- **TLS settings** (certificates, identities, versions, `danger_accept_invalid_certs`): WaOS checks against the system's trust store.
- **The HTTP version** (`http1_only`, `http2_prior_knowledge`, `Response::version`), the connection pool, `local_address`, a resolver of the app's own, `tcp_*` settings.
- **Headers WaOS sets itself**: `Host`, and those of the connection (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `Upgrade`, `Proxy-*`). A request naming one fails when it is sent, with a builder error saying which.
- A request body is bytes held in memory: no `Body::wrap_stream`, no `multipart`.
- `Response::text` reads UTF-8; `text_with_charset` and the `charset` feature's decoding are not here.
- Response bodies arrive as the server sent them: `gzip`, `brotli`, `zstd` and `deflate` do nothing, and no `Accept-Encoding` is added.
- `blocking`, `retry`, `Response::remote_addr`, `extensions`, `upgrade`.

## Checked

In a headless WaOS on a Mac, `reqwest-check` (WaOS's maintainers' experiments, `2026-11-25-winit-on-waos/reqwest-check`) against a small server on the same computer, with the app's local-network switch on:

```
get: 200 /hello Some(5) "hello"
post: text/plain 12 from reqwest
a megabyte posted: 1048576
head: 200, length Some(5), 0 bytes of body
headers: Accept=*/*; User-Agent=reqwest-check/1; Referer=none; X-Check=yes
redirects: 200 /hello Some(5) "hello"
referer: Accept=*/*; User-Agent=reqwest-check/1; Referer=http://127.0.0.1:18473/to-headers; X-Check=yes
303: GET 0
307: POST 4
too many: is_connect false is_redirect true is_timeout false: error following redirect for url (http://127.0.0.1:18473/loop): too many redirects
no redirect: 302 to Some("../hello")
cookies: "flavour=oat" then "flavour=oat"; another client "none"
chunks: 100000 bytes in several chunk(s), length None
404: is_status true Some(404): HTTP status client error (404 Not Found) for url (http://127.0.0.1:18473/missing)
refused: is_connect true is_redirect false is_timeout false: error sending request for url (http://127.0.0.1:18474/): WaOS answered ErrorCode::ConnectionRefused
timeout: is_timeout true: error sending request for url (http://127.0.0.1:18473/never): operation timed out
timeout in the body: is_timeout true is_body true: request or response body error for url (http://127.0.0.1:18473/slow): operation timed out
connect and read timeouts: is_timeout true: error sending request for url (http://127.0.0.1:18473/never): WaOS answered ErrorCode::ConnectionTimeout
inside a wait: 200 /hello Some(5) "hello"
https: 200 OK
```

With the switch off, each request to the server fails with `WaOS answered ErrorCode::HttpRequestDenied`, and the HTTPS request still answers.

Not run: `bytes_stream`, `json`, `query` and `form`, which build; a redirect across origins; a header WaOS refuses; the system's proxy.
