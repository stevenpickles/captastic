//! What a filename template may say, and whether one says it.
//!
//! Split from the expansion that uses it (`captastic-app`'s `filename_template`) so that
//! `captastic config validate` can refuse a template the daemon would refuse at startup. A
//! configuration that passes validation and then fails to launch is a worse error than either
//! half on its own: the check that would have caught it ran in the wrong process.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
