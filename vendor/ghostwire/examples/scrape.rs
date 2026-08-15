//! # ghostwire — CLI example
//!
//! Fetch a URL through ghostwire's Cloudflare bypass logic.
//!
//! Usage:
//!   cargo run --example scrape -- [OPTIONS] <URL>
//!
//! Options:
//!   -X, --method <METHOD>              HTTP method (default: GET)
//!   -p, --proxy <URL>                  Proxy URL
//!       --captcha-provider <PROVIDER>  2captcha | anticaptcha | capsolver
//!       --api-key <KEY>                Captcha provider API key
//!       --no-stealth                   Disable stealth mode
//!   -d, --debug                        Enable debug-level tracing
//!   -h, --help                         Print this help

use std::process;

use ghostwire::{GhostwireBuilder, RequestOptions, StealthConfig, captcha::CaptchaConfig};
use reqwest;
use tracing_subscriber::{EnvFilter, fmt};

fn print_usage() {
    eprintln!(
        "Usage: cargo run --example scrape -- [OPTIONS] <URL>

Options:
  -X, --method <METHOD>              HTTP method (default: GET)
  -p, --proxy <URL>                  Proxy URL
      --captcha-provider <PROVIDER>  2captcha | anticaptcha | capsolver
      --api-key <KEY>                Captcha provider API key
      --no-stealth                   Disable stealth mode
  -d, --debug                        Enable debug-level tracing
  -h, --help                         Print this help"
    );
}

struct Args {
    url: String,
    method: String,
    proxy: Option<String>,
    captcha_provider: Option<String>,
    api_key: Option<String>,
    no_stealth: bool,
    debug: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1).peekable();

    let mut url: Option<String> = None;
    let mut method = "GET".to_string();
    let mut proxy: Option<String> = None;
    let mut captcha_provider: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut no_stealth = false;
    let mut debug = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                process::exit(0);
            }
            "-d" | "--debug" => debug = true,
            "--no-stealth" => no_stealth = true,
            "-X" | "--method" => {
                method = args.next().unwrap_or_else(|| {
                    eprintln!("error: {arg} requires a value");
                    process::exit(1);
                });
            }
            "-p" | "--proxy" => {
                proxy = Some(args.next().unwrap_or_else(|| {
                    eprintln!("error: {arg} requires a value");
                    process::exit(1);
                }));
            }
            "--captcha-provider" => {
                captcha_provider = Some(args.next().unwrap_or_else(|| {
                    eprintln!("error: {arg} requires a value");
                    process::exit(1);
                }));
            }
            "--api-key" => {
                api_key = Some(args.next().unwrap_or_else(|| {
                    eprintln!("error: {arg} requires a value");
                    process::exit(1);
                }));
            }
            other if other.starts_with('-') => {
                eprintln!("error: unknown option '{other}'");
                print_usage();
                process::exit(1);
            }
            positional => {
                if url.is_some() {
                    eprintln!("error: unexpected argument '{positional}'");
                    process::exit(1);
                }
                url = Some(positional.to_string());
            }
        }
    }

    let url = url.unwrap_or_else(|| {
        eprintln!("error: URL is required");
        print_usage();
        process::exit(1);
    });

    Args {
        url,
        method,
        proxy,
        captcha_provider,
        api_key,
        no_stealth,
        debug,
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    let default_level = if args.debug {
        "ghostwire=debug"
    } else {
        "ghostwire=info"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    fmt().with_env_filter(filter).with_target(false).init();

    let captcha = match (args.captcha_provider.as_deref(), args.api_key.as_deref()) {
        (Some(provider), Some(key)) => Some(CaptchaConfig {
            provider: provider.to_string(),
            api_key: Some(key.to_string()),
            ..Default::default()
        }),
        (Some(_), None) => {
            eprintln!("error: --captcha-provider requires --api-key");
            process::exit(1);
        }
        _ => None,
    };

    let stealth = StealthConfig {
        enabled: !args.no_stealth,
        ..Default::default()
    };

    let mut builder = GhostwireBuilder::new().debug(args.debug).stealth(stealth);
    if let Some(cap) = captcha {
        builder = builder.captcha(cap);
    }
    if let Some(proxy) = args.proxy {
        builder = builder.add_proxy(proxy);
    }

    let mut ghostwire = match builder.build() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to create ghostwire: {e}");
            process::exit(1);
        }
    };

    let method = match args.method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "HEAD" => reqwest::Method::HEAD,
        other => {
            eprintln!("error: unsupported method '{other}'");
            process::exit(1);
        }
    };

    match ghostwire
        .request(method, &args.url, RequestOptions::default())
        .await
    {
        Ok(resp) => {
            eprintln!("Status: {}", resp.status());
            match resp.text().await {
                Ok(body) => print!("{body}"),
                Err(e) => {
                    eprintln!("error: failed to read response body: {e}");
                    process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    }
}
