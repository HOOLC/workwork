use std::sync::Arc;

use gpui::{prelude::*, px, size, AnyWindowHandle, App, Bounds, WindowAppearance};
use gpui_platform::application;
use zork_gui::api::GatewayClient;
use zork_gui::assets::EmbeddedAssets;
use zork_gui::automation::{AutomationRoot, DevAutomation, DEFAULT_DEV_PORT};
use zork_gui::components;
use zork_gui::views::RootView;
use zork_gui::window_chrome::native_titlebar_options;

const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:3000";

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    gateway_url: String,
    gateway_token: Option<String>,
    dev: bool,
    dev_port: u16,
    dev_token: Option<String>,
    help: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            gateway_url: DEFAULT_GATEWAY_URL.to_owned(),
            gateway_token: None,
            dev: false,
            dev_port: DEFAULT_DEV_PORT,
            dev_token: None,
            help: false,
        }
    }
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<CliOptions, String> {
    let mut options = CliOptions::default();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--gateway-url" => {
                options.gateway_url = args
                    .next()
                    .ok_or_else(|| "--gateway-url requires a value".to_owned())?;
            }
            "--gateway-token" => {
                options.gateway_token = Some(
                    args.next()
                        .ok_or_else(|| "--gateway-token requires a value".to_owned())?,
                );
            }
            "--dev" => options.dev = true,
            "--dev-port" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--dev-port requires a value".to_owned())?;
                options.dev_port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --dev-port value: {value}"))?;
            }
            "--dev-token" => {
                options.dev_token = Some(
                    args.next()
                        .ok_or_else(|| "--dev-token requires a value".to_owned())?,
                );
            }
            "--help" | "-h" => options.help = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

fn print_help() {
    println!(
        "zork-gui [options]\n\n\
         Options:\n\
         --gateway-url <url>  gateway runtime URL (default {DEFAULT_GATEWAY_URL})\n\
         --gateway-token <t>  optional bearer token for the gateway\n\
         --dev                enable loopback-only UI automation API\n\
         --dev-port <port>    automation port; 0 chooses a free port (default {DEFAULT_DEV_PORT})\n\
         --dev-token <token>  automation bearer token (default: generated)\n\
         Environment:\n\
         ZORK_GATEWAY_TOKEN and ZORK_GUI_DEV_TOKEN provide the corresponding tokens.\n"
    );
}

fn main() {
    let options = parse_args_from(std::env::args().skip(1)).unwrap_or_else(|error| {
        eprintln!("{error}\nTry zork-gui --help");
        std::process::exit(2);
    });
    if options.help {
        print_help();
        return;
    }
    let client = Arc::new(GatewayClient::new(
        options.gateway_url,
        options
            .gateway_token
            .or_else(|| std::env::var("ZORK_GATEWAY_TOKEN").ok()),
    ));
    let mut automation = options.dev.then(|| {
        let token = options
            .dev_token
            .or_else(|| std::env::var("ZORK_GUI_DEV_TOKEN").ok());
        DevAutomation::bind(options.dev_port, token).unwrap_or_else(|error| {
            eprintln!("failed to start zork-gui dev API: {error}");
            std::process::exit(2);
        })
    });
    if let Some(automation) = automation.as_ref() {
        eprintln!(
            "zork-gui dev API: http://{}/v1\nzork-gui dev token: {}",
            automation.address(),
            automation.token()
        );
    }

    application()
        .with_assets(EmbeddedAssets)
        .run(move |cx: &mut App| {
            components::init(cx);
            cx.set_window_appearance(Some(WindowAppearance::Light));
            let window_options = gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.0), px(800.0)),
                    cx,
                ))),
                titlebar: Some(native_titlebar_options()),
                ..Default::default()
            };
            let window: AnyWindowHandle = if let Some(dev) = automation.as_ref() {
                dev.install(cx);
                cx.open_window(window_options, |_window, cx| {
                    let root = cx.new(|cx| RootView::new(client.clone(), cx));
                    cx.new(|_| AutomationRoot::new(root))
                })
                .expect("failed to open window")
                .into()
            } else {
                cx.open_window(window_options, |_window, cx| {
                    cx.new(|cx| RootView::new(client.clone(), cx))
                })
                .expect("failed to open window")
                .into()
            };
            if let Some(dev) = automation.take() {
                dev.attach(window, cx);
            }
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dev_automation_options() {
        let options = parse_args_from(
            [
                "--dev",
                "--dev-port",
                "0",
                "--dev-token",
                "test-token",
                "--gateway-url",
                "http://127.0.0.1:9999",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("arguments should parse");

        assert!(options.dev);
        assert_eq!(options.dev_port, 0);
        assert_eq!(options.dev_token.as_deref(), Some("test-token"));
        assert_eq!(options.gateway_url, "http://127.0.0.1:9999");
    }

    #[test]
    fn rejects_unknown_or_incomplete_options() {
        assert!(parse_args_from(["--unknown".to_owned()]).is_err());
        assert!(parse_args_from(["--dev-port".to_owned()]).is_err());
        assert!(parse_args_from(["--dev-port".to_owned(), "70000".to_owned()]).is_err());
    }
}
