use crate::api::schema::{BrowserNavigateParams, BrowserOpenParams, Method};

pub(super) fn run_browser_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_browser_help();
        return Ok(2);
    };

    match subcommand {
        "open" => browser_open(&args[1..]),
        "navigate" => browser_navigate(&args[1..]),
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
    super::send_ok_request(Method::BrowserNavigate(BrowserNavigateParams { pane_id, url }))
}

fn print_browser_help() {
    println!("usage: herdr browser <subcommand>");
    println!();
    println!("Subcommands:");
    println!("  open [url] [--pane <pane_id>]   Split a Browser pane");
    println!("  navigate <pane_id> <url>        Navigate a Browser pane");
}
