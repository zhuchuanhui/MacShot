# MacShot 1.2.2

[日本語](README.md) · [English](README.en.md)

MacShot 可用于截屏、编辑和比较图像，也可将屏幕录制为 GIF 或 MP4。拍摄的 PNG/JPEG 图片和录屏可在证据列表中查看。

## 下载

- macOS Apple Silicon（arm64）：[MacShot-v1.2.2-macos-arm64.zip](https://github.com/zhuchuanhui/MacShot/releases/download/v1.2.2/MacShot-v1.2.2-macos-arm64.zip)
- Windows x64：[MacShot-v1.2.2-windows-x64.zip](https://github.com/zhuchuanhui/MacShot/releases/download/v1.2.2/MacShot-v1.2.2-windows-x64.zip)
- [GitHub Release 页面](https://github.com/zhuchuanhui/MacShot/releases/tag/v1.2.2)

macOS：解压文件并将 `MacShot.app` 移到“应用程序”文件夹，便于以后启动。应用使用 Apple Development 签名，尚未公证。如果 macOS 阻止首次启动，请在 Finder 中按住 Control 键并点按应用，然后选择“打开”。从 Ad Hoc 签名版本升级后，如果屏幕录制未获许可，请在“系统设置 > 隐私与安全性 > 屏幕与系统音频录制”中将 MacShot 关闭再开启一次，然后重新启动应用。支持 macOS 14 或更高版本的 Apple Silicon Mac。

Windows：解压后运行 `MacShot.exe`。压缩包内含所需的 Windows 运行库 DLL。MP4 录制会采集默认播放设备和默认麦克风；如有需要，请在 Windows 隐私设置中允许桌面应用访问麦克风。

点按窗口的关闭按钮时，MacShot 不会退出，而会留在菜单栏／通知区域。在 macOS 上，从 Dock 选择“退出”也会隐藏 Dock 图标和窗口，并让菜单栏应用继续运行。选择菜单中的“打开 MacShot”可重新显示窗口，若要完全退出，请在 MacShot 菜单中选择“退出”。窗口隐藏期间，录制和连续截图仍会继续。

## 使用方法

### 截图

1. 在“截图与编辑”中选择活动窗口、主显示器、所有显示器、指定显示器或列表中的窗口。
2. 点按“截图并预览”，或按截图快捷键（默认：`Option`+`Shift`+`S`）。要截取区域，请点按“选择区域”并在屏幕上拖动。
3. 图片会保存到“证据列表”。文件名包含拍摄时间和序号；也可在设置中将时间显示在图片上。

### 连续截图

1. 选择“开始连续截图”，MacShot 窗口会隐藏。
2. 每按一次截图快捷键，应用就会拍摄并按顺序保存一张图片。
3. 从菜单栏或通知区域菜单停止。排队中的截图保存完成后，窗口会重新显示。

### 屏幕录制

在设置中选择 GIF 或 MP4（H.264）、分辨率和质量。点按“录制”或按录制快捷键（默认：`Option`+`Shift`+`R`）。可从菜单栏或再次按下快捷键停止。录屏列表会显示录制时长；点选录屏或按“播放”可在独立窗口播放 GIF、MP4 和受支持的 MOV。播放器支持播放／暂停、拖动进度、音量和静音。MP4 和受支持的 MOV 音频可在应用内播放，并显示实时音量；MOV 音频可能需要一些时间准备。GIF 按文件的循环设置播放，不包含音频。“编辑”可修改录屏名称，“外部打开”会使用系统播放器。MP4 会录制系统声音，并在获得许可时录制麦克风声音。

### 编辑和比较

在证据列表中使用连续编辑队列添加矩形、箭头或裁剪区域。编辑结果会另存为 PNG，不会覆盖原图。“图像比较”支持并排、叠加和像素差异视图，可比较两张 PNG 或 JPEG 图片。

## macOS 权限

设置页面会显示屏幕录制和麦克风权限状态。更改权限后，请点按“刷新权限状态”。五秒麦克风测试会显示输入音量。如果麦克风权限被拒绝，MP4 仍可在不请求麦克风权限的情况下录制系统声音。

MacShot 可以打开 macOS 标准“截图”面板。在设置中启用自动导入后，保存在常用文件夹中的支持格式截图和录屏会复制到 MacShot 保存文件夹，并显示在证据列表中。

## 支持格式

- 截图和编辑：PNG、JPEG
- MacShot 录屏：GIF、MP4（H.264）
- macOS 标准面板录屏：受支持的 MOV 可在 MacShot 中预览，也可通过系统播放器打开

## 开发

```sh
cargo run
cargo build --release
```

打包前请验证 macOS 隐私说明和应用签名：

```sh
scripts/verify_macos_bundle.sh output/MacShot.app
```

macOS 版本支持 macOS 14 或更高版本的 Apple Silicon。Windows x64 可使用 GNU 目标交叉编译；Windows 运行情况需要在 Windows 设备上验证。

应用内置了 Noto Sans JP 和 Noto Sans SC 字体，均采用 SIL Open Font License 1.1。许可证文本位于 `assets/OFL-NotoSansJP.txt` 和 `assets/OFL-NotoSansSC.txt`。
