//! fork: preserve URLs in `uv.lock` across re-locks when the index URL changes.
//!
//! Upstream `uv lock` bakes the currently-resolved index URL — and file URLs
//! derived from it — into `uv.lock` via [`Source::Registry`], [`SourceDist::Url`],
//! and [`WheelWireSource::Url`]. When `UV_DEFAULT_INDEX` points at an internal
//! mirror that differs across environments, re-running `uv lock` rewrites these
//! URLs, causing noisy diffs and breaking portability.
//!
//! This module adds [`Lock::rewrite_urls_from`], which copies URL fields from a
//! previous lockfile onto the newly-resolved lock when the package is still
//! present at the same (name, version). Hashes and versions continue to be
//! written as resolved; only URL fields are held stable.
//!
//! Implementation notes: [`PackageId`] appears in two places inside the lock —
//! on each [`Package`] via `Package.id`, and on every [`Dependency`] via
//! `Dependency.package_id`. `PackageId` implements `Hash + Eq` over its `source`
//! field, so mutating the registry URL on `Package.id` without mutating the
//! matching `Dependency.package_id` breaks any `HashMap<&PackageId, _>` lookup
//! downstream (e.g. the dependency-graph build in `installable.rs`). To keep
//! the lock internally consistent we apply the URL substitution to every
//! `PackageId` occurrence.
//!
//! Non-registry sources (git, direct URL, path, directory, editable, virtual)
//! are left untouched.

use rustc_hash::FxHashMap;

use uv_distribution_types::UrlString;
use uv_normalize::PackageName;
use uv_pep440::Version;

use super::{Lock, Package, PackageId, RegistrySource, Source, SourceDist, Wheel, WheelWireSource};

impl Lock {
    /// Preserve URLs from a previous lockfile for packages whose (name, version)
    /// are unchanged. See module-level docs for the matching rules.
    pub fn rewrite_urls_from(&mut self, previous: &Self) {
        // Build a map of (name, version) → preserved registry URL from the
        // previous lock. These are the URLs we want every PackageId with the
        // same (name, version) to use after rewriting.
        let preserved_registry: FxHashMap<(&PackageName, Option<&Version>), &UrlString> = previous
            .packages
            .iter()
            .filter_map(|package| {
                let Source::Registry(RegistrySource::Url(url)) = &package.id.source else {
                    return None;
                };
                Some(((&package.id.name, package.id.version.as_ref()), url))
            })
            .collect();

        // Apply the registry URL mapping to every PackageId in the lock. This
        // covers both `Package.id` and every `Dependency.package_id` nested
        // inside a package's dependencies, optional dependencies, and
        // dependency groups — all must stay in sync so downstream HashMap
        // lookups (e.g. in `installable.rs`) still work.
        for package in &mut self.packages {
            apply_preserved_registry(&mut package.id, &preserved_registry);
            for dep in &mut package.dependencies {
                apply_preserved_registry(&mut dep.package_id, &preserved_registry);
            }
            for deps in package.optional_dependencies.values_mut() {
                for dep in deps {
                    apply_preserved_registry(&mut dep.package_id, &preserved_registry);
                }
            }
            for deps in package.dependency_groups.values_mut() {
                for dep in deps {
                    apply_preserved_registry(&mut dep.package_id, &preserved_registry);
                }
            }
        }

        // Preserve sdist / wheel URLs per-package.
        let previous_by_key: FxHashMap<(&PackageName, Option<&Version>), &Package> = previous
            .packages
            .iter()
            .map(|package| ((&package.id.name, package.id.version.as_ref()), package))
            .collect();
        for new_package in &mut self.packages {
            let key = (&new_package.id.name, new_package.id.version.as_ref());
            let Some(previous_package) = previous_by_key.get(&key).copied() else {
                continue;
            };
            copy_sdist_url(previous_package.sdist.as_ref(), new_package.sdist.as_mut());
            copy_wheel_urls(&previous_package.wheels, &mut new_package.wheels);
        }
    }
}

fn apply_preserved_registry(
    id: &mut PackageId,
    preserved: &FxHashMap<(&PackageName, Option<&Version>), &UrlString>,
) {
    let Source::Registry(RegistrySource::Url(url)) = &mut id.source else {
        return;
    };
    let key = (&id.name, id.version.as_ref());
    if let Some(preserved_url) = preserved.get(&key) {
        *url = (*preserved_url).clone();
    }
}

fn copy_sdist_url(previous: Option<&SourceDist>, new: Option<&mut SourceDist>) {
    let (Some(previous), Some(new)) = (previous, new) else {
        return;
    };
    if let (
        SourceDist::Url {
            url: previous_url, ..
        },
        SourceDist::Url { url: new_url, .. },
    ) = (previous, new)
    {
        *new_url = previous_url.clone();
    }
}

fn copy_wheel_urls(previous: &[Wheel], new: &mut [Wheel]) {
    for new_wheel in new {
        let matching_url = previous.iter().find_map(|previous_wheel| {
            if previous_wheel.filename != new_wheel.filename {
                return None;
            }
            if let WheelWireSource::Url { url } = &previous_wheel.url {
                Some(url.clone())
            } else {
                None
            }
        });
        let Some(matching_url) = matching_url else {
            continue;
        };
        if let WheelWireSource::Url { url: new_url } = &mut new_wheel.url {
            *new_url = matching_url;
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

    /// Builds a two-package lockfile (`anyio` depending on `iniconfig`) at the
    /// given registry/file prefix. Both packages use the same registry URL.
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
    fn preserves_urls_on_mirror_change() {
        let previous = make_lock(
            "https://pypi.org/simple",
            "https://files.pythonhosted.org/packages/iniconfig",
        );
        let mut new = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        new.rewrite_urls_from(&previous);

        let rendered = new.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://pypi.org/simple""#),
            "registry URL should be preserved from previous lock:\n{rendered}"
        );
        assert!(
            rendered.contains(
                "https://files.pythonhosted.org/packages/iniconfig/iniconfig-2.0.0.tar.gz"
            ),
            "sdist URL should be preserved:\n{rendered}"
        );
        assert!(
            rendered.contains(
                "https://files.pythonhosted.org/packages/iniconfig/iniconfig-2.0.0-py3-none-any.whl"
            ),
            "wheel URL should be preserved:\n{rendered}"
        );
        assert!(
            !rendered.contains("mirror.example.com"),
            "no mirror URLs should leak into the rewritten lock:\n{rendered}"
        );
    }

    #[test]
    fn rewrites_dependency_package_ids() {
        // Two-package lock with a dependency edge — guards against the panic
        // in installable.rs:508 where `inverse[&package.id]` fails when
        // `Package.id` and `Dependency.package_id` drift out of sync.
        let previous = make_lock_with_dependency(
            "https://pypi.org/simple",
            "https://files.pythonhosted.org/packages",
        );
        let mut new = make_lock_with_dependency(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files",
        );

        new.rewrite_urls_from(&previous);

        // Every `Dependency.package_id.source` must be updated in lockstep with
        // `Package.id.source`; otherwise downstream HashMap lookups break. We
        // verify this structurally: walk every dependency's PackageId and make
        // sure no mirror URL survives.
        for package in &new.packages {
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

        // The re-serialized new lock must be byte-identical to the previous one
        // (same resolution, URLs all preserved) — this is the invariant the
        // skip-commit optimization relies on.
        let new_rendered = new.to_toml().expect("serialize new lock");
        let previous_rendered = previous.to_toml().expect("serialize previous lock");
        assert_eq!(
            new_rendered, previous_rendered,
            "rewritten lock should match previous lock byte-for-byte\n\
             --- new ---\n{new_rendered}\n--- previous ---\n{previous_rendered}"
        );
    }

    #[test]
    fn refreshes_urls_on_version_bump() {
        let previous = make_lock(
            "https://pypi.org/simple",
            "https://files.pythonhosted.org/packages/iniconfig",
        );
        let data = r#"
version = 1
requires-python = ">=3.12"

[[package]]
name = "iniconfig"
version = "2.1.0"
source = { registry = "https://mirror.example.com/simple" }
sdist = { url = "https://mirror.example.com/files/iniconfig/iniconfig-2.1.0.tar.gz", hash = "sha256:0000000000000000000000000000000000000000000000000000000000000001", size = 4646 }
wheels = [{ url = "https://mirror.example.com/files/iniconfig/iniconfig-2.1.0-py3-none-any.whl", hash = "sha256:0000000000000000000000000000000000000000000000000000000000000002", size = 5892 }]
"#;
        let mut new: Lock = toml::from_str(data).expect("parse lock");

        new.rewrite_urls_from(&previous);

        let rendered = new.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://mirror.example.com/simple""#),
            "registry URL should not be preserved across version bump:\n{rendered}"
        );
    }

    #[test]
    fn preserves_urls_but_keeps_new_hash_when_hash_differs() {
        let previous = make_lock(
            "https://pypi.org/simple",
            "https://files.pythonhosted.org/packages/iniconfig",
        );
        let data = r#"
version = 1
requires-python = ">=3.12"

[[package]]
name = "iniconfig"
version = "2.0.0"
source = { registry = "https://mirror.example.com/simple" }
sdist = { url = "https://mirror.example.com/files/iniconfig/iniconfig-2.0.0.tar.gz", hash = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", size = 4646 }
wheels = [{ url = "https://mirror.example.com/files/iniconfig/iniconfig-2.0.0-py3-none-any.whl", hash = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", size = 5892 }]
"#;
        let mut new: Lock = toml::from_str(data).expect("parse lock");

        new.rewrite_urls_from(&previous);

        let rendered = new.to_toml().expect("serialize lock");
        assert!(
            !rendered.contains("mirror.example.com"),
            "mirror URLs should not leak:\n{rendered}"
        );
        assert!(
            rendered.contains("aaaaaaaaaaaa"),
            "new sdist hash should be kept:\n{rendered}"
        );
        assert!(
            rendered.contains("bbbbbbbbbbbb"),
            "new wheel hash should be kept:\n{rendered}"
        );
    }

    #[test]
    fn no_previous_match_leaves_urls_untouched() {
        let previous_data = r#"
version = 1
requires-python = ">=3.12"
"#;
        let previous: Lock = toml::from_str(previous_data).expect("parse lock");
        let mut new = make_lock(
            "https://mirror.example.com/simple",
            "https://mirror.example.com/files/iniconfig",
        );

        new.rewrite_urls_from(&previous);

        let rendered = new.to_toml().expect("serialize lock");
        assert!(
            rendered.contains(r#"registry = "https://mirror.example.com/simple""#),
            "registry URL should be untouched when previous has no match:\n{rendered}"
        );
    }
}
