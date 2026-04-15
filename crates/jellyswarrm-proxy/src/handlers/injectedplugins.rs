use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use regex::Regex;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use tracing::{debug, error, trace};

use crate::{
    handlers::{
        common::{execute_json_request, process_media_item},
        items::get_items,
    },
    processors::{
        request_analyzer::RequestAnalyzer,
        request_processor::{RequestProcessingContext, RequestProcessor},
    },
    models::enums::{BaseItemKind, CollectionType},
    request_preprocessing::{preprocess_request, JellyfinAuthorization},
    AppState,
};

//not sure if some endpoints need additional handling
//for all we do is repackage them to look like they come from a normal client so the auth is the same

pub async fn just_proxy(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {

    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let request_url = preprocessed.request.url().clone();
    trace!(
        "Proxy request details:\n  Original: {:?}\n  Target URL: {}\n  Transformed: {:?}",
        preprocessed.original_request,
        preprocessed.request.url(),
        preprocessed.request
    );

    let payload_processing_context = RequestProcessingContext::new(&preprocessed);
    let mut request = preprocessed.request;

    let preprocessor = &state.processors.request_processor;
    if let Some(mut json_value) = body_to_json(&request) {
        let response =
            processors::process_json(&mut json_value, preprocessor, &payload_processing_context)
                .await
                .map_err(|e| {
                    error!("Failed to process JSON body: {}", e);
                    StatusCode::BAD_REQUEST
                })?;
        if response.was_modified {
            debug!("Modified JSON body for request to {}", request_url);
            let new_body = serde_json::to_vec(&response.data).map_err(|e| {
                error!("Failed to serialize processed JSON body: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            *request.body_mut() = Some(reqwest::Body::from(new_body.clone()));
            // Update Content-Length header
            request.headers_mut().insert(
                reqwest::header::CONTENT_LENGTH,
                reqwest::header::HeaderValue::from_str(&new_body.len().to_string()).unwrap(),
            );
        }
    }
    let response = state.reqwest_client.execute(request).await.map_err(|e| {
        error!("Failed to execute proxy request: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = response.status();
    if !status.is_success() {
        warn!(
            "Upstream server returned error status: {} for Request to: {}",
            status, request_url
        );
    }
    let headers = response.headers().clone();
    let body_bytes = response.bytes().await.map_err(|e| {
        error!("Failed to read response body: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let mut response_builder = Response::builder().status(status);

    // Copy headers, filtering out hop-by-hop headers
    for (name, value) in headers.iter() {
        if !is_hop_by_hop_header(name) {
            response_builder = response_builder.header(name, value);
        }
    }

    let response = response_builder.body(Body::from(body_bytes)).map_err(|e| {
        error!("Failed to build response: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(response)
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    // RFC 7230 Section 6.1: Hop-by-hop headers
    matches!(
        name.as_str().to_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

