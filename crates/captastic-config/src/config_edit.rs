//! Surgical edits to the user's own configuration file.
//!
//! Captastic's own memory lives in `state.toml` and is serialized whole ([`crate::ui_state`]).
//! This module is the other case: a setting the user owns, expressed in their file, that a tray
//! command has to change. Their file has comments, ordering, and hand edits in it, so it is
//! patched through `toml_edit` rather than round-tripped through `AppConfig` — a serialize of the
//! parsed configuration would silently rewrite the document into Captastic's preferred shape and
//! drop every comment in it.
//!
//! The write goes through [`atomic_write`], so a reader sees either the previous file or the
//! complete new one, and is serialized against other writers with the same lock the state store
//! uses.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use toml_edit::{value, Document, Item, Table};

use crate::fsio::{atomic_write, finalize_new, replace_file, FileLock};
use crate::{ConfigError, CONFIG_SCHEMA_VERSION};

/// What a configuration file created for a single setting starts as.
///
/// Reached when the default profile has never been written and the user turns file output on from
/// the notification area. Everything absent stays at its default, which is what the comment says
/// so the next reader of a two-line configuration is not left guessing.
fn new_document(enabled: bool) -> String {
    format!(
        "# Created by Captastic when \"Save Captures to Disk\" was switched on from the\n\
         # notification area. Every setting not named here keeps its default; see\n\
         # captastic.example.toml for the full list.\n\
         schema_version = {CONFIG_SCHEMA_VERSION}\n\
         \n\
         [output]\n\
         enabled = {enabled}\n"
    )
}

/// Sets `output.enabled` in the configuration at `path`, preserving everything else in the file.
///
/// Creates the file, and any missing parent directory, when it does not exist yet: the default
/// profile is only written when something needs it, and a preference the user just expressed is
/// exactly that. Every other setting is left absent rather than materialized at its current
/// default, so a later Captastic changing a default still reaches this installation.
///
/// A file that appears between the read and the write is edited rather than overwritten. Not every
/// writer of this file takes the lock this one holds — `ensure_default_config` does not, and
/// neither does the user's text editor — so "it was not there a moment ago" is not a licence to
/// replace whatever is there now.
pub fn set_output_enabled(path: &Path, enabled: bool) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    // Held across the read and the write, so two processes flipping the same setting cannot lose
    // one another's edit — and, more to the point, so a read-modify-write never bases itself on a
    // document another writer is halfway through replacing.
    let _lock = FileLock::acquire(path)?;
    // Twice at most. The first pass can find no file and be beaten to creating one by a writer
    // that does not take this lock — `ensure_default_config`, or the user's editor — in which case
    // the second pass reads what they wrote and edits it. A creation that loses that race twice
    // would mean the file is being created and deleted underneath us, which is not a race worth
    // looping on.
    for attempt in 0..2 {
        let existing = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(source) if source.kind() == ErrorKind::NotFound => None,
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        let Some(text) = existing else {
            // Created, not replaced: between the read above and this write, anything may have put
            // a configuration there, and replacing it would destroy a file this call never read.
            match atomic_write(path, new_document(enabled).as_bytes(), finalize_new) {
                Ok(()) => return Ok(()),
                Err(source) if source.kind() == ErrorKind::AlreadyExists && attempt == 0 => {
                    continue
                }
                Err(source) => {
                    return Err(ConfigError::Write {
                        path: path.display().to_string(),
                        source,
                    })
                }
            }
        };
        let mut document = text.parse::<Document>()?;
        set_enabled(&mut document, enabled)?;
        return atomic_write(path, document.to_string().as_bytes(), replace_file).map_err(
            |source| ConfigError::Write {
                path: path.display().to_string(),
                source,
            },
        );
    }
    Err(ConfigError::Write {
        path: path.display().to_string(),
        source: std::io::Error::new(
            ErrorKind::AlreadyExists,
            "the configuration file appeared and disappeared while it was being written",
        ),
    })
}

/// Writes the key into `[output]`, creating the table if the file has never mentioned it.
fn set_enabled(document: &mut Document, enabled: bool) -> Result<(), ConfigError> {
    let output = document
        .as_table_mut()
        .entry("output")
        .or_insert_with(|| Item::Table(Table::new()));
    // A table Captastic just created would otherwise be written without its `[output]` header,
    // leaving the key at the top level where the strict parser rejects it as unknown.
    if let Some(table) = output.as_table_mut() {
        table.set_implicit(false);
    }
    // Table-like rather than table: a user is entitled to have written `output = { enabled = … }`,
    // and an inline table is still where this setting lives.
    let type_name = output.type_name();
    let table = output.as_table_like_mut().ok_or_else(|| {
        ConfigError::InvalidValue(format!(
            "output must be a table to hold output.enabled, not {type_name}"
        ))
    })?;
    // Assigned through the existing entry where there is one, rather than inserted over it:
    // inserting replaces the key as well as the value, and a comment the user wrote above
    // `enabled` is attached to that key. Replacing it would delete their note as the price of
    // flipping their setting.
    match table.get_mut("enabled") {
        Some(existing) => *existing = value(enabled),
        None => {
            table.insert("enabled", value(enabled));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "captastic-config-edit-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create test directory");
        directory
    }

    #[test]
    fn flipping_the_setting_keeps_comments_and_every_other_section() {
        let directory = temporary_directory("preserves");
        let path = directory.join("captastic.toml");
        let original = "schema_version = 1\n\
             \n\
             # The hotkey I spent an afternoon choosing.\n\
             [hotkey]\n\
             binding = \"ctrl+alt+p\"\n\
             \n\
             [output]\n\
             # Write every capture to disk.\n\
             enabled = false\n\
             format = \"jpeg\"\n\
             jpeg_quality = 72\n";
        fs::write(&path, original).expect("seed configuration");

        set_output_enabled(&path, true).expect("enable file output");

        let updated = fs::read_to_string(&path).expect("read back");
        assert!(updated.contains("enabled = true"), "{updated}");
        assert!(!updated.contains("enabled = false"), "{updated}");
        // Everything the user wrote is still theirs: the comments, the unrelated section, and the
        // two output settings this command has no opinion about.
        assert!(updated.contains("# The hotkey I spent an afternoon choosing."));
        assert!(updated.contains("binding = \"ctrl+alt+p\""));
        assert!(updated.contains("# Write every capture to disk."));
        assert!(updated.contains("format = \"jpeg\""));
        assert!(updated.contains("jpeg_quality = 72"));

        // And it is still a configuration this binary reads, with the flip visible through the
        // strict parser the daemon uses.
        let config = crate::AppConfig::load(&path).expect("the edited file still loads");
        assert!(config.output.enabled);
        assert_eq!(config.output.jpeg_quality, 72);

        set_output_enabled(&path, false).expect("disable file output");
        let config = crate::AppConfig::load(&path).expect("the edited file still loads");
        assert!(!config.output.enabled);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_configuration_without_an_output_section_gains_one() {
        let directory = temporary_directory("section");
        let path = directory.join("captastic.toml");
        fs::write(&path, "schema_version = 1\n\n[clipboard]\nenabled = true\n")
            .expect("seed configuration");

        set_output_enabled(&path, true).expect("enable file output");

        let updated = fs::read_to_string(&path).expect("read back");
        // The header matters: without it the key lands at the top level, where the strict parser
        // would reject it as unknown rather than read it as file output.
        assert!(updated.contains("[output]"), "{updated}");
        assert!(updated.contains("[clipboard]"), "{updated}");
        let config = crate::AppConfig::load(&path).expect("the edited file still loads");
        assert!(config.output.enabled);
        assert!(config.clipboard.enabled);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_missing_configuration_is_created_with_only_this_setting() {
        let directory = temporary_directory("missing");
        let path = directory.join("nested").join("captastic.toml");

        set_output_enabled(&path, true).expect("create the configuration");

        let written = fs::read_to_string(&path).expect("read back");
        assert!(written.contains("schema_version = 1"), "{written}");
        assert!(written.contains("[output]"), "{written}");
        assert!(written.contains("enabled = true"), "{written}");
        // Nothing else is materialized: a default written down is a default frozen at the version
        // that wrote it.
        assert!(!written.contains("[clipboard]"), "{written}");
        assert!(!written.contains("format"), "{written}");
        let config = crate::AppConfig::load(&path).expect("the created file loads");
        assert!(config.output.enabled);
        assert_eq!(config.output.format, crate::OutputFormat::Png);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_shipped_example_configuration_survives_a_round_trip_byte_for_byte() {
        // The one document most likely to be someone's starting point, and the one with the most
        // to lose: 100-odd lines of comments explaining every setting, and three `enabled` keys in
        // different sections that a careless edit could confuse for one another. Enabling and
        // disabling again has to leave it exactly as it shipped.
        let example =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../captastic.example.toml");
        let original = fs::read_to_string(&example).expect("read the shipped example");
        let directory = temporary_directory("example");
        let path = directory.join("captastic.toml");
        fs::write(&path, &original).expect("seed from the example");

        set_output_enabled(&path, true).expect("enable file output");
        let enabled = crate::AppConfig::load(&path).expect("the edited example still loads");
        assert!(enabled.output.enabled);
        // The neighbours that also say `enabled` are untouched.
        assert!(enabled.selection.enabled);
        assert_eq!(enabled.clipboard.enabled, {
            let shipped = crate::AppConfig::load(&example).expect("the example loads");
            shipped.clipboard.enabled
        });

        set_output_enabled(&path, false).expect("disable file output");
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            original,
            "a round trip through the tray toggle must leave the document as it was"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_setting_written_as_a_dotted_key_is_edited_in_place() {
        // `output.enabled = false` at the top level is the same setting in a shape the writer
        // preferred. Flipping it must not rewrite their document into a `[output]` section.
        let directory = temporary_directory("dotted");
        let path = directory.join("captastic.toml");
        fs::write(
            &path,
            "schema_version = 1
# Mine, written this way on purpose.
output.enabled = false
output.format = \"bmp\"
",
        )
        .expect("seed configuration");

        set_output_enabled(&path, true).expect("enable file output");

        let updated = fs::read_to_string(&path).expect("read back");
        assert!(updated.contains("output.enabled = true"), "{updated}");
        assert!(
            !updated.contains("[output]"),
            "the dotted form is the user's choice, not something to normalize: {updated}"
        );
        assert!(updated.contains("# Mine, written this way on purpose."));
        assert!(updated.contains("output.format = \"bmp\""));
        let config = crate::AppConfig::load(&path).expect("the edited file still loads");
        assert!(config.output.enabled);
        assert_eq!(config.output.format, crate::OutputFormat::Bmp);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_setting_written_in_an_inline_table_is_edited_in_place() {
        let directory = temporary_directory("inline");
        let path = directory.join("captastic.toml");
        fs::write(
            &path,
            "schema_version = 1
output = { enabled = false, jpeg_quality = 55 }
",
        )
        .expect("seed configuration");

        set_output_enabled(&path, true).expect("enable file output");

        let updated = fs::read_to_string(&path).expect("read back");
        assert!(updated.contains("enabled = true"), "{updated}");
        assert!(
            updated.contains("jpeg_quality = 55"),
            "the rest of the inline table is theirs: {updated}"
        );
        let config = crate::AppConfig::load(&path).expect("the edited file still loads");
        assert!(config.output.enabled);
        assert_eq!(config.output.jpeg_quality, 55);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_damaged_configuration_is_reported_rather_than_replaced() {
        let directory = temporary_directory("damaged");
        let path = directory.join("captastic.toml");
        fs::write(&path, "schema_version = 1\n[output\nenabled = false\n").expect("seed");

        let error = set_output_enabled(&path, true).expect_err("the file cannot be parsed");

        assert!(matches!(error, ConfigError::Edit(_)), "{error:?}");
        // The user's file is left exactly as it was: this command changes one setting or nothing.
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            "schema_version = 1\n[output\nenabled = false\n"
        );
        let _ = fs::remove_dir_all(&directory);
    }
}
