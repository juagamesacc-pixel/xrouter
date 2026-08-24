# reqwest 0.12.23
- default-features false + rustls-tls-webpki-roots + json + stream
- Client::builder().timeout().build()
- client.get(url).header("Authorization", format!("Bearer {}", key)).send().await
- streaming: bytes_stream()
- Gotcha: need to disable default which pulls openssl; use rustls
