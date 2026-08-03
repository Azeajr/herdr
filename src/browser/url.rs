//! Validates and normalizes URLs before they reach the `agent-browser` CLI.
//!
//! Every navigation ends up as a positional argument to a subprocess, so a
//! value starting with `-` would be parsed as a flag rather than a URL. That
//! is the one input here that is a correctness problem rather than a taste
//! problem, and it is rejected outright.

/// Schemes a Browser pane will navigate to. Anything else -- `javascript:`,
/// `data:`, custom app schemes -- is refused: a pane is driven by whoever can
/// reach the socket API, and these are the schemes where that is just
/// browsing.
const ALLOWED_SCHEMES: &[&str] = &["http://", "https://", "file://", "about:"];

/// Returns the URL to navigate to, or a reason it was refused.
///
/// A bare host (`example.com`) gets `https://`, matching what every address
/// bar does; anything with a scheme must have one from [`ALLOWED_SCHEMES`].
pub(crate) fn normalize(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("url is empty".to_string());
    }
    if trimmed.starts_with('-') {
        // `agent-browser goto -foo` would read this as a flag.
        return Err(format!("url must not start with '-': {trimmed}"));
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(format!("url must not contain whitespace: {trimmed}"));
    }
    let lowered = trimmed.to_ascii_lowercase();
    if ALLOWED_SCHEMES
        .iter()
        .any(|scheme| lowered.starts_with(scheme))
    {
        return Ok(trimmed.to_string());
    }
    if has_scheme(&lowered) {
        return Err(format!(
            "unsupported url scheme: {trimmed} (allowed: {})",
            ALLOWED_SCHEMES.join(", ")
        ));
    }
    Ok(format!("https://{trimmed}"))
}

/// Whether the value already carries a `scheme:` prefix, as opposed to being
/// a bare host or path. Deliberately conservative: a scheme is letters,
/// digits, `+`, `-`, `.` before the first `:`, so `localhost:3000` is treated
/// as a host and port rather than a scheme.
fn has_scheme(lowered: &str) -> bool {
    let Some(colon) = lowered.find(':') else {
        return false;
    };
    let (prefix, rest) = lowered.split_at(colon);
    if prefix.is_empty() || !prefix.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }
    // `host:3000` is a port, not a scheme.
    !rest[1..].starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_urls_pass_through_unchanged() {
        assert_eq!(
            normalize("https://example.com/a?b=c").as_deref(),
            Ok("https://example.com/a?b=c")
        );
        assert_eq!(normalize("about:blank").as_deref(), Ok("about:blank"));
        assert_eq!(
            normalize("  http://example.com  ").as_deref(),
            Ok("http://example.com")
        );
    }

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(
            normalize("example.com").as_deref(),
            Ok("https://example.com")
        );
        assert_eq!(
            normalize("example.com/path").as_deref(),
            Ok("https://example.com/path")
        );
        // A port is not a scheme.
        assert_eq!(
            normalize("localhost:3000").as_deref(),
            Ok("https://localhost:3000")
        );
    }

    #[test]
    fn flag_shaped_urls_are_refused() {
        // The whole point: this would reach the CLI as an argument.
        assert!(normalize("--version").is_err());
        assert!(normalize("--session").is_err());
        assert!(normalize("").is_err());
        assert!(normalize("   ").is_err());
        assert!(normalize("https://example.com /etc").is_err());
    }

    #[test]
    fn unsupported_schemes_are_refused() {
        assert!(normalize("javascript:alert(1)").is_err());
        assert!(normalize("data:text/html,hi").is_err());
        assert!(normalize("ftp://example.com").is_err());
    }
}
