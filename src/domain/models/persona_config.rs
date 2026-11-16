//! Story C-2 — the `[persona] config = { file = "…" }` schema.
//!
//! Pure validation of the raw `config` value (no I/O): the file is read later
//! by `adapters::persona_file::load_persona_file`. `file` is the only key.

use thiserror::Error;

/// A validated persona-file reference, relative to `<config_dir>/personas/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaFileConfig {
    /// The raw string as written in the profile (AD-2 rule already applied).
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PersonaConfigError {
    #[error("[persona] config must be a table such as `{{ file = \"devops.md\" }}`")]
    NotATable,
    #[error(
        "[persona] config key '{0}' is not read; the only supported key is `file` \
         (remove it or use `config = {{ file = \"devops.md\" }}`)"
    )]
    UnknownKey(String),
    #[error("[persona] config.file must be a string path relative to the personas directory")]
    FileNotString,
    #[error("[persona] config.file {file:?} is not allowed: {reason}")]
    FileInvalid { file: String, reason: &'static str },
}

impl PersonaFileConfig {
    /// Validate a persona `config` value. `None` and an empty table mean "no file".
    pub fn from_config(
        config: Option<&toml::Value>,
    ) -> Result<Option<PersonaFileConfig>, PersonaConfigError> {
        let Some(value) = config else {
            return Ok(None);
        };
        let table = value.as_table().ok_or(PersonaConfigError::NotATable)?;
        if let Some(key) = table.keys().find(|k| k.as_str() != "file") {
            return Err(PersonaConfigError::UnknownKey(key.clone()));
        }
        let Some(file) = table.get("file") else {
            return Ok(None);
        };
        let file = file.as_str().ok_or(PersonaConfigError::FileNotString)?;
        validate_relative_path(file)?;
        Ok(Some(PersonaFileConfig {
            file: file.to_string(),
        }))
    }

    /// The `/`-separated segments; the last one is the file name.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.file.split('/')
    }
}

/// AD-2: validate the raw string, not `Path::components()`, which would
/// normalise `.` and `//` away.
fn validate_relative_path(file: &str) -> Result<(), PersonaConfigError> {
    let refuse = |reason: &'static str| PersonaConfigError::FileInvalid {
        file: file.to_string(),
        reason,
    };
    if file.is_empty() {
        return Err(refuse("it is empty"));
    }
    if file.starts_with('/') {
        return Err(refuse("it is absolute"));
    }
    if file.contains('\\') {
        return Err(refuse("it contains a backslash"));
    }
    if file.contains('\0') {
        return Err(refuse("it contains a NUL byte"));
    }
    for segment in file.split('/') {
        match segment {
            "" => return Err(refuse("it has an empty path segment")),
            "." | ".." => return Err(refuse("it has a `.` or `..` path segment")),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> toml::Value {
        src.parse::<toml::Value>().expect("fixture toml")
    }

    fn config(inline: &str) -> toml::Value {
        let doc = parse(&format!("config = {inline}"));
        doc.get("config").expect("config key").clone()
    }

    #[test]
    fn absent_and_empty_config_mean_no_file() {
        assert_eq!(PersonaFileConfig::from_config(None), Ok(None));
        let empty = config("{}");
        assert_eq!(PersonaFileConfig::from_config(Some(&empty)), Ok(None));
    }

    #[test]
    fn accepts_plain_and_nested_relative_files() {
        for file in ["devops.md", "team/devops.md", "a/b/c.md", "..hidden.md"] {
            let value = config(&format!("{{ file = {file:?} }}"));
            let got = PersonaFileConfig::from_config(Some(&value))
                .unwrap_or_else(|e| panic!("{file} refused: {e}"))
                .expect("some");
            assert_eq!(got.file, file);
        }
    }

    #[test]
    fn segments_split_on_slash() {
        let cfg = PersonaFileConfig {
            file: "team/devops.md".into(),
        };
        assert_eq!(
            cfg.segments().collect::<Vec<_>>(),
            vec!["team", "devops.md"]
        );
    }

    #[test]
    fn refuses_every_rule_breaking_path_with_the_path_in_the_message() {
        let cases: [(&str, &str); 9] = [
            ("", "empty"),
            ("/etc/passwd", "absolute"),
            ("../x.md", "`..`"),
            ("a/../b.md", "`..`"),
            ("./a.md", "`.`"),
            ("a/./b.md", "`.`"),
            ("a//b.md", "empty path segment"),
            ("a/", "empty path segment"),
            ("a\\b.md", "backslash"),
        ];
        for (file, needle) in cases {
            let value = config(&format!("{{ file = {file:?} }}"));
            let err = PersonaFileConfig::from_config(Some(&value))
                .expect_err(&format!("{file:?} must be refused"));
            let text = err.to_string();
            assert!(text.contains(needle), "{file:?}: {text}");
            assert!(matches!(err, PersonaConfigError::FileInvalid { .. }));
        }
    }

    #[test]
    fn refuses_nul_byte() {
        let value = toml::Value::Table(toml::map::Map::from_iter([(
            "file".to_string(),
            toml::Value::String("a\0b.md".into()),
        )]));
        assert!(matches!(
            PersonaFileConfig::from_config(Some(&value)),
            Err(PersonaConfigError::FileInvalid { .. })
        ));
    }

    #[test]
    fn refuses_unknown_keys_alone_or_beside_file() {
        for inline in [
            "{ tone = \"concise\" }",
            "{ file = \"a.md\", tone = \"x\" }",
        ] {
            let value = config(inline);
            assert_eq!(
                PersonaFileConfig::from_config(Some(&value)),
                Err(PersonaConfigError::UnknownKey("tone".into())),
                "{inline}"
            );
        }
    }

    #[test]
    fn refuses_non_table_and_non_string_file() {
        let not_table = config("\"devops.md\"");
        assert_eq!(
            PersonaFileConfig::from_config(Some(&not_table)),
            Err(PersonaConfigError::NotATable)
        );
        let int_file = config("{ file = 3 }");
        assert_eq!(
            PersonaFileConfig::from_config(Some(&int_file)),
            Err(PersonaConfigError::FileNotString)
        );
    }
}
