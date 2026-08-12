//! A minimal stand-in for a Kubernetes API server, for tests only.
//!
//! It exists so the real client, the real request paths and the real degradation behaviour can
//! be exercised without a cluster: routes are plain `(path, status, body)` triples, so a test
//! can hand out a `403` for Nodes and a `200` for Pods and assert that inspection copes.
//!
//! Only loopback is used, and the module is compiled out of release builds entirely.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// One canned response.
pub struct Route {
    /// Request path, without the query string.
    pub path: &'static str,
    /// HTTP status to answer with.
    pub status: u16,
    /// JSON body.
    pub body: String,
}

impl Route {
    /// A successful JSON response.
    pub fn ok(path: &'static str, body: serde_json::Value) -> Self {
        Self {
            path,
            status: 200,
            body: body.to_string(),
        }
    }

    /// A `Status` failure, as the API server would report one.
    pub fn failure(path: &'static str, status: u16, message: &str) -> Self {
        let body = serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": message,
            "reason": if status == 403 { "Forbidden" } else { "InternalError" },
            "code": status,
        });
        Self {
            path,
            status,
            body: body.to_string(),
        }
    }
}

/// A running fake API server on a loopback port.
pub struct FakeApiServer {
    /// Base URL, e.g. `http://127.0.0.1:34567`.
    pub url: String,
}

impl FakeApiServer {
    /// Bind a port and serve `routes` until the test process exits.
    pub fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding a loopback port");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(routes);

        // Detached on purpose: the threads end with the test binary, and joining them would mean
        // implementing shutdown machinery that the tests do not need.
        std::thread::Builder::new()
            .name("kctx-fake-api".to_string())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // One thread per connection: inspection issues its reads concurrently, and
                    // hyper keeps each connection alive, so a sequential loop would deadlock.
                    let routes = Arc::clone(&routes);
                    std::thread::spawn(move || serve(stream, &routes));
                }
            })
            .expect("spawning the fake API server");

        Self { url }
    }
}

/// Answer every request on one connection.
fn serve(mut stream: TcpStream, routes: &[Route]) {
    loop {
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(clone) => clone,
            Err(_) => return,
        });

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        // Drain the headers so the next request on this connection starts cleanly.
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => return,
                Ok(_) if header == "\r\n" || header == "\n" => break,
                Ok(_) => {}
            }
        }

        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .split('?')
            .next()
            .unwrap_or("/")
            .to_string();

        let (status, body) = match routes.iter().find(|route| route.path == path) {
            Some(route) => (route.status, route.body.clone()),
            None => {
                let route = Route::failure("", 404, &format!("{path} not found"));
                (404, route.body)
            }
        };

        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            reason_phrase(status),
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err() || stream.flush().is_err() {
            return;
        }
    }
}

/// Just enough of the status line for hyper to be happy.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Internal Server Error",
    }
}
