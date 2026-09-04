//! Version pointer for builds that come from the public grok-build tree, not a
//! channel install.
//!
//! `needs_update` on the stable channel treats any pre-release (including the
//! `1.0.16-omarchy.<sha>` stamp) as stale, so a source build that is already
//! ahead of `https://x.ai/cli/stable` still got "v1.0.13 available". The TUI
//! nag for those binaries compares against the first CHANGELOG version on
//! `xai-org/grok-build` main instead, and never downloads a managed artifact.

use std::time::Duration;

use anyhow::Result;

use crate::auto_update::UpdateAvailable;
use crate::version::{get_installed_grok_version, is_loopback_base};

/// First heading in the OS-repo changelog is the published source version.
pub const OS_REPO_CHANGELOG_URL: &str = "https://raw.githubusercontent.com/xai-org/grok-build/main/crates/codegen/xai-grok-shell/CHANGELOG.md";

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// [`OS_REPO_CHANGELOG_URL`], unless tests set `GROK_OS_REPO_CHANGELOG_URL` to a
/// loopback URL. Non-loopback overrides are ignored, matching `GROK_CLI_BASE_URL`.
fn changelog_url() -> String {
    if let Ok(url) = std::env::var("GROK_OS_REPO_CHANGELOG_URL") {
        let url = url.trim();
        if is_loopback_base(url) {
            return url.to_owned();
        }
        if !url.is_empty() {
            tracing::warn!("GROK_OS_REPO_CHANGELOG_URL ignored: only loopback URLs are honored");
        }
    }
    OS_REPO_CHANGELOG_URL.to_string()
}

/// First `# X.Y.Z` heading in a grok-build CHANGELOG.md body.
pub fn parse_changelog_version(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("# ") else {
            continue;
        };
        let Some(ver) = rest.split_whitespace().next() else {
            continue;
        };
        if semver::Version::parse(ver).is_ok() {
            return Some(ver.to_string());
        }
    }
    None
}

/// True when the OS repo's release is newer than the running build's release.
///
/// `1.0.16-omarchy.<sha>` is the same release as `1.0.16`: the pre-release
/// suffix must not make a source build look stale against its own changelog.
/// Changelog versions are plain `X.Y.Z`; a pre-release pointer is rejected.
pub fn os_repo_is_newer(current: &str, os_repo: &str) -> bool {
    let Ok(current) = semver::Version::parse(current) else {
        return false;
    };
    let Ok(os_repo) = semver::Version::parse(os_repo) else {
        return false;
    };
    if !os_repo.pre.is_empty() {
        return false;
    }
    let current_release = semver::Version::new(current.major, current.minor, current.patch);
    os_repo > current_release
}

pub(crate) async fn fetch_os_repo_version_from(url: &str) -> Result<String> {
    let client = xai_grok_extra_ca::build_reqwest_client(|builder| {
        builder.timeout(FETCH_TIMEOUT).user_agent("grok-build")
    })?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "OS repo changelog fetch failed: HTTP {} for {}",
            resp.status(),
            url
        );
    }
    let body = resp.text().await?;
    parse_changelog_version(&body).ok_or_else(|| {
        anyhow::anyhow!("OS repo changelog at {url} has no '# X.Y.Z' version heading")
    })
}

/// Fetch the OS-repo changelog and return [`UpdateAvailable`] when it is ahead
/// of the running release. Fails open: any fetch or parse error is `None`.
///
/// Skips the check when `cli.auto_update` is explicitly `false`, matching the
/// managed-channel background path. Does not read or write `version.json`.
pub async fn check_os_repo_update() -> Option<UpdateAvailable> {
    let current_config = xai_grok_shell::util::config::load_config().await;
    if current_config.cli.auto_update == Some(false) {
        return None;
    }
    check_os_repo_update_from(&changelog_url(), &get_installed_grok_version()).await
}

/// Like [`check_os_repo_update`], but uses an explicit changelog URL and
/// running version. No config / auto_update gate.
pub async fn check_os_repo_update_from(url: &str, current: &str) -> Option<UpdateAvailable> {
    match fetch_os_repo_version_from(url).await {
        Ok(latest) if os_repo_is_newer(current, &latest) => {
            tracing::info!(
                %current,
                %latest,
                "OS repo has a newer grok-build release"
            );
            Some(UpdateAvailable {
                latest_version: latest,
            })
        }
        Ok(latest) => {
            tracing::debug!(
                %current,
                %latest,
                "running build is at or ahead of the OS repo"
            );
            None
        }
        Err(e) => {
            tracing::warn!("OS repo update check failed: {e:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_changelog_version_takes_the_first_semver_heading() {
        let body = "# Changelog\n\n# 1.0.16 — 2026-09-01\n\n## Features\n\n# 1.0.15 — 2026-08-31\n";
        assert_eq!(
            parse_changelog_version(body).as_deref(),
            Some("1.0.16"),
            "em-dash date headings"
        );
        assert_eq!(
            parse_changelog_version("# 1.0.17\n").as_deref(),
            Some("1.0.17")
        );
        assert_eq!(
            parse_changelog_version("# Changelog\n\nno versions\n"),
            None
        );
        assert_eq!(parse_changelog_version(""), None);
        assert_eq!(
            parse_changelog_version("# 1.0\n# 1.0.16\n").as_deref(),
            Some("1.0.16"),
            "partial versions are not semver"
        );
    }

    #[test]
    fn os_repo_is_newer_strips_omarchy_prerelease_and_rejects_downgrades() {
        let cases: &[(&str, &str, bool)] = &[
            // same release as the changelog we were built from
            ("1.0.16-omarchy.a2997ca6", "1.0.16", false),
            ("1.0.16", "1.0.16", false),
            // OS repo moved on
            ("1.0.16-omarchy.a2997ca6", "1.0.17", true),
            ("1.0.16", "1.0.17", true),
            // source build already ahead of the published changelog
            ("1.0.17-omarchy.deadbeef", "1.0.16", false),
            ("1.0.17", "1.0.16", false),
            // the original false nag: stable 1.0.13 vs omarchy 1.0.16
            ("1.0.16-omarchy.a2997ca6", "1.0.13", false),
            // garbage
            ("not-a-version", "1.0.17", false),
            ("1.0.16", "garbage", false),
            // changelog must be a release
            ("1.0.16", "1.0.17-alpha.1", false),
        ];
        for (current, os_repo, expected) in cases {
            assert_eq!(
                os_repo_is_newer(current, os_repo),
                *expected,
                "os_repo_is_newer({current:?}, {os_repo:?})"
            );
        }
    }

    #[tokio::test]
    async fn check_os_repo_update_from_nags_only_when_changelog_is_ahead() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/CHANGELOG.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("# Changelog\n\n# 1.0.17 — 2026-09-04\n"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/CHANGELOG.md", server.uri());

        let nag = check_os_repo_update_from(&url, "1.0.16-omarchy.a2997ca6")
            .await
            .expect("1.0.17 is ahead of 1.0.16-omarchy");
        assert_eq!(nag.latest_version, "1.0.17");

        assert!(
            check_os_repo_update_from(&url, "1.0.17-omarchy.deadbeef")
                .await
                .is_none(),
            "same release as the changelog must not nag"
        );
        assert!(
            check_os_repo_update_from(&url, "1.0.18-omarchy.ffff")
                .await
                .is_none(),
            "a newer source stamp must not nag to go backwards"
        );
    }

    #[tokio::test]
    async fn check_os_repo_update_from_fails_open_on_http_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(
            check_os_repo_update_from(&format!("{}/CHANGELOG.md", server.uri()), "1.0.16")
                .await
                .is_none()
        );
    }
}
