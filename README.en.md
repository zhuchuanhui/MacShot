# MacShot 1.0.0

[日本語](README.md) · [简体中文](README.zh-CN.md)

MacShot captures, edits, and compares screenshots, and records the screen as GIF or MP4. Captured PNG/JPEG images and recordings are available in the evidence list.

## Download

- macOS Apple Silicon (arm64): [MacShot-v1.0.0-macos-arm64.zip](https://github.com/zhuchuanhui/MacShot/releases/download/v1.0.0/MacShot-v1.0.0-macos-arm64.zip)
- Windows x64: [MacShot-v1.0.0-windows-x64.zip](https://github.com/zhuchuanhui/MacShot/releases/download/v1.0.0/MacShot-v1.0.0-windows-x64.zip)
- [GitHub release page](https://github.com/zhuchuanhui/MacShot/releases/tag/v1.0.0)

On macOS, unzip the archive and move `MacShot.app` to Applications for convenient access. The app is ad hoc signed and not notarized. If macOS blocks the first launch, Control-click the app in Finder and choose **Open**. macOS 14 or later on Apple Silicon is supported.

On Windows, unzip the package and run `MacShot.exe`. Required Windows runtime DLLs are included. MP4 recording captures the default playback device and default microphone; allow desktop apps to use the microphone in Windows privacy settings if needed.

The window's close button hides MacShot to the menu bar or notification area without quitting. Choose **Open MacShot** in the menu to restore the window, or **Quit** to exit. Recording and continuous capture continue while the window is hidden.

## Use

### Screenshots

1. In **Capture & Edit**, choose the active window, main display, all displays, a display, or a listed window.
2. Select **Capture & Preview** or press the screenshot shortcut (default: `Option`+`Shift`+`S`). For a region, choose **Select Region** and drag across the screen.
3. Images are saved in **Evidence** with a timestamp and sequence number in the filename. You can also add the timestamp to the image in Settings.

### Continuous capture

1. Choose **Start Continuous Capture**. MacShot hides its window.
2. Press the screenshot shortcut for each capture; images are saved in order.
3. Stop from the menu bar or notification-area menu. The window returns when queued captures finish.

### Screen recording

Choose GIF or MP4 (H.264), resolution, and quality in Settings. Press **Record** or the recording shortcut (default: `Option`+`Shift`+`R`). Stop from the menu bar or with the same shortcut. The recordings list shows each duration. Click a row or choose **Play** to open GIF/MP4 playback in a separate window. **Edit** changes the recording name, and **Open Externally** opens the file in its default player (including MP4 audio). MP4 records system audio and, when permitted, microphone audio. GIF has no audio.

### Edit and compare

In Evidence, use the edit queue to add rectangles, arrows, or a crop. Edits save as new PNG files, leaving the source image intact. **Compare** supports side-by-side, overlay, and pixel-difference views for two PNG or JPEG images.

## macOS permissions

The Settings page shows screen-recording and microphone status. Use **Refresh Permission Status** after changing a permission. For microphone input, the five-second microphone test displays the input level. If microphone access is denied, MP4 can still record system audio without requesting microphone access.

MacShot can open Apple's standard Screenshot panel. When automatic import is enabled in Settings, supported screenshots and recordings saved in common folders are added to the configured MacShot folder and evidence list.

## Supported formats

- Capture and edit: PNG, JPEG
- MacShot recording: GIF, MP4 (H.264)
- macOS standard-panel recordings: MOV files can be opened in the system player

## Development

```sh
cargo run
cargo build --release
```

Verify macOS privacy descriptions and the app signature before packaging:

```sh
scripts/verify_macos_bundle.sh output/MacShot.app
```

The macOS app supports Apple Silicon on macOS 14 or later. Windows x64 can be cross-built for the GNU target; Windows runtime behavior must be checked on a Windows device.

The app bundles Noto Sans JP and Noto Sans SC under the SIL Open Font License 1.1. The license texts are in `assets/OFL-NotoSansJP.txt` and `assets/OFL-NotoSansSC.txt`.
