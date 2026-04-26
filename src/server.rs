//! HTTP server runtime built on top of axum.

use crate::exec::{self, BodyValue, ExecContext, ExecOutput};
use crate::spec::*;
use crate::value::{self, ValidationError};
use axum::{
    Router,
    body::Bytes,
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, HeaderName, Method as HttpMethod, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{MethodRouter, get, post, put, delete, patch},
};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    spec: Arc<Spec>,
    auth_secret: Option<Vec<u8>>,
    auth_jwt_verifier: Option<String>,
}

pub async fn serve(spec: Spec, addr: SocketAddr) -> std::io::Result<()> {
    let auth_secret = match &spec.setup.token_secret {
        Some(src) => Some(resolve_static_source(src)?.into_bytes()),
        None => None,
    };
    let auth_jwt_verifier = match &spec.setup.jwt_verifier {
        Some(src) => Some(resolve_static_source(src)?),
        None => None,
    };
    let state = AppState {
        spec: Arc::new(spec),
        auth_secret,
        auth_jwt_verifier,
    };
    let router = build_router(state.clone());
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("mii-http listening on {}", addr);
    axum::serve(listener, router.into_make_service())
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn resolve_static_source(src: &ValueSource) -> std::io::Result<String> {
    match src {
        ValueSource::Env { name, .. } => std::env::var(name).map_err(|_| {
            std::io::Error::other(format!("env var `{}` not set", name))
        }),
        ValueSource::Literal { value, .. } => Ok(value.clone()),
        ValueSource::Header { .. } => Err(std::io::Error::other(
            "[HEADER ...] is not valid for static setup values",
        )),
    }
}

fn build_router(state: AppState) -> Router {
    let mut routes: HashMap<String, MethodRouter<AppState>> = HashMap::new();
    let prefix = compute_prefix(&state.spec.setup);

    for (idx, ep) in state.spec.endpoints.iter().enumerate() {
        let path = format!("{}{}", prefix, axum_path(&ep.path_segments));
        let method = ep.method;
        let handler = MethodRouter::<AppState>::new();
        let entry = routes.entry(path.clone()).or_insert(handler);
        let idx_clone = idx;
        let mr = match method {
            Method::Get => get(move |s: State<AppState>, p: AxPath<HashMap<String, String>>, q: Query<HashMap<String, String>>, h: HeaderMap, b: Bytes| handle(s, p, q, h, b, idx_clone)),
            Method::Post => post(move |s: State<AppState>, p: AxPath<HashMap<String, String>>, q: Query<HashMap<String, String>>, h: HeaderMap, b: Bytes| handle(s, p, q, h, b, idx_clone)),
            Method::Put => put(move |s: State<AppState>, p: AxPath<HashMap<String, String>>, q: Query<HashMap<String, String>>, h: HeaderMap, b: Bytes| handle(s, p, q, h, b, idx_clone)),
            Method::Delete => delete(move |s: State<AppState>, p: AxPath<HashMap<String, String>>, q: Query<HashMap<String, String>>, h: HeaderMap, b: Bytes| handle(s, p, q, h, b, idx_clone)),
            Method::Patch => patch(move |s: State<AppState>, p: AxPath<HashMap<String, String>>, q: Query<HashMap<String, String>>, h: HeaderMap, b: Bytes| handle(s, p, q, h, b, idx_clone)),
        };
        let merged = std::mem::take(entry).merge(mr);
        *entry = merged;
    }

    let mut router = Router::new();
    for (path, mr) in routes {
        router = router.route(&path, mr);
    }
    router.with_state(state)
}

fn compute_prefix(setup: &Setup) -> String {
    let base = setup.base.clone().unwrap_or_default();
    let version = setup
        .version
        .map(|v| format!("/v{}", v))
        .unwrap_or_default();
    format!("{}{}", base, version)
}

fn axum_path(segs: &[PathSegment]) -> String {
    let mut out = String::new();
    for seg in segs {
        out.push('/');
        match seg {
            PathSegment::Literal(s) => out.push_str(s),
            PathSegment::Param { name, .. } => {
                out.push(':');
                out.push_str(name);
            }
        }
    }
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}

async fn handle(
    State(state): State<AppState>,
    AxPath(path): AxPath<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
    endpoint_idx: usize,
) -> Response {
    let ep = match state.spec.endpoints.get(endpoint_idx) {
        Some(e) => e,
        None => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "endpoint missing"),
    };
    match handle_inner(&state, ep, path, query, headers, body).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    }
}

async fn handle_inner(
    state: &AppState,
    ep: &Endpoint,
    path: HashMap<String, String>,
    query: HashMap<String, String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let setup = &state.spec.setup;

    // Body size check
    if let Some(max) = setup.max_body_size {
        if body.len() as u64 > max {
            return Err(HandlerError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("body exceeds max size of {} bytes", max),
            ));
        }
    }

    // Authentication
    if let Some(AuthSpec::BearerHeader { header: hname, .. }) = &setup.auth {
        let token = extract_bearer(&headers, hname)?;
        verify_token(state, &token)?;
    }

    // Validate query params
    let mut q_map = BTreeMap::new();
    for f in &ep.query_params {
        match query.get(&f.name) {
            Some(v) => {
                if let Some(max) = setup.max_query_param_size {
                    if v.len() as u64 > max {
                        return Err(HandlerError::new(
                            StatusCode::URI_TOO_LONG,
                            format!("query param `{}` exceeds max size", f.name),
                        ));
                    }
                }
                check_validation(value::validate_text(v, &f.ty), &format!("query `{}`", f.name))?;
                q_map.insert(f.name.clone(), v.clone());
            }
            None => {
                if !f.optional {
                    return Err(HandlerError::new(
                        StatusCode::BAD_REQUEST,
                        format!("missing query parameter `{}`", f.name),
                    ));
                }
            }
        }
    }

    // Validate headers
    let mut h_map = BTreeMap::new();
    for f in &ep.headers {
        let v = header_get(&headers, &f.name);
        match v {
            Some(v) => {
                if let Some(max) = setup.max_header_size {
                    if v.len() as u64 > max {
                        return Err(HandlerError::new(
                            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                            format!("header `{}` exceeds max size", f.name),
                        ));
                    }
                }
                check_validation(value::validate_text(&v, &f.ty), &format!("header `{}`", f.name))?;
                h_map.insert(f.name.clone(), v);
            }
            None => {
                if !f.optional {
                    return Err(HandlerError::new(
                        StatusCode::BAD_REQUEST,
                        format!("missing header `{}`", f.name),
                    ));
                }
            }
        }
    }

    // Validate path params
    let mut p_map = BTreeMap::new();
    for seg in &ep.path_segments {
        if let PathSegment::Param { name, ty, .. } = seg {
            let v = path
                .get(name)
                .ok_or_else(|| HandlerError::new(
                    StatusCode::BAD_REQUEST,
                    format!("missing path param `{}`", name),
                ))?
                .clone();
            check_validation(value::validate_text(&v, ty), &format!("path `{}`", name))?;
            p_map.insert(name.clone(), v);
        }
    }

    // Resolve vars
    let mut v_map = BTreeMap::new();
    for v in &ep.vars {
        let resolved = resolve_runtime_source(&v.source, &headers).map_err(|e| {
            HandlerError::new(StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;
        v_map.insert(v.name.clone(), resolved);
    }

    // Body
    let body_value = match &ep.body {
        None => BodyValue::None,
        Some(BodySpec::String { .. }) => BodyValue::Text(
            String::from_utf8(body.to_vec()).map_err(|_| {
                HandlerError::new(StatusCode::BAD_REQUEST, "body is not valid UTF-8")
            })?,
        ),
        Some(BodySpec::Binary { .. }) => BodyValue::Binary(body.clone()),
        Some(BodySpec::Json { schema, .. }) => {
            let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
                HandlerError::new(StatusCode::BAD_REQUEST, format!("invalid JSON body: {}", e))
            })?;
            if let Some(schema) = schema {
                check_validation(value::validate_json(&v, schema), "json body")?;
            }
            BodyValue::Json(v)
        }
        Some(BodySpec::Form { fields, .. }) => {
            let parsed: BTreeMap<String, String> = form_urlencoded::parse(&body)
                .into_owned()
                .collect();
            for f in fields {
                match parsed.get(&f.name) {
                    Some(v) => check_validation(
                        value::validate_text(v, &f.ty),
                        &format!("form field `{}`", f.name),
                    )?,
                    None => {
                        if !f.optional {
                            return Err(HandlerError::new(
                                StatusCode::BAD_REQUEST,
                                format!("missing form field `{}`", f.name),
                            ));
                        }
                    }
                }
            }
            BodyValue::Form(parsed)
        }
    };

    let ctx = ExecContext {
        query: q_map,
        path: p_map,
        headers: h_map,
        vars: v_map,
        body: body_value,
    };

    let timeout = setup.timeout_ms.map(Duration::from_millis);

    let ExecOutput {
        status,
        stdout,
        stderr,
    } = exec::run_pipeline(&ep.exec.pipeline, &ctx, timeout)
        .await
        .map_err(|e| HandlerError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    if status != 0 {
        tracing::warn!(
            method = ep.method.as_str(),
            path = %ep.path,
            status,
            stderr = %String::from_utf8_lossy(&stderr),
            "exec returned non-zero"
        );
        return Err(HandlerError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("command exited with status {}", status),
        ));
    }

    let content_type = ep
        .response_type
        .clone()
        .unwrap_or_else(|| "text/plain; charset=utf-8".into());
    let mut resp = Response::new(stdout.into());
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type.parse().unwrap_or_else(|_| {
            header::HeaderValue::from_static("text/plain; charset=utf-8")
        }),
    );
    Ok(resp)
}

fn check_validation(r: Result<(), ValidationError>, scope: &str) -> Result<(), HandlerError> {
    r.map_err(|e| HandlerError::new(StatusCode::BAD_REQUEST, format!("{}: {}", scope, e.message)))
}

fn header_get(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn extract_bearer(headers: &HeaderMap, header_name: &str) -> Result<String, HandlerError> {
    let raw = header_get(headers, header_name).ok_or_else(|| {
        HandlerError::new(StatusCode::UNAUTHORIZED, format!("missing `{}`", header_name))
    })?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .unwrap_or(&raw)
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(HandlerError::new(
            StatusCode::UNAUTHORIZED,
            "empty bearer token",
        ));
    }
    Ok(token)
}

fn verify_token(state: &AppState, token: &str) -> Result<(), HandlerError> {
    if let Some(verifier) = &state.auth_jwt_verifier {
        use jsonwebtoken::{DecodingKey, Validation, decode};
        let key = DecodingKey::from_secret(verifier.as_bytes());
        let mut validation = Validation::default();
        validation.validate_exp = true;
        decode::<serde_json::Value>(token, &key, &validation).map_err(|e| {
            HandlerError::new(StatusCode::UNAUTHORIZED, format!("invalid token: {}", e))
        })?;
        return Ok(());
    }
    if let Some(secret) = &state.auth_secret {
        if constant_time_eq(token.as_bytes(), secret) {
            return Ok(());
        }
        return Err(HandlerError::new(StatusCode::UNAUTHORIZED, "invalid token"));
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn resolve_runtime_source(src: &ValueSource, headers: &HeaderMap) -> Result<String, String> {
    match src {
        ValueSource::Env { name, .. } => {
            std::env::var(name).map_err(|_| format!("env var `{}` not set", name))
        }
        ValueSource::Header { name, .. } => header_get(headers, name)
            .ok_or_else(|| format!("header `{}` not present", name)),
        ValueSource::Literal { value, .. } => Ok(value.clone()),
    }
}

#[derive(Debug)]
struct HandlerError {
    status: StatusCode,
    message: String,
}

impl HandlerError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            message: msg.into(),
        }
    }
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message)
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    let mut resp = Response::new(format!("{}\n", msg).into());
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

// keep these to silence unused-import warnings if features change
#[allow(dead_code)]
fn _force_uses() {
    let _: HttpMethod = HttpMethod::GET;
    let _ = HeaderName::from_static("x");
}
