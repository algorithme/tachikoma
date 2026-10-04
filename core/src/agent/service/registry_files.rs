//! Startup-only loading of explicitly selected resource directories.

use std::{fs, path::Path};

use crate::agent::AgentError;

pub(super) fn read_resources(
    directory: &Path,
    extension: &str,
) -> Result<Vec<(String, String)>, AgentError> {
    let entries = fs::read_dir(directory)
        .map_err(|_| AgentError::Configuration("Could not read the resource directory."))?;
    let mut resources = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|_| AgentError::Configuration("Could not read a resource directory entry."))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some(extension) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|_| AgentError::Configuration("Could not inspect a resource file."))?;
        if file_type.is_symlink() {
            return Err(AgentError::Configuration(
                "Resource files must not be symbolic links.",
            ));
        }
        if !file_type.is_file() {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .ok_or(AgentError::Configuration("Invalid resource identifier."))?
            .to_owned();
        let content = fs::read_to_string(path)
            .map_err(|_| AgentError::Configuration("Could not read a UTF-8 resource file."))?;
        resources.push((id, content));
    }
    Ok(resources)
}

#[cfg(test)]
pub(super) mod tests {
    use std::{fs, path::PathBuf};

    use super::read_resources;

    pub(crate) struct TestDirectory(pub(crate) PathBuf);

    impl TestDirectory {
        pub(crate) fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("murmeli-registry-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        pub(crate) fn write(&self, name: &str, content: &str) {
            fs::write(self.0.join(name), content).unwrap();
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_only_top_level_files_of_the_requested_kind() {
        let dir = TestDirectory::new();
        dir.write("prompt.md", "Instructions");
        dir.write("ignore.json", "{}");
        fs::create_dir(dir.0.join("nested")).unwrap();
        fs::write(dir.0.join("nested/hidden.md"), "Hidden").unwrap();
        assert_eq!(
            read_resources(&dir.0, "md").unwrap(),
            vec![("prompt".to_owned(), "Instructions".to_owned())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_link_resources() {
        let dir = TestDirectory::new();
        let outside = TestDirectory::new();
        outside.write("secret.md", "Private content");
        std::os::unix::fs::symlink(outside.0.join("secret.md"), dir.0.join("linked.md")).unwrap();
        assert!(read_resources(&dir.0, "md").is_err());
    }
}
