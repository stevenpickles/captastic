use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const WINDOWS_ICON: &str = "../../assets/branding/captastic.ico";
const WINDOWS_MANIFEST: &str = "captastic.manifest";
const BUILD_INFO_FILE: &str = "captastic_build_info.rs";

#[derive(Debug)]
struct BuildIdentity {
    release_version: String,
    version: String,
    channel: &'static str,
    git_commit: Option<String>,
    git_short_commit: Option<String>,
    revision_count: Option<u64>,
    source_tag: Option<String>,
    dirty: bool,
    ci_run_id: Option<String>,
    ci_run_number: Option<u64>,
    ci_run_attempt: Option<u64>,
    ci_run_url: Option<String>,
    target: String,
    profile: String,
}

fn main() -> io::Result<()> {
    emit_rebuild_directives();
    let identity = resolve_build_identity();
    write_build_info(&identity)?;

    println!("cargo:rerun-if-changed={WINDOWS_ICON}");
    println!("cargo:rerun-if-changed={WINDOWS_MANIFEST}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(WINDOWS_ICON)
        .set_manifest_file(WINDOWS_MANIFEST)
        .set("FileDescription", "Captastic screenshot capture")
        .set("ProductName", "Captastic");
    resource.compile()
}

fn emit_rebuild_directives() {
    for name in [
        "CAPTASTIC_BUILD_VERSION",
        "CAPTASTIC_GIT_COMMIT",
        "CAPTASTIC_GIT_DIRTY",
        "GITHUB_ACTIONS",
        "GITHUB_REF_NAME",
        "GITHUB_REF_TYPE",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_RUN_ID",
        "GITHUB_RUN_NUMBER",
        "GITHUB_SERVER_URL",
        "GITHUB_REPOSITORY",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let repository = repository_root();
    let git_directory = repository.join(".git");
    println!(
        "cargo:rerun-if-changed={}",
        git_directory.join("HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        git_directory.join("index").display()
    );
}

fn resolve_build_identity() -> BuildIdentity {
    let release_version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let repository = repository_root();
    let git_commit = nonempty_env("CAPTASTIC_GIT_COMMIT")
        .or_else(|| nonempty_env("GITHUB_SHA"))
        .or_else(|| git(&repository, &["rev-parse", "HEAD"]));
    let git_short_commit = git_commit
        .as_deref()
        .map(|commit| commit.chars().take(9).collect::<String>());
    let dirty = nonempty_env("CAPTASTIC_GIT_DIRTY")
        .map(|value| parse_bool(&value, "CAPTASTIC_GIT_DIRTY"))
        .unwrap_or_else(|| {
            git(
                &repository,
                &["status", "--porcelain", "--untracked-files=normal"],
            )
            .is_some_and(|status| !status.is_empty())
        });
    let expected_tag = format!("v{release_version}");
    let source_tag = tags_at_head(&repository)
        .into_iter()
        .find(|tag| tag == &expected_tag);
    let github_tag = (nonempty_env("GITHUB_REF_TYPE").as_deref() == Some("tag"))
        .then(|| nonempty_env("GITHUB_REF_NAME"))
        .flatten();
    if let Some(tag) = github_tag.as_deref() {
        assert_eq!(
            tag, expected_tag,
            "release tag {tag} must match workspace version {release_version}"
        );
    }
    let source_tag = source_tag.or(github_tag);
    let revision_count = revision_count(&repository);
    let is_ci = nonempty_env("GITHUB_ACTIONS").is_some();
    let channel = if source_tag.is_some() {
        "release"
    } else if is_ci {
        "ci"
    } else {
        "development"
    };
    if channel == "release" {
        assert!(!dirty, "release builds require a clean worktree");
        assert!(git_commit.is_some(), "release builds require a Git commit");
    }

    let ci_run_number = optional_u64_env("GITHUB_RUN_NUMBER");
    let ci_run_attempt = optional_u64_env("GITHUB_RUN_ATTEMPT");
    let generated_version = match channel {
        "release" => release_version.clone(),
        "ci" => format!(
            "{}-ci.{}.{}.g{}",
            release_version,
            ci_run_number.unwrap_or(0),
            ci_run_attempt.unwrap_or(1),
            git_short_commit.as_deref().unwrap_or("unknown")
        ),
        _ => {
            let mut version = format!(
                "{}-dev.{}.g{}",
                release_version,
                revision_count.unwrap_or(0),
                git_short_commit.as_deref().unwrap_or("unknown")
            );
            if dirty {
                version.push_str(".dirty");
            }
            version
        }
    };
    let version = nonempty_env("CAPTASTIC_BUILD_VERSION").unwrap_or(generated_version);
    let ci_run_id = nonempty_env("GITHUB_RUN_ID");
    let ci_run_url = match (
        nonempty_env("GITHUB_SERVER_URL"),
        nonempty_env("GITHUB_REPOSITORY"),
        ci_run_id.as_deref(),
    ) {
        (Some(server), Some(repository), Some(run_id)) => {
            Some(format!("{server}/{repository}/actions/runs/{run_id}"))
        }
        _ => None,
    };

    BuildIdentity {
        release_version,
        version,
        channel,
        git_commit,
        git_short_commit,
        revision_count,
        source_tag,
        dirty,
        ci_run_id,
        ci_run_number,
        ci_run_attempt,
        ci_run_url,
        target: env::var("TARGET").expect("Cargo target triple"),
        profile: env::var("PROFILE").expect("Cargo profile"),
    }
}

fn write_build_info(identity: &BuildIdentity) -> io::Result<()> {
    let contents = format!(
        "pub const BUILD_INFO: BuildInfo = BuildInfo {{\n\
         \x20   release_version: {release_version:?},\n\
         \x20   version: {version:?},\n\
         \x20   channel: {channel:?},\n\
         \x20   git_commit: {git_commit:?},\n\
         \x20   git_short_commit: {git_short_commit:?},\n\
         \x20   revision_count: {revision_count:?},\n\
         \x20   source_tag: {source_tag:?},\n\
         \x20   dirty: {dirty},\n\
         \x20   ci_run_id: {ci_run_id:?},\n\
         \x20   ci_run_number: {ci_run_number:?},\n\
         \x20   ci_run_attempt: {ci_run_attempt:?},\n\
         \x20   ci_run_url: {ci_run_url:?},\n\
         \x20   target: {target:?},\n\
         \x20   profile: {profile:?},\n\
         }};\n",
        release_version = identity.release_version,
        version = identity.version,
        channel = identity.channel,
        git_commit = identity.git_commit.as_deref(),
        git_short_commit = identity.git_short_commit.as_deref(),
        revision_count = identity.revision_count,
        source_tag = identity.source_tag.as_deref(),
        dirty = identity.dirty,
        ci_run_id = identity.ci_run_id.as_deref(),
        ci_run_number = identity.ci_run_number,
        ci_run_attempt = identity.ci_run_attempt,
        ci_run_url = identity.ci_run_url.as_deref(),
        target = identity.target,
        profile = identity.profile,
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
            .join(BUILD_INFO_FILE),
        contents,
    )
}

fn repository_root() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("../..")
}

fn revision_count(repository: &Path) -> Option<u64> {
    let latest_tag = git(
        repository,
        &["describe", "--tags", "--match", "v[0-9]*", "--abbrev=0"],
    );
    let range = latest_tag.as_deref().map(|tag| format!("{tag}..HEAD"));
    let args = match range.as_deref() {
        Some(range) => vec!["rev-list", "--count", range],
        None => vec!["rev-list", "--count", "HEAD"],
    };
    git(repository, &args)?.parse().ok()
}

fn tags_at_head(repository: &Path) -> Vec<String> {
    git(repository, &["tag", "--points-at", "HEAD"])
        .map(|tags| tags.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn git(repository: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn optional_u64_env(name: &str) -> Option<u64> {
    nonempty_env(name).map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an unsigned integer"))
    })
}

fn parse_bool(value: &str, name: &str) -> bool {
    match value {
        "1" | "true" | "True" | "TRUE" => true,
        "0" | "false" | "False" | "FALSE" => false,
        _ => panic!("{name} must be true or false"),
    }
}
