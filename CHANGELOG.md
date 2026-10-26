# Changelog

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
