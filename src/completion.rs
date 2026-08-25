//! Working-directory completion shared by the TUI's new-session dialog and the web UI's
//! `/api/fs/complete` endpoint. Both surfaces must offer the same candidates -- including the
//! same `~` handling -- so there is exactly one implementation here rather than one per caller.

use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn workspace_directory_completions(value: &str, home: Option<&Path>) -> Vec<String> {
    let (lookup, tilde) = if let Some(rest) = value.strip_prefix("~/") {
        let Some(home) = home else {
            return Vec::new();
        };
        (home.join(rest), Some(home))
    } else {
        (PathBuf::from(value), None)
    };
    let ends_with_separator = value.ends_with(std::path::MAIN_SEPARATOR);
    let (parent, prefix) = if ends_with_separator {
        (lookup.as_path(), "")
    } else {
        (
            lookup.parent().unwrap_or_else(|| Path::new(".")),
            lookup
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
        )
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.starts_with(prefix).then(|| {
                let path = parent.join(name);
                let display = tilde.map_or_else(
                    || path.display().to_string(),
                    |home| {
                        path.strip_prefix(home).map_or_else(
                            |_| path.display().to_string(),
                            |rest| format!("~/{}", rest.display()),
                        )
                    },
                );
                format!("{display}{}", std::path::MAIN_SEPARATOR)
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|value| value.to_lowercase());
    matches
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn workspace_completion_lists_only_matching_directories_and_preserves_tilde() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("alpha-one")).unwrap();
        fs::create_dir_all(root.path().join("alpha-two")).unwrap();
        fs::write(root.path().join("alpha-file"), "not a directory").unwrap();

        let absolute = format!("{}/alpha", root.path().display());
        assert_eq!(
            workspace_directory_completions(&absolute, Some(root.path())),
            vec![
                format!("{}/alpha-one/", root.path().display()),
                format!("{}/alpha-two/", root.path().display()),
            ]
        );
        assert_eq!(
            workspace_directory_completions("~/alpha", Some(root.path())),
            vec!["~/alpha-one/", "~/alpha-two/"]
        );
    }

    #[test]
    fn a_tilde_path_without_a_known_home_completes_to_nothing() {
        assert!(workspace_directory_completions("~/anything", None).is_empty());
    }
}
