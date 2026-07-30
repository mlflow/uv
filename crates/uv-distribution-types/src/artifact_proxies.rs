//! fork: rewrite proxy artifact URLs to their canonical counterparts.
//!
//! A proxy index serves the Simple API from its own host, and the artifact
//! URLs it advertises point back at that same host (e.g.
//! `https://pypi-proxy.example.com/packages/...`). Those URLs are baked into
//! `uv.lock`, which makes the lockfile non-portable across environments that
//! use different mirrors, and forces every download through the proxy.
//!
//! The `UV_ARTIFACT_PROXIES` environment variable provides a mapping from
//! canonical artifact base URLs to proxy base URLs. Matching URLs are rewritten
//! to the canonical base as soon as they are parsed out of an index response,
//! so both resolution downloads and the URLs recorded in `uv.lock` use the
//! canonical host.
//!
//! Unlike [`crate::IndexUrl`] mappings, these are *prefix* mappings: every
//! artifact has a distinct path below the base.
//!
//! Format: `<canonical>:<proxy>,<canonical2>:<proxy2>`
//!
//! Example:
//!   UV_ARTIFACT_PROXIES=https://files.pythonhosted.org/packages:https://pypi-proxy.example.com/packages

use std::sync::LazyLock;

use tracing::trace;
use uv_small_str::SmallString;

/// A single canonical ↔ proxy artifact base URL mapping.
struct ArtifactProxy {
    canonical: String,
    proxy: String,
}

/// The mappings configured via `UV_ARTIFACT_PROXIES`.
static ARTIFACT_PROXIES: LazyLock<Vec<ArtifactProxy>> = LazyLock::new(|| {
    let Ok(value) = std::env::var("UV_ARTIFACT_PROXIES") else {
        return Vec::new();
    };
    parse_mappings(&value)
});

/// Parse a raw `UV_ARTIFACT_PROXIES` value into a list of mappings.
///
/// Entries that are empty or malformed are skipped.
fn parse_mappings(value: &str) -> Vec<ArtifactProxy> {
    value
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            // Split on `:https://` or `:http://` to avoid splitting the scheme.
            let delimiter_pos = entry
                .find(":https://")
                .or_else(|| entry.find(":http://"))
                .filter(|&pos| pos > 0)?;
            let canonical = entry[..delimiter_pos].trim().trim_end_matches('/');
            let proxy = entry[delimiter_pos + 1..].trim().trim_end_matches('/');
            if canonical.is_empty() || proxy.is_empty() {
                return None;
            }
            Some(ArtifactProxy {
                canonical: canonical.to_string(),
                proxy: proxy.to_string(),
            })
        })
        .collect()
}

/// Rewrite an artifact URL served by a proxy to its canonical equivalent.
///
/// Returns the URL unchanged if no mapping applies.
pub(crate) fn canonicalize(url: SmallString) -> SmallString {
    rewrite(url, &ARTIFACT_PROXIES)
}

/// Rewrite `url` using `mappings`, returning it unchanged if no prefix matches.
fn rewrite(url: SmallString, mappings: &[ArtifactProxy]) -> SmallString {
    for mapping in mappings {
        // Only rewrite at a path boundary, so that a proxy base of
        // `https://example.com/packages` does not match
        // `https://example.com/packages-other/...`.
        let Some(rest) = url.strip_prefix(&mapping.proxy) else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            continue;
        }
        let canonical = SmallString::from(format!("{}{rest}", mapping.canonical).as_str());
        trace!("Rewriting proxy artifact URL `{url}` to canonical `{canonical}`");
        return canonical;
    }
    url
}

#[cfg(test)]
mod tests {
    use super::{parse_mappings, rewrite};
    use uv_small_str::SmallString;

    const MAPPING: &str =
        "https://files.pythonhosted.org/packages:https://pypi-proxy.example.com/packages";

    fn rewritten(url: &str, value: &str) -> String {
        rewrite(SmallString::from(url), &parse_mappings(value)).to_string()
    }

    #[test]
    fn rewrites_proxy_artifact_to_canonical() {
        assert_eq!(
            rewritten(
                "https://pypi-proxy.example.com/packages/e1/04/abc/foo-1.0-py3-none-any.whl",
                MAPPING,
            ),
            "https://files.pythonhosted.org/packages/e1/04/abc/foo-1.0-py3-none-any.whl",
        );
    }

    #[test]
    fn rewrites_metadata_sidecar() {
        assert_eq!(
            rewritten(
                "https://pypi-proxy.example.com/packages/e1/04/abc/foo-1.0-py3-none-any.whl.metadata",
                MAPPING,
            ),
            "https://files.pythonhosted.org/packages/e1/04/abc/foo-1.0-py3-none-any.whl.metadata",
        );
    }

    #[test]
    fn leaves_other_hosts_untouched() {
        let url = "https://other.example.com/packages/foo-1.0-py3-none-any.whl";
        assert_eq!(rewritten(url, MAPPING), url);
    }

    #[test]
    fn does_not_match_partial_path_segment() {
        // `/packages-internal` must not match a `/packages` base.
        let url = "https://pypi-proxy.example.com/packages-internal/foo-1.0-py3-none-any.whl";
        assert_eq!(rewritten(url, MAPPING), url);
    }

    #[test]
    fn no_mappings_is_noop() {
        let url = "https://pypi-proxy.example.com/packages/foo-1.0-py3-none-any.whl";
        assert_eq!(rewritten(url, ""), url);
    }

    #[test]
    fn trailing_slash_in_config_is_normalized() {
        assert_eq!(
            rewritten(
                "https://pypi-proxy.example.com/packages/foo-1.0-py3-none-any.whl",
                "https://files.pythonhosted.org/packages/:https://pypi-proxy.example.com/packages/",
            ),
            "https://files.pythonhosted.org/packages/foo-1.0-py3-none-any.whl",
        );
    }
}
