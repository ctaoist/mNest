use axum::{
    body::Body,
    http::{
        Method, StatusCode, Uri,
        header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

const INDEX_HTML: &str = "index.html";
const INDEX_CACHE_CONTROL: &str = "no-cache";
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const PUBLIC_CACHE_CONTROL: &str = "public, max-age=86400";

#[derive(Embed)]
#[folder = "web/dist/"]
struct FrontendAssets;

pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let requested_path = uri.path().trim_start_matches('/');
    let asset_path = if requested_path.is_empty() {
        INDEX_HTML
    } else {
        requested_path
    };

    if let Some(response) = embedded_response(asset_path, &method) {
        return response;
    }

    if !asset_path.contains('.') {
        return embedded_response(INDEX_HTML, &method)
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    }

    StatusCode::NOT_FOUND.into_response()
}

fn embedded_response(path: &str, method: &Method) -> Option<Response> {
    let asset = FrontendAssets::get(path)?;
    let content_length = asset.data.len();
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    let cache_control = match path {
        INDEX_HTML => INDEX_CACHE_CONTROL,
        path if path.starts_with("assets/") => ASSET_CACHE_CONTROL,
        _ => PUBLIC_CACHE_CONTROL,
    };

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.data)
    };

    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, content_type.as_ref())
            .header(CONTENT_LENGTH, content_length)
            .header(CACHE_CONTROL, cache_control)
            .body(body)
            .expect("embedded frontend response should be valid"),
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::to_bytes,
        http::{Method, StatusCode, Uri, header::CACHE_CONTROL},
    };

    use super::{ASSET_CACHE_CONTROL, FrontendAssets, INDEX_CACHE_CONTROL, serve};

    #[tokio::test]
    async fn serves_embedded_index() {
        let response = serve(Method::GET, Uri::from_static("/")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            INDEX_CACHE_CONTROL
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.windows(9).any(|bytes| bytes == b"id=\"root\""));
    }

    #[tokio::test]
    async fn falls_back_to_index_for_spa_routes() {
        let response = serve(Method::GET, Uri::from_static("/player/artists/example")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            INDEX_CACHE_CONTROL
        );
    }

    #[tokio::test]
    async fn serves_hashed_assets_with_immutable_cache() {
        let asset_path = FrontendAssets::iter()
            .find(|path| path.starts_with("assets/") && path.ends_with(".js"))
            .expect("Vite JavaScript bundle should be embedded");
        let uri: Uri = format!("/{asset_path}").parse().unwrap();
        let response = serve(Method::HEAD, uri).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            ASSET_CACHE_CONTROL
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn does_not_return_html_for_missing_assets() {
        let response = serve(Method::GET, Uri::from_static("/assets/missing-bundle.js")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
