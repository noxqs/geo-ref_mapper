//! Mock file tree backing the FILES panel.
//!
//! This is placeholder data with the same shape a real directory scan will
//! produce, so the panel, the mosaic and the INFO panel can be built against a
//! stable interface before any disk I/O exists.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Path to a node, as a list of child indices from the roots downward.
pub type NodePath = Vec<usize>;

/// Which of the two marked sets a row is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    Folder,
    Image,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Folder,
    Image(ImageMeta),
}

#[derive(Clone, Debug)]
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub format: String,
    /// `images.id` once the file has been indexed; `None` for mock rows.
    /// Both of these are what the decoder and thumbnail cache will key off.
    #[allow(dead_code)]
    pub db_id: Option<i64>,
    #[allow(dead_code)]
    pub path: Option<String>,
    /// The file says where on the earth it is. Read from the library rather
    /// than the file: the tree draws every visible row every frame, and opening
    /// files to find out would be the same mistake as counting the subtree on
    /// demand.
    pub georeferenced: bool,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub kind: Kind,
    pub children: Vec<Node>,
    pub expanded: bool,
    /// Absolute path, for folders built from indexed rows. Images carry theirs
    /// in [`ImageMeta`]; mock data has neither.
    pub abs_path: Option<String>,
    /// Images directly inside this folder, and images anywhere beneath it.
    ///
    /// Cached rather than counted on demand: the tree panel needs one of these
    /// for every visible row, every frame, and walking the subtree each time
    /// cost 1.2 ms a frame on a library of 26,000 pictures. They only change
    /// when the library does, so they are worked out once when it is built.
    pub direct_images: usize,
    pub nested_images: usize,
}

impl Node {
    fn folder(name: &str, expanded: bool, children: Vec<Self>) -> Self {
        Self {
            name: name.to_owned(),
            kind: Kind::Folder,
            children,
            expanded,
            abs_path: None,
            direct_images: 0,
            nested_images: 0,
        }
    }

    fn folder_at(name: &str, path: &Path, children: Vec<Self>) -> Self {
        Self {
            name: name.to_owned(),
            kind: Kind::Folder,
            children,
            expanded: false,
            abs_path: Some(path.to_string_lossy().into_owned()),
            direct_images: 0,
            nested_images: 0,
        }
    }

    /// Absolute path of this node, whichever kind it is.
    pub fn path(&self) -> Option<&str> {
        match &self.kind {
            Kind::Image(meta) => meta.path.as_deref(),
            Kind::Folder => self.abs_path.as_deref(),
        }
    }

    fn image(name: &str, width: u32, height: u32, bytes: u64, format: &str) -> Self {
        Self {
            name: name.to_owned(),
            kind: Kind::Image(ImageMeta {
                width,
                height,
                bytes,
                format: format.to_owned(),
                db_id: None,
                path: None,
                georeferenced: false,
            }),
            children: Vec::new(),
            expanded: false,
            abs_path: None,
            direct_images: 0,
            nested_images: 0,
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.kind, Kind::Folder)
    }
}

/// Gathers the real files under a node. A folder contributes the images
/// directly inside it, not the whole subtree — marking a parent by accident
/// should not silently enrol hundreds of files in a batch.
fn collect_images(node: &Node, out: &mut Vec<PathBuf>) {
    match &node.kind {
        Kind::Image(meta) => {
            if let Some(path) = &meta.path {
                out.push(PathBuf::from(path));
            }
        }
        Kind::Folder => {
            for child in &node.children {
                if let Kind::Image(meta) = &child.kind
                    && let Some(path) = &meta.path
                {
                    out.push(PathBuf::from(path));
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileTree {
    pub roots: Vec<Node>,
    pub selected: NodePath,
    /// Extra rows picked out for a batch action.
    ///
    /// Kept apart from `selected`, which stays exactly what the viewer is
    /// showing. Conflating the two would mean ctrl-clicking a second file
    /// changed the picture on screen, which is not what anyone expects.
    pub marked: std::collections::BTreeSet<NodePath>,
    /// Folders picked out to set the Mosaic's scope.
    ///
    /// Kept apart from `marked` because the two answer different questions —
    /// "which folders am I looking through" and "which pictures am I about to
    /// act on" — and clearing one should not clear the other. Choosing an image
    /// used to wipe the folder range that had produced the grid it came from.
    pub marked_folders: std::collections::BTreeSet<NodePath>,
    /// Where a shift-click measures its range from.
    pub anchor: Option<NodePath>,
    /// True while the panel is showing the built-in sample data because the
    /// library has not been scanned yet.
    pub is_mock: bool,
}

impl Default for FileTree {
    fn default() -> Self {
        let mut tree = Self {
            roots: mock_roots(),
            // Start on the first folder so the mosaic has something to show.
            selected: vec![0],
            marked: std::collections::BTreeSet::new(),
            marked_folders: std::collections::BTreeSet::new(),
            anchor: None,
            is_mock: true,
        };
        tree.recount();
        tree
    }
}

impl FileTree {
    /// Rebuilds the tree from indexed rows, nesting them under their shared
    /// ancestor so a scan of one folder does not produce a chain of empty
    /// parents.
    pub fn from_records(records: &[crate::db::ImageRecord]) -> Self {
        if records.is_empty() {
            return Self::default();
        }

        let root_path = common_ancestor(records);
        let mut builder = Builder::default();

        for record in records {
            let relative = Path::new(&record.folder)
                .strip_prefix(&root_path)
                .unwrap_or(Path::new(""));
            let components: Vec<String> = relative
                .components()
                .filter_map(|component| match component {
                    Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();

            builder.insert(&components, record);
        }

        let root_name = display_name(&root_path);
        let mut root = Node::folder(&root_name, true, builder.finish(&root_path));
        root.abs_path = Some(root_path.to_string_lossy().into_owned());
        compact_chains(&mut root);
        // A single image folder is more useful expanded than collapsed.
        if root.children.len() == 1 && root.children[0].is_folder() {
            root.children[0].expanded = true;
        }

        let mut tree = Self {
            roots: vec![root],
            selected: vec![0],
            marked: std::collections::BTreeSet::new(),
            marked_folders: std::collections::BTreeSet::new(),
            anchor: None,
            is_mock: false,
        };
        tree.recount();
        tree
    }

    // ------------------------------------------------------ multiple selection

    /// Rows a batch action would run on: the marked set if there is one, else
    /// whatever single row is selected. That fallback is what makes right-click
    /// work without forcing the user to mark anything first.
    pub fn action_targets(&self) -> Vec<NodePath> {
        // Only the picture marks: a batch acts on images, and the folder marks
        // are a viewing scope rather than a list of things to export or delete.
        if self.marked.is_empty() {
            vec![self.selected.clone()]
        } else {
            self.marked.iter().cloned().collect()
        }
    }

    /// Absolute paths of every image the action would touch, expanding any
    /// folders in the selection to the images directly inside them.
    pub fn action_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for path in self.action_targets() {
            let Some(node) = self.get(&path) else {
                continue;
            };
            collect_images(node, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn is_marked(&self, path: &[usize]) -> bool {
        self.marked.contains(path) || self.marked_folders.contains(path)
    }

    /// Which set a row belongs to, so the two can be drawn differently.
    pub fn mark_of(&self, path: &[usize]) -> Option<Mark> {
        if self.marked_folders.contains(path) {
            Some(Mark::Folder)
        } else if self.marked.contains(path) {
            Some(Mark::Image)
        } else {
            None
        }
    }

    /// The set a given row would be marked into.
    fn set_for(&mut self, path: &[usize]) -> &mut std::collections::BTreeSet<NodePath> {
        let folder = self.get(path).is_some_and(Node::is_folder);
        if folder {
            &mut self.marked_folders
        } else {
            &mut self.marked
        }
    }

    /// Ctrl-click: add or remove one row without disturbing the rest.
    pub fn toggle_mark(&mut self, path: &NodePath) {
        let owned = path.clone();
        let set = self.set_for(path);
        if !set.remove(&owned) {
            set.insert(owned);
        }
        self.anchor = Some(path.clone());
    }

    /// Clears whichever set the given row belongs to, leaving the other alone.
    /// A plain click on a picture starts a fresh batch; it does not throw away
    /// the folder range you are looking through.
    pub fn clear_marks_like(&mut self, path: &NodePath) {
        self.set_for(path).clear();
        self.anchor = None;
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
        self.marked_folders.clear();
        self.anchor = None;
    }

    /// Shift-click: mark everything between the anchor and here, as the rows
    /// are currently laid out on screen.
    pub fn mark_range(&mut self, visible: &[NodePath], to: &NodePath) {
        let Some(anchor) = self.anchor.clone().or_else(|| Some(self.selected.clone())) else {
            return;
        };
        let (Some(from_index), Some(to_index)) = (
            visible.iter().position(|row| *row == anchor),
            visible.iter().position(|row| row == to),
        ) else {
            // The anchor scrolled out of the tree; treat it as a plain mark.
            self.toggle_mark(to);
            return;
        };

        let (low, high) = if from_index <= to_index {
            (from_index, to_index)
        } else {
            (to_index, from_index)
        };
        let rows: Vec<NodePath> = visible[low..=high].to_vec();
        for row in rows {
            self.set_for(&row).insert(row.clone());
        }
    }

    /// Records that a picture now does — or no longer does — say where it is.
    ///
    /// The tree carries its own copy of this, taken from the library when it was
    /// built, and it is the copy the green row colour is drawn from. Writing a
    /// position updates the database and the map page immediately; without this
    /// the tree alone kept saying no until the next scan rebuilt it, so the one
    /// place the user was looking was the last to know.
    ///
    /// Returns whether the picture was found.
    pub fn set_georeferenced(&mut self, target: &Path, georeferenced: bool) -> bool {
        let Some(found) = self.find_by_path(target) else {
            return false;
        };
        match self.get_mut(&found).map(|node| &mut node.kind) {
            Some(Kind::Image(meta)) => {
                meta.georeferenced = georeferenced;
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, path: &[usize]) -> Option<&Node> {
        let (first, rest) = path.split_first()?;
        let mut node = self.roots.get(*first)?;
        for index in rest {
            node = node.children.get(*index)?;
        }
        Some(node)
    }

    pub fn get_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let (first, rest) = path.split_first()?;
        let mut node = self.roots.get_mut(*first)?;
        for index in rest {
            node = node.children.get_mut(*index)?;
        }
        Some(node)
    }

    /// Locates the node holding a given file, for selecting an image that was
    /// opened from outside the tree.
    pub fn find_by_path(&self, path: &Path) -> Option<NodePath> {
        fn walk(nodes: &[Node], prefix: &[usize], target: &Path) -> Option<NodePath> {
            for (index, node) in nodes.iter().enumerate() {
                let mut current = prefix.to_vec();
                current.push(index);

                match &node.kind {
                    Kind::Image(meta) => {
                        if meta.path.as_deref().map(Path::new) == Some(target) {
                            return Some(current);
                        }
                    }
                    Kind::Folder => {
                        if let Some(found) = walk(&node.children, &current, target) {
                            return Some(found);
                        }
                    }
                }
            }
            None
        }

        walk(&self.roots, &[], path)
    }

    /// Finds any node — folder or image — by absolute path, expanding the
    /// folders above it so the selection is actually visible.
    pub fn reveal(&mut self, target: &str) -> Option<NodePath> {
        fn walk(nodes: &[Node], prefix: &[usize], target: &str) -> Option<NodePath> {
            for (index, node) in nodes.iter().enumerate() {
                let mut current = prefix.to_vec();
                current.push(index);
                if node.path() == Some(target) {
                    return Some(current);
                }
                if let Some(found) = walk(&node.children, &current, target) {
                    return Some(found);
                }
            }
            None
        }

        let found = walk(&self.roots, &[], target)?;
        for depth in 1..found.len() {
            if let Some(node) = self.get_mut(&found[..depth]) {
                node.expanded = true;
            }
        }
        Some(found)
    }

    /// Fills in [`Node::direct_images`] and [`Node::nested_images`] bottom-up.
    ///
    /// Only images are counted, never subfolders: a folder holding twelve
    /// subfolders and no pictures is empty as far as a picture viewer goes, and
    /// printing `12` beside it sent people looking for photographs not in it.
    pub fn recount(&mut self) {
        fn walk(node: &mut Node) -> usize {
            let mut direct = 0;
            let mut nested = 0;
            for child in &mut node.children {
                match child.kind {
                    Kind::Image(_) => {
                        direct += 1;
                        nested += 1;
                    }
                    Kind::Folder => nested += walk(child),
                }
            }
            node.direct_images = direct;
            node.nested_images = nested;
            nested
        }
        for root in &mut self.roots {
            walk(root);
        }
    }

    /// Every row the tree is showing, in the order they appear — what the
    /// arrow keys walk.
    pub fn visible_rows(&self) -> Vec<NodePath> {
        fn walk(nodes: &[Node], prefix: &[usize], out: &mut Vec<NodePath>) {
            for (index, node) in nodes.iter().enumerate() {
                let mut path = prefix.to_vec();
                path.push(index);
                out.push(path.clone());
                if node.is_folder() && node.expanded {
                    walk(&node.children, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, &[], &mut out);
        out
    }

    /// Absolute paths of every folder currently unfolded.
    ///
    /// The tree is rebuilt from the library on every scan tick, and a rebuild
    /// starts with everything collapsed. Without carrying this across, a folder
    /// opened during an import snaps shut a moment later — which reads as the
    /// import having reset rather than as the tree being rebuilt.
    pub fn expanded_paths(&self) -> Vec<String> {
        fn walk(nodes: &[Node], out: &mut Vec<String>) {
            for node in nodes {
                if node.is_folder()
                    && node.expanded
                    && let Some(path) = node.path()
                {
                    out.push(path.to_owned());
                }
                walk(&node.children, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, &mut out);
        out
    }

    /// Marks every picture in the tree.
    ///
    /// Pictures only — folders are a separate set answering a different
    /// question, and sweeping them in would change what the Mosaic is looking
    /// through as a side effect of picking a batch.
    pub fn mark_all_images(&mut self) {
        fn walk(nodes: &[Node], prefix: &[usize], out: &mut Vec<NodePath>) {
            for (index, node) in nodes.iter().enumerate() {
                let mut here = prefix.to_vec();
                here.push(index);
                if !node.is_folder() {
                    out.push(here.clone());
                }
                walk(&node.children, &here, out);
            }
        }
        let mut found = Vec::new();
        walk(&self.roots, &[], &mut found);
        self.marked.extend(found);
    }

    /// Adds every picture in `add` to the marked set — the OR.
    /// The marked pictures, and only those.
    ///
    /// [`action_paths`](Self::action_paths) falls back to the selected row when
    /// nothing is marked, so that Export and Delete act on the picture you are
    /// looking at. That is right for a command and wrong for a filter: showing
    /// "only what is selected" with nothing selected has an obvious answer, and
    /// it is not "the one under the cursor".
    pub fn marked_paths_only(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for path in &self.marked {
            if let Some(node) = self.get(path) {
                collect_images(node, &mut out);
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Whether anything is picked out at all.
    ///
    /// Asked several times a frame, only ever to decide whether to draw a
    /// footer or enable a button. `action_paths` answers it too, but it builds,
    /// sorts and dedups the whole list to do so — which on a set of five
    /// thousand is real work repeated for a question with a one-word answer.
    pub fn has_marks(&self) -> bool {
        !self.marked.is_empty() || !self.marked_folders.is_empty()
    }

    /// One walk of the tree, not one per path.
    ///
    /// `find_by_path` searches the whole tree for a single name, so calling it
    /// per path made this cost the number of paths *times* the size of the
    /// tree — five thousand pictures against eight thousand nodes is forty
    /// million visits, and that was the pause after clicking a tag. Walking
    /// once and asking the set about each node it passes is the same answer in
    /// a fraction of the time.
    pub fn mark_those_in(&mut self, add: &std::collections::HashSet<PathBuf>) -> usize {
        fn walk(
            nodes: &[Node],
            prefix: &[usize],
            add: &std::collections::HashSet<PathBuf>,
            out: &mut Vec<NodePath>,
        ) {
            for (index, node) in nodes.iter().enumerate() {
                let mut here = prefix.to_vec();
                here.push(index);
                if let Kind::Image(meta) = &node.kind
                    && let Some(path) = &meta.path
                    && add.contains(Path::new(path))
                {
                    out.push(here.clone());
                }
                walk(&node.children, &here, add, out);
            }
        }
        let before = self.marked.len();
        let mut found = Vec::new();
        walk(&self.roots, &[], add, &mut found);
        self.marked.extend(found);
        self.marked.len() - before
    }

    /// The marked rows as paths, for carrying a set across a rebuild.
    ///
    /// A mark is a list of child indices, and a rebuild renumbers them — a scan
    /// or a copy adds rows and every index after them shifts, so the old mark
    /// would quietly point at a different file. Paths survive that; indices do
    /// not.
    pub fn marked_paths(&self) -> (Vec<String>, Vec<String>) {
        let named = |set: &std::collections::BTreeSet<NodePath>| -> Vec<String> {
            set.iter()
                .filter_map(|path| self.get(path)?.path().map(str::to_owned))
                .collect()
        };
        (named(&self.marked), named(&self.marked_folders))
    }

    /// Marks again whatever [`marked_paths`] named, as far as it still exists.
    ///
    /// Anything gone is quietly dropped, which is what makes this right after a
    /// *move* as well as a copy: the files left where they were stay marked,
    /// and the ones that were carried off do not linger as marks on rows that
    /// are no longer there.
    pub fn restore_marked(&mut self, images: &[String], folders: &[String]) {
        for (paths, folders_wanted) in [(images, false), (folders, true)] {
            for path in paths {
                let Some(node) = self.find_by_path(Path::new(path)) else {
                    continue;
                };
                let is_folder = self.get(&node).is_some_and(Node::is_folder);
                if is_folder != folders_wanted {
                    continue;
                }
                if is_folder {
                    self.marked_folders.insert(node);
                } else {
                    self.marked.insert(node);
                }
            }
        }
    }

    /// Re-opens the folders named by [`expanded_paths`].
    pub fn restore_expanded(&mut self, paths: &[String]) {
        fn walk(nodes: &mut [Node], paths: &[String]) {
            for node in nodes {
                if node.is_folder()
                    && node
                        .path()
                        .is_some_and(|path| paths.iter().any(|p| p == path))
                {
                    node.expanded = true;
                }
                walk(&mut node.children, paths);
            }
        }
        walk(&mut self.roots, paths);
    }

    /// Folds every folder shut. Used before revealing a bookmark, so the tree
    /// shows that one branch rather than that branch plus whatever was already
    /// unfolded halfway down the panel.
    pub fn collapse_all(&mut self) {
        fn walk(nodes: &mut [Node]) {
            for node in nodes {
                node.expanded = false;
                walk(&mut node.children);
            }
        }
        walk(&mut self.roots);
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.get(&self.selected)
    }

    /// Human-readable location of the selection, e.g. `Photos / 2026 / Iceland`.
    /// A readable location for one node.
    ///
    /// The absolute path when the node knows it, which is both shorter and
    /// truer than walking the tree: the display names include a root called
    /// `/` and a compacted `home/morgan/Pictures`, so joining them produced
    /// `/  /  home/morgan/Pictures  /  MAPS-NEW` — a leading double slash and
    /// wide gaps around every separator.
    pub fn label_for(&self, path: &[usize]) -> String {
        if let Some(absolute) = self.get(path).and_then(Node::path) {
            return absolute.to_owned();
        }
        let mut parts = Vec::new();
        let mut prefix: NodePath = Vec::new();
        for index in path {
            prefix.push(*index);
            match self.get(&prefix) {
                Some(node) => parts.push(node.name.clone()),
                None => break,
            }
        }
        if parts.is_empty() {
            "No selection".to_owned()
        } else {
            parts.join("/").replace("//", "/")
        }
    }

    /// The folder the mosaic should draw from: the selection itself if it is a
    /// folder, otherwise the folder containing the selected image.
    pub fn selected_folder(&self) -> NodePath {
        match self.selected_node() {
            Some(node) if node.is_folder() => self.selected.clone(),
            Some(_) => {
                let mut path = self.selected.clone();
                path.pop();
                path
            }
            None => Vec::new(),
        }
    }

    /// Images in the selected folder, optionally including every subfolder.
    pub fn images_for_mosaic(&self, recursive: bool) -> Vec<(NodePath, &Node)> {
        self.images_in(&self.selected_folder(), recursive)
    }

    /// Images under an explicit folder.
    ///
    /// The Mosaic needs this rather than following the selection: clicking a
    /// tile selects that image, and deriving the grid from the selection meant
    /// the grid collapsed to the one subfolder the image happened to live in.
    pub fn images_in(&self, folder: &[usize], recursive: bool) -> Vec<(NodePath, &Node)> {
        let folder = folder.to_vec();
        let mut out = Vec::new();

        let children: &[Node] = match folder.as_slice() {
            [] => &self.roots,
            path => match self.get(path) {
                Some(node) => &node.children,
                None => return out,
            },
        };

        collect(children, &folder, recursive, &mut out);
        out
    }
}

fn collect<'a>(
    nodes: &'a [Node],
    prefix: &[usize],
    recursive: bool,
    out: &mut Vec<(NodePath, &'a Node)>,
) {
    for (index, node) in nodes.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(index);
        match node.kind {
            Kind::Image(_) => out.push((path, node)),
            Kind::Folder => {
                if recursive {
                    collect(&node.children, &path, recursive, out);
                }
            }
        }
    }
}

/// Collapses runs of folders that hold nothing but one subfolder, so a scan of
/// `/home/me/Pictures` shows one row instead of a three-deep ladder.
fn compact_chains(node: &mut Node) {
    for child in &mut node.children {
        compact_chains(child);
    }

    while node.is_folder() && node.children.len() == 1 && node.children[0].is_folder() {
        let child = node.children.remove(0);
        node.name = join_names(&node.name, &child.name);
        // The merged row now stands for the deepest folder.
        node.abs_path = child.abs_path.clone().or_else(|| node.abs_path.clone());
        node.children = child.children;
    }
}

fn join_names(parent: &str, child: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{child}")
    } else {
        format!("{parent}/{child}")
    }
}

/// Deepest directory that contains every indexed folder.
fn common_ancestor(records: &[crate::db::ImageRecord]) -> PathBuf {
    let mut iter = records.iter().map(|record| PathBuf::from(&record.folder));
    let Some(mut common) = iter.next() else {
        return PathBuf::new();
    };

    for folder in iter {
        while !folder.starts_with(&common) {
            if !common.pop() {
                return common;
            }
        }
    }
    common
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            let text = path.to_string_lossy();
            if text.is_empty() {
                "Library".to_owned()
            } else {
                text.into_owned()
            }
        })
}

/// Intermediate tree keyed by folder name, so children come out sorted and
/// each folder is created exactly once.
#[derive(Default)]
struct Builder {
    folders: BTreeMap<String, Builder>,
    images: BTreeMap<String, Node>,
}

impl Builder {
    fn insert(&mut self, components: &[String], record: &crate::db::ImageRecord) {
        match components.split_first() {
            Some((head, rest)) => self
                .folders
                .entry(head.clone())
                .or_default()
                .insert(rest, record),
            None => {
                self.images.insert(
                    record.filename.clone(),
                    Node {
                        name: record.filename.clone(),
                        kind: Kind::Image(ImageMeta {
                            width: record.width.unwrap_or(0),
                            height: record.height.unwrap_or(0),
                            bytes: record.bytes,
                            format: record
                                .format
                                .clone()
                                .unwrap_or_else(|| "Unknown".to_owned()),
                            db_id: Some(record.id),
                            path: Some(record.path.clone()),
                            georeferenced: record.georeferenced.unwrap_or(false),
                        }),
                        children: Vec::new(),
                        expanded: false,
                        abs_path: None,
                        direct_images: 0,
                        nested_images: 0,
                    },
                );
            }
        }
    }

    /// Folders first, then images, each alphabetically.
    fn finish(self, prefix: &Path) -> Vec<Node> {
        let mut nodes: Vec<Node> = self
            .folders
            .into_iter()
            .map(|(name, child)| {
                let path = prefix.join(&name);
                let children = child.finish(&path);
                Node::folder_at(&name, &path, children)
            })
            .collect();
        nodes.extend(self.images.into_values());
        nodes
    }
}

fn mock_roots() -> Vec<Node> {
    vec![
        Node::folder(
            "Photos",
            true,
            vec![
                Node::folder(
                    "Iceland 2026",
                    true,
                    vec![
                        Node::image("vestrahorn.jpg", 6000, 4000, 8_912_000, "JPEG"),
                        Node::image("skogafoss.jpg", 6000, 4000, 7_430_000, "JPEG"),
                        Node::image("black-sand.png", 4032, 3024, 21_500_000, "PNG"),
                        Node::image("aurora-01.jpg", 6000, 4000, 9_100_000, "JPEG"),
                        Node::image("aurora-02.jpg", 6000, 4000, 8_740_000, "JPEG"),
                    ],
                ),
                Node::folder(
                    "Studio",
                    false,
                    vec![
                        Node::image("backdrop-test.tif", 5472, 3648, 59_800_000, "TIFF"),
                        Node::image("lighting-a.webp", 3000, 2000, 1_240_000, "WebP"),
                        Node::image("lighting-b.webp", 3000, 2000, 1_180_000, "WebP"),
                    ],
                ),
                Node::image("cover.jpg", 2400, 1600, 1_020_000, "JPEG"),
            ],
        ),
        Node::folder(
            "Scans",
            false,
            vec![
                Node::image("negative-014.tif", 4800, 3200, 44_100_000, "TIFF"),
                Node::image("negative-015.tif", 4800, 3200, 43_600_000, "TIFF"),
                Node::image("contact-sheet.pdf.png", 2480, 3508, 5_600_000, "PNG"),
            ],
        ),
        Node::folder(
            "Renders",
            false,
            vec![
                Node::image("hall-final.exr", 3840, 2160, 132_000_000, "OpenEXR"),
                Node::image("hall-draft.hdr", 1920, 1080, 18_400_000, "Radiance HDR"),
                Node::image("turntable.gif", 800, 800, 4_300_000, "GIF"),
                Node::image("icon-sheet.qoi", 1024, 1024, 640_000, "QOI"),
            ],
        ),
    ]
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::db::ImageRecord;

    fn tree_of(paths: &[&str]) -> FileTree {
        let records: Vec<ImageRecord> = paths
            .iter()
            .map(|path| {
                let (folder, filename) = path.rsplit_once('/').unwrap();
                ImageRecord {
                    id: 1,
                    path: (*path).to_owned(),
                    folder: folder.to_owned(),
                    filename: filename.to_owned(),
                    format: Some("PNG".to_owned()),
                    width: Some(4),
                    height: Some(4),
                    bytes: 10,
                    mtime: 0,
                    has_thumbnail: false,
                    georeferenced: None,
                }
            })
            .collect();
        FileTree::from_records(&records)
    }

    /// Every visible row, in the order the panel draws them.
    fn visible(tree: &FileTree) -> Vec<NodePath> {
        fn walk(nodes: &[Node], prefix: &[usize], out: &mut Vec<NodePath>) {
            for (index, node) in nodes.iter().enumerate() {
                let mut path = prefix.to_vec();
                path.push(index);
                out.push(path.clone());
                if node.is_folder() {
                    walk(&node.children, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&tree.roots, &[], &mut out);
        out
    }

    #[test]
    fn with_nothing_marked_the_action_falls_back_to_the_selection() {
        let tree = tree_of(&["/photos/a.png", "/photos/b.png"]);
        assert!(tree.marked.is_empty());
        // One target, and it is whatever is selected.
        assert_eq!(tree.action_targets(), vec![tree.selected.clone()]);
    }

    #[test]
    fn marking_toggles_and_clears() {
        let mut tree = tree_of(&["/photos/a.png", "/photos/b.png"]);
        let rows = visible(&tree);
        let first = rows[1].clone();

        tree.toggle_mark(&first);
        assert!(tree.is_marked(&first));
        assert_eq!(tree.marked.len(), 1);

        // Toggling the same row again removes it.
        tree.toggle_mark(&first);
        assert!(!tree.is_marked(&first));

        tree.toggle_mark(&first);
        tree.clear_marks();
        assert!(tree.marked.is_empty());
        assert!(tree.anchor.is_none());
    }

    #[test]
    fn a_shift_range_covers_everything_between_the_anchor_and_the_click() {
        let mut tree = tree_of(&[
            "/photos/a.png",
            "/photos/b.png",
            "/photos/c.png",
            "/photos/d.png",
        ]);
        let rows = visible(&tree);
        assert!(rows.len() >= 4, "expected several rows, got {}", rows.len());

        // Anchor on the second row, shift-click the fourth.
        tree.anchor = Some(rows[1].clone());
        tree.mark_range(&rows, &rows[3]);

        assert!(tree.is_marked(&rows[1]));
        assert!(tree.is_marked(&rows[2]));
        assert!(tree.is_marked(&rows[3]));
        assert!(!tree.is_marked(&rows[0]), "outside the range");
    }

    #[test]
    fn a_range_dragged_upwards_works_the_same_way() {
        let mut tree = tree_of(&[
            "/photos/a.png",
            "/photos/b.png",
            "/photos/c.png",
            "/photos/d.png",
        ]);
        let rows = visible(&tree);
        tree.anchor = Some(rows[3].clone());
        tree.mark_range(&rows, &rows[1]);

        assert!(tree.is_marked(&rows[1]));
        assert!(tree.is_marked(&rows[2]));
        assert!(tree.is_marked(&rows[3]));
    }

    #[test]
    fn a_range_whose_anchor_has_gone_marks_only_the_clicked_row() {
        let mut tree = tree_of(&["/photos/a.png", "/photos/b.png"]);
        let rows = visible(&tree);
        // An anchor that is not in the visible list any more.
        tree.anchor = Some(vec![99]);
        tree.mark_range(&rows, &rows[1]);

        assert!(tree.is_marked(&rows[1]));
        assert_eq!(tree.marked.len(), 1);
    }

    #[test]
    fn action_paths_resolve_to_real_files_and_deduplicate() {
        let mut tree = tree_of(&["/photos/a.png", "/photos/b.png"]);
        let rows = visible(&tree);
        for row in &rows {
            tree.toggle_mark(row);
        }

        let paths = tree.action_paths();
        // Both images, once each, even though the folder is marked too.
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(
            paths
                .iter()
                .all(|path| path.to_string_lossy().ends_with(".png"))
        );
        let mut sorted = paths.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len(), "duplicates leaked through");
    }

    #[test]
    fn marking_a_folder_gathers_the_images_inside_it() {
        let mut tree = tree_of(&["/photos/a.png", "/photos/b.png"]);
        let rows = visible(&tree);
        // The first row is the root folder.
        tree.toggle_mark(&rows[0]);
        let paths = tree.action_paths();
        assert!(!paths.is_empty(), "a folder should contribute its images");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ImageRecord;

    fn record(folder: &str, filename: &str) -> ImageRecord {
        ImageRecord {
            id: 1,
            path: format!("{folder}/{filename}"),
            folder: folder.to_owned(),
            filename: filename.to_owned(),
            format: Some("PNG".to_owned()),
            width: Some(10),
            height: Some(20),
            bytes: 100,
            mtime: 0,
            has_thumbnail: false,
            georeferenced: None,
        }
    }

    /// Reads back what the tree row is actually coloured from.
    fn shown_as_georeferenced(tree: &FileTree, path: &str) -> Option<bool> {
        let found = tree.find_by_path(Path::new(path))?;
        match &tree.get(&found)?.kind {
            Kind::Image(meta) => Some(meta.georeferenced),
            Kind::Folder => None,
        }
    }

    /// What a plain click in the tree does, as the panel decides it: adds to a
    /// set already being built, and only starts fresh when there is none.
    fn plain_click(tree: &mut FileTree, path: &NodePath) {
        let is_folder = tree.get(path).is_some_and(Node::is_folder);
        let building = if is_folder {
            !tree.marked_folders.is_empty()
        } else {
            !tree.marked.is_empty()
        };
        if building {
            tree.toggle_mark(path);
        } else {
            tree.clear_marks_like(path);
        }
        tree.selected = path.clone();
    }

    #[test]
    fn a_shut_folder_is_distinguishable_from_an_open_one() {
        // What the Viewer decides on: a folder row the arrows land on shows its
        // first picture only when it is open. Shut, it shows nothing, which is
        // the sign that one folder has ended and another has not been entered.
        let mut tree =
            FileTree::from_records(&[record("/maps/1932", "a.tif"), record("/maps/1945", "b.tif")]);
        // By name: `find_by_path` resolves pictures, not folders.
        let folder = tree
            .visible_rows()
            .into_iter()
            .find(|row| {
                tree.get(row)
                    .is_some_and(|node| node.is_folder() && node.name == "1945")
            })
            .expect("the 1945 folder is in the tree");

        // `from_records` may open a folder on the way in; say what this test is
        // about rather than relying on that.
        if let Some(node) = tree.get_mut(&folder) {
            node.expanded = false;
        }
        assert!(!tree.get(&folder).unwrap().expanded);
        assert_eq!(
            tree.images_in(&folder, false).len(),
            1,
            "the pictures are still there — it is the folder that is shut",
        );

        if let Some(node) = tree.get_mut(&folder) {
            node.expanded = true;
        }
        assert!(tree.get(&folder).unwrap().expanded);
    }

    /// A set of paths, as a tag query returns one.
    fn set(paths: &[&str]) -> std::collections::HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn showing_only_the_selection_shows_nothing_when_nothing_is_selected() {
        // `action_paths` falls back to the selected row so that Export and
        // Delete act on the picture in front of you. A filter must not inherit
        // that: "only what is selected", with nothing selected, is empty.
        let tree = FileTree::from_records(&[record("/maps", "a.tif"), record("/maps", "b.tif")]);
        assert!(!tree.has_marks());
        assert!(
            !tree.action_paths().is_empty(),
            "the fallback is still there for the commands that want it — with a \
             folder selected it resolves to the pictures inside it",
        );
        assert!(
            tree.marked_paths_only().is_empty(),
            "but the filter must see an empty set",
        );
    }

    #[test]
    fn adding_a_tag_widens_the_set_without_duplicating_it() {
        let mut tree =
            FileTree::from_records(&[record("/maps", "a.tif"), record("/maps", "b.tif")]);
        assert_eq!(tree.mark_those_in(&set(&["/maps/a.tif"])), 1);
        // Asking again adds nothing: a set holds a thing once.
        assert_eq!(tree.mark_those_in(&set(&["/maps/a.tif"])), 0);
        assert_eq!(tree.mark_those_in(&set(&["/maps/b.tif"])), 1);
        assert_eq!(tree.action_paths().len(), 2);
    }

    #[test]
    fn selecting_everything_takes_pictures_and_not_folders() {
        // Folders are a separate set answering "what am I looking through";
        // sweeping them in would change the Mosaic's scope as a side effect.
        let mut tree =
            FileTree::from_records(&[record("/maps/1932", "a.tif"), record("/maps/1945", "b.tif")]);
        tree.mark_all_images();
        assert_eq!(tree.action_paths().len(), 2);
        assert!(
            tree.marked_folders.is_empty(),
            "no folder should have been marked",
        );
    }

    #[test]
    fn a_set_survives_the_tree_being_rebuilt_under_it() {
        // What happens after a copy: the library is re-read and the tree built
        // afresh, with a new row in the middle shifting every index after it.
        let mut tree =
            FileTree::from_records(&[record("/maps", "a.tif"), record("/maps", "c.tif")]);
        let a = tree.find_by_path(Path::new("/maps/a.tif")).unwrap();
        let c = tree.find_by_path(Path::new("/maps/c.tif")).unwrap();
        tree.toggle_mark(&a);
        tree.toggle_mark(&c);

        let (images, folders) = tree.marked_paths();
        let mut rebuilt = FileTree::from_records(&[
            record("/maps", "a.tif"),
            record("/maps", "b.tif"), // arrives between them, shifting c
            record("/maps", "c.tif"),
        ]);
        rebuilt.restore_marked(&images, &folders);

        let mut marked = rebuilt.action_paths();
        marked.sort();
        assert_eq!(
            marked,
            vec![PathBuf::from("/maps/a.tif"), PathBuf::from("/maps/c.tif")],
            "carrying marks by index would have marked b.tif instead of c.tif",
        );
    }

    #[test]
    fn a_moved_file_drops_out_of_the_set_by_simply_not_being_there() {
        let mut tree =
            FileTree::from_records(&[record("/maps", "a.tif"), record("/maps", "b.tif")]);
        let a = tree.find_by_path(Path::new("/maps/a.tif")).unwrap();
        let b = tree.find_by_path(Path::new("/maps/b.tif")).unwrap();
        tree.toggle_mark(&a);
        tree.toggle_mark(&b);

        let (images, folders) = tree.marked_paths();
        // b.tif was moved away, so the rebuilt library no longer has it.
        let mut rebuilt = FileTree::from_records(&[record("/maps", "a.tif")]);
        rebuilt.restore_marked(&images, &folders);
        assert_eq!(rebuilt.action_paths(), vec![PathBuf::from("/maps/a.tif")]);
    }

    #[test]
    fn clicking_with_a_set_already_going_adds_to_it_rather_than_wiping_it() {
        let mut tree = FileTree::from_records(&[
            record("/maps", "a.tif"),
            record("/maps", "b.tif"),
            record("/maps", "c.tif"),
        ]);
        let a = tree.find_by_path(Path::new("/maps/a.tif")).unwrap();
        let b = tree.find_by_path(Path::new("/maps/b.tif")).unwrap();
        let c = tree.find_by_path(Path::new("/maps/c.tif")).unwrap();

        // Nothing marked yet: a click just selects, marking nothing.
        plain_click(&mut tree, &a);
        assert!(tree.marked.is_empty(), "a first click starts no set");

        // Mark one, then plain-click the others: they join it.
        tree.toggle_mark(&a);
        plain_click(&mut tree, &b);
        plain_click(&mut tree, &c);
        assert_eq!(tree.action_paths().len(), 3, "the set must have grown");

        // And clicking one already in the set takes it back out — the whole
        // point being that the other two survive.
        plain_click(&mut tree, &b);
        assert!(!tree.is_marked(&b));
        assert_eq!(tree.action_paths().len(), 2, "the rest must be untouched");
    }

    #[test]
    fn marking_the_picture_on_screen_adds_it_and_marking_again_takes_it_back() {
        // What the spacebar does in the Viewer: go through a folder one picture
        // at a time, picking out the keepers as they come.
        let mut tree = FileTree::from_records(&[
            record("/maps", "a.tif"),
            record("/maps", "b.tif"),
            record("/maps", "c.tif"),
        ]);

        let a = tree.find_by_path(Path::new("/maps/a.tif")).unwrap();
        let c = tree.find_by_path(Path::new("/maps/c.tif")).unwrap();

        tree.toggle_mark(&a);
        tree.toggle_mark(&c);
        assert_eq!(tree.action_paths().len(), 2);
        assert!(tree.is_marked(&a) && tree.is_marked(&c));

        // The same key again takes one back off, leaving the other alone.
        tree.toggle_mark(&a);
        assert!(!tree.is_marked(&a), "marking twice must undo it");
        assert!(tree.is_marked(&c), "and must not disturb the rest");
        assert_eq!(tree.action_paths(), vec![PathBuf::from("/maps/c.tif")]);
    }

    #[test]
    fn writing_a_position_turns_the_row_green_without_a_rescan() {
        let mut tree =
            FileTree::from_records(&[record("/maps", "placed.tif"), record("/maps", "other.tif")]);
        assert_eq!(
            shown_as_georeferenced(&tree, "/maps/placed.tif"),
            Some(false)
        );

        assert!(tree.set_georeferenced(Path::new("/maps/placed.tif"), true));
        assert_eq!(
            shown_as_georeferenced(&tree, "/maps/placed.tif"),
            Some(true)
        );
        // And only that one.
        assert_eq!(
            shown_as_georeferenced(&tree, "/maps/other.tif"),
            Some(false)
        );

        // Taking the position back out unmarks it again.
        assert!(tree.set_georeferenced(Path::new("/maps/placed.tif"), false));
        assert_eq!(
            shown_as_georeferenced(&tree, "/maps/placed.tif"),
            Some(false)
        );
    }

    #[test]
    fn marking_a_picture_the_tree_has_never_heard_of_changes_nothing() {
        let mut tree = FileTree::from_records(&[record("/maps", "a.tif")]);
        assert!(!tree.set_georeferenced(Path::new("/elsewhere/b.tif"), true));
        assert_eq!(shown_as_georeferenced(&tree, "/maps/a.tif"), Some(false));
    }

    #[test]
    fn no_records_falls_back_to_sample_data() {
        let tree = FileTree::from_records(&[]);
        assert!(tree.is_mock);
    }

    #[test]
    fn a_single_folder_becomes_the_root() {
        let tree = FileTree::from_records(&[
            record("/home/me/Pictures", "a.png"),
            record("/home/me/Pictures", "b.png"),
        ]);

        assert!(!tree.is_mock);
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].name, "Pictures");
        assert_eq!(tree.roots[0].children.len(), 2);
    }

    #[test]
    fn single_child_folder_chains_are_collapsed() {
        // Two scans that only meet at the filesystem root would otherwise show
        // as `/` → `home` → `me` → `Pictures`.
        let tree = FileTree::from_records(&[
            record("/home/me/Pictures", "a.png"),
            record("/tmp/shots", "b.png"),
        ]);

        let names: Vec<&str> = tree.roots[0]
            .children
            .iter()
            .map(|child| child.name.as_str())
            .collect();
        assert_eq!(names, vec!["home/me/Pictures", "tmp/shots"]);
    }

    #[test]
    fn folders_sort_before_images() {
        let tree =
            FileTree::from_records(&[record("/root", "zebra.png"), record("/root/sub", "a.png")]);

        let children = &tree.roots[0].children;
        assert!(children[0].is_folder(), "folders come first");
        assert_eq!(children[0].name, "sub");
        assert_eq!(children[1].name, "zebra.png");
    }

    #[test]
    fn find_by_path_locates_a_nested_image() {
        let tree = FileTree::from_records(&[
            record("/root", "top.png"),
            record("/root/deep", "buried.png"),
        ]);

        let found = tree
            .find_by_path(Path::new("/root/deep/buried.png"))
            .expect("the image should be somewhere in the tree");
        assert_eq!(tree.get(&found).unwrap().name, "buried.png");
        assert!(tree.find_by_path(Path::new("/root/missing.png")).is_none());
    }

    #[test]
    fn recursive_mosaic_reaches_subfolders() {
        let mut tree = FileTree::from_records(&[
            record("/root", "top.png"),
            record("/root/deep", "buried.png"),
        ]);
        tree.selected = vec![0];

        assert_eq!(tree.images_for_mosaic(false).len(), 1);
        assert_eq!(tree.images_for_mosaic(true).len(), 2);
    }

    #[test]
    fn selecting_an_image_uses_its_parent_folder_for_the_mosaic() {
        let mut tree =
            FileTree::from_records(&[record("/root", "a.png"), record("/root", "b.png")]);
        // Select the first image rather than the folder.
        tree.selected = vec![0, 0];

        assert_eq!(tree.selected_folder(), vec![0]);
        assert_eq!(tree.images_for_mosaic(false).len(), 2);
    }

    #[test]
    fn visible_rows_follow_what_is_unfolded() {
        // What the arrow keys walk: folders included, and only the rows a
        // collapsed parent is actually showing.
        let records = ["/photos/a/1.png", "/photos/b/2.png"]
            .into_iter()
            .map(|path| crate::db::ImageRecord {
                path: path.to_owned(),
                folder: Path::new(path)
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                filename: Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let mut tree = FileTree::from_records(&records);

        // Root expanded, its two folders shut: root + a + b.
        assert_eq!(tree.visible_rows().len(), 3);

        tree.get_mut(&[0, 0]).unwrap().expanded = true;
        assert_eq!(
            tree.visible_rows().len(),
            4,
            "unfolding one adds only its own child",
        );
    }

    #[test]
    fn folder_scope_and_picture_batch_do_not_clear_each_other() {
        // The bug this covers: marking three folders to build a Mosaic, then
        // clicking a picture in it, wiped the folder range that produced the
        // grid you were looking at.
        let records = ["/photos/a/1.png", "/photos/b/2.png"]
            .into_iter()
            .map(|path| crate::db::ImageRecord {
                path: path.to_owned(),
                folder: Path::new(path)
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                filename: Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let mut tree = FileTree::from_records(&records);

        let folder = vec![0, 0];
        let image = tree
            .find_by_path(Path::new("/photos/a/1.png"))
            .expect("the picture is in the tree");
        assert!(tree.get(&folder).unwrap().is_folder());

        tree.toggle_mark(&folder);
        tree.toggle_mark(&image);
        assert_eq!(tree.mark_of(&folder), Some(Mark::Folder));
        assert_eq!(tree.mark_of(&image), Some(Mark::Image));

        // Clicking a picture starts a fresh batch and leaves the scope alone.
        tree.clear_marks_like(&image);
        assert_eq!(tree.mark_of(&folder), Some(Mark::Folder), "scope survived");
        assert_eq!(tree.mark_of(&image), None);

        // And the other way round.
        tree.toggle_mark(&image);
        tree.clear_marks_like(&folder);
        assert_eq!(tree.mark_of(&folder), None);
        assert_eq!(tree.mark_of(&image), Some(Mark::Image), "batch survived");
    }

    #[test]
    fn expansion_survives_a_rebuild() {
        // The tree is rebuilt from the library on every scan tick, and a
        // rebuild starts collapsed. A folder opened during an import must not
        // snap shut a moment later — that reads as the import resetting.
        let records = vec![
            crate::db::ImageRecord {
                path: "/photos/trip/a.png".to_owned(),
                folder: "/photos/trip".to_owned(),
                filename: "a.png".to_owned(),
                ..Default::default()
            },
            crate::db::ImageRecord {
                path: "/photos/other/b.png".to_owned(),
                folder: "/photos/other".to_owned(),
                filename: "b.png".to_owned(),
                ..Default::default()
            },
        ];

        // The root is `/photos`; `trip` and `other` are its children, sorted.
        let mut tree = FileTree::from_records(&records);
        let trip = vec![0, 1];
        assert_eq!(
            tree.get(&trip).map(|node| node.name.as_str()),
            Some("trip"),
            "the folder this test is about",
        );
        tree.get_mut(&trip).unwrap().expanded = true;
        let open = tree.expanded_paths();
        assert!(open.iter().any(|path| path == "/photos/trip"), "{open:?}");

        let mut rebuilt = FileTree::from_records(&records);
        assert!(
            !rebuilt.get(&trip).unwrap().expanded,
            "a fresh rebuild really does start collapsed",
        );
        rebuilt.restore_expanded(&open);
        assert!(rebuilt.get(&trip).unwrap().expanded);
    }

    #[test]
    fn metadata_survives_the_round_trip() {
        let tree = FileTree::from_records(&[record("/root", "a.png")]);
        let node = tree.get(&[0, 0]).unwrap();
        match &node.kind {
            Kind::Image(meta) => {
                assert_eq!((meta.width, meta.height), (10, 20));
                assert_eq!(meta.format, "PNG");
                assert_eq!(meta.path.as_deref(), Some("/root/a.png"));
            }
            Kind::Folder => panic!("expected an image"),
        }
    }

    #[test]
    fn byte_formatting_switches_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
