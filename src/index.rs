//! Generating `wiki/index.md` (OKF SPEC §8).
//!
//! Deterministic on purpose. The index is a mechanical listing of titles and
//! descriptions that already exist in each page's frontmatter, so asking a
//! model to write it would spend money and introduce drift for no benefit —
//! and the spec explicitly permits producers to generate it automatically.
//!
//! It exists for **progressive disclosure**: letting a reader, or the agent on
//! its next run, see what the wiki contains before opening anything. That makes
//! it part of the retrieval story, not just a courtesy for humans.

use crate::okf;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// The bundle-root index filename.
pub const FILENAME: &str = "index.md";

/// Regenerate `<wiki>/index.md` from the pages on disk.
pub fn generate(wiki: &Path) -> Result<String> {
    let mut sections: BTreeMap<String, Vec<Entry>> = BTreeMap::new();

    for path in okf::pages(wiki)? {
        let relative = okf::bundle_path(wiki, &path)
            .trim_start_matches('/')
            .to_string();
        if relative == FILENAME {
            continue;
        }

        let raw = std::fs::read_to_string(&path)?;
        // A page that doesn't parse still deserves a listing: omitting it would
        // hide the problem from exactly the reader most able to fix it.
        let (title, description) = match okf::parse(&raw) {
            Ok(page) => (
                page.title().map(str::to_string),
                page.description().map(str::to_string),
            ),
            Err(_) => (None, None),
        };

        let heading = match relative.rsplit_once('/') {
            Some((directory, _)) => directory.to_string(),
            None => String::new(),
        };

        sections.entry(heading).or_default().push(Entry {
            title: title.unwrap_or_else(|| stem(&relative)),
            link: format!("/{relative}"),
            description,
        });
    }

    let mut out = String::from("# lifewiki\n");
    if sections.is_empty() {
        out.push_str("\nThis wiki has no pages yet. Add entries with `lw add`, then `lw build`.\n");
        return Ok(out);
    }

    // Root-level pages first, then directories alphabetically: the shallowest
    // things are the most likely entry points.
    for (heading, mut entries) in sections {
        entries.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
        let label = if heading.is_empty() {
            "Pages".to_string()
        } else {
            heading.clone()
        };
        out.push_str(&format!("\n# {label}\n\n"));
        for entry in entries {
            match &entry.description {
                Some(description) if !description.trim().is_empty() => out.push_str(&format!(
                    "* [{}]({}) - {}\n",
                    entry.title,
                    entry.link,
                    description.trim()
                )),
                _ => out.push_str(&format!("* [{}]({})\n", entry.title, entry.link)),
            }
        }
    }
    Ok(out)
}

/// Write `<wiki>/index.md`, returning how many pages it lists.
pub fn write(wiki: &Path) -> Result<usize> {
    let content = generate(wiki)?;
    std::fs::write(wiki.join(FILENAME), &content)?;
    Ok(content
        .lines()
        .filter(|line| line.starts_with("* ["))
        .count())
}

struct Entry {
    title: String,
    link: String,
    description: Option<String>,
}

fn stem(relative: &str) -> String {
    relative
        .rsplit('/')
        .next()
        .unwrap_or(relative)
        .trim_end_matches(".md")
        .replace(['-', '_'], " ")
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests::tempdir::Dir;

    fn page(dir: &Dir, path: &str, title: &str, description: &str) {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(
            full,
            format!(
                "---\ntype: Person\ntitle: {title}\ndescription: {description}\n---\n\nBody.\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn lists_pages_grouped_by_directory() {
        let dir = Dir::new();
        page(&dir, "people/ada.md", "Ada", "The owner's sister.");
        page(&dir, "places/porto.md", "Porto", "Where Ada lives.");

        let index = generate(dir.path()).unwrap();
        assert!(index.contains("# people\n"), "{index}");
        assert!(
            index.contains("* [Ada](/people/ada.md) - The owner's sister."),
            "{index}"
        );
        assert!(
            index.contains("* [Porto](/places/porto.md) - Where Ada lives."),
            "{index}"
        );
    }

    #[test]
    fn does_not_list_itself() {
        let dir = Dir::new();
        page(&dir, "people/ada.md", "Ada", "Sister.");
        write(dir.path()).unwrap();
        // Regenerate now that index.md exists on disk.
        let index = generate(dir.path()).unwrap();
        assert!(!index.contains("](/index.md)"), "{index}");
    }

    #[test]
    fn falls_back_to_the_filename_when_a_page_has_no_title() {
        let dir = Dir::new();
        std::fs::write(
            dir.path().join("no-title.md"),
            "---\ntype: Reference\n---\n\nBody.\n",
        )
        .unwrap();
        let index = generate(dir.path()).unwrap();
        assert!(index.contains("* [no title](/no-title.md)"), "{index}");
    }

    /// A page that fails to parse must still appear: hiding it would conceal
    /// the problem from the person able to fix it.
    #[test]
    fn lists_unparseable_pages_too() {
        let dir = Dir::new();
        std::fs::write(dir.path().join("broken.md"), "no frontmatter here\n").unwrap();
        let index = generate(dir.path()).unwrap();
        assert!(index.contains("](/broken.md)"), "{index}");
    }

    #[test]
    fn an_empty_wiki_says_so() {
        let dir = Dir::new();
        let index = generate(dir.path()).unwrap();
        assert!(index.contains("no pages yet"), "{index}");
    }

    #[test]
    fn skips_dot_directories() {
        let dir = Dir::new();
        page(&dir, "people/ada.md", "Ada", "Sister.");
        std::fs::create_dir_all(dir.path().join(".obsidian")).unwrap();
        std::fs::write(dir.path().join(".obsidian/workspace.md"), "state\n").unwrap();
        let index = generate(dir.path()).unwrap();
        assert!(!index.contains("obsidian"), "{index}");
    }

    #[test]
    fn write_reports_the_page_count() {
        let dir = Dir::new();
        page(&dir, "people/ada.md", "Ada", "Sister.");
        page(&dir, "people/me.md", "Me", "The owner.");
        assert_eq!(write(dir.path()).unwrap(), 2);
    }
}

/// A dangling internal link: a page links to a page that does not exist.
pub struct Dangling {
    pub from: String,
    pub to: String,
}

/// Find links pointing at pages that don't exist.
///
/// Checked across the whole wiki after a run rather than at write time: the
/// agent legitimately links to a page it is about to create, so a per-write
/// check would reject correct work. That means dangling links are reported, not
/// prevented — the tradeoff is deliberate.
pub fn dangling_links(wiki: &Path) -> Result<Vec<Dangling>> {
    let files = okf::pages(wiki)?;
    let existing: std::collections::HashSet<String> = files
        .iter()
        .map(|path| okf::bundle_path(wiki, path))
        .collect();

    let mut dangling = Vec::new();
    for path in &files {
        let from = okf::bundle_path(wiki, path);
        let raw = std::fs::read_to_string(path)?;
        let body = match okf::parse(&raw) {
            Ok(page) => page.body,
            // An unparseable page is a different problem, reported elsewhere.
            Err(_) => continue,
        };
        for target in okf::page_links(&body) {
            if !existing.contains(&target) {
                dangling.push(Dangling {
                    from: from.clone(),
                    to: target,
                });
            }
        }
    }
    Ok(dangling)
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use crate::tools::tests::tempdir::Dir;

    fn write_page(dir: &Dir, path: &str, body: &str) {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, format!("---\ntype: Person\n---\n\n{body}\n")).unwrap();
    }

    #[test]
    fn finds_a_link_to_a_missing_page() {
        let dir = Dir::new();
        write_page(&dir, "people/ada.md", "Sister of [Me](/people/me.md).");
        let dangling = dangling_links(dir.path()).unwrap();
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].from, "/people/ada.md");
        assert_eq!(dangling[0].to, "/people/me.md");
    }

    #[test]
    fn accepts_links_that_resolve() {
        let dir = Dir::new();
        write_page(&dir, "people/ada.md", "Sister of [Me](/people/me.md).");
        write_page(&dir, "people/me.md", "The owner.");
        assert!(dangling_links(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn ignores_external_links_and_anchors() {
        let dir = Dir::new();
        write_page(
            &dir,
            "a.md",
            "See [docs](https://example.com/x.md) and [top](#heading).",
        );
        assert!(dangling_links(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn extracts_links_from_a_body() {
        let links = okf::page_links("A [b](/c.md) and [d](/e/f.md), plus [g](https://h.md).");
        assert_eq!(links, vec!["/c.md", "/e/f.md"]);
    }
}

