# Stock Dark Theme + Re-release 0.1.5 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Force MultiUSB onto stock GPUI Kit dark mode, hide the Windows console window, and replace the published `v0.1.5` release binary.

**Architecture:** After `gpui_kit::init`, call `Theme::change(ThemeMode::Dark, None, cx)` so every component inherits the default dark token set. Mark the binary as a Windows GUI subsystem app. Rebuild `--release` and overwrite the existing GitHub release asset.

**Tech Stack:** Rust, gpui-kit 0.9.1, Windows, `gh` releases

**Spec:** `docs/superpowers/specs/2026-11-10-cursor-dark-theme-design.md`

## Global Constraints

- Stay on `main`; no feature branch or PR.
- Use stock GPUI Kit dark only — do not remap colors to Cursor Dark Anysphere.
- Keep version `0.1.5`; replace tag/release `v0.1.5` and asset `MultiUSB-0.1.5.exe`.
- Prefer `cx.theme()` tokens; no per-widget raw hex theming.

## Review Focus

- Cold launch still paints the form, selects, and Start button in dark (not a flash of light).
- Wipe confirmation `Alert` / dialog uses dark surfaces and readable danger emphasis.
- Double-clicking `MultiUSB-0.1.5.exe` opens only the GUI window (no console).
- Existing disk wipe / copy / verify behavior is unchanged.
- Elevated relaunch still lands in dark mode.

---

### Task 1: Dark theme + GUI subsystem

**Files:**
- Modify: `src/main.rs`
- Modify: `src/app.rs` (`run`, imports)
- Test: `src/app.rs` (or adjacent module test already in crate)

**Interfaces:**
- Consumes: `gpui_kit::component::{Theme, ThemeMode}`; `gpui_kit::init(cx)`
- Produces: `run()` forces dark mode before `open_window`; binary uses Windows subsystem `windows`

- [ ] **Step 1: Write the failing theme assertion**

Add a `#[test]` (or `#[gpui_kit::test]` if that is how this crate already tests GPUI) that after `gpui_kit::init` + `Theme::change(ThemeMode::Dark, None, cx)`, `Theme::global(cx).mode` (or the 0.9.1-equivalent reader) equals `ThemeMode::Dark`.

- [ ] **Step 2: Run the test and confirm it fails or is not yet wired from `run()`**

Run: `cargo test --lib theme_starts_dark -- --nocapture`  
Expected: FAIL, or PASS only after Step 3 if the assertion is written against the helper you are about to call from `run()`.

- [ ] **Step 3: Implement dark mode in `run()` and the subsystem attribute**

In `src/app.rs`, immediately after `gpui_kit::init(cx)`, call `Theme::change(ThemeMode::Dark, None, cx)`.

In `src/main.rs`, add as the first line of the crate root:

```rust
#![windows_subsystem = "windows"]
```

- [ ] **Step 4: Run tests**

Run: `cargo test`  
Expected: PASS (all existing + new theme test)

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/app.rs
git commit -m "fix: use stock GPUI Kit dark and hide the console"
```

---

### Task 2: Replace GitHub release 0.1.5

**Files:**
- Build: `target/release/MultiUSB.exe` → `target/release/MultiUSB-0.1.5.exe`
- Remote: GitHub release `v0.1.5`

**Interfaces:**
- Consumes: Task 1 changes on `main`
- Produces: Updated `https://github.com/jfmiller52/MultiUSB/releases/tag/v0.1.5` with new `MultiUSB-0.1.5.exe`

- [ ] **Step 1: Confirm clean releasable `main`**

Run: `git status -sb`; `git log -1 --oneline`  
Expected: Task 1 commit on `main`, no unrelated dirty files staged for release.

- [ ] **Step 2: Push `main` if ahead of origin**

Run: `git push origin HEAD`

- [ ] **Step 3: Move tag `v0.1.5` to current `HEAD` and force-push the tag**

```bash
git tag -f -a v0.1.5 -m "0.1.5"
git push origin v0.1.5 --force
```

- [ ] **Step 4: Build the release binary with the VS toolchain**

```bash
cmd /C 'call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=amd64 && cd /d C:\MultiUSB && set "CARGO_TARGET_DIR=C:\MultiUSB\target" && cargo build --release'
```

Expected: `Finished release` for `multiusb v0.1.5`

- [ ] **Step 5: Upload the exe (create or clobber)**

```bash
Copy-Item target\release\MultiUSB.exe target\release\MultiUSB-0.1.5.exe -Force
gh release upload v0.1.5 target\release\MultiUSB-0.1.5.exe --clobber
```

If the release notes/title need refreshing after the retag, use `gh release edit v0.1.5` only as needed; keep title `0.1.5`.

- [ ] **Step 6: Verify release asset**

Run: `gh release view v0.1.5 --json url,assets,tagName`  
Expected: asset `MultiUSB-0.1.5.exe` present; URL `https://github.com/jfmiller52/MultiUSB/releases/tag/v0.1.5`

- [ ] **Step 7: Commit plan/spec docs if untracked or updated**

```bash
git add docs/superpowers/specs/2026-11-10-cursor-dark-theme-design.md docs/superpowers/plans/2026-11-10-stock-dark-theme-rerelease.md .gitignore
git commit -m "docs: record dark theme plan and design"
git push origin HEAD
```

Only include `.gitignore` if `.superpowers/` was added there during brainstorming.
