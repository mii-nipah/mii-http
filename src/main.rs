use mii_http::{check, diag, parser, server};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        "mii-http — run an HTTP server defined by a .http specs file\n\n\
         Usage:\n  \
         mii-http <path>            run the server\n  \
         mii-http --check <path>    validate the specs and exit\n  \
         mii-http --addr 0.0.0.0:8080 <path>   run on a specific address\n"
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::from(2);
    }

    let mut check_only = false;
    let mut addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check_only = true,
            "--addr" => {
                i += 1;
                let a = match args.get(i) {
                    Some(s) => s,
                    None => {
                        eprintln!("--addr requires an argument");
                        return ExitCode::from(2);
                    }
                };
                addr = match a.parse() {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("invalid --addr: {}", e);
                        return ExitCode::from(2);
                    }
                };
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with('-') => {
                path = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("unknown option `{}`", other);
                print_usage();
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let path = match path {
        Some(p) => p,
        None => {
            print_usage();
            return ExitCode::from(2);
        }
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read {}: {}", path.display(), e);
            return ExitCode::from(1);
        }
    };
    let file_name = path.to_string_lossy().to_string();

    let parsed = parser::parse(&source);
    diag::emit_all(&parsed.diags, &file_name, &source);
    let has_parse_errors = parsed
        .diags
        .iter()
        .any(|d| d.kind == diag::DiagKind::Error);
    let spec = match parsed.spec {
        Some(s) if !has_parse_errors => s,
        _ => {
            eprintln!("aborting due to parse errors");
            return ExitCode::from(1);
        }
    };

    let semantic_diags = check::check(&spec);
    diag::emit_all(&semantic_diags, &file_name, &source);
    let has_semantic_errors = semantic_diags
        .iter()
        .any(|d| d.kind == diag::DiagKind::Error);

    if check_only {
        if has_semantic_errors {
            eprintln!("validation failed");
            return ExitCode::from(1);
        }
        eprintln!("ok — {} endpoint(s) validated", spec.endpoints.len());
        return ExitCode::SUCCESS;
    }
    if has_semantic_errors {
        eprintln!("aborting due to validation errors");
        return ExitCode::from(1);
    }

    if let Err(e) = server::serve(spec, addr).await {
        eprintln!("server error: {}", e);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
