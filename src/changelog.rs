//! The bundled CHANGELOG.md, rendered to ANSI for paging in a terminal tab.
//! The maintainer preamble and the empty Unreleased section are skipped;
//! an `## Upcoming` section, if there is one, leads.

use std::path::PathBuf;

use crate::markdown;

const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// Write the rendered changelog to the cache; returns its path and the code
/// blocks its "copy" links refer to.
pub fn write_rendered(width: usize) -> Option<(PathBuf, Vec<String>)> {
    let rendered = render_ansi(CHANGELOG, width);
    Some((
        markdown::write_cache("changelog.txt", &rendered.text)?,
        rendered.code,
    ))
}

pub fn render_ansi(md: &str, width: usize) -> markdown::Rendered {
    let mut body = String::from("# OmniPTY — what's new\n\n");
    // Skip everything before the first released version heading, except
    // an Upcoming notice (which sits above Unreleased).
    let released = |l: &str| l.starts_with("## [") && !l.starts_with("## [Unreleased]");
    let mut skipping = true;
    for line in md.lines() {
        if skipping && (line == "## Upcoming" || released(line)) {
            skipping = false;
        }
        if line.starts_with("## [Unreleased]") {
            skipping = true;
        }
        if skipping {
            continue;
        }
        if line.starts_with("## ") {
            body.push_str(&line.replace(['[', ']'], ""));
        } else {
            body.push_str(line);
        }
        body.push('\n');
    }
    markdown::render(&body, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upcoming_section_leads_and_unreleased_is_still_skipped() {
        let md = "# Changelog\n\npreamble\n\n## Upcoming\n\n- the rename\n\n## [Unreleased]\n\n- pending\n\n## [0.8.2] - 2026-10-10\n\n- shipped\n";
        let out = render_ansi(md, 80).text;
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s:?} missing in:\n{out}"));
        assert!(at("Upcoming") < at("the rename") && at("the rename") < at("0.8.2 - 2026-10-10"));
        assert!(at("0.8.2") < at("shipped"));
        assert!(!out.contains("preamble") && !out.contains("pending"), "{out}");
    }

    #[test]
    fn renders_headings_bullets_and_inline() {
        let md = "# Changelog\n\npreamble\n\n## [Unreleased]\n\n- pending\n\n## [0.5.1] - 2026-09-13\n\n### Fixed\n- `ls` and **bold** and [docs](https://x)\n";
        let out = render_ansi(md, 80).text;
        assert!(
            !out.contains("preamble") && !out.contains("pending"),
            "skips preamble and Unreleased"
        );
        assert!(
            out.contains("\x1b[1m\x1b[36m0.5.1 - 2026-09-13\x1b[0m"),
            "version heading, brackets dropped"
        );
        assert!(out.contains("\x1b[1m\x1b[33mFixed\x1b[0m"));
        assert!(
            out.contains("  • \x1b[36mls\x1b[0m and \x1b[1mbold\x1b[0m and \x1b[4mdocs\x1b[0m"),
            "{out}"
        );
    }

    #[test]
    fn bundled_changelog_renders() {
        assert!(render_ansi(CHANGELOG, 80).text.contains("0.1.0"));
    }
}
