use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }

    pub fn short(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url())
    }
}

fn valid_name(s: &str) -> bool {
    s.len() <= 100
        && s.chars().any(|c| c != '.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A PR number as GitHub writes it: digits only, no leading zero.
fn pr_number(s: &str) -> Option<u64> {
    (s.len() <= 10 && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

/// Accepts PR links in their usual variants (`/files`, `/commits/<sha>`, query, fragment,
/// trailing slash, missing scheme, `www.`) and the `owner/repo#123` shorthand.
pub fn parse(input: &str) -> Option<PrRef> {
    let s = input.trim();
    if let Some((repo_part, num)) = s.split_once('#')
        && !repo_part.contains("://")
        && !repo_part.contains("github.com")
    {
        let (owner, repo) = repo_part.split_once('/')?;
        let number = pr_number(num)?;
        return build(owner, repo, number);
    }
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s.split(['#', '?']).next()?;
    let mut parts = s.split('/');
    let host = parts.next()?.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next()? != "pull" {
        return None;
    }
    let number = pr_number(parts.next()?)?;
    build(owner, repo, number)
}

fn build(owner: &str, repo: &str, number: u64) -> Option<PrRef> {
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if !valid_name(owner) || !valid_name(repo) {
        return None;
    }
    Some(PrRef {
        owner: owner.to_ascii_lowercase(),
        repo: repo.to_ascii_lowercase(),
        number,
    })
}

pub fn canonical(input: &str) -> Option<String> {
    parse(input).map(|p| p.url())
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = "https://github.com/rust-lang/rust/pull/12345";

    #[test]
    fn variants_normalise() {
        for v in [
            "https://github.com/rust-lang/rust/pull/12345",
            "https://github.com/rust-lang/rust/pull/12345/",
            "https://github.com/rust-lang/rust/pull/12345/files",
            "https://github.com/rust-lang/rust/pull/12345/files#diff-abc123R10",
            "https://github.com/rust-lang/rust/pull/12345/commits/deadbeef",
            "https://github.com/rust-lang/rust/pull/12345?w=1",
            "https://github.com/rust-lang/rust/pull/12345#issuecomment-1",
            "http://github.com/rust-lang/rust/pull/12345",
            "https://www.github.com/rust-lang/rust/pull/12345",
            "github.com/rust-lang/rust/pull/12345",
            "https://github.com/Rust-Lang/Rust/pull/12345",
            "  https://github.com/rust-lang/rust/pull/12345  ",
            "rust-lang/rust#12345",
            "https://github.com/rust-lang/rust.git/pull/12345",
        ] {
            assert_eq!(canonical(v).as_deref(), Some(C), "{v}");
        }
    }

    #[test]
    fn rejects_non_prs() {
        let long_repo = format!("a/{}#1", "r".repeat(101));
        for v in [
            "",
            "https://github.com/rust-lang/rust",
            "https://github.com/rust-lang/rust/issues/12345",
            "https://github.com/rust-lang/rust/pull/abc",
            "https://github.com/rust-lang/rust/pull/0",
            "https://gitlab.com/a/b/pull/1",
            "https://evil.com/github.com/a/b/pull/1",
            "https://github.com/a b/c/pull/1",
            "https://github.com/../c/pull/1",
            "javascript:alert(1)//github.com/a/b/pull/1",
            "a/b#x",
            "a/b#+8",
            "a/b#007",
            "a/b#18446744073709551615",
            ".../b#1",
            &long_repo,
        ] {
            assert_eq!(canonical(v), None, "{v}");
        }
    }

    #[test]
    fn short_form() {
        assert_eq!(parse(C).unwrap().short(), "rust-lang/rust#12345");
    }
}
