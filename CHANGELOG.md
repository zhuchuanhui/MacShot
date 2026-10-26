# Changelog

## 0.4.6 — 2026-10-26

- Add a persistent GIF / MP4 (H.264) recording-format choice for macOS and Windows.
- Add MP4 quality presets: Standard (10 fps / 6 Mbps), High (15 fps / 12 Mbps), and Best (30 fps / 20 Mbps).
- Record system audio and microphone together as AAC audio in MP4 on macOS and Windows.
- Add a recordings list to Evidence, with in-app first-frame, duration, and file-size previews for GIF and MP4. Full playback opens the system's associated player.
- Correct H.264 input to use RGBA pixels and remove Annex B start codes before MP4 sample muxing.

## 0.4.5 — 2026-10-26

- Minimize the Windows main window during capture instead of hiding it; restore it after capture, recording, cancellation, or timeout.
- Apply the same minimize/restore behavior to continuous capture and range-selection preparation.

## 0.4.4 — 2026-10-26

- Restore the Windows window after capture with both a repaint timer and an independent watchdog.
- Make the Windows close button and tray-menu Exit command terminate the process; finish and save an active GIF recording before exit.
- Register screenshot and recording shortcuts independently, so a conflict in one does not disable the other or the on-screen controls.

## 0.4.3 — 2026-10-26

- Keep the Windows app responsive while monitor discovery and screen capture run; restore the main window with guidance after a 15-second timeout.
- Ignore late capture results after a timeout so they cannot overwrite the current app state.

## 0.4.2 — 2026-10-26

- Use opaque Windows windows for the main app and range selector to avoid transparent-window rendering and input issues. macOS transparency remains enabled.

## 0.4.1 — 2026-10-26

- Added a tray-menu command to reopen the main window on macOS and Windows.
- Windows range selection now paints a fresh desktop snapshot behind its overlay instead of relying on transparent-window composition.
- Added a signed macOS Apple Silicon ZIP and a Windows x64 ZIP, each with a SHA-256 checksum file.

## 0.4.0 — 2026-10-26

- Added configurable screenshot and recording global shortcuts.
- Added continuous capture with ordered saving.
- Added evidence history and sequential editing for PNG/JPEG images.
- Added crop, rectangle and arrow annotations, plus side-by-side, overlay and pixel-difference comparison.
- Added GIF screen recording and menu-bar / Windows notification-area actions.
- Added configurable capture target, filename prefix, timestamp mark, completion notice, save location, annotation color and width.

### Windows test package

`MacShot-v0.4.0-windows-x64.zip` contains the Windows x64 executable and README. The Windows runtime still needs a real-device launch and capture check.
