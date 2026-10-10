# Contributing to OmniPTY

Thanks for taking the time to contribute. OmniPTY is a native terminal emulator for macOS
and Linux, built in Rust on [GPUI](https://www.gpui.rs) and
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal). This document covers
how to get a build running, what a good pull request looks like, and what needs updating
alongside a code change.

## Before you start

- **Bugs and feature requests** go in [issues](https://github.com/omnipty-terminal/omnipty/issues).
  If you're planning a non-trivial change (new feature, config option, or anything that
  touches the architecture), open an issue first to talk it through; it saves everyone
  a rewritten PR.
- **Questions and ideas** are welcome on the [Discord](https://discord.gg/APV9FYGgeh).
- Check open issues and PRs before starting, so two people don't end up building the
  same thing.
- There is currently only one maintainer [@bobbycoleman-dev](https://github.com/bobbycoleman-dev),
  so please be patient as reviewing each PR may take some time.

## Getting set up

You'll need [Rust](https://rustup.rs) (2024 edition).

**macOS**: full Xcode with the Metal toolchain, since GPUI compiles Metal shaders at
build time:

```sh
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
xcodebuild -downloadComponent MetalToolchain   # if the build asks for it
```

**Linux**: GPUI's build dependencies (Arch shown; see `.github/workflows/ci.yml` for the
Debian/Ubuntu package list):

```sh
sudo pacman -S --needed base-devel fontconfig freetype2 libxkbcommon libxkbcommon-x11 \
  libxcb wayland vulkan-icd-loader libnotify
```

Then:

```sh
git clone https://github.com/omnipty-terminal/omnipty.git
cd omnipty
cargo run          # development build
cargo test         # run the test suite
```

The first build compiles GPUI, which takes a few minutes. A `cargo run` binary is a debug
build, fine for iterating on a change, but full-screen programs like nvim will feel
sluggish in it, since an unoptimized GPUI redraw doesn't fit in a 60 Hz frame. It also
never checks for updates and isn't a real app bundle on macOS. See the
[README](README.md#build-from-source) for the release-build scripts if you need one.

## Making a change

1. **Fork the repo and branch from `main`.** Use a short, descriptive branch name
   (`fix-tree-drag-drop`, `add-fish-prompt-support`).
2. **Keep PRs focused.** One logical change per PR is easier to review and easier to
   revert if something's wrong. If you find an unrelated bug while you're in there, file
   an issue or open a separate PR for it.
3. **Follow the existing code style.** Run `cargo fmt` before committing and check
   `cargo clippy --all-targets` for anything it flags. There's no separate lint step in
   CI beyond the build and test jobs, but clean `clippy` output is expected.
4. **Add tests for non-trivial logic.** Not every change needs a new test, but new
   behavior (parsing, state machines, anything with edge cases) should leave one
   behind. See `tests/` and the `#[cfg(test)]` modules throughout `src/` for the existing
   patterns.
5. **Build and test both platforms if you can.** CI runs `cargo build --all-targets` and
   `cargo test` on both macOS and Linux (see `.github/workflows/ci.yml`); if you only have
   one platform available, say so in the PR and a maintainer can check the other.

### Checklist for a user-visible change

A change that affects behavior, configuration, or what the app looks like isn't done
until everything below that applies is updated. It's fine to skip an item that genuinely
doesn't apply; just say which ones you skipped in the PR description.

- **`CHANGELOG.md`**: add an entry under `## [Unreleased]`, in the right section:
  - `### Added` for new features
  - `### Changed` for changes to existing behavior
  - `### Fixed` for bug fixes

  The changelog follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
  **Help → What's New** shows this file inside the app, so write the entry for users:
  what changed and why it matters, not which files moved or how it was implemented.
  Don't add a version heading yourself; that's cut at release time. Also, feel free 
  to add `Contributed by <Github Username>` at the end of the entry description so you
  are credited in the release notes for your contribution.

- **`README.md`**: update the feature list, install instructions, or any keybinding/config
  table the change affects.

- **Default config**: a new or changed config option needs to be added in both
  `src/config/schema.rs` (the field and its default) and the commented template in
  `src/config/mod.rs`, so it shows up documented in a fresh config file.

- **Tests**: `cargo test` should pass, and non-trivial logic should leave a test behind.

Maintainers handle the project website (a separate repo) and cutting releases, so you
don't need to touch either of those.

## Commit messages

Write commit messages that explain *why*, not just *what*. The diff already shows what
changed. A short summary line, and a body if the reasoning isn't obvious from the code.

## Submitting the pull request

- Describe what the change does and why, and link the issue it addresses if there is one.
- Mention which platform(s) you built and tested on.
- Note which items in the checklist above you updated, and which (if any) you skipped
  and why.
- Include screenshots or a short clip for anything visual.
- Make sure CI is green before asking for review. If a check fails for a reason unrelated
  to your change, say so in the PR.

A maintainer will review, may ask for changes, and will merge once it's ready. Don't take
requested changes personally; they're about the code, not you.

## Reporting bugs

Use **Help → Report an Issue** in the menu bar (the ☰ menu on Linux), *Report an Issue* in
the command palette, or [open one directly](https://github.com/omnipty-terminal/omnipty/issues/new).
Include:

- What you expected to happen and what happened instead.
- Steps to reproduce, if you have them.
- Your platform and OmniPTY version (**OmniPTY → About**, or `omnipty --version`).
- Your shell, and anything unusual about your setup (a custom prompt, an unusual
  terminal multiplexer setup, a Wayland compositor without server-side decorations, etc.).

## License

By contributing, you agree that your contributions will be licensed under the
[MIT License](LICENSE), the same license that covers the rest of the project.
