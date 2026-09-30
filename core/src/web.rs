use axum::{Router, response::Html, routing::get};

pub(crate) fn build_router() -> Router {
    Router::new().route("/", get(index))
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

#[cfg(test)]
mod tests {
    use super::build_router;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn root_returns_hello_world_html() {
        let response = build_router()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8",
        );

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("<title>Tachikoma</title>"));
        assert!(html.contains("<h1>Hello world</h1>"));
    }
}
