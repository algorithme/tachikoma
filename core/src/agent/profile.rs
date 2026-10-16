//! Runtime profile loading; file contents become resolved configuration snapshots.

use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{AgentConfig, AgentError, OutputSchemaSource, PromptSource};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    system_prompt: Source<String>,
    tools: BTreeMap<String, Map<String, Value>>,
    terminal_tools: Vec<String>,
    max_turns: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source<T> {
    Inline(T),
    File(PathBuf),
}

impl AgentConfig {
    /// Load a JSON profile, resolving file sources relative to the profile's directory.
    /// The Answer schema is read from `tools.answer.output_schema`; other tool options
    /// remain metadata and do not change tool registration or execution.
    pub fn from_profile_file(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        let path = path.as_ref();
        let content = read_file(path, "Could not read the agent profile file.")?;
        let profile: Profile = serde_json::from_str(&content).map_err(|_| {
            AgentError::Configuration(
                "The agent profile is not valid JSON with the required fields.",
            )
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let schema = profile
            .tools
            .get("answer")
            .and_then(|options| options.get("output_schema"))
            .ok_or(AgentError::Configuration(
                "The agent profile requires tools.answer.output_schema.",
            ))?;
        let schema: Source<Map<String, Value>> = serde_json::from_value(schema.clone())
            .map_err(|_| AgentError::Configuration(
                "tools.answer.output_schema must contain exactly one inline schema object or file path.",
            ))?;
        let prompt = match profile.system_prompt {
            Source::Inline(text) => text,
            Source::File(file) => {
                read_file(&base.join(file), "Could not read the system_prompt file.")?
            }
        };
        let schema = match schema {
            Source::Inline(schema) => schema,
            Source::File(file) => serde_json::from_str(&read_file(
                &base.join(file),
                "Could not read the Answer output_schema file.",
            )?)
            .map_err(|_| {
                AgentError::Configuration(
                    "The Answer output_schema file must contain a JSON object.",
                )
            })?,
        };
        Ok(Self {
            system_prompt: PromptSource::Inline(prompt),
            output_schema: OutputSchemaSource::Inline(Value::Object(schema)),
            tools: profile.tools,
            terminal_tools: profile.terminal_tools,
            max_turns: profile.max_turns,
        })
    }
}

fn read_file(path: &Path, message: &'static str) -> Result<String, AgentError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| AgentError::Configuration(message))?;
    if !metadata.is_file() {
        return Err(AgentError::Configuration(
            "Agent profile files and sources must be regular files, not symbolic links.",
        ));
    }
    fs::read_to_string(path).map_err(|_| AgentError::Configuration(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("tachikoma-profile-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn load(&self, profile: &Value) -> Result<AgentConfig, AgentError> {
            let path = self.0.join("agent.json");
            fs::write(&path, profile.to_string()).unwrap();
            AgentConfig::from_profile_file(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn profile() -> Value {
        json!({
            "system_prompt": {"inline": "missing-prompt.md"},
            "tools": {"answer": {"output_schema": {"inline": {"type": "object"}}},
                "future_tool": {"nested": {"enabled": true, "values": [1, "two"]}}},
            "terminal_tools": ["answer"], "max_turns": 10
        })
    }

    #[test]
    fn sources_resolve_from_the_profile_directory_and_remain_snapshots() {
        let dir = Fixture::new();
        let mut profile = profile();
        let inline = dir.load(&profile).unwrap();
        assert!(
            matches!(inline.system_prompt, PromptSource::Inline(text) if text == "missing-prompt.md")
        );
        assert_eq!(
            serde_json::to_value(inline.tools).unwrap(),
            profile["tools"]
        );
        fs::write(dir.0.join("prompt.md"), "First prompt").unwrap();
        fs::write(dir.0.join("schema.json"), r#"{"type":"object"}"#).unwrap();
        profile["system_prompt"] = json!({"file": "prompt.md"});
        profile["tools"]["answer"]["output_schema"] = json!({"file": dir.0.join("schema.json")});
        let loaded = dir.load(&profile).unwrap();
        assert!(
            matches!(loaded.output_schema, OutputSchemaSource::Inline(schema) if schema == json!({"type": "object"}))
        );
        fs::write(dir.0.join("prompt.md"), "Second prompt").unwrap();
        assert!(
            matches!(loaded.system_prompt, PromptSource::Inline(text) if text == "First prompt")
        );
        assert!(
            matches!(dir.load(&profile).unwrap().system_prompt, PromptSource::Inline(text) if text == "Second prompt")
        );
    }

    #[test]
    fn malformed_profiles_and_sources_fail_without_fallback() {
        let dir = Fixture::new();
        for (pointer, invalid) in [
            (
                "/system_prompt",
                json!({"inline": "text", "file": "prompt.md"}),
            ),
            ("/system_prompt", json!({"unknown": "text"})),
            ("/system_prompt", json!("text")),
            (
                "/tools/answer/output_schema",
                json!({"inline": {}, "file": "schema.json"}),
            ),
            ("/tools/answer/output_schema", json!({"inline": false})),
            ("/tools/answer/output_schema", json!({"file": 42})),
            ("/tools/answer", json!({})),
            ("/tools", json!({})),
            ("/tools/future_tool", json!([])),
            ("/max_turns", json!(-1)),
        ] {
            let mut value = profile();
            *value.pointer_mut(pointer).unwrap() = invalid;
            assert!(
                dir.load(&value).is_err(),
                "accepted invalid field {pointer}"
            );
        }
        for missing in ["system_prompt", "tools", "terminal_tools", "max_turns"] {
            let mut value = profile();
            value.as_object_mut().unwrap().remove(missing);
            assert!(dir.load(&value).is_err());
        }
        let mut legacy = profile();
        legacy["output_schema"] = json!({"inline": {"type": "object"}});
        assert!(dir.load(&legacy).is_err());
        fs::write(dir.0.join("agent.json"), "{private invalid JSON").unwrap();
        let error = AgentConfig::from_profile_file(dir.0.join("agent.json"))
            .err()
            .unwrap();
        assert!(!error.to_string().contains("private"));
    }

    #[test]
    fn unavailable_non_regular_and_invalid_source_files_fail() {
        let dir = Fixture::new();
        assert!(AgentConfig::from_profile_file(dir.0.join("missing.json")).is_err());
        fs::write(dir.0.join("invalid-utf8"), [0xff]).unwrap();
        for field in ["/system_prompt", "/tools/answer/output_schema"] {
            for file in ["missing", ".", "invalid-utf8"] {
                let mut value = profile();
                *value.pointer_mut(field).unwrap() = json!({"file": file});
                assert!(dir.load(&value).is_err());
            }
        }
        let mut value = profile();
        value["tools"]["answer"]["output_schema"] = json!({"file": "schema.json"});
        for contents in ["{invalid", "[]", "true"] {
            fs::write(dir.0.join("schema.json"), contents).unwrap();
            assert!(dir.load(&value).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_profile_and_source_symlinks() {
        let dir = Fixture::new();
        dir.load(&profile()).unwrap();
        std::os::unix::fs::symlink("agent.json", dir.0.join("link")).unwrap();
        assert!(AgentConfig::from_profile_file(dir.0.join("link")).is_err());
        for field in ["/system_prompt", "/tools/answer/output_schema"] {
            let mut value = profile();
            *value.pointer_mut(field).unwrap() = json!({"file": "link"});
            assert!(dir.load(&value).is_err());
        }
    }
}
