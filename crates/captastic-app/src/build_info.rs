use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BuildInfo {
    pub release_version: &'static str,
    pub version: &'static str,
    pub channel: &'static str,
    pub git_commit: Option<&'static str>,
    pub git_short_commit: Option<&'static str>,
    pub revision_count: Option<u64>,
    pub source_tag: Option<&'static str>,
    pub dirty: bool,
    pub ci_run_id: Option<&'static str>,
    pub ci_run_number: Option<u64>,
    pub ci_run_attempt: Option<u64>,
    pub ci_run_url: Option<&'static str>,
    pub target: &'static str,
    pub profile: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/captastic_build_info.rs"));

pub const BUILD_VERSION: &str = BUILD_INFO.version;
