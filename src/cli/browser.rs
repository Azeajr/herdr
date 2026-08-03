use crate::api::schema::{
    BrowserNavigateParams, BrowserOpenParams, BrowserPaneTarget, Method, Request,
};

pub(super) fn run_browser_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_browser_help();
        return Ok(2);
    };

    match subcommand {
        "open" => browser_open(&args[1..]),
        "navigate" => browser_navigate(&args[1..]),
        "reload" => browser_page_action(&args[1..], "reload", Method::BrowserReload),
        "back" => browser_page_action(&args[1..], "back", Method::BrowserBack),
        "forward" => browser_page_action(&args[1..], "forward", Method::BrowserForward),
        "close" => browser_page_action(&args[1..], "close", Method::BrowserClose),
        "info" => browser_info(&args[1..]),
        "help" | "--help" | "-h" => {
            print_browser_help();
            Ok(0)
        }
        _ => {
            print_browser_help();
            Ok(2)
        }
    }
}

fn browser_open(args: &[String]) -> std::io::Result<i32> {
    let mut pane_id = None;
    let mut url = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("usage: herdr browser open [url] [--pane <pane_id>]");
                    return Ok(2);
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            other => {
                url = Some(other.to_string());
                index += 1;
            }
        }
    }
    super::send_ok_request(Method::BrowserOpen(BrowserOpenParams { pane_id, url }))
}

fn browser_navigate(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr browser navigate <pane_id> <url>");
        return Ok(2);
    }
    let pane_id = super::normalize_pane_id(&args[0]);
    let url = args[1].clone();
    super::send_ok_request(Method::BrowserNavigate(BrowserNavigateParams {
        pane_id,
        url,
    }))
}

/// Shared shape for the subcommands whose only argument is the pane.
fn browser_page_action(
    args: &[String],
    name: &str,
    method: fn(BrowserPaneTarget) -> Method,
) -> std::io::Result<i32> {
    let Some(pane_id) = args.first() else {
        eprintln!("usage: herdr browser {name} <pane_id>");
        return Ok(2);
    };
    super::send_ok_request(method(BrowserPaneTarget {
        pane_id: super::normalize_pane_id(pane_id),
    }))
}

fn browser_info(args: &[String]) -> std::io::Result<i32> {
    let Some(pane_id) = args.first() else {
        eprintln!("usage: herdr browser info <pane_id>");
        return Ok(2);
    };
    let response = super::send_request(&Request {
        id: "cli:browser:info".into(),
        method: Method::BrowserInfo(BrowserPaneTarget {
            pane_id: super::normalize_pane_id(pane_id),
        }),
    })?;
    super::print_response(&response)
}

fn print_browser_help() {
    println!("usage: herdr browser <subcommand>");
    println!();
    println!("Subcommands:");
    println!("  open [url] [--pane <pane_id>]   Split a Browser pane");
    println!("  navigate <pane_id> <url>        Navigate a Browser pane");
    println!("  reload <pane_id>                Reload a Browser pane");
    println!("  back <pane_id>                  Go back in a Browser pane");
    println!("  forward <pane_id>               Go forward in a Browser pane");
    println!("  info <pane_id>                  Show what a Browser pane is displaying");
    println!("  close <pane_id>                 Close a Browser pane");
}
