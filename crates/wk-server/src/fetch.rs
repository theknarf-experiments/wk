//! Fetching a URL over HTTPS, in-process.

use std::time::Duration;

/// How long one fetch may take end to end: long, because these pull wasm
/// artifacts and image layers over links we don't control, but bounded so a
/// hung server can't wedge an image build forever.
const TIMEOUT: Duration = Duration::from_secs(120);

/// GET `url` and return the body, or a message naming what failed. Redirects
/// are followed (the artifacts this fetches live behind them) and a non-2xx
/// status is an error, not a body of error-page content.
///
/// Blocking: both callers sit in synchronous build/pull paths, so this owns a
/// current-thread runtime for the request.
pub(crate) fn get(url: &str) -> Result<Vec<u8>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("wk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            // An error page is a failure, not content.
            return Err(format!("GET {url}: HTTP {status}"));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("GET {url}: reading body: {e}"))?;
        Ok(body.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve one canned HTTP/1.1 response per connection, then stop. Returns
    /// the base url. `replies` are consumed in order, so a redirect and its
    /// target can be scripted as two entries.
    fn serve(replies: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for (reply, conn) in replies.into_iter().zip(listener.incoming()) {
                let Ok(mut s) = conn else { continue };
                // Read the request head so the client isn't writing into a
                // closed socket while we reply.
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(reply.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn ok_body(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn a_body_comes_back_whole() {
        let base = serve(vec![ok_body("adapter-bytes")]);
        assert_eq!(get(&base).expect("fetch"), b"adapter-bytes");
    }

    /// Redirects are followed — the artifacts this fetches live behind them.
    #[test]
    fn a_redirect_is_followed_to_the_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        // The redirect target must be a stable url, so bind the same port for
        // both hops: reply 302 on the first connection, the body on the next.
        let base = format!("http://127.0.0.1:{port}");
        let l = TcpListener::bind(format!("127.0.0.1:{port}")).expect("rebind");
        let target = format!("{base}/real");
        std::thread::spawn(move || {
            let replies = [
                format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                ok_body("after-redirect"),
            ];
            for (reply, conn) in replies.into_iter().zip(l.incoming()) {
                let Ok(mut s) = conn else { continue };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(reply.as_bytes());
                let _ = s.flush();
            }
        });
        assert_eq!(get(&base).expect("fetch"), b"after-redirect");
    }

    /// A non-2xx is an error, not a body of error-page content — otherwise a
    /// 404 page would be cached as if it were the artifact.
    #[test]
    fn an_error_status_is_not_content() {
        let base = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot here!"
                .into(),
        ]);
        let err = get(&base).expect_err("404 fails");
        assert!(err.contains("404"), "{err}");
        assert!(
            !err.contains("not here!"),
            "the page is not the payload: {err}"
        );
    }

    /// The real thing: TLS to a host that redirects to a CDN. Needs the
    /// network, so it is opt-in — `cargo test -p wk-server fetches_the_real -- --ignored`.
    #[test]
    #[ignore = "needs the network"]
    fn fetches_the_real_wasi_adapter_over_tls() {
        let url = "https://github.com/bytecodealliance/wasmtime/releases/download/v46.0.1/wasi_snapshot_preview1.command.wasm";
        let bytes = get(url).expect("fetch the adapter");
        assert!(
            bytes.starts_with(b"\0asm"),
            "got {} bytes, not a wasm module",
            bytes.len()
        );
    }
}
