# Changelog

## 0.9.0 — 2026-10-28

- Added a Settings control to reload the saved app language and refresh localized UI and menu-bar labels.
- Refined the menu-bar camera icon with smoother curves and a hollow lens ring.

## 0.8.1 — 2026-10-28

- Joined the macOS window titlebar to the app header and removed the separator between them.
- Made the main window background opaque while keeping the range-selection overlay transparent.

## 0.8.0 — 2026-10-28

- Refined the shared visual style with light surfaces, restrained lavender controls, clearer selection and hover states, and softer card framing.
- Simplified the backdrop and adjusted spacing and corner radii across capture and settings screens.

## 0.7.0 — 2026-10-28

- Added a persistent Japanese, English, Simplified Chinese, and Traditional Chinese language selector across the app.
- Expanded localization across capture, editing, evidence, comparison, settings, permissions, and status/error messages.
- Bundled Noto Sans SC and its SIL Open Font License notice for Chinese glyph coverage.
- Added English and Simplified Chinese README editions with reciprocal language links.

## 0.6.1 — 2026-10-27

- Calibrate the GIF size estimate against saved MacShot recordings so its range better reflects real screen-capture files.

## 0.6.0 — 2026-10-27

- Rework settings layout, with reorderable/resizable cards and saved display-density and text-size controls.
- Add quick font-size buttons and presets for more precise adjustments.
- Add 480p, 720p, 1080p, 2K/1440p, and 4K/2160p recording output choices for GIF and MP4.
- Show per-minute recording-size estimates for MP4 and an activity-based estimate range for GIF.
- Improve capture and recording controls and make file information easier to scan.

## 0.5.2 — 2026-10-27

- Include the required MinGW runtime DLLs and license notices in the Windows x64 ZIP so the app can start on clean Windows installations.
- Show saved PNG pixel dimensions in the capture completion status and notice.

## 0.5.1 — 2026-10-27

- Update the recording playback status when users pause or resume in-app playback.

## 0.5.0 — 2026-10-27

- Add in-app playback and pause controls for GIF and MP4 recordings, with streaming frame decode and GIF loop support. MP4 audio remains available through the system media player.
- Add a five-second microphone input test that distinguishes speech from ambient noise and shows the measured input level.
- Prefer CoreAudio microphone input on macOS, retain a ScreenCaptureKit fallback on macOS 15 and later, and report silent or interrupted microphone input in the saved-recording notice.
- Preserve full Retina resolution when capturing an individual display and include the visible selection outline in range-capture bounds.
- Add the macOS audio-input signing entitlement and validate it with the bundle verifier.
- Keep the in-app recording list easy to reach and improve recording preview sizing and playback state.

## 0.4.7 — 2026-10-27

- Move the GIF / MP4 recordings list above the long image list and label the Evidence page as images and recordings so saved videos are easier to find.
- Prevent screenshots, edits, GIFs, and MP4s from overwriting an existing file when generated names collide; add a `-copy-N` suffix and keep edited images grouped with their source.
- Keep GIF and MP4 first-frame previews readable at the bottom of the recordings list, automatically scroll them into view when selected, and preserve their aspect ratio up to 720×405.
- Add a live permission panel for macOS screen/system-audio and microphone access, with direct settings links and refresh; microphone access can be requested from the panel.
- Add a Finder reveal action so users can locate MacShot.app while adding it to macOS Screen & System Audio Recording settings.
- Add Windows microphone-device status and a shortcut to Windows microphone privacy settings.
- Restore the macOS main window and reject late results when screen capture exceeds the 15-second timeout; count timed-out continuous captures as failures and clear already-stopped sessions from the menu-bar status.
- Add a macOS bundle check to detect missing microphone or screen-capture usage descriptions and invalid signatures before distribution.
- Explain how to enable microphone access when MP4 recording fails because macOS denied the microphone.
- Exclude the macOS "Software Cursor" overlay from captureable windows so active-window capture does not save a blank cursor-only image.
- Keep MP4 video and system-audio recording available when microphone access is not authorized; show a clear system-audio-only status and never request microphone access during recording.
- Keep the microphone warning in the recording-complete notification so users know when an MP4 contains system audio without microphone audio.
- Show the result of the macOS microphone-permission request in the app, and clarify that MP4 continues with system audio only when microphone access is unavailable.
- Distinguish undecided and denied microphone states, label the undecided result accurately, and match the available action to the detected state.

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
