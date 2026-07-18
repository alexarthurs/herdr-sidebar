//! Filesystem tree model: which directories are expanded, and the flat list of
//! visible rows the UI renders. Directory listings are cached and re-read only on
//! explicit refresh, so redraws never touch the disk.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// One visible line of the tree, in render order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub expanded: bool,
}

pub struct Tree {
    root: PathBuf,
    expanded: BTreeSet<PathBuf>,
    cache: HashMap<PathBuf, Vec<Entry>>,
    pub show_hidden: bool,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            expanded: BTreeSet::new(),
            cache: HashMap::new(),
            show_hidden: true,
        }
    }

    /// The workspace root directory the tree is rooted at.
    pub fn root_path(&self) -> PathBuf {
        self.root.clone()
    }

    /// Display name for the header: the folder's own name, or the full path for
    /// roots like `C:\` that have no final component.
    pub fn root_name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Drop all cached listings; the next `rows()` re-reads the disk.
    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub fn expand(&mut self, path: &Path) {
        self.expanded.insert(path.to_path_buf());
    }

    pub fn collapse(&mut self, path: &Path) {
        self.expanded.remove(path);
    }

    pub fn toggle(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
    }

    fn children(&mut self, dir: &Path) -> Vec<Entry> {
        if let Some(cached) = self.cache.get(dir) {
            return cached.clone();
        }
        let mut entries: Vec<Entry> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| Entry {
                        is_dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                        name: e.file_name().to_string_lossy().into_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        sort_entries(&mut entries);
        self.cache.insert(dir.to_path_buf(), entries.clone());
        entries
    }

    /// The visible rows, depth-first through expanded directories.
    pub fn rows(&mut self) -> Vec<Row> {
        let mut out = Vec::new();
        let root = self.root.clone();
        self.walk(&root, 0, &mut out);
        out
    }

    fn walk(&mut self, dir: &Path, depth: usize, out: &mut Vec<Row>) {
        let show_hidden = self.show_hidden;
        for entry in self.children(dir) {
            if !visible(&entry.name, show_hidden) {
                continue;
            }
            let path = dir.join(&entry.name);
            let expanded = entry.is_dir && self.is_expanded(&path);
            out.push(Row {
                name: entry.name,
                is_dir: entry.is_dir,
                depth,
                expanded,
                path: path.clone(),
            });
            if expanded {
                self.walk(&path, depth + 1, out);
            }
        }
    }
}

/// VS Code Explorer order: directories first, then files, each case-insensitive.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// `.git` is always hidden; other dotfiles only when `show_hidden` is off.
fn visible(name: &str, show_hidden: bool) -> bool {
    name != ".git" && (show_hidden || !name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("aa-filetree-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn mkdir(&self, rel: &str) {
            fs::create_dir_all(self.0.join(rel)).unwrap();
        }
        fn touch(&self, rel: &str) {
            fs::write(self.0.join(rel), b"").unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(rows: &[Row]) -> Vec<(String, usize)> {
        rows.iter().map(|r| (r.name.clone(), r.depth)).collect()
    }

    #[test]
    fn dirs_first_case_insensitive_and_git_hidden() {
        let tmp = TempDir::new("order");
        tmp.mkdir("b_dir");
        tmp.mkdir("A_dir");
        tmp.mkdir(".git");
        tmp.touch("Zebra.txt");
        tmp.touch("apple.rs");
        let mut tree = Tree::new(tmp.0.clone());
        assert_eq!(
            names(&tree.rows()),
            vec![
                ("A_dir".into(), 0),
                ("b_dir".into(), 0),
                ("apple.rs".into(), 0),
                ("Zebra.txt".into(), 0),
            ]
        );
    }

    #[test]
    fn expand_and_collapse_nest_children() {
        let tmp = TempDir::new("expand");
        tmp.mkdir("src");
        tmp.touch("src/main.rs");
        tmp.touch("Cargo.toml");
        let mut tree = Tree::new(tmp.0.clone());
        tree.toggle(&tmp.0.join("src"));
        assert_eq!(
            names(&tree.rows()),
            vec![
                ("src".into(), 0),
                ("main.rs".into(), 1),
                ("Cargo.toml".into(), 0),
            ]
        );
        assert!(tree.rows()[0].expanded);
        tree.toggle(&tmp.0.join("src"));
        assert_eq!(
            names(&tree.rows()),
            vec![("src".into(), 0), ("Cargo.toml".into(), 0)]
        );
    }

    #[test]
    fn hidden_toggle_filters_dotfiles() {
        let tmp = TempDir::new("hidden");
        tmp.touch(".env");
        tmp.touch("visible.txt");
        let mut tree = Tree::new(tmp.0.clone());
        assert_eq!(tree.rows().len(), 2);
        tree.show_hidden = false;
        assert_eq!(names(&tree.rows()), vec![("visible.txt".into(), 0)]);
    }

    #[test]
    fn refresh_picks_up_new_files() {
        let tmp = TempDir::new("refresh");
        tmp.touch("one.txt");
        let mut tree = Tree::new(tmp.0.clone());
        assert_eq!(tree.rows().len(), 1);
        tmp.touch("two.txt");
        assert_eq!(tree.rows().len(), 1, "cached listing must not re-read disk");
        tree.refresh();
        assert_eq!(tree.rows().len(), 2);
    }

    #[test]
    fn unreadable_or_missing_dir_is_empty() {
        let mut tree = Tree::new(std::env::temp_dir().join("aa-filetree-does-not-exist"));
        assert!(tree.rows().is_empty());
    }

    #[test]
    fn root_name_uses_final_component() {
        let tmp = TempDir::new("rootname");
        let tree = Tree::new(tmp.0.clone());
        assert!(tree.root_name().starts_with("aa-filetree-"));
    }
}
