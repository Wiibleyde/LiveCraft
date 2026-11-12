//! Whether a newer LiveCraft is out. A few seconds after a real start, and at
//! most once a day, one request asks GitHub's public list of releases; nothing
//! else leaves the computer. It runs away from the window and fails in silence:
//! an operator in the middle of a show has better things to read.

use semver::Version;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// The running version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES: &str = "https://api.github.com/repos/Wiibleyde/LiveCraft/releases?per_page=10";
/// Release pages are built from the version, never taken from the answer, so
/// that nothing but this page can ever be opened from it.
const RELEASE_PAGE: &str = "https://github.com/Wiibleyde/LiveCraft/releases/tag/v";
/// Seconds between two checks at start.
const EVERY: i64 = 24 * 3600;
/// Long enough for a slow link; nothing waits on it anyway.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What the session keeps about updates.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Updates {
    /// Whether to ask at start. On, also for sessions saved before it existed.
    #[serde(default = "on")]
    pub check: bool,
    /// When a check last asked, in seconds since 1970.
    pub checked_at: Option<i64>,
    /// The newest version that check found, when newer than the one running then.
    pub found: Option<String>,
    /// The version whose notice was put away: it and older ones stay quiet.
    pub dismissed: String,
}

fn on() -> bool {
    true
}

impl Default for Updates {
    fn default() -> Self {
        Self { check: on(), checked_at: None, found: None, dismissed: String::new() }
    }
}

impl Updates {
    /// Whether the check at start should ask now. A clock put back counts as a
    /// new day, or a wrong date could silence the check for months.
    pub fn due(&self, now: i64) -> bool {
        self.check && self.checked_at.is_none_or(|at| now < at || now - at >= EVERY)
    }

    /// The version found, if it is newer than `current`, put away or not.
    pub fn newer(&self, current: &str) -> Option<Release> {
        let found = parse(self.found.as_deref()?)?;
        is_newer(&found, current).then(|| Release::of(&found))
    }

    /// The release the desk tells about, if any. Turning the check off also
    /// silences what an earlier check found.
    pub fn notice(&self, current: &str) -> Option<Release> {
        let found = parse(self.found.as_deref()?)?;
        let news = is_newer(&found, current) && is_newer(&found, &self.dismissed);
        (self.check && news).then(|| Release::of(&found))
    }

    /// Keeps what a check answered; an answer with nothing newer clears an
    /// older find, which the running version has caught up with.
    pub fn answered(&mut self, newest: Option<&Release>) {
        self.found = newest.map(|r| r.version.clone());
    }
}

/// A published version, and its page.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub page: String,
}

impl Release {
    fn of(v: &Version) -> Self {
        Self { version: v.to_string(), page: format!("{RELEASE_PAGE}{v}") }
    }
}

fn parse(version: &str) -> Option<Version> {
    Version::parse(version).ok()
}

/// By semantic version precedence: 0.1.0-alpha.1 < alpha.2 < beta.1 < rc.1 < 0.1.0.
/// Anything is newer than something that is not a version, such as nothing.
fn is_newer(v: &Version, than: &str) -> bool {
    parse(than).is_none_or(|t| v.cmp_precedence(&t) == Ordering::Greater)
}

/// One release as the API lists it; the many other fields are left aside.
#[derive(Deserialize)]
struct Listed {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The newest release this build should move to, from the API's answer. A test
/// build (a version with a `-` suffix) takes test releases as well as stable
/// ones; a stable build only stable ones. Drafts and tags that are not `v` and
/// a version are left out.
pub fn newest(answer: &str, current: &str) -> Result<Option<Release>, String> {
    let listed: Vec<Listed> = serde_json::from_str(answer).map_err(|e| e.to_string())?;
    let running = parse(current).ok_or_else(|| format!("{current}?"))?;
    let takes_tests = !running.pre.is_empty();
    let best = listed
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| Some((r.prerelease, parse(r.tag_name.strip_prefix('v')?)?)))
        .filter(|(marked, v)| takes_tests || (!marked && v.pre.is_empty()))
        .map(|(_, v)| v)
        .filter(|v| v.cmp_precedence(&running) == Ordering::Greater)
        .max_by(Version::cmp_precedence);
    Ok(best.as_ref().map(Release::of))
}

/// Asks GitHub; blocks for up to ten seconds, so never on the window's thread.
/// The request carries the program's name and version and nothing else: no
/// cookie, no query about the person.
pub fn fetch(current: &str) -> Result<Option<Release>, String> {
    use ureq::tls::{RootCerts, TlsConfig};
    // The system's own certificates, so that a company proxy that inspects
    // traffic with its own authority is trusted as the browser trusts it.
    let tls = TlsConfig::builder()
        .root_certs(RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(TIMEOUT))
        .user_agent(format!("LiveCraft/{current}"))
        .build()
        .into();
    let answer = agent
        .get(RELEASES)
        .header("Accept", "application/vnd.github+json")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| e.to_string())?;
    newest(&answer, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(answer: &str, current: &str) -> Option<String> {
        newest(answer, current).unwrap().map(|r| r.version)
    }

    /// Cut down from a real answer: every field the check does not read is ignored.
    const REAL: &str = r#"[
        {"url": "https://api.github.com/repos/Wiibleyde/LiveCraft/releases/1",
         "html_url": "https://github.com/Wiibleyde/LiveCraft/releases/tag/v0.1.0-alpha.2",
         "tag_name": "v0.1.0-alpha.2", "name": "LiveCraft 0.1.0-alpha.2", "draft": false,
         "prerelease": true, "assets": [{"name": "SHA256SUMS.txt", "size": 200}],
         "body": "Second alpha"},
        {"tag_name": "v0.1.0-alpha.1", "draft": false, "prerelease": true, "assets": []}
    ]"#;

    #[test]
    fn a_test_build_sees_the_next_test_release() {
        assert_eq!(tags(REAL, "0.1.0-alpha.1").as_deref(), Some("0.1.0-alpha.2"));
        let r = newest(REAL, "0.1.0-alpha.1").unwrap().unwrap();
        assert_eq!(r.page, "https://github.com/Wiibleyde/LiveCraft/releases/tag/v0.1.0-alpha.2");
    }

    #[test]
    fn versions_follow_semantic_precedence() {
        let order =
            ["0.1.0-alpha.1", "0.1.0-alpha.2", "0.1.0-alpha.10", "0.1.0-beta.1", "0.1.0-rc.1", "0.1.0"];
        for pair in order.windows(2) {
            let later = parse(pair[1]).unwrap();
            assert!(is_newer(&later, pair[0]), "{} after {}", pair[1], pair[0]);
            assert!(!is_newer(&parse(pair[0]).unwrap(), pair[1]), "{} not after {}", pair[0], pair[1]);
        }
        assert!(is_newer(&parse("0.10.0").unwrap(), "0.9.0"), "numbers, not text");
        assert!(!is_newer(&parse("1.0.0+build.2").unwrap(), "1.0.0"), "build metadata does not count");
    }

    #[test]
    fn a_test_build_takes_the_newest_of_tests_and_stable_releases() {
        let answer = r#"[
            {"tag_name": "v0.1.0-beta.1", "prerelease": true},
            {"tag_name": "v0.1.0", "prerelease": false},
            {"tag_name": "v0.1.0-rc.1", "prerelease": true}
        ]"#;
        assert_eq!(tags(answer, "0.1.0-alpha.1").as_deref(), Some("0.1.0"));
        let later_test = r#"[{"tag_name": "v0.1.0"}, {"tag_name": "v0.2.0-alpha.1", "prerelease": true}]"#;
        assert_eq!(tags(later_test, "0.1.0-alpha.1").as_deref(), Some("0.2.0-alpha.1"));
    }

    #[test]
    fn a_stable_build_ignores_test_releases() {
        let answer = r#"[
            {"tag_name": "v0.3.0-alpha.1", "prerelease": true},
            {"tag_name": "v0.2.1", "prerelease": false},
            {"tag_name": "v0.4.0", "prerelease": true},
            {"tag_name": "v0.5.0-rc.1", "prerelease": false}
        ]"#;
        // Marked a prerelease on GitHub, or carrying a suffix: either is a test.
        assert_eq!(tags(answer, "0.2.0").as_deref(), Some("0.2.1"));
        assert_eq!(tags(answer, "0.2.1"), None);
    }

    #[test]
    fn drafts_are_never_offered() {
        let answer = r#"[{"tag_name": "v9.0.0", "draft": true}, {"tag_name": "v0.2.0", "draft": false}]"#;
        assert_eq!(tags(answer, "0.1.0").as_deref(), Some("0.2.0"));
        assert_eq!(tags(r#"[{"tag_name": "v9.0.0", "draft": true}]"#, "0.1.0"), None);
    }

    #[test]
    fn the_running_version_or_an_older_one_is_not_news() {
        let answer = r#"[{"tag_name": "v0.1.0-alpha.1", "prerelease": true}, {"tag_name": "v0.0.9"}]"#;
        assert_eq!(tags(answer, "0.1.0-alpha.1"), None);
        assert_eq!(tags(r#"[{"tag_name": "v0.1.0"}]"#, "0.1.0"), None);
    }

    #[test]
    fn tags_that_are_not_versions_are_skipped() {
        let answer = r#"[
            {"tag_name": "nightly"}, {"tag_name": "9.0.0"}, {"tag_name": "v9"},
            {"tag_name": "v9.0"}, {"tag_name": "v09.0.0"}, {"tag_name": ""}, {"name": "no tag"},
            {"tag_name": "v0.2.0"}
        ]"#;
        assert_eq!(tags(answer, "0.1.0").as_deref(), Some("0.2.0"));
    }

    #[test]
    fn an_empty_list_offers_nothing_and_a_wrong_answer_is_an_error() {
        assert_eq!(tags("[]", "0.1.0"), None);
        assert!(newest(r#"{"message": "API rate limit exceeded"}"#, "0.1.0").is_err());
        assert!(newest("<html>", "0.1.0").is_err());
        assert!(newest("[]", "not a version").is_err());
    }

    #[test]
    fn the_check_at_start_asks_at_most_once_a_day() {
        let now = 1_800_000_000;
        let mut u = Updates::default();
        assert!(u.due(now), "never asked");
        u.checked_at = Some(now);
        assert!(!u.due(now));
        assert!(!u.due(now + EVERY - 1));
        assert!(u.due(now + EVERY));
        assert!(u.due(now - 60), "the clock went back");
        u.check = false;
        assert!(!u.due(now + 10 * EVERY), "turned off");
    }

    #[test]
    fn a_put_away_notice_stays_away_until_a_newer_version() {
        let mut u = Updates { found: Some("0.2.0".into()), ..Updates::default() };
        assert_eq!(u.notice("0.1.0").map(|r| r.version).as_deref(), Some("0.2.0"));
        u.dismissed = "0.2.0".into();
        assert_eq!(u.notice("0.1.0"), None);
        assert!(u.newer("0.1.0").is_some(), "About still says it");
        u.answered(newest(r#"[{"tag_name": "v0.3.0"}]"#, "0.1.0").unwrap().as_ref());
        assert_eq!(u.notice("0.1.0").map(|r| r.version).as_deref(), Some("0.3.0"));
        u.dismissed = "0.4.0".into();
        assert_eq!(u.notice("0.1.0"), None, "an older one stays quiet too");
        u.dismissed.clear();
        u.check = false;
        assert_eq!(u.notice("0.1.0"), None, "turned off, nothing is said");
        assert!(u.newer("0.1.0").is_some(), "but About still knows");
    }

    #[test]
    fn once_installed_the_found_version_is_no_longer_news() {
        let u = Updates { found: Some("0.2.0".into()), ..Updates::default() };
        assert_eq!(u.notice("0.2.0"), None);
        assert_eq!(u.newer("0.3.0"), None);
        let mut u = u;
        u.answered(None);
        assert_eq!(u.found, None, "an answer with nothing newer clears it");
    }

    #[test]
    fn an_older_session_checks_by_default() {
        let old: Updates = serde_json::from_str("{}").unwrap();
        assert!(old.check);
        let some: Updates = serde_json::from_str(r#"{"dismissed": "0.2.0"}"#).unwrap();
        assert!(some.check);
        let off: Updates = serde_json::from_str(r#"{"check": false}"#).unwrap();
        assert!(!off.check);
    }

    /// The real API, by hand: `cargo test real_releases -- --ignored --nocapture`.
    #[test]
    #[ignore = "asks GitHub over the network"]
    fn real_releases_are_read() {
        let got = fetch("0.0.1-alpha.0").expect("GitHub answers");
        println!("newest for 0.0.1-alpha.0: {got:?}");
        assert!(got.is_some(), "at least one release is newer than 0.0.1-alpha.0");
        println!("newest for {CURRENT}: {:?}", fetch(CURRENT).expect("GitHub answers"));
    }
}
