//! Parser for the .http specs DSL.
//!
//! The grammar is line-oriented. Setup directives precede the first endpoint
//! (whose first line is `METHOD /path`). Block bodies (`BODY form { ... }`,
//! `BODY json { ... }`) span multiple lines and are closed by a `}` on its own
//! line.
//!
//! Whole-line comments start with `#`. Trailing inline comments are not
//! supported (to avoid ambiguity with regex/exec content).
//!
//! The Exec sub-language is parsed by [`crate::parse::exec`] (chumsky); this
//! module only handles the line-oriented outer grammar.

use crate::diag::Diag;
use crate::spec::*;

pub struct ParseResult {
    pub spec: Option<Spec>,
    pub diags: Vec<Diag>,
}

pub fn parse(source: &str) -> ParseResult {
    let mut p = Parser::new(source);
    let spec = p.parse_spec();
    ParseResult {
        spec,
        diags: p.diags,
    }
}

struct Parser<'a> {
    /// (line text without trailing newline, absolute byte offset of line start)
    lines: Vec<(&'a str, usize)>,
    cursor: usize,
    diags: Vec<Diag>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut offset = 0usize;
        for line in src.split_inclusive('\n') {
            let trimmed = line.strip_suffix('\n').unwrap_or(line);
            let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
            lines.push((trimmed, offset));
            offset += line.len();
        }
        Self {
            lines,
            cursor: 0,
            diags: Vec::new(),
        }
    }

    fn err(&mut self, msg: impl Into<String>, span: Span, label: impl Into<String>) {
        self.diags.push(Diag::error(msg, span, label));
    }

    fn peek(&self) -> Option<(&'a str, usize)> {
        self.lines.get(self.cursor).copied()
    }

    fn advance(&mut self) -> Option<(&'a str, usize)> {
        let item = self.peek();
        if item.is_some() {
            self.cursor += 1;
        }
        item
    }

    fn skip_blank_and_comments(&mut self) {
        while let Some((text, _)) = self.peek() {
            let t = text.trim_start();
            if t.is_empty() || t.starts_with('#') {
                self.cursor += 1;
            } else {
                break;
            }
        }
    }

    fn parse_spec(&mut self) -> Option<Spec> {
        let setup_start = self.peek().map(|(_, o)| o).unwrap_or(0);
        let setup = self.parse_setup(setup_start);
        let mut endpoints = Vec::new();
        loop {
            self.skip_blank_and_comments();
            if self.peek().is_none() {
                break;
            }
            if let Some(ep) = self.parse_endpoint() {
                endpoints.push(ep);
            } else {
                // give up if we couldn't parse, advance one to avoid infinite loop
                if self.advance().is_none() {
                    break;
                }
            }
        }
        Some(Spec { setup, endpoints })
    }

    fn parse_setup(&mut self, start: usize) -> Setup {
        let mut setup = Setup {
            span: start..start,
            ..Setup::default()
        };
        loop {
            self.skip_blank_and_comments();
            let Some((text, offset)) = self.peek() else {
                break;
            };
            let trimmed = text.trim_start();
            // detect endpoint method line
            let upper_first = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("");
            if matches!(upper_first, "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
                break;
            }
            // consume directive line
            self.cursor += 1;
            self.parse_setup_directive(&mut setup, text, offset);
            setup.span.end = offset + text.len();
        }
        setup
    }

    fn parse_setup_directive(&mut self, setup: &mut Setup, text: &str, offset: usize) {
        let leading_ws = text.len() - text.trim_start().len();
        let body = text.trim_start();
        let (key, rest) = split_first_word(body);
        let key_span = (offset + leading_ws)..(offset + leading_ws + key.len());
        let rest_offset = offset + leading_ws + key.len();
        let rest_trim_off = rest.len() - rest.trim_start().len();
        let value = rest.trim();
        let value_offset = rest_offset + rest_trim_off;
        let value_span = value_offset..(value_offset + value.len());
        match key {
            "VERSION" => match value.parse::<u32>() {
                Ok(v) => setup.version = Some(v),
                Err(_) => self.err("invalid VERSION", value_span, "expected positive integer"),
            },
            "BASE" => {
                if value.is_empty() {
                    self.err("missing BASE value", key_span, "expected a path like /api");
                } else {
                    let mut v = value.to_string();
                    if !v.starts_with('/') {
                        v.insert(0, '/');
                    }
                    setup.base = Some(v.trim_end_matches('/').to_string());
                }
            }
            "AUTH" => match parse_auth(value, value_offset) {
                Ok(a) => setup.auth = Some(a),
                Err(d) => self.diags.push(d),
            },
            "JWT_VERIFIER" => match parse_value_source(value, value_offset) {
                Ok(s) => setup.jwt_verifier = Some(s),
                Err(d) => self.diags.push(d),
            },
            "TOKEN_SECRET" => match parse_value_source(value, value_offset) {
                Ok(s) => setup.token_secret = Some(s),
                Err(d) => self.diags.push(d),
            },
            "MAX_BODY_SIZE" => match parse_size(value) {
                Some(n) => setup.max_body_size = Some(n),
                None => self.err("invalid MAX_BODY_SIZE", value_span, "expected e.g. 1mb, 512kb, 1024"),
            },
            "MAX_QUERY_PARAM_SIZE" => match value.parse::<u64>() {
                Ok(n) => setup.max_query_param_size = Some(n),
                Err(_) => self.err("invalid MAX_QUERY_PARAM_SIZE", value_span, "expected integer"),
            },
            "MAX_HEADER_SIZE" => match value.parse::<u64>() {
                Ok(n) => setup.max_header_size = Some(n),
                Err(_) => self.err("invalid MAX_HEADER_SIZE", value_span, "expected integer"),
            },
            "TIMEOUT" => match parse_duration_ms(value) {
                Some(n) => setup.timeout_ms = Some(n),
                None => self.err("invalid TIMEOUT", value_span, "expected e.g. 30s, 500ms, 1m"),
            },
            other => {
                self.err(
                    format!("unknown setup directive `{}`", other),
                    key_span,
                    "expected one of VERSION, BASE, AUTH, JWT_VERIFIER, TOKEN_SECRET, MAX_BODY_SIZE, MAX_QUERY_PARAM_SIZE, MAX_HEADER_SIZE, TIMEOUT",
                );
            }
        }
    }

    fn parse_endpoint(&mut self) -> Option<Endpoint> {
        let (text, offset) = self.advance()?;
        let trimmed = text.trim_start();
        let leading = text.len() - trimmed.len();
        let (method_str, rest) = split_first_word(trimmed);
        let method = match method_str {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            other => {
                self.err(
                    format!("expected HTTP method, found `{}`", other),
                    (offset + leading)..(offset + leading + method_str.len()),
                    "expected GET/POST/PUT/DELETE/PATCH",
                );
                return None;
            }
        };
        let path_off = offset + leading + method_str.len() + (rest.len() - rest.trim_start().len());
        let path_str = rest.trim().to_string();
        let path_span = path_off..(path_off + path_str.len());
        let path_segments = self.parse_path(&path_str, path_off);
        let header_span = (offset + leading)..(offset + text.len());
        let mut endpoint = Endpoint {
            method,
            path: path_str,
            path_segments,
            response_type: None,
            query_params: Vec::new(),
            headers: Vec::new(),
            vars: Vec::new(),
            body: None,
            exec: ExecSpec {
                raw: String::new(),
                span: 0..0,
                pipeline: Vec::new(),
            },
            span: header_span,
        };
        let _ = path_span;

        loop {
            self.skip_blank_and_comments();
            let Some((line_text, line_off)) = self.peek() else {
                break;
            };
            let t = line_text.trim_start();
            let first_word = t.split_whitespace().next().unwrap_or("");
            if matches!(first_word, "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
                break;
            }
            self.cursor += 1;
            self.parse_endpoint_directive(&mut endpoint, line_text, line_off);
            endpoint.span.end = line_off + line_text.len();
        }
        if endpoint.exec.raw.is_empty() {
            self.err(
                "endpoint missing Exec directive",
                endpoint.span.clone(),
                "every endpoint requires an `Exec:` line",
            );
        }
        Some(endpoint)
    }

    fn parse_endpoint_directive(&mut self, ep: &mut Endpoint, text: &str, offset: usize) {
        let leading = text.len() - text.trim_start().len();
        let body = text.trim_start();

        // Some directives are case-sensitive and use `:` separators (Response-Type, Exec).
        // Others use space separators (QUERY, HEADER, VAR, BODY).
        if let Some(rest) = body.strip_prefix("Response-Type") {
            let rest = rest.trim_start_matches([':', ' ', '\t']);
            ep.response_type = Some(rest.trim().to_string());
            return;
        }
        if let Some(rest) = body.strip_prefix("Exec:") {
            let exec_off = offset + leading + "Exec:".len();
            let trim_off = rest.len() - rest.trim_start().len();
            let raw = rest.trim().to_string();
            let span = (exec_off + trim_off)..(exec_off + trim_off + raw.len());
            let pipeline = match crate::parse::exec::parse_exec(&raw, span.start) {
                Ok(p) => p,
                Err(d) => {
                    self.diags.push(d);
                    Vec::new()
                }
            };
            ep.exec = ExecSpec {
                raw,
                span,
                pipeline,
            };
            return;
        }

        let (key, rest) = split_first_word(body);
        let key_off = offset + leading;
        let rest_off = key_off + key.len();
        let rest_trim_off = rest.len() - rest.trim_start().len();
        let value = rest.trim();
        let val_off = rest_off + rest_trim_off;

        match key {
            "QUERY" => match self.parse_named_field(value, val_off) {
                Ok(f) => ep.query_params.push(f),
                Err(d) => self.diags.push(d),
            },
            "HEADER" => match self.parse_named_field(value, val_off) {
                Ok(f) => ep.headers.push(f),
                Err(d) => self.diags.push(d),
            },
            "VAR" => match self.parse_var_def(value, val_off) {
                Ok(v) => ep.vars.push(v),
                Err(d) => self.diags.push(d),
            },
            "BODY" => self.parse_body(ep, value, val_off),
            other => self.err(
                format!("unknown directive `{}`", other),
                key_off..key_off + key.len(),
                "expected QUERY, HEADER, VAR, BODY, Response-Type or Exec",
            ),
        }
    }

    fn parse_path(&mut self, path: &str, offset: usize) -> Vec<PathSegment> {
        let mut segs = Vec::new();
        if !path.starts_with('/') {
            self.err(
                "path must start with `/`",
                offset..(offset + path.len()),
                "add a leading slash",
            );
        }
        for (idx, raw) in path.split('/').enumerate() {
            if idx == 0 {
                continue;
            }
            // compute span of this segment
            // (rough; sufficient for diagnostics)
            let local_off = offset
                + path
                    .match_indices('/')
                    .nth(idx - 1)
                    .map(|(i, _)| i + 1)
                    .unwrap_or(0);
            let seg_span = local_off..(local_off + raw.len());
            if raw.is_empty() {
                continue;
            }
            if let Some(rest) = raw.strip_prefix(':') {
                let mut parts = rest.splitn(2, ':');
                let name = parts.next().unwrap_or("").to_string();
                let ty_str = parts.next().unwrap_or("string");
                if name.is_empty() {
                    self.err(
                        "empty path parameter name",
                        seg_span.clone(),
                        "use `:name:type`",
                    );
                    continue;
                }
                let ty = match parse_type_expr(ty_str, seg_span.end - ty_str.len()) {
                    Ok(t) => t,
                    Err(d) => {
                        self.diags.push(d);
                        TypeExpr::String
                    }
                };
                segs.push(PathSegment::Param {
                    name,
                    ty,
                    span: seg_span,
                });
            } else {
                segs.push(PathSegment::Literal(raw.to_string()));
            }
        }
        segs
    }

    fn parse_named_field(&mut self, value: &str, offset: usize) -> Result<NamedField, Diag> {
        // syntax: name[?]: <type>
        let colon_pos = value.find(':').ok_or_else(|| {
            Diag::error(
                "missing `:` in field declaration",
                offset..offset + value.len(),
                "expected `name: <type>`",
            )
        })?;
        let head = &value[..colon_pos];
        let tail = value[colon_pos + 1..].trim_start();
        let tail_off = offset + colon_pos + 1 + (value[colon_pos + 1..].len() - tail.len());
        let (name, optional) = if let Some(stripped) = head.strip_suffix('?') {
            (stripped.trim().to_string(), true)
        } else {
            (head.trim().to_string(), false)
        };
        if name.is_empty() {
            return Err(Diag::error(
                "empty field name",
                offset..offset + value.len(),
                "expected a name before `:`",
            ));
        }
        let ty = parse_type_expr(tail, tail_off)?;
        Ok(NamedField {
            name,
            optional,
            ty,
            span: offset..(offset + value.len()),
        })
    }

    fn parse_var_def(&mut self, value: &str, offset: usize) -> Result<VarDef, Diag> {
        // syntax: VAR name <source>
        let (name, rest) = split_first_word(value);
        if name.is_empty() {
            return Err(Diag::error(
                "missing var name",
                offset..offset + value.len(),
                "expected `VAR name <source>`",
            ));
        }
        let rest_trim_off = rest.len() - rest.trim_start().len();
        let src_str = rest.trim();
        let src_off = offset + name.len() + rest_trim_off;
        let source = parse_value_source(src_str, src_off)?;
        Ok(VarDef {
            name: name.to_string(),
            source,
            span: offset..(offset + value.len()),
        })
    }

    fn parse_body(&mut self, ep: &mut Endpoint, value: &str, offset: usize) {
        // Cases:
        //   BODY string
        //   BODY json
        //   BODY binary
        //   BODY json { ... }
        //   BODY form { ... }
        let (kind, rest) = split_first_word(value);
        let kind_span = offset..(offset + kind.len());
        let rest_trim = rest.trim();
        let opens_block = rest_trim.starts_with('{');
        match kind {
            "string" => {
                if opens_block {
                    self.err("BODY string takes no schema", kind_span.clone(), "");
                }
                ep.body = Some(BodySpec::String { span: kind_span });
            }
            "binary" => {
                if opens_block {
                    self.err("BODY binary takes no schema", kind_span.clone(), "");
                }
                ep.body = Some(BodySpec::Binary { span: kind_span });
            }
            "json" => {
                if !opens_block {
                    ep.body = Some(BodySpec::Json {
                        schema: None,
                        span: kind_span,
                    });
                } else {
                    let fields = self.parse_json_block();
                    ep.body = Some(BodySpec::Json {
                        schema: Some(JsonSchema { fields }),
                        span: kind_span,
                    });
                }
            }
            "form" => {
                if !opens_block {
                    self.err(
                        "BODY form requires `{ ... }` schema",
                        kind_span.clone(),
                        "add a `{` block listing form fields",
                    );
                    ep.body = Some(BodySpec::Form {
                        fields: Vec::new(),
                        span: kind_span,
                    });
                } else {
                    let fields = self.parse_form_block();
                    ep.body = Some(BodySpec::Form {
                        fields,
                        span: kind_span,
                    });
                }
            }
            other => self.err(
                format!("unknown body kind `{}`", other),
                kind_span,
                "expected one of: string, json, form, binary",
            ),
        }
    }

    fn parse_form_block(&mut self) -> Vec<NamedField> {
        let mut out = Vec::new();
        loop {
            self.skip_blank_and_comments();
            let Some((text, off)) = self.peek() else {
                self.err("unterminated BODY form block", 0..0, "missing `}`");
                break;
            };
            let t = text.trim();
            if t == "}" {
                self.cursor += 1;
                break;
            }
            self.cursor += 1;
            let leading = text.len() - text.trim_start().len();
            let val = t.trim_end_matches(',').trim();
            match self.parse_named_field(val, off + leading) {
                Ok(f) => out.push(f),
                Err(d) => self.diags.push(d),
            }
        }
        out
    }

    fn parse_json_block(&mut self) -> Vec<JsonField> {
        let mut out = Vec::new();
        loop {
            self.skip_blank_and_comments();
            let Some((text, off)) = self.peek() else {
                self.err("unterminated BODY json block", 0..0, "missing `}`");
                break;
            };
            let t = text.trim();
            if t == "}" {
                self.cursor += 1;
                break;
            }
            self.cursor += 1;
            let leading = text.len() - text.trim_start().len();
            let val = t.trim_end_matches(',').trim();
            match self.parse_json_field(val, off + leading) {
                Ok(f) => out.push(f),
                Err(d) => self.diags.push(d),
            }
        }
        out
    }

    fn parse_json_field(&mut self, value: &str, offset: usize) -> Result<JsonField, Diag> {
        let colon_pos = value.find(':').ok_or_else(|| {
            Diag::error(
                "missing `:` in field declaration",
                offset..offset + value.len(),
                "expected `name: <type>`",
            )
        })?;
        let head = &value[..colon_pos];
        let tail = value[colon_pos + 1..].trim_start();
        let tail_off = offset + colon_pos + 1 + (value[colon_pos + 1..].len() - tail.len());
        let (name, optional) = if let Some(stripped) = head.strip_suffix('?') {
            (stripped.trim().to_string(), true)
        } else {
            (head.trim().to_string(), false)
        };
        if name.is_empty() {
            return Err(Diag::error(
                "empty field name",
                offset..offset + value.len(),
                "expected a name before `:`",
            ));
        }
        let ty = if let Some(inner) = tail.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            JsonFieldType::Array(parse_type_expr(inner.trim(), tail_off + 1)?)
        } else {
            JsonFieldType::Scalar(parse_type_expr(tail, tail_off)?)
        };
        Ok(JsonField {
            name,
            optional,
            ty,
            span: offset..(offset + value.len()),
        })
    }
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s
        .find(|c: char| c.is_whitespace())
        .unwrap_or(s.len());
    (&s[..end], &s[end..])
}

fn parse_value_source(s: &str, offset: usize) -> Result<ValueSource, Diag> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        let inner = inner.trim();
        let (kind, rest) = split_first_word(inner);
        let rest = rest.trim();
        match kind {
            "ENV" => Ok(ValueSource::Env {
                name: rest.to_string(),
                span: offset..offset + s.len(),
            }),
            "HEADER" => Ok(ValueSource::Header {
                name: rest.to_string(),
                span: offset..offset + s.len(),
            }),
            other => Err(Diag::error(
                format!("unknown value source `{}`", other),
                offset..offset + s.len(),
                "expected [ENV NAME] or [HEADER NAME]",
            )),
        }
    } else if !s.is_empty() {
        Ok(ValueSource::Literal {
            value: s.to_string(),
            span: offset..offset + s.len(),
        })
    } else {
        Err(Diag::error(
            "missing value source",
            offset..offset,
            "expected [ENV NAME], [HEADER NAME] or a literal",
        ))
    }
}

fn parse_auth(value: &str, offset: usize) -> Result<AuthSpec, Diag> {
    let value = value.trim();
    let (scheme, rest) = split_first_word(value);
    let rest = rest.trim();
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(Diag::error(
            format!("unsupported auth scheme `{}`", scheme),
            offset..offset + scheme.len(),
            "only Bearer is supported",
        ));
    }
    let inner = rest
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| {
            Diag::error(
                "missing `[HEADER name]` after Bearer",
                offset..offset + value.len(),
                "expected `AUTH Bearer [HEADER NAME]`",
            )
        })?;
    let (kind, name) = split_first_word(inner.trim());
    let name = name.trim();
    if !kind.eq_ignore_ascii_case("HEADER") {
        return Err(Diag::error(
            format!("unsupported auth source `{}`", kind),
            offset..offset + value.len(),
            "only [HEADER NAME] is supported",
        ));
    }
    if name.is_empty() {
        return Err(Diag::error(
            "missing header name",
            offset..offset + value.len(),
            "expected `[HEADER NAME]`",
        ));
    }
    Ok(AuthSpec::BearerHeader {
        header: name.to_string(),
        span: offset..offset + value.len(),
    })
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(rest) = s.strip_suffix("kb") {
        (rest.trim(), 1024u64)
    } else if let Some(rest) = s.strip_suffix("mb") {
        (rest.trim(), 1024u64 * 1024)
    } else if let Some(rest) = s.strip_suffix("gb") {
        (rest.trim(), 1024u64 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix("b") {
        (rest.trim(), 1u64)
    } else {
        (s.as_str(), 1u64)
    };
    let n: u64 = num.trim().parse().ok()?;
    n.checked_mul(mult)
}

fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(rest) = s.strip_suffix("ms") {
        (rest.trim(), 1u64)
    } else if let Some(rest) = s.strip_suffix("s") {
        (rest.trim(), 1000u64)
    } else if let Some(rest) = s.strip_suffix("m") {
        (rest.trim(), 60_000u64)
    } else {
        (s.as_str(), 1000u64)
    };
    let n: u64 = num.trim().parse().ok()?;
    n.checked_mul(mult)
}

pub fn parse_type_expr(s: &str, offset: usize) -> Result<TypeExpr, Diag> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Diag::error(
            "missing type",
            offset..offset,
            "expected a type expression",
        ));
    }
    // regex
    if let Some(stripped) = s.strip_prefix('/') {
        if let Some(pat) = stripped.strip_suffix('/') {
            return Ok(TypeExpr::Regex {
                pattern: pat.to_string(),
                span: offset..offset + s.len(),
            });
        } else {
            return Err(Diag::error(
                "unterminated regex",
                offset..offset + s.len(),
                "regex must be enclosed in `/.../`",
            ));
        }
    }
    // int range / float range
    if let Some(rest) = s.strip_prefix("int(") {
        if let Some(inner) = rest.strip_suffix(')') {
            let parts: Vec<&str> = inner.splitn(2, "..").collect();
            if parts.len() == 2 {
                if let (Ok(a), Ok(b)) = (parts[0].trim().parse::<i64>(), parts[1].trim().parse::<i64>()) {
                    return Ok(TypeExpr::IntRange {
                        min: a,
                        max: b,
                        span: offset..offset + s.len(),
                    });
                }
            }
            return Err(Diag::error(
                "invalid int range",
                offset..offset + s.len(),
                "expected `int(a..b)`",
            ));
        }
    }
    if let Some(rest) = s.strip_prefix("float(") {
        if let Some(inner) = rest.strip_suffix(')') {
            let parts: Vec<&str> = inner.splitn(2, "..").collect();
            if parts.len() == 2 {
                if let (Ok(a), Ok(b)) = (parts[0].trim().parse::<f64>(), parts[1].trim().parse::<f64>()) {
                    return Ok(TypeExpr::FloatRange {
                        min: a,
                        max: b,
                        span: offset..offset + s.len(),
                    });
                }
            }
            return Err(Diag::error(
                "invalid float range",
                offset..offset + s.len(),
                "expected `float(a..b)`",
            ));
        }
    }
    match s {
        "int" => Ok(TypeExpr::Int),
        "float" => Ok(TypeExpr::Float),
        "boolean" | "bool" => Ok(TypeExpr::Boolean),
        "uuid" => Ok(TypeExpr::Uuid),
        "string" => Ok(TypeExpr::String),
        "json" => Ok(TypeExpr::Json),
        "binary" => Ok(TypeExpr::Binary),
        _ if s.contains('|') => {
            let variants: Vec<String> = s
                .split('|')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            if variants.is_empty() {
                Err(Diag::error(
                    "empty union",
                    offset..offset + s.len(),
                    "expected at least one variant",
                ))
            } else {
                Ok(TypeExpr::Union {
                    variants,
                    span: offset..offset + s.len(),
                })
            }
        }
        other => Err(Diag::error(
            format!("unknown type `{}`", other),
            offset..offset + s.len(),
            "expected int, float, boolean, uuid, string, json, binary, a range, union or regex",
        )),
    }
}
