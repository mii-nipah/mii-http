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
            diags.push(
                Diag::warning(
                    "AUTH Bearer configured without JWT_VERIFIER or TOKEN_SECRET",
                    span.clone(),
                    "tokens cannot be validated; any value will be accepted",
                )
                .with_note("add `JWT_VERIFIER [ENV ...]` or `TOKEN_SECRET [ENV ...]`"),
            );
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
    for stage in ep.exec.all_stages() {
        match stage {
            ExecStage::Source { reference, span } => {
                check_ref(reference, span, &scope, diags);
            }
            ExecStage::Command { tokens, .. } => {
                if let Some(first) = tokens.first() {
                    check_executable_token(first, diags);
                }
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
    // `string` and `json` (untyped) are allowed to be *declared* on any
    // field; they are reserved for stdin use, which is enforced at argv
    // construction time by `check_argv_safety`. `binary` is allowed only on
    // top-level BODY or as a FORM field (where it is materialized to a
    // temp file path when used as argv).
    if matches!(ty, TypeExpr::Binary) && kind != "form field" {
        diags.push(Diag::error(
            format!("`binary` type only allowed as BODY or FORM field for {} `{}`", kind, name),
            span.clone(),
            "binary is allowed only on top-level BODY or inside `BODY form { ... }`",
        ));
    }
}

fn security_check_type(ty: &TypeExpr, span: &Span, diags: &mut Vec<Diag>) {
    if let TypeExpr::Regex { pattern, .. } = ty {
        let suspicious = matches!(
            pattern.as_str(),
            ".*" | ".+" | "(.*)" | "(.+)" | "[\\s\\S]*"
        );
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
            (
                Some(BodySpec::Json {
                    schema: Some(schema),
                    ..
                }),
                false,
            ) => {
                let head = &p[0];
                schema.fields.iter().any(|f| &f.name == head)
            }
            (Some(BodySpec::Form { fields, .. }), false) if p.len() == 1 => {
                fields.iter().any(|f| f.name == p[0])
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
    match r {
        ValueRef::Query(name) => {
            if let Some(f) = ep.query_params.iter().find(|f| &f.name == name) {
                argv_unsafe_named(&f.ty, "query parameter", &f.name, span, diags);
            }
        }
        ValueRef::Header(name) => {
            if let Some(f) = ep.headers.iter().find(|f| &f.name == name) {
                argv_unsafe_named(&f.ty, "header", &f.name, span, diags);
            }
        }
        ValueRef::Path(name) => {
            for seg in &ep.path_segments {
                if let PathSegment::Param { name: n, ty, .. } = seg
                    && n == name
                {
                    argv_unsafe_named(ty, "path parameter", n, span, diags);
                }
            }
        }
        ValueRef::Var(name) => {
            if let Some(v) = ep.vars.iter().find(|v| &v.name == name)
                && matches!(v.source, ValueSource::Header { .. })
            {
                diags.push(
                    Diag::error(
                        format!("VAR `{}` from a request header cannot be passed as argv", name),
                        span.clone(),
                        "declare a typed HEADER and reference it directly, or pipe the VAR via stdin",
                    )
                    .with_note("header-backed VAR values are request input and have no type declaration"),
                );
            }
        }
        ValueRef::Body { path: p } => match &ep.body {
            Some(BodySpec::String { .. }) => diags.push(Diag::error(
                "string body cannot be passed as argv",
                span.clone(),
                "use stdin (e.g. `$ | command`)",
            )),
            Some(BodySpec::Binary { .. }) if p.is_empty() => {}
            Some(BodySpec::Binary { .. }) => diags.push(Diag::error(
                "binary body fields cannot be passed as argv",
                span.clone(),
                "binary bodies do not have named fields",
            )),
            Some(BodySpec::Json { schema: None, .. }) => diags.push(Diag::error(
                "untyped JSON body cannot be passed as argv",
                span.clone(),
                "declare a JSON schema with safe types, or use stdin",
            )),
            Some(BodySpec::Json {
                schema: Some(schema),
                ..
            }) if !p.is_empty() => {
                if let Some(field) = schema.fields.iter().find(|f| f.name == p[0]) {
                    let inner = match &field.ty {
                        JsonFieldType::Scalar(t) | JsonFieldType::Array(t) => t,
                    };
                    argv_unsafe_named(inner, "JSON field", &field.name, span, diags);
                }
            }
            Some(BodySpec::Form { fields, .. }) if !p.is_empty() => {
                if let Some(field) = fields.iter().find(|f| f.name == p[0]) {
                    argv_unsafe_named(&field.ty, "form field", &field.name, span, diags);
                }
            }
            _ => {}
        },
    }
}

/// Emit an argv-context error for a named field whose declared type is
/// unconstrained (`string` or `json`). Other types are safe.
fn argv_unsafe_named(ty: &TypeExpr, kind: &str, name: &str, span: &Span, diags: &mut Vec<Diag>) {
    if matches!(ty, TypeExpr::String | TypeExpr::Json) {
        diags.push(
            Diag::error(
                format!(
                    "{} `{}` of type `{}` cannot be passed as argv",
                    kind,
                    name,
                    ty.name()
                ),
                span.clone(),
                "use a constrained type (regex, union, int, ...) or pipe via stdin",
            )
            .with_note("`string`/`json` are reserved for stdin to avoid command injection"),
        );
    }
}

fn check_executable_token(t: &ExecToken, diags: &mut Vec<Diag>) {
    if token_contains_interpolation(t) {
        diags.push(Diag::error(
            "command executable cannot be interpolated",
            token_span(t),
            "make the program name a literal in the spec",
        ));
    }
}

fn token_contains_interpolation(t: &ExecToken) -> bool {
    match t {
        ExecToken::Text { parts, .. } => parts.iter().any(|p| matches!(p, TextPart::Interp(_))),
        ExecToken::Group { pieces, .. } => pieces
            .iter()
            .flat_map(|piece| piece.parts.iter())
            .any(|p| matches!(p, TextPart::Interp(_))),
    }
}

fn token_span(t: &ExecToken) -> Span {
    match t {
        ExecToken::Text { span, .. } | ExecToken::Group { span, .. } => span.clone(),
    }
}

fn check_token(t: &ExecToken, scope: &RefScope<'_>, diags: &mut Vec<Diag>) {
    let parts_iter: Box<dyn Iterator<Item = (&Span, &Vec<TextPart>)>> = match t {
        ExecToken::Text { parts, span, .. } => Box::new(std::iter::once((span, parts))),
        ExecToken::Group { pieces, span } => Box::new(pieces.iter().map(move |p| (span, &p.parts))),
    };
    for (span, parts) in parts_iter {
        for p in parts {
            match p {
                TextPart::Interp(r) => {
                    check_ref(r, span, scope, diags);
                    check_argv_safety(r, span, scope.ep, diags);
                }
                TextPart::Literal(s) => check_bare_reference_literals(s, span, scope, diags),
            }
        }
    }
}

fn check_bare_reference_literals(
    text: &str,
    span: &Span,
    scope: &RefScope<'_>,
    diags: &mut Vec<Diag>,
) {
    for bare in bare_reference_candidates(text, scope) {
        diags.push(
            Diag::warning(
                format!("bare Exec reference `{}` is not interpolated", bare),
                span.clone(),
                "wrap shell pieces in `[...]`, or use `{...}` inside a quoted string",
            )
            .with_note(format!(
                "write `[{}]` for a shell piece, or escape it as `\\{}` for literal text",
                bare, bare
            )),
        );
    }
}

fn bare_reference_candidates(text: &str, scope: &RefScope<'_>) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, ch) in text.char_indices() {
        if is_escaped(text, idx) {
            continue;
        }
        match ch {
            '%' | ':' | '^' | '@' => {
                let rest = &text[idx + ch.len_utf8()..];
                let Some((name, _)) = take_ident(rest) else {
                    continue;
                };
                if bare_named_ref_exists(ch, name, scope) {
                    out.push(format!("{}{}", ch, name));
                }
            }
            '$' => {
                let rest = &text[idx + 1..];
                if let Some(path) = rest.strip_prefix('.') {
                    let Some((parts, _)) = take_body_path(path) else {
                        continue;
                    };
                    if body_ref_exists(&parts, scope.ep) {
                        out.push(format!("$.{}", parts.join(".")));
                    }
                } else if rest.is_empty() && scope.ep.body.is_some() {
                    out.push("$".into());
                }
            }
            _ => {}
        }
    }
    out
}

fn bare_named_ref_exists(sigil: char, name: &str, scope: &RefScope<'_>) -> bool {
    match sigil {
        '%' => scope.query.contains(name),
        ':' => scope.path.contains(name),
        '^' => scope.headers.contains(name),
        '@' => scope.vars.contains(name),
        _ => false,
    }
}

fn body_ref_exists(path: &[String], ep: &Endpoint) -> bool {
    match &ep.body {
        Some(BodySpec::Json {
            schema: Some(schema),
            ..
        }) => path
            .first()
            .is_some_and(|head| schema.fields.iter().any(|f| &f.name == head)),
        Some(BodySpec::Form { fields, .. }) if path.len() == 1 => {
            fields.iter().any(|f| f.name == path[0])
        }
        Some(BodySpec::Json { schema: None, .. }) => true,
        _ => false,
    }
}

fn take_body_path(mut rest: &str) -> Option<(Vec<String>, &str)> {
    let mut parts = Vec::new();
    loop {
        let (part, after) = take_ident(rest)?;
        parts.push(part.to_string());
        rest = after;
        let Some(after_dot) = rest.strip_prefix('.') else {
            break;
        };
        rest = after_dot;
    }
    Some((parts, rest))
}

fn take_ident(rest: &str) -> Option<(&str, &str)> {
    let end = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .map(|(idx, c)| idx + c.len_utf8())
        .last()?;
    Some(rest.split_at(end))
}

fn is_escaped(text: &str, idx: usize) -> bool {
    let mut count = 0;
    for ch in text[..idx].chars().rev() {
        if ch == '\\' {
            count += 1;
        } else {
            break;
        }
    }
    count % 2 == 1
}
