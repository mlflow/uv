//! fork: rewrite proxy registry URLs in `uv.lock` to their canonical counterparts.
//!
//! When `UV_DEFAULT_INDEX` points at an internal PyPI mirror/proxy, upstream
//! `uv lock` bakes the proxy URL into every `source.registry` field in
//! `uv.lock`.  This creates noisy diffs and breaks portability across
//! environments that use different mirrors.
//!
//! The `UV_INDEX_PROXIES` environment variable provides a mapping from
//! canonical URLs to proxy URLs.  After resolution, [`Lock::rewrite_proxy_urls`]
//! replaces every proxy registry URL with its canonical counterpart in the
//! lockfile, keeping it stable regardless of which mirror resolved the package.
//!
//! Format: `<canonical>:<proxy>,<canonical2>:<proxy2>`
//!
//! Example:
//!   UV_INDEX_PROXIES=https://pypi.org/simple:https://pypi-proxy.example.com/simple

use std::collections::BTreeSet;

use tracing::{debug, trace};
use uv_distribution_types::UrlString;
use uv_redacted::SafeUrl;

use super::{Lock, PackageId, PackageIndex, RegistrySource, Source};

/// A single canonical ↔ proxy URL mapping.
struct ProxyMapping {
    canonical: UrlString,
    proxy: UrlString,
}

/// Parse `UV_INDEX_PROXIES` into a list of mappings.
///
/// Format: `canonical:proxy,canonical2:proxy2`
fn parse_proxy_mappings() -> Vec<ProxyMapping> {
    let Ok(value) = std::env::var("UV_INDEX_PROXIES") else {
        return Vec::new();
    };
    parse_mappings(&value)
}

/// Parse a raw `UV_INDEX_PROXIES` value into a list of mappings.
///
/// Entries that are empty or that do not parse as valid URLs are skipped.
fn parse_mappings(value: &str) -> Vec<ProxyMapping> {
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
            Some(ProxyMapping {
                canonical: parse_url(entry[..delimiter_pos].trim())?,
                proxy: parse_url(entry[delimiter_pos + 1..].trim())?,
            })
        })
        .collect()
}

/// Parse a single URL into a [`UrlString`], returning `None` if it is not valid.
fn parse_url(url: &str) -> Option<UrlString> {
    SafeUrl::parse(url).ok().map(|url| UrlString::from(&url))
}

/// Add canonical URLs from `UV_INDEX_PROXIES` to the set of known remote
/// indexes so that `satisfies()` recognizes lockfile entries written with
/// canonical URLs as valid.
pub(super) fn canonical_urls(remotes: &mut BTreeSet<UrlString>) {
    for mapping in parse_proxy_mappings() {
        remotes.insert(mapping.canonical);
    }
}

/// Resolve a canonical registry URL back to its proxy URL using
/// `UV_INDEX_PROXIES`.  This is the reverse of [`Lock::rewrite_proxy_urls`]:
/// the lockfile stores canonical URLs, but at install time we need to fetch
/// from the proxy that is actually reachable.
///
/// Returns the original URL unchanged if no mapping matches.
pub(super) fn proxy_url(url: &UrlString) -> UrlString {
    resolve_proxy_url(url, &parse_proxy_mappings())
}

/// Resolve a canonical registry URL back to its proxy URL using `mappings`.
fn resolve_proxy_url(url: &UrlString, mappings: &[ProxyMapping]) -> UrlString {
    for mapping in mappings {
        if *url == mapping.canonical {
            trace!(
                "Resolving canonical registry URL `{url}` to proxy `{}`",
                mapping.proxy
            );
            return mapping.proxy.clone();
        }
    }
    url.clone()
}

impl Lock {
    /// Rewrite proxy registry URLs to their canonical counterparts based on
    /// the `UV_INDEX_PROXIES` environment variable.
    pub fn rewrite_proxy_urls(&mut self) {
        self.rewrite_proxy_urls_with(&parse_proxy_mappings());
    }

    /// Rewrite proxy registry URLs to their canonical counterparts using `mappings`.
    fn rewrite_proxy_urls_with(&mut self, mappings: &[ProxyMapping]) {
        if mappings.is_empty() {
            return;
        }

        for mapping in mappings {
            debug!(
                "Rewriting proxy registry URLs: `{}` → `{}`",
                mapping.proxy, mapping.canonical
            );
        }

        for package in &mut self.packages {
            apply_proxy_mapping(&mut package.id, mappings);
            for dep in &mut package.dependencies {
                apply_proxy_mapping(&mut dep.package_id, mappings);
            }
            for deps in package.optional_dependencies.values_mut() {
                for dep in deps {
                    apply_proxy_mapping(&mut dep.package_id, mappings);
                }
            }
            for deps in package.dependency_groups.values_mut() {
                for dep in deps {
                    apply_proxy_mapping(&mut dep.package_id, mappings);
                }
            }
        }

        // Rebuild `by_id` since we mutated `Package.id.source`.
        self.by_id.clear();
        for (index, package) in self.packages.iter().enumerate() {
            self.by_id.insert(package.id.clone(), PackageIndex(index));
        }
    }
}

/// Replace proxy URL with its canonical counterpart on a [`PackageId`].
fn apply_proxy_mapping(id: &mut PackageId, mappings: &[ProxyMapping]) {
    let Source::Registry(RegistrySource::Url(url)) = &mut id.source else {
        return;
    };
    for mapping in mappings {
        if *url == mapping.proxy {
            trace!(
                "Rewriting proxy registry URL `{url}` to canonical `{}`",
                mapping.canonical
            );
            *url = mapping.canonical.clone();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Lock;
    use super::{parse_mappings, parse_url, resolve_proxy_url};

    /// A mapping from the canonical PyPI URL to a proxy mirror.
    const MAPPING: &str = "https://pypi.org/simple:https://mirror.example.com/simple";

    fn make_lock(registry: &str, file_prefix: &str) -> Lock {
        let data = format!(
            r#"
version = 1
requires-python = ">=3.12"

[[package]]
name = "iniconfig"
version = "2.0.0"
source = {{ registry = "{registry}" }}
sdist = {{ url = "{file_prefix}/iniconfig-2.0.0.tar.gz", hash = "sha256:2d91e135bf72d31a410b17c16da610a82cb55f6b0477d1a902134b24a455b8b3", size = 4646 }}
wheels = [{{ url = "{file_prefix}/iniconfig-2.0.0-py3-none-any.whl", hash = "sha256:b6a85871a79d2e3b22d2d1b94ac2824226a63c6b741c88f7ae975f18b6778374", size = 5892 }}]
"#
        );
        toml::from_str(&data).expect("parse lock")
    }

    fn make_lock_with_dependency(registry: &str, file_prefix: &str) -> Lock {
        let data = format!(
            r#"
version = 1
requires-python = ">=3.12"

[[package]]
name = "anyio"
version = "4.3.0"
source = {{ registry = "{registry}" }}
sdist = {{ url = "{file_prefix}/anyio-4.3.0.tar.gz", hash = "sha256:f75253795a87df48568485fd18cdd2a3fa5c4f7c5be8e5e36637733fce06fed6", size = 159642 }}
wheels = [{{ url = "{file_prefix}/anyio-4.3.0-py3-none-any.whl", hash = "sha256:048e05d0f6caeed70d731f3db756d35dcc1f35747c8c403364a8332c630441b8", size = 85584 }}]

[[package.dependencies]]
name = "iniconfig"
version = "2.0.0"
source = {{ registry = "{registry}" }}

[[package]]
name = "iniconfig"
version = "2.0.0"
source = {{ registry = "{registry}" }}
sdist = {{ url = "{file_prefix}/iniconfig-2.0.0.tar.gz", hash = "sha256:2d91e135bf72d31a410b17c16da610a82cb55f6b0477d1a902134b24a455b8b3", size = 4646 }}
wheels = [{{ url = "{file_prefix}/iniconfig-2.0.0-py3-none-any.whl", hash = "sha256:b6a85871a79d2e3b22d2d1b94ac2824226a63c6b741c88f7ae975f18b6778374", size = 5892 }}]
"#
        );
        toml::from_str(&data).expect("parse lock")
    }

    #[test]
    fn rewrites_proxy_to_canonical() {
        let mut lock = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        lock.rewrite_proxy_urls_with(&parse_mappings(MAPPING));

        let rendered = lock.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://pypi.org/simple""#),
            "registry URL should be rewritten to canonical:\n{rendered}"
        );
        // File URLs are not rewritten — only registry sources.
        assert!(
            rendered.contains("https://mirror.example.com/files/iniconfig"),
            "file URLs should be left as-is:\n{rendered}"
        );
    }

    #[test]
    fn rewrites_dependency_package_ids() {
        let mut lock = make_lock_with_dependency(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files",
        );

        lock.rewrite_proxy_urls_with(&parse_mappings(MAPPING));

        for package in &lock.packages {
            for dep in &package.dependencies {
                if let super::Source::Registry(super::RegistrySource::Url(url)) =
                    &dep.package_id.source
                {
                    assert!(
                        !url.to_string().contains("mirror.example.com"),
                        "dependency {} still references mirror URL {url:?}",
                        dep.package_id.name
                    );
                }
            }
        }

        // `by_id` lookups must still resolve after the rewrite, since the
        // rewritten `PackageId`s are the map's keys.
        for package in &lock.packages {
            for dep in &package.dependencies {
                let resolved = lock.package(lock.by_id[&dep.package_id]);
                assert_eq!(resolved.id.name, dep.package_id.name);
            }
        }
    }

    #[test]
    fn no_mappings_is_noop() {
        let mut lock = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        lock.rewrite_proxy_urls_with(&parse_mappings(""));

        let rendered = lock.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://mirror.example.com/simple""#),
            "registry URL should be unchanged without mappings:\n{rendered}"
        );
    }

    #[test]
    fn no_match_leaves_urls_untouched() {
        let mut lock = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        lock.rewrite_proxy_urls_with(&parse_mappings(
            "https://pypi.org/simple:https://other-proxy.example.com/simple",
        ));

        let rendered = lock.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://mirror.example.com/simple""#),
            "non-matching registry URL should be unchanged:\n{rendered}"
        );
    }

    #[test]
    fn proxy_url_resolves_canonical_to_proxy() {
        let mappings = parse_mappings(MAPPING);

        let canonical = parse_url("https://pypi.org/simple").expect("valid URL");
        let resolved = resolve_proxy_url(&canonical, &mappings);
        assert_eq!(
            resolved.to_string(),
            "https://mirror.example.com/simple",
            "canonical URL should resolve to proxy URL"
        );

        // Non-matching URL should be returned unchanged.
        let other = parse_url("https://other.example.com/simple").expect("valid URL");
        let resolved = resolve_proxy_url(&other, &mappings);
        assert_eq!(
            resolved.to_string(),
            "https://other.example.com/simple",
            "non-matching URL should be returned unchanged"
        );
    }

    #[test]
    fn proxy_url_noop_without_mappings() {
        let url = parse_url("https://pypi.org/simple").expect("valid URL");
        let resolved = resolve_proxy_url(&url, &parse_mappings(""));
        assert_eq!(
            resolved.to_string(),
            "https://pypi.org/simple",
            "URL should be unchanged without mappings"
        );
    }
}
