//! Semantic validation of a parsed Spec.

use crate::diag::Diag;
use crate::spec::*;
use std::collections::HashSet;

pub fn check(spec: &Spec) -> Vec<Diag> {
    tracing::debug!(endpoints = spec.endpoints.len(), "check::check");
    let mut diags = Vec::new();
    check_setup(&spec.setup, &mut diags);
    let mut seen: HashSet<(Method, String)> = HashSet::new();
    for ep in &spec.endpoints {
        check_endpoint(ep, &spec.setup, &mut diags);
        let key = (ep.method, normalize_path(&ep.path));
        if !seen.insert(key) {
            diags.push(Diag::warning(
                format!("duplicate endpoint {} {}", ep.method.as_str(), ep.path),
                ep.span.clone(),
                "this overrides another endpoint with the same method+path",
            ));
        }
    }
    diags
}

fn normalize_path(path: &str) -> String {
    // collapse parameter type annotations for collision detection
    path.split('/')
        .map(|seg| {
            if let Some(rest) = seg.strip_prefix(':') {
                format!(":{}", rest.split(':').next().unwrap_or(""))
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn check_setup(setup: &Setup, diags: &mut Vec<Diag>) {
    if let Some(AuthSpec::BearerHeader { header, span }) = &setup.auth {
        if header.is_empty() {
            diags.push(Diag::error(
                "AUTH header name is empty",
                span.clone(),
                "specify a header name",
            ));
        }
        if setup.jwt_verifier.is_none() && setup.token_secret.is_none() {
            diags.push(Diag::warning(
                "AUTH Bearer configured without JWT_VERIFIER or TOKEN_SECRET",
                span.clone(),
                "tokens cannot be validated; any value will be accepted",
            ).with_note("add `JWT_VERIFIER [ENV ...]` or `TOKEN_SECRET [ENV ...]`"));
        }
    }
}

fn check_endpoint(ep: &Endpoint, _setup: &Setup, diags: &mut Vec<Diag>) {
    // unique names within scope
    check_unique(&ep.query_params, "query parameter", diags);
    check_unique(&ep.headers, "header", diags);
    let var_names: HashSet<&str> = ep.vars.iter().map(|v| v.name.as_str()).collect();
    if var_names.len() != ep.vars.len() {
        diags.push(Diag::error(
            "duplicate VAR name",
            ep.span.clone(),
            "VAR names must be unique within an endpoint",
        ));
    }

    // path params
    let path_params: HashSet<&str> = ep
        .path_segments
        .iter()
        .filter_map(|s| match s {
            PathSegment::Param { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    // body schema validation
    if let Some(body) = &ep.body {
        match body {
            BodySpec::Form { fields, .. } => {
                check_unique(fields, "form field", diags);
                for f in fields {
                    forbid_stdin_only_in_field(&f.ty, &f.name, "form field", diags, &f.span);
                }
            }
            BodySpec::Json {
                schema: Some(schema),
                ..
            } => {
                let mut names = HashSet::new();
                for f in &schema.fields {
                    if !names.insert(f.name.clone()) {
                        diags.push(Diag::error(
                            format!("duplicate JSON field `{}`", f.name),
                            f.span.clone(),
                            "JSON field names must be unique",
                        ));
                    }
                    let inner_ty = match &f.ty {
                        JsonFieldType::Scalar(t) | JsonFieldType::Array(t) => t,
                    };
                    forbid_stdin_only_in_field(inner_ty, &f.name, "JSON field", diags, &f.span);
                    security_check_type(inner_ty, &f.span, diags);
                }
            }
            _ => {}
        }
    }

    for q in &ep.query_params {
        forbid_stdin_only_in_field(&q.ty, &q.name, "query parameter", diags, &q.span);
        security_check_type(&q.ty, &q.span, diags);
    }
    for h in &ep.headers {
        forbid_stdin_only_in_field(&h.ty, &h.name, "header", diags, &h.span);
        security_check_type(&h.ty, &h.span, diags);
    }
    for seg in &ep.path_segments {
        if let PathSegment::Param { name, ty, span } = seg {
            forbid_stdin_only_in_field(ty, name, "path parameter", diags, span);
            security_check_type(ty, span, diags);
        }
    }

    // exec references resolve
    let scope = RefScope {
        query: ep.query_params.iter().map(|f| f.name.as_str()).collect(),
        headers: ep.headers.iter().map(|f| f.name.as_str()).collect(),
        path: path_params,
        vars: var_names,
        ep,
    };
    for stage in &ep.exec.pipeline {
        match stage {
            ExecStage::Source { reference, span } => {
                check_ref(reference, span, &scope, diags);
            }
            ExecStage::Command { tokens, .. } => {
                for t in tokens {
                    check_token(t, &scope, diags);
                }
            }
        }
    }

    // GET should not have a BODY
    if ep.method == Method::Get && ep.body.is_some() {
        diags.push(Diag::warning(
            "GET endpoint declares a BODY",
            ep.span.clone(),
            "request bodies on GET requests are unusual",
        ));
    }
}

fn check_unique(fields: &[NamedField], kind: &str, diags: &mut Vec<Diag>) {
    let mut seen = HashSet::new();
    for f in fields {
        if !seen.insert(f.name.clone()) {
            diags.push(Diag::error(
                format!("duplicate {} `{}`", kind, f.name),
                f.span.clone(),
                "names must be unique",
            ));
        }
    }
}

fn forbid_stdin_only_in_field(
    ty: &TypeExpr,
    name: &str,
    kind: &str,
    diags: &mut Vec<Diag>,
    span: &Span,
) {
    match ty {
        TypeExpr::String => diags.push(Diag::error(
            format!("`string` type not allowed for {} `{}`", kind, name),
            span.clone(),
            "use a regex, union, or another constrained type",
        ).with_note("`string` is reserved for stdin to avoid command injection")),
        TypeExpr::Json => diags.push(Diag::error(
            format!("`json` type not allowed for {} `{}`", kind, name),
            span.clone(),
            "use a typed json schema instead",
        )),
        TypeExpr::Binary => diags.push(Diag::error(
            format!("`binary` type only allowed as BODY for {} `{}`", kind, name),
            span.clone(),
            "binary is allowed only on top-level BODY",
        )),
        _ => {}
    }
}

fn security_check_type(ty: &TypeExpr, span: &Span, diags: &mut Vec<Diag>) {
    if let TypeExpr::Regex { pattern, .. } = ty {
        let suspicious = matches!(pattern.as_str(), ".*" | ".+" | "(.*)" | "(.+)" | "[\\s\\S]*");
        if suspicious {
            diags.push(Diag::warning(
                format!("permissive regex `/{}/` accepts almost any input", pattern),
                span.clone(),
                "consider restricting the pattern",
            ));
        }
        // also check for unanchored .* style
        if pattern.contains(".*") || pattern.contains(".+") {
            diags.push(Diag::warning(
                "regex contains `.*`/`.+` which can match command-injection payloads",
                span.clone(),
                "constrain to expected character class (e.g. /[a-zA-Z0-9_-]+/)",
            ));
        }
    }
}

/// All names declared on an endpoint that an Exec reference can resolve to,
/// plus a back-pointer to the endpoint for body-schema lookups.
struct RefScope<'a> {
    query: HashSet<&'a str>,
    headers: HashSet<&'a str>,
    path: HashSet<&'a str>,
    vars: HashSet<&'a str>,
    ep: &'a Endpoint,
}

/// Verify that a `ValueRef` resolves to something declared on the endpoint.
/// Argv-context safety (forbidding unconstrained types as command arguments)
/// is handled separately by [`check_argv_safety`].
fn check_ref(r: &ValueRef, span: &Span, scope: &RefScope<'_>, diags: &mut Vec<Diag>) {
    let ep = scope.ep;
    let ok = match r {
        ValueRef::Query(n) => scope.query.contains(n.as_str()),
        ValueRef::Header(n) => scope.headers.contains(n.as_str()),
        ValueRef::Path(n) => scope.path.contains(n.as_str()),
        ValueRef::Var(n) => scope.vars.contains(n.as_str()),
        ValueRef::Body { path: p } => match (&ep.body, p.is_empty()) {
            (Some(BodySpec::Json { schema: Some(schema), .. }), false) => {
                let head = &p[0];
                schema.fields.iter().any(|f| &f.name == head)
            }
            (Some(BodySpec::Form { fields, .. }), false) if p.len() == 1 => {
                fields.iter().any(|f| &f.name == &p[0])
            }
            (Some(_), true) => true,
            (Some(BodySpec::Json { schema: None, .. }), false) => true,
            _ => false,
        },
    };
    if !ok {
        diags.push(Diag::error(
            format!("unresolved reference: {}", r.describe()),
            span.clone(),
            "no such field declared on this endpoint",
        ));
    }
}

/// Reject references that, used as a command argv token, would expose
/// unconstrained user input directly to the command line.
fn check_argv_safety(r: &ValueRef, span: &Span, ep: &Endpoint, diags: &mut Vec<Diag>) {
    let ValueRef::Body { path: p } = r else {
        return;
    };
    match &ep.body {
        Some(BodySpec::String { .. }) => diags.push(Diag::error(
            "string body cannot be passed as argv",
            span.clone(),
            "use stdin (e.g. `$ | command`)",
        )),
        Some(BodySpec::Binary { .. }) => diags.push(Diag::error(
            "binary body cannot be passed as argv",
            span.clone(),
            "use stdin (e.g. `$ | command`)",
        )),
        Some(BodySpec::Json { schema: None, .. }) => diags.push(Diag::error(
            "untyped JSON body cannot be passed as argv",
            span.clone(),
            "declare a JSON schema with safe types, or use stdin",
        )),
        Some(BodySpec::Json { schema: Some(schema), .. }) if !p.is_empty() => {
            if let Some(field) = schema.fields.iter().find(|f| f.name == p[0]) {
                let inner = match &field.ty {
                    JsonFieldType::Scalar(t) | JsonFieldType::Array(t) => t,
                };
                if matches!(inner, TypeExpr::String | TypeExpr::Json) {
                    diags.push(Diag::error(
                        format!(
                            "body field `{}` of type `{}` cannot be passed as argv",
                            p.join("."),
                            inner.name()
                        ),
                        span.clone(),
                        "use a constrained type or stdin",
                    ));
                }
            }
        }
        _ => {}
    }
}

fn check_token(t: &ExecToken, scope: &RefScope<'_>, diags: &mut Vec<Diag>) {
    let parts_iter: Box<dyn Iterator<Item = (&Span, &Vec<TextPart>)>> = match t {
        ExecToken::Text { parts, span } => Box::new(std::iter::once((span, parts))),
        ExecToken::Group { pieces, span } => {
            Box::new(pieces.iter().map(move |p| (span, &p.parts)))
        }
    };
    for (span, parts) in parts_iter {
        for p in parts {
            if let TextPart::Interp(r) = p {
                check_ref(r, span, scope, diags);
                check_argv_safety(r, span, scope.ep, diags);
            }
        }
    }
}
