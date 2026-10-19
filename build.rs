use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");

    // An archive nested inside another repository must not claim that repo's HEAD.
    let git_root = git_output(&["rev-parse", "--show-toplevel"]);
    let is_source_repo = git_root
        .as_deref()
        .and_then(|root| Path::new(root).canonicalize().ok())
        .zip(Path::new(".").canonicalize().ok())
        .is_some_and(|(root, source)| root == source);
    let git_head = if is_source_repo {
        // Let Git resolve both per-worktree HEAD and shared refs/packed-refs.
        watch_git_path("HEAD", false);
        watch_git_path("packed-refs", false);
        if let Some(reference) = git_output(&["symbolic-ref", "--quiet", "HEAD"]) {
            watch_git_path(&reference, true);
        }
        git_output(&["rev-parse", "--verify", "HEAD^{commit}"])
    } else {
        None
    };
    let explicit = std::env::var("HERDR_BUILD_COMMIT");
    let commit = match explicit {
        Ok(ref value) => select_commit(Some(value), git_head.as_deref()),
        Err(std::env::VarError::NotPresent) => select_commit(None, git_head.as_deref()),
        Err(std::env::VarError::NotUnicode(_)) => "unknown",
    };
    println!("cargo:rustc-env=HERDR_BUILD_COMMIT={commit}");
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn watch_git_path(name: &str, watch_parent: bool) {
    let Some(path) = git_output(&["rev-parse", "--git-path", name]) else {
        return;
    };
    // A missing loose ref can be created later (for example after packing).
    // Watch its nearest existing ancestor rather than forcing every build dirty.
    let path = Path::new(&path);
    let watched = if watch_parent {
        path.ancestors().find(|path| path.exists())
    } else {
        path.exists().then_some(path)
    };
    if let Some(path) = watched {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn select_commit<'a>(explicit: Option<&'a str>, git_head: Option<&'a str>) -> &'a str {
    let value = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(git_head);
    match value {
        // Accept full SHA-1/SHA-256 IDs and abbreviated Git object IDs.
        Some(value)
            if (7..=64).contains(&value.len())
                && value.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            value
        }
        // An invalid explicit override must not silently claim the checkout's ID.
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::select_commit;

    #[test]
    fn explicit_commit_takes_precedence() {
        assert_eq!(
            select_commit(Some("  abc1234  "), Some("def5678")),
            "abc1234"
        );
        assert_eq!(select_commit(Some(&"a".repeat(40)), None), "a".repeat(40));
        assert_eq!(select_commit(Some(&"b".repeat(64)), None), "b".repeat(64));
    }

    #[test]
    fn absent_or_empty_override_uses_git_or_unknown() {
        assert_eq!(select_commit(None, Some("def5678")), "def5678");
        assert_eq!(select_commit(Some(" \t"), Some("def5678")), "def5678");
        assert_eq!(select_commit(None, None), "unknown");
        assert_eq!(select_commit(Some(""), None), "unknown");
    }

    #[test]
    fn invalid_override_does_not_claim_checkout_provenance() {
        for value in ["not-a-commit", "abcdef", "abc1234\ncargo:rustc-cfg=bad"] {
            assert_eq!(select_commit(Some(value), Some("def5678")), "unknown");
        }
        assert_eq!(
            select_commit(Some(&"a".repeat(65)), Some("def5678")),
            "unknown"
        );
        assert_eq!(select_commit(None, Some("invalid")), "unknown");
    }
}
