//! fork: rewrite proxy registry URLs in `uv.lock` to their canonical counterparts.
//!
//! When `UV_DEFAULT_INDEX` points at an internal PyPI mirror/proxy, upstream
//! `uv lock` bakes the proxy URL into every `source.registry` field in
//! `uv.lock`.  This creates noisy diffs and breaks portability across
//! environments that use different mirrors.
//!
//! The `UV_PYPI_PROXIES` environment variable provides a mapping from
//! canonical URLs to proxy URLs.  After resolution, [`Lock::rewrite_proxy_urls`]
//! replaces every proxy registry URL with its canonical counterpart in the
//! lockfile, keeping it stable regardless of which mirror resolved the package.
//!
//! Format: `<canonical>:<proxy>,<canonical2>:<proxy2>`
//!
//! Example:
//!   UV_PYPI_PROXIES=https://pypi.org/simple:https://pypi-proxy.dev.databricks.com/simple

use std::collections::BTreeSet;

use uv_distribution_types::UrlString;

use super::{Lock, PackageId, RegistrySource, Source};

/// A single canonical ↔ proxy URL mapping.
struct ProxyMapping {
    canonical: UrlString,
    proxy: UrlString,
}

/// Parse `UV_PYPI_PROXIES` into a list of mappings.
///
/// Format: `canonical:proxy,canonical2:proxy2`
fn parse_proxy_mappings() -> Vec<ProxyMapping> {
    let Some(value) = std::env::var("UV_PYPI_PROXIES").ok() else {
        return Vec::new();
    };
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
            let canonical = entry[..delimiter_pos].trim();
            let proxy = entry[delimiter_pos + 1..].trim();
            if canonical.is_empty() || proxy.is_empty() {
                return None;
            }
            Some(ProxyMapping {
                canonical: UrlString::from(canonical),
                proxy: UrlString::from(proxy),
            })
        })
        .collect()
}

/// Add canonical URLs from `UV_PYPI_PROXIES` to the set of known remote
/// indexes so that `satisfies()` recognizes lockfile entries written with
/// canonical URLs as valid.
pub(super) fn canonical_urls(remotes: &mut BTreeSet<UrlString>) {
    for mapping in parse_proxy_mappings() {
        remotes.insert(mapping.canonical);
    }
}

impl Lock {
    /// Rewrite proxy registry URLs to their canonical counterparts based on
    /// the `UV_PYPI_PROXIES` environment variable.
    pub fn rewrite_proxy_urls(&mut self) {
        let mappings = parse_proxy_mappings();
        if mappings.is_empty() {
            return;
        }

        for package in &mut self.packages {
            apply_proxy_mapping(&mut package.id, &mappings);
            for dep in &mut package.dependencies {
                apply_proxy_mapping(&mut dep.package_id, &mappings);
            }
            for deps in package.optional_dependencies.values_mut() {
                for dep in deps {
                    apply_proxy_mapping(&mut dep.package_id, &mappings);
                }
            }
            for deps in package.dependency_groups.values_mut() {
                for dep in deps {
                    apply_proxy_mapping(&mut dep.package_id, &mappings);
                }
            }
        }

        // Rebuild `by_id` since we mutated `Package.id.source`.
        self.by_id.clear();
        for (index, package) in self.packages.iter().enumerate() {
            self.by_id.insert(package.id.clone(), index);
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
            *url = mapping.canonical.clone();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Lock;

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
        std::env::set_var(
            "UV_PYPI_PROXIES",
            "https://pypi.org/simple:https://mirror.example.com/simple",
        );
        let mut lock = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        lock.rewrite_proxy_urls();

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
        std::env::remove_var("UV_PYPI_PROXIES");
    }

    #[test]
    fn rewrites_dependency_package_ids() {
        std::env::set_var(
            "UV_PYPI_PROXIES",
            "https://pypi.org/simple:https://mirror.example.com/simple",
        );
        let mut lock = make_lock_with_dependency(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files",
        );

        lock.rewrite_proxy_urls();

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

        // find_by_id must work after rewrite (by_id was rebuilt).
        for package in &lock.packages {
            for dep in &package.dependencies {
                let resolved = lock.find_by_id(&dep.package_id);
                assert_eq!(resolved.id.name, dep.package_id.name);
            }
        }
        std::env::remove_var("UV_PYPI_PROXIES");
    }

    #[test]
    fn no_env_var_is_noop() {
        std::env::remove_var("UV_PYPI_PROXIES");
        let mut lock = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        lock.rewrite_proxy_urls();

        let rendered = lock.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://mirror.example.com/simple""#),
            "registry URL should be unchanged without env var:\n{rendered}"
        );
    }

    #[test]
    fn no_match_leaves_urls_untouched() {
        std::env::set_var(
            "UV_PYPI_PROXIES",
            "https://pypi.org/simple:https://other-proxy.example.com/simple",
        );
        let mut lock = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        lock.rewrite_proxy_urls();

        let rendered = lock.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://mirror.example.com/simple""#),
            "non-matching registry URL should be unchanged:\n{rendered}"
        );
        std::env::remove_var("UV_PYPI_PROXIES");
    }
}
