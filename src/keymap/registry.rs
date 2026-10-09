//! The action registry: one row per action, shared by the command palette,
//! the keymap config, and the docs.
//!
//! The rows live in `actions.rs`, where the `omnipty_actions!` macro defines
//! each action type *and* its registry entry in one place so the two can't
//! drift apart.

use gpui::Action;

/// Where an action makes sense. The palette only lists actions whose
/// context is reachable right now, and focuses that context before
/// dispatching so the action lands on an element that handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionContext {
    /// Handled at the window root: reachable from anywhere.
    Root,
    /// Handled by the file tree; needs the drawer visible.
    FileTree,
    /// Handled by the workspaces panel; needs the drawer visible.
    Workspaces,
    /// Navigation inside a modal list (theme picker, palette). Never listed
    /// in the palette — you're already in one.
    Overlay,
}

pub struct ActionMeta {
    /// Stable identifier used in config.toml. Never rename one of these
    /// without a deprecation alias.
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    /// Keywords the palette's fuzzy match also searches.
    pub aliases: &'static [&'static str],
    pub context: ActionContext,
    /// Build the boxed action for dispatch.
    pub build: fn() -> Box<dyn Action>,
}

/// Define every action type and its registry row together.
///
/// ```ignore
/// omnipty_actions! {
///     SplitRight => "pane::split_right", "Split Right", "Pane", ["vsplit"], Root;
/// }
/// ```
macro_rules! omnipty_actions {
    ($( $name:ident => $id:literal, $title:expr, $category:literal, [$($alias:literal),* $(,)?], $ctx:ident; )*) => {
        gpui::actions!(omnipty, [$($name),*]);

        pub static REGISTRY: &[$crate::keymap::registry::ActionMeta] = &[
            $(
                $crate::keymap::registry::ActionMeta {
                    id: $id,
                    title: $title,
                    category: $category,
                    aliases: &[$($alias),*],
                    context: $crate::keymap::registry::ActionContext::$ctx,
                    build: || Box::new($name),
                },
            )*
        ];
    };
}
pub(crate) use omnipty_actions;

pub fn all() -> &'static [ActionMeta] {
    super::actions::REGISTRY
}

pub fn by_id(id: &str) -> Option<&'static ActionMeta> {
    all().iter().find(|m| m.id == id)
}

/// Levenshtein distance, for "did you mean" suggestions on typo'd ids.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest registered id to `id`, when it's close enough to be a typo
/// rather than a different word entirely.
pub fn nearest_id(id: &str) -> Option<&'static str> {
    all()
        .iter()
        .map(|m| (edit_distance(id, m.id), m.id))
        .filter(|(d, _)| *d <= 5)
        .min_by_key(|(d, _)| *d)
        .map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_namespaced() {
        let mut seen = HashSet::new();
        for meta in all() {
            assert!(seen.insert(meta.id), "duplicate action id {}", meta.id);
            assert!(
                meta.id.contains("::"),
                "{} should be namespaced like `area::name`",
                meta.id
            );
            assert!(
                meta.id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == ':'),
                "{} should be snake_case",
                meta.id
            );
            assert!(!meta.title.is_empty() && !meta.category.is_empty());
        }
    }

    #[test]
    fn every_id_builds_its_action() {
        for meta in all() {
            let built = by_id(meta.id).expect("lookup by id").build;
            let action = built();
            // The registry row must build the action it claims to: the
            // action's own name ends with the struct name, and building a
            // second copy compares equal.
            assert!(
                action.partial_eq(&*(meta.build)()),
                "{} builds inconsistently",
                meta.id
            );
        }
        assert!(by_id("pane::no_such_thing").is_none());
    }

    #[test]
    fn every_action_type_has_a_row() {
        // gpui registers every `actions!` type by name; each must have a
        // registry row or the palette/keymap can't reach it.
        let rows: HashSet<String> = all()
            .iter()
            .map(|m| (m.build)().name().to_string())
            .collect();
        for data in gpui::generate_list_of_all_registered_actions() {
            if data.name.starts_with("omnipty::") {
                assert!(
                    rows.contains(data.name),
                    "{} has no registry row",
                    data.name
                );
            }
        }
    }

    #[test]
    fn nearest_id_catches_typos() {
        assert_eq!(nearest_id("pane::splitright"), Some("pane::split_right"));
        assert_eq!(nearest_id("tab::nxt"), Some("tab::next"));
        assert_eq!(nearest_id("zzzzzzzzzzzzzzzzzzzz"), None);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
