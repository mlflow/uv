//! fork: preserve URLs in `uv.lock` across re-locks when the index URL changes.
//!
//! Upstream `uv lock` bakes the currently-resolved index URL — and file URLs
//! derived from it — into `uv.lock` via [`Source::Registry`], [`SourceDist::Url`],
//! and [`WheelWireSource::Url`]. When `UV_DEFAULT_INDEX` points at an internal
//! mirror that differs across environments (developer machine vs. CI, or across
//! regions), re-running `uv lock` rewrites these URLs, causing noisy diffs in
//! committed lockfiles and breaking portability.
//!
//! This module adds [`Lock::rewrite_urls_from`], which always copies URL fields
//! from a previous lockfile onto the newly-resolved lock when the package is
//! still present at the same (name, version). Hash and version changes are
//! written as-is by the resolver; only the URL fields are normalized back to
//! the previous lock's values.
//!
//! - `source.registry` is copied when the package name+version match and both
//!   sides are [`Source::Registry`] with a URL.
//! - `sdist.url` is copied when the package name+version match and both sides
//!   use [`SourceDist::Url`].
//! - Each `wheels[].url` is copied when a previous wheel with the same filename
//!   exists and uses [`WheelWireSource::Url`].
//!
//! A version change yields a (name, version) mismatch, so upgraded packages
//! naturally pick up fresh URLs. Non-registry sources (git, direct URL, path,
//! directory, editable, virtual) are left untouched.

use rustc_hash::FxHashMap;

use uv_normalize::PackageName;
use uv_pep440::Version;

use super::{Lock, Package, RegistrySource, Source, SourceDist, Wheel, WheelWireSource};

impl Lock {
    /// Preserve URLs from a previous lockfile for packages whose (name, version)
    /// are unchanged. See module-level docs for the matching rules.
    pub fn rewrite_urls_from(&mut self, previous: &Self) {
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

            copy_registry_url(&previous_package.id.source, &mut new_package.id.source);
            copy_sdist_url(previous_package.sdist.as_ref(), new_package.sdist.as_mut());
            copy_wheel_urls(&previous_package.wheels, &mut new_package.wheels);
        }
    }
}

fn copy_registry_url(previous: &Source, new: &mut Source) {
    if let (
        Source::Registry(RegistrySource::Url(previous_url)),
        Source::Registry(RegistrySource::Url(new_url)),
    ) = (previous, new)
    {
        *new_url = previous_url.clone();
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

    /// Parses a minimal TOML lockfile for a single iniconfig package, with the
    /// given registry URL and file URL prefix. Returns the parsed [`Lock`].
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
    fn refreshes_urls_on_version_bump() {
        let previous = make_lock(
            "https://pypi.org/simple",
            "https://files.pythonhosted.org/packages/iniconfig",
        );
        // new lock is at 2.1.0 — (name, version) key differs → no preservation.
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
        // Same (name, version) but different hashes — URLs preserved, hashes kept as-is.
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
        // The new hashes are preserved from the new resolution.
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
