use std::path::{Component, Path};

pub(crate) fn matches_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn safe_relative_file(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && !path.split('/').any(|part| matches!(part, "." | ".."))
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

// Called only after validated_request_path rejects traversal and invalid UTF-8.
pub(crate) fn normalized_path(path: &str) -> String {
    let mut result = String::from("/");
    result.push_str(
        &path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/"),
    );
    if path.ends_with('/') && result != "/" {
        result.push('/');
    }
    result
}

pub(crate) fn denied(path: &str, paths: &[String], files: &[String]) -> bool {
    path.split('/').any(|part| {
        part.starts_with('.')
            || files.iter().any(|pattern| {
                if let Some((start, end)) = pattern.split_once('*') {
                    part.len() >= start.len() + end.len()
                        && part.starts_with(start)
                        && part.ends_with(end)
                } else {
                    part == pattern
                }
            })
    }) || paths.iter().any(|prefix| matches_prefix(path, prefix))
}
