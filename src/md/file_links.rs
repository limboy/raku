//! In-memory resolution of inline code spans against a workspace file index.
//! Build alongside file discovery on the background executor; rendering only
//! performs exact hash lookups, including unambiguous path suffixes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub struct FileLinks {
    root: PathBuf,
    exact: HashMap<String, Arc<str>>,
    suffixes: HashMap<String, Option<Arc<str>>>,
}

impl FileLinks {
    pub fn new<'a>(root: &Path, files: impl IntoIterator<Item = &'a str>) -> Self {
        let mut index = Self {
            root: root.to_owned(),
            exact: HashMap::new(),
            suffixes: HashMap::new(),
        };
        for file in files {
            let Ok(target) = url::Url::from_file_path(root.join(file)) else {
                continue;
            };
            let target: Arc<str> = target.as_str().into();
            index.exact.insert(file.to_owned(), target.clone());
            for (offset, _) in file.match_indices('/') {
                index
                    .suffixes
                    .entry(file[offset + 1..].to_owned())
                    .and_modify(|existing| {
                        if existing.as_ref() != Some(&target) {
                            *existing = None;
                        }
                    })
                    .or_insert_with(|| Some(target.clone()));
            }
        }
        index
    }

    pub fn resolve(&self, reference: &str) -> Option<String> {
        let reference = reference.trim();
        let path = strip_file_location(reference);
        let location = &reference[path.len()..];
        let path = if Path::new(path).is_absolute() {
            Path::new(path).strip_prefix(&self.root).ok()?.to_str()?
        } else {
            path.strip_prefix("./").unwrap_or(path)
        };
        let target = self
            .exact
            .get(path)
            .or_else(|| self.suffixes.get(path)?.as_ref())?;
        Some(format!("{target}{location}"))
    }
}

fn positive_number(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<usize>().is_ok_and(|value| value > 0)
}

fn line_fragment(fragment: &str) -> bool {
    let Some(location) = fragment.strip_prefix('L') else {
        return false;
    };
    match location.split_once('C') {
        Some((line, column)) => positive_number(line) && positive_number(column),
        None => positive_number(location),
    }
}

/// The compact editor does not consume locations yet, but they must not
/// become part of the filesystem path when a provider includes one.
pub fn strip_file_location(target: &str) -> &str {
    if let Some((path, fragment)) = target.rsplit_once('#')
        && line_fragment(fragment)
    {
        return path;
    }
    let Some((before_last, last)) = target.rsplit_once(':') else {
        return target;
    };
    if !positive_number(last) {
        return target;
    }
    if let Some((path, line)) = before_last.rsplit_once(':')
        && positive_number(line)
    {
        path
    } else {
        before_last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_exact_absolute_and_unique_shortened_paths() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let index = FileLinks::new(
            root,
            [
                "src/lib/utils.server.ts",
                "src/pages/articles/new.astro",
                "src/pages/articles/drafts/[draftId].astro",
                "docs/My Notes.md",
                "docs/配额%20#notes.md",
                "README.md",
            ],
        );
        for (reference, file) in [
            ("src/lib/utils.server.ts", "src/lib/utils.server.ts"),
            ("./src/lib/utils.server.ts", "src/lib/utils.server.ts"),
            ("utils.server.ts", "src/lib/utils.server.ts"),
            ("articles/new.astro", "src/pages/articles/new.astro"),
            (
                "articles/drafts/[draftId].astro",
                "src/pages/articles/drafts/[draftId].astro",
            ),
            ("My Notes.md", "docs/My Notes.md"),
            ("配额%20#notes.md", "docs/配额%20#notes.md"),
            ("README.md", "README.md"),
        ] {
            let absolute = root.join(file);
            let target = url::Url::from_file_path(&absolute).unwrap().to_string();
            assert_eq!(
                index.resolve(reference),
                Some(target.clone()),
                "{reference}"
            );
            assert_eq!(index.resolve(&absolute.to_string_lossy()), Some(target));
        }
        for location in [":12", ":12:4", "#L12", "#L12C4"] {
            assert_eq!(
                index.resolve(&format!("utils.server.ts{location}")),
                Some(format!(
                    "{}{location}",
                    url::Url::from_file_path(root.join("src/lib/utils.server.ts")).unwrap()
                ))
            );
        }
    }

    #[test]
    fn exact_paths_win_and_ambiguous_or_unknown_references_stay_plain() {
        let index = FileLinks::new(
            Path::new("/workspace"),
            [
                "README.md",
                "docs/README.md",
                "src/a/index.ts",
                "src/b/index.ts",
            ],
        );
        assert_eq!(
            index.resolve("README.md"),
            Some("file:///workspace/README.md".into())
        );
        assert_eq!(
            index.resolve("a/index.ts"),
            Some("file:///workspace/src/a/index.ts".into())
        );
        for reference in [
            "index.ts",
            "missing.ts",
            "remainingPosts",
            "getTodayRemainingPostCount",
            "https://example.com/README.md",
            "../README.md",
            "/another/workspace/README.md",
            "README.md:0",
            "README.md#heading",
            "cat README.md",
        ] {
            assert_eq!(index.resolve(reference), None, "{reference}");
        }
    }
}
