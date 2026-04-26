//! HTTP server runtime built on top of axum.

use crate::exec::{self, BodyValue, ExecContext, ExecOutput};
use crate::spec::*;
use crate::value::{self, ValidationError};
use axum::{
    Router,
    body::Bytes,
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter},
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
    dry_run: bool,
}

pub async fn serve(spec: Spec, addr: SocketAddr, dry_run: bool) -> std::io::Result<()> {
    tracing::debug!(addr = %addr, dry_run, endpoints = spec.endpoints.len(), "server::serve");
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
        dry_run,
    };
    let router = build_router(state.clone());
    let listener = TcpListener::bind(addr).await?;
    if dry_run {
        tracing::info!(
            "mii-http listening on {} (dry-run: commands will not be executed)",
            addr
        );
    } else {
        tracing::info!("mii-http listening on {}", addr);
    }
    axum::serve(listener, router.into_make_service())
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn resolve_static_source(src: &ValueSource) -> std::io::Result<String> {
    match src {
        ValueSource::Env { name, .. } => std::env::var(name)
            .map_err(|_| std::io::Error::other(format!("env var `{}` not set", name))),
        ValueSource::Literal { value, .. } => Ok(value.clone()),
        ValueSource::Header { .. } => Err(std::io::Error::other(
            "[HEADER ...] is not valid for static setup values",
        )),
    }
}

fn build_router(state: AppState) -> Router {
    tracing::debug!("server::build_router");
    let mut routes: HashMap<String, MethodRouter<AppState>> = HashMap::new();
    let prefix = compute_prefix(&state.spec.setup);

    for (idx, ep) in state.spec.endpoints.iter().enumerate() {
        let path = format!("{}{}", prefix, axum_path(&ep.path_segments));
        tracing::debug!(method = ep.method.as_str(), path = %path, "server::build_router: mounting route");
        let entry = routes
            .entry(path)
            .or_insert_with(MethodRouter::<AppState>::new);
        let idx_clone = idx;
        let mr = MethodRouter::<AppState>::new().on(
            method_filter(ep.method),
            move |s: State<AppState>,
                  p: AxPath<HashMap<String, String>>,
                  q: Query<HashMap<String, String>>,
                  h: HeaderMap,
                  b: Bytes| handle(s, p, q, h, b, idx_clone),
        );
        let merged = std::mem::take(entry).merge(mr);
        *entry = merged;
    }

    let mut router = Router::new();
    for (path, mr) in routes {
        router = router.route(&path, mr);
    }
    router.with_state(state)
}

fn method_filter(m: Method) -> MethodFilter {
    match m {
        Method::Get => MethodFilter::GET,
        Method::Post => MethodFilter::POST,
        Method::Put => MethodFilter::PUT,
        Method::Delete => MethodFilter::DELETE,
        Method::Patch => MethodFilter::PATCH,
    }
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
    if out.is_empty() { "/".into() } else { out }
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
    tracing::info!(method = ep.method.as_str(), path = %ep.path, "server::handle: incoming request");
    match handle_inner(&state, ep, path, query, headers, body).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(method = ep.method.as_str(), path = %ep.path, status = %err.status, error = %err.message, "server::handle: returning error");
            err.into_response()
        }
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

    enforce_body_size(setup, &body)?;
    authenticate(state, &headers)?;

    let ctx = ExecContext {
        query: validate_query(setup, ep, &query)?,
        headers: validate_headers(setup, ep, &headers)?,
        path: validate_path(ep, &path)?,
        vars: resolve_vars(ep, &headers)?,
        body: build_body(ep, body)?,
    };

    let timeout = setup.timeout_ms.map(Duration::from_millis);

    if state.dry_run {
        let preview = exec::preview_pipeline(&ep.exec.pipeline, &ctx);
        tracing::info!(
            method = ep.method.as_str(),
            path = %ep.path,
            stages = ?preview,
            "dry-run: skipping execution",
        );
        let mut body_text = String::from("[dry-run] would execute:\n");
        for stage in &preview {
            body_text.push_str("  ");
            body_text.push_str(stage);
            body_text.push('\n');
        }
        let mut resp = Response::new(body_text.into());
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        return Ok(resp);
    }

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
        content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("text/plain; charset=utf-8")),
    );
    Ok(resp)
}

fn check_validation(r: Result<(), ValidationError>, scope: &str) -> Result<(), HandlerError> {
    r.map_err(|e| HandlerError::new(StatusCode::BAD_REQUEST, format!("{}: {}", scope, e.message)))
}

fn enforce_body_size(setup: &Setup, body: &Bytes) -> Result<(), HandlerError> {
    if let Some(max) = setup.max_body_size {
        if body.len() as u64 > max {
            return Err(HandlerError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("body exceeds max size of {} bytes", max),
            ));
        }
    }
    Ok(())
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(), HandlerError> {
    tracing::debug!("server::authenticate");
    if let Some(AuthSpec::BearerHeader { header: hname, .. }) = &state.spec.setup.auth {
        let token = extract_bearer(headers, hname)?;
        verify_token(state, &token)?;
    }
    Ok(())
}

fn enforce_size(
    actual: usize,
    max: Option<u64>,
    status: StatusCode,
    label: impl FnOnce() -> String,
) -> Result<(), HandlerError> {
    if let Some(max) = max {
        if actual as u64 > max {
            return Err(HandlerError::new(status, label()));
        }
    }
    Ok(())
}

fn require_or_optional<T>(
    found: Option<T>,
    optional: bool,
    missing_msg: impl FnOnce() -> String,
) -> Result<Option<T>, HandlerError> {
    match found {
        Some(v) => Ok(Some(v)),
        None if optional => Ok(None),
        None => Err(HandlerError::new(StatusCode::BAD_REQUEST, missing_msg())),
    }
}

fn validate_query(
    setup: &Setup,
    ep: &Endpoint,
    query: &HashMap<String, String>,
) -> Result<BTreeMap<String, String>, HandlerError> {
    tracing::debug!(endpoint = %ep.path, fields = ep.query_params.len(), "server::validate_query");
    let mut out = BTreeMap::new();
    for f in &ep.query_params {
        let v = require_or_optional(query.get(&f.name).cloned(), f.optional, || {
            format!("missing query parameter `{}`", f.name)
        })?;
        if let Some(v) = v {
            enforce_size(
                v.len(),
                setup.max_query_param_size,
                StatusCode::URI_TOO_LONG,
                || format!("query param `{}` exceeds max size", f.name),
            )?;
            check_validation(
                value::validate_text(&v, &f.ty),
                &format!("query `{}`", f.name),
            )?;
            out.insert(f.name.clone(), v);
        }
    }
    Ok(out)
}

fn validate_headers(
    setup: &Setup,
    ep: &Endpoint,
    headers: &HeaderMap,
) -> Result<BTreeMap<String, String>, HandlerError> {
    tracing::debug!(endpoint = %ep.path, fields = ep.headers.len(), "server::validate_headers");
    let mut out = BTreeMap::new();
    for f in &ep.headers {
        let v = require_or_optional(header_get(headers, &f.name), f.optional, || {
            format!("missing header `{}`", f.name)
        })?;
        if let Some(v) = v {
            enforce_size(
                v.len(),
                setup.max_header_size,
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                || format!("header `{}` exceeds max size", f.name),
            )?;
            check_validation(
                value::validate_text(&v, &f.ty),
                &format!("header `{}`", f.name),
            )?;
            out.insert(f.name.clone(), v);
        }
    }
    Ok(out)
}

fn validate_path(
    ep: &Endpoint,
    path: &HashMap<String, String>,
) -> Result<BTreeMap<String, String>, HandlerError> {
    tracing::debug!(endpoint = %ep.path, "server::validate_path");
    let mut out = BTreeMap::new();
    for seg in &ep.path_segments {
        if let PathSegment::Param { name, ty, .. } = seg {
            let v = path.get(name).cloned().ok_or_else(|| {
                HandlerError::new(
                    StatusCode::BAD_REQUEST,
                    format!("missing path param `{}`", name),
                )
            })?;
            check_validation(value::validate_text(&v, ty), &format!("path `{}`", name))?;
            out.insert(name.clone(), v);
        }
    }
    Ok(out)
}

fn resolve_vars(
    ep: &Endpoint,
    headers: &HeaderMap,
) -> Result<BTreeMap<String, String>, HandlerError> {
    tracing::debug!(endpoint = %ep.path, vars = ep.vars.len(), "server::resolve_vars");
    let mut out = BTreeMap::new();
    for v in &ep.vars {
        let resolved = resolve_runtime_source(&v.source, headers)
            .map_err(|e| HandlerError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        out.insert(v.name.clone(), resolved);
    }
    Ok(out)
}

fn build_body(ep: &Endpoint, body: Bytes) -> Result<BodyValue, HandlerError> {
    tracing::debug!(endpoint = %ep.path, body_len = body.len(), "server::build_body");
    Ok(match &ep.body {
        None => BodyValue::None,
        Some(BodySpec::String { .. }) => {
            BodyValue::Text(String::from_utf8(body.to_vec()).map_err(|_| {
                HandlerError::new(StatusCode::BAD_REQUEST, "body is not valid UTF-8")
            })?)
        }
        Some(BodySpec::Binary { .. }) => BodyValue::Binary(body),
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
            let parsed: BTreeMap<String, String> =
                form_urlencoded::parse(&body).into_owned().collect();
            for f in fields {
                let v = require_or_optional(parsed.get(&f.name), f.optional, || {
                    format!("missing form field `{}`", f.name)
                })?;
                if let Some(v) = v {
                    check_validation(
                        value::validate_text(v, &f.ty),
                        &format!("form field `{}`", f.name),
                    )?;
                }
            }
            BodyValue::Form(parsed)
        }
    })
}

fn header_get(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn extract_bearer(headers: &HeaderMap, header_name: &str) -> Result<String, HandlerError> {
    let raw = header_get(headers, header_name).ok_or_else(|| {
        HandlerError::new(
            StatusCode::UNAUTHORIZED,
            format!("missing `{}`", header_name),
        )
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
        ValueSource::Header { name, .. } => {
            header_get(headers, name).ok_or_else(|| format!("header `{}` not present", name))
        }
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
