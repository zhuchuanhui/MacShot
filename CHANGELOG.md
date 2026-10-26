# Changelog

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
