# tokio 1.47.1
- features = ["full"] enables rt-multi-thread, macros, net, io-util, time, sync, fs, rt, signal
- #[tokio::main] async fn main
- spawn, select!, time::timeout, time::sleep
- Gotcha: need `rt-multi-thread` for axum
- Example: tokio::time::timeout(Duration::from_secs(3), future).await
