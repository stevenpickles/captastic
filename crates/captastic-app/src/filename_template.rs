//! Turning a capture into a file name the user chose the shape of.
//!
//! A filename template mixes text the user wrote with text they did not: a window title is set by
//! whatever application owns the window, which is to say by anyone. `..`, a path separator, a
//! reserved device name, or a trailing dot are all things an application can put in its own title
//! bar, and all of them mean something to a filesystem.
//!
//! So substituted values are sanitized on the way in and the assembled name is checked again on
//! the way out, and neither step trusts the other to have been enough. Milestone 4's exit criteria
//! put it plainly: filename input cannot escape the configured output directory.

use std::path::{Component, Path, PathBuf};

/// Characters Windows forbids outright, plus the separators every platform cares about.
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
/// Names MS-DOS device files still reserve, with or without an extension, in any case.
const RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];
/// How much of one substituted value survives. Long enough for a real window title, short enough
/// that several fields together cannot approach a path limit.
const MAX_FIELD_CHARS: usize = 64;
/// How long the assembled stem may be, before the extension.
const MAX_STEM_CHARS: usize = 120;
/// Used when a template expands to nothing usable, so a capture is never lost to its own name.
const FALLBACK_STEM: &str = "capture";

/// What a template can refer to.
pub struct TemplateContext<'a> {
    pub timestamp_micros: u128,
    pub display: &'a str,
    pub mode: &'a str,
    pub width: u32,
    pub height: u32,
    /// The owning application of a captured window, where the capture had one.
    pub application: Option<&'a str>,
    /// The captured window's title. Attacker-adjacent: any application sets its own.
    pub title: Option<&'a str>,
}

/// Every token a template may use. Anything else is rejected when the configuration loads, so a
/// typo is a startup error rather than a literal `{tilte}` appearing in a file name forever.
pub const TOKENS: &[&str] = &[
    "timestamp",
    "date",
    "time",
    "display",
    "mode",
    "width",
    "height",
    "application",
    "title",
];

/// Reports what is wrong with a template, for the configuration validator.
pub fn validate_template(template: &str) -> Result<(), String> {
    if template.trim().is_empty() {
        return Err("output.filename_template must not be empty".to_owned());
    }
    // A separator in the *template* is the user asking for a subdirectory, which this does not
    // support: the output directory is the boundary, and honouring it here would make the
    // traversal guarantee a matter of how carefully the template was written.
    if template.contains('/') || template.contains('\\') {
        return Err(
            "output.filename_template must not contain path separators; it names a file, not a path"
                .to_owned(),
        );
    }
    let mut rest = template;
    let mut has_token = false;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(format!(
                "output.filename_template has an unclosed '{{' in {template:?}"
            ));
        };
        let token = &after[..close];
        if !TOKENS.contains(&token) {
            return Err(format!(
                "output.filename_template uses unknown token {{{token}}}; known tokens are {}",
                TOKENS
                    .iter()
                    .map(|token| format!("{{{token}}}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        has_token = true;
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!(
            "output.filename_template has an unmatched '}}' in {template:?}"
        ));
    }
    // A template of pure literal text names every capture the same thing, and every capture after
    // the first would land on the collision path forever.
    if !has_token {
        return Err(
            "output.filename_template must use at least one token, or every capture would compete for one name"
                .to_owned(),
        );
    }
    Ok(())
}

/// Expands a validated template into a file-name stem, sanitizing as it goes.
pub fn expand(template: &str, context: &TemplateContext<'_>) -> String {
    let (year, month, day, hour, minute, second, millis) =
        crate::clock::utc_parts(context.timestamp_micros);
    let mut stem = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        stem.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // Unreachable for a validated template; treated as literal rather than panicking,
            // because a filename is not worth aborting a capture over.
            stem.push_str(after);
            rest = "";
            break;
        };
        let value = match &after[..close] {
            "timestamp" => {
                format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{millis:03}")
            }
            "date" => format!("{year:04}{month:02}{day:02}"),
            "time" => format!("{hour:02}{minute:02}{second:02}-{millis:03}"),
            "display" => context.display.to_owned(),
            "mode" => context.mode.to_owned(),
            "width" => context.width.to_string(),
            "height" => context.height.to_string(),
            "application" => context.application.unwrap_or_default().to_owned(),
            "title" => context.title.unwrap_or_default().to_owned(),
            _ => String::new(),
        };
        stem.push_str(&sanitize_field(&value));
        rest = &after[close + 1..];
    }
    stem.push_str(rest);
    finalize_stem(&stem)
}

/// Reduces one substituted value to something safe to put in a name.
fn sanitize_field(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut pending_separator = false;
    for character in value.chars() {
        // Control characters, forbidden punctuation, and anything a filesystem treats specially
        // all collapse to a single separator rather than vanishing, so `a<b` cannot become `ab`.
        let acceptable = !character.is_control()
            && !FORBIDDEN.contains(&character)
            && character != '{'
            && character != '}';
        if acceptable && !character.is_whitespace() && character != '.' {
            if pending_separator && !sanitized.is_empty() {
                sanitized.push('-');
            }
            pending_separator = false;
            sanitized.push(character);
        } else {
            // Runs of unusable characters become one separator, not one each.
            pending_separator = true;
        }
        if sanitized.chars().count() >= MAX_FIELD_CHARS {
            break;
        }
    }
    sanitized
}

/// Applies the rules that are about the name as a whole rather than any one field.
fn finalize_stem(stem: &str) -> String {
    let mut finalized: String = stem
        .trim_matches(|character: char| character == '-' || character.is_whitespace())
        .chars()
        .take(MAX_STEM_CHARS)
        .collect();
    // Windows strips trailing dots and spaces when resolving a name, so a stem ending in one
    // resolves to a *different* file than the one that was created.
    while finalized.ends_with('.') || finalized.ends_with(' ') {
        finalized.pop();
    }
    if finalized.is_empty() {
        return FALLBACK_STEM.to_owned();
    }
    // `nul.png` still opens the null device. Reserved names are compared against the part before
    // the first dot, which is what the rule actually applies to.
    let leading = finalized
        .split('.')
        .next()
        .unwrap_or(&finalized)
        .to_ascii_lowercase();
    if RESERVED_STEMS.contains(&leading.as_str()) {
        return format!("{finalized}-capture");
    }
    finalized
}

/// Confirms an assembled path is a direct child of the directory it was meant for.
///
/// The last line rather than the only one: sanitization should have made this impossible, and
/// this is what makes "should have" unnecessary to believe. Compares components rather than
/// string prefixes, so `captures-elsewhere` is not mistaken for a child of `captures`.
pub fn is_inside(directory: &Path, path: &Path) -> bool {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    match path.parent() {
        Some(parent) => parent == directory,
        None => false,
    }
}

/// Joins a sanitized stem to its directory, refusing anything that would land elsewhere.
pub fn resolve(directory: &Path, stem: &str, extension: &str) -> Option<PathBuf> {
    let candidate = directory.join(format!("{stem}.{extension}"));
    is_inside(directory, &candidate).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> TemplateContext<'static> {
        TemplateContext {
            // 2026-08-16T23:05:00.348Z
            timestamp_micros: 1_786_921_500_348_000,
            display: "primary",
            mode: "region",
            width: 1920,
            height: 1080,
            application: None,
            title: None,
        }
    }

    #[test]
    fn the_default_shape_expands_to_something_sortable() {
        let stem = expand("captastic-{timestamp}", &context());
        assert_eq!(stem, "captastic-20260816-230500-348");
    }

    #[test]
    fn every_token_resolves() {
        let mut context = context();
        context.application = Some("Firefox");
        context.title = Some("Some Page");
        assert_eq!(
            expand(
                "{date}_{time}_{display}_{mode}_{width}x{height}_{application}_{title}",
                &context
            ),
            "20260816_230500-348_primary_region_1920x1080_Firefox_Some-Page"
        );
    }

    #[test]
    fn a_hostile_window_title_cannot_escape_the_directory() {
        // The exit criterion, stated as a test. Every one of these is something an application
        // can put in its own title bar.
        let directory = Path::new("C:/captures");
        let hostile = [
            "../../../Windows/System32/evil",
            "..\\..\\secrets",
            "/etc/passwd",
            "C:\\Windows\\System32\\drivers\\etc\\hosts",
            "a/../../b",
            "....//....//x",
        ];
        for title in hostile {
            let mut context = context();
            context.title = Some(title);
            let stem = expand("{title}", &context);

            assert!(
                !stem.contains('/') && !stem.contains('\\'),
                "{title:?} produced a separator: {stem:?}"
            );
            assert!(
                !stem.contains(".."),
                "{title:?} kept a parent hop: {stem:?}"
            );
            let path = resolve(directory, &stem, "png").expect("a sanitized stem resolves");
            assert_eq!(
                path.parent(),
                Some(directory),
                "{title:?} escaped to {}",
                path.display()
            );
        }
    }

    #[test]
    fn reserved_device_names_are_defused() {
        // `nul.png` still opens the null device, so the capture would vanish with no error.
        for reserved in ["nul", "NUL", "con", "Com1", "LPT9", "aux"] {
            let mut context = context();
            context.title = Some(reserved);
            let stem = expand("{title}", &context);
            let leading = stem.split('.').next().unwrap_or(&stem).to_ascii_lowercase();
            assert!(
                !RESERVED_STEMS.contains(&leading.as_str()),
                "{reserved} survived as {stem}"
            );
        }
        // A name that merely starts with one is fine: `console-log` is not a device.
        let mut context = context();
        context.title = Some("console-log");
        assert_eq!(expand("{title}", &context), "console-log");
    }

    #[test]
    fn trailing_dots_and_spaces_are_removed() {
        // Windows resolves `name.` to `name`, so a file created as the first is found as the
        // second — and a collision check against the first would never fire.
        for title in ["report.", "report ", "report. . ", "report..."] {
            let mut context = context();
            context.title = Some(title);
            let stem = expand("{title}", &context);
            assert!(
                !stem.ends_with('.') && !stem.ends_with(' '),
                "{title:?} produced {stem:?}"
            );
        }
    }

    #[test]
    fn control_characters_and_forbidden_punctuation_collapse_to_separators() {
        let mut context = context();
        context.title = Some("a<b>c:d\"e|f?g*h\u{7}i");
        let stem = expand("{title}", &context);
        assert_eq!(stem, "a-b-c-d-e-f-g-h-i");
        assert!(!stem.chars().any(char::is_control));
    }

    #[test]
    fn a_title_that_sanitizes_to_nothing_still_produces_a_name() {
        // A capture must never be lost because of what an application called its window.
        for title in ["", "   ", "...", "///", "<<<>>>"] {
            let mut context = context();
            context.title = Some(title);
            let stem = expand("{title}", &context);
            assert!(!stem.is_empty(), "{title:?} produced an empty stem");
            assert_eq!(stem, FALLBACK_STEM);
        }
    }

    #[test]
    fn long_values_are_bounded_per_field_and_overall() {
        let long = "x".repeat(500);
        let mut context = context();
        context.title = Some(&long);
        context.application = Some(&long);
        let stem = expand("{application}-{title}-{timestamp}", &context);
        assert!(stem.chars().count() <= MAX_STEM_CHARS, "{}", stem.len());
        // Each field is capped before the whole is, so one long value cannot crowd out the rest.
        assert!(stem.starts_with(&"x".repeat(MAX_FIELD_CHARS)));
    }

    #[test]
    fn unknown_tokens_are_rejected_when_the_configuration_loads() {
        // A typo should be a startup error, not a literal `{tilte}` in every file name.
        assert!(validate_template("captastic-{tilte}").is_err());
        assert!(validate_template("captastic-{timestamp}").is_ok());
        assert!(validate_template("{date}/{time}").is_err(), "separators");
        assert!(validate_template("{date}").is_ok());
        assert!(validate_template("").is_err(), "empty");
        assert!(validate_template("   ").is_err(), "blank");
        assert!(validate_template("screenshot").is_err(), "no token");
        assert!(validate_template("{timestamp").is_err(), "unclosed");
        assert!(validate_template("timestamp}").is_err(), "unmatched close");
    }

    #[test]
    fn containment_compares_components_rather_than_prefixes() {
        let directory = Path::new("C:/captures");
        assert!(is_inside(directory, &directory.join("a.png")));
        // A sibling whose name merely starts with the directory's is not inside it.
        assert!(!is_inside(
            directory,
            Path::new("C:/captures-elsewhere/a.png")
        ));
        // A subdirectory is not a direct child, and this writes files, not trees.
        assert!(!is_inside(
            directory,
            &directory.join("nested").join("a.png")
        ));
        assert!(!is_inside(
            directory,
            Path::new("C:/captures/../other/a.png")
        ));
    }

    #[test]
    fn resolving_refuses_a_stem_that_would_land_elsewhere() {
        // `resolve` is the last line, so it must hold even if a caller hands it something
        // sanitization would never have produced.
        let directory = Path::new("C:/captures");
        assert!(resolve(directory, "ok", "png").is_some());
        assert!(resolve(directory, "../escape", "png").is_none());
        assert!(resolve(directory, "nested/deep", "png").is_none());
    }
}
