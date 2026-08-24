# axum 0.7.9
- Router::new().route("/v1/chat/completions", post(handler))
- Handlers take State, Json, Body etc. Return impl IntoResponse.
- App with axum::serve(listener, app)
- Use axum::extract::State, axum::Json, axum::body::Body
- Streaming: Body::from_stream, Sse
- Gotcha: axum 0.7 uses http-body 1.0, tower 0.5
