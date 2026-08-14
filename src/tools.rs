//! Filesystem tools exposed to the reconciliation agent.
//!
//! Every tool is scoped to the wiki directory. The log is never writable and,
//! by default, not even readable through these tools: the agent is handed the
//! events it needs to fold, and letting it wander the log invites it to
//! rediscover facts we did not ask it to reconcile.
//!
//! Path handling is the security-critical part of this file. The agent
//! proposes paths as strings, so every one is resolved against the wiki root
//! and rejected if it escapes.

use anyhow::{bail, Context, Result};
use rmcp::model::Tool;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Records what an agent did, so a run can be summarised and audited.
#[derive(Debug, Default, Clone)]
pub struct Journal {
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub searches: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    pub journal: Journal,
    /// When false, `write_page` and `delete_page` refuse. Used for dry runs.
    pub writable: bool,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            bail!("wiki directory {} does not exist", root.display());
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("could not resolve {}", root.display()))?;
        Ok(Self {
            root,
            journal: Journal::default(),
            writable: true,
        })
    }

    /// Resolve an agent-supplied path against the wiki root.
    ///
    /// Accepts both OKF bundle-absolute form (`/people/ada.md`) and plain
    /// relative form (`people/ada.md`). Rejects anything that escapes the root,
    /// including via `..` or an absolute path outside it. We resolve
    /// lexically rather than with `canonicalize` because the target often does
    /// not exist yet, and a symlink inside the wiki is checked separately.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf> {
        let trimmed = requested.trim();
        if trimmed.is_empty() {
            bail!("path must not be empty");
        }
        let relative = trimmed.strip_prefix('/').unwrap_or(trimmed);
        let mut path = PathBuf::new();
        for component in Path::new(relative).components() {
            match component {
                Component::Normal(part) => path.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    bail!("path `{requested}` escapes the wiki directory")
                }
                Component::RootDir | Component::Prefix(_) => {
                    bail!("path `{requested}` must be relative to the wiki root")
                }
            }
        }
        if path.as_os_str().is_empty() {
            bail!("path `{requested}` does not name a file");
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            bail!("`{requested}` is not a .md file; the wiki contains only markdown");
        }
        let full = self.root.join(&path);

        // A symlink could still point outside the root; refuse to follow any.
        if let Ok(metadata) = fs::symlink_metadata(&full) {
            if metadata.file_type().is_symlink() {
                bail!("`{requested}` is a symlink; refusing to follow it");
            }
        }
        Ok(full)
    }

    pub fn list_pages(&self) -> Result<Vec<String>> {
        let mut pages = Vec::new();
        collect(&self.root, &self.root, &mut pages)?;
        pages.sort();
        Ok(pages)
    }

    pub fn read_page(&mut self, path: &str) -> Result<String> {
        let full = self.resolve(path)?;
        let text = fs::read_to_string(&full)
            .with_context(|| format!("`{path}` does not exist in the wiki"))?;
        self.journal.reads.push(path.to_string());
        Ok(text)
    }

    pub fn write_page(&mut self, path: &str, content: &str) -> Result<()> {
        if !self.writable {
            bail!("this run is read-only; no pages may be written");
        }
        let full = self.resolve(path)?;
        crate::okf::validate(content)
            .with_context(|| format!("refusing to write invalid page `{path}`"))?;
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = content.to_string();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        fs::write(&full, body)?;
        self.journal.writes.push(path.to_string());
        Ok(())
    }

    pub fn delete_page(&mut self, path: &str) -> Result<()> {
        if !self.writable {
            bail!("this run is read-only; no pages may be deleted");
        }
        let full = self.resolve(path)?;
        if !full.exists() {
            bail!("`{path}` does not exist");
        }
        fs::remove_file(&full)?;
        self.journal.writes.push(format!("deleted {path}"));
        Ok(())
    }

    /// Literal, case-insensitive substring search across page text.
    pub fn search(&mut self, query: &str) -> Result<Vec<(String, String)>> {
        if query.trim().is_empty() {
            bail!("search query must not be empty");
        }
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for page in self.list_pages()? {
            let full = self.root.join(page.trim_start_matches('/'));
            let Ok(text) = fs::read_to_string(&full) else {
                continue;
            };
            for line in text.lines() {
                if line.to_lowercase().contains(&needle) {
                    hits.push((page.clone(), line.trim().to_string()));
                    break;
                }
            }
        }
        self.journal.searches.push(query.to_string());
        Ok(hits)
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_symlink() {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            out.push(format!("/{}", relative.to_string_lossy()));
        }
    }
    Ok(())
}

fn schema(properties: Value, required: &[&str]) -> std::sync::Arc<Map<String, Value>> {
    let mut map = Map::new();
    map.insert("type".into(), json!("object"));
    map.insert("properties".into(), properties);
    map.insert("required".into(), json!(required));
    std::sync::Arc::new(map)
}

fn tool(
    name: &'static str,
    description: &'static str,
    input_schema: Value,
    required: &[&str],
) -> Tool {
    Tool::new(name, description, schema(input_schema, required))
}

/// The toolset offered to the reconciliation agent.
pub fn definitions() -> Vec<Tool> {
    vec![
        tool(
            "list_pages",
            "List every page in the wiki, as bundle-absolute paths like /people/ada.md.",
            json!({}),
            &[],
        ),
        tool(
            "search_wiki",
            "Case-insensitive literal search across all pages. Returns matching pages with \
             the first matching line. Use this to find pages that may contradict a new fact.",
            json!({"query": {"type": "string", "description": "Text to search for."}}),
            &["query"],
        ),
        tool(
            "read_page",
            "Read a page's full contents, including its YAML frontmatter.",
            json!({"path": {"type": "string", "description": "Bundle-absolute path, e.g. /people/ada.md"}}),
            &["path"],
        ),
        tool(
            "write_page",
            "Create or overwrite a page. Content must be a complete OKF document: YAML \
             frontmatter with a non-empty `type`, then the markdown body. Overwrites the \
             whole file, so include everything the page should still say.",
            json!({
                "path": {"type": "string", "description": "Bundle-absolute path, e.g. /people/ada.md"},
                "content": {"type": "string", "description": "The complete file contents."}
            }),
            &["path", "content"],
        ),
        tool(
            "delete_page",
            "Delete a page. Rare: prefer rewriting. Use only when a page should not exist.",
            json!({"path": {"type": "string"}}),
            &["path"],
        ),
        tool(
            "finish",
            "Call when the wiki fully reflects the events you were given. Summarise what \
             you changed and why.",
            json!({"summary": {"type": "string", "description": "What changed, and why."}}),
            &["summary"],
        ),
    ]
}

/// Execute a tool call. Returns (text_for_model, is_error).
pub fn dispatch(workspace: &mut Workspace, name: &str, arguments: &Value) -> (String, bool) {
    let argument = |key: &str| -> Result<String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("missing required argument `{key}`"))
    };

    let result: Result<String> = (|| match name {
        "list_pages" => {
            let pages = workspace.list_pages()?;
            Ok(if pages.is_empty() {
                "The wiki is empty; no pages exist yet.".to_string()
            } else {
                pages.join("\n")
            })
        }
        "search_wiki" => {
            let query = argument("query")?;
            let hits = workspace.search(&query)?;
            Ok(if hits.is_empty() {
                format!("No page mentions {query:?}.")
            } else {
                hits.iter()
                    .map(|(page, line)| format!("{page}: {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "read_page" => workspace.read_page(&argument("path")?),
        "write_page" => {
            let path = argument("path")?;
            workspace.write_page(&path, &argument("content")?)?;
            Ok(format!("Wrote {path}."))
        }
        "delete_page" => {
            let path = argument("path")?;
            workspace.delete_page(&path)?;
            Ok(format!("Deleted {path}."))
        }
        other => bail!("unknown tool `{other}`"),
    })();

    match result {
        Ok(text) => (text, false),
        // Tool errors are returned to the model, not raised: a bad path or a
        // malformed page is something it can correct on the next turn.
        Err(error) => (format!("Error: {error:#}"), true),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn workspace() -> (tempdir::Dir, Workspace) {
        let dir = tempdir::Dir::new();
        fs::create_dir_all(dir.path().join("people")).unwrap();
        fs::write(
            dir.path().join("people/ada.md"),
            "---\ntype: Person\n---\n\nOranges are her favourite fruit.\n",
        )
        .unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        (dir, workspace)
    }

    #[test]
    fn rejects_traversal() {
        let (_dir, workspace) = workspace();
        for bad in [
            "../secrets.md",
            "/../secrets.md",
            "people/../../escape.md",
        ] {
            let error = workspace.resolve(bad).unwrap_err().to_string();
            assert!(
                error.contains("escapes") || error.contains("relative"),
                "{bad} was not rejected: {error}"
            );
        }
    }

    /// A leading `/` is OKF bundle-absolute (SPEC §6.1), not filesystem-absolute,
    /// so it must land inside the wiki rather than at the real filesystem root.
    #[test]
    fn bundle_absolute_paths_stay_inside_the_wiki() {
        let (dir, workspace) = workspace();
        let resolved = workspace.resolve("/etc/passwd.md").unwrap();
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn rejects_non_markdown() {
        let (_dir, workspace) = workspace();
        assert!(workspace.resolve("/config.toml").is_err());
        assert!(workspace.resolve("/log/2026-08.jsonl").is_err());
    }

    #[test]
    fn accepts_both_path_forms() {
        let (_dir, workspace) = workspace();
        let a = workspace.resolve("/people/ada.md").unwrap();
        let b = workspace.resolve("people/ada.md").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn lists_and_searches() {
        let (_dir, mut workspace) = workspace();
        assert_eq!(workspace.list_pages().unwrap(), vec!["/people/ada.md"]);
        let hits = workspace.search("ORANGES").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "/people/ada.md");
    }

    #[test]
    fn write_requires_valid_okf() {
        let (_dir, mut workspace) = workspace();
        let bad = workspace.write_page("/people/bob.md", "no frontmatter here");
        assert!(bad.is_err(), "invalid OKF should be refused");
        assert!(!_dir.path().join("people/bob.md").exists());

        workspace
            .write_page("/people/bob.md", "---\ntype: Person\n---\n\nHi.\n")
            .unwrap();
        assert!(_dir.path().join("people/bob.md").exists());
    }

    #[test]
    fn read_only_workspace_refuses_writes() {
        let (_dir, mut workspace) = workspace();
        workspace.writable = false;
        assert!(workspace
            .write_page("/people/bob.md", "---\ntype: Person\n---\n\nHi.\n")
            .is_err());
    }

    #[test]
    fn dispatch_returns_errors_to_the_model() {
        let (_dir, mut workspace) = workspace();
        let (text, is_error) = dispatch(&mut workspace, "read_page", &json!({"path": "/nope.md"}));
        assert!(is_error);
        assert!(text.starts_with("Error:"), "{text}");
    }

    /// Minimal scratch directory helper, so tests need no dev-dependency.
    pub mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU64, Ordering};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                // Include a process-wide counter: two tests running in
                // parallel can otherwise land on the same nanosecond, and the
                // first to finish deletes the other's directory.
                let path = std::env::temp_dir().join(format!(
                    "lifewiki-test-{}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
