//! Command palette matching: a small fuzzy scorer over the action registry.
//!
//! Pure functions, no GPUI, so ranking is unit-testable. The app owns the
//! palette's state and rendering.

use crate::keymap::registry::ActionMeta;

/// One row in the palette's filtered list.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteItem {
    pub action_id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    /// Char positions in `title` that matched, for highlight rendering.
    pub highlights: Vec<usize>,
    /// Keystrokes to show, already pretty-printed.
    pub binding: Option<String>,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

const MATCH: i32 = 10;
const CONSECUTIVE: i32 = 8;
const WORD_START: i32 = 6;
const GAP: i32 = 1;

fn is_word_start(chars: &[char], ix: usize) -> bool {
    ix == 0 || !chars[ix - 1].is_alphanumeric()
}

/// Case-insensitive subsequence match with the alignment that scores best:
/// consecutive runs beat scattered hits, word-initial hits beat mid-word
/// ones, and every skipped character between hits costs a little.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<Match> {
    Matcher::new(query).score(candidate)
}

/// A query prepared once for matching against many candidates. The file
/// finder scores every indexed path on each keystroke — up to 100k of them —
/// so the query is lowercased once, candidates that can't match are turned
/// away in one pass, and the tables are scratch space kept between calls.
pub struct Matcher {
    q: Vec<char>,
    c: Vec<char>,
    cl: Vec<char>,
    /// `best[i * m + j]`: best score with `q[i]` matched at `c[j]`.
    best: Vec<i32>,
    /// `prev[i * m + j]`: the `j` chosen for `q[i - 1]` in that alignment.
    prev: Vec<usize>,
}

impl Matcher {
    pub fn new(query: &str) -> Self {
        Self {
            q: query
                .chars()
                .flat_map(|c| c.to_lowercase())
                .filter(|c| !c.is_whitespace())
                .collect(),
            c: Vec::new(),
            cl: Vec::new(),
            best: Vec::new(),
            prev: Vec::new(),
        }
    }

    pub fn score(&mut self, candidate: &str) -> Option<Match> {
        let Self {
            q,
            c,
            cl,
            best,
            prev,
        } = self;
        if q.is_empty() {
            return Some(Match {
                score: 0,
                positions: Vec::new(),
            });
        }
        // One pass: the chars, their lowercase, and whether the query is a
        // subsequence at all. Most candidates stop here.
        c.clear();
        cl.clear();
        let mut matched = 0;
        for ch in candidate.chars() {
            let lower = if ch.is_ascii() {
                ch.to_ascii_lowercase()
            } else {
                ch.to_lowercase().next().unwrap_or(ch)
            };
            if matched < q.len() && lower == q[matched] {
                matched += 1;
            }
            c.push(ch);
            cl.push(lower);
        }
        if matched < q.len() {
            return None;
        }

        let (n, m) = (q.len(), cl.len());
        best.clear();
        best.resize(n * m, i32::MIN);
        prev.clear();
        prev.resize(n * m, usize::MAX);
        let hit = |j: usize| MATCH + if is_word_start(c, j) { WORD_START } else { 0 };
        for j in 0..m {
            if cl[j] == q[0] {
                best[j] = hit(j);
            }
        }
        for i in 1..n {
            let (above, row) = best[(i - 1) * m..(i + 1) * m].split_at_mut(m);
            // Coming from column k costs (j - k - 1) * GAP, so the best k
            // with a gap is the one maximising above[k] + k * GAP — a
            // running maximum over k <= j - 2, earliest k winning ties.
            let (mut run_top, mut run_from) = (i32::MIN, usize::MAX);
            for j in i..m {
                if j > i && above[j - 2] != i32::MIN {
                    let v = above[j - 2] + (j - 2) as i32 * GAP;
                    if v > run_top {
                        (run_top, run_from) = (v, j - 2);
                    }
                }
                if cl[j] != q[i] {
                    continue;
                }
                let here = hit(j);
                let (mut top, mut from) = (i32::MIN, usize::MAX);
                if run_top != i32::MIN {
                    (top, from) = (run_top + here - (j as i32 - 1) * GAP, run_from);
                }
                // The adjacent column is considered last, as a run.
                if above[j - 1] != i32::MIN {
                    let s = above[j - 1] + here + CONSECUTIVE;
                    if s > top {
                        (top, from) = (s, j - 1);
                    }
                }
                if top != i32::MIN {
                    row[j] = top;
                    prev[i * m + j] = from;
                }
            }
        }
        let last = &best[(n - 1) * m..];
        let (mut j, score) = (0..m)
            .filter(|&j| last[j] != i32::MIN)
            .map(|j| (j, last[j]))
            .max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
        let mut positions = vec![0; n];
        for i in (0..n).rev() {
            positions[i] = j;
            if i > 0 {
                j = prev[i * m + j];
            }
        }
        // Shorter candidates win ties: a match covering more of the title is
        // a better answer to the same query.
        Some(Match {
            score: score - (m as i32) / 8,
            positions,
        })
    }
}

/// Score an action against a query across its title, category, and aliases.
/// Only a title match yields highlight positions.
pub fn score_action(query: &str, meta: &ActionMeta) -> Option<Match> {
    let mut best = fuzzy_match(query, meta.title);
    let mut consider = |text: &str, penalty: i32| {
        if let Some(m) = fuzzy_match(query, text) {
            let alt = Match {
                score: m.score - penalty,
                positions: Vec::new(),
            };
            if best.as_ref().is_none_or(|b| alt.score > b.score) {
                best = Some(alt);
            }
        }
    };
    consider(&format!("{} {}", meta.category, meta.title), 3);
    for alias in meta.aliases {
        consider(alias, 2);
    }
    best
}

/// Build the palette's rows: with a query, every candidate that matches,
/// best first; without one, most recently used first, then registry order.
pub fn build_items<'a>(
    query: &str,
    candidates: impl Iterator<Item = &'a ActionMeta>,
    recent: &[&str],
    binding_for: impl Fn(&str) -> Option<String>,
) -> Vec<PaletteItem> {
    let query = query.trim();
    let mut items: Vec<PaletteItem> = candidates
        .filter_map(|meta| {
            let m = score_action(query, meta)?;
            Some(PaletteItem {
                action_id: meta.id,
                title: meta.title,
                category: meta.category,
                highlights: m.positions,
                binding: binding_for(meta.id),
                score: m.score,
            })
        })
        .collect();
    if query.is_empty() {
        let rank = |id: &str| recent.iter().position(|r| *r == id).unwrap_or(usize::MAX);
        items.sort_by_key(|it| rank(it.action_id));
    } else {
        items.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(a.title.len().cmp(&b.title.len()))
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::registry;

    fn top(query: &str) -> Vec<&'static str> {
        build_items(query, registry::all().iter(), &[], |_| None)
            .into_iter()
            .map(|i| i.action_id)
            .collect()
    }

    /// The file finder reuses one matcher across 100k candidates of every
    /// length; scratch left over from one must never colour the next.
    #[test]
    fn a_reused_matcher_agrees_with_a_fresh_one() {
        let candidates = [
            "src/terminal/mod.rs",
            "a",
            "docs/docs/file-tree/index.html",
            "",
            "src/app.rs",
            "Überstraße/ÄPFEL.md",
            "target/debug/deps/omnipty-032b53269b7ca225",
            "s/r/c/a/p/p",
            "src/app.rs",
        ];
        for query in [
            "", "s", "sap", "src/app", "apfel", "SRC", "x y z", "omnipty032",
        ] {
            let mut reused = Matcher::new(query);
            for c in candidates {
                assert_eq!(reused.score(c), fuzzy_match(query, c), "{query:?} on {c:?}");
            }
        }
        // Gaps cost their length, runs and word starts pay: pin one alignment.
        let m = fuzzy_match("sap", "src/app.rs").unwrap();
        assert_eq!(m.positions, [0, 4, 5], "s, then the run at the word start");
    }

    #[test]
    fn subsequence_required() {
        assert!(fuzzy_match("spr", "Split Right").is_some());
        assert!(fuzzy_match("xyz", "Split Right").is_none());
        assert!(
            fuzzy_match("thgir", "Split Right").is_none(),
            "order matters"
        );
        assert_eq!(
            fuzzy_match("", "anything").unwrap().positions,
            Vec::<usize>::new()
        );
    }

    #[test]
    fn highlights_point_at_matched_chars() {
        let m = fuzzy_match("spr", "Split Right").unwrap();
        assert_eq!(m.positions, vec![0, 1, 6]);
        let m = fuzzy_match("SR", "split right").unwrap();
        assert_eq!(m.positions, vec![0, 6], "case-insensitive");
    }

    #[test]
    fn consecutive_beats_scattered_and_word_starts_beat_mid_word() {
        let run = fuzzy_match("spl", "Split Right").unwrap().score;
        let scattered = fuzzy_match("spl", "Select Pane Left-ish").unwrap().score;
        assert!(run > scattered, "{run} vs {scattered}");
        let boundary = fuzzy_match("fr", "Font Reset").unwrap().score;
        let mid = fuzzy_match("fr", "Buffer Ram").unwrap().score;
        assert!(boundary > mid, "{boundary} vs {mid}");
    }

    #[test]
    fn shorter_titles_win_ties() {
        let short = fuzzy_match("tab", "New Tab").unwrap().score;
        let long = fuzzy_match("tab", "New Tab In A Very Long Title")
            .unwrap()
            .score;
        assert!(short > long);
    }

    #[test]
    fn spr_ranks_split_right_first() {
        let ranked = top("spr");
        assert_eq!(ranked[0], "pane::split_right", "{ranked:?}");
        // "Workspace › Workspaces: Pin / Unpin" is a scattered match at best.
        let pin = ranked
            .iter()
            .position(|id| *id == "workspace::toggle_persist");
        assert!(pin.is_none_or(|ix| ix > 3), "{ranked:?}");
        assert_eq!(top("pal")[0], "app::palette");
        assert_eq!(top("theme")[0], "app::select_theme");
        assert_eq!(top("eq")[0], "pane::equalize");
        // Word order the panel titles ("Workspaces: Rename Selected") can't match.
        assert_eq!(top("rename workspace")[0], "workspace::rename");
        assert_eq!(top("pin workspace")[0], "workspace::toggle_persist");
    }

    #[test]
    fn aliases_and_categories_match_without_highlights() {
        let items = build_items("vsplit", registry::all().iter(), &[], |_| None);
        let ids: Vec<_> = items.iter().map(|i| i.action_id).collect();
        // Both vertical splits carry the alias; nothing else should outrank them.
        assert!(
            ids[..2].contains(&"pane::split_right") && ids[..2].contains(&"pane::split_left"),
            "{ids:?}"
        );
        assert!(items[0].highlights.is_empty());
        let items = build_items("terminal copy", registry::all().iter(), &[], |_| None);
        assert_eq!(items[0].action_id, "terminal::copy");
    }

    #[test]
    fn empty_query_lists_recent_first_then_registry_order() {
        let items = build_items(
            "",
            registry::all().iter(),
            &["tab::new", "app::quit"],
            |_| None,
        );
        assert_eq!(items[0].action_id, "tab::new");
        assert_eq!(items[1].action_id, "app::quit");
        assert_eq!(items.len(), registry::all().len());
        let rest: Vec<_> = items[2..].iter().map(|i| i.action_id).collect();
        let expected: Vec<_> = registry::all()
            .iter()
            .map(|m| m.id)
            .filter(|id| *id != "tab::new" && *id != "app::quit")
            .collect();
        assert_eq!(rest, expected);
    }

    #[test]
    fn bindings_are_attached() {
        let items = build_items("quit", registry::all().iter(), &[], |id| {
            (id == "app::quit").then(|| "⌘Q".to_string())
        });
        assert_eq!(items[0].binding.as_deref(), Some("⌘Q"));
    }
}
