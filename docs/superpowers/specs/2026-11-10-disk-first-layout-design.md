# MultiUSB disk-first layout + serial logs

## Intent

Rebuild the MultiUSB window around a **disk-first runway**: compact format settings on top, the removable-disk list as the main stage, per-disk and batch-total progress while a run is active, a live log pane beside the disks, and **USB serial numbers** shown on each disk row and written into run logs.

Approved visual direction: mockup “Disk-first + serials in the log” (option 4 + progress + live log + S/N).

## Goals

- Settings strip: source (+ Browse), filesystem, partition, volume label (and existing label validation note).
- Disk list is the primary content: checkbox, model/size/letters, serial when known, status, per-disk progress while formatting/copying/verifying.
- **Total progress** bar for the whole copy event (all selected disks’ bytes vs source×targets, same meaning as today’s overall fraction).
- **Live log** pane visible during the run (not only after finish), scrolling newest activity; still write the same content under `Logs\` in the process cwd.
- Every prepare / copy / verify / result log line for a disk includes that disk’s serial when available; otherwise `S/N unavailable`.
- Keep existing behavior: admin gate, wipe confirm, prepare-then-concurrent-copy, path+size verify, run another batch, stock GPUI Kit dark, no console window.

## Non-goals

- Wizard / multi-step navigation.
- Changing wipe, format, copy, or verify algorithms.
- Matching Cursor Dark Anysphere palette (stock dark stays).
- Editable serials or serial-based user filtering beyond display/logging.
- Redesigning the wipe confirmation into a full-page “confirm stage” (keep current alert/dialog unless GPUI Kit forces a small adaptation).

## Anti-goals

- Dropping per-disk progress when adding the total bar.
- Inventing serial numbers when Windows returns none.
- Hiding the live log until the batch ends.

## Constraints

- Stay on `main`; GPUI Kit components and `cx.theme()` tokens only.
- `EnumeratedDisk` / WMI already expose `id.serial`; reuse it — do not invent a second inventory path.
- Windows subsystem remains `windows` (no console).

## Approaches

1. **Stacked form (today)** — simple; disks compete with settings for vertical space; log only after done.
2. **Split desk / wizard / batch console** — explored in mockups; heavier or more clicks than needed for this operator tool.
3. **Disk-first runway + total progress + live log + serials** — **chosen.** Compact strip, checklist stage, dual progress, side log.

## How it gets built (decided)

- Restructure `App::render` into: settings strip → total progress (when a batch is active) → two-column body (disk list | live log) → footer actions (Start / Run another batch / disabled Start while locked).
- Stream status lines into `ui.log` (or equivalent) as prepare/copy/verify events arrive, including serial text via a shared formatter.
- Disk row detail/label includes serial (`S/N …` or `S/N unavailable`).
- Result lines and on-disk log files use the same disk naming helper so UI and file stay consistent.
- Preserve dark theme apply + elevation / refresh / browse / start flows.

## Left to the builder

- Exact flex/scroll sizing so the log pane and disk list share height without breaking the 560×640 minimum window.
- Whether total progress shows during prepare-only (format phase) as disk-count progress, or only once byte copy starts — prefer: disk-prep counts while preparing, then byte total during copy/verify (match operator expectation from the mockup’s “Formatting finished · copying…”).
- GPUI Kit scroll/log container choice for the live log pane.
