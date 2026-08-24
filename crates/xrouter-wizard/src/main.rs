/// Minimal HTTP server that serves the self-contained wizard HTML templates
/// produced by the `xrouter-wizard` library crate.
///
/// The `xrouter` CLI spawns this binary (as `xrouter-wizard-server`) on :3001
/// for the `xrouter wizard --web` flow. It deliberately depends only on `std`
/// so it stays out of the main server hot path.
///
/// Routes:
///   GET /            -> WIZARD_HTML
///   GET /metrics     -> METRICS_HTML
///   anything else    -> 404

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn main() {
    let port = std::env::args()
        .position(|a| a == "--port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(3001);

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).expect("failed to bind wizard server");
    eprintln!("xrouter-wizard-server listening on http://{}", addr);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                thread::spawn(move || {
                    if let Err(e) = handle(&mut stream) {
                        eprintln!("wizard request error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("wizard accept error: {}", e),
        }
    }
}

fn handle(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();

    let (body, status) = match path.as_str() {
        "/" => (xrouter_wizard::WIZARD_HTML, "200 OK"),
        "/metrics" => (xrouter_wizard::METRICS_HTML, "200 OK"),
        _ => ("<h1>404 Not Found</h1>", "404 Not Found"),
    };

    let content_type = if status.starts_with("200") {
        "text/html; charset=utf-8"
    } else {
        "text/html"
    };

    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        content_type,
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()?;
    Ok(())
}
