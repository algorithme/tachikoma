//! Immutable prompt snapshots loaded once from an explicit directory.

use std::{collections::HashMap, path::Path, sync::Arc};

use crate::agent::{AgentError, PromptSource};

use super::registry_files::read_resources;

pub struct PromptRegistry {
    prompts: HashMap<String, Arc<str>>,
    prefix: Arc<str>,
}

impl PromptRegistry {
    pub fn from_directory(
        path: impl AsRef<Path>,
        prefix_source: PromptSource,
    ) -> Result<Self, AgentError> {
        let mut prompts = HashMap::new();
        for (id, content) in read_resources(path.as_ref(), "md")? {
            if content.trim().is_empty() {
                return Err(AgentError::Configuration("A registered prompt is empty."));
            }
            prompts.insert(id, Arc::from(content));
        }
        let mut registry = Self {
            prompts,
            prefix: Arc::from(""),
        };
        registry.prefix = registry.resolve(&prefix_source)?;
        Ok(registry)
    }

    pub fn resolve(&self, source: &PromptSource) -> Result<Arc<str>, AgentError> {
        match source {
            PromptSource::Reference(id) => {
                self.prompts
                    .get(id)
                    .cloned()
                    .ok_or(AgentError::Configuration(
                        "The prompt reference is unknown.",
                    ))
            }
            PromptSource::Inline(content) => {
                if content.trim().is_empty() {
                    return Err(AgentError::Configuration("The inline prompt is empty."));
                }
                Ok(Arc::from(content.as_str()))
            }
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::service::registry_files::tests::TestDirectory;

    #[test]
    fn resolves_shared_snapshots_and_an_inline_prompt() {
        let dir = TestDirectory::new();
        dir.write("prefix.md", "Follow the protocol.");
        dir.write("helper.md", "Help the user.");
        let registry =
            PromptRegistry::from_directory(&dir.0, PromptSource::Reference("prefix".into()))
                .unwrap();
        let source = PromptSource::Reference("helper".into());
        let first = registry.resolve(&source).unwrap();
        dir.write("helper.md", "Changed on disk.");
        let second = registry.resolve(&source).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(&*second, "Help the user.");
        assert_eq!(registry.prefix(), "Follow the protocol.");
        assert_eq!(
            &*registry
                .resolve(&PromptSource::Inline("Custom instructions.".into()))
                .unwrap(),
            "Custom instructions."
        );
        assert!(
            registry
                .resolve(&PromptSource::Reference("../helper".into()))
                .is_err()
        );
        assert!(
            registry
                .resolve(&PromptSource::Inline(" \n".into()))
                .is_err()
        );
    }

    #[test]
    fn invalid_resources_and_missing_prefix_fail_during_loading() {
        let dir = TestDirectory::new();
        assert!(
            PromptRegistry::from_directory(&dir.0, PromptSource::Reference("missing".into()))
                .is_err()
        );
        dir.write("empty.md", " \n");
        assert!(
            PromptRegistry::from_directory(&dir.0, PromptSource::Inline("Prefix".into())).is_err()
        );
        assert!(
            PromptRegistry::from_directory(
                dir.0.join("missing-directory"),
                PromptSource::Inline("Prefix".into())
            )
            .is_err()
        );
    }
}
