# MultiUSB dark theme + console-free 0.1.5

## Intent

Ship MultiUSB looking like a dark desktop app (stock GPUI Kit dark), hide the extra Windows console window, and replace the existing **0.1.5** GitHub release with that build.

## Goals

- Force dark mode for the whole GPUI Kit UI (form, buttons, dialogs, progress, alerts).
- Stop the console window from appearing when the GUI starts.
- Rebuild and re-publish tag/release **v0.1.5** with the new exe.

## Non-goals

- Matching Cursor Dark Anysphere’s custom charcoal / blue / pink palette.
- Adding a light/dark toggle or following system appearance.
- Redesigning layout, copy, or control structure.

## Anti-goals

- Leaving some surfaces light while others are dark.
- Painting individual widgets with raw hex instead of the theme mode switch.
- Creating a new version number when the requested release is still 0.1.5.

## Constraints

- Stay on `main`; no feature branch or PR.
- Keep using GPUI Kit semantic tokens via `cx.theme()`; only change theme mode at startup.
- Overwrite the existing `v0.1.5` release asset (`MultiUSB-0.1.5.exe`).

## Approaches

1. **Remap GPUI tokens to Cursor Dark Anysphere** — closest to Cursor’s product UI; more theme plumbing.
2. **Hand-paint hex per control** — brittle; drifts as new widgets appear.
3. **Stock GPUI Kit dark** — call `Theme::change(ThemeMode::Dark, …)` (or equivalent) once at app start. **Chosen.**

Also add `#![windows_subsystem = "windows"]` in `src/main.rs`.

## How it gets built (decided)

- In `run()` after `gpui_kit::init`, switch the global theme to dark mode so every component inherits it.
- Add the Windows subsystem attribute so double-clicking does not open a terminal.
- Commit on `main`, retarget/replace `v0.1.5`, rebuild release exe, upload `MultiUSB-0.1.5.exe`.

## Left to the builder

- Exact GPUI Kit API call (`Theme::change` vs `Theme::update`) verified against 0.9.1 source.
- Whether `Cargo.lock` needs a touch when rebuilding (only if the build changes it).
