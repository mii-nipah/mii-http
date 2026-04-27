use mii_http::diag::{Diag, DiagKind};
use mii_http::spec::{
    AuthSpec, BodySpec, Endpoint, JsonFieldType, Method, NamedField, PathSegment, Setup, TypeExpr,
};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{ToTokens, format_ident, quote};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Ident, LitStr, Result, Token, Type, Visibility, parse_macro_input};

#[proc_macro]
pub fn client(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ClientInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct ClientInput {
    vis: Visibility,
    name: Ident,
    spec_path: LitStr,
    mappings: Vec<EndpointMapping>,
}

struct EndpointMapping {
    method: Ident,
    path: String,
    span: Span,
    fn_name: Ident,
    output: Type,
}

impl Parse for ClientInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let vis = input.parse()?;
        input.parse::<Token![struct]>()?;
        let name = input.parse()?;
        input.parse::<Token![;]>()?;

        let spec_key: Ident = input.parse()?;
        if spec_key != "spec" {
            return Err(syn::Error::new(spec_key.span(), "expected `spec = ...`"));
        }
        input.parse::<Token![=]>()?;
        let spec_path = input.parse()?;
        let _ = input.parse::<Option<Token![;]>>()?;

        let mut mappings = Vec::new();
        while !input.is_empty() {
            mappings.push(input.parse()?);
        }

        Ok(Self {
            vis,
            name,
            spec_path,
            mappings,
        })
    }
}

impl Parse for EndpointMapping {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let method: Ident = input.parse()?;
        let span = method.span();
        let mut path_tokens = Vec::new();
        while !input.peek(Token![as]) {
            if input.is_empty() {
                return Err(syn::Error::new(
                    span,
                    "expected `as method_name => ReturnType;`",
                ));
            }
            path_tokens.push(input.parse::<TokenTree>()?);
        }
        input.parse::<Token![as]>()?;
        let fn_name = input.parse()?;
        input.parse::<Token![=>]>()?;
        let output = input.parse()?;
        input.parse::<Token![;]>()?;
        let path = parse_path_tokens(&path_tokens, span)?;

        Ok(Self {
            method,
            path,
            span,
            fn_name,
            output,
        })
    }
}

fn expand(input: ClientInput) -> Result<TokenStream2> {
    let spec_path = resolve_spec_path(&input.spec_path)?;
    let source = std::fs::read_to_string(&spec_path).map_err(|error| {
        syn::Error::new(
            input.spec_path.span(),
            format!("failed to read spec `{}`: {}", spec_path.display(), error),
        )
    })?;

    let parsed = mii_http::parse::parse(&source);
    let parse_errors = errors_only(&parsed.diags);
    if !parse_errors.is_empty() {
        return Err(syn::Error::new(
            input.spec_path.span(),
            format_diagnostics("spec parse failed", &parse_errors, &source),
        ));
    }
    let spec = parsed.spec.ok_or_else(|| {
        syn::Error::new(input.spec_path.span(), "spec parser did not produce an AST")
    })?;

    let semantic_diags = mii_http::check::check(&spec);
    let semantic_errors = errors_only(&semantic_diags);
    if !semantic_errors.is_empty() {
        return Err(syn::Error::new(
            input.spec_path.span(),
            format_diagnostics("spec validation failed", &semantic_errors, &source),
        ));
    }

    let mut errors: Option<syn::Error> = None;
    if input.mappings.is_empty() {
        push_error(
            &mut errors,
            syn::Error::new(input.name.span(), "client must map at least one endpoint"),
        );
    }

    let mut method_names = HashSet::new();
    let mut mapped_endpoints = HashSet::new();
    let mut request_types = Vec::new();
    let mut methods = Vec::new();
    for mapping in &input.mappings {
        if !method_names.insert(mapping.fn_name.to_string()) {
            push_error(
                &mut errors,
                syn::Error::new(
                    mapping.fn_name.span(),
                    format!("duplicate generated method `{}`", mapping.fn_name),
                ),
            );
            continue;
        }

        let method = match parse_method(&mapping.method) {
            Ok(method) => method,
            Err(error) => {
                push_error(&mut errors, error);
                continue;
            }
        };

        let matches: Vec<&Endpoint> = spec
            .endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.method == method && endpoint_matches_path(endpoint, &mapping.path)
            })
            .collect();
        let endpoint = match matches.as_slice() {
            [] => {
                push_error(
                    &mut errors,
                    syn::Error::new(
                        mapping.span,
                        format!(
                            "no endpoint matches `{} {}` in `{}`",
                            method.as_str(),
                            mapping.path,
                            input.spec_path.value()
                        ),
                    ),
                );
                continue;
            }
            [endpoint] => *endpoint,
            _ => {
                push_error(
                    &mut errors,
                    syn::Error::new(
                        mapping.span,
                        format!(
                            "`{} {}` matches multiple endpoints; include the full typed path",
                            method.as_str(),
                            mapping.path
                        ),
                    ),
                );
                continue;
            }
        };

        let endpoint_key = format!("{} {}", endpoint.method.as_str(), endpoint.path);
        if !mapped_endpoints.insert(endpoint_key.clone()) {
            push_error(
                &mut errors,
                syn::Error::new(
                    mapping.span,
                    format!("endpoint `{}` is mapped more than once", endpoint_key),
                ),
            );
            continue;
        }

        match generate_endpoint(&input.vis, &spec.setup, endpoint, mapping) {
            Ok(generated) => {
                request_types.extend(generated.request_types);
                methods.push(generated.method);
            }
            Err(error) => push_error(&mut errors, error),
        }
    }

    if let Some(error) = errors {
        return Err(error);
    }

    let vis = &input.vis;
    let name = &input.name;
    let spec_path_str = spec_path
        .to_str()
        .ok_or_else(|| syn::Error::new(input.spec_path.span(), "spec path is not valid UTF-8"))?;

    Ok(quote! {
        const _: &str = include_str!(#spec_path_str);

        #[derive(Clone, Debug)]
        #vis struct #name {
            client: ::mii_http_client::Client,
        }

        impl #name {
            pub fn new(base_url: impl AsRef<str>) -> ::mii_http_client::Result<Self> {
                Ok(Self {
                    client: ::mii_http_client::Client::new(base_url)?,
                })
            }

            pub fn with_http_client(
                base_url: impl AsRef<str>,
                http: ::mii_http_client::reqwest::Client,
            ) -> ::mii_http_client::Result<Self> {
                Ok(Self {
                    client: ::mii_http_client::Client::with_http_client(base_url, http)?,
                })
            }

            pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
                self.client = self.client.bearer_token(token);
                self
            }

            pub fn set_bearer_token(&mut self, token: impl Into<String>) {
                self.client.set_bearer_token(token);
            }

            pub fn clear_bearer_token(&mut self) {
                self.client.clear_bearer_token();
            }

            pub fn inner(&self) -> &::mii_http_client::Client {
                &self.client
            }

            #(#methods)*
        }

        #(#request_types)*
    })
}

struct GeneratedEndpoint {
    request_types: Vec<TokenStream2>,
    method: TokenStream2,
}

fn generate_endpoint(
    vis: &Visibility,
    setup: &Setup,
    endpoint: &Endpoint,
    mapping: &EndpointMapping,
) -> Result<GeneratedEndpoint> {
    if endpoint.response_type.as_deref().unwrap_or("").is_empty() {
        return Err(syn::Error::new(
            mapping.span,
            format!(
                "`{} {}` needs a Response-Type before a client can be generated",
                endpoint.method.as_str(),
                endpoint.path
            ),
        ));
    }

    let output_kind = output_kind(&mapping.output);
    if endpoint.response_stream && output_kind != OutputKind::ByteStream {
        return Err(syn::Error::new(
            mapping.output.span(),
            "streaming endpoints must use `mii_http_client::ByteStream` as their return type",
        ));
    }
    if !endpoint.response_stream && output_kind == OutputKind::ByteStream {
        return Err(syn::Error::new(
            mapping.output.span(),
            "`mii_http_client::ByteStream` can only be used with `Response-Type stream ...` endpoints",
        ));
    }

    let fn_name = &mapping.fn_name;
    let request_name = format_ident!("{}Request", upper_camel(&fn_name.to_string()));
    let body_name = format_ident!("{}Body", upper_camel(&fn_name.to_string()));
    let mut request_fields = Vec::new();
    let mut request_field_defs = Vec::new();
    let mut request_field_names = HashSet::new();

    for field in path_fields(endpoint) {
        push_request_field(
            &mut request_fields,
            &mut request_field_defs,
            &mut request_field_names,
            field,
            mapping.span,
        )?;
    }
    for field in endpoint
        .query_params
        .iter()
        .map(|field| input_field(field, FieldSource::Query))
    {
        push_request_field(
            &mut request_fields,
            &mut request_field_defs,
            &mut request_field_names,
            field,
            mapping.span,
        )?;
    }
    for field in endpoint
        .headers
        .iter()
        .map(|field| input_field(field, FieldSource::Header))
    {
        push_request_field(
            &mut request_fields,
            &mut request_field_defs,
            &mut request_field_names,
            field,
            mapping.span,
        )?;
    }

    let mut request_types = Vec::new();
    let mut body_apply = TokenStream2::new();
    let mut body_is_multipart = false;
    if let Some(body) = &endpoint.body {
        let body_ty = match body {
            BodySpec::String { .. } => quote! { ::std::string::String },
            BodySpec::Binary { .. } => quote! { ::std::vec::Vec<u8> },
            BodySpec::Json { schema: None, .. } => quote! { ::mii_http_client::serde_json::Value },
            BodySpec::Json {
                schema: Some(schema),
                ..
            } => {
                let fields = json_body_fields(&schema.fields, mapping.span)?;
                request_types.push(body_struct(
                    vis,
                    &body_name,
                    fields,
                    true,
                    TokenStream2::new(),
                ));
                quote! { #body_name }
            }
            BodySpec::Form { fields, .. } => {
                let has_binary = fields
                    .iter()
                    .any(|field| matches!(field.ty, TypeExpr::Binary));
                body_is_multipart = has_binary;
                let field_tokens = form_body_fields(fields, mapping.span, !has_binary)?;
                let multipart_impl = if has_binary {
                    multipart_body_impl(&body_name, fields)
                } else {
                    TokenStream2::new()
                };
                request_types.push(body_struct(
                    vis,
                    &body_name,
                    field_tokens,
                    !has_binary,
                    multipart_impl,
                ));
                quote! { #body_name }
            }
        };

        if !request_field_names.insert("body".into()) {
            return Err(syn::Error::new(
                mapping.span,
                "generated request field `body` collides with another spec input",
            ));
        }
        let body_ident = format_ident!("body");
        request_field_defs.push(quote! {
            pub #body_ident: #body_ty
        });
        body_apply = match body {
            BodySpec::String { .. } | BodySpec::Binary { .. } => {
                quote! {
                    request_builder = request_builder.body(request.body);
                }
            }
            BodySpec::Json { .. } => {
                quote! {
                    request_builder = request_builder.json(&request.body);
                }
            }
            BodySpec::Form { .. } if body_is_multipart => {
                quote! {
                    request_builder = request_builder.multipart(request.body.into_multipart_form().await?);
                }
            }
            BodySpec::Form { .. } => {
                quote! {
                    request_builder = request_builder.form(&request.body);
                }
            }
        };
    }

    let needs_request = !request_field_defs.is_empty();
    if needs_request {
        request_types.insert(
            0,
            quote! {
                #[derive(Debug, Clone)]
                #vis struct #request_name {
                    #(#request_field_defs,)*
                }
            },
        );
    }

    let path_build = build_path(endpoint, setup);
    let method = method_tokens(endpoint.method);
    let auth_header = auth_header_tokens(setup);
    let query_apply = build_query_apply(&request_fields);
    let header_apply = build_header_apply(&request_fields);
    let decode = decode_response(&mapping.output, output_kind);
    let output = &mapping.output;

    let args = if needs_request {
        quote! { request: #request_name }
    } else {
        quote! {}
    };

    let method = quote! {
        pub async fn #fn_name(&self, #args) -> ::mii_http_client::Result<#output> {
            #path_build
            let mut request_builder = self.client.request(#method, &path, #auth_header)?;
            #query_apply
            #header_apply
            #body_apply
            let response = ::mii_http_client::ensure_success(request_builder.send().await?).await?;
            #decode
        }
    };

    Ok(GeneratedEndpoint {
        request_types,
        method,
    })
}

#[derive(Clone)]
struct InputField {
    wire_name: String,
    rust_name: Ident,
    ty: TokenStream2,
    optional: bool,
    source: FieldSource,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldSource {
    Path,
    Query,
    Header,
}

fn path_fields(endpoint: &Endpoint) -> Vec<InputField> {
    endpoint
        .path_segments
        .iter()
        .filter_map(|segment| match segment {
            PathSegment::Param { name, ty, .. } => Some(InputField {
                wire_name: name.clone(),
                rust_name: format_ident!("{}", sanitize_ident(name)),
                ty: rust_type_for(ty),
                optional: false,
                source: FieldSource::Path,
            }),
            PathSegment::Literal(_) => None,
        })
        .collect()
}

fn input_field(field: &NamedField, source: FieldSource) -> InputField {
    InputField {
        wire_name: field.name.clone(),
        rust_name: format_ident!("{}", sanitize_ident(&field.name)),
        ty: rust_type_for(&field.ty),
        optional: field.optional,
        source,
    }
}

fn push_request_field(
    request_fields: &mut Vec<InputField>,
    request_field_defs: &mut Vec<TokenStream2>,
    request_field_names: &mut HashSet<String>,
    field: InputField,
    span: Span,
) -> Result<()> {
    let rust_name = field.rust_name.to_string();
    if !request_field_names.insert(rust_name.clone()) {
        return Err(syn::Error::new(
            span,
            format!(
                "generated request field `{}` collides; rename one of the spec inputs",
                rust_name
            ),
        ));
    }

    let ident = &field.rust_name;
    let ty = if field.optional {
        let inner = &field.ty;
        quote! { ::std::option::Option<#inner> }
    } else {
        field.ty.clone()
    };
    request_field_defs.push(quote! {
        pub #ident: #ty
    });
    request_fields.push(field);
    Ok(())
}

fn json_body_fields(fields: &[mii_http::spec::JsonField], span: Span) -> Result<Vec<TokenStream2>> {
    let mut names = HashSet::new();
    fields
        .iter()
        .map(|field| {
            let ident = format_ident!("{}", sanitize_ident(&field.name));
            check_body_field_name(&mut names, &ident, span)?;
            let ty = match &field.ty {
                JsonFieldType::Scalar(ty) => rust_type_for(ty),
                JsonFieldType::Array(ty) => {
                    let ty = rust_type_for(ty);
                    quote! { ::std::vec::Vec<#ty> }
                }
            };
            Ok(body_field_tokens(
                &field.name,
                &ident,
                ty,
                field.optional,
                true,
            ))
        })
        .collect()
}

fn form_body_fields(
    fields: &[NamedField],
    span: Span,
    serde_attrs: bool,
) -> Result<Vec<TokenStream2>> {
    let mut names = HashSet::new();
    fields
        .iter()
        .map(|field| {
            let ident = format_ident!("{}", sanitize_ident(&field.name));
            check_body_field_name(&mut names, &ident, span)?;
            Ok(body_field_tokens(
                &field.name,
                &ident,
                form_body_type_for(&field.ty),
                field.optional,
                serde_attrs,
            ))
        })
        .collect()
}

fn check_body_field_name(names: &mut HashSet<String>, ident: &Ident, span: Span) -> Result<()> {
    let name = ident.to_string();
    if !names.insert(name.clone()) {
        return Err(syn::Error::new(
            span,
            format!(
                "generated body field `{}` collides; rename one of the schema fields",
                name
            ),
        ));
    }
    Ok(())
}

fn body_field_tokens(
    wire_name: &str,
    ident: &Ident,
    ty: TokenStream2,
    optional: bool,
    serde_attrs: bool,
) -> TokenStream2 {
    let ty = if optional {
        quote! { ::std::option::Option<#ty> }
    } else {
        ty
    };
    let rename = if !serde_attrs || ident == wire_name {
        quote! {}
    } else {
        quote! { #[serde(rename = #wire_name)] }
    };
    let skip = if serde_attrs && optional {
        quote! { #[serde(skip_serializing_if = "Option::is_none")] }
    } else {
        quote! {}
    };
    quote! {
        #rename
        #skip
        pub #ident: #ty
    }
}

fn multipart_body_impl(name: &Ident, fields: &[NamedField]) -> TokenStream2 {
    let pushes = fields
        .iter()
        .map(|field| {
            let ident = format_ident!("{}", sanitize_ident(&field.name));
            let wire_name = &field.name;
            match (matches!(field.ty, TypeExpr::Binary), field.optional) {
                (true, true) => quote! {
                    if let Some(value) = self.#ident {
                        form = form.part(#wire_name, value.into_multipart_part().await?);
                    }
                },
                (true, false) => quote! {
                    form = form.part(#wire_name, self.#ident.into_multipart_part().await?);
                },
                (false, true) => quote! {
                    if let Some(value) = self.#ident {
                        form = form.text(#wire_name, value.to_string());
                    }
                },
                (false, false) => quote! {
                    form = form.text(#wire_name, self.#ident.to_string());
                },
            }
        })
        .collect::<Vec<_>>();

    quote! {
        impl #name {
            pub async fn into_multipart_form(
                self,
            ) -> ::mii_http_client::Result<::mii_http_client::reqwest::multipart::Form> {
                let mut form = ::mii_http_client::reqwest::multipart::Form::new();
                #(#pushes)*
                Ok(form)
            }
        }
    }
}

fn body_struct(
    vis: &Visibility,
    name: &Ident,
    fields: Vec<TokenStream2>,
    serialize: bool,
    extra_impl: TokenStream2,
) -> TokenStream2 {
    let serde_derive = if serialize {
        quote! { , ::mii_http_client::serde::Serialize }
    } else {
        TokenStream2::new()
    };
    let serde_crate = if serialize {
        quote! { #[serde(crate = "::mii_http_client::serde")] }
    } else {
        TokenStream2::new()
    };

    quote! {
        #[derive(Debug, Clone #serde_derive)]
        #serde_crate
        #vis struct #name {
            #(#fields,)*
        }

        #extra_impl
    }
}

fn build_path(endpoint: &Endpoint, setup: &Setup) -> TokenStream2 {
    let prefix = compute_prefix(setup);
    let mut parts = Vec::new();
    if !prefix.is_empty() {
        parts.push(quote! {
            path.push_str(#prefix);
        });
    }
    for segment in &endpoint.path_segments {
        match segment {
            PathSegment::Literal(value) => {
                parts.push(quote! {
                    path.push('/');
                    path.push_str(#value);
                });
            }
            PathSegment::Param { name, .. } => {
                let ident = format_ident!("{}", sanitize_ident(name));
                parts.push(quote! {
                    path.push('/');
                    path.push_str(&::mii_http_client::encode_path_segment(&request.#ident));
                });
            }
        }
    }
    if endpoint.path_segments.is_empty() {
        parts.push(quote! {
            path.push('/');
        });
    }

    quote! {
        let mut path = ::std::string::String::new();
        #(#parts)*
    }
}

fn build_query_apply(fields: &[InputField]) -> TokenStream2 {
    let pushes = fields
        .iter()
        .filter(|field| field.source == FieldSource::Query)
        .map(|field| {
            let ident = &field.rust_name;
            let wire_name = &field.wire_name;
            if field.optional {
                quote! {
                    if let Some(value) = &request.#ident {
                        query_params.push((#wire_name, value.to_string()));
                    }
                }
            } else {
                quote! {
                    query_params.push((#wire_name, request.#ident.to_string()));
                }
            }
        })
        .collect::<Vec<_>>();

    if pushes.is_empty() {
        return TokenStream2::new();
    }

    quote! {
        let mut query_params = ::std::vec::Vec::<(&str, ::std::string::String)>::new();
        #(#pushes)*
        request_builder = request_builder.query(&query_params);
    }
}

fn build_header_apply(fields: &[InputField]) -> TokenStream2 {
    fields
        .iter()
        .filter(|field| field.source == FieldSource::Header)
        .map(|field| {
            let ident = &field.rust_name;
            let wire_name = &field.wire_name;
            if field.optional {
                quote! {
                    if let Some(value) = &request.#ident {
                        request_builder = request_builder.header(#wire_name, value.to_string());
                    }
                }
            } else {
                quote! {
                    request_builder = request_builder.header(#wire_name, request.#ident.to_string());
                }
            }
        })
        .collect()
}

fn decode_response(output: &Type, kind: OutputKind) -> TokenStream2 {
    match kind {
        OutputKind::Text => quote! {
            Ok(response.text().await?)
        },
        OutputKind::Bytes => quote! {
            Ok(response.bytes().await?)
        },
        OutputKind::VecBytes => quote! {
            Ok(response.bytes().await?.to_vec())
        },
        OutputKind::ByteStream => quote! {
            Ok(::mii_http_client::ByteStream::new(response))
        },
        OutputKind::Json => quote! {
            Ok(response.json::<#output>().await?)
        },
    }
}

fn rust_type_for(ty: &TypeExpr) -> TokenStream2 {
    match ty {
        TypeExpr::Int | TypeExpr::IntRange { .. } => quote! { i64 },
        TypeExpr::Float | TypeExpr::FloatRange { .. } => quote! { f64 },
        TypeExpr::Boolean => quote! { bool },
        TypeExpr::Uuid => quote! { ::mii_http_client::uuid::Uuid },
        TypeExpr::String | TypeExpr::Regex { .. } | TypeExpr::Union { .. } => {
            quote! { ::std::string::String }
        }
        TypeExpr::Json => quote! { ::mii_http_client::serde_json::Value },
        TypeExpr::Binary => quote! { ::std::vec::Vec<u8> },
    }
}

fn form_body_type_for(ty: &TypeExpr) -> TokenStream2 {
    if matches!(ty, TypeExpr::Binary) {
        quote! { ::mii_http_client::FilePart }
    } else {
        rust_type_for(ty)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Text,
    Bytes,
    VecBytes,
    ByteStream,
    Json,
}

fn output_kind(ty: &Type) -> OutputKind {
    let compact = ty.to_token_stream().to_string().replace(' ', "");
    match compact.as_str() {
        "String" | "std::string::String" | "::std::string::String" => OutputKind::Text,
        "Vec<u8>" | "std::vec::Vec<u8>" | "::std::vec::Vec<u8>" => OutputKind::VecBytes,
        "Bytes" | "mii_http_client::Bytes" | "::mii_http_client::Bytes" => OutputKind::Bytes,
        "ByteStream" | "mii_http_client::ByteStream" | "::mii_http_client::ByteStream" => {
            OutputKind::ByteStream
        }
        _ => OutputKind::Json,
    }
}

fn method_tokens(method: Method) -> TokenStream2 {
    match method {
        Method::Get => quote! { ::mii_http_client::reqwest::Method::GET },
        Method::Post => quote! { ::mii_http_client::reqwest::Method::POST },
        Method::Put => quote! { ::mii_http_client::reqwest::Method::PUT },
        Method::Delete => quote! { ::mii_http_client::reqwest::Method::DELETE },
        Method::Patch => quote! { ::mii_http_client::reqwest::Method::PATCH },
    }
}

fn auth_header_tokens(setup: &Setup) -> TokenStream2 {
    match &setup.auth {
        Some(AuthSpec::BearerHeader { header, .. }) => quote! { Some(#header) },
        None => quote! { None },
    }
}

fn parse_method(method: &Ident) -> Result<Method> {
    match method.to_string().as_str() {
        "GET" => Ok(Method::Get),
        "POST" => Ok(Method::Post),
        "PUT" => Ok(Method::Put),
        "DELETE" => Ok(Method::Delete),
        "PATCH" => Ok(Method::Patch),
        other => Err(syn::Error::new(
            method.span(),
            format!("unsupported HTTP method `{}`", other),
        )),
    }
}

fn endpoint_matches_path(endpoint: &Endpoint, path: &str) -> bool {
    endpoint.path == path || normalized_endpoint_path(endpoint) == path
}

fn normalized_endpoint_path(endpoint: &Endpoint) -> String {
    let mut out = String::new();
    for segment in &endpoint.path_segments {
        out.push('/');
        match segment {
            PathSegment::Literal(value) => out.push_str(value),
            PathSegment::Param { name, .. } => {
                out.push(':');
                out.push_str(name);
            }
        }
    }
    if out.is_empty() { "/".into() } else { out }
}

fn compute_prefix(setup: &Setup) -> String {
    let base = setup.base.clone().unwrap_or_default();
    let version = setup
        .version
        .map(|version| format!("/v{}", version))
        .unwrap_or_default();
    format!("{}{}", base, version)
}

fn parse_path_tokens(tokens: &[TokenTree], span: Span) -> Result<String> {
    if tokens.len() == 1
        && let TokenTree::Literal(literal) = &tokens[0]
        && let Ok(lit) = syn::parse2::<LitStr>(quote! { #literal })
    {
        return Ok(lit.value());
    }

    let path = tokens
        .iter()
        .map(TokenTree::to_string)
        .collect::<Vec<_>>()
        .join("");
    if !path.starts_with('/') {
        return Err(syn::Error::new(span, "endpoint path must start with `/`"));
    }
    Ok(path)
}

fn resolve_spec_path(spec_path: &LitStr) -> Result<PathBuf> {
    let raw = spec_path.value();
    let path = Path::new(&raw);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|error| {
        syn::Error::new(
            spec_path.span(),
            format!("failed to read CARGO_MANIFEST_DIR: {}", error),
        )
    })?;
    Ok(PathBuf::from(manifest_dir).join(path))
}

fn errors_only(diags: &[Diag]) -> Vec<Diag> {
    diags
        .iter()
        .filter(|diag| diag.kind == DiagKind::Error)
        .cloned()
        .collect()
}

fn format_diagnostics(title: &str, diags: &[Diag], source: &str) -> String {
    let mut message = title.to_string();
    for diag in diags.iter().take(5) {
        let (line, column) = line_column(source, diag.span.start);
        message.push_str(&format!(
            "\n{}:{}: {} ({})",
            line + 1,
            column + 1,
            diag.message,
            diag.label
        ));
    }
    if diags.len() > 5 {
        message.push_str(&format!("\n... and {} more error(s)", diags.len() - 5));
    }
    message
}

fn line_column(source: &str, byte_offset: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    (
        line,
        source[line_start..byte_offset.min(source.len())]
            .chars()
            .count(),
    )
}

fn sanitize_ident(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out = out.trim_matches('_').to_string();
    if out.is_empty() {
        out.push_str("value");
    }
    if out.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if is_keyword(&out) {
        out.push('_');
    }
    out
}

fn upper_camel(raw: &str) -> String {
    let mut out = String::new();
    for part in raw.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            for ch in chars {
                out.push(ch);
            }
        }
    }
    if out.is_empty() {
        "Endpoint".into()
    } else {
        out
    }
}

fn is_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
    )
}

fn push_error(errors: &mut Option<syn::Error>, error: syn::Error) {
    if let Some(existing) = errors {
        existing.combine(error);
    } else {
        *errors = Some(error);
    }
}
