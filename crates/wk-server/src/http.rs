//! Serving `wasi:http` components: a node that exports `wasi:http/incoming-handler`
//! is exposed on `127.0.0.1:port` (when wired to a HostPort node). The host owns
//! the listening socket — the guest never touches the network — and dispatches
//! each request to a fresh, isolated `Store` (the `wasmtime serve` model).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use wasmtime::{Engine, Result, Store};
use wasmtime_wasi_http::p2::bindings::http::types::{ErrorCode, Scheme};
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::p2::WasiHttpView;

use crate::plugin::HostState;

type StateFn = dyn Fn() -> HostState + Send + Sync + 'static;

/// Run an HTTP server on `127.0.0.1:port` dispatching to `pre`'s
/// `wasi:http/incoming-handler`, building a fresh `HostState` per request via
/// `make_state`. Blocks until `kill` is set (then the socket is released).
pub fn serve(
    engine: Engine,
    pre: ProxyPre<HostState>,
    make_state: impl Fn() -> HostState + Send + Sync + 'static,
    port: u16,
    kill: Arc<AtomicBool>,
) -> Result<()> {
    // wasmtime-wasi pumps bodies via `tokio::spawn` (Send), so we need a
    // multi-thread runtime. The wasi:http response body is `!Send`, so we can't
    // spawn the connection itself — connections are handled one at a time (fine
    // for a HostPort demo; concurrency within a connection still works).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let make_state: Arc<StateFn> = Arc::new(make_state);
    rt.block_on(async move {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| wasmtime::Error::msg(format!("bind {addr}: {e}")))?;
        loop {
            if kill.load(Ordering::Relaxed) {
                break;
            }
            // Accept with a timeout so the kill flag is checked periodically.
            let stream =
                match tokio::time::timeout(Duration::from_millis(200), listener.accept()).await {
                    Ok(Ok((stream, _))) => stream,
                    Ok(Err(_)) | Err(_) => continue,
                };
            let io = hyper_util::rt::TokioIo::new(stream);
            let (engine, pre, make_state) = (engine.clone(), pre.clone(), make_state.clone());
            let service = hyper::service::service_fn(move |req| {
                let (engine, pre, make_state) = (engine.clone(), pre.clone(), make_state.clone());
                async move {
                    Ok::<_, std::convert::Infallible>(handle(engine, pre, make_state, req).await)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        }
        Ok(())
    })
}

/// Handle one request, turning any failure into a 500 so hyper always gets a
/// response.
async fn handle(
    engine: Engine,
    pre: ProxyPre<HostState>,
    make_state: Arc<StateFn>,
    req: Request<hyper::body::Incoming>,
) -> Response<HyperOutgoingBody> {
    match dispatch(engine, pre, make_state, req).await {
        Ok(resp) => resp,
        Err(e) => {
            eprintln!("[http] dispatch error: {e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn dispatch(
    engine: Engine,
    pre: ProxyPre<HostState>,
    make_state: Arc<StateFn>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<HyperOutgoingBody>> {
    let mut store = Store::new(&engine, make_state());
    // The engine has epoch interruption on (the server uses it to kill runaway
    // nodes); request handlers run to completion, so push the deadline far out.
    // (It's relative to the current epoch — `u64::MAX` would overflow once the
    // server has ticked the epoch.)
    store.set_epoch_deadline(1 << 60);
    // Convert the incoming body's error type to wasi:http's ErrorCode.
    let req = req.map(|b| {
        b.map_err(|e| ErrorCode::InternalError(Some(e.to_string())))
            .boxed_unsync()
    });
    let req = store
        .data_mut()
        .http()
        .new_incoming_request(Scheme::Http, req)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let out = store.data_mut().http().new_response_outparam(tx)?;

    // Drive the guest in a task so its (possibly streaming) response body keeps
    // streaming while we return the response we receive on `rx`.
    let task = tokio::task::spawn(async move {
        let proxy = pre.instantiate_async(&mut store).await?;
        proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, req, out)
            .await
    });

    match rx.await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => {
            eprintln!("[http] guest set an error response: {e:?}");
            Ok(error(StatusCode::INTERNAL_SERVER_ERROR))
        }
        Err(_) => {
            // Guest finished without producing a response (or trapped).
            match task.await {
                Ok(Ok(())) => eprintln!("[http] guest returned without setting a response"),
                Ok(Err(e)) => eprintln!("[http] guest handler trapped: {e:#}"),
                Err(e) => eprintln!("[http] guest task panicked: {e}"),
            }
            Ok(error(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

fn error(status: StatusCode) -> Response<HyperOutgoingBody> {
    let body = Empty::<Bytes>::new().map_err(|e| match e {}).boxed_unsync();
    Response::builder().status(status).body(body).unwrap()
}
