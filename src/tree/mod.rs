pub mod model;
pub mod scan;
pub mod watch;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use gpui::AppContext as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, ScrollStrategy, SharedString, StatefulInteractiveElement,
    Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};

use crate::config::{Config, Theme};
use crate::git::{self, GitFileStatus};
use crate::keymap::actions::*;
use crate::line_edit::LineEdit;
use crate::terminal::colors::{blend, translucent};
use model::{Node, RowKind, VisibleRow, rebuild_visible, remove_subtree};
use watch::TreeWatcher;

pub enum TreeEvent {
    OpenFile(PathBuf),
    /// Render a markdown file and page it in a new tab.
    PreviewMarkdown(PathBuf),
    /// The user re-rooted the tree; the shell should follow.
    ChangedRoot(PathBuf),
    /// The root changed for any reason (including following the shell);
    /// informational, for anything that resolves paths against it.
    RootChanged(PathBuf),
    /// Type the quoted path at the prompt, relative to the pane's cwd when
    /// it's beneath it unless `absolute`.
    InsertPath {
        path: PathBuf,
        absolute: bool,
    },
    /// `cd` the shell to a directory without re-rooting the tree.
    CdShell(PathBuf),
    FocusTerminal,
    /// A name or path is needed. The window asks for it in its prompt modal
    /// and hands the answer to `FileTree::apply_prompt`.
    Prompt {
        prompt: TreePrompt,
        title: String,
        hint: &'static str,
        initial: LineEdit,
    },
}

/// What the text typed into the window's prompt modal is for.
#[derive(Clone)]
pub enum TreePrompt {
    Add { parent: PathBuf },
    Rename { target: PathBuf },
    Move { target: PathBuf },
}

/// A row being dragged out of the tree; dropping it on a terminal pane
/// inserts the quoted path.
#[derive(Clone)]
pub struct TreeDrag {
    pub path: PathBuf,
}

/// The label that follows the pointer during a drag, and a row's tooltip.
struct DragLabel(SharedString);

impl Render for DragLabel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(gpui::black())
            .text_color(gpui::white())
            .text_size(px(12.0))
            .child(self.0.clone())
    }
}

/// A right-click menu on a tree row, or on the root: its name in the
/// header, or the empty space under the rows.
struct TreeContextMenu {
    ix: Option<usize>,
    position: gpui::Point<gpui::Pixels>,
}

const ROW_HEIGHT: f32 = 24.0;
/// Blank space kept at the bottom of the panel, so a tree that fills it
/// still has somewhere to drop (or right-click) for the root.
const ROOT_DROP_SPACE: f32 = 36.0;
/// A drag held this close to the top or bottom of the rows scrolls them.
const DRAG_SCROLL_EDGE: f32 = 28.0;
const DRAG_SCROLL_TICK: Duration = Duration::from_millis(16);

/// How far a drag held at `y` scrolls the rows each tick, in pixels:
/// nothing in the middle of the list, faster the deeper into an edge (or
/// past it). Negative is towards the top.
fn drag_scroll_step(y: f32, top: f32, bottom: f32) -> f32 {
    let depth = if y < top + DRAG_SCROLL_EDGE {
        y - (top + DRAG_SCROLL_EDGE)
    } else if y > bottom - DRAG_SCROLL_EDGE {
        y - (bottom - DRAG_SCROLL_EDGE)
    } else {
        return 0.0;
    };
    (depth * 0.4).clamp(-16.0, 16.0)
}

/// How often the git decorations refresh on their own. Filesystem events
/// and root changes refresh sooner.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// What the drawer's footer line is collecting, when active. Names and
/// paths are typed in the window's prompt modal instead (`TreeEvent::Prompt`).
enum InputMode {
    Filter,
    ConfirmDelete { target: PathBuf },
}

pub struct FileTree {
    pub root: PathBuf,
    /// Linux: the window's ☰ menu button floats over the header's corner,
    /// so the root name is indented to clear it. The window sets this from
    /// its layout (`Oxide::app_menu_corner`).
    pub app_menu_clearance: bool,
    nodes: HashMap<PathBuf, Node>,
    visible: Vec<VisibleRow>,
    selected: usize,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    config: Rc<Config>,
    theme: Rc<Theme>,
    show_hidden: bool,
    scanning: HashSet<PathBuf>,
    watcher: Option<TreeWatcher>,
    input: Option<InputMode>,
    filter: LineEdit,
    /// Select this path (by name) when its parent's next scan lands.
    pending_select: Option<PathBuf>,
    /// Keep expanding towards this path as its ancestors' scans land.
    pending_reveal: Option<PathBuf>,
    /// Git state per absolute path, directories rolled up. Replaced
    /// wholesale on every refresh so a render never sees a half-built map.
    git: Rc<HashMap<PathBuf, GitFileStatus>>,
    git_refresh_scheduled: bool,
    git_refresh_running: bool,
    context_menu: Option<TreeContextMenu>,
    /// Pixels per tick the rows scroll while a drag is held near an edge.
    drag_scroll: f32,
    drag_scrolling: bool,
}

impl EventEmitter<TreeEvent> for FileTree {}

impl Focusable for FileTree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl FileTree {
    pub fn new(
        root: PathBuf,
        config: Rc<Config>,
        theme: Rc<Theme>,
        cx: &mut Context<Self>,
    ) -> Self {
        let show_hidden = config.tree.show_hidden;
        let mut this = Self {
            root: root.clone(),
            app_menu_clearance: false,
            nodes: HashMap::new(),
            visible: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            config,
            theme,
            show_hidden,
            scanning: HashSet::new(),
            watcher: None,
            input: None,
            filter: LineEdit::default(),
            pending_select: None,
            pending_reveal: None,
            git: Rc::new(HashMap::new()),
            git_refresh_scheduled: false,
            git_refresh_running: false,
            context_menu: None,
            drag_scroll: 0.0,
            drag_scrolling: false,
        };

        if let Some((watcher, mut rx)) = watch::create() {
            this.watcher = Some(watcher);
            cx.spawn(async move |tree, cx| {
                while let Some(dirs) = rx.next().await {
                    let alive = tree
                        .update(cx, |tree, cx| {
                            for dir in dirs {
                                // Rescan only affected, already-scanned dirs.
                                if tree.nodes.get(&dir).is_some_and(|n| n.children.is_some())
                                    || dir == tree.root
                                {
                                    tree.scan_dir(dir, cx);
                                }
                            }
                            // Anything changing on disk may change git state;
                            // one refresh after the burst settles.
                            tree.schedule_git_refresh(Duration::from_millis(800), cx);
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
            })
            .detach();
        }

        this.set_root_node(root);
        this.scan_dir(this.root.clone(), cx);
        this.refresh_git(cx);
        // A slow heartbeat catches changes the watcher doesn't see (commits,
        // stashes, checkouts under .git, which isn't watched).
        cx.spawn(async move |tree, cx| {
            loop {
                let timer = match tree.update(cx, |_, cx| {
                    cx.background_executor().timer(GIT_REFRESH_INTERVAL)
                }) {
                    Ok(timer) => timer,
                    Err(_) => break,
                };
                timer.await;
                if tree.update(cx, |tree, cx| tree.refresh_git(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    // --- Git decorations ---

    fn schedule_git_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if self.git_refresh_scheduled {
            return;
        }
        self.git_refresh_scheduled = true;
        let timer = cx.background_executor().timer(delay);
        cx.spawn(async move |tree, cx| {
            timer.await;
            tree.update(cx, |tree, cx| {
                tree.git_refresh_scheduled = false;
                tree.refresh_git(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Re-read `git status` for the root on the background pool. Outside a
    /// repo, or when decorations are off, the map is simply empty.
    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        if !self.config.tree.git_status {
            if !self.git.is_empty() {
                self.git = Rc::new(HashMap::new());
                cx.notify();
            }
            return;
        }
        if self.git_refresh_running {
            return;
        }
        self.git_refresh_running = true;
        let root = self.root.clone();
        let bg = cx.background_executor().clone();
        cx.spawn(async move |tree, cx| {
            let result = bg.spawn(async move { git::file_statuses(&root) }).await;
            tree.update(cx, |tree, cx| {
                tree.git_refresh_running = false;
                let map = result.unwrap_or_default();
                if *tree.git != map {
                    tree.git = Rc::new(map);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The git colour for a row, from the ANSI palette so themes carry it.
    fn git_color(&self, path: &Path) -> Option<gpui::Hsla> {
        let theme = &self.theme;
        Some(match self.git.get(path)? {
            GitFileStatus::Modified => theme.ansi[3],
            GitFileStatus::Added => theme.ansi[2],
            GitFileStatus::Renamed => theme.ansi[6],
            GitFileStatus::Untracked => blend(theme.ansi[2], theme.background, 0.4),
            GitFileStatus::Deleted => theme.ansi[1],
            GitFileStatus::Conflicted => theme.ansi[9],
        })
    }

    // --- Reveal ---

    /// Expand and scroll to `path`, re-rooting first if it lies outside the
    /// tree. Directories that haven't been scanned yet are expanded as their
    /// scans land, so this may take a few round trips.
    pub fn reveal(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !path.exists() {
            return;
        }
        if !path.starts_with(&self.root) {
            let new_root = if path.is_dir() {
                path.clone()
            } else {
                path.parent().map(Path::to_path_buf).unwrap_or(path.clone())
            };
            self.set_root(new_root, cx);
        }
        self.filter = LineEdit::default();
        // Walk root → path, expanding each directory on the way.
        let mut cursor = self.root.clone();
        let Ok(rest) = path.strip_prefix(&self.root) else {
            return;
        };
        for component in rest.components() {
            cursor = cursor.join(component);
            if cursor == path && !path.is_dir() {
                break;
            }
            match self.nodes.get(&cursor) {
                Some(node) if node.children.is_some() => {
                    if let Some(node) = self.nodes.get_mut(&cursor) {
                        node.expanded = true;
                    }
                }
                Some(_) => {
                    self.pending_reveal = Some(path);
                    self.expand_dir(cursor, cx);
                    return;
                }
                None => {
                    // Its parent hasn't been scanned yet: expanding that
                    // parent (already on the walk) will bring it in.
                    self.pending_reveal = Some(path);
                    self.rebuild(cx);
                    return;
                }
            }
        }
        self.pending_reveal = None;
        self.rebuild(cx);
        if let Some(ix) = self.index_of(&path) {
            self.select(ix, cx);
        }
    }

    // --- The tree as an input device ---

    fn on_yank_path(&mut self, _: &TreeYankPath, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row().filter(|r| r.kind == RowKind::Entry) {
            cx.emit(TreeEvent::InsertPath {
                path: row.path.clone(),
                absolute: false,
            });
        }
    }

    fn on_yank_absolute(&mut self, _: &TreeYankAbsolute, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row().filter(|r| r.kind == RowKind::Entry) {
            cx.emit(TreeEvent::InsertPath {
                path: row.path.clone(),
                absolute: true,
            });
        }
    }

    fn on_copy_path(&mut self, _: &TreeCopyPath, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row().filter(|r| r.kind == RowKind::Entry) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                row.path.to_string_lossy().to_string(),
            ));
        }
    }

    fn on_cd_here(&mut self, _: &TreeCdHere, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row().filter(|r| r.kind == RowKind::Entry) {
            let dir = if row.is_dir {
                row.path.clone()
            } else {
                row.path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(row.path.clone())
            };
            cx.emit(TreeEvent::CdShell(dir));
        }
    }

    fn on_reveal_in_finder(
        &mut self,
        _: &TreeRevealFinder,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.selected_row().filter(|r| r.kind == RowKind::Entry) {
            _cx.reveal_path(&row.path);
        }
    }

    pub fn set_config(&mut self, config: Rc<Config>, theme: Rc<Theme>, cx: &mut Context<Self>) {
        self.config = config;
        self.theme = theme;
        self.rebuild(cx);
    }

    fn set_root_node(&mut self, root: PathBuf) {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.to_string_lossy().to_string());
        self.nodes.entry(root.clone()).or_insert(Node {
            name,
            is_dir: true,
            expanded: true,
            children: None,
            is_hidden: false,
            is_ignored: false,
            truncated: 0,
        });
        let node = self.nodes.get_mut(&root).unwrap();
        node.expanded = true;
        self.root = root;
        if let Some(watcher) = &mut self.watcher {
            watcher.watch(&self.root);
        }
    }

    fn scan_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.scanning.insert(dir.clone()) {
            return;
        }
        let respect_gitignore = self.config.tree.respect_gitignore;
        let bg = cx.background_executor().clone();
        cx.spawn(async move |tree, cx| {
            let scan_dir = dir.clone();
            let result = bg
                .spawn(async move { scan::read_dir_sorted(&scan_dir, respect_gitignore) })
                .await;
            tree.update(cx, |tree, cx| {
                tree.scanning.remove(&dir);
                tree.apply_scan(dir, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply_scan(&mut self, dir: PathBuf, result: scan::ScanResult, cx: &mut Context<Self>) {
        let select_after = self
            .pending_select
            .take_if(|p| p.parent() == Some(dir.as_path()))
            .filter(|p| result.entries.iter().any(|(path, _)| path == p));
        let new_children: Vec<PathBuf> = result.entries.iter().map(|(p, _)| p.clone()).collect();

        // Diff and patch: drop removed subtrees, keep surviving nodes so
        // expansion state is preserved, insert new ones.
        if let Some(node) = self.nodes.get(&dir)
            && let Some(old_children) = node.children.clone()
        {
            for old in old_children {
                if !new_children.contains(&old) {
                    remove_subtree(&mut self.nodes, &old);
                }
            }
        }
        // A walk rooted inside an ignored directory doesn't test that root
        // against its parents' rules, so everything under it inherits the flag.
        let dir_ignored = self.nodes.get(&dir).is_some_and(|n| n.is_ignored);
        for (path, mut node) in result.entries {
            node.is_ignored |= dir_ignored;
            match self.nodes.get_mut(&path) {
                Some(existing) => {
                    existing.name = node.name;
                    existing.is_dir = node.is_dir;
                    existing.is_hidden = node.is_hidden;
                    existing.is_ignored = node.is_ignored;
                }
                None => {
                    self.nodes.insert(path, node);
                }
            }
        }
        if let Some(node) = self.nodes.get_mut(&dir) {
            node.children = Some(new_children);
            node.truncated = result.truncated;
        }
        self.rebuild(cx);
        if let Some(path) = select_after
            && let Some(ix) = self.index_of(&path)
        {
            self.select(ix, cx);
        }
        if let Some(target) = self.pending_reveal.take()
            && target.starts_with(&dir)
        {
            self.reveal(target, cx);
        }
    }

    /// Rebuild `visible`, preserving selection by path — never by index.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let selected_path = self.visible.get(self.selected).map(|r| r.path.clone());
        self.visible = rebuild_visible(&self.root, &self.nodes, self.show_hidden);
        if !self.filter.text.is_empty() {
            self.visible = filter_rows(std::mem::take(&mut self.visible), &self.filter.text);
        }
        if let Some(path) = selected_path {
            self.selected = self.index_of(&path).unwrap_or_else(|| {
                // Nearest surviving ancestor.
                let mut p: &Path = &path;
                while let Some(parent) = p.parent() {
                    if let Some(ix) = self.index_of(parent) {
                        return ix;
                    }
                    p = parent;
                }
                0
            });
        }
        if !self.visible.is_empty() {
            self.selected = self.selected.min(self.visible.len() - 1);
        } else {
            self.selected = 0;
        }
        cx.notify();
    }

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.visible.iter().position(|r| r.path == path)
    }

    fn selected_row(&self) -> Option<&VisibleRow> {
        self.visible.get(self.selected)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = index.min(self.visible.len() - 1);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn viewport_rows(&self) -> usize {
        let state = self.scroll.0.borrow();
        let viewport = state.base_handle.bounds().size.height;
        let item = state
            .last_item_size
            .map(|s| f32::from(s.item.height))
            .unwrap_or(24.0);
        let rows = (f32::from(viewport) / item.max(1.0)) as usize;
        rows.max(2)
    }

    fn expand_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let needs_scan = match self.nodes.get_mut(&path) {
            Some(node) if node.is_dir => {
                node.expanded = true;
                node.children.is_none()
            }
            _ => return,
        };
        if needs_scan {
            self.scan_dir(path.clone(), cx);
        }
        if let Some(watcher) = &mut self.watcher {
            watcher.watch(&path);
        }
        self.rebuild(cx);
    }

    fn collapse_dir(&mut self, path: &PathBuf, cx: &mut Context<Self>) {
        if let Some(node) = self.nodes.get_mut(path) {
            node.expanded = false;
        }
        if let Some(watcher) = &mut self.watcher
            && *path != self.root
        {
            watcher.unwatch(path);
        }
        self.rebuild(cx);
    }

    fn open_row(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        if row.kind != RowKind::Entry {
            return;
        }
        if row.is_dir {
            if row.expanded {
                self.collapse_dir(&row.path, cx);
            } else {
                self.expand_dir(row.path, cx);
            }
        } else {
            cx.emit(TreeEvent::OpenFile(row.path));
        }
    }

    fn select_parent(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row()
            && let Some(parent) = row.path.parent()
            && let Some(ix) = self.index_of(parent)
        {
            self.select(ix, cx);
        }
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if root == self.root || !root.is_dir() {
            return;
        }
        if let Some(watcher) = &mut self.watcher {
            watcher.unwatch_all();
        }
        self.set_root_node(root.clone());
        self.selected = 0;
        // Re-watch expanded descendants that survive under the new root.
        let expanded: Vec<PathBuf> = self
            .nodes
            .iter()
            .filter(|(p, n)| n.expanded && n.children.is_some() && p.starts_with(&root))
            .map(|(p, _)| p.clone())
            .collect();
        if let Some(watcher) = &mut self.watcher {
            for dir in &expanded {
                watcher.watch(dir);
            }
        }
        self.scan_dir(root, cx);
        self.git = Rc::new(HashMap::new());
        self.refresh_git(cx);
        self.rebuild(cx);
        cx.emit(TreeEvent::RootChanged(self.root.clone()));
    }

    // --- Actions ---

    fn on_down(&mut self, _: &TreeDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn on_up(&mut self, _: &TreeUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn on_top(&mut self, _: &TreeTop, _w: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn on_bottom(&mut self, _: &TreeBottom, _w: &mut Window, cx: &mut Context<Self>) {
        self.select(self.visible.len().saturating_sub(1), cx);
    }

    fn on_half_page_down(&mut self, _: &TreeHalfPageDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + self.viewport_rows() / 2, cx);
    }

    fn on_half_page_up(&mut self, _: &TreeHalfPageUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(self.viewport_rows() / 2), cx);
    }

    /// `l`: collapsed dir -> expand; expanded dir -> first child; file -> open.
    fn on_expand(&mut self, _: &TreeExpand, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        if row.kind != RowKind::Entry {
            return;
        }
        if row.is_dir {
            if !row.expanded {
                self.expand_dir(row.path, cx);
            } else if let Some(next) = self.visible.get(self.selected + 1)
                && next.path.parent() == Some(&row.path)
            {
                self.select(self.selected + 1, cx);
            }
        } else {
            cx.emit(TreeEvent::OpenFile(row.path));
        }
    }

    /// `h`: expanded dir -> collapse; else -> parent row; top-level -> no-op.
    fn on_collapse(&mut self, _: &TreeCollapse, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        if row.is_dir && row.expanded && row.kind == RowKind::Entry {
            self.collapse_dir(&row.path, cx);
        } else if row.depth > 0 {
            self.select_parent(cx);
        }
    }

    /// `enter` / `o`: a file opens in `$EDITOR`; a directory toggles open.
    /// (With `follow_cwd` a shell `cd` re-roots the tree, so cd-ing is left
    /// to `c`, the context menu, and the `tree::cd` action.)
    fn on_open(&mut self, _: &TreeOpen, _w: &mut Window, cx: &mut Context<Self>) {
        self.open_row(cx);
    }

    fn on_parent(&mut self, _: &TreeParent, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_parent(cx);
    }

    /// `c`: re-root at the selection (or its parent for files); cd the shell.
    fn on_set_root(&mut self, _: &TreeSetRoot, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        let target = if row.is_dir {
            row.path
        } else {
            match row.path.parent() {
                Some(p) => p.to_path_buf(),
                None => return,
            }
        };
        self.set_root(target.clone(), cx);
        cx.emit(TreeEvent::ChangedRoot(target));
    }

    fn on_root_up(&mut self, _: &TreeRootUp, _w: &mut Window, cx: &mut Context<Self>) {
        let old_root = self.root.clone();
        let Some(parent) = old_root.parent().map(Path::to_path_buf) else {
            return;
        };
        // Keep the old root expanded so re-rooting upward feels like zooming out.
        self.set_root(parent.clone(), cx);
        if let Some(node) = self.nodes.get_mut(&old_root) {
            node.expanded = true;
        }
        cx.emit(TreeEvent::ChangedRoot(parent));
        self.rebuild(cx);
        if let Some(ix) = self.index_of(&old_root) {
            self.select(ix, cx);
        }
    }

    fn on_toggle_hidden(&mut self, _: &TreeToggleHidden, _w: &mut Window, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.rebuild(cx);
    }

    fn on_refresh(&mut self, _: &TreeRefresh, _w: &mut Window, cx: &mut Context<Self>) {
        let scanned: Vec<PathBuf> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.children.is_some())
            .map(|(p, _)| p.clone())
            .collect();
        for dir in scanned {
            self.scan_dir(dir, cx);
        }
    }

    // --- Filter and file operations ---

    fn on_filter(&mut self, _: &TreeFilter, _w: &mut Window, cx: &mut Context<Self>) {
        self.input = Some(InputMode::Filter);
        cx.notify();
    }

    fn on_add(&mut self, _: &TreeAdd, _w: &mut Window, cx: &mut Context<Self>) {
        // An open directory takes the new entry; a closed one gets a sibling.
        // Otherwise a tree of nothing but directories has no row that adds
        // at the top level.
        let parent = match self.selected_row() {
            Some(row) if row.kind == RowKind::Entry => {
                if row.is_dir && row.expanded {
                    row.path.clone()
                } else {
                    row.path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.root.clone())
                }
            }
            _ => self.root.clone(),
        };
        self.prompt_add(parent, cx);
    }

    fn prompt_add(&mut self, parent: PathBuf, cx: &mut Context<Self>) {
        let dir = parent
            .strip_prefix(&self.root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| format!("{}/", p.to_string_lossy()))
            .unwrap_or_else(|| "./".into());
        cx.emit(TreeEvent::Prompt {
            prompt: TreePrompt::Add { parent },
            title: format!("New file or directory in {dir}"),
            hint: "end with / for a directory",
            initial: LineEdit::default(),
        });
    }

    fn on_rename(&mut self, _: &TreeRename, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row()
            && row.kind == RowKind::Entry
        {
            let name = file_name(&row.path);
            cx.emit(TreeEvent::Prompt {
                prompt: TreePrompt::Rename {
                    target: row.path.clone(),
                },
                title: format!("Rename {name}"),
                hint: "",
                initial: LineEdit::new(name),
            });
        }
    }

    fn on_move(&mut self, _: &TreeMove, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row()
            && row.kind == RowKind::Entry
            && row.path != self.root
        {
            let buffer = row
                .path
                .strip_prefix(&self.root)
                .unwrap_or(&row.path)
                .to_string_lossy()
                .to_string();
            cx.emit(TreeEvent::Prompt {
                prompt: TreePrompt::Move {
                    target: row.path.clone(),
                },
                title: format!("Move {}", file_name(&row.path)),
                hint: "relative to the tree root",
                initial: LineEdit::at_start(buffer),
            });
        }
    }

    /// The answer to a `TreeEvent::Prompt`, trimmed and not empty.
    pub fn apply_prompt(&mut self, prompt: TreePrompt, text: String, cx: &mut Context<Self>) {
        match prompt {
            TreePrompt::Add { parent } => self.create_entry(parent, text, cx),
            TreePrompt::Rename { target } if !text.contains('/') => {
                self.rename_entry(target, text, cx)
            }
            TreePrompt::Rename { .. } => {}
            TreePrompt::Move { target } => {
                let dest = resolve_dest(&self.root, &text);
                self.move_entry(target, dest, cx);
            }
        }
    }

    fn on_preview(&mut self, _: &TreePreview, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row()
            && row.kind == RowKind::Entry
            && !row.is_dir
            && crate::markdown::is_markdown(&row.path)
        {
            cx.emit(TreeEvent::PreviewMarkdown(row.path.clone()));
        }
    }

    /// A drag over the rows: near their top or bottom edge they scroll, so
    /// a row that's out of view can still be dropped on.
    fn on_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<TreeDrag>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (at, bounds) = (event.event.position, event.bounds);
        // Beside the drawer the drag is on its way to a terminal pane.
        self.drag_scroll = if at.x >= bounds.left() && at.x <= bounds.right() {
            drag_scroll_step(at.y.into(), bounds.top().into(), bounds.bottom().into())
        } else {
            0.0
        };
        if self.drag_scroll == 0.0 || self.drag_scrolling {
            return;
        }
        // A timer, not the mouse events: a pointer held still keeps scrolling.
        self.drag_scrolling = true;
        cx.spawn(async move |tree, cx| {
            loop {
                let Ok(timer) =
                    tree.update(cx, |_, cx| cx.background_executor().timer(DRAG_SCROLL_TICK))
                else {
                    break;
                };
                timer.await;
                if !tree
                    .update(cx, |tree, cx| tree.drag_scroll_tick(cx))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// One step of the drag scroll; false once the drag has left the edge
    /// or ended.
    fn drag_scroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.drag_scroll == 0.0 || !cx.has_active_drag() {
            self.drag_scroll = 0.0;
            self.drag_scrolling = false;
            return false;
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let viewport = f32::from(handle.bounds().size.height);
        // As far as the last row sitting on top of the root's drop space.
        let content = self.visible.len() as f32 * ROW_HEIGHT + ROOT_DROP_SPACE;
        let mut offset = handle.offset();
        let y = f32::from(offset.y) - self.drag_scroll;
        offset.y = px(y.clamp((viewport - content).min(0.0), 0.0));
        handle.set_offset(offset);
        cx.notify();
        true
    }

    fn open_context_menu(
        &mut self,
        ix: Option<usize>,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if let Some(ix) = ix {
            self.select(ix, cx);
        }
        self.context_menu = Some(TreeContextMenu {
            ix,
            position: event.position,
        });
        // A row's menu, not also the one for the space it sits on.
        cx.stop_propagation();
        cx.notify();
    }

    /// A row dropped on another: into a directory, or beside a file.
    fn drop_on_row(&mut self, ix: usize, dragged: &Path, cx: &mut Context<Self>) {
        let Some(row) = self.visible.get(ix) else {
            return;
        };
        let dir = if row.is_dir {
            Some(row.path.clone())
        } else {
            row.path.parent().map(Path::to_path_buf)
        };
        if let Some(dir) = dir {
            self.move_entry(dragged.to_path_buf(), dir, cx);
        }
    }

    fn on_delete(&mut self, _: &TreeDelete, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row()
            && row.kind == RowKind::Entry
            && row.path != self.root
        {
            self.input = Some(InputMode::ConfirmDelete {
                target: row.path.clone(),
            });
            cx.notify();
        }
    }

    /// escape is a dismiss chain: input line, then filter, then hand focus back.
    fn on_escape(&mut self, _: &TreeEscape, _w: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            self.input = None;
            cx.notify();
        } else if !self.filter.text.is_empty() {
            self.filter = LineEdit::default();
            self.rebuild(cx);
        } else {
            cx.emit(TreeEvent::FocusTerminal);
        }
    }

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(mut input) = self.input.take() else {
            return;
        };
        let ks = &event.keystroke;
        match &mut input {
            InputMode::Filter => match ks.key.as_str() {
                "escape" => {
                    self.filter = LineEdit::default();
                    self.rebuild(cx);
                }
                "enter" => {} // keep the filter applied, leave input mode
                // Backspace on nothing leaves input mode too.
                "backspace" if self.filter.text.is_empty() => {}
                _ => {
                    if self.filter.handle(ks) {
                        self.rebuild(cx);
                    }
                    self.input = Some(input);
                }
            },
            InputMode::ConfirmDelete { target } => {
                // Anything but y cancels.
                if ks.key.as_str() == "y" {
                    let target = target.clone();
                    self.delete_entry(target, cx);
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn create_entry(&mut self, parent: PathBuf, name: String, cx: &mut Context<Self>) {
        let is_dir = name.ends_with('/');
        let path = parent.join(name.trim_end_matches('/'));
        let result = if is_dir {
            std::fs::create_dir_all(&path)
        } else {
            path.parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .map(|_| ())
                .and_then(|_| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map(|_| ())
                })
        };
        match result {
            Ok(()) => {
                if let Some(node) = self.nodes.get_mut(&parent) {
                    node.expanded = true;
                }
                // Select the created entry once its parent's rescan lands.
                self.pending_select = Some(path.clone());
                let scan_parent = path.parent().map(Path::to_path_buf).unwrap_or(parent);
                self.scan_dir(scan_parent, cx);
            }
            Err(e) => eprintln!("oxide: create {path:?}: {e}"),
        }
    }

    fn rename_entry(&mut self, target: PathBuf, name: String, cx: &mut Context<Self>) {
        let Some(parent) = target.parent().map(Path::to_path_buf) else {
            return;
        };
        let new_path = parent.join(&name);
        match std::fs::rename(&target, &new_path) {
            Ok(()) => {
                remove_subtree(&mut self.nodes, &target);
                self.pending_select = Some(new_path);
                self.scan_dir(parent, cx);
            }
            Err(e) => eprintln!("oxide: rename {target:?}: {e}"),
        }
    }

    /// `mv` semantics: an existing directory as the destination means "into
    /// it"; a missing parent is created; nothing is overwritten.
    fn move_entry(&mut self, target: PathBuf, dest: PathBuf, cx: &mut Context<Self>) {
        let dest = if dest.is_dir() {
            match target.file_name() {
                Some(name) => dest.join(name),
                None => return,
            }
        } else {
            dest
        };
        // Where it already is — or, for a directory, somewhere inside itself.
        if dest.starts_with(&target) {
            return;
        }
        if dest.exists() {
            eprintln!("oxide: move {target:?}: {dest:?} already exists");
            return;
        }
        let (Some(old_parent), Some(new_parent)) = (
            target.parent().map(Path::to_path_buf),
            dest.parent().map(Path::to_path_buf),
        ) else {
            return;
        };
        let result =
            std::fs::create_dir_all(&new_parent).and_then(|_| std::fs::rename(&target, &dest));
        match result {
            Ok(()) => {
                remove_subtree(&mut self.nodes, &target);
                if let Some(node) = self.nodes.get_mut(&new_parent) {
                    node.expanded = true;
                }
                self.pending_select = Some(dest);
                self.scan_dir(old_parent.clone(), cx);
                if new_parent != old_parent && self.nodes.contains_key(&new_parent) {
                    self.scan_dir(new_parent, cx);
                }
            }
            Err(e) => eprintln!("oxide: move {target:?}: {e}"),
        }
    }

    /// Move to the trash when possible (recoverable — `~/.Trash` on macOS,
    /// the XDG trash on Linux); hard-delete only as a fallback, for a volume
    /// with no trash directory.
    fn delete_entry(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = target.parent().map(Path::to_path_buf) else {
            return;
        };
        let trashed = trash::delete(&target).ok();
        if trashed.is_none() {
            let result = if target.is_dir() {
                std::fs::remove_dir_all(&target)
            } else {
                std::fs::remove_file(&target)
            };
            if let Err(e) = result {
                eprintln!("oxide: delete {target:?}: {e}");
            }
        }
        remove_subtree(&mut self.nodes, &target);
        self.scan_dir(parent, cx);
    }

    /// See [`FooterText`].
    fn footer_text(&self) -> Option<FooterText> {
        match &self.input {
            Some(InputMode::Filter) => {
                let (before, after) = self.filter.split();
                Some((
                    "filter: ".into(),
                    Some((before.into(), after.into())),
                    String::new(),
                ))
            }
            Some(InputMode::ConfirmDelete { target }) => Some((
                format!("delete {}? (y/n)", file_name(target)),
                None,
                String::new(),
            )),
            None if !self.filter.text.is_empty() => Some((
                format!("filter: {}", self.filter.text),
                None,
                "   (esc clears)".into(),
            )),
            None => None,
        }
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let theme = self.theme.clone();
        let focused = self.focus_handle.is_focused(window);
        let indent = self.config.tree.indent;
        let icons = self.config.tree.icons;
        let mut rows = Vec::new();
        for ix in range {
            let Some(row) = self.visible.get(ix) else {
                continue;
            };
            let is_selected = ix == self.selected;
            let mut selection_bg = theme.selection_bg;
            if !focused {
                // Dim the selection when the drawer doesn't own the cursor.
                selection_bg.a = 0.45;
            }
            let (chevron, icon) = match (&row.kind, row.is_dir, row.expanded) {
                (RowKind::Entry, true, true) => ("▾", if icons { "\u{f07c}" } else { "" }),
                (RowKind::Entry, true, false) => ("▸", if icons { "\u{f07b}" } else { "" }),
                (RowKind::Entry, false, _) => (" ", if icons { "\u{f15b}" } else { "" }),
                _ => (" ", ""),
            };
            let label: SharedString = match &row.kind {
                RowKind::Entry => self
                    .nodes
                    .get(&row.path)
                    .map(|n| n.name.clone())
                    .unwrap_or_default()
                    .into(),
                RowKind::Loading => "…".into(),
                RowKind::Truncated(n) => format!("… {n} more").into(),
            };
            let dim = blend(theme.foreground, theme.background, 0.45);
            let text_color = match &row.kind {
                RowKind::Entry if row.is_dir => theme.foreground,
                RowKind::Entry => blend(theme.foreground, theme.background, 0.15),
                _ => dim,
            };
            let ignored = self.nodes.get(&row.path).is_some_and(|n| n.is_ignored);
            let icon_color = if row.is_dir && !ignored {
                theme.ansi[4]
            } else {
                dim
            };
            let text_color = match (&row.kind, self.git_color(&row.path)) {
                _ if ignored => dim,
                (RowKind::Entry, Some(color)) => color,
                _ => text_color,
            };
            let mut drop_bg = theme.ansi[4];
            drop_bg.a = 0.25;
            let drag_path = row.path.clone();
            let drag_name: SharedString = label.clone();
            let full_name: SharedString = label.clone();
            rows.push(
                div()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .pl(px(8.0 + row.depth as f32 * indent))
                    .pr_2()
                    .when(is_selected, |d| d.bg(selection_bg))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |tree, event: &gpui::MouseDownEvent, window, cx| {
                            window.focus(&tree.focus_handle);
                            tree.select(ix, cx);
                            if event.click_count > 1 {
                                // Directories already toggled on the first mouse-down.
                                if tree.selected_row().is_some_and(|r| !r.is_dir) {
                                    tree.open_row(cx);
                                }
                            } else if let Some(row) = tree.selected_row().cloned()
                                && row.is_dir
                                && row.kind == RowKind::Entry
                            {
                                if row.expanded {
                                    tree.collapse_dir(&row.path, cx);
                                } else {
                                    tree.expand_dir(row.path, cx);
                                }
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |tree, event: &gpui::MouseDownEvent, window, cx| {
                            tree.open_context_menu(Some(ix), event, window, cx);
                        }),
                    )
                    // The whole name on hover, for when the row cut it short.
                    .when(row.kind == RowKind::Entry, |d| {
                        d.tooltip(move |_window, cx| {
                            cx.new(|_| DragLabel(full_name.clone())).into()
                        })
                    })
                    .when(row.kind == RowKind::Entry, |d| {
                        d.on_drag(
                            TreeDrag {
                                path: drag_path.clone(),
                            },
                            move |_, _, _window, cx| cx.new(|_| DragLabel(drag_name.clone())),
                        )
                        // Dropped on a directory it moves in; on a file, in
                        // beside it.
                        .drag_over::<TreeDrag>(move |style, _, _, _| style.bg(drop_bg))
                        .on_drop(cx.listener(
                            move |tree, drag: &TreeDrag, _w, cx| {
                                tree.drop_on_row(ix, &drag.path, cx);
                            },
                        ))
                    })
                    .child(div().w(px(12.0)).flex_none().text_color(dim).child(chevron))
                    .when(icons, |d| {
                        d.child(div().flex_none().text_color(icon_color).child(icon))
                    })
                    // One line, ending in an ellipsis: a wrapped name spills
                    // over the fixed-height row below it.
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(text_color)
                            .child(label),
                    )
                    .into_any_element(),
            );
        }
        rows
    }
}

/// The context-menu label for `App::reveal_path`: Finder on macOS, the
/// default file manager (via `org.freedesktop.FileManager1`) on Linux.
const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else {
    "Reveal in File Manager"
};

/// The footer line: a label, the text either side of the caret when
/// something is being typed, and a hint.
type FooterText = (String, Option<(String, String)>, String);

impl FileTree {
    fn render_context_menu(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let Some(menu) = &self.context_menu else {
            return div().into_any_element();
        };
        // No row: the root itself.
        let (path, is_dir, is_root) = match menu.ix {
            Some(ix) => match self.visible.get(ix).filter(|r| r.kind == RowKind::Entry) {
                Some(row) => (row.path.clone(), row.is_dir, false),
                None => return div().into_any_element(),
            },
            None => (self.root.clone(), true, true),
        };
        let ix = menu.ix.unwrap_or(0);
        let is_markdown = !is_dir && crate::markdown::is_markdown(&path);
        let theme = &self.theme;
        let panel_bg = blend(theme.background, gpui::black(), 0.2);
        let border = blend(theme.foreground, theme.background, 0.8);
        let mut hover_bg = theme.selection_bg;
        hover_bg.a = 0.6;

        // Keep the menu on screen when the click lands near an edge.
        let viewport = window.viewport_size();
        let items = match (is_root, is_dir, is_markdown) {
            (true, ..) => 3,
            (_, true, _) | (_, _, true) => 8,
            _ => 7,
        };
        let (menu_w, menu_h) = (200.0, items as f32 * 28.0 + 24.0);
        let x = f32::from(menu.position.x).min(f32::from(viewport.width) - menu_w - 8.0);
        let y = f32::from(menu.position.y).min(f32::from(viewport.height) - menu_h - 8.0);

        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .child(label)
        };
        let backdrop = div()
            .w(viewport.width)
            .h(viewport.height)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|tree, _: &gpui::MouseDownEvent, _w, cx| {
                    tree.context_menu = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|tree, _: &gpui::MouseDownEvent, _w, cx| {
                    tree.context_menu = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(menu_w))
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(panel_bg)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .text_size(px(13.0))
                    .text_color(theme.foreground)
                    .on_mouse_down(MouseButton::Left, |_: &gpui::MouseDownEvent, _w, cx| {
                        cx.stop_propagation()
                    })
                    // A directory's first item re-roots the tree, which (with
                    // follow_cwd) also cd's the shell — a separate "cd here"
                    // would be indistinguishable. `tree::cd` exists for
                    // anyone who turns follow_cwd off and wants it bound.
                    .when(!is_dir, |d| {
                        d.child(item("tree-menu-open", "Open in $EDITOR").on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let path = path.clone();
                                move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                    tree.context_menu = None;
                                    cx.emit(TreeEvent::OpenFile(path.clone()));
                                    cx.notify();
                                }
                            }),
                        ))
                    })
                    .when(is_markdown, |d| {
                        d.child(item("tree-menu-preview", "Preview markdown").on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let path = path.clone();
                                move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                    tree.context_menu = None;
                                    cx.emit(TreeEvent::PreviewMarkdown(path.clone()));
                                    cx.notify();
                                }
                            }),
                        ))
                    })
                    .when(is_dir && !is_root, |d| {
                        d.child(item("tree-menu-root", "Set as tree root").on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let path = path.clone();
                                move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                    tree.context_menu = None;
                                    tree.set_root(path.clone(), cx);
                                    cx.emit(TreeEvent::ChangedRoot(path.clone()));
                                }
                            }),
                        ))
                    })
                    .when(is_dir, |d| {
                        d.child(item("tree-menu-add", "New file or folder…").on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let path = path.clone();
                                move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                    tree.context_menu = None;
                                    // Open, so what's added shows up.
                                    tree.expand_dir(path.clone(), cx);
                                    tree.prompt_add(path.clone(), cx);
                                }
                            }),
                        ))
                    })
                    .when(!is_root, |d| {
                        d.child(
                            item("tree-menu-insert", "Insert path at prompt").on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let path = path.clone();
                                    move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                        tree.context_menu = None;
                                        cx.emit(TreeEvent::InsertPath {
                                            path: path.clone(),
                                            absolute: false,
                                        });
                                        cx.notify();
                                    }
                                }),
                            ),
                        )
                    })
                    .child(item("tree-menu-copy", "Copy path").on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let path = path.clone();
                            move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                tree.context_menu = None;
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    path.to_string_lossy().to_string(),
                                ));
                                cx.notify();
                            }
                        }),
                    ))
                    .when(!is_root, |d| {
                        d.child(item("tree-menu-rename", "Rename…").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |tree, _: &gpui::MouseDownEvent, window, cx| {
                                tree.context_menu = None;
                                tree.select(ix, cx);
                                tree.on_rename(&TreeRename, window, cx);
                            }),
                        ))
                        .child(
                            item("tree-menu-move", "Move…").on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |tree, _: &gpui::MouseDownEvent, window, cx| {
                                    tree.context_menu = None;
                                    tree.select(ix, cx);
                                    tree.on_move(&TreeMove, window, cx);
                                }),
                            ),
                        )
                    })
                    .child(div().h(px(1.0)).my_1().bg(border))
                    .child(item("tree-menu-finder", REVEAL_LABEL).on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let path = path.clone();
                            move |tree, _: &gpui::MouseDownEvent, _w, cx| {
                                tree.context_menu = None;
                                cx.reveal_path(&path);
                                cx.notify();
                            }
                        }),
                    ))
                    .when(!is_root, |d| {
                        d.child(
                            item("tree-menu-delete", "Delete")
                                .text_color(theme.ansi[1])
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(
                                        move |tree, _: &gpui::MouseDownEvent, window, cx| {
                                            tree.context_menu = None;
                                            tree.select(ix, cx);
                                            // Same y/n confirmation `d` asks for.
                                            window.focus(&tree.focus_handle);
                                            tree.on_delete(&TreeDelete, window, cx);
                                        },
                                    ),
                                ),
                        )
                    }),
            );

        gpui::deferred(
            gpui::anchored()
                .position(gpui::point(px(0.0), px(0.0)))
                .child(backdrop),
        )
        .with_priority(1)
        .into_any_element()
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Where a typed move destination points: absolute and `~` paths as written,
/// anything else relative to the tree root.
fn resolve_dest(root: &Path, input: &str) -> PathBuf {
    let input = input.trim();
    let home = crate::app::home_dir();
    if let Some(rest) = input.strip_prefix("~/").or((input == "~").then_some(""))
        && let Some(home) = home
    {
        return home.join(rest);
    }
    if input.starts_with('/') {
        PathBuf::from(input)
    } else {
        root.join(input)
    }
}

/// Keep rows whose name matches the filter, plus their ancestor directories,
/// preserving the DFS structure.
fn filter_rows(rows: Vec<VisibleRow>, filter: &str) -> Vec<VisibleRow> {
    let needle = filter.to_lowercase();
    let mut keep = vec![false; rows.len()];
    let mut ancestors: Vec<usize> = Vec::new();
    for i in 0..rows.len() {
        while let Some(&top) = ancestors.last() {
            if rows[top].depth >= rows[i].depth {
                ancestors.pop();
            } else {
                break;
            }
        }
        let name = rows[i]
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.contains(&needle) {
            keep[i] = true;
            for &a in &ancestors {
                keep[a] = true;
            }
        }
        if rows[i].is_dir {
            ancestors.push(i);
        }
    }
    rows.into_iter()
        .zip(keep)
        .filter_map(|(row, k)| k.then_some(row))
        .collect()
}

impl Render for FileTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let header: SharedString = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.root.to_string_lossy().to_string())
            .into();
        let drawer_bg = translucent(
            blend(theme.background, gpui::black(), 0.25),
            self.config.window.opacity,
        );
        div()
            // Input modes switch context so bare-letter bindings don't fire
            // and keys fall through to the raw handler.
            .key_context(if self.input.is_some() {
                "FileTreeInput"
            } else {
                "FileTree"
            })
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(drawer_bg)
            .text_size(px(13.0))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::on_yank_path))
            .on_action(cx.listener(Self::on_yank_absolute))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_cd_here))
            .on_action(cx.listener(Self::on_reveal_in_finder))
            .on_action(cx.listener(Self::on_filter))
            .on_action(cx.listener(Self::on_add))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_move))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_preview))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_top))
            .on_action(cx.listener(Self::on_bottom))
            .on_action(cx.listener(Self::on_half_page_down))
            .on_action(cx.listener(Self::on_half_page_up))
            .on_action(cx.listener(Self::on_expand))
            .on_action(cx.listener(Self::on_collapse))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_parent))
            .on_action(cx.listener(Self::on_set_root))
            .on_action(cx.listener(Self::on_root_up))
            .on_action(cx.listener(Self::on_toggle_hidden))
            .on_action(cx.listener(Self::on_refresh))
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|tree, event: &gpui::MouseDownEvent, window, cx| {
                            tree.open_context_menu(None, event, window, cx);
                        }),
                    )
                    // Linux: the window's ☰ menu button floats over this
                    // corner; the root name steps aside so they don't overlap.
                    .when(self.app_menu_clearance, |d| {
                        d.pl(px(crate::app::APP_MENU_BUTTON_CLEARANCE))
                    })
                    .text_color(blend(theme.foreground, theme.background, 0.3))
                    .child(header),
            )
            .child(
                uniform_list(
                    "file-tree",
                    self.visible.len(),
                    cx.processor(Self::render_rows),
                )
                .flex_1()
                .pb(px(ROOT_DROP_SPACE))
                .track_scroll(self.scroll.clone())
                .on_drag_move(cx.listener(Self::on_drag_move))
                // Below the last row: the root itself. A row under the
                // pointer takes the drop, or the right-click, first.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|tree, event: &gpui::MouseDownEvent, window, cx| {
                        tree.open_context_menu(None, event, window, cx);
                    }),
                )
                .on_drop(cx.listener(|tree, drag: &TreeDrag, _w, cx| {
                    let root = tree.root.clone();
                    tree.move_entry(drag.path.clone(), root, cx);
                })),
            )
            .when_some(self.footer_text(), |d, (label, edit, hint)| {
                d.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(blend(theme.foreground, theme.background, 0.85))
                        .text_color(theme.ansi[3])
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .child(label)
                        .when_some(edit, |d, (before, after)| {
                            // A 1px caret between the halves, not a glyph:
                            // a glyph would take a whole monospace cell and
                            // read as a space.
                            d.child(before)
                                .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(theme.ansi[3]))
                                .child(after)
                        })
                        .child(hint),
                )
            })
            .when(self.context_menu.is_some(), |d| {
                d.child(self.render_context_menu(window, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::RowKind;

    fn row(path: &str, depth: usize, is_dir: bool) -> VisibleRow {
        VisibleRow {
            path: path.into(),
            depth,
            is_dir,
            expanded: is_dir,
            kind: RowKind::Entry,
        }
    }

    #[test]
    fn move_destinations_resolve_against_the_root() {
        let root = Path::new("/r");
        assert_eq!(resolve_dest(root, "src/a.rs"), PathBuf::from("/r/src/a.rs"));
        assert_eq!(
            resolve_dest(root, " /tmp/a.rs "),
            PathBuf::from("/tmp/a.rs")
        );
        let home = resolve_dest(root, "~/a.rs");
        assert!(home.is_absolute() && home.ends_with("a.rs") && !home.starts_with("/r"));
    }

    #[test]
    fn filter_keeps_matches_and_ancestors() {
        let rows = vec![
            row("/r/src", 0, true),
            row("/r/src/main.rs", 1, false),
            row("/r/src/lib.rs", 1, false),
            row("/r/docs", 0, true),
            row("/r/docs/guide.md", 1, false),
        ];
        let filtered = filter_rows(rows, "main");
        let names: Vec<_> = filtered
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        // main.rs matches; src is kept as its ancestor; docs subtree drops.
        assert_eq!(names, vec!["src", "main.rs"]);
    }

    #[test]
    fn a_drag_scrolls_the_rows_only_near_their_edges() {
        let (top, bottom) = (100.0, 500.0);
        assert_eq!(drag_scroll_step(300.0, top, bottom), 0.0);
        // Towards the top near the top, faster the closer; capped past it.
        assert!(drag_scroll_step(120.0, top, bottom) < 0.0);
        assert!(drag_scroll_step(105.0, top, bottom) < drag_scroll_step(120.0, top, bottom));
        assert_eq!(drag_scroll_step(-400.0, top, bottom), -16.0);
        assert!(drag_scroll_step(490.0, top, bottom) > 0.0);
    }

    #[test]
    fn filter_is_case_insensitive() {
        let rows = vec![row("/r/README.md", 0, false), row("/r/notes.txt", 0, false)];
        assert_eq!(filter_rows(rows, "readme").len(), 1);
    }
}
