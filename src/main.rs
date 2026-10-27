#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ab_glyph::{FontRef, PxScale};
#[cfg(target_os = "macos")]
use block2::RcBlock;
use bytes::Bytes;
use chrono::Local;
#[cfg(target_os = "windows")]
use cpal::traits::HostTrait;
use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use image::{DynamicImage, GenericImageView, ImageBuffer, ImageEncoder, Rgba};
use imageproc::{
    drawing::{
        draw_filled_rect_mut, draw_hollow_rect_mut, draw_line_segment_mut, draw_text_mut, text_size,
    },
    rect::Rect as ImageRect,
};
use mp4::{
    AacConfig, AudioObjectType, AvcConfig, ChannelConfig, FourCC, Mp4Config, Mp4Sample, Mp4Writer,
    SampleFreqIndex, TrackConfig,
};
#[cfg(target_os = "macos")]
use objc2_06::runtime::Bool as ObjcBool;
#[cfg(target_os = "macos")]
use objc2_avf_audio::{AVAudioApplication, AVAudioApplicationRecordPermission};
use openh264::decoder::Decoder as H264Decoder;
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder as H264Encoder, EncoderConfig, FrameRate, FrameType, UsageType},
    formats::{RgbaSliceU8, YUVBuffer},
};
#[cfg(target_os = "macos")]
use screencapturekit::{
    screenshot_manager::{CGImageExt, SCScreenshotManager},
    shareable_content::{SCShareableContent, SCShareableContentInfo, SCWindow},
    stream::{configuration::SCStreamConfiguration, content_filter::SCContentFilter},
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, Submenu},
};
use xcap::Monitor;
#[cfg(not(target_os = "macos"))]
use xcap::Window;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod recording_audio;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGShieldingWindowLevel() -> i32;
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MicrophonePermission {
    Authorized,
    NotDetermined,
    Denied,
    Restricted,
}

fn screen_recording_access_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: This Core Graphics API is a read-only authorization check.
        unsafe { CGPreflightScreenCaptureAccess() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
fn microphone_permission() -> MicrophonePermission {
    // SAFETY: AVAudioApplication is available on the app's macOS 14+ deployment target.
    microphone_permission_from_record_permission(unsafe {
        AVAudioApplication::sharedInstance().recordPermission()
    })
}

#[cfg(target_os = "macos")]
fn microphone_permission_from_record_permission(
    permission: AVAudioApplicationRecordPermission,
) -> MicrophonePermission {
    match permission {
        AVAudioApplicationRecordPermission::Granted => MicrophonePermission::Authorized,
        AVAudioApplicationRecordPermission::Undetermined => MicrophonePermission::NotDetermined,
        AVAudioApplicationRecordPermission::Denied => MicrophonePermission::Denied,
        _ => MicrophonePermission::Restricted,
    }
}

#[cfg(target_os = "macos")]
fn should_capture_microphone(permission: MicrophonePermission) -> bool {
    permission == MicrophonePermission::Authorized
}

#[cfg(target_os = "macos")]
fn microphone_permission_action_label(permission: MicrophonePermission) -> Option<&'static str> {
    match permission {
        MicrophonePermission::NotDetermined => Some("macOSに許可を確認"),
        MicrophonePermission::Denied => Some("設定でMacShotを許可"),
        MicrophonePermission::Authorized | MicrophonePermission::Restricted => None,
    }
}

#[cfg(target_os = "macos")]
fn microphone_permission_result_message(granted: bool) -> &'static str {
    if granted {
        "マイクの許可を確認しました。次のMP4録画からマイク音声を含めます。"
    } else {
        "マイクの許可は拒否されました。MP4はシステム音声のみで録画します。必要ならマイク設定でMacShotを許可してください。"
    }
}

fn append_recording_warning(status: &str, warning: Option<&str>) -> String {
    warning.map_or_else(
        || status.to_string(),
        |warning| format!("{status} — {warning}"),
    )
}

fn screen_permission_label() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        if screen_recording_access_granted() {
            "許可済み"
        } else {
            "未許可、またはmacOSへの反映待ち"
        }
    }
    #[cfg(target_os = "windows")]
    {
        "Windowsでは個別の画面収録許可は不要です"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "このOSでは状態を確認できません"
    }
}

#[cfg(target_os = "macos")]
fn microphone_permission_label_for(permission: MicrophonePermission) -> String {
    match permission {
        MicrophonePermission::Authorized => "許可済み".into(),
        MicrophonePermission::NotDetermined => "状態未決定（MP4はシステム音声のみ）".into(),
        MicrophonePermission::Denied => {
            "拒否されています（MP4はシステム音声のみ）。マイク設定で許可してください".into()
        }
        MicrophonePermission::Restricted => "このMacではマイク利用が制限されています".into(),
    }
}

fn microphone_permission_label() -> String {
    #[cfg(target_os = "macos")]
    {
        microphone_permission_label_for(microphone_permission())
    }
    #[cfg(target_os = "windows")]
    {
        match cpal::default_host().default_input_device() {
            Some(device) => format!(
                "既定のマイク: {}（Windowsのプライバシー設定も確認）",
                device
            ),
            None => "既定のマイクが見つかりません".into(),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "このOSでは状態を確認できません".into()
    }
}

fn should_show_screen_permission_banner(
    access_granted: bool,
    editing_existing_image: bool,
) -> bool {
    !access_granted && !editing_existing_image
}

fn minimize_main_window_for_capture(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    #[cfg(not(target_os = "windows"))]
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
}

fn restore_main_window(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

struct MacShot {
    monitors: Vec<Monitor>,
    windows: Vec<WindowChoice>,
    capture_target: CaptureTarget,
    continuous_capture: bool,
    capture_queue: VecDeque<CaptureTarget>,
    region_selecting: bool,
    region_drag_start: Option<egui::Pos2>,
    region_drag_current: Option<egui::Pos2>,
    region_overlay_position: egui::Pos2,
    region_overlay_size: egui::Vec2,
    #[cfg(target_os = "windows")]
    region_snapshot_tx: Sender<Result<image::RgbaImage, String>>,
    #[cfg(target_os = "windows")]
    region_snapshot_rx: Receiver<Result<image::RgbaImage, String>>,
    #[cfg(target_os = "windows")]
    region_geometry_tx: Sender<Result<(f32, f32, f32, f32), String>>,
    #[cfg(target_os = "windows")]
    region_geometry_rx: Receiver<Result<(f32, f32, f32, f32), String>>,
    #[cfg(target_os = "windows")]
    region_selection_deadline: Option<Instant>,
    #[cfg(target_os = "windows")]
    region_geometry_pending: bool,
    #[cfg(target_os = "windows")]
    region_background_texture: Option<egui::TextureHandle>,
    save_folder: Option<PathBuf>,
    evidence_files: Vec<PathBuf>,
    recording_files: Vec<PathBuf>,
    recording_preview_path: Option<PathBuf>,
    recording_preview_image: Option<DynamicImage>,
    recording_preview_texture: Option<egui::TextureHandle>,
    recording_preview_details: Option<String>,
    recording_playback: Option<RecordingPlayback>,
    scroll_to_recording_preview: bool,
    timestamp_enabled: bool,
    filename_prefix: String,
    show_capture_notice: bool,
    annotation_color: String,
    annotation_width: f32,
    status: String,
    capture_in_progress: bool,
    active_capture_sequence: Option<u64>,
    capture_deadline: Option<Instant>,
    capture_cancel: Option<Arc<AtomicBool>>,
    capture_tx: Sender<CaptureResult>,
    capture_rx: Receiver<CaptureResult>,
    _hotkey_manager: Option<GlobalHotKeyManager>,
    registered_hotkeys: Vec<HotKey>,
    capture_hotkey: HotKey,
    recording_hotkey: HotKey,
    capture_hotkey_text: String,
    recording_hotkey_text: String,
    hotkey_rx: Receiver<GlobalHotKeyEvent>,
    hotkey_tx: Sender<GlobalHotKeyEvent>,
    hotkey_initialized: bool,
    _tray_icon: Option<TrayIcon>,
    tray_capture_status_item: Option<MenuItem>,
    tray_recording_status_item: Option<MenuItem>,
    tray_recording_state: Option<bool>,
    tray_recording_seconds: Option<u64>,
    tray_capture_status_state: Option<(bool, bool, bool, u64, u64)>,
    tray_initialized: bool,
    tray_action_rx: Receiver<TrayAction>,
    tray_action_tx: Sender<TrayAction>,
    capture_sequence: u64,
    last_capture_sequence: u64,
    continuous_capture_saved_count: u64,
    continuous_capture_failed_count: u64,
    continuous_capture_session_active: bool,
    recording_sequence: u64,
    recording_format: RecordingFormat,
    recording_quality: RecordingQuality,
    display_density: DisplayDensity,
    font_size_percent: u32,
    applied_display_settings: Option<(DisplayDensity, u32)>,
    settings_layout: Vec<SettingsCardLayout>,
    saved_settings_layout: Vec<SettingsCardLayout>,
    editing_settings_layout: bool,
    capture_notice: Option<CaptureNotice>,
    recording_rx: Receiver<RecordingEvent>,
    recording_tx: Sender<RecordingEvent>,
    recording_stop_tx: Arc<Mutex<Option<Sender<()>>>>,
    recording: bool,
    recording_starting: bool,
    recording_started_at: Option<Instant>,
    recording_notice: Option<(PathBuf, String, Option<String>)>,
    recording_warning: Option<String>,
    #[cfg(target_os = "macos")]
    microphone_permission_result: Arc<Mutex<Option<bool>>>,
    #[cfg(target_os = "macos")]
    microphone_reset_confirmation: bool,
    #[cfg(target_os = "macos")]
    microphone_test_rx: Option<Receiver<recording_audio::MicrophoneTestEvent>>,
    #[cfg(target_os = "macos")]
    microphone_test_peak: f32,
    #[cfg(target_os = "macos")]
    microphone_test_message: Option<String>,
    record_pending_stop: bool,
    quit_after_recording: bool,
    page: Page,
    left_path: Option<PathBuf>,
    right_path: Option<PathBuf>,
    left_image: Option<DynamicImage>,
    right_image: Option<DynamicImage>,
    diff_image: Option<DynamicImage>,
    overlay_image: Option<DynamicImage>,
    left_texture: Option<egui::TextureHandle>,
    right_texture: Option<egui::TextureHandle>,
    diff_texture: Option<egui::TextureHandle>,
    overlay_texture: Option<egui::TextureHandle>,
    crop_selection: Option<egui::Rect>,
    crop_drag_start: Option<egui::Pos2>,
    edit_tool: EditTool,
    annotations: Vec<Annotation>,
    live_annotation: Option<Annotation>,
    editing_queue: Vec<PathBuf>,
    editing_index: Option<usize>,
    diff_count: u64,
    compare_mode: CompareMode,
    settings_error: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum CaptureTarget {
    ActiveWindow,
    PrimaryDisplay,
    AllDisplays,
    Display(usize),
    Window(u32),
}

fn capture_target_preflight_error(
    target: CaptureTarget,
    recording: bool,
    recording_starting: bool,
    monitor_count: usize,
    windows: &[WindowChoice],
) -> Option<&'static str> {
    if recording || recording_starting {
        Some("録画を終了してからスクリーンショットを撮影してください。")
    } else {
        match target {
            CaptureTarget::Display(index) if index >= monitor_count => {
                Some("撮影できる画面が見つかりません。画面収録の許可を確認してください。")
            }
            CaptureTarget::Window(id) if !windows.iter().any(|window| window.id == id) => {
                Some("撮影対象のウィンドウが見つかりません。ウィンドウ一覧を更新してください。")
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum EditTool {
    Crop,
    Rectangle,
    Arrow,
}

#[derive(Clone, Copy)]
struct Annotation {
    tool: EditTool,
    start: egui::Pos2,
    end: egui::Pos2,
}

struct CaptureNotice {
    sequence: u64,
    timestamp: String,
    path: PathBuf,
    width: u32,
    height: u32,
}

struct WindowChoice {
    id: u32,
    title: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Capture,
    Evidence,
    Compare,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum CompareMode {
    SideBySide,
    Overlay,
    Difference,
}

type CaptureResult = (
    u64,
    Result<(PathBuf, image::RgbaImage, String, u64), String>,
);

#[derive(Clone, Copy)]
enum TrayAction {
    ShowWindow,
    Capture,
    CaptureActiveWindow,
    CaptureAllDisplays,
    CaptureRegion,
    OpenNativeScreenshot,
    ToggleContinuousCapture,
    ToggleRecording,
    Quit,
}

enum RecordingEvent {
    Started {
        path: PathBuf,
        microphone_warning: Option<String>,
    },
    MicrophoneWarning(String),
    Finished(Result<PathBuf, String>),
}

enum RecordingPlaybackEvent {
    Frame(DynamicImage, Duration),
    End,
    Error(String),
}

struct RecordingPlayback {
    request_tx: Sender<()>,
    frame_rx: Receiver<RecordingPlaybackEvent>,
    playing: bool,
    frame_due: Instant,
    awaiting_frame: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RecordingFormat {
    #[default]
    Gif,
    Mp4,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DisplayDensity {
    Compact,
    #[default]
    Standard,
    Relaxed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SettingsCardId {
    Display,
    Permissions,
    Shortcuts,
    Recording,
    Capture,
}

impl SettingsCardId {
    const ALL: [Self; 5] = [
        Self::Display,
        Self::Permissions,
        Self::Shortcuts,
        Self::Recording,
        Self::Capture,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Display => "表示",
            Self::Permissions => "権限とデバイス",
            Self::Shortcuts => "ショートカット",
            Self::Recording => "録画",
            Self::Capture => "撮影の動作",
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Permissions => "permissions",
            Self::Shortcuts => "shortcuts",
            Self::Recording => "recording",
            Self::Capture => "capture",
        }
    }

    fn from_setting(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.setting() == value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsCardWidth {
    Half,
    #[default]
    Full,
}

impl SettingsCardWidth {
    fn setting(self) -> &'static str {
        match self {
            Self::Half => "half",
            Self::Full => "full",
        }
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "half" => Self::Half,
            _ => Self::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsCardPadding {
    Compact,
    #[default]
    Standard,
    Spacious,
}

impl SettingsCardPadding {
    fn setting(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
            Self::Spacious => "spacious",
        }
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "compact" => Self::Compact,
            "spacious" => Self::Spacious,
            _ => Self::Standard,
        }
    }

    fn margin(self) -> i8 {
        match self {
            Self::Compact => 8,
            Self::Standard => 16,
            Self::Spacious => 24,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SettingsCardLayout {
    id: SettingsCardId,
    width: SettingsCardWidth,
    padding: SettingsCardPadding,
}

fn default_settings_layout() -> Vec<SettingsCardLayout> {
    [
        (SettingsCardId::Recording, SettingsCardWidth::Half),
        (SettingsCardId::Capture, SettingsCardWidth::Half),
        (SettingsCardId::Shortcuts, SettingsCardWidth::Half),
        (SettingsCardId::Permissions, SettingsCardWidth::Half),
        (SettingsCardId::Display, SettingsCardWidth::Full),
    ]
    .into_iter()
    .map(|(id, width)| SettingsCardLayout {
        id,
        width,
        padding: if id == SettingsCardId::Display {
            SettingsCardPadding::Compact
        } else {
            SettingsCardPadding::Standard
        },
    })
    .collect()
}

fn serialize_settings_layout(layout: &[SettingsCardLayout]) -> String {
    layout
        .iter()
        .map(|card| {
            format!(
                "{}:{}:{}",
                card.id.setting(),
                card.width.setting(),
                card.padding.setting()
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn parse_settings_layout(value: &str) -> Vec<SettingsCardLayout> {
    let mut layout = Vec::new();
    for entry in value.split(';') {
        let mut fields = entry.split(':');
        let (Some(id), Some(width), Some(padding)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(id) = SettingsCardId::from_setting(id) else {
            continue;
        };
        if layout.iter().any(|card: &SettingsCardLayout| card.id == id) {
            continue;
        }
        layout.push(SettingsCardLayout {
            id,
            width: SettingsCardWidth::from_setting(width),
            padding: SettingsCardPadding::from_setting(padding),
        });
    }
    for default in default_settings_layout() {
        if !layout.iter().any(|card| card.id == default.id) {
            layout.push(default);
        }
    }
    layout
}

impl DisplayDensity {
    fn label(self) -> &'static str {
        match self {
            Self::Compact => "コンパクト",
            Self::Standard => "標準",
            Self::Relaxed => "ゆったり",
        }
    }

    fn spacing_scale(self) -> f32 {
        match self {
            Self::Compact => 0.75,
            Self::Standard => 1.0,
            Self::Relaxed => 1.2,
        }
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "compact" => Self::Compact,
            "relaxed" => Self::Relaxed,
            _ => Self::Standard,
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
            Self::Relaxed => "relaxed",
        }
    }
}

fn parse_font_size_percent(value: &str) -> u32 {
    value.parse::<u32>().unwrap_or(100).clamp(80, 140)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VideoQuality {
    #[default]
    Standard,
    High,
    Best,
}

impl VideoQuality {
    fn bitrate(self) -> u32 {
        match self {
            Self::Standard => 6_000_000,
            Self::High => 12_000_000,
            Self::Best => 20_000_000,
        }
    }

    fn frame_rate(self) -> f32 {
        match self {
            Self::Standard => 10.0,
            Self::High => 15.0,
            Self::Best => 30.0,
        }
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "high" => Self::High,
            "best" => Self::Best,
            _ => Self::Standard,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecordingQuality {
    mp4_bitrate_mbps: u32,
    mp4_frame_rate: u32,
    gif_quality_percent: u32,
    gif_frame_rate: u32,
    resolution: RecordingResolution,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RecordingResolution {
    #[default]
    Original,
    P480,
    P720,
    P1080,
    P1440,
    P2160,
}

impl RecordingResolution {
    const ALL: [Self; 6] = [
        Self::Original,
        Self::P480,
        Self::P720,
        Self::P1080,
        Self::P1440,
        Self::P2160,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Original => "元の画面サイズ",
            Self::P480 => "480p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
            Self::P1440 => "2K / 1440p",
            Self::P2160 => "4K / 2160p",
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::P480 => "480p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
            Self::P1440 => "1440p",
            Self::P2160 => "2160p",
        }
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "480p" => Self::P480,
            "720p" => Self::P720,
            "1080p" => Self::P1080,
            "1440p" => Self::P1440,
            "2160p" => Self::P2160,
            _ => Self::Original,
        }
    }

    fn output_dimensions(self, width: u32, height: u32, even: bool) -> (u32, u32) {
        let max_height = match self {
            Self::Original => height,
            Self::P480 => 480,
            Self::P720 => 720,
            Self::P1080 => 1080,
            Self::P1440 => 1440,
            Self::P2160 => 2160,
        };
        let scale = (f64::from(max_height) / f64::from(height.max(1))).min(1.0);
        let multiple = if even { 2 } else { 1 };
        let scaled_width = ((f64::from(width.max(1)) * scale).round() as u32 / multiple) * multiple;
        let scaled_height =
            ((f64::from(height.max(1)) * scale).round() as u32 / multiple) * multiple;
        (scaled_width.max(multiple), scaled_height.max(multiple))
    }

    fn resize(self, image: image::RgbaImage, even: bool) -> image::RgbaImage {
        let (width, height) = self.output_dimensions(image.width(), image.height(), even);
        if width == image.width() && height == image.height() {
            image
        } else {
            image::imageops::resize(&image, width, height, image::imageops::FilterType::Lanczos3)
        }
    }
}

impl RecordingQuality {
    fn from_settings(
        legacy_video_quality: VideoQuality,
        mp4_bitrate_mbps: &str,
        mp4_frame_rate: &str,
        gif_quality_percent: &str,
        gif_frame_rate: &str,
        resolution: &str,
    ) -> Self {
        Self {
            mp4_bitrate_mbps: mp4_bitrate_mbps
                .parse()
                .unwrap_or(legacy_video_quality.bitrate() / 1_000_000)
                .clamp(2, 30),
            mp4_frame_rate: mp4_frame_rate
                .parse()
                .unwrap_or(legacy_video_quality.frame_rate() as u32)
                .clamp(5, 60),
            gif_quality_percent: gif_quality_percent.parse().unwrap_or(70).clamp(10, 100),
            gif_frame_rate: gif_frame_rate.parse().unwrap_or(10).clamp(1, 30),
            resolution: RecordingResolution::from_setting(resolution),
        }
    }

    fn mp4_bitrate(self) -> u32 {
        self.mp4_bitrate_mbps * 1_000_000
    }

    fn mp4_frame_interval(self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.mp4_frame_rate))
    }

    fn mp4_frame_duration_ms(self) -> u64 {
        (1000.0 / f64::from(self.mp4_frame_rate)).round() as u64
    }

    fn gif_frame_interval(self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.gif_frame_rate))
    }

    fn gif_quantizer_speed(self) -> i32 {
        (30.0 - (self.gif_quality_percent - 10) as f32 * 29.0 / 90.0).round() as i32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RecordingSizeEstimate {
    low_bytes: f64,
    high_bytes: f64,
}

fn estimate_recording_size_per_minute(
    format: RecordingFormat,
    quality: RecordingQuality,
    width: u32,
    height: u32,
) -> RecordingSizeEstimate {
    const MINUTE_SECONDS: f64 = 60.0;
    match format {
        RecordingFormat::Mp4 => {
            let total_bitrate = f64::from(quality.mp4_bitrate() + recording_audio::bitrate());
            let bytes = total_bitrate * MINUTE_SECONDS / 8.0 * 1.02;
            RecordingSizeEstimate {
                low_bytes: bytes,
                high_bytes: bytes,
            }
        }
        RecordingFormat::Gif => {
            // GIF size depends heavily on how much the screen changes. Use a broad
            // low-motion to high-motion range; quality mostly changes palette
            // quantization work, while frame rate and pixel count drive the estimate.
            let pixels = f64::from(width.max(1)) * f64::from(height.max(1));
            let frames = f64::from(quality.gif_frame_rate) * MINUTE_SECONDS;
            let quality_factor = 0.9 + f64::from(quality.gif_quality_percent) / 1000.0;
            let estimate =
                |bits_per_pixel: f64| pixels * frames * bits_per_pixel / 8.0 * quality_factor;
            RecordingSizeEstimate {
                low_bytes: estimate(0.02),
                high_bytes: estimate(0.12),
            }
        }
    }
}

fn format_estimated_size(bytes: f64) -> String {
    if bytes >= 1_000_000_000.0 {
        format!("{:.1} GB", bytes / 1_000_000_000.0)
    } else {
        format!("{:.0} MB", bytes / 1_000_000.0)
    }
}

impl RecordingFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::Mp4 => "MP4（H.264）",
        }
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "mp4" => Self::Mp4,
            _ => Self::Gif,
        }
    }
}

const DEFAULT_CAPTURE_HOTKEY: &str = "Alt+Shift+KeyS";
const DEFAULT_RECORDING_HOTKEY: &str = "Alt+Shift+KeyR";

fn recording_time_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn recording_playback_status(playing: bool) -> &'static str {
    if playing {
        "録画をアプリ内で再生しています（映像のみ）。"
    } else {
        "録画を一時停止しました。"
    }
}

fn continuous_capture_status_label(
    mode_active: bool,
    session_active: bool,
    capture_in_progress: bool,
    saved_count: u64,
    failed_count: u64,
) -> String {
    if mode_active && capture_in_progress {
        format!("連続撮影中 — 撮影中・保存{saved_count}枚・失敗{failed_count}件")
    } else if mode_active && failed_count > 0 {
        format!("連続撮影中 — 保存{saved_count}枚・失敗{failed_count}件")
    } else if mode_active && saved_count > 0 {
        format!("連続撮影中 — {saved_count}枚保存済み")
    } else if mode_active {
        "連続撮影中 — 保存待ち".into()
    } else if session_active {
        format!("撮影終了待ち — 保存{saved_count}枚・失敗{failed_count}件")
    } else if saved_count > 0 {
        format!("前回の連続撮影 — {saved_count}枚保存・失敗{failed_count}件")
    } else if failed_count > 0 {
        format!("前回の連続撮影 — 保存0枚・失敗{failed_count}件")
    } else {
        "連続撮影 — 停止中".into()
    }
}

#[cfg(any(target_os = "macos", test))]
fn continuous_capture_title(
    mode_active: bool,
    capture_in_progress: bool,
    saved_count: u64,
    failed_count: u64,
) -> Option<String> {
    if !mode_active {
        None
    } else if capture_in_progress {
        Some("撮影中".into())
    } else if saved_count > 0 && failed_count > 0 {
        Some(format!("✓{saved_count}!{failed_count}"))
    } else if saved_count > 0 {
        Some(format!("✓{saved_count}"))
    } else if failed_count > 0 {
        Some(format!("!{failed_count}"))
    } else {
        Some("連続".into())
    }
}

fn make_tray_icon(recording: bool) -> Icon {
    Icon::from_rgba(tray_icon_pixels(recording), 24, 24).expect("tray icon dimensions are valid")
}

fn tray_icon_pixels(recording: bool) -> Vec<u8> {
    let mut pixels = vec![0_u8; 24 * 24 * 4];
    let color = if recording {
        [220, 35, 45, 255]
    } else {
        [24, 24, 24, 255]
    };
    // Compact, flat-topped mirrorless body with a large front lens.
    for y in 8..18 {
        for x in 3..21 {
            tray_pixel(&mut pixels, x, y, color);
        }
    }
    for y in 6..9 {
        for x in 5..19 {
            tray_pixel(&mut pixels, x, y, color);
        }
    }
    for y in 5..7 {
        for x in 8..11 {
            tray_pixel(&mut pixels, x, y, color);
        }
    }
    for y in 6..7 {
        for x in 15..18 {
            tray_pixel(&mut pixels, x, y, color);
        }
    }

    // Concentric lens rings form the main detail of the tiny silhouette.
    for y in 7..19 {
        for x in 7..20 {
            let dx = x as i32 - 13;
            let dy = y as i32 - 13;
            let distance = dx * dx + dy * dy;
            if distance <= 25 {
                if distance >= 20 || (8..=12).contains(&distance) {
                    tray_pixel(&mut pixels, x, y, color);
                } else {
                    tray_pixel(&mut pixels, x, y, [0, 0, 0, 0]);
                }
            }
        }
    }
    if recording {
        // The recording lamp sits above the lens; no model name or logo is used.
        for y in 5..9 {
            for x in 19..23 {
                let dx = x as i32 - 21;
                let dy = y as i32 - 7;
                if dx * dx + dy * dy <= 4 {
                    tray_pixel(&mut pixels, x, y, color);
                }
            }
        }
    }
    pixels
}

fn tray_pixel(pixels: &mut [u8], x: usize, y: usize, color: [u8; 4]) {
    let index = (y * 24 + x) * 4;
    pixels[index..index + 4].copy_from_slice(&color);
}

fn dispatch_recording_menu_action(
    recording_stop_tx: &Arc<Mutex<Option<Sender<()>>>>,
    action_tx: &Sender<TrayAction>,
) {
    let stop = recording_stop_tx.lock().ok().and_then(|stop| stop.clone());
    if let Some(stop) = stop {
        let _ = stop.send(());
    } else {
        let _ = action_tx.send(TrayAction::ToggleRecording);
    }
}

impl Default for MacShot {
    fn default() -> Self {
        let monitors = Monitor::all().unwrap_or_default();
        let (windows, mut status) = match discover_windows() {
            Ok(windows) => (
                windows,
                "準備完了。ショートカットまたは撮影ボタンから開始できます。".to_string(),
            ),
            Err(error) => (Vec::new(), capture_failure_message(&error)),
        };
        if cfg!(target_os = "macos") && !screen_recording_access_granted() {
            status = "画面収録の許可を設定してください。撮影画面の案内から要求できます。".into();
        }
        let (capture_tx, capture_rx) = mpsc::channel();
        #[cfg(target_os = "windows")]
        let (region_snapshot_tx, region_snapshot_rx) = mpsc::channel();
        #[cfg(target_os = "windows")]
        let (region_geometry_tx, region_geometry_rx) = mpsc::channel();
        let (hotkey_tx, hotkey_rx) = mpsc::channel();
        let (tray_action_tx, tray_action_rx) = mpsc::channel();
        let (recording_tx, recording_rx) = mpsc::channel();
        let (capture_hotkey_text, recording_hotkey_text) = load_hotkey_settings();
        let capture_hotkey_text = canonical_hotkey_text(&capture_hotkey_text);
        let recording_hotkey_text = canonical_hotkey_text(&recording_hotkey_text);
        let folder_setting = load_setting("save_folder", "");
        let save_folder = (!folder_setting.is_empty()).then(|| PathBuf::from(folder_setting));
        let target_setting = load_setting("capture_target", "active");
        let capture_target = match target_setting.as_str() {
            "primary" => CaptureTarget::PrimaryDisplay,
            "all" => CaptureTarget::AllDisplays,
            value if value.starts_with("display:") => value[8..]
                .parse::<usize>()
                .ok()
                .filter(|index| *index < monitors.len())
                .map(CaptureTarget::Display)
                .unwrap_or(CaptureTarget::ActiveWindow),
            value if value.starts_with("window:") => value[7..]
                .parse::<u32>()
                .ok()
                .filter(|id| windows.iter().any(|window| window.id == *id))
                .map(CaptureTarget::Window)
                .unwrap_or(CaptureTarget::ActiveWindow),
            _ => CaptureTarget::ActiveWindow,
        };
        let timestamp_enabled = load_setting("timestamp_enabled", "true") != "false";
        let filename_prefix = load_setting("filename_prefix", "evidence");
        let show_capture_notice = load_setting("show_capture_notice", "true") != "false";
        let annotation_color = load_setting("annotation_color", "red");
        let annotation_width = load_setting("annotation_width", "3.0")
            .parse::<f32>()
            .unwrap_or(3.0)
            .clamp(1.0, 8.0);
        let capture_hotkey = capture_hotkey_text
            .parse()
            .unwrap_or_else(|_| DEFAULT_CAPTURE_HOTKEY.parse().unwrap());
        let recording_hotkey = recording_hotkey_text
            .parse()
            .unwrap_or_else(|_| DEFAULT_RECORDING_HOTKEY.parse().unwrap());
        let recording_format =
            RecordingFormat::from_setting(&load_setting("recording_format", "gif"));
        let legacy_video_quality =
            VideoQuality::from_setting(&load_setting("video_quality", "standard"));
        let recording_quality = RecordingQuality::from_settings(
            legacy_video_quality,
            &load_setting("mp4_bitrate_mbps", ""),
            &load_setting("mp4_frame_rate", ""),
            &load_setting("gif_quality_percent", "70"),
            &load_setting("gif_frame_rate", "10"),
            &load_setting("recording_resolution", "original"),
        );
        let display_density =
            DisplayDensity::from_setting(&load_setting("display_density", "standard"));
        let font_size_percent = parse_font_size_percent(&load_setting("font_size_percent", "100"));
        let settings_layout = parse_settings_layout(&load_setting(
            "settings_layout",
            &serialize_settings_layout(&default_settings_layout()),
        ));
        Self {
            monitors,
            windows,
            capture_target,
            continuous_capture: false,
            capture_queue: VecDeque::new(),
            region_selecting: false,
            region_drag_start: None,
            region_drag_current: None,
            region_overlay_position: egui::Pos2::ZERO,
            region_overlay_size: egui::Vec2::ZERO,
            #[cfg(target_os = "windows")]
            region_snapshot_tx,
            #[cfg(target_os = "windows")]
            region_snapshot_rx,
            #[cfg(target_os = "windows")]
            region_geometry_tx,
            #[cfg(target_os = "windows")]
            region_geometry_rx,
            #[cfg(target_os = "windows")]
            region_selection_deadline: None,
            #[cfg(target_os = "windows")]
            region_geometry_pending: false,
            #[cfg(target_os = "windows")]
            region_background_texture: None,
            save_folder,
            evidence_files: Vec::new(),
            recording_files: Vec::new(),
            recording_preview_path: None,
            recording_preview_image: None,
            recording_preview_texture: None,
            recording_preview_details: None,
            recording_playback: None,
            scroll_to_recording_preview: false,
            timestamp_enabled,
            filename_prefix,
            show_capture_notice,
            annotation_color,
            annotation_width,
            status,
            capture_in_progress: false,
            active_capture_sequence: None,
            capture_deadline: None,
            capture_cancel: None,
            capture_tx,
            capture_rx,
            _hotkey_manager: None,
            registered_hotkeys: Vec::new(),
            capture_hotkey,
            recording_hotkey,
            capture_hotkey_text,
            recording_hotkey_text,
            hotkey_rx,
            hotkey_tx,
            hotkey_initialized: false,
            _tray_icon: None,
            tray_capture_status_item: None,
            tray_recording_status_item: None,
            tray_recording_state: None,
            tray_recording_seconds: None,
            tray_capture_status_state: None,
            tray_initialized: false,
            tray_action_rx,
            tray_action_tx,
            capture_sequence: 0,
            last_capture_sequence: 0,
            continuous_capture_saved_count: 0,
            continuous_capture_failed_count: 0,
            continuous_capture_session_active: false,
            recording_sequence: 0,
            recording_format,
            recording_quality,
            display_density,
            font_size_percent,
            applied_display_settings: None,
            settings_layout: settings_layout.clone(),
            saved_settings_layout: settings_layout,
            editing_settings_layout: false,
            capture_notice: None,
            recording_rx,
            recording_tx,
            recording_stop_tx: Arc::new(Mutex::new(None)),
            recording: false,
            recording_starting: false,
            recording_started_at: None,
            recording_notice: None,
            recording_warning: None,
            #[cfg(target_os = "macos")]
            microphone_permission_result: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "macos")]
            microphone_reset_confirmation: false,
            #[cfg(target_os = "macos")]
            microphone_test_rx: None,
            #[cfg(target_os = "macos")]
            microphone_test_peak: 0.0,
            #[cfg(target_os = "macos")]
            microphone_test_message: None,
            record_pending_stop: false,
            quit_after_recording: false,
            page: Page::Capture,
            left_path: None,
            right_path: None,
            left_image: None,
            right_image: None,
            diff_image: None,
            overlay_image: None,
            left_texture: None,
            right_texture: None,
            diff_texture: None,
            overlay_texture: None,
            crop_selection: None,
            crop_drag_start: None,
            edit_tool: EditTool::Crop,
            annotations: Vec::new(),
            live_annotation: None,
            editing_queue: Vec::new(),
            editing_index: None,
            diff_count: 0,
            compare_mode: CompareMode::SideBySide,
            settings_error: None,
        }
    }
}

fn discover_windows() -> Result<Vec<WindowChoice>, String> {
    #[cfg(target_os = "macos")]
    {
        let content = SCShareableContent::get().map_err(|error| {
            format!("ScreenCaptureKitでウィンドウ一覧を取得できません: {error}")
        })?;
        Ok(content
            .windows()
            .into_iter()
            .filter(is_captureable_screen_capture_window)
            .filter_map(|window| {
                Some(WindowChoice {
                    id: window.window_id(),
                    title: window.title()?,
                })
            })
            .collect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Window::all()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter_map(|window| {
                let id = window.id().ok()?;
                let title = window.title().ok()?;
                if title.trim().is_empty() || !is_captureable_window(&window) {
                    return None;
                }
                Some(WindowChoice { id, title })
            })
            .collect())
    }
}

fn default_save_folder() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Pictures").join("MacShot")
}

fn settings_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library")
        .join("Application Support");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_default();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config");
    base.join("MacShot").join("settings.txt")
}

#[cfg(any(target_os = "macos", test))]
fn app_bundle_path_from_executable(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn current_macos_app_bundle_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|executable| app_bundle_path_from_executable(&executable))
}

fn load_hotkey_settings() -> (String, String) {
    let Ok(contents) = std::fs::read_to_string(settings_path()) else {
        return (
            DEFAULT_CAPTURE_HOTKEY.into(),
            DEFAULT_RECORDING_HOTKEY.into(),
        );
    };
    parse_hotkey_settings(&contents)
}

fn parse_hotkey_settings(contents: &str) -> (String, String) {
    let mut lines = contents.lines();
    let capture = lines.next().unwrap_or(DEFAULT_CAPTURE_HOTKEY).trim();
    let recording = lines.next().unwrap_or(DEFAULT_RECORDING_HOTKEY).trim();
    (
        if capture.is_empty() {
            DEFAULT_CAPTURE_HOTKEY
        } else {
            capture
        }
        .into(),
        if recording.is_empty() {
            DEFAULT_RECORDING_HOTKEY
        } else {
            recording
        }
        .into(),
    )
}

fn load_setting(key: &str, fallback: &str) -> String {
    std::fs::read_to_string(settings_path())
        .map(|contents| parse_setting(&contents, key, fallback))
        .unwrap_or_else(|_| fallback.to_string())
}

fn parse_setting(contents: &str, key: &str, fallback: &str) -> String {
    contents
        .lines()
        .find_map(|line| {
            let (saved_key, value) = line.split_once('=')?;
            (saved_key.trim() == key).then(|| value.trim().to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

struct Preferences<'a> {
    capture_hotkey: &'a str,
    recording_hotkey: &'a str,
    capture_target: &'a str,
    timestamp_enabled: bool,
    filename_prefix: &'a str,
    show_capture_notice: bool,
    annotation_color: &'a str,
    annotation_width: f32,
    save_folder: &'a Path,
    recording_format: RecordingFormat,
    recording_quality: RecordingQuality,
    display_density: DisplayDensity,
    font_size_percent: u32,
    settings_layout: &'a [SettingsCardLayout],
}

fn save_preferences(preferences: &Preferences<'_>) -> Result<(), String> {
    let path = settings_path();
    let parent = path
        .parent()
        .ok_or_else(|| "設定フォルダを取得できません。".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    write_preferences(&mut file, preferences)
}

fn write_preferences(file: &mut impl Write, preferences: &Preferences<'_>) -> Result<(), String> {
    writeln!(file, "{}", preferences.capture_hotkey).map_err(|error| error.to_string())?;
    writeln!(file, "{}", preferences.recording_hotkey).map_err(|error| error.to_string())?;
    writeln!(file, "capture_target={}", preferences.capture_target)
        .map_err(|error| error.to_string())?;
    writeln!(file, "timestamp_enabled={}", preferences.timestamp_enabled)
        .map_err(|error| error.to_string())?;
    writeln!(file, "filename_prefix={}", preferences.filename_prefix)
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "show_capture_notice={}",
        preferences.show_capture_notice
    )
    .map_err(|error| error.to_string())?;
    writeln!(file, "annotation_color={}", preferences.annotation_color)
        .map_err(|error| error.to_string())?;
    writeln!(file, "annotation_width={}", preferences.annotation_width)
        .map_err(|error| error.to_string())?;
    writeln!(file, "save_folder={}", preferences.save_folder.display())
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "recording_format={}",
        preferences.recording_format.extension()
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "mp4_bitrate_mbps={}",
        preferences.recording_quality.mp4_bitrate_mbps
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "mp4_frame_rate={}",
        preferences.recording_quality.mp4_frame_rate
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "gif_quality_percent={}",
        preferences.recording_quality.gif_quality_percent
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "gif_frame_rate={}",
        preferences.recording_quality.gif_frame_rate
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "recording_resolution={}",
        preferences.recording_quality.resolution.setting()
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "display_density={}",
        preferences.display_density.setting()
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "font_size_percent={}",
        preferences.font_size_percent.clamp(80, 140)
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "settings_layout={}",
        serialize_settings_layout(preferences.settings_layout)
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn evidence_images(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg"
                    )
                })
                .unwrap_or(false)
        })
        .collect();
    files.sort_by(|left, right| {
        let left_key = evidence_sort_key(left);
        let right_key = evidence_sort_key(right);
        right_key
            .0
            .cmp(&left_key.0)
            .then_with(|| right_key.1.cmp(&left_key.1))
            .then_with(|| left_key.2.cmp(&right_key.2))
            .then_with(|| left_key.3.cmp(&right_key.3))
    });
    files
}

fn recording_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(extension.to_ascii_lowercase().as_str(), "gif" | "mp4")
                })
        })
        .collect();
    files.sort_by(|left, right| {
        let left_time = std::fs::metadata(left)
            .and_then(|meta| meta.modified())
            .ok();
        let right_time = std::fs::metadata(right)
            .and_then(|meta| meta.modified())
            .ok();
        right_time.cmp(&left_time)
    });
    files
}

fn recording_preview_display_size(
    image_width: u32,
    image_height: u32,
    available_width: f32,
) -> egui::Vec2 {
    if image_width == 0 || image_height == 0 {
        return egui::Vec2::ZERO;
    }

    const MAX_WIDTH: f32 = 720.0;
    const MAX_HEIGHT: f32 = 405.0;
    let max_width = available_width.clamp(1.0, MAX_WIDTH);
    let scale = (max_width / image_width as f32)
        .min(MAX_HEIGHT / image_height as f32)
        .min(1.0);
    egui::vec2(image_width as f32 * scale, image_height as f32 * scale)
}

fn recording_preview(path: &Path) -> Result<(DynamicImage, String), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let file_size = std::fs::metadata(path)
        .map(|meta| meta.len())
        .unwrap_or_default();
    if extension == "gif" {
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        options.skip_frame_decoding(true);
        let mut decoder = options.read_info(file).map_err(|error| error.to_string())?;
        let (width, height) = (decoder.width(), decoder.height());
        let mut frame_count = 0_u64;
        let mut duration_ms = 0_u64;
        while let Some(frame) = decoder
            .read_next_frame()
            .map_err(|error| error.to_string())?
        {
            frame_count += 1;
            duration_ms += u64::from(frame.delay) * 10;
        }
        drop(decoder);
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(file).map_err(|error| error.to_string())?;
        let first = decoder
            .read_next_frame()
            .map_err(|error| error.to_string())?
            .map(|frame| frame.buffer.to_vec());
        let pixels = first.ok_or_else(|| "GIFに表示できるフレームがありません。".to_string())?;
        let image = image::RgbaImage::from_raw(u32::from(width), u32::from(height), pixels)
            .ok_or_else(|| "GIFのプレビュー画像を読み込めません。".to_string())?;
        return Ok((
            DynamicImage::ImageRgba8(image),
            format!(
                "GIF・{frame_count}フレーム・{:.2}秒・{}",
                duration_ms as f64 / 1000.0,
                format_file_size(file_size)
            ),
        ));
    }
    if extension != "mp4" {
        return Err("この録画形式のプレビューには対応していません。".into());
    }

    use openh264::formats::YUVSource;
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let size = file.metadata().map_err(|error| error.to_string())?.len();
    let mut reader = mp4::Mp4Reader::read_header(file, size).map_err(|error| error.to_string())?;
    let has_aac_audio = reader.tracks().values().any(|track| {
        track
            .media_type()
            .is_ok_and(|kind| kind == mp4::MediaType::AAC)
    });
    let track_id = *reader
        .tracks()
        .iter()
        .find(|(_, track)| {
            track
                .media_type()
                .is_ok_and(|kind| kind == mp4::MediaType::H264)
        })
        .map(|(id, _)| id)
        .ok_or_else(|| "MP4にH.264映像トラックがありません。".to_string())?;
    let track = reader
        .tracks()
        .get(&track_id)
        .ok_or("MP4映像トラックがありません。")?;
    let avcc = &track
        .trak
        .mdia
        .minf
        .stbl
        .stsd
        .avc1
        .as_ref()
        .ok_or("MP4映像情報がありません。")?
        .avcc;
    let mut decoder = H264Decoder::new().map_err(|error| error.to_string())?;
    let mut packet = Vec::new();
    for nal in avcc
        .sequence_parameter_sets
        .iter()
        .chain(&avcc.picture_parameter_sets)
    {
        packet.extend_from_slice(&[0, 0, 0, 1]);
        packet.extend_from_slice(&nal.bytes);
    }
    let _ = decoder.decode(&packet).map_err(|error| error.to_string())?;
    let sample = reader
        .read_sample(track_id, 1)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "MP4に映像フレームがありません。".to_string())?;
    let mut cursor = 0;
    while cursor + 4 <= sample.bytes.len() {
        let len = u32::from_be_bytes(sample.bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        let end = cursor
            .checked_add(len)
            .filter(|end| *end <= sample.bytes.len())
            .ok_or_else(|| "MP4映像データを読み取れません。".to_string())?;
        packet.extend_from_slice(&[0, 0, 0, 1]);
        packet.extend_from_slice(&sample.bytes[cursor..end]);
        cursor = end;
    }
    let decoded = decoder
        .decode(&packet)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "MP4の最初のフレームをデコードできません。".to_string())?;
    let (width, height) = decoded.dimensions();
    let mut rgb = vec![0; decoded.rgb8_len()];
    decoded.write_rgb8(&mut rgb);
    let image = image::RgbImage::from_raw(width as u32, height as u32, rgb)
        .ok_or_else(|| "MP4のプレビュー画像を作成できません。".to_string())?;
    Ok((
        DynamicImage::ImageRgb8(image),
        format!(
            "MP4・H.264・{}・{:.2}秒・{}",
            if has_aac_audio {
                "AAC音声トラックあり"
            } else {
                "音声トラックなし"
            },
            reader.duration().as_secs_f64(),
            format_file_size(file_size)
        ),
    ))
}

enum RecordingFrameReader {
    Gif {
        path: PathBuf,
        decoder: gif::Decoder<std::fs::File>,
        repeat: gif::Repeat,
        completed_repetitions: u16,
        frames_in_current_loop: u32,
    },
    Mp4 {
        reader: mp4::Mp4Reader<std::fs::File>,
        track_id: u32,
        sample_id: u32,
        sample_count: u32,
        timescale: u32,
        decoder: H264Decoder,
    },
}

impl RecordingFrameReader {
    fn open(path: &Path) -> Result<Self, String> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if extension == "gif" {
            let decoder = open_gif_playback_decoder(path)?;
            let repeat = decoder.repeat();
            return Ok(Self::Gif {
                path: path.to_path_buf(),
                decoder,
                repeat,
                completed_repetitions: 0,
                frames_in_current_loop: 0,
            });
        }
        if extension != "mp4" {
            return Err("この録画形式の再生には対応していません。".into());
        }

        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let size = file.metadata().map_err(|error| error.to_string())?.len();
        let reader = mp4::Mp4Reader::read_header(file, size).map_err(|error| error.to_string())?;
        let track_id = *reader
            .tracks()
            .iter()
            .find(|(_, track)| {
                track
                    .media_type()
                    .is_ok_and(|kind| kind == mp4::MediaType::H264)
            })
            .map(|(id, _)| id)
            .ok_or_else(|| "MP4にH.264映像トラックがありません。".to_string())?;
        let track = reader
            .tracks()
            .get(&track_id)
            .ok_or("MP4映像トラックがありません。")?;
        let timescale = track.timescale();
        let sample_count = reader
            .sample_count(track_id)
            .map_err(|error| error.to_string())?;
        let avcc = &track
            .trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_ref()
            .ok_or("MP4映像情報がありません。")?
            .avcc;
        let mut packet = Vec::new();
        for nal in avcc
            .sequence_parameter_sets
            .iter()
            .chain(&avcc.picture_parameter_sets)
        {
            packet.extend_from_slice(&[0, 0, 0, 1]);
            packet.extend_from_slice(&nal.bytes);
        }
        let mut decoder = H264Decoder::new().map_err(|error| error.to_string())?;
        let _ = decoder.decode(&packet).map_err(|error| error.to_string())?;
        Ok(Self::Mp4 {
            reader,
            track_id,
            sample_id: 1,
            sample_count,
            timescale,
            decoder,
        })
    }

    fn next_frame(&mut self) -> Result<Option<(DynamicImage, Duration)>, String> {
        match self {
            Self::Gif {
                path,
                decoder,
                repeat,
                completed_repetitions,
                frames_in_current_loop,
            } => loop {
                let width = u32::from(decoder.width());
                let height = u32::from(decoder.height());
                let Some(frame) = decoder
                    .read_next_frame()
                    .map_err(|error| error.to_string())?
                else {
                    if *frames_in_current_loop == 0 {
                        return Ok(None);
                    }
                    let should_repeat = match repeat {
                        gif::Repeat::Infinite => true,
                        gif::Repeat::Finite(repetitions) => *completed_repetitions < *repetitions,
                    };
                    if !should_repeat {
                        return Ok(None);
                    }
                    *completed_repetitions = completed_repetitions.saturating_add(1);
                    *frames_in_current_loop = 0;
                    *decoder = open_gif_playback_decoder(path)?;
                    continue;
                };
                *frames_in_current_loop = frames_in_current_loop.saturating_add(1);
                let delay = Duration::from_millis(u64::from(frame.delay.max(2)) * 10);
                let pixels = frame.buffer.to_vec();
                let image = image::RgbaImage::from_raw(width, height, pixels)
                    .ok_or_else(|| "GIFフレームを画像に変換できません。".to_string())?;
                return Ok(Some((DynamicImage::ImageRgba8(image), delay)));
            },
            Self::Mp4 {
                reader,
                track_id,
                sample_id,
                sample_count,
                timescale,
                decoder,
            } => {
                use openh264::formats::YUVSource;
                while *sample_id <= *sample_count {
                    let Some(sample) = reader
                        .read_sample(*track_id, *sample_id)
                        .map_err(|error| error.to_string())?
                    else {
                        *sample_id = sample_count.saturating_add(1);
                        return Ok(None);
                    };
                    *sample_id = sample_id.saturating_add(1);
                    let delay = Duration::from_secs_f64(
                        f64::from(sample.duration) / f64::from((*timescale).max(1)),
                    );
                    let mut packet = Vec::with_capacity(sample.bytes.len());
                    let mut cursor = 0;
                    while cursor + 4 <= sample.bytes.len() {
                        let len = u32::from_be_bytes(
                            sample.bytes[cursor..cursor + 4]
                                .try_into()
                                .map_err(|_| "MP4映像データを読み取れません。")?,
                        ) as usize;
                        cursor += 4;
                        let end = cursor
                            .checked_add(len)
                            .filter(|end| *end <= sample.bytes.len())
                            .ok_or_else(|| "MP4映像データを読み取れません。".to_string())?;
                        packet.extend_from_slice(&[0, 0, 0, 1]);
                        packet.extend_from_slice(&sample.bytes[cursor..end]);
                        cursor = end;
                    }
                    let Some(decoded) =
                        decoder.decode(&packet).map_err(|error| error.to_string())?
                    else {
                        continue;
                    };
                    let (width, height) = decoded.dimensions();
                    let mut rgb = vec![0; decoded.rgb8_len()];
                    decoded.write_rgb8(&mut rgb);
                    let image = image::RgbImage::from_raw(width as u32, height as u32, rgb)
                        .ok_or_else(|| "MP4映像フレームを作成できません。".to_string())?;
                    return Ok(Some((DynamicImage::ImageRgb8(image), delay)));
                }
                Ok(None)
            }
        }
    }
}

fn open_gif_playback_decoder(path: &Path) -> Result<gif::Decoder<std::fs::File>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.read_info(file).map_err(|error| error.to_string())
}

fn start_recording_playback(path: PathBuf) -> RecordingPlayback {
    let (request_tx, request_rx) = mpsc::channel::<()>();
    let (frame_tx, frame_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = match RecordingFrameReader::open(&path) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = frame_tx.send(RecordingPlaybackEvent::Error(error));
                return;
            }
        };
        while request_rx.recv().is_ok() {
            let event = match reader.next_frame() {
                Ok(Some((frame, delay))) => RecordingPlaybackEvent::Frame(frame, delay),
                Ok(None) => RecordingPlaybackEvent::End,
                Err(error) => RecordingPlaybackEvent::Error(error),
            };
            let ended = matches!(
                event,
                RecordingPlaybackEvent::End | RecordingPlaybackEvent::Error(_)
            );
            if frame_tx.send(event).is_err() || ended {
                return;
            }
        }
    });
    RecordingPlayback {
        request_tx,
        frame_rx,
        playing: true,
        frame_due: Instant::now(),
        awaiting_frame: false,
    }
}

fn format_file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

fn parse_evidence_identity(path: &Path) -> Option<(String, u64)> {
    let stem = strip_collision_suffix(path.file_stem()?.to_str()?);
    let stem = stem
        .strip_suffix("-crop")
        .or_else(|| stem.strip_suffix("-marked"))
        .unwrap_or(stem);
    let (before_sequence, sequence) = stem.rsplit_once('-')?;
    let (before_millis, millis) = before_sequence.rsplit_once('-')?;
    let (before_time, time) = before_millis.rsplit_once('-')?;
    let (_, date) = before_time.rsplit_once('-')?;
    if date.len() != 8
        || time.len() != 6
        || millis.len() != 3
        || !date.chars().all(|value| value.is_ascii_digit())
        || !time.chars().all(|value| value.is_ascii_digit())
        || !millis.chars().all(|value| value.is_ascii_digit())
    {
        return None;
    }
    let sequence = sequence.parse::<u64>().ok()?;
    Some((format!("{date}-{time}-{millis}"), sequence))
}

fn evidence_order_stamp(path: &Path) -> String {
    if let Some((stamp, _)) = parse_evidence_identity(path) {
        return stamp;
    }
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(|modified| {
            chrono::DateTime::<Local>::from(modified)
                .format("%Y%m%d-%H%M%S-%3f")
                .to_string()
        })
        .unwrap_or_default()
}

fn evidence_sort_key(path: &Path) -> (String, u64, u8, String) {
    let (stamp, sequence) =
        parse_evidence_identity(path).unwrap_or_else(|| (evidence_order_stamp(path), 0));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let stem = strip_collision_suffix(stem);
    let edit_order = u8::from(stem.ends_with("-crop") || stem.ends_with("-marked"));
    (
        stamp,
        sequence,
        edit_order,
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string(),
    )
}

fn evidence_identity_for_edit(path: Option<&Path>, fallback_sequence: u64) -> (String, u64) {
    path.and_then(parse_evidence_identity)
        .unwrap_or_else(|| match path {
            Some(path) => (evidence_order_stamp(path), 0),
            None => (
                Local::now().format("%Y%m%d-%H%M%S-%3f").to_string(),
                fallback_sequence,
            ),
        })
}

fn sanitize_filename_prefix(prefix: &str) -> String {
    let prefix: String = prefix
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if prefix.trim_matches('_').is_empty() {
        "evidence".into()
    } else {
        prefix
    }
}

fn new_evidence_path(
    folder: &Path,
    prefix: &str,
    timestamp: &str,
    sequence: u64,
    suffix: &str,
) -> PathBuf {
    let prefix = sanitize_filename_prefix(prefix);
    folder.join(format!("{prefix}-{timestamp}-{sequence:03}{suffix}.png"))
}

#[cfg(test)]
fn new_recording_path(folder: &Path, timestamp: &str, sequence: u64) -> PathBuf {
    new_recording_path_for_format(folder, timestamp, sequence, RecordingFormat::Gif)
}

fn new_recording_path_for_format(
    folder: &Path,
    timestamp: &str,
    sequence: u64,
    format: RecordingFormat,
) -> PathBuf {
    folder.join(format!(
        "recording-{timestamp}-{sequence:03}.{}",
        format.extension()
    ))
}

fn strip_collision_suffix(stem: &str) -> &str {
    stem.rsplit_once("-copy-")
        .filter(|(_, number)| {
            !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
        })
        .map_or(stem, |(base, _)| base)
}

fn create_unique_file(folder: &Path, preferred: &Path) -> Result<(PathBuf, std::fs::File), String> {
    let stem = preferred
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "保存ファイル名を作成できません。".to_string())?;
    let extension = preferred
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();

    for copy_number in 1..=u32::MAX {
        let path = if copy_number == 1 {
            preferred.to_path_buf()
        } else {
            folder.join(format!("{stem}-copy-{copy_number}{extension}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }

    Err("同じ名前のファイルが多すぎるため保存できません。".into())
}

fn save_png_without_overwrite(
    folder: &Path,
    preferred: &Path,
    image: &image::RgbaImage,
) -> Result<PathBuf, String> {
    let (path, mut file) = create_unique_file(folder, preferred)?;
    let result = image::codecs::png::PngEncoder::new(&mut file).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    );
    if let Err(error) = result {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error.to_string());
    }
    Ok(path)
}

fn gif_delay_centiseconds(elapsed: Duration) -> u16 {
    elapsed.as_millis().div_ceil(10).clamp(1, u16::MAX as u128) as u16
}

fn should_show_window_after_recording(continuous_capture: bool) -> bool {
    !continuous_capture
}

fn should_defer_quit_until_recording_finishes(recording: bool, recording_starting: bool) -> bool {
    recording || recording_starting
}

#[cfg(not(target_os = "macos"))]
fn is_macshot_window(window: &Window) -> bool {
    window.app_name().is_ok_and(|name| name == "MacShot")
        || window.title().is_ok_and(|title| title.contains("MacShot"))
}

#[cfg(not(target_os = "macos"))]
fn is_captureable_window(window: &Window) -> bool {
    if is_macshot_window(window) || window.app_name().is_ok_and(|name| name == "Window Server") {
        return false;
    }

    // Ignore menu bars, tooltips, and other utility surfaces that can appear
    // above the application window in the operating system's window list.
    window
        .width()
        .ok()
        .zip(window.height().ok())
        .is_some_and(|(width, height)| has_captureable_window_size(width, height))
}

#[cfg(any(not(target_os = "macos"), test))]
fn has_captureable_window_size(width: u32, height: u32) -> bool {
    width >= 120 && height >= 80
}

fn capture_failure_message(error: &str) -> String {
    let error = error.trim_end_matches(['。', '.']);
    if error.contains("ウィンドウが見つかりません") {
        return format!("撮影できませんでした: {error}。");
    }

    if cfg!(target_os = "macos") {
        if error.contains("TCC") && error.contains("拒否") {
            return "macOSから画面収録を拒否されました。「システム設定 > プライバシーとセキュリティ > 画面収録とシステムオーディオ録音」でMacShotを許可してください。一覧にない場合は「＋」からMacShot.appを追加してオンにし、MacShotを再起動します。既にオンの場合は一度オフ→オンしてから再起動してください。".into();
        }
        format!(
            "撮影できませんでした: {error}。画面が撮れない場合は「画面収録」の許可を確認してください。"
        )
    } else {
        format!("撮影できませんでした: {error}。")
    }
}

fn capture_timeout_message() -> &'static str {
    "撮影が15秒以内に完了しませんでした。画面収録の許可と他の撮影アプリを確認してください。画面は再表示されています。"
}

fn recording_failure_message(error: &str) -> String {
    #[cfg(target_os = "macos")]
    if let Some(message) = microphone_permission_failure_message(
        error,
        microphone_permission() == MicrophonePermission::Denied,
    ) {
        return message;
    }
    if cfg!(target_os = "macos") && error.contains("TCC") && error.contains("拒否") {
        capture_failure_message(error)
    } else {
        format!("録画できませんでした: {error}")
    }
}

#[cfg(any(target_os = "macos", test))]
fn microphone_permission_failure_message(error: &str, denied: bool) -> Option<String> {
    (denied && error.contains("マイク")).then(|| {
        "マイクの使用が拒否されています。「システム設定 > プライバシーとセキュリティ > マイク」でMacShotを許可し、権限状態を更新してから再試行してください。".into()
    })
}

#[cfg(not(target_os = "macos"))]
fn find_active_window() -> Result<Window, String> {
    let windows = Window::all().map_err(|error| error.to_string())?;
    let is_candidate = |window: &Window| {
        is_captureable_window(window) && window.title().is_ok_and(|title| !title.trim().is_empty())
    };

    let focused_index = windows
        .iter()
        .position(|window| is_candidate(window) && window.is_focused().unwrap_or(false));

    // On macOS, xcap returns windows in Core Graphics' front-to-back order.
    // The app window has already been hidden before capture, but macOS can still
    // report MacShot as the active application while focus is being transferred.
    #[cfg(target_os = "macos")]
    let active_index = focused_index.or_else(|| windows.iter().position(is_candidate));

    #[cfg(not(target_os = "macos"))]
    let active_index = focused_index;

    active_index
        .and_then(|index| windows.into_iter().nth(index))
        .ok_or_else(|| "アクティブウィンドウが見つかりません。".to_string())
}

struct DisplayCapture {
    #[cfg(target_os = "macos")]
    filter: SCContentFilter,
    #[cfg(target_os = "macos")]
    configuration: SCStreamConfiguration,
    #[cfg(not(target_os = "macos"))]
    monitor: Monitor,
}

#[cfg(any(target_os = "macos", test))]
fn display_capture_pixel_dimensions(
    width_points: u32,
    height_points: u32,
    scale_factor: f32,
) -> (u32, u32) {
    let scale = if scale_factor.is_finite() {
        f64::from(scale_factor.max(1.0))
    } else {
        1.0
    };
    let pixels = |points: u32| {
        (f64::from(points) * scale)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32
    };
    (pixels(width_points), pixels(height_points))
}

impl DisplayCapture {
    fn new(monitor: &Monitor) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            let monitor_id = monitor.id().map_err(|error| error.to_string())?;
            let content = SCShareableContent::get()
                .map_err(|error| format!("ScreenCaptureKitで画面一覧を取得できません: {error}"))?;
            let display = content
                .displays()
                .into_iter()
                .find(|display| display.display_id() == monitor_id)
                .ok_or_else(|| "ScreenCaptureKitで撮影対象の画面が見つかりません。".to_string())?;
            let filter = SCContentFilter::create()
                .with_display(&display)
                .with_excluding_windows(&[])
                .build()
                .map_err(|error| format!("画面撮影の準備に失敗しました: {error}"))?;
            // SCDisplay dimensions are points. Request the display's pixel
            // dimensions so Retina screenshots retain their full resolution.
            let (width, height) = display_capture_pixel_dimensions(
                display.width(),
                display.height(),
                monitor.scale_factor().unwrap_or(1.0),
            );
            let configuration = SCStreamConfiguration::new()
                .with_width(width)
                .with_height(height);
            Ok(Self {
                filter,
                configuration,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(Self {
                monitor: monitor.clone(),
            })
        }
    }

    fn capture_image(&self) -> Result<image::RgbaImage, String> {
        #[cfg(target_os = "macos")]
        {
            let captured = SCScreenshotManager::capture_image(&self.filter, &self.configuration)
                .map_err(|error| format!("ScreenCaptureKitで画面を撮影できません: {error}"))?;
            let width = u32::try_from(captured.width())
                .map_err(|_| "撮影画像の幅が上限を超えています。".to_string())?;
            let height = u32::try_from(captured.height())
                .map_err(|_| "撮影画像の高さが上限を超えています。".to_string())?;
            let pixels = captured
                .rgba_data()
                .map_err(|error| format!("撮影画像を読み取れません: {error}"))?;
            image::RgbaImage::from_raw(width, height, pixels)
                .ok_or_else(|| "撮影画像のサイズとピクセルデータが一致しません。".to_string())
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.monitor
                .capture_image()
                .map_err(|error| error.to_string())
        }
    }
}

#[cfg(target_os = "macos")]
fn capture_window_image(window_id: u32) -> Result<image::RgbaImage, String> {
    let content = SCShareableContent::get()
        .map_err(|error| format!("ScreenCaptureKitでウィンドウ一覧を取得できません: {error}"))?;
    let window = content
        .windows()
        .into_iter()
        .find(|window| window.window_id() == window_id)
        .ok_or_else(|| "ScreenCaptureKitで撮影対象のウィンドウが見つかりません。".to_string())?;
    let filter = SCContentFilter::create()
        .with_window(&window)
        .build()
        .map_err(|error| format!("ウィンドウ撮影の準備に失敗しました: {error}"))?;
    let (width, height) = SCShareableContentInfo::for_filter(&filter)
        .map(|info| info.pixel_size())
        .filter(|(width, height)| *width > 0 && *height > 0)
        .ok_or_else(|| "ウィンドウの撮影サイズを取得できません。".to_string())?;
    let configuration = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height);
    let captured = SCScreenshotManager::capture_image(&filter, &configuration)
        .map_err(|error| format!("ScreenCaptureKitでウィンドウを撮影できません: {error}"))?;
    let width = u32::try_from(captured.width())
        .map_err(|_| "撮影画像の幅が上限を超えています。".to_string())?;
    let height = u32::try_from(captured.height())
        .map_err(|_| "撮影画像の高さが上限を超えています。".to_string())?;
    let pixels = captured
        .rgba_data()
        .map_err(|error| format!("撮影画像を読み取れません: {error}"))?;
    image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| "撮影画像のサイズとピクセルデータが一致しません。".to_string())
}

#[cfg(target_os = "macos")]
fn is_captureable_screen_capture_window(window: &SCWindow) -> bool {
    if !window.is_on_screen() || window.window_layer() != 0 {
        return false;
    }
    let Some(title) = window.title() else {
        return false;
    };
    if !is_captureable_screen_capture_title(&title) {
        return false;
    }
    if window
        .owning_application()
        .is_some_and(|application| application.bundle_identifier() == "com.macshot.app")
    {
        return false;
    }
    let frame = window.frame();
    frame.size.width >= 120.0 && frame.size.height >= 80.0
}

#[cfg(target_os = "macos")]
fn is_captureable_screen_capture_title(title: &str) -> bool {
    let title = title.trim();
    !title.is_empty() && !title.eq_ignore_ascii_case("Software Cursor")
}

#[cfg(target_os = "macos")]
fn find_active_screen_capture_window_id() -> Result<u32, String> {
    let content = SCShareableContent::get()
        .map_err(|error| format!("ScreenCaptureKitでウィンドウ一覧を取得できません: {error}"))?;
    let windows = content.windows();
    windows
        .iter()
        .find(|window| is_captureable_screen_capture_window(window) && window.is_active())
        .or_else(|| {
            windows
                .iter()
                .find(|window| is_captureable_screen_capture_window(window))
        })
        .map(SCWindow::window_id)
        .ok_or_else(|| "アクティブウィンドウが見つかりません。".to_string())
}

#[cfg(target_os = "macos")]
fn monitor_for_screen_capture_window(window_id: u32) -> Result<Monitor, String> {
    let content = SCShareableContent::get()
        .map_err(|error| format!("ScreenCaptureKitでウィンドウ一覧を取得できません: {error}"))?;
    let window = content
        .windows()
        .into_iter()
        .find(|window| window.window_id() == window_id)
        .ok_or_else(|| "録画するウィンドウが見つかりません。".to_string())?;
    let frame = window.frame();
    let center_x = frame.origin.x + frame.size.width / 2.0;
    let center_y = frame.origin.y + frame.size.height / 2.0;
    let monitors = Monitor::all().map_err(|error| error.to_string())?;
    monitors
        .iter()
        .find(|monitor| {
            let Ok(x) = monitor.x() else { return false };
            let Ok(y) = monitor.y() else { return false };
            let Ok(width) = monitor.width() else {
                return false;
            };
            let Ok(height) = monitor.height() else {
                return false;
            };
            monitor_contains_point(center_x, center_y, x, y, width, height)
        })
        .cloned()
        .or_else(|| {
            monitors
                .iter()
                .find(|monitor| monitor.is_primary().unwrap_or(false))
                .cloned()
        })
        .or_else(|| monitors.first().cloned())
        .ok_or_else(|| "録画できる画面が見つかりません。".to_string())
}

#[cfg(any(target_os = "macos", test))]
fn monitor_contains_point(
    point_x: f64,
    point_y: f64,
    monitor_x: i32,
    monitor_y: i32,
    monitor_width: u32,
    monitor_height: u32,
) -> bool {
    point_x >= f64::from(monitor_x)
        && point_x < f64::from(monitor_x) + f64::from(monitor_width)
        && point_y >= f64::from(monitor_y)
        && point_y < f64::from(monitor_y) + f64::from(monitor_height)
}

fn capture_target_image(target: CaptureTarget) -> Result<image::RgbaImage, String> {
    match target {
        CaptureTarget::Display(index) => {
            let monitor = Monitor::all()
                .map_err(|e| e.to_string())?
                .get(index)
                .cloned()
                .ok_or_else(|| "選択した画面が見つかりません。".to_string())?;
            DisplayCapture::new(&monitor)?.capture_image()
        }
        CaptureTarget::PrimaryDisplay => {
            let monitors = Monitor::all().map_err(|e| e.to_string())?;
            let primary = monitors
                .iter()
                .find(|monitor| monitor.is_primary().unwrap_or(false))
                .or_else(|| monitors.first())
                .ok_or_else(|| "撮影できる画面が見つかりません。".to_string())?;
            DisplayCapture::new(primary)?.capture_image()
        }
        CaptureTarget::AllDisplays => capture_all_displays(),
        CaptureTarget::ActiveWindow => {
            #[cfg(target_os = "macos")]
            {
                capture_window_image(find_active_screen_capture_window_id()?)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let window = find_active_window()?;
                window.capture_image().map_err(|error| error.to_string())
            }
        }
        CaptureTarget::Window(id) => {
            #[cfg(target_os = "macos")]
            {
                capture_window_image(id)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let window = Window::all()
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .find(|window| window.id().ok() == Some(id))
                    .ok_or_else(|| "選択したウィンドウが見つかりません。".to_string())?;
                window.capture_image().map_err(|error| error.to_string())
            }
        }
    }
}

fn capture_all_displays() -> Result<image::RgbaImage, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    if monitors.is_empty() {
        return Err("撮影できる画面が見つかりません。".into());
    }
    let mut captures = Vec::with_capacity(monitors.len());
    for monitor in &monitors {
        let x = monitor.x().map_err(|e| e.to_string())?;
        let y = monitor.y().map_err(|e| e.to_string())?;
        let width = monitor.width().map_err(|e| e.to_string())?;
        let height = monitor.height().map_err(|e| e.to_string())?;
        let scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
        let image = DisplayCapture::new(monitor)?.capture_image()?;
        captures.push((x, y, width, height, scale, image));
    }

    combine_display_images(captures)
}

fn combine_display_images(
    captures: Vec<(i32, i32, u32, u32, f32, image::RgbaImage)>,
) -> Result<image::RgbaImage, String> {
    if captures.is_empty() {
        return Err("撮影できる画面が見つかりません。".into());
    }

    let min_x = captures.iter().map(|capture| capture.0).min().unwrap_or(0);
    let min_y = captures.iter().map(|capture| capture.1).min().unwrap_or(0);
    let max_x = captures
        .iter()
        .map(|capture| capture.0 + capture.2 as i32)
        .max()
        .unwrap_or(min_x);
    let max_y = captures
        .iter()
        .map(|capture| capture.1 + capture.3 as i32)
        .max()
        .unwrap_or(min_y);
    let scale = captures
        .iter()
        .map(|capture| capture.4)
        .fold(1.0_f32, f32::max);
    let width = ((max_x - min_x) as f32 * scale).ceil() as u32;
    let height = ((max_y - min_y) as f32 * scale).ceil() as u32;
    if width == 0 || height == 0 {
        return Err("画面サイズを確認できません。".into());
    }
    let mut combined = ImageBuffer::from_pixel(width, height, Rgba([30, 30, 30, 255]));
    for (x, y, display_width, display_height, _, image) in captures {
        let offset_x = ((x - min_x) as f32 * scale).round() as i64;
        let offset_y = ((y - min_y) as f32 * scale).round() as i64;
        let target_width = (display_width as f32 * scale).round().max(1.0) as u32;
        let target_height = (display_height as f32 * scale).round().max(1.0) as u32;
        let image = if image.width() == target_width && image.height() == target_height {
            image
        } else {
            image::imageops::resize(
                &image,
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            )
        };
        image::imageops::replace(&mut combined, &image, offset_x, offset_y);
    }
    Ok(combined)
}

#[cfg(target_os = "macos")]
fn raise_window_above_system_bars(title: &str) {
    use objc2_app_kit::{NSApplication, NSScreen, NSWindowLevel};
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    let screen_frame = NSScreen::mainScreen(main_thread).map(|screen| screen.frame());
    let windows = application.windows();
    let target_title = NSString::from_str(title);
    for index in 0..windows.count() {
        let window = unsafe { windows.objectAtIndex(index) };
        let current_title = window.title();
        if unsafe { current_title.isEqualToString(&target_title) } {
            // Use AppKit's full screen frame, which includes the menu bar and Dock.
            // The viewport's requested size can be constrained to the usable area by macOS.
            if let Some(frame) = screen_frame {
                let current = window.frame();
                if (current.origin.x - frame.origin.x).abs() > 0.5
                    || (current.origin.y - frame.origin.y).abs() > 0.5
                    || (current.size.width - frame.size.width).abs() > 0.5
                    || (current.size.height - frame.size.height).abs() > 0.5
                {
                    window.setFrame_display(frame, true);
                }
            }
            // Match fullscreen overlay ordering so the selector covers the menu bar and Dock.
            window.setLevel(unsafe { CGShieldingWindowLevel() } as NSWindowLevel + 1);
            return;
        }
    }
}

#[cfg(target_os = "macos")]
fn set_standard_quit_items_enabled(enabled: bool) {
    use objc2::{runtime::Sel, sel};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    let Some(main_menu) = (unsafe { application.mainMenu() }) else {
        return;
    };
    let terminate_action: Sel = sel!(terminate:);
    unsafe {
        for menu_item in main_menu.itemArray().iter() {
            let Some(submenu) = menu_item.submenu() else {
                continue;
            };
            for action_item in submenu.itemArray().iter() {
                if action_item.action() == Some(terminate_action) {
                    let item_key = std::ptr::from_ref(&**action_item) as usize;
                    QUIT_MENU_KEY_EQUIVALENTS.with(|saved| {
                        let mut saved = saved.borrow_mut();
                        if enabled {
                            if let Some((key_equivalent, modifiers)) = saved.get(&item_key) {
                                action_item.setKeyEquivalent(key_equivalent);
                                action_item.setKeyEquivalentModifierMask(*modifiers);
                            }
                            action_item.setEnabled(true);
                        } else {
                            saved.entry(item_key).or_insert_with(|| {
                                (
                                    action_item.keyEquivalent(),
                                    action_item.keyEquivalentModifierMask(),
                                )
                            });
                            action_item.setEnabled(false);
                            action_item.setKeyEquivalent(&NSString::new());
                        }
                    });
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
thread_local! {
    static QUIT_MENU_KEY_EQUIVALENTS: std::cell::RefCell<std::collections::HashMap<
        usize,
        (objc2::rc::Retained<objc2_foundation::NSString>, objc2_app_kit::NSEventModifierFlags),
    >> = std::cell::RefCell::new(std::collections::HashMap::new());
}

fn crop_normalized(image: &image::RgbaImage, bounds: [f32; 4]) -> Option<image::RgbaImage> {
    let width = image.width();
    let height = image.height();
    let left = bounds[0].min(bounds[2]).clamp(0.0, 1.0);
    let top = bounds[1].min(bounds[3]).clamp(0.0, 1.0);
    let right = bounds[0].max(bounds[2]).clamp(0.0, 1.0);
    let bottom = bounds[1].max(bounds[3]).clamp(0.0, 1.0);
    let x = (left * width as f32).floor() as u32;
    let y = (top * height as f32).floor() as u32;
    let right = (right * width as f32).ceil() as u32;
    let bottom = (bottom * height as f32).ceil() as u32;
    let crop_width = right.saturating_sub(x).min(width.saturating_sub(x));
    let crop_height = bottom.saturating_sub(y).min(height.saturating_sub(y));
    if crop_width < 2 || crop_height < 2 {
        return None;
    }
    Some(image::imageops::crop_imm(image, x, y, crop_width, crop_height).to_image())
}

fn normalized_region_bounds(
    screen: egui::Rect,
    start: egui::Pos2,
    end: egui::Pos2,
) -> Option<[f32; 4]> {
    if screen.width() <= 0.0 || screen.height() <= 0.0 {
        return None;
    }
    let selection = egui::Rect::from_two_pos(start, end).intersect(screen);
    if selection.width() < 4.0 || selection.height() < 4.0 {
        return None;
    }
    // Include the outer selection outline and a small safety margin in the
    // saved crop. Without this, pixels drawn on the visible border can appear
    // cut off even though they looked inside the highlighted frame.
    let selection = selection.expand(3.0).intersect(screen);
    Some([
        (selection.left() - screen.left()) / screen.width(),
        (selection.top() - screen.top()) / screen.height(),
        (selection.right() - screen.left()) / screen.width(),
        (selection.bottom() - screen.top()) / screen.height(),
    ])
}

fn capture_size_label(width: u32, height: u32) -> String {
    format!("画像サイズ: {width} × {height} px")
}

fn capture_saved_status(sequence: u64, timestamp: &str, width: u32, height: u32) -> String {
    format!("撮影 {sequence} 枚目を保存しました ({timestamp}) · {width}×{height} px")
}

fn stamp_timestamp(image: &mut image::RgbaImage, timestamp: &str) {
    let font = FontRef::try_from_slice(include_bytes!("../assets/NotoSansJP-Medium.ttf"))
        .expect("bundled timestamp font must be valid");
    let scale = PxScale::from((image.width().min(image.height()) as f32 / 45.0).clamp(18.0, 34.0));
    let (text_width, text_height) = text_size(scale, &font, timestamp);
    let padding = (scale.y * 0.45).round() as i32;
    let left = (image.width() as i32 - text_width as i32 - padding * 3).max(padding);
    let top = (image.height() as i32 - text_height as i32 - padding * 3).max(padding);
    draw_filled_rect_mut(
        image,
        ImageRect::at(left - padding, top - padding).of_size(
            text_width + (padding * 2) as u32,
            text_height + (padding * 2) as u32,
        ),
        Rgba([0, 0, 0, 190]),
    );
    draw_text_mut(
        image,
        Rgba([255, 255, 255, 255]),
        left,
        top,
        scale,
        &font,
        timestamp,
    );
}

fn annotation_rgb(color: &str) -> [u8; 3] {
    match color {
        "yellow" => [255, 197, 0],
        "blue" => [30, 130, 255],
        _ => [255, 38, 38],
    }
}

fn draw_annotation(
    image: &mut image::RgbaImage,
    annotation: Annotation,
    color: [u8; 3],
    annotation_width: f32,
) {
    let color = Rgba([color[0], color[1], color[2], 255]);
    let width = image.width() as f32;
    let height = image.height() as f32;
    let (x1, y1) = (annotation.start.x * width, annotation.start.y * height);
    let (x2, y2) = (annotation.end.x * width, annotation.end.y * height);
    match annotation.tool {
        EditTool::Rectangle => {
            let left = x1.min(x2).round() as i32;
            let top = y1.min(y2).round() as i32;
            let box_width = (x1 - x2).abs().round().max(1.0) as u32;
            let box_height = (y1 - y2).abs().round().max(1.0) as u32;
            let thickness = ((image.width().min(image.height()) as f32 / 500.0) * annotation_width)
                .round()
                .clamp(1.0, 20.0) as u32;
            for inset in 0..thickness {
                let inset = inset as i32;
                let w = box_width.saturating_sub((inset * 2) as u32);
                let h = box_height.saturating_sub((inset * 2) as u32);
                if w > 1 && h > 1 {
                    draw_hollow_rect_mut(
                        image,
                        ImageRect::at(left + inset, top + inset).of_size(w, h),
                        color,
                    );
                }
            }
        }
        EditTool::Arrow => {
            draw_line_segment_mut(image, (x1, y1), (x2, y2), color);
            let angle = (y2 - y1).atan2(x2 - x1);
            let head = ((image.width().min(image.height()) as f32 / 45.0)
                * (annotation_width / 3.0))
                .clamp(8.0, 50.0);
            for side in [-0.55_f32, 0.55_f32] {
                let head_angle = angle + std::f32::consts::PI + side;
                draw_line_segment_mut(
                    image,
                    (x2, y2),
                    (x2 + head * head_angle.cos(), y2 + head * head_angle.sin()),
                    color,
                );
            }
        }
        EditTool::Crop => {}
    }
}

fn annotated_image(
    image: &DynamicImage,
    annotations: &[Annotation],
    color: [u8; 3],
    annotation_width: f32,
) -> image::RgbaImage {
    let mut output = image.to_rgba8();
    for annotation in annotations {
        draw_annotation(&mut output, *annotation, color, annotation_width);
    }
    output
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(82, 99, 211);
const ACCENT_BRIGHT: egui::Color32 = egui::Color32::from_rgb(78, 91, 192);
const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(76, 86, 107);

fn application_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../assets/MacShot-Icon.png"))
        .expect("bundled application icon must be valid")
        .to_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn configure_macshot_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::light();
    let foreground = egui::Color32::from_rgb(29, 36, 52);
    visuals.panel_fill = egui::Color32::from_rgb(238, 242, 250);
    visuals.window_fill = egui::Color32::from_rgb(250, 251, 255);
    visuals.extreme_bg_color = egui::Color32::from_rgb(248, 249, 253);
    visuals.faint_bg_color = egui::Color32::from_rgba_unmultiplied(82, 99, 211, 16);
    visuals.override_text_color = Some(foreground);
    visuals.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(82, 99, 211, 68);
    visuals.hyperlink_color = ACCENT_BRIGHT;
    visuals.weak_text_color = Some(TEXT_MUTED);
    visuals.window_corner_radius = egui::CornerRadius::same(22);
    visuals.window_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(34));
    visuals.widgets.noninteractive.bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 150);
    visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(24));
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(12);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 180);
    visuals.widgets.inactive.weak_bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 100);
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(25));
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(12);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgba_unmultiplied(110, 126, 232, 40);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(
        1.0_f32,
        egui::Color32::from_rgba_unmultiplied(82, 99, 211, 105),
    );
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(12);
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT_BRIGHT);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(12);
    visuals.widgets.open.bg_fill = egui::Color32::from_rgba_unmultiplied(110, 126, 232, 44);
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(12);
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(15.0, 9.0);
    style.spacing.interact_size = egui::vec2(34.0, 34.0);
    ctx.set_style(style);
}

fn glass_frame() -> egui::Frame {
    glass_frame_with_margin(16)
}

fn settings_card_frame(padding: SettingsCardPadding) -> egui::Frame {
    glass_frame_with_margin(padding.margin())
}

fn glass_frame_with_margin(margin: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 170))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 200),
        ))
        .corner_radius(egui::CornerRadius::same(20))
        .inner_margin(egui::Margin::same(margin))
}

fn primary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(label.into().color(egui::Color32::WHITE))
        .fill(ACCENT)
        .stroke(egui::Stroke::new(1.0_f32, ACCENT_BRIGHT))
        .corner_radius(egui::CornerRadius::same(13))
        .min_size(egui::vec2(112.0, 42.0))
}

fn paint_glass_backdrop(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(238, 242, 250));
    let glow = rect.right_top() + egui::vec2(-100.0, 130.0);
    for (radius, alpha) in [(610.0, 4), (460.0, 5), (310.0, 6), (190.0, 7)] {
        painter.circle_filled(
            glow,
            radius,
            egui::Color32::from_rgba_unmultiplied(117, 131, 235, alpha * 2),
        );
    }
    let lower_glow = rect.left_bottom() + egui::vec2(140.0, -35.0);
    for (radius, alpha) in [(450.0, 2), (320.0, 3), (210.0, 4)] {
        painter.circle_filled(
            lower_glow,
            radius,
            egui::Color32::from_rgba_unmultiplied(68, 161, 197, alpha * 3),
        );
    }
}

fn hotkey_badge_text(value: &str) -> String {
    canonical_hotkey_text(value)
        .split('+')
        .map(|part| match part.trim().to_ascii_lowercase().as_str() {
            "alt" => if cfg!(target_os = "macos") {
                "Option"
            } else {
                "Alt"
            }
            .to_string(),
            "shift" => "⇧".to_string(),
            "control" | "ctrl" => if cfg!(target_os = "macos") {
                "⌃"
            } else {
                "Ctrl"
            }
            .to_string(),
            "super" => if cfg!(target_os = "macos") {
                "⌘"
            } else {
                "⊞"
            }
            .to_string(),
            key if key.starts_with("key") => part.trim()[3..].to_string(),
            key => key.to_string(),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn canonical_hotkey_text(value: &str) -> String {
    let Ok(hotkey) = value.parse::<HotKey>() else {
        return value.to_string();
    };

    let mut modifiers = Vec::new();
    let mut keys = Vec::new();
    for part in hotkey.to_string().split('+') {
        let normalized = match part.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => Some((0, "Control")),
            "alt" => Some((1, "Alt")),
            "super" => Some((2, "Super")),
            "shift" => Some((3, "Shift")),
            _ => None,
        };
        if let Some((order, label)) = normalized {
            modifiers.push((order, label));
        } else if part.to_ascii_lowercase().starts_with("key") {
            keys.push(format!("Key{}", &part[3..]));
        } else {
            keys.push(part.to_string());
        }
    }
    modifiers.sort_by_key(|(order, _)| *order);
    modifiers
        .into_iter()
        .map(|(_, label)| label.to_string())
        .chain(keys)
        .collect::<Vec<_>>()
        .join("+")
}

fn parse_hotkey_pair(capture_text: &str, recording_text: &str) -> Result<(HotKey, HotKey), String> {
    let capture = match capture_text.parse::<HotKey>() {
        Ok(hotkey) if !hotkey.mods.is_empty() => hotkey,
        _ => {
            return Err(
                "撮影キーを確認してください（例: Alt+Shift+KeyS）。修飾キーが1つ以上必要です。"
                    .into(),
            );
        }
    };
    let recording = match recording_text.parse::<HotKey>() {
        Ok(hotkey) if !hotkey.mods.is_empty() => hotkey,
        _ => {
            return Err(
                "録画キーを確認してください（例: Alt+Shift+KeyR）。修飾キーが1つ以上必要です。"
                    .into(),
            );
        }
    };
    if capture == recording {
        return Err("撮影と録画に同じショートカットは使えません。".into());
    }
    Ok((capture, recording))
}

fn monitor_for_recording(target: CaptureTarget) -> Result<Monitor, String> {
    let monitors = Monitor::all().map_err(|error| error.to_string())?;
    match target {
        CaptureTarget::Display(index) => monitors
            .get(index)
            .cloned()
            .ok_or_else(|| "録画する画面が見つかりません。".to_string()),
        CaptureTarget::Window(id) => {
            #[cfg(target_os = "macos")]
            {
                monitor_for_screen_capture_window(id)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Window::all()
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .find(|window| window.id().ok() == Some(id))
                    .ok_or_else(|| "録画するウィンドウが見つかりません。".to_string())?
                    .current_monitor()
                    .map_err(|error| error.to_string())
            }
        }
        CaptureTarget::ActiveWindow => {
            #[cfg(target_os = "macos")]
            {
                monitor_for_screen_capture_window(find_active_screen_capture_window_id()?)
            }
            #[cfg(not(target_os = "macos"))]
            {
                find_active_window()
                    .map_err(|_| "録画するアクティブウィンドウが見つかりません。".to_string())?
                    .current_monitor()
                    .map_err(|error| error.to_string())
            }
        }
        CaptureTarget::PrimaryDisplay | CaptureTarget::AllDisplays => monitors
            .iter()
            .find(|monitor| monitor.is_primary().unwrap_or(false))
            .or_else(|| monitors.first())
            .cloned()
            .ok_or_else(|| "録画できる画面が見つかりません。".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_screen(
    target: CaptureTarget,
    folder: PathBuf,
    file_timestamp: String,
    sequence: u64,
    format: RecordingFormat,
    quality: RecordingQuality,
    stop_rx: Receiver<()>,
    event_tx: Sender<RecordingEvent>,
    ctx: egui::Context,
) {
    let result = match format {
        RecordingFormat::Gif => record_screen_to_gif(
            target,
            folder,
            file_timestamp,
            sequence,
            quality,
            stop_rx,
            event_tx.clone(),
            ctx.clone(),
        ),
        RecordingFormat::Mp4 => record_screen_to_mp4(
            target,
            folder,
            file_timestamp,
            sequence,
            quality,
            stop_rx,
            event_tx.clone(),
            ctx.clone(),
        ),
    };
    let _ = event_tx.send(RecordingEvent::Finished(result));
    ctx.request_repaint();
}

fn record_screen_to_gif(
    target: CaptureTarget,
    folder: PathBuf,
    file_timestamp: String,
    sequence: u64,
    quality: RecordingQuality,
    stop_rx: Receiver<()>,
    event_tx: Sender<RecordingEvent>,
    ctx: egui::Context,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let monitor = monitor_for_recording(target)?;
    let display_capture = DisplayCapture::new(&monitor)?;
    let first = quality
        .resolution
        .resize(display_capture.capture_image()?, false);
    if first.width() > u16::MAX as u32 || first.height() > u16::MAX as u32 {
        return Err("画面サイズがGIFの上限を超えています。".into());
    }
    let width = first.width() as u16;
    let height = first.height() as u16;
    let preferred_path =
        new_recording_path_for_format(&folder, &file_timestamp, sequence, RecordingFormat::Gif);
    let (path, file) = create_unique_file(&folder, &preferred_path)?;
    let mut partial_file = PartialRecordingFile::new(path.clone());
    let _ = event_tx.send(RecordingEvent::Started {
        path: path.clone(),
        microphone_warning: None,
    });

    let mut encoder =
        gif::Encoder::new(file, width, height, &[]).map_err(|error| error.to_string())?;
    encoder
        .set_repeat(gif::Repeat::Infinite)
        .map_err(|error| error.to_string())?;

    let mut first_pixels = first.into_raw();
    let frame_interval = quality.gif_frame_interval();
    let frame_delay = (100.0 / quality.gif_frame_rate as f32).round().max(1.0) as u16;
    write_gif_frame_with_speed(
        &mut encoder,
        width,
        height,
        &mut first_pixels,
        frame_delay,
        quality.gif_quantizer_speed(),
    )
    .map_err(|error| error.to_string())?;
    let mut last_written = Instant::now();
    let mut last_status = Instant::now();

    loop {
        match stop_rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if last_status.elapsed() > Duration::from_millis(250) {
            ctx.request_repaint();
            last_status = Instant::now();
        }
        match stop_rx.recv_timeout(frame_interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let frame = quality
            .resolution
            .resize(display_capture.capture_image()?, false);
        if frame.width() != width as u32
            || frame.height() != height as u32
            || last_written.elapsed() < frame_interval
        {
            continue;
        }
        let mut pixels = frame.into_raw();
        write_gif_frame_with_speed(
            &mut encoder,
            width,
            height,
            &mut pixels,
            gif_delay_centiseconds(last_written.elapsed()),
            quality.gif_quantizer_speed(),
        )
        .map_err(|error| error.to_string())?;
        last_written = Instant::now();
    }
    encoder
        .into_inner()
        .map_err(|error| error.to_string())?
        .sync_all()
        .map_err(|error| error.to_string())?;
    partial_file.keep();
    Ok(path)
}

struct H264Packet {
    sample: Vec<u8>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    is_sync: bool,
}

type H264Priming = (Vec<u8>, Vec<u8>, Vec<H264Packet>);

fn encode_h264_frame(
    encoder: &mut H264Encoder,
    frame: &image::RgbaImage,
) -> Result<H264Packet, String> {
    let rgb = RgbaSliceU8::new(
        frame.as_raw(),
        (frame.width() as usize, frame.height() as usize),
    );
    let yuv = YUVBuffer::from_rgb_source(rgb);
    let encoded = encoder.encode(&yuv).map_err(|error| error.to_string())?;
    let mut sample = Vec::new();
    let mut sps = None;
    let mut pps = None;
    for layer_index in 0..encoded.num_layers() {
        let layer = encoded
            .layer(layer_index)
            .ok_or_else(|| "H.264フレームを読み取れません。".to_string())?;
        for nal_index in 0..layer.nal_count() {
            let nal = layer
                .nal_unit(nal_index)
                .ok_or_else(|| "H.264データが不完全です。".to_string())?;
            let nal = nal
                .strip_prefix(&[0, 0, 0, 1])
                .or_else(|| nal.strip_prefix(&[0, 0, 1]))
                .unwrap_or(nal);
            let Some(&header) = nal.first() else {
                continue;
            };
            match header & 0x1f {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                _ => {
                    let size = u32::try_from(nal.len())
                        .map_err(|_| "H.264フレームが大きすぎます。".to_string())?;
                    sample.extend_from_slice(&size.to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
    }
    Ok(H264Packet {
        sample,
        sps,
        pps,
        is_sync: encoded.frame_type() == FrameType::IDR,
    })
}

fn prime_h264_encoder(
    encoder: &mut H264Encoder,
    frame: &image::RgbaImage,
) -> Result<H264Priming, String> {
    let mut sps = None;
    let mut pps = None;
    let mut packets = Vec::new();
    for _ in 0..5 {
        let mut packet = encode_h264_frame(encoder, frame)?;
        sps = sps.or(packet.sps.take());
        pps = pps.or(packet.pps.take());
        packets.push(packet);
        if packets.iter().any(|packet| !packet.sample.is_empty())
            && let (Some(sps), Some(pps)) = (sps.take(), pps.take())
        {
            return Ok((sps, pps, packets));
        }
    }
    Err("H.264の初期フレームを作成できません。".into())
}

#[allow(clippy::too_many_arguments)]
fn record_screen_to_mp4(
    target: CaptureTarget,
    folder: PathBuf,
    file_timestamp: String,
    sequence: u64,
    quality: RecordingQuality,
    stop_rx: Receiver<()>,
    event_tx: Sender<RecordingEvent>,
    ctx: egui::Context,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let monitor = monitor_for_recording(target)?;
    let display_capture = DisplayCapture::new(&monitor)?;
    let first = quality
        .resolution
        .resize(display_capture.capture_image()?, true);
    let width = first.width() & !1;
    let height = first.height() & !1;
    if width < 2 || height < 2 || width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err("MP4録画には2〜65535ピクセルの画面サイズが必要です。".into());
    }
    let first = if first.width() != width || first.height() != height {
        image::imageops::crop_imm(&first, 0, 0, width, height).to_image()
    } else {
        first
    };
    let mut aac_encoder = recording_audio::create_aac_encoder()?;
    let preferred_path =
        new_recording_path_for_format(&folder, &file_timestamp, sequence, RecordingFormat::Mp4);
    let (path, file) = create_unique_file(&folder, &preferred_path)?;
    let mut partial_file = PartialRecordingFile::new(path.clone());
    let mut h264 = H264Encoder::with_api_config(
        OpenH264API::from_source(),
        EncoderConfig::new()
            .bitrate(BitRate::from_bps(quality.mp4_bitrate()))
            .max_frame_rate(FrameRate::from_hz(quality.mp4_frame_rate as f32))
            .usage_type(UsageType::ScreenContentRealTime),
    )
    .map_err(|error| error.to_string())?;
    let (sps, pps, initial_packets) = prime_h264_encoder(&mut h264, &first)?;

    // Starting ScreenCaptureKit/CoreAudio before H.264 priming includes encoder startup
    // time in the audio track even though the first video sample starts at zero.
    #[cfg(target_os = "macos")]
    let include_microphone = should_capture_microphone(microphone_permission());
    #[cfg(target_os = "windows")]
    let include_microphone = true;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let include_microphone = false;
    let recording_started_at = Instant::now();
    let mut audio_capture = recording_audio::AudioCaptureSession::start(include_microphone)?;
    let microphone_warning = audio_capture.microphone_warning().map(str::to_owned);
    let mut audio_mixer = recording_audio::AudioMixer::new();

    let fourcc = |value: &str| value.parse::<FourCC>().map_err(|error| error.to_string());
    let config = Mp4Config {
        major_brand: fourcc("isom")?,
        minor_version: 512,
        compatible_brands: vec![
            fourcc("isom")?,
            fourcc("iso2")?,
            fourcc("avc1")?,
            fourcc("mp41")?,
        ],
        timescale: 1000,
    };
    let mut writer = Mp4Writer::write_start(file, &config).map_err(|error| error.to_string())?;
    writer
        .add_track(&TrackConfig::from(AvcConfig {
            width: width as u16,
            height: height as u16,
            seq_param_set: sps,
            pic_param_set: pps,
        }))
        .map_err(|error| error.to_string())?;
    let mut audio_track = TrackConfig::from(AacConfig {
        bitrate: recording_audio::bitrate(),
        profile: AudioObjectType::AacLowComplexity,
        freq_index: SampleFreqIndex::Freq48000,
        chan_conf: ChannelConfig::Stereo,
    });
    audio_track.timescale = recording_audio::sample_rate();
    writer
        .add_track(&audio_track)
        .map_err(|error| error.to_string())?;
    let _ = event_tx.send(RecordingEvent::Started {
        path: path.clone(),
        microphone_warning: microphone_warning.clone(),
    });
    let mut frame_index = 0u64;
    let mut last_video_start_ms = 0u64;
    let mut pending_video_sample: Option<(u64, H264Packet)> = None;
    let frame_interval = quality.mp4_frame_interval();
    let frame_duration_ms = quality.mp4_frame_duration_ms();
    for packet in initial_packets {
        if !packet.sample.is_empty() {
            let start_time_ms = frame_index * frame_duration_ms;
            write_mp4_sample_with_duration(&mut writer, packet, frame_duration_ms)?;
            last_video_start_ms = start_time_ms;
            frame_index += 1;
        }
    }
    let mut last_written = Instant::now();
    let mut last_status = Instant::now();
    let mut last_microphone_warning = microphone_warning.clone();
    loop {
        match stop_rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        audio_capture.drain_into(&mut audio_mixer)?;
        if let Some(warning) = audio_capture.microphone_warning()
            && last_microphone_warning.as_deref() != Some(warning)
        {
            let _ = event_tx.send(RecordingEvent::MicrophoneWarning(warning.to_owned()));
            last_microphone_warning = Some(warning.to_owned());
        }
        let max_audio_elapsed_ns = recording_started_at
            .elapsed()
            .as_nanos()
            .min(i64::MAX as u128) as i64;
        for (start, samples) in audio_mixer.take_ready_through(max_audio_elapsed_ns) {
            let encoded = recording_audio::encode_block(&mut *aac_encoder, start, samples)?;
            write_mp4_audio_samples(&mut writer, encoded)?;
        }
        if last_status.elapsed() > Duration::from_millis(250) {
            ctx.request_repaint();
            last_status = Instant::now();
        }
        match stop_rx.recv_timeout(frame_interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let frame = quality
            .resolution
            .resize(display_capture.capture_image()?, true);
        if frame.width() != width || frame.height() != height {
            continue;
        }
        if last_written.elapsed() < frame_interval {
            continue;
        }
        let packet = encode_h264_frame(&mut h264, &frame)?;
        if !packet.sample.is_empty() {
            let elapsed_ms = recording_started_at
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64;
            let start_time_ms =
                next_video_start_ms(elapsed_ms, last_video_start_ms, frame_duration_ms);
            if let Some((previous_start_ms, previous_packet)) = pending_video_sample.take() {
                let duration_ms = start_time_ms
                    .saturating_sub(previous_start_ms)
                    .max(frame_duration_ms);
                write_mp4_sample_with_duration(&mut writer, previous_packet, duration_ms)?;
            }
            pending_video_sample = Some((start_time_ms, packet));
            last_video_start_ms = start_time_ms;
            frame_index += 1;
        }
        last_written = Instant::now();
    }
    if frame_index < 2 {
        return Err("MP4動画を作るには、1秒以上録画してください。".into());
    }
    if let Some((last_start_ms, packet)) = pending_video_sample {
        let elapsed_ms = recording_started_at
            .elapsed()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let duration_ms = elapsed_ms
            .saturating_sub(last_start_ms)
            .max(frame_duration_ms);
        write_mp4_sample_with_duration(&mut writer, packet, duration_ms)?;
    }
    let max_audio_elapsed_ns = recording_started_at
        .elapsed()
        .as_nanos()
        .min(i64::MAX as u128) as i64;
    for (start, samples) in audio_mixer.finish(&mut audio_capture, max_audio_elapsed_ns)? {
        let encoded = recording_audio::encode_block(&mut *aac_encoder, start, samples)?;
        write_mp4_audio_samples(&mut writer, encoded)?;
    }
    if let Some(warning) = audio_capture.microphone_warning()
        && last_microphone_warning.as_deref() != Some(warning)
    {
        let _ = event_tx.send(RecordingEvent::MicrophoneWarning(warning.to_owned()));
    }
    let encoded = recording_audio::finish_encoder(&mut *aac_encoder)?;
    write_mp4_audio_samples(&mut writer, encoded)?;
    writer.write_end().map_err(|error| error.to_string())?;
    writer
        .into_writer()
        .sync_all()
        .map_err(|error| error.to_string())?;
    partial_file.keep();
    Ok(path)
}

struct PartialRecordingFile {
    path: PathBuf,
    keep: bool,
}

impl PartialRecordingFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for PartialRecordingFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_mp4_sample_with_duration<W: std::io::Write + std::io::Seek>(
    writer: &mut Mp4Writer<W>,
    packet: H264Packet,
    frame_duration_ms: u64,
) -> Result<(), String> {
    if packet.sample.is_empty() {
        return Ok(());
    }
    let duration = u32::try_from(frame_duration_ms)
        .map_err(|_| "動画フレームの長さが不正です。".to_string())?;
    writer
        .write_sample(
            1,
            &Mp4Sample {
                start_time: 0,
                duration,
                rendering_offset: 0,
                is_sync: packet.is_sync,
                bytes: Bytes::from(packet.sample),
            },
        )
        .map_err(|error| error.to_string())
}

fn write_mp4_audio_samples<W: std::io::Write + std::io::Seek>(
    writer: &mut Mp4Writer<W>,
    samples: Vec<zvidlib_core::EncodedSample>,
) -> Result<(), String> {
    for sample in samples {
        let start_time = u64::try_from(sample.pts.max(0))
            .map_err(|_| "音声の時間情報が不正です。".to_string())?;
        writer
            .write_sample(
                2,
                &Mp4Sample {
                    start_time,
                    duration: sample.duration,
                    rendering_offset: 0,
                    is_sync: sample.is_sync,
                    bytes: Bytes::from(sample.data),
                },
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn next_video_start_ms(elapsed_ms: u64, previous_start_ms: u64, frame_duration_ms: u64) -> u64 {
    elapsed_ms.max(previous_start_ms.saturating_add(frame_duration_ms))
}

#[cfg(test)]
fn write_gif_frame<W: Write>(
    encoder: &mut gif::Encoder<W>,
    width: u16,
    height: u16,
    rgba: &mut [u8],
    delay: u16,
) -> Result<(), gif::EncodingError> {
    write_gif_frame_with_speed(encoder, width, height, rgba, delay, 10)
}

fn write_gif_frame_with_speed<W: Write>(
    encoder: &mut gif::Encoder<W>,
    width: u16,
    height: u16,
    rgba: &mut [u8],
    delay: u16,
    speed: i32,
) -> Result<(), gif::EncodingError> {
    let mut frame = gif::Frame::from_rgba_speed(width, height, rgba, speed.clamp(1, 30));
    frame.delay = delay;
    encoder.write_frame(&frame)
}

impl MacShot {
    fn initialize_hotkey(&mut self, ctx: &egui::Context) {
        if self.hotkey_initialized {
            return;
        }
        self.hotkey_initialized = true;
        let manager = match GlobalHotKeyManager::new() {
            Ok(manager) => Some(manager),
            Err(error) => {
                self.status =
                    format!("ショートカットを初期化できません。ボタンから操作できます: {error}");
                None
            }
        };
        let mut registration_errors = Vec::new();
        if let Some(manager) = manager.as_ref() {
            for (name, hotkey) in [
                ("撮影", self.capture_hotkey),
                ("録画", self.recording_hotkey),
            ] {
                match manager.register(hotkey) {
                    Ok(()) => self.registered_hotkeys.push(hotkey),
                    Err(error) => registration_errors.push(format!("{name}: {error}")),
                }
            }
        }
        self._hotkey_manager = manager;
        if !registration_errors.is_empty() {
            self.status = format!(
                "一部のショートカットを登録できません（{}）。設定画面で別のキーを選べます。ボタンからの操作は使えます。",
                registration_errors.join("、")
            );
        }
        let events = GlobalHotKeyEvent::receiver().clone();
        let sender = self.hotkey_tx.clone();
        let recording_stop_tx = Arc::clone(&self.recording_stop_tx);
        let recording_hotkey_id = self.recording_hotkey.id();
        let repaint = ctx.clone();
        thread::spawn(move || {
            while let Ok(event) = events.recv() {
                if event.state == HotKeyState::Pressed {
                    if event.id == recording_hotkey_id {
                        let stop = recording_stop_tx.lock().ok().and_then(|stop| stop.clone());
                        if let Some(stop) = stop {
                            let _ = stop.send(());
                            continue;
                        }
                    }
                    let _ = sender.send(event);
                    repaint.request_repaint();
                }
            }
        });
    }

    fn handle_hotkey_requests(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.hotkey_rx.try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            if event.id == self.capture_hotkey.id() {
                if self.recording || self.recording_starting {
                    self.status = "録画を終了してからスクリーンショットを撮影してください。".into();
                } else {
                    self.begin_capture(ctx);
                }
            } else if event.id == self.recording_hotkey.id() {
                self.toggle_recording(ctx);
            }
        }
    }

    fn initialize_tray(&mut self, ctx: &egui::Context) {
        if self.tray_initialized {
            return;
        }
        self.tray_initialized = true;
        let show_window_item =
            MenuItem::with_id("macshot.window.show", "MacShotを開く", true, None);
        let capture_area_item =
            MenuItem::with_id("macshot.capture.area", "範囲を選択して撮影…", true, None);
        let capture_active_item = MenuItem::with_id(
            "macshot.capture.active",
            "アクティブウィンドウを撮影",
            true,
            None,
        );
        let capture_all_item = MenuItem::with_id("macshot.capture.all", "全画面を撮影", true, None);
        let capture_selected_item = MenuItem::with_id(
            "macshot.capture.selected",
            "選択中の撮影対象で撮影",
            true,
            None,
        );
        let continuous_capture_item = MenuItem::with_id(
            "macshot.capture.continuous",
            "連続撮影モードを開始／終了",
            true,
            None,
        );
        let native_screenshot_item = MenuItem::with_id(
            "macshot.capture.native",
            "macOS標準の撮影・録画を開く…",
            cfg!(target_os = "macos"),
            None,
        );
        let capture_submenu = Submenu::new("スクリーンショット", true);
        let capture_status_item =
            MenuItem::with_id("macshot.capture.status", "連続撮影 — 停止中", false, None);
        let recording_status_item =
            MenuItem::with_id("macshot.recording.status", "録画時間 00:00", false, None);
        let recording_item = MenuItem::with_id("macshot.record", "録画開始／停止", true, None);
        let quit_item = MenuItem::with_id("macshot.quit", "終了", true, None);
        if let Err(error) = capture_submenu.append_items(&[
            &capture_area_item,
            &capture_active_item,
            &capture_all_item,
            &capture_selected_item,
            &continuous_capture_item,
            &native_screenshot_item,
        ]) {
            self.status = format!("撮影メニューを作成できません: {error}");
            return;
        }
        let menu = match Menu::with_items(&[
            &show_window_item,
            &capture_submenu,
            &capture_status_item,
            &recording_status_item,
            &recording_item,
            &quit_item,
        ]) {
            Ok(menu) => menu,
            Err(error) => {
                self.status = format!("メニューバーのメニューを作成できません: {error}");
                return;
            }
        };
        let icon = make_tray_icon(false);
        let sender = self.tray_action_tx.clone();
        let recording_stop_tx = Arc::clone(&self.recording_stop_tx);
        let repaint = ctx.clone();
        let show_window_id = show_window_item.id().0.clone();
        let capture_area_id = capture_area_item.id().0.clone();
        let capture_active_id = capture_active_item.id().0.clone();
        let capture_all_id = capture_all_item.id().0.clone();
        let capture_selected_id = capture_selected_item.id().0.clone();
        let native_screenshot_id = native_screenshot_item.id().0.clone();
        let continuous_capture_id = continuous_capture_item.id().0.clone();
        let record_id = recording_item.id().0.clone();
        let quit_id = quit_item.id().0.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id.0 == record_id {
                dispatch_recording_menu_action(&recording_stop_tx, &sender);
                repaint.request_repaint();
                return;
            }
            let action = match event.id.0.as_str() {
                id if id == show_window_id => Some(TrayAction::ShowWindow),
                id if id == capture_area_id => Some(TrayAction::CaptureRegion),
                id if id == capture_active_id => Some(TrayAction::CaptureActiveWindow),
                id if id == capture_all_id => Some(TrayAction::CaptureAllDisplays),
                id if id == capture_selected_id => Some(TrayAction::Capture),
                id if id == native_screenshot_id => Some(TrayAction::OpenNativeScreenshot),
                id if id == continuous_capture_id => Some(TrayAction::ToggleContinuousCapture),
                id if id == record_id => Some(TrayAction::ToggleRecording),
                id if id == quit_id => Some(TrayAction::Quit),
                _ => None,
            };
            if let Some(action) = action {
                let _ = sender.send(action);
                repaint.request_repaint();
            }
        }));
        let builder = TrayIconBuilder::new().with_menu(Box::new(menu));
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_tooltip("MacShot — エビデンス撮影");
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon);
        match builder.build() {
            Ok(tray) => {
                self.tray_capture_status_item = Some(capture_status_item);
                self.tray_recording_status_item = Some(recording_status_item);
                self._tray_icon = Some(tray);
                self.update_tray_recording_status();
            }
            Err(error) => self.status = format!("メニューバーを表示できません: {error}"),
        }
    }

    fn update_tray_recording_status(&mut self) {
        let active = self.recording || self.recording_starting;
        let seconds = if self.recording {
            self.recording_started_at
                .map(|at| at.elapsed().as_secs())
                .unwrap_or(0)
        } else {
            0
        };
        let capture_status_state = (
            self.continuous_capture,
            self.continuous_capture_session_active,
            self.capture_in_progress,
            self.continuous_capture_saved_count,
            self.continuous_capture_failed_count,
        );
        if self.tray_recording_state == Some(active)
            && self.tray_recording_seconds == Some(seconds)
            && self.tray_capture_status_state == Some(capture_status_state)
        {
            return;
        }
        self.tray_recording_state = Some(active);
        self.tray_recording_seconds = Some(seconds);
        self.tray_capture_status_state = Some(capture_status_state);
        let label = if active {
            format!(
                "録画中 {}",
                recording_time_label(Duration::from_secs(seconds))
            )
        } else {
            "録画時間 00:00".to_owned()
        };
        if let Some(item) = &self.tray_recording_status_item {
            item.set_text(label);
        }
        if let Some(item) = &self.tray_capture_status_item {
            item.set_text(continuous_capture_status_label(
                self.continuous_capture,
                self.continuous_capture_session_active,
                self.capture_in_progress,
                self.continuous_capture_saved_count,
                self.continuous_capture_failed_count,
            ));
        }
        if let Some(tray) = &self._tray_icon {
            let icon = make_tray_icon(active);
            #[cfg(target_os = "macos")]
            let result = tray.set_icon_templated(if self.continuous_capture {
                None
            } else {
                Some(icon)
            });
            #[cfg(not(target_os = "macos"))]
            let result = tray.set_icon(Some(icon));
            if let Err(error) = result {
                self.status = format!("メニューバーアイコンを更新できません: {error}");
            }
            #[cfg(target_os = "macos")]
            tray.set_title(if active {
                Some(format!(
                    "● {}",
                    recording_time_label(Duration::from_secs(seconds))
                ))
            } else {
                continuous_capture_title(
                    self.continuous_capture,
                    self.capture_in_progress,
                    self.continuous_capture_saved_count,
                    self.continuous_capture_failed_count,
                )
            });
        }
    }

    fn handle_tray_actions(&mut self, ctx: &egui::Context) {
        while let Ok(action) = self.tray_action_rx.try_recv() {
            match action {
                TrayAction::ShowWindow => {
                    self.status = "MacShotのウィンドウを表示しました。".into();
                    restore_main_window(ctx);
                }
                TrayAction::Capture => {
                    if self.recording || self.recording_starting {
                        self.report_capture_failure(
                            "録画を終了してからスクリーンショットを撮影してください。",
                        );
                    } else {
                        self.begin_capture(ctx);
                    }
                }
                TrayAction::CaptureActiveWindow => {
                    self.begin_capture_for(ctx, CaptureTarget::ActiveWindow, None);
                }
                TrayAction::CaptureAllDisplays => {
                    self.begin_capture_for(ctx, CaptureTarget::AllDisplays, None);
                }
                TrayAction::CaptureRegion => self.begin_region_selection(ctx),
                TrayAction::OpenNativeScreenshot => self.open_native_screenshot(),
                TrayAction::ToggleContinuousCapture => self.toggle_continuous_capture(ctx),
                TrayAction::ToggleRecording => self.toggle_recording(ctx),
                TrayAction::Quit => self.request_quit(ctx),
            }
        }
    }

    fn request_quit(&mut self, _ctx: &egui::Context) {
        if should_defer_quit_until_recording_finishes(self.recording, self.recording_starting) {
            self.quit_after_recording = true;
            self.stop_recording();
            #[cfg(target_os = "windows")]
            {
                _ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        } else {
            std::process::exit(0);
        }
    }

    fn open_native_screenshot(&mut self) {
        #[cfg(target_os = "macos")]
        {
            match Command::new("open")
                .args(["-b", "com.apple.screenshot.launcher"])
                .status()
            {
                Ok(status) if status.success() => {
                    self.status = "macOS標準の撮影パネルを開きました。撮影・録画の対象とオプションを選べます。".into();
                }
                Ok(status) => {
                    self.status = format!("macOS標準の撮影パネルを開けませんでした ({status})");
                }
                Err(error) => {
                    self.status = format!("macOS標準の撮影パネルを開けません: {error}");
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.status = "macOS標準の撮影パネルはmacOSで利用できます。".into();
        }
    }

    fn open_screen_recording_settings(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let settings_url = "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ScreenCapture";
            match Command::new("open").arg(settings_url).status() {
                Ok(status) if status.success() => {
                    self.status = "設定でMacShotをオンにして再起動してください。オンでも許可されない場合は、一度オフ→オンして再起動します。".into();
                }
                Ok(status) => {
                    self.status = format!("画面収録の許可設定を開けませんでした ({status})");
                }
                Err(error) => {
                    self.status = format!("画面収録の許可設定を開けません: {error}");
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.status = "画面収録の許可設定はmacOSで利用できます。".into();
        }
    }

    #[cfg(target_os = "macos")]
    fn reveal_macshot_app(&mut self) {
        let app_path = current_macos_app_bundle_path();
        let Some(app_path) = app_path else {
            self.status =
                "MacShot.appの場所を特定できません。アプリケーション版からお試しください。".into();
            return;
        };
        match Command::new("open").arg("-R").arg(&app_path).status() {
            Ok(status) if status.success() => {
                self.status = format!("FinderでMacShot.appを表示しました: {}", app_path.display());
            }
            Ok(status) => self.status = format!("Finderでアプリを表示できませんでした ({status})"),
            Err(error) => self.status = format!("Finderでアプリを表示できません: {error}"),
        }
    }

    #[cfg(target_os = "macos")]
    fn start_microphone_test(&mut self) {
        if microphone_permission() != MicrophonePermission::Authorized {
            self.microphone_test_message = Some("先にマイクを許可してください。".into());
            return;
        }
        self.microphone_test_peak = 0.0;
        self.microphone_test_message =
            Some("5秒間テストします。マイクに向かって話してください。".into());
        self.microphone_test_rx = Some(recording_audio::start_microphone_level_test());
    }

    #[cfg(target_os = "macos")]
    fn handle_microphone_test_events(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.microphone_test_rx.as_ref() else {
            return;
        };
        let mut finished = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                recording_audio::MicrophoneTestEvent::Level(peak) => {
                    self.microphone_test_peak = peak;
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
                recording_audio::MicrophoneTestEvent::Finished { peak, detected } => {
                    self.microphone_test_peak = peak;
                    self.microphone_test_message = Some(if detected {
                        format!(
                            "マイク入力を検出しました（最大 {:.0} dBFS）",
                            peak.max(1.0e-8).log10() * 20.0
                        )
                    } else {
                        "マイク入力を検出できませんでした。入力デバイスと入力音量を確認してください。".into()
                    });
                    finished = true;
                }
                recording_audio::MicrophoneTestEvent::Failed(error) => {
                    self.microphone_test_message =
                        Some(format!("マイクテストを開始できませんでした: {error}"));
                    finished = true;
                }
            }
        }
        if finished {
            self.microphone_test_rx = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn open_microphone_settings(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let settings_url = "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Microphone";
            match Command::new("open").arg(settings_url).status() {
                Ok(status) if status.success() => {
                    self.status =
                        "マイク設定を開きました。変更後に「権限状態を更新」を押してください。"
                            .into();
                }
                Ok(status) => self.status = format!("マイク設定を開けませんでした ({status})"),
                Err(error) => self.status = format!("マイク設定を開けません: {error}"),
            }
        }
        #[cfg(target_os = "windows")]
        {
            match Command::new("explorer")
                .arg("ms-settings:privacy-microphone")
                .status()
            {
                Ok(status) if status.success() => {
                    self.status = "Windowsのマイク設定を開きました。デスクトップアプリのアクセスを確認してください。".into();
                }
                Ok(status) => self.status = format!("マイク設定を開けませんでした ({status})"),
                Err(error) => self.status = format!("マイク設定を開けません: {error}"),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            self.status = "マイク設定への移動はこのOSでは利用できません。".into();
        }
    }

    #[cfg(target_os = "macos")]
    fn request_microphone_permission(&mut self, ctx: &egui::Context) {
        match microphone_permission() {
            MicrophonePermission::Authorized => {
                self.status = "マイクは既に許可されています。".into();
                return;
            }
            MicrophonePermission::Denied => {
                self.status = "マイクが拒否されています。設定でMacShotを許可してください。".into();
                self.open_microphone_settings();
                return;
            }
            MicrophonePermission::Restricted => {
                self.status =
                    "このMacではマイク利用が制限されています。管理者に確認してください。".into();
                return;
            }
            MicrophonePermission::NotDetermined => {}
        }
        self.issue_microphone_permission_request(ctx);
    }

    #[cfg(target_os = "macos")]
    fn reset_and_request_microphone_permission(&mut self, ctx: &egui::Context) {
        use objc2_foundation::NSBundle;

        let Some(bundle_identifier) = (unsafe { NSBundle::mainBundle().bundleIdentifier() }) else {
            self.status =
                "MacShotのアプリ識別子を確認できず、マイク許可をリセットできません。".into();
            return;
        };
        let bundle_identifier = bundle_identifier.to_string();
        match Command::new("/usr/bin/tccutil")
            .args(["reset", "Microphone", bundle_identifier.as_str()])
            .status()
        {
            Ok(status) if status.success() => {
                self.status =
                    "MacShotのマイク許可状態をリセットしました。許可確認を表示します。".into();
                // The user confirmed the reset in the dialog. Ask macOS again immediately.
                self.issue_microphone_permission_request(ctx);
            }
            Ok(status) => {
                self.status =
                    format!("MacShotのマイク許可状態をリセットできませんでした ({status})");
            }
            Err(error) => {
                self.status = format!("MacShotのマイク許可状態をリセットできません: {error}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn issue_microphone_permission_request(&mut self, ctx: &egui::Context) {
        let repaint = ctx.clone();
        let permission_result = self.microphone_permission_result.clone();
        let callback = RcBlock::new(move |granted: ObjcBool| {
            if let Ok(mut result) = permission_result.lock() {
                *result = Some(granted.as_bool());
            }
            repaint.request_repaint();
        });
        // SAFETY: AVAudioApplication presents macOS's app-specific recording permission dialog.
        unsafe { AVAudioApplication::requestRecordPermissionWithCompletionHandler(&callback) };
        self.status =
            "macOSにマイクの許可を確認しています。表示された画面で選択してください。".into();
    }

    fn refresh_permission_status(&mut self) {
        self.status = format!(
            "権限状態 — 画面・システム音声: {} ／ マイク: {}",
            screen_permission_label(),
            microphone_permission_label()
        );
    }

    fn toggle_recording(&mut self, ctx: &egui::Context) {
        if self.recording || self.recording_starting {
            self.stop_recording();
        } else {
            self.begin_recording(ctx);
        }
    }

    fn begin_recording(&mut self, ctx: &egui::Context) {
        if self.capture_in_progress || self.recording || self.recording_starting {
            return;
        }
        self.recording_sequence += 1;
        let sequence = self.recording_sequence;
        let target = self.capture_target;
        let format = self.recording_format;
        let quality = self.recording_quality;
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        let now = Local::now();
        let file_timestamp = now.format("%Y%m%d-%H%M%S-%3f").to_string();
        let (stop_tx, stop_rx) = mpsc::channel();
        if let Ok(mut recording_stop_tx) = self.recording_stop_tx.lock() {
            *recording_stop_tx = Some(stop_tx);
        }
        self.recording_starting = true;
        self.record_pending_stop = false;
        self.status = format!(
            "録画を開始しています（{}・{}枚目）…",
            format.label(),
            sequence
        );
        minimize_main_window_for_capture(ctx);
        let sender = self.recording_tx.clone();
        let context = ctx.clone();
        thread::spawn(move || {
            record_screen(
                target,
                folder,
                file_timestamp,
                sequence,
                format,
                quality,
                stop_rx,
                sender,
                context,
            );
        });
    }

    fn stop_recording(&mut self) {
        if self.record_pending_stop {
            return;
        }
        if let Ok(stop) = self.recording_stop_tx.lock() {
            if let Some(stop) = stop.as_ref() {
                let _ = stop.send(());
            }
            self.record_pending_stop = true;
            self.status = "録画を保存しています…".into();
        }
    }

    fn handle_recording_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.recording_rx.try_recv() {
            match event {
                RecordingEvent::Started {
                    path,
                    microphone_warning,
                } => {
                    self.recording_starting = false;
                    self.recording = true;
                    self.recording_started_at = Some(Instant::now());
                    self.recording_warning = microphone_warning;
                    let status = format!(
                        "録画中 — {}。メニューバーまたは録画ショートカットで終了",
                        path.display()
                    );
                    self.status =
                        append_recording_warning(&status, self.recording_warning.as_deref());
                    if self.record_pending_stop {
                        self.stop_recording();
                    }
                }
                RecordingEvent::MicrophoneWarning(warning) => {
                    self.recording_warning = Some(warning.clone());
                    self.status = append_recording_warning(&self.status, Some(&warning));
                }
                RecordingEvent::Finished(result) => {
                    self.recording = false;
                    self.recording_starting = false;
                    self.recording_started_at = None;
                    let recording_warning = self.recording_warning.take();
                    if let Ok(mut recording_stop_tx) = self.recording_stop_tx.lock() {
                        *recording_stop_tx = None;
                    }
                    self.record_pending_stop = false;
                    let should_show_window =
                        should_show_window_after_recording(self.continuous_capture);
                    if should_show_window {
                        restore_main_window(ctx);
                    } else {
                        minimize_main_window_for_capture(ctx);
                    }
                    match result {
                        Ok(path) => {
                            let finished_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                            let status = format!("録画を保存しました ({finished_at})");
                            self.status =
                                append_recording_warning(&status, recording_warning.as_deref());
                            self.recording_notice = Some((path, finished_at, recording_warning));
                        }
                        Err(error) => self.status = recording_failure_message(&error),
                    }
                    if self.quit_after_recording {
                        std::process::exit(0);
                    }
                }
            }
        }
    }

    fn apply_hotkey_settings(&mut self) {
        let (capture, recording) =
            match parse_hotkey_pair(&self.capture_hotkey_text, &self.recording_hotkey_text) {
                Ok(hotkeys) => hotkeys,
                Err(error) => {
                    self.settings_error = Some(error);
                    return;
                }
            };
        let manager = match self._hotkey_manager.take() {
            Some(manager) => manager,
            None => match GlobalHotKeyManager::new() {
                Ok(manager) => manager,
                Err(error) => {
                    self.settings_error =
                        Some(format!("ショートカットを初期化できません: {error}"));
                    return;
                }
            },
        };
        if !self.registered_hotkeys.is_empty()
            && let Err(error) = manager.unregister_all(&self.registered_hotkeys)
        {
            self._hotkey_manager = Some(manager);
            self.settings_error = Some(format!("現在のショートカットを解除できません: {error}"));
            return;
        }
        self.registered_hotkeys.clear();
        let mut registration_errors = Vec::new();
        for (name, hotkey) in [("撮影", capture), ("録画", recording)] {
            match manager.register(hotkey) {
                Ok(()) => self.registered_hotkeys.push(hotkey),
                Err(error) => registration_errors.push(format!("{name}: {error}")),
            }
        }
        self._hotkey_manager = Some(manager);
        self.capture_hotkey = capture;
        self.recording_hotkey = recording;
        let capture_text = canonical_hotkey_text(&capture.to_string());
        let recording_text = canonical_hotkey_text(&recording.to_string());
        self.capture_hotkey_text = capture_text.clone();
        self.recording_hotkey_text = recording_text.clone();
        match self.save_preferences() {
            Ok(()) if registration_errors.is_empty() => {
                self.settings_error = None;
                self.status = "ショートカットを保存しました。".into();
            }
            Ok(()) => {
                let message = format!(
                    "設定は保存しましたが、一部のキーは他のアプリと競合して使えません（{}）。別のキーを選んでください。",
                    registration_errors.join("、")
                );
                self.settings_error = Some(message.clone());
                self.status = message;
            }
            Err(error) => self.settings_error = Some(format!("設定を保存できません: {error}")),
        }
    }

    fn capture_target_setting(&self) -> String {
        match self.capture_target {
            CaptureTarget::ActiveWindow => "active".into(),
            CaptureTarget::PrimaryDisplay => "primary".into(),
            CaptureTarget::AllDisplays => "all".into(),
            CaptureTarget::Display(index) => format!("display:{index}"),
            CaptureTarget::Window(id) => format!("window:{id}"),
        }
    }

    fn save_preferences(&self) -> Result<(), String> {
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        save_preferences(&Preferences {
            capture_hotkey: &self.capture_hotkey_text,
            recording_hotkey: &self.recording_hotkey_text,
            capture_target: &self.capture_target_setting(),
            timestamp_enabled: self.timestamp_enabled,
            filename_prefix: &self.filename_prefix,
            show_capture_notice: self.show_capture_notice,
            annotation_color: &self.annotation_color,
            annotation_width: self.annotation_width,
            save_folder: &folder,
            recording_format: self.recording_format,
            recording_quality: self.recording_quality,
            display_density: self.display_density,
            font_size_percent: self.font_size_percent,
            settings_layout: &self.saved_settings_layout,
        })
    }

    fn save_settings_after_slider_change(
        &mut self,
        response: &egui::Response,
        ui: &egui::Ui,
        success_message: &'static str,
    ) {
        let finished_adjusting = response.drag_stopped()
            || (response.changed() && !ui.input(|input| input.pointer.any_down()));
        if !finished_adjusting {
            return;
        }
        match self.save_preferences() {
            Ok(()) => {
                self.settings_error = None;
                self.status = success_message.into();
            }
            Err(error) => self.settings_error = Some(format!("録画設定を保存できません: {error}")),
        }
    }

    fn apply_display_settings(&mut self, ctx: &egui::Context) {
        let settings = (self.display_density, self.font_size_percent);
        if self.applied_display_settings == Some(settings) {
            return;
        }
        let scale = self.display_density.spacing_scale();
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 10.0) * scale;
        style.spacing.button_padding = egui::vec2(15.0, 9.0) * scale;
        style.spacing.interact_size = egui::vec2(34.0, 34.0) * scale;
        ctx.set_style(style);
        ctx.set_zoom_factor(self.font_size_percent as f32 / 100.0);
        self.applied_display_settings = Some(settings);
    }

    fn open_evidence_image(&mut self, queue: Vec<PathBuf>, index: usize) {
        let Some(path) = queue.get(index).cloned() else {
            return;
        };
        match image::open(&path) {
            Ok(image) => {
                self.editing_queue = queue;
                self.editing_index = Some(index);
                self.left_path = Some(path.clone());
                self.left_image = Some(image);
                self.left_texture = None;
                self.crop_selection = None;
                self.crop_drag_start = None;
                self.annotations.clear();
                self.live_annotation = None;
                self.page = Page::Capture;
                self.status = format!("編集する画像を開きました: {}", path.display());
                self.refresh_diff();
            }
            Err(error) => self.status = format!("画像を開けません: {error}"),
        }
    }

    fn move_in_edit_queue(&mut self, direction: isize) {
        let Some(index) = self.editing_index else {
            return;
        };
        let next = index as isize + direction;
        if next < 0 || next >= self.editing_queue.len() as isize {
            return;
        }
        self.open_evidence_image(self.editing_queue.clone(), next as usize);
    }

    fn save_then_next(&mut self) {
        let saved = if self.crop_selection.is_some() {
            self.save_crop()
        } else if !self.annotations.is_empty() {
            self.save_annotated_image()
        } else {
            true
        };
        if saved {
            self.move_in_edit_queue(1);
        }
    }

    fn has_pending_edit(&self) -> bool {
        self.crop_selection.is_some() || !self.annotations.is_empty()
    }

    fn begin_capture(&mut self, ctx: &egui::Context) {
        self.begin_capture_for(ctx, self.capture_target, None);
    }

    fn report_capture_failure(&mut self, message: impl Into<String>) {
        self.status = message.into();
        if self.continuous_capture_session_active {
            self.continuous_capture_failed_count =
                self.continuous_capture_failed_count.saturating_add(1);
            self.update_tray_recording_status();
        }
    }

    fn advance_capture_queue(&mut self, ctx: &egui::Context) {
        while let Some(next_target) = self.capture_queue.pop_front() {
            self.begin_capture_for(ctx, next_target, None);
            if self.capture_in_progress {
                return;
            }
        }
        if !self.continuous_capture {
            self.continuous_capture_session_active = false;
            restore_main_window(ctx);
        }
        self.update_tray_recording_status();
    }

    fn toggle_continuous_capture(&mut self, ctx: &egui::Context) {
        self.continuous_capture = !self.continuous_capture;
        if self.continuous_capture {
            self.continuous_capture_saved_count = 0;
            self.continuous_capture_failed_count = 0;
            self.continuous_capture_session_active = true;
            self.status = format!(
                "連続撮影中です。{} を押すたびに順番に保存します。メニューバーから終了できます。",
                self.capture_hotkey_text
            );
            self.capture_notice = None;
            minimize_main_window_for_capture(ctx);
        } else {
            self.status = "連続撮影を終了しました。".into();
            if !self.capture_in_progress && self.capture_queue.is_empty() {
                self.continuous_capture_session_active = false;
                restore_main_window(ctx);
            }
        }
        self.update_tray_recording_status();
    }

    fn begin_region_selection(&mut self, ctx: &egui::Context) {
        if self.recording || self.recording_starting || self.capture_in_progress {
            self.report_capture_failure("録画または撮影が終わってから範囲撮影を開始してください。");
            return;
        }
        #[cfg(target_os = "windows")]
        self.begin_region_selection_windows(ctx);
        #[cfg(not(target_os = "windows"))]
        {
            let monitors = match Monitor::all() {
                Ok(monitors) => monitors,
                Err(error) => {
                    self.report_capture_failure(format!("画面を確認できません: {error}"));
                    return;
                }
            };
            let Some(monitor) = monitors
                .iter()
                .find(|monitor| monitor.is_primary().unwrap_or(false))
                .or_else(|| monitors.first())
            else {
                self.report_capture_failure("撮影できる画面が見つかりません。");
                return;
            };
            let Ok(width) = monitor.width() else {
                self.report_capture_failure("画面サイズを確認できません。");
                return;
            };
            let Ok(height) = monitor.height() else {
                self.report_capture_failure("画面サイズを確認できません。");
                return;
            };
            let (x, y, overlay_width, overlay_height) = (
                monitor.x().unwrap_or(0) as f32,
                monitor.y().unwrap_or(0) as f32,
                width as f32,
                height as f32,
            );
            self.region_overlay_position = egui::pos2(x, y);
            self.region_overlay_size = egui::vec2(overlay_width, overlay_height);
            self.region_drag_start = None;
            self.region_drag_current = None;
            self.status =
                "ドラッグして撮影範囲を選択してください。Escでキャンセルできます。".into();
            self.region_selecting = true;
            minimize_main_window_for_capture(ctx);
            ctx.request_repaint();
        }
    }

    #[cfg(target_os = "windows")]
    fn handle_region_snapshot(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.region_geometry_rx.try_recv() {
            if !self.region_geometry_pending {
                continue;
            }
            self.region_geometry_pending = false;
            match result {
                Ok((x, y, width, height)) => {
                    self.region_overlay_position = egui::pos2(x, y);
                    self.region_overlay_size = egui::vec2(width, height);
                    self.region_drag_start = None;
                    self.region_drag_current = None;
                    self.region_background_texture = None;
                    let sender = self.region_snapshot_tx.clone();
                    let repaint = ctx.clone();
                    self.region_selection_deadline = Some(Instant::now() + Duration::from_secs(15));
                    minimize_main_window_for_capture(ctx);
                    thread::spawn(move || {
                        // Let Windows reveal the desktop before taking the selector backdrop.
                        thread::sleep(Duration::from_millis(350));
                        let result = capture_target_image(CaptureTarget::PrimaryDisplay);
                        let _ = sender.send(result);
                        repaint.request_repaint();
                    });
                }
                Err(error) => {
                    self.region_selection_deadline = None;
                    self.status = format!("画面を確認できません: {error}");
                    restore_main_window(ctx);
                }
            }
        }
        if self
            .region_selection_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.region_selection_deadline = None;
            self.region_geometry_pending = false;
            self.status = "範囲選択の準備が時間切れになりました。画面収録の許可と他の撮影アプリを確認して、もう一度お試しください。".into();
            restore_main_window(ctx);
        }
        while let Ok(result) = self.region_snapshot_rx.try_recv() {
            if self.region_selection_deadline.is_none() {
                continue;
            }
            self.region_selection_deadline = None;
            match result {
                Ok(image) => {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width() as usize, image.height() as usize],
                        image.as_raw(),
                    );
                    self.region_background_texture = Some(ctx.load_texture(
                        "macshot-region-background",
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.region_selecting = true;
                }
                Err(error) => {
                    self.status = format!("範囲選択の背景を撮影できません: {error}");
                    restore_main_window(ctx);
                }
            }
            ctx.request_repaint();
        }
        if self.region_selection_deadline.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    #[cfg(target_os = "windows")]
    fn begin_region_selection_windows(&mut self, ctx: &egui::Context) {
        self.status =
            "範囲選択を準備しています…画面の確認に時間がかかる場合は自動で戻ります。".into();
        self.region_selection_deadline = Some(Instant::now() + Duration::from_secs(15));
        self.region_geometry_pending = true;
        let sender = self.region_geometry_tx.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let result = (|| {
                let monitors = Monitor::all().map_err(|error| error.to_string())?;
                let monitor = monitors
                    .iter()
                    .find(|monitor| monitor.is_primary().unwrap_or(false))
                    .or_else(|| monitors.first())
                    .ok_or_else(|| "撮影できる画面が見つかりません".to_string())?;
                let width = monitor.width().map_err(|error| error.to_string())?;
                let height = monitor.height().map_err(|error| error.to_string())?;
                let scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
                Ok((
                    monitor.x().unwrap_or(0) as f32 / scale,
                    monitor.y().unwrap_or(0) as f32 / scale,
                    width as f32 / scale,
                    height as f32 / scale,
                ))
            })();
            let _ = sender.send(result);
            repaint.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn show_region_overlay(&mut self, ctx: &egui::Context) {
        let viewport_id = egui::ViewportId::from_hash_of("macshot-region-selector");
        let builder = egui::ViewportBuilder::default()
            .with_title("MacShot 範囲選択")
            .with_position(self.region_overlay_position)
            .with_inner_size(self.region_overlay_size)
            .with_min_inner_size(self.region_overlay_size)
            .with_max_inner_size(self.region_overlay_size)
            .with_decorations(false)
            .with_transparent(cfg!(target_os = "macos"))
            .with_always_on_top()
            .with_mouse_passthrough(false)
            .with_resizable(false);
        let mut action = None;
        ctx.show_viewport_immediate(viewport_id, builder, |viewport_ctx, _class| {
            #[cfg(target_os = "macos")]
            raise_window_above_system_bars("MacShot 範囲選択");

            if viewport_ctx.input(|input| input.key_pressed(egui::Key::Escape))
                || viewport_ctx.input(|input| input.viewport().close_requested())
            {
                action = Some(None);
                viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(viewport_ctx, |ui| {
                    let screen = ui.max_rect();
                    ui.allocate_rect(screen, egui::Sense::click_and_drag());
                    let (pressed, down, released, pointer) = viewport_ctx.input(|input| {
                        (
                            input.pointer.button_pressed(egui::PointerButton::Primary),
                            input.pointer.button_down(egui::PointerButton::Primary),
                            input.pointer.button_released(egui::PointerButton::Primary),
                            input.pointer.interact_pos(),
                        )
                    });
                    if pressed {
                        self.region_drag_start = pointer;
                        self.region_drag_current = pointer;
                    } else if down || released {
                        self.region_drag_current = pointer.or(self.region_drag_current);
                    }
                    let painter = ui.painter();
                    #[cfg(target_os = "windows")]
                    if let Some(texture) = &self.region_background_texture {
                        painter.image(
                            texture.id(),
                            screen,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                    let selection = self
                        .region_drag_start
                        .zip(self.region_drag_current)
                        .map(|(start, end)| egui::Rect::from_two_pos(start, end).intersect(screen))
                        .filter(|selection| selection.width() >= 1.0 && selection.height() >= 1.0);
                    if let Some(selection) = selection {
                        let dim = egui::Color32::from_black_alpha(78);
                        for outside in [
                            egui::Rect::from_min_max(
                                screen.min,
                                egui::pos2(screen.right(), selection.top()),
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(screen.left(), selection.bottom()),
                                screen.max,
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(screen.left(), selection.top()),
                                egui::pos2(selection.left(), selection.bottom()),
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(selection.right(), selection.top()),
                                egui::pos2(screen.right(), selection.bottom()),
                            ),
                        ] {
                            if outside.is_positive() {
                                painter.rect_filled(outside, 0.0, dim);
                            }
                        }
                        painter.rect_filled(
                            selection,
                            2.0,
                            egui::Color32::from_rgba_unmultiplied(48, 145, 255, 18),
                        );
                        painter.rect_stroke(
                            selection,
                            2.0,
                            egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );
                        painter.rect_stroke(
                            selection.expand(1.5),
                            3.5,
                            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(36, 126, 255)),
                            egui::StrokeKind::Inside,
                        );
                        let label = format!(
                            "{} × {} pt",
                            selection.width().round() as u32,
                            selection.height().round() as u32
                        );
                        let label_pos = egui::pos2(
                            selection
                                .left()
                                .clamp(screen.left() + 8.0, screen.right() - 150.0),
                            if selection.top() > screen.top() + 42.0 {
                                selection.top() - 34.0
                            } else {
                                (selection.bottom() + 8.0).min(screen.bottom() - 34.0)
                            },
                        );
                        let label_rect =
                            egui::Rect::from_min_size(label_pos, egui::vec2(142.0, 26.0));
                        painter.rect_filled(
                            label_rect,
                            8.0,
                            egui::Color32::from_rgba_unmultiplied(22, 32, 48, 225),
                        );
                        painter.text(
                            label_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            label,
                            egui::FontId::proportional(13.0),
                            egui::Color32::WHITE,
                        );
                    } else {
                        painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(78));
                        painter.text(
                            screen.center_top() + egui::vec2(0.0, 30.0),
                            egui::Align2::CENTER_TOP,
                            "ドラッグして撮影範囲を選択　·　Escでキャンセル",
                            egui::FontId::proportional(22.0),
                            egui::Color32::WHITE,
                        );
                    }
                    if released
                        && let (Some(start), Some(end)) = (
                            self.region_drag_start.take(),
                            self.region_drag_current.take(),
                        )
                        && let Some(bounds) = normalized_region_bounds(screen, start, end)
                    {
                        action = Some(Some(bounds));
                        viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            viewport_ctx.request_repaint_after(Duration::from_millis(16));
        });
        if let Some(selection) = action {
            self.region_selecting = false;
            #[cfg(target_os = "windows")]
            {
                self.region_background_texture = None;
            }
            restore_main_window(ctx);
            if let Some(bounds) = selection {
                self.begin_capture_for(ctx, CaptureTarget::PrimaryDisplay, Some(bounds));
            } else {
                self.status = "範囲撮影をキャンセルしました。".into();
            }
        }
        if self.region_selecting {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn begin_capture_for(
        &mut self,
        ctx: &egui::Context,
        target: CaptureTarget,
        region: Option<[f32; 4]>,
    ) {
        if self.capture_in_progress {
            self.capture_queue.push_back(target);
            self.status = format!(
                "撮影を順番待ちに追加しました（残り{}枚）。",
                self.capture_queue.len()
            );
            return;
        }
        if let Some(message) = capture_target_preflight_error(
            target,
            self.recording,
            self.recording_starting,
            self.monitors.len(),
            &self.windows,
        ) {
            self.report_capture_failure(message);
            return;
        }

        self.capture_sequence += 1;
        let sequence = self.capture_sequence;
        self.active_capture_sequence = Some(sequence);
        self.last_capture_sequence = sequence;
        let captured_at = Local::now();
        let timestamp = captured_at.format("%Y-%m-%d %H:%M:%S").to_string();
        let file_timestamp = captured_at.format("%Y%m%d-%H%M%S-%3f").to_string();
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        let filename_prefix = self.filename_prefix.clone();
        let timestamp_enabled = self.timestamp_enabled;
        let sender = self.capture_tx.clone();
        let ctx = ctx.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            self.capture_deadline = Some(Instant::now() + Duration::from_secs(15));
            self.capture_cancel = Some(cancelled.clone());
            let watchdog_context = ctx.clone();
            let watchdog_cancelled = cancelled.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(15));
                if !watchdog_cancelled.load(Ordering::Relaxed) {
                    restore_main_window(&watchdog_context);
                    watchdog_context.request_repaint();
                }
            });
        }
        self.capture_in_progress = true;
        self.status = format!("撮影 {} 枚目を保存しています…", sequence);
        minimize_main_window_for_capture(&ctx);
        self.update_tray_recording_status();
        ctx.request_repaint_after(Duration::from_millis(100));

        thread::spawn(move || {
            // Give the window manager time to reveal the previously active application.
            thread::sleep(Duration::from_millis(350));
            let capture = if cancelled.load(Ordering::Relaxed) {
                Err(capture_timeout_message().to_string())
            } else {
                capture_target_image(target).and_then(|image| {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(capture_timeout_message().to_string());
                    }
                    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
                    let path =
                        new_evidence_path(&folder, &filename_prefix, &file_timestamp, sequence, "");
                    let mut image = if let Some(bounds) = region {
                        crop_normalized(&image, bounds).ok_or_else(|| {
                            "選択範囲が小さすぎます。ドラッグして範囲を選択してください。"
                                .to_string()
                        })?
                    } else {
                        image
                    };
                    if timestamp_enabled {
                        stamp_timestamp(&mut image, &timestamp);
                    }
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(capture_timeout_message().to_string());
                    }
                    let path = save_png_without_overwrite(&folder, &path, &image)?;
                    if cancelled.load(Ordering::Relaxed) {
                        let _ = std::fs::remove_file(&path);
                        return Err(capture_timeout_message().to_string());
                    }
                    Ok((path, image, timestamp, sequence))
                })
            };

            let _ = sender.send((sequence, capture));
            ctx.request_repaint();
        });
    }

    fn handle_capture_results(&mut self, ctx: &egui::Context) {
        // Resolve an expired deadline before consuming results so a late result
        // cannot win a race with the next UI update.
        self.handle_capture_timeout(ctx);
        while let Ok((sequence, result)) = self.capture_rx.try_recv() {
            if self.active_capture_sequence != Some(sequence) {
                continue;
            }
            self.capture_in_progress = false;
            self.active_capture_sequence = None;
            {
                self.capture_deadline = None;
                if let Some(cancelled) = self.capture_cancel.take() {
                    cancelled.store(true, Ordering::Relaxed);
                }
            }
            match result {
                Ok((path, image, timestamp, sequence)) => {
                    let (width, height) = image.dimensions();
                    self.status = capture_saved_status(sequence, &timestamp, width, height);
                    if self.continuous_capture_session_active {
                        self.continuous_capture_saved_count =
                            self.continuous_capture_saved_count.saturating_add(1);
                    }
                    if self.show_capture_notice && !self.continuous_capture {
                        self.capture_notice = Some(CaptureNotice {
                            sequence,
                            timestamp,
                            path: path.clone(),
                            width,
                            height,
                        });
                    }
                    self.editing_queue.clear();
                    self.editing_index = None;
                    self.left_path = Some(path);
                    self.left_image = Some(DynamicImage::ImageRgba8(image));
                    let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                    self.evidence_files = evidence_images(&folder);
                    self.recording_files = recording_files(&folder);
                    self.left_texture = None;
                    self.crop_selection = None;
                    self.crop_drag_start = None;
                    self.annotations.clear();
                    self.live_annotation = None;
                    self.page = Page::Capture;
                    self.refresh_diff();
                }
                Err(error) => {
                    self.status = capture_failure_message(&error);
                    if self.continuous_capture_session_active {
                        self.continuous_capture_failed_count =
                            self.continuous_capture_failed_count.saturating_add(1);
                    }
                }
            }
            self.advance_capture_queue(ctx);
        }
    }

    fn handle_capture_timeout(&mut self, ctx: &egui::Context) {
        if self
            .capture_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            if let Some(cancelled) = self.capture_cancel.take() {
                cancelled.store(true, Ordering::Relaxed);
            }
            self.capture_deadline = None;
            self.capture_in_progress = false;
            self.active_capture_sequence = None;
            self.capture_queue.clear();
            if self.continuous_capture_session_active {
                self.continuous_capture_failed_count =
                    self.continuous_capture_failed_count.saturating_add(1);
            }
            if !self.continuous_capture {
                self.continuous_capture_session_active = false;
            }
            self.status = capture_timeout_message().into();
            restore_main_window(ctx);
            self.update_tray_recording_status();
        }
        if self.capture_deadline.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn choose_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_directory(
                self.save_folder
                    .as_deref()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .pick_folder()
        {
            self.save_folder = Some(path.clone());
            let _ = self.save_preferences();
            self.status = format!("保存先: {}", path.display());
        }
    }

    fn choose_image(&mut self, left: bool) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("画像", &["png", "jpg", "jpeg"])
            .pick_file()
        {
            match image::open(&path) {
                Ok(image) => {
                    if left {
                        self.editing_queue.clear();
                        self.editing_index = None;
                        self.left_path = Some(path.clone());
                        self.left_image = Some(image);
                        self.left_texture = None;
                        self.crop_selection = None;
                        self.crop_drag_start = None;
                    } else {
                        self.right_path = Some(path.clone());
                        self.right_image = Some(image);
                        self.right_texture = None;
                        self.page = Page::Compare;
                    }
                    self.refresh_diff();
                    self.status = format!("読み込みました: {}", path.display());
                }
                Err(error) => self.status = format!("画像を開けません: {error}"),
            }
        }
    }

    fn save_crop(&mut self) -> bool {
        let (Some(image), Some(selection)) = (&self.left_image, self.crop_selection) else {
            return false;
        };
        let source = annotated_image(
            image,
            &self.annotations,
            annotation_rgb(&self.annotation_color),
            self.annotation_width,
        );
        let Some(cropped) = crop_normalized(
            &source,
            [
                selection.min.x,
                selection.min.y,
                selection.max.x,
                selection.max.y,
            ],
        ) else {
            self.status = "切り出す範囲が小さすぎます。もう一度ドラッグしてください。".into();
            return false;
        };
        let folder = self
            .save_folder
            .clone()
            .or_else(|| {
                self.left_path
                    .as_ref()
                    .and_then(|path| path.parent().map(Path::to_path_buf))
            })
            .unwrap_or_else(default_save_folder);
        if let Err(error) = std::fs::create_dir_all(&folder) {
            self.status = format!("保存先を作成できません: {error}");
            return false;
        }
        let (timestamp, sequence) = evidence_identity_for_edit(
            self.left_path.as_deref(),
            self.editing_index
                .map(|index| index as u64 + 1)
                .unwrap_or(self.last_capture_sequence),
        );
        let path = new_evidence_path(
            &folder,
            &self.filename_prefix,
            &timestamp,
            sequence,
            "-crop",
        );
        let path = match save_png_without_overwrite(&folder, &path, &cropped) {
            Ok(path) => path,
            Err(error) => {
                self.status = format!("切り出し画像を保存できません: {error}");
                return false;
            }
        };
        self.left_path = Some(path.clone());
        self.left_image = Some(DynamicImage::ImageRgba8(cropped));
        self.left_texture = None;
        self.crop_selection = None;
        self.crop_drag_start = None;
        self.annotations.clear();
        self.live_annotation = None;
        self.refresh_diff();
        self.status = format!("切り出して保存しました: {}", path.display());
        true
    }

    fn save_annotated_image(&mut self) -> bool {
        let Some(image) = &self.left_image else {
            return false;
        };
        if self.annotations.is_empty() {
            return false;
        }
        let output = annotated_image(
            image,
            &self.annotations,
            annotation_rgb(&self.annotation_color),
            self.annotation_width,
        );
        let folder = self
            .save_folder
            .clone()
            .or_else(|| {
                self.left_path
                    .as_ref()
                    .and_then(|path| path.parent().map(Path::to_path_buf))
            })
            .unwrap_or_else(default_save_folder);
        if let Err(error) = std::fs::create_dir_all(&folder) {
            self.status = format!("保存先を作成できません: {error}");
            return false;
        }
        let (timestamp, sequence) = evidence_identity_for_edit(
            self.left_path.as_deref(),
            self.editing_index
                .map(|index| index as u64 + 1)
                .unwrap_or(self.last_capture_sequence),
        );
        let path = new_evidence_path(
            &folder,
            &self.filename_prefix,
            &timestamp,
            sequence,
            "-marked",
        );
        let path = match save_png_without_overwrite(&folder, &path, &output) {
            Ok(path) => path,
            Err(error) => {
                self.status = format!("注釈画像を保存できません: {error}");
                return false;
            }
        };
        self.left_path = Some(path.clone());
        self.left_image = Some(DynamicImage::ImageRgba8(output));
        self.left_texture = None;
        self.annotations.clear();
        self.live_annotation = None;
        self.refresh_diff();
        self.status = format!("注釈付き画像を保存しました: {}", path.display());
        true
    }

    fn refresh_diff(&mut self) {
        let (Some(left), Some(right)) = (&self.left_image, &self.right_image) else {
            self.diff_image = None;
            self.overlay_image = None;
            self.diff_texture = None;
            self.overlay_texture = None;
            return;
        };
        let (diff, overlay, count) = compare_images(left, right);
        self.diff_count = count;
        self.diff_image = Some(DynamicImage::ImageRgba8(diff));
        self.overlay_image = Some(DynamicImage::ImageRgba8(overlay));
        self.diff_texture = None;
        self.overlay_texture = None;
    }

    fn show_image(
        ui: &mut egui::Ui,
        image: &DynamicImage,
        texture: &mut Option<egui::TextureHandle>,
        id: &str,
    ) {
        let size = [image.width() as usize, image.height() as usize];
        if texture.is_none() {
            let rgba = image.to_rgba8();
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            *texture = Some(
                ui.ctx()
                    .load_texture(id, color, egui::TextureOptions::LINEAR),
            );
        }
        let available = ui.available_size();
        let scale = (available.x / size[0] as f32)
            .min(available.y / size[1] as f32)
            .min(1.0);
        ui.image((
            texture.as_ref().unwrap().id(),
            egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale),
        ));
    }

    fn show_recording_preview_image(
        ui: &mut egui::Ui,
        image: &DynamicImage,
        texture: &mut Option<egui::TextureHandle>,
    ) -> egui::Response {
        let size = [image.width() as usize, image.height() as usize];
        if texture.is_none() {
            let rgba = image.to_rgba8();
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            *texture = Some(ui.ctx().load_texture(
                "recording-preview",
                color,
                egui::TextureOptions::LINEAR,
            ));
        }
        let display_size =
            recording_preview_display_size(image.width(), image.height(), ui.available_width());
        ui.image((texture.as_ref().unwrap().id(), display_size))
    }

    fn show_croppable_image(&mut self, ui: &mut egui::Ui) {
        let Some(image) = &self.left_image else {
            ui.centered_and_justified(|ui| {
                ui.label("画面全体またはウィンドウを撮影すると、ここにプレビューが表示されます。\n画像をドラッグして必要な範囲を切り出せます。");
            });
            return;
        };

        let size = [image.width() as usize, image.height() as usize];
        if self.left_texture.is_none() {
            let rgba = image.to_rgba8();
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            self.left_texture = Some(ui.ctx().load_texture(
                "capture-preview",
                color,
                egui::TextureOptions::LINEAR,
            ));
        }
        let available = ui.available_size();
        let scale = (available.x / size[0] as f32)
            .min(available.y / size[1] as f32)
            .min(1.0);
        let display_size = egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale);
        let (rect, response) = ui.allocate_exact_size(display_size, egui::Sense::click_and_drag());
        ui.painter().image(
            self.left_texture.as_ref().unwrap().id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        let to_normalized = |point: egui::Pos2| {
            egui::pos2(
                ((point.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
                ((point.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
            )
        };
        if self.edit_tool == EditTool::Crop {
            if response.drag_started() {
                self.crop_drag_start = response.interact_pointer_pos();
            }
            if response.dragged()
                && let (Some(start), Some(pointer)) =
                    (self.crop_drag_start, response.interact_pointer_pos())
            {
                let selection = egui::Rect::from_two_pos(start, pointer).intersect(rect);
                if selection.width() > 1.0 && selection.height() > 1.0 {
                    self.crop_selection = Some(egui::Rect::from_min_max(
                        to_normalized(selection.min),
                        to_normalized(selection.max),
                    ));
                }
            }
            // Two clicks remain available when dragging is inconvenient.
            if response.clicked()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                if let Some(start) = self.crop_drag_start.take() {
                    let selection = egui::Rect::from_two_pos(start, pointer).intersect(rect);
                    if selection.width() > 1.0 && selection.height() > 1.0 {
                        self.crop_selection = Some(egui::Rect::from_min_max(
                            to_normalized(selection.min),
                            to_normalized(selection.max),
                        ));
                    }
                } else {
                    self.crop_drag_start = Some(pointer);
                    self.status =
                        "始点を指定しました。終点をクリックするかドラッグしてください。".into();
                }
            }
        } else {
            if response.drag_started()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                self.crop_drag_start = Some(to_normalized(pointer));
            }
            if response.dragged()
                && let (Some(start), Some(pointer)) =
                    (self.crop_drag_start, response.interact_pointer_pos())
            {
                self.live_annotation = Some(Annotation {
                    tool: self.edit_tool,
                    start,
                    end: to_normalized(pointer),
                });
            }
            if response.drag_stopped() {
                if let Some(annotation) = self.live_annotation.take() {
                    self.annotations.push(annotation);
                }
                self.crop_drag_start = None;
            }
            if response.clicked()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let point = to_normalized(pointer);
                if let Some(start) = self.crop_drag_start.take() {
                    self.annotations.push(Annotation {
                        tool: self.edit_tool,
                        start,
                        end: point,
                    });
                } else {
                    self.crop_drag_start = Some(point);
                    self.status =
                        "始点を指定しました。終点をクリックするかドラッグしてください。".into();
                }
            }
        }

        if self.edit_tool == EditTool::Crop
            && let Some(selection) = self.crop_selection
        {
            let overlay = egui::Rect::from_min_max(
                rect.min
                    + egui::vec2(
                        selection.min.x * rect.width(),
                        selection.min.y * rect.height(),
                    ),
                rect.min
                    + egui::vec2(
                        selection.max.x * rect.width(),
                        selection.max.y * rect.height(),
                    ),
            );
            ui.painter().rect_filled(
                overlay,
                0.0,
                egui::Color32::from_rgba_unmultiplied(35, 130, 255, 45),
            );
            ui.painter().rect_stroke(
                overlay,
                0.0,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(25, 125, 255)),
                egui::StrokeKind::Inside,
            );
        }
        for annotation in self.annotations.iter().copied().chain(self.live_annotation) {
            let start = rect.min
                + egui::vec2(
                    annotation.start.x * rect.width(),
                    annotation.start.y * rect.height(),
                );
            let end = rect.min
                + egui::vec2(
                    annotation.end.x * rect.width(),
                    annotation.end.y * rect.height(),
                );
            let rgb = annotation_rgb(&self.annotation_color);
            let stroke = egui::Stroke::new(
                self.annotation_width,
                egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
            );
            match annotation.tool {
                EditTool::Rectangle => {
                    ui.painter().rect_stroke(
                        egui::Rect::from_two_pos(start, end),
                        0.0,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }
                EditTool::Arrow => {
                    ui.painter().line_segment([start, end], stroke);
                    let angle = (end.y - start.y).atan2(end.x - start.x);
                    let head = 14.0 * (self.annotation_width / 3.0);
                    for side in [-0.55_f32, 0.55_f32] {
                        let head_angle = angle + std::f32::consts::PI + side;
                        let tip =
                            end + egui::vec2(head * head_angle.cos(), head * head_angle.sin());
                        ui.painter().line_segment([end, tip], stroke);
                    }
                }
                EditTool::Crop => {}
            }
        }
    }
}

fn compare_images(
    left: &DynamicImage,
    right: &DynamicImage,
) -> (image::RgbaImage, image::RgbaImage, u64) {
    let width = left.width().max(right.width());
    let height = left.height().max(right.height());
    let mut diff = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(width, height);
    let mut overlay = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(width, height);
    let mut count = 0;

    for y in 0..height {
        for x in 0..width {
            let a = if x < left.width() && y < left.height() {
                left.get_pixel(x, y)
            } else {
                Rgba([0, 0, 0, 0])
            };
            let b = if x < right.width() && y < right.height() {
                right.get_pixel(x, y)
            } else {
                Rgba([0, 0, 0, 0])
            };
            if a != b {
                count += 1;
                diff.put_pixel(x, y, Rgba([255, 54, 54, 255]));
            } else {
                diff.put_pixel(x, y, Rgba([a[0] / 5, a[1] / 5, a[2] / 5, 255]));
            }
            overlay.put_pixel(
                x,
                y,
                Rgba([
                    (a[0] as u16 / 2 + b[0] as u16 / 2) as u8,
                    (a[1] as u16 / 2 + b[1] as u16 / 2) as u8,
                    (a[2] as u16 / 2 + b[2] as u16 / 2) as u8,
                    255,
                ]),
            );
        }
    }

    (diff, overlay, count)
}

impl MacShot {
    fn target_caption(&self) -> String {
        match self.capture_target {
            CaptureTarget::ActiveWindow => "アクティブウィンドウ".into(),
            CaptureTarget::PrimaryDisplay => "メインディスプレイ".into(),
            CaptureTarget::AllDisplays => "すべてのディスプレイ".into(),
            CaptureTarget::Display(index) => self
                .monitors
                .get(index)
                .and_then(|monitor| monitor.name().ok())
                .unwrap_or_else(|| format!("ディスプレイ {}", index + 1)),
            CaptureTarget::Window(id) => self
                .windows
                .iter()
                .find(|window| window.id == id)
                .map(|window| window.title.clone())
                .unwrap_or_else(|| "ウィンドウを選択".into()),
        }
    }

    fn show_header(&mut self, ctx: &egui::Context) {
        let recording = self.recording || self.recording_starting;
        egui::TopBottomPanel::top("macshot_header")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(250, 251, 255, 228))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        egui::Color32::from_black_alpha(20),
                    ))
                    .inner_margin(egui::Margin::symmetric(22, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("◉")
                                .size(29.0)
                                .strong()
                                .color(ACCENT_BRIGHT),
                        );
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("MacShot").size(20.0).strong());
                            ui.label(
                                egui::RichText::new("EVIDENCE STUDIO")
                                    .size(9.0)
                                    .color(TEXT_MUTED),
                            );
                        });
                    });
                    ui.add_space(22.0);
                    for (page, label) in [
                        (Page::Capture, "◉  撮影・編集"),
                        (Page::Evidence, "▦  証跡一覧"),
                        (Page::Compare, "⇄  画像比較"),
                        (Page::Settings, "⚙  設定"),
                    ] {
                        let selected = self.page == page;
                        let fill = if selected {
                            egui::Color32::from_rgba_unmultiplied(100, 116, 220, 35)
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let stroke = if selected {
                            egui::Stroke::new(
                                1.0_f32,
                                egui::Color32::from_rgba_unmultiplied(82, 99, 211, 100),
                            )
                        } else {
                            egui::Stroke::NONE
                        };
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new(label).size(13.0))
                                    .fill(fill)
                                    .stroke(stroke)
                                    .corner_radius(egui::CornerRadius::same(13))
                                    .min_size(egui::vec2(104.0, 40.0)),
                            )
                            .clicked()
                        {
                            self.page = page;
                            if page == Page::Evidence {
                                let folder =
                                    self.save_folder.clone().unwrap_or_else(default_save_folder);
                                self.evidence_files = evidence_images(&folder);
                                self.recording_files = recording_files(&folder);
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let record_label = if recording {
                            "■  録画を終了"
                        } else {
                            "●  録画"
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(record_label)
                                        .color(egui::Color32::from_rgb(177, 50, 73)),
                                )
                                .fill(egui::Color32::from_rgba_unmultiplied(249, 83, 108, 38))
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    egui::Color32::from_rgba_unmultiplied(205, 75, 98, 90),
                                ))
                                .corner_radius(egui::CornerRadius::same(13))
                                .min_size(egui::vec2(116.0, 42.0)),
                            )
                            .clicked()
                        {
                            self.toggle_recording(ctx);
                        }
                        let capture_label = if self.capture_in_progress {
                            "撮影中…"
                        } else {
                            "撮影してプレビュー"
                        };
                        if ui
                            .add_enabled(!self.capture_in_progress, primary_button(capture_label))
                            .clicked()
                        {
                            self.begin_capture(ctx);
                        }
                    });
                });
            });
    }

    fn show_capture_target_card(&mut self, ui: &mut egui::Ui) {
        glass_frame().show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("撮影対象").size(11.0).color(TEXT_MUTED));
                        ui.add_space(4.0);
                        egui::ComboBox::from_id_salt("capture_target")
                            .selected_text(self.target_caption())
                            .width(250.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.capture_target,
                                    CaptureTarget::ActiveWindow,
                                    "アクティブウィンドウ",
                                );
                                ui.selectable_value(
                                    &mut self.capture_target,
                                    CaptureTarget::PrimaryDisplay,
                                    "デスクトップ（メイン画面）",
                                );
                                ui.selectable_value(
                                    &mut self.capture_target,
                                    CaptureTarget::AllDisplays,
                                    "全画面（全ディスプレイ）",
                                );
                                ui.separator();
                                for (index, monitor) in self.monitors.iter().enumerate() {
                                    let name = monitor
                                        .name()
                                        .unwrap_or_else(|_| format!("画面 {}", index + 1));
                                    ui.selectable_value(
                                        &mut self.capture_target,
                                        CaptureTarget::Display(index),
                                        format!("画面全体 — {name}"),
                                    );
                                }
                                if !self.windows.is_empty() {
                                    ui.separator();
                                    for window in &self.windows {
                                        ui.selectable_value(
                                            &mut self.capture_target,
                                            CaptureTarget::Window(window.id),
                                            format!("ウィンドウ — {}", window.title),
                                        );
                                    }
                                }
                            });
                    });
                    if ui.add(primary_button("範囲を選択")).clicked() {
                        self.begin_region_selection(ui.ctx());
                    }
                    if ui.button("↻  ウィンドウ一覧を更新").clicked() {
                        match discover_windows() {
                            Ok(windows) => {
                                self.windows = windows;
                                self.status =
                                    format!("{}個のウィンドウを確認しました。", self.windows.len());
                                if matches!(self.capture_target, CaptureTarget::Window(id) if !self.windows.iter().any(|window| window.id == id)) {
                                    self.capture_target = CaptureTarget::ActiveWindow;
                                }
                            }
                            Err(error) => self.status = capture_failure_message(&error),
                        }
                    }
                });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("追加の操作").size(11.0).color(TEXT_MUTED));
                    let continuous_label = if self.continuous_capture {
                        "連続撮影を終了"
                    } else {
                        "連続撮影を開始"
                    };
                    if ui.button(continuous_label).clicked() {
                        self.toggle_continuous_capture(ui.ctx());
                    }
                    if cfg!(target_os = "macos")
                        && ui.button("macOS標準の撮影・録画…").clicked()
                    {
                        self.open_native_screenshot();
                    }
                    ui.separator();
                    for (title, value) in [
                        ("撮影", self.capture_hotkey_text.as_str()),
                        ("録画", self.recording_hotkey_text.as_str()),
                    ] {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 145))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(22)))
                            .corner_radius(egui::CornerRadius::same(10))
                            .inner_margin(egui::Margin::symmetric(10, 7))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(title).size(10.0).color(TEXT_MUTED));
                                    ui.label(
                                        egui::RichText::new(hotkey_badge_text(value))
                                            .size(11.0)
                                            .strong(),
                                    );
                                });
                            });
                    }
                    if ui.button("▣  保存先を変更").clicked() {
                        self.choose_folder();
                    }
                });
            });
        });
    }

    fn show_capture_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if cfg!(target_os = "macos")
            && should_show_screen_permission_banner(
                screen_recording_access_granted(),
                self.editing_index.is_some(),
            )
        {
            glass_frame().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("画面撮影・録画には画面収録の許可が必要です。")
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("権限状態は設定画面で確認できます。オンでも撮影が拒否される場合はMacShot.appを登録し直して再起動してください。")
                            .size(12.0)
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui.add(primary_button("画面収録設定を開く")).clicked() {
                            self.open_screen_recording_settings();
                        }
                        #[cfg(target_os = "macos")]
                        if ui.button("FinderでMacShotを表示").clicked() {
                            self.reveal_macshot_app();
                        }
                        if ui.button("権限状態を更新").clicked() {
                            self.refresh_permission_status();
                        }
                    });
                });
            });
            ui.add_space(12.0);
        }
        self.show_capture_target_card(ui);
        ui.add_space(12.0);
        glass_frame().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("▦  撮影済みを開く").clicked() {
                    self.page = Page::Evidence;
                    let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                    self.evidence_files = evidence_images(&folder);
                    self.recording_files = recording_files(&folder);
                }
                if let Some(index) = self.editing_index {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            index + 1,
                            self.editing_queue.len()
                        ))
                        .size(12.0)
                        .strong(),
                    );
                    if ui
                        .add_enabled(index > 0, egui::Button::new("‹ 前へ"))
                        .clicked()
                    {
                        self.move_in_edit_queue(-1);
                    }
                    let has_next = index + 1 < self.editing_queue.len();
                    if ui
                        .add_enabled(has_next, egui::Button::new("次へ ›"))
                        .clicked()
                    {
                        self.move_in_edit_queue(1);
                    }
                    if has_next
                        && self.has_pending_edit()
                        && ui.add(primary_button("保存して次へ")).clicked()
                    {
                        self.save_then_next();
                    }
                }
                ui.label(
                    egui::RichText::new("編集ツール")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                ui.separator();
                for (tool, label) in [
                    (EditTool::Crop, "⌗  切り出し"),
                    (EditTool::Rectangle, "□  赤枠"),
                    (EditTool::Arrow, "↗  矢印"),
                ] {
                    let selected = self.edit_tool == tool;
                    if ui
                        .add(
                            egui::Button::new(label)
                                .fill(if selected {
                                    egui::Color32::from_rgba_unmultiplied(100, 116, 220, 35)
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .stroke(if selected {
                                    egui::Stroke::new(1.0_f32, ACCENT)
                                } else {
                                    egui::Stroke::NONE
                                })
                                .corner_radius(egui::CornerRadius::same(11)),
                        )
                        .clicked()
                    {
                        self.edit_tool = tool;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.annotations.is_empty() {
                        if ui.button("注釈を消去").clicked() {
                            self.annotations.clear();
                            self.live_annotation = None;
                        }
                        if ui.button("注釈画像を保存").clicked() {
                            self.save_annotated_image();
                        }
                    }
                    if self.edit_tool == EditTool::Crop && self.crop_selection.is_some() {
                        if ui.button("選択解除").clicked() {
                            self.crop_selection = None;
                            self.crop_drag_start = None;
                        }
                        if ui.add(primary_button("範囲を保存")).clicked() {
                            self.save_crop();
                        }
                    }
                });
            });
        });
        ui.add_space(12.0);
        let preview_height = ui.available_height().max(340.0);
        glass_frame().show(ui, |ui| {
            ui.set_min_height(preview_height - 34.0);
            if self.left_image.is_some() {
                ui.centered_and_justified(|ui| self.show_croppable_image(ui));
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space((preview_height * 0.2).max(36.0));
                    ui.label(egui::RichText::new("◉").size(64.0).color(ACCENT));
                    ui.add_space(7.0);
                    ui.label(
                        egui::RichText::new("証跡の撮影を始めましょう")
                            .size(22.0)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("撮影した画面は、ここで切り出しや注釈を追加できます。")
                            .size(13.0)
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(18.0);
                    if ui.add(primary_button("撮影してプレビュー")).clicked() {
                        self.begin_capture(ctx);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "ショートカット  {}",
                            hotkey_badge_text(&self.capture_hotkey_text)
                        ))
                        .size(11.0)
                        .color(TEXT_MUTED),
                    );
                });
            }
        });
    }

    fn show_compare_page(&mut self, ui: &mut egui::Ui) {
        glass_frame().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("画像比較").size(20.0).strong());
                    ui.label(
                        egui::RichText::new("2枚の画面を並べて、変更点を確認します。")
                            .color(TEXT_MUTED),
                    );
                });
                ui.separator();
                for (mode, label) in [
                    (CompareMode::SideBySide, "左右"),
                    (CompareMode::Overlay, "重ね合わせ"),
                    (CompareMode::Difference, "差分"),
                ] {
                    let selected = self.compare_mode == mode;
                    if ui
                        .add(
                            egui::Button::new(label)
                                .fill(if selected {
                                    egui::Color32::from_rgba_unmultiplied(100, 116, 220, 35)
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .stroke(if selected {
                                    egui::Stroke::new(1.0_f32, ACCENT)
                                } else {
                                    egui::Stroke::NONE
                                })
                                .corner_radius(egui::CornerRadius::same(11)),
                        )
                        .clicked()
                    {
                        self.compare_mode = mode;
                    }
                }
                ui.label(
                    egui::RichText::new(format!("差分ピクセル  {}", self.diff_count))
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                if ui.button("＋  基準画像").clicked() {
                    self.choose_image(true);
                }
                if ui.button("＋  比較画像").clicked() {
                    self.choose_image(false);
                }
            });
        });
        ui.add_space(14.0);
        match self.compare_mode {
            CompareMode::SideBySide => {
                ui.columns(2, |columns| {
                    for (index, column) in columns.iter_mut().enumerate() {
                        let path = if index == 0 {
                            self.left_path.as_ref()
                        } else {
                            self.right_path.as_ref()
                        };
                        let name = path
                            .map(|path| {
                                path.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string()
                            })
                            .unwrap_or_else(|| {
                                if index == 0 {
                                    "基準画像".into()
                                } else {
                                    "比較画像".into()
                                }
                            });
                        glass_frame().show(column, |ui| {
                            ui.label(egui::RichText::new(name).size(13.0).strong());
                            ui.add_space(8.0);
                            ui.set_min_height(ui.available_height().max(300.0));
                            if index == 0 {
                                if let Some(image) = &self.left_image {
                                    Self::show_image(ui, image, &mut self.left_texture, "left");
                                } else {
                                    compare_empty_state(ui, "画像を選択してください");
                                }
                            } else if let Some(image) = &self.right_image {
                                Self::show_image(ui, image, &mut self.right_texture, "right");
                            } else {
                                compare_empty_state(ui, "画像を選択してください");
                            }
                        });
                    }
                });
            }
            CompareMode::Overlay => {
                glass_frame().show(ui, |ui| {
                    if let Some(image) = &self.overlay_image {
                        Self::show_image(ui, image, &mut self.overlay_texture, "overlay");
                    } else {
                        compare_empty_state(ui, "比較する2枚の画像を選んでください");
                    }
                });
            }
            CompareMode::Difference => {
                glass_frame().show(ui, |ui| {
                    if let Some(image) = &self.diff_image {
                        Self::show_image(ui, image, &mut self.diff_texture, "diff");
                    } else {
                        compare_empty_state(ui, "比較する2枚の画像を選んでください");
                    }
                });
            }
        }
    }

    fn advance_recording_playback(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut event = None;
        let mut request_failed = false;
        if let Some(playback) = &mut self.recording_playback {
            while let Ok(next) = playback.frame_rx.try_recv() {
                event = Some(next);
            }
            if playback.playing && !playback.awaiting_frame && now >= playback.frame_due {
                if playback.request_tx.send(()).is_ok() {
                    playback.awaiting_frame = true;
                } else {
                    request_failed = true;
                }
            }
            if playback.playing {
                ctx.request_repaint_after(Duration::from_millis(10));
            }
        }

        let mut finish = false;
        if let Some(event) = event {
            if let Some(playback) = &mut self.recording_playback {
                playback.awaiting_frame = false;
            }
            match event {
                RecordingPlaybackEvent::Frame(image, delay) => {
                    self.recording_preview_image = Some(image);
                    self.recording_preview_texture = None;
                    if let Some(playback) = &mut self.recording_playback {
                        playback.frame_due = Instant::now() + delay.max(Duration::from_millis(10));
                    }
                }
                RecordingPlaybackEvent::End => {
                    self.status = "録画の再生が終わりました。もう一度再生できます。".into();
                    finish = true;
                }
                RecordingPlaybackEvent::Error(error) => {
                    self.status = format!("録画を再生できませんでした: {error}");
                    finish = true;
                }
            }
        }
        if request_failed {
            self.status = "録画の再生を開始できませんでした。".into();
            finish = true;
        }
        if finish {
            self.recording_playback = None;
        }
    }

    fn show_evidence_page(&mut self, ui: &mut egui::Ui) {
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("撮影済みの画像・録画")
                        .size(24.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new("録画は一覧からプレビュー、画像は編集して保存できます。")
                        .color(TEXT_MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("▣  保存先を開く").clicked() {
                    #[cfg(target_os = "macos")]
                    let command = "open";
                    #[cfg(target_os = "windows")]
                    let command = "explorer";
                    let _ = std::process::Command::new(command).arg(&folder).spawn();
                }
                if ui.button("↻  更新").clicked() {
                    self.evidence_files = evidence_images(&folder);
                    self.recording_files = recording_files(&folder);
                }
            });
        });
        ui.add_space(14.0);
        glass_frame().show(ui, |ui| {
            ui.label(egui::RichText::new(folder.display().to_string()).size(11.0).color(TEXT_MUTED));
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("録画一覧").size(19.0).strong());
                ui.label(egui::RichText::new(format!("{} 件（GIF / MP4）", self.recording_files.len())).color(TEXT_MUTED));
            });
            ui.label(egui::RichText::new("録画名を選ぶとプレビューを表示します。MP4の音声を聴くときは「外部で開く」を使ってください。").size(11.0).color(TEXT_MUTED));
            ui.add_space(8.0);
            if self.recording_files.is_empty() {
                ui.label(egui::RichText::new("録画が見つかりません。保存先を確認するか「更新」を押してください。").color(TEXT_MUTED));
            } else {
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    let files = self.recording_files.clone();
                    for path in files {
                        let selected = self.recording_preview_path.as_ref() == Some(&path);
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        ui.horizontal_wrapped(|ui| {
                            if ui.selectable_label(selected, format!("▧  {name}")).clicked() {
                                match recording_preview(&path) {
                                    Ok((image, details)) => {
                                        self.recording_playback = None;
                                        self.recording_preview_path = Some(path.clone());
                                        self.recording_preview_image = Some(image);
                                        self.recording_preview_texture = None;
                                        self.recording_preview_details = Some(details.clone());
                                        self.scroll_to_recording_preview = true;
                                        self.status = format!("録画を読み込みました: {details}");
                                    }
                                    Err(error) => self.status = format!("録画を読み込めませんでした: {error}"),
                                }
                            }
                            let recording_summary = std::fs::metadata(&path)
                                .map(|metadata| {
                                    let format = path
                                        .extension()
                                        .and_then(|extension| extension.to_str())
                                        .unwrap_or_default()
                                        .to_ascii_uppercase();
                                    format!("{format} · {}", format_file_size(metadata.len()))
                                })
                                .unwrap_or_else(|_| "録画情報を読み込めません".into());
                            ui.label(
                                egui::RichText::new(recording_summary)
                                    .size(11.0)
                                    .color(TEXT_MUTED),
                            );
                            if ui.small_button("外部で開く").clicked() {
                                #[cfg(target_os = "macos")]
                                let command = "open";
                                #[cfg(target_os = "windows")]
                                let command = "explorer";
                                let _ = std::process::Command::new(command).arg(&path).spawn();
                            }
                        });
                    }
                });
                if let (Some(path), Some(image)) = (&self.recording_preview_path, &self.recording_preview_image) {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(path.file_name().unwrap_or_default().to_string_lossy()).strong());
                    if let Some(details) = &self.recording_preview_details {
                        ui.label(egui::RichText::new(details).size(11.0).color(TEXT_MUTED));
                    }
                    let playing = self
                        .recording_playback
                        .as_ref()
                        .is_some_and(|playback| playback.playing);
                    if ui.button(if playing { "⏸ 一時停止" } else { "▶ アプリ内再生" }).clicked() {
                        let next_playing;
                        if let Some(playback) = &mut self.recording_playback {
                            playback.playing = !playback.playing;
                            next_playing = playback.playing;
                            if playback.playing {
                                playback.frame_due = Instant::now();
                            }
                        } else {
                            self.recording_playback = Some(start_recording_playback(path.clone()));
                            next_playing = true;
                        }
                        self.status = recording_playback_status(next_playing).into();
                        ui.ctx().request_repaint_after(Duration::from_millis(10));
                    }
                    ui.add_space(6.0);
                    let response = Self::show_recording_preview_image(
                        ui,
                        image,
                        &mut self.recording_preview_texture,
                    );
                    if self.scroll_to_recording_preview {
                        self.scroll_to_recording_preview = false;
                        ui.scroll_to_rect(response.rect, Some(egui::Align::BOTTOM));
                    }
                }
            }
            ui.add_space(16.0);
            ui.separator();
            if self.evidence_files.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(34.0);
                    ui.label(egui::RichText::new("▦").size(42.0).color(ACCENT_BRIGHT));
                    ui.label(egui::RichText::new("まだ画像がありません").size(17.0).strong());
                    ui.label(egui::RichText::new("撮影した画像はここに並びます。保存先を変更した場合は設定を確認してください。").color(TEXT_MUTED));
                    ui.add_space(24.0);
                });
            } else {
                ui.label(format!("{} 枚の画像", self.evidence_files.len()));
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(570.0).show(ui, |ui| {
                    let files = self.evidence_files.clone();
                    for (index, path) in files.into_iter().enumerate() {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        let modified = std::fs::metadata(&path)
                            .and_then(|metadata| metadata.modified())
                            .ok()
                            .map(chrono::DateTime::<chrono::Local>::from)
                            .map(|date| date.format("%Y-%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| "日時不明".into());
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 135))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(18)))
                            .corner_radius(egui::CornerRadius::same(12))
                            .inner_margin(egui::Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(format!("{:03}", index + 1)).color(ACCENT_BRIGHT).strong());
                                    ui.vertical(|ui| {
                                        ui.label(egui::RichText::new(name.as_ref()).strong());
                                        ui.label(egui::RichText::new(modified).size(10.0).color(TEXT_MUTED));
                                    });
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.add(primary_button("画像を編集")).clicked() {
                                            self.open_evidence_image(self.evidence_files.clone(), index);
                                        }
                                    });
                                });
                            });
                        ui.add_space(5.0);
                    }
                });
            }
        });
    }

    fn show_settings_page(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("設定").size(24.0).strong());
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("MacShotを自分の撮影手順に合わせます。").color(TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.editing_settings_layout {
                    if ui.button("キャンセル").clicked() {
                        self.settings_layout = self.saved_settings_layout.clone();
                        self.editing_settings_layout = false;
                    }
                    if ui.add(primary_button("保存して固定")).clicked() {
                        let previous_saved_layout = self.saved_settings_layout.clone();
                        self.saved_settings_layout = self.settings_layout.clone();
                        match self.save_preferences() {
                            Ok(()) => {
                                self.editing_settings_layout = false;
                                self.settings_error = None;
                                self.status = "カード配置を保存して固定しました。".into();
                            }
                            Err(error) => {
                                self.saved_settings_layout = previous_saved_layout;
                                self.settings_error =
                                    Some(format!("カード配置を保存できません: {error}"));
                            }
                        }
                    }
                } else if ui.button("カード配置を調整").clicked() {
                    self.saved_settings_layout = self.settings_layout.clone();
                    self.editing_settings_layout = true;
                }
            });
        });
        if let Some(error) = &self.settings_error {
            ui.colored_label(egui::Color32::from_rgb(188, 51, 73), error);
        }
        ui.add_space(12.0);

        let layout = self.settings_layout.clone();
        let mut half_cards = Vec::new();
        for card in layout {
            if card.width == SettingsCardWidth::Half {
                half_cards.push(card);
                if half_cards.len() == 2 {
                    let row = std::mem::take(&mut half_cards);
                    self.show_settings_card_row(ui, &row);
                }
            } else {
                if !half_cards.is_empty() {
                    let row = std::mem::take(&mut half_cards);
                    self.show_settings_card_row(ui, &row);
                }
                self.show_settings_card(ui, card);
            }
        }
        if !half_cards.is_empty() {
            self.show_settings_card_row(ui, &half_cards);
        }
    }

    fn show_settings_card_row(&mut self, ui: &mut egui::Ui, cards: &[SettingsCardLayout]) {
        ui.columns(2, |columns| {
            for (index, card) in cards.iter().copied().enumerate() {
                self.show_settings_card(&mut columns[index], card);
            }
        });
        ui.add_space(12.0);
    }

    fn show_settings_card(&mut self, ui: &mut egui::Ui, card: SettingsCardLayout) {
        if card.id == SettingsCardId::Permissions {
            self.show_permission_panel(ui, card.padding);
            ui.add_space(12.0);
            return;
        }
        settings_card_frame(card.padding).show(ui, |ui| {
            if self.editing_settings_layout {
                self.show_settings_card_controls(ui, card.id);
            }
            match card.id {
                SettingsCardId::Display => self.show_display_settings_contents(ui),
                SettingsCardId::Shortcuts => self.show_shortcut_settings_contents(ui),
                SettingsCardId::Recording => self.show_recording_settings_contents(ui),
                SettingsCardId::Capture => self.show_capture_settings_contents(ui),
                SettingsCardId::Permissions => unreachable!(),
            }
        });
        ui.add_space(12.0);
    }

    fn show_settings_card_controls(&mut self, ui: &mut egui::Ui, id: SettingsCardId) {
        let Some(index) = self.settings_layout.iter().position(|card| card.id == id) else {
            return;
        };
        ui.push_id(id.setting(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("{} · 配置", id.label()))
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                if ui
                    .add_enabled(index > 0, egui::Button::new("↑ 上へ"))
                    .clicked()
                {
                    self.settings_layout.swap(index, index - 1);
                }
                if ui
                    .add_enabled(
                        index + 1 < self.settings_layout.len(),
                        egui::Button::new("↓ 下へ"),
                    )
                    .clicked()
                {
                    self.settings_layout.swap(index, index + 1);
                }
                if let Some(card) = self.settings_layout.iter_mut().find(|card| card.id == id) {
                    let width_label = match card.width {
                        SettingsCardWidth::Half => "幅: 半分",
                        SettingsCardWidth::Full => "幅: 全体",
                    };
                    if ui.button(width_label).clicked() {
                        card.width = match card.width {
                            SettingsCardWidth::Half => SettingsCardWidth::Full,
                            SettingsCardWidth::Full => SettingsCardWidth::Half,
                        };
                    }
                    let padding_label = match card.padding {
                        SettingsCardPadding::Compact => "余白: 小",
                        SettingsCardPadding::Standard => "余白: 中",
                        SettingsCardPadding::Spacious => "余白: 大",
                    };
                    if ui.button(padding_label).clicked() {
                        card.padding = match card.padding {
                            SettingsCardPadding::Compact => SettingsCardPadding::Standard,
                            SettingsCardPadding::Standard => SettingsCardPadding::Spacious,
                            SettingsCardPadding::Spacious => SettingsCardPadding::Compact,
                        };
                    }
                }
            });
            ui.separator();
        });
    }

    fn show_display_settings_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("表示").size(18.0).strong());
            ui.label(egui::RichText::new("文字サイズと画面の密度").color(TEXT_MUTED));
        });
        ui.add_space(8.0);
        let display_context = ui.ctx().clone();
        ui.columns(2, |columns| {
            columns[0].horizontal(|ui| {
                ui.label("表示量");
                let previous = self.display_density;
                egui::ComboBox::from_id_salt("display_density")
                    .selected_text(self.display_density.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.display_density,
                            DisplayDensity::Compact,
                            DisplayDensity::Compact.label(),
                        );
                        ui.selectable_value(
                            &mut self.display_density,
                            DisplayDensity::Standard,
                            DisplayDensity::Standard.label(),
                        );
                        ui.selectable_value(
                            &mut self.display_density,
                            DisplayDensity::Relaxed,
                            DisplayDensity::Relaxed.label(),
                        );
                    });
                if self.display_density != previous {
                    self.apply_display_settings(ui.ctx());
                    match self.save_preferences() {
                        Ok(()) => {
                            self.settings_error = None;
                            self.status = "表示設定を保存しました。".into();
                        }
                        Err(error) => {
                            self.settings_error = Some(format!("表示設定を保存できません: {error}"))
                        }
                    }
                }
            });
            let previous_font_size = self.font_size_percent;
            let mut save_font_size = false;
            columns[1].vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label("文字サイズ");
                    if ui.button("− 5%").clicked() {
                        self.font_size_percent = self.font_size_percent.saturating_sub(5).max(80);
                        save_font_size = true;
                    }
                    ui.label(egui::RichText::new(format!("{}%", self.font_size_percent)).strong());
                    if ui.button("＋ 5%").clicked() {
                        self.font_size_percent = (self.font_size_percent + 5).min(140);
                        save_font_size = true;
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for value in [80, 100, 120, 140] {
                        if ui
                            .selectable_label(self.font_size_percent == value, format!("{value}%"))
                            .clicked()
                        {
                            self.font_size_percent = value;
                            save_font_size = true;
                        }
                    }
                });
                let font_size = ui.add(
                    egui::Slider::new(&mut self.font_size_percent, 80..=140)
                        .step_by(5.0)
                        .show_value(false),
                );
                if font_size.drag_stopped()
                    || (font_size.changed() && !ui.input(|input| input.pointer.any_down()))
                {
                    save_font_size = true;
                }
            });
            if save_font_size && self.font_size_percent != previous_font_size {
                self.apply_display_settings(&display_context);
                match self.save_preferences() {
                    Ok(()) => {
                        self.settings_error = None;
                        self.status = "表示設定を保存しました。".into();
                    }
                    Err(error) => {
                        self.settings_error = Some(format!("表示設定を保存できません: {error}"));
                    }
                }
            }
        });
        ui.label(
            egui::RichText::new(
                "コンパクトにすると一画面に多く表示し、文字サイズを上げると全体を拡大します。",
            )
            .size(11.0)
            .color(TEXT_MUTED),
        );
    }

    fn show_shortcut_settings_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("ショートカット").size(18.0).strong());
        ui.label(
            egui::RichText::new("どのアプリを使っていても呼び出せます。設定後すぐに反映されます.")
                .color(TEXT_MUTED),
        );
        ui.add_space(12.0);
        egui::Grid::new("shortcut_settings")
            .num_columns(2)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label("スクリーンショット");
                ui.add(
                    egui::TextEdit::singleline(&mut self.capture_hotkey_text)
                        .desired_width(220.0)
                        .hint_text("Alt+Shift+KeyS"),
                );
                ui.end_row();
                ui.label("録画の開始／終了");
                ui.add(
                    egui::TextEdit::singleline(&mut self.recording_hotkey_text)
                        .desired_width(220.0)
                        .hint_text("Alt+Shift+KeyR"),
                );
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("例: Alt+Shift+KeyS、Control+Alt+KeyF、Super+Shift+Key4")
                .size(11.0)
                .color(TEXT_MUTED),
        );
        ui.label(
            egui::RichText::new("macOSのAltはOptionキーです。WindowsではAltキーとして動作します。")
                .size(11.0)
                .color(TEXT_MUTED),
        );
        ui.add_space(10.0);
        if ui.add(primary_button("設定を保存して反映")).clicked() {
            self.apply_hotkey_settings();
        }
    }

    fn recording_estimate_dimensions(&self) -> (u32, u32, bool) {
        let approximate = matches!(
            self.capture_target,
            CaptureTarget::ActiveWindow | CaptureTarget::Window(_)
        );
        let monitor = match self.capture_target {
            CaptureTarget::Display(index) => self.monitors.get(index),
            _ => self
                .monitors
                .iter()
                .find(|monitor| monitor.is_primary().unwrap_or(false))
                .or_else(|| self.monitors.first()),
        };
        let Some(monitor) = monitor else {
            return (1920, 1080, true);
        };
        let (Ok(width), Ok(height)) = (monitor.width(), monitor.height()) else {
            return (1920, 1080, true);
        };
        let scale = if cfg!(target_os = "macos") {
            monitor.scale_factor().unwrap_or(1.0).max(1.0)
        } else {
            1.0
        };
        (
            (width as f64 * f64::from(scale)).round().max(1.0) as u32,
            (height as f64 * f64::from(scale)).round().max(1.0) as u32,
            approximate,
        )
    }

    fn show_recording_size_estimate(&self, ui: &mut egui::Ui) {
        let (source_width, source_height, approximate) = self.recording_estimate_dimensions();
        let (width, height) = self.recording_quality.resolution.output_dimensions(
            source_width,
            source_height,
            self.recording_format == RecordingFormat::Mp4,
        );
        let estimate = estimate_recording_size_per_minute(
            self.recording_format,
            self.recording_quality,
            width,
            height,
        );
        ui.add_space(4.0);
        ui.label(egui::RichText::new("予想ファイルサイズ（1分録画）").strong());
        match self.recording_format {
            RecordingFormat::Mp4 => {
                ui.label(format!("約 {}", format_estimated_size(estimate.high_bytes)));
                ui.label(
                    egui::RichText::new("映像ビットレート・AAC音声・コンテナ分を含む目安です。")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
            }
            RecordingFormat::Gif => {
                ui.label(format!(
                    "約 {}〜{}",
                    format_estimated_size(estimate.low_bytes),
                    format_estimated_size(estimate.high_bytes)
                ));
                ui.label(
                    egui::RichText::new(
                        "画面の動きや色で大きく変わります。解像度とfpsからの概算です。",
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
            }
        }
        let dimensions_note = if approximate {
            format!("{}×{} px（メイン画面を仮定）", width, height)
        } else {
            format!("{}×{} px", width, height)
        };
        ui.label(
            egui::RichText::new(dimensions_note)
                .size(10.0)
                .color(TEXT_MUTED),
        );
    }

    fn show_recording_settings_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("録画").size(18.0).strong());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("保存形式");
            let previous = self.recording_format;
            egui::ComboBox::from_id_salt("recording_format")
                .selected_text(self.recording_format.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.recording_format, RecordingFormat::Gif, "GIF");
                    ui.selectable_value(
                        &mut self.recording_format,
                        RecordingFormat::Mp4,
                        "MP4（H.264）",
                    );
                });
            if self.recording_format != previous {
                if let Err(error) = self.save_preferences() {
                    self.settings_error = Some(format!("録画形式を保存できません: {error}"));
                } else {
                    self.settings_error = None;
                    self.status =
                        format!("録画形式を{}に変更しました", self.recording_format.label());
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("録画解像度");
            let previous = self.recording_quality.resolution;
            egui::ComboBox::from_id_salt("recording_resolution")
                .selected_text(self.recording_quality.resolution.label())
                .show_ui(ui, |ui| {
                    for resolution in RecordingResolution::ALL {
                        ui.selectable_value(
                            &mut self.recording_quality.resolution,
                            resolution,
                            resolution.label(),
                        );
                    }
                });
            if self.recording_quality.resolution != previous {
                if let Err(error) = self.save_preferences() {
                    self.settings_error = Some(format!("録画解像度を保存できません: {error}"));
                    self.recording_quality.resolution = previous;
                } else {
                    self.settings_error = None;
                    self.status = format!(
                        "録画解像度を{}に変更しました",
                        self.recording_quality.resolution.label()
                    );
                }
            }
        });
        ui.label(
            egui::RichText::new("選んだ高さを上限にして縮小します。元の画面より大きくはしません。")
                .size(11.0)
                .color(TEXT_MUTED),
        );
        match self.recording_format {
            RecordingFormat::Gif => {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("GIFの画質").strong());
                ui.label(
                    egui::RichText::new("色の再現を上げるほど、変換に時間がかかることがあります。")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                let quality = ui.add(
                    egui::Slider::new(&mut self.recording_quality.gif_quality_percent, 10..=100)
                        .suffix("%")
                        .text("色の再現"),
                );
                self.save_settings_after_slider_change(&quality, ui, "録画設定を保存しました。");
                let frame_rate = ui.add(
                    egui::Slider::new(&mut self.recording_quality.gif_frame_rate, 1..=30)
                        .suffix(" fps")
                        .text("なめらかさ"),
                );
                self.save_settings_after_slider_change(&frame_rate, ui, "録画設定を保存しました。");
                ui.label(
                    egui::RichText::new("GIFは音声なしです。")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                self.show_recording_size_estimate(ui);
            }
            RecordingFormat::Mp4 => {
                ui.add_space(6.0);
                let bitrate = ui.add(
                    egui::Slider::new(&mut self.recording_quality.mp4_bitrate_mbps, 2..=30)
                        .suffix(" Mbps")
                        .text("画質・容量"),
                );
                self.save_settings_after_slider_change(&bitrate, ui, "録画設定を保存しました。");
                let frame_rate = ui.add(
                    egui::Slider::new(&mut self.recording_quality.mp4_frame_rate, 5..=60)
                        .suffix(" fps")
                        .text("なめらかさ"),
                );
                self.save_settings_after_slider_change(&frame_rate, ui, "録画設定を保存しました。");
                ui.label(
                    egui::RichText::new(
                        "システム音声を録音し、マイク許可時はマイク音声も追加します。",
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
                self.show_recording_size_estimate(ui);
            }
        }
        if cfg!(target_os = "macos") {
            ui.add_space(8.0);
            if ui.button("macOS標準の撮影・録画を開く").clicked() {
                self.open_native_screenshot();
            }
        }
    }

    fn show_capture_settings_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("撮影の動作").size(18.0).strong());
        ui.label(
            egui::RichText::new(
                "撮影対象や画像に記録する情報を調整できます。設定は次回起動後も引き継がれます。",
            )
            .color(TEXT_MUTED),
        );
        ui.add_space(12.0);
        ui.columns(2, |columns| {
            glass_frame().show(&mut columns[0], |ui| {
                ui.label(egui::RichText::new("撮影とファイル名").size(16.0).strong());
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("既定の撮影対象")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                egui::ComboBox::from_id_salt("settings_capture_target")
                    .selected_text(self.target_caption())
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.capture_target,
                            CaptureTarget::ActiveWindow,
                            "アクティブウィンドウ",
                        );
                        ui.selectable_value(
                            &mut self.capture_target,
                            CaptureTarget::PrimaryDisplay,
                            "デスクトップ（メイン画面）",
                        );
                        ui.selectable_value(
                            &mut self.capture_target,
                            CaptureTarget::AllDisplays,
                            "全画面（全ディスプレイ）",
                        );
                        for (index, monitor) in self.monitors.iter().enumerate() {
                            let name = monitor
                                .name()
                                .unwrap_or_else(|_| format!("画面 {}", index + 1));
                            ui.selectable_value(
                                &mut self.capture_target,
                                CaptureTarget::Display(index),
                                format!("画面全体 — {name}"),
                            );
                        }
                        for window in &self.windows {
                            ui.selectable_value(
                                &mut self.capture_target,
                                CaptureTarget::Window(window.id),
                                format!("ウィンドウ — {}", window.title),
                            );
                        }
                    });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("ファイル名の先頭")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                ui.add(egui::TextEdit::singleline(&mut self.filename_prefix).desired_width(240.0));
                ui.add_space(8.0);
                ui.checkbox(&mut self.timestamp_enabled, "画像に撮影日時を表示");
                ui.checkbox(&mut self.show_capture_notice, "撮影後に完了小窓を表示");
            });
            glass_frame().show(&mut columns[1], |ui| {
                ui.label(egui::RichText::new("注釈").size(16.0).strong());
                ui.add_space(8.0);
                ui.label(egui::RichText::new("色").size(11.0).color(TEXT_MUTED));
                egui::ComboBox::from_id_salt("annotation_color_setting")
                    .selected_text(match self.annotation_color.as_str() {
                        "yellow" => "黄色",
                        "blue" => "青色",
                        _ => "赤色",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.annotation_color, "red".into(), "赤色");
                        ui.selectable_value(&mut self.annotation_color, "yellow".into(), "黄色");
                        ui.selectable_value(&mut self.annotation_color, "blue".into(), "青色");
                    });
                ui.add_space(10.0);
                ui.add(egui::Slider::new(&mut self.annotation_width, 1.0..=8.0).text("線の太さ"));
                ui.add_space(18.0);
                let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                ui.label(egui::RichText::new("保存先").size(11.0).color(TEXT_MUTED));
                ui.label(
                    egui::RichText::new(folder.display().to_string())
                        .size(12.0)
                        .monospace(),
                );
                ui.add_space(6.0);
                if ui.button("保存先を変更").clicked() {
                    self.choose_folder();
                }
            });
        });
        ui.add_space(10.0);
        if ui.add(primary_button("撮影設定を保存")).clicked() {
            match self.save_preferences() {
                Ok(()) => self.status = "撮影設定を保存しました。".into(),
                Err(error) => self.settings_error = Some(format!("設定を保存できません: {error}")),
            }
        }
    }

    fn show_permission_panel(&mut self, ui: &mut egui::Ui, padding: SettingsCardPadding) {
        settings_card_frame(padding).show(ui, |ui| {
            if self.editing_settings_layout {
                self.show_settings_card_controls(ui, SettingsCardId::Permissions);
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("権限とデバイス").size(18.0).strong());
                if ui.button("↻  権限状態を更新").clicked() {
                    self.refresh_permission_status();
                }
            });
            ui.add_space(6.0);

            #[cfg(target_os = "macos")]
            if let Some(app_path) = current_macos_app_bundle_path() {
                let running_copy = format!(
                    "起動中のMacShot: v{} · {}",
                    env!("CARGO_PKG_VERSION"),
                    app_path.display()
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(running_copy)
                            .size(11.0)
                            .color(TEXT_MUTED)
                            .monospace(),
                    )
                    .selectable(true),
                );
                ui.add_space(4.0);
            }

            let screen_granted = screen_recording_access_granted();
            let microphone = microphone_permission_label();
            #[cfg(target_os = "macos")]
            let ctx = ui.ctx().clone();
            #[cfg(target_os = "macos")]
            let microphone_state = microphone_permission();
            ui.columns(2, |columns| {
                columns[0].vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("画面収録・システム音声");
                        if screen_granted {
                            ui.colored_label(egui::Color32::from_rgb(38, 132, 88), "許可済み");
                        } else {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 105, 32),
                                "未許可、またはmacOSへの反映待ち",
                            );
                        }
                    });
                    #[cfg(target_os = "macos")]
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("画面・音声の設定を開く").clicked() {
                            self.open_screen_recording_settings();
                        }
                        if ui.button("FinderでMacShotを表示").clicked() {
                            self.reveal_macshot_app();
                        }
                    });
                    #[cfg(target_os = "windows")]
                    ui.label(egui::RichText::new("Windowsでは個別の画面収録許可は不要です").color(TEXT_MUTED));
                });
                columns[1].vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("マイク");
                        #[cfg(target_os = "macos")]
                        {
                            let permitted = microphone_state == MicrophonePermission::Authorized;
                            ui.colored_label(
                                if permitted {
                                    egui::Color32::from_rgb(38, 132, 88)
                                } else {
                                    egui::Color32::from_rgb(190, 105, 32)
                                },
                                microphone,
                            );
                        }
                        #[cfg(target_os = "windows")]
                        ui.label(egui::RichText::new(microphone).color(TEXT_MUTED));
                        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                        ui.label(egui::RichText::new(microphone).color(TEXT_MUTED));
                        #[cfg(target_os = "macos")]
                        if let Some(label) = microphone_permission_action_label(microphone_state)
                            && ui.button(label).clicked()
                        {
                            if microphone_state == MicrophonePermission::Denied {
                                self.open_microphone_settings();
                            } else {
                                self.request_microphone_permission(&ctx);
                            }
                        }
                        #[cfg(target_os = "macos")]
                        if microphone_state == MicrophonePermission::Denied
                            && ui.button("拒否記録を解除").clicked()
                        {
                            self.microphone_reset_confirmation = true;
                        }
                        if ui.button("マイク設定を開く").clicked() {
                            self.open_microphone_settings();
                        }
                    });

                    #[cfg(target_os = "macos")]
                    ui.horizontal_wrapped(|ui| {
                        let testing = self.microphone_test_rx.is_some();
                        if ui
                            .add_enabled(
                                microphone_state == MicrophonePermission::Authorized && !testing,
                                egui::Button::new(if testing { "マイクテスト中…" } else { "マイク入力をテスト（5秒）" }),
                            )
                            .clicked()
                        {
                            self.start_microphone_test();
                        }
                        if testing {
                            ui.add(egui::ProgressBar::new(self.microphone_test_peak.clamp(0.0, 1.0))
                                .desired_width(160.0));
                        }
                        if let Some(message) = &self.microphone_test_message {
                            ui.label(egui::RichText::new(message).color(TEXT_MUTED));
                        }
                    });
                });
            });

            #[cfg(target_os = "macos")]
            ui.label(
                egui::RichText::new("画面収録の一覧にMacShotがない場合は「＋」から追加できます。マイク一覧には追加ボタンがありません。MacShotから許可を要求すると一覧に表示されます。拒否済みで一覧にもない場合は「拒否記録を解除」を使ってください。")
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
            #[cfg(target_os = "windows")]
            ui.label(
                egui::RichText::new("MP4は既定の再生デバイスとマイクを使います。Windowsのマイク設定でデスクトップアプリへのアクセスも確認してください。")
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
        });

        #[cfg(target_os = "macos")]
        if self.microphone_reset_confirmation {
            let mut open = true;
            let mut confirmed = false;
            let mut cancelled = false;
            egui::Window::new("マイクの拒否記録を解除")
                .id(egui::Id::new("microphone_permission_reset_confirmation"))
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.label("MacShotだけのマイク拒否記録を解除して、macOSに許可を確認します。解除してもマイクは許可されません。確認画面が表示された場合は、そこで許可を選んでください。");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(primary_button("解除して許可を確認")).clicked() {
                            confirmed = true;
                        }
                        if ui.button("キャンセル").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if cancelled {
                open = false;
            }
            self.microphone_reset_confirmation = open;
            if confirmed {
                self.microphone_reset_confirmation = false;
                self.reset_and_request_microphone_permission(&ui.ctx().clone());
            }
        }
    }
}

fn compare_empty_state(ui: &mut egui::Ui, message: &str) {
    ui.set_min_height(310.0);
    ui.vertical_centered(|ui| {
        ui.add_space(95.0);
        ui.label(egui::RichText::new("⇄").size(42.0).color(ACCENT_BRIGHT));
        ui.label(egui::RichText::new(message).color(TEXT_MUTED));
    });
}

impl eframe::App for MacShot {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_display_settings(ctx);
        self.advance_recording_playback(ctx);
        #[cfg(target_os = "macos")]
        self.handle_microphone_test_events(ctx);
        #[cfg(target_os = "macos")]
        if let Ok(mut result) = self.microphone_permission_result.lock()
            && let Some(granted) = result.take()
        {
            self.status = microphone_permission_result_message(granted).into();
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            if should_defer_quit_until_recording_finishes(self.recording, self.recording_starting) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quit_after_recording = true;
                self.stop_recording();
            } else if cfg!(target_os = "windows") {
                std::process::exit(0);
            }
        }
        if cfg!(target_os = "macos")
            && screen_recording_access_granted()
            && (self.status.starts_with("画面収録の許可を設定してください")
                || self.status.starts_with("macOSから画面収録を拒否されました")
                || self.status.starts_with("画面収録の許可設定を開きました")
                || self.status.starts_with("設定でMacShotをオンにしてください"))
        {
            self.status =
                "画面収録の許可を確認しました。反映のためMacShotを再起動してください。".into();
        }
        self.initialize_hotkey(ctx);
        self.initialize_tray(ctx);
        self.handle_hotkey_requests(ctx);
        self.handle_tray_actions(ctx);
        self.handle_capture_results(ctx);
        #[cfg(target_os = "windows")]
        self.handle_region_snapshot(ctx);
        self.handle_recording_events(ctx);
        #[cfg(target_os = "macos")]
        set_standard_quit_items_enabled(!self.recording && !self.recording_starting);
        self.update_tray_recording_status();
        if self.recording || self.recording_starting {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.region_selecting {
            self.show_region_overlay(ctx);
            return;
        }

        self.show_header(ctx);

        egui::TopBottomPanel::bottom("macshot_status")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(250, 251, 255, 235))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        egui::Color32::from_black_alpha(20),
                    ))
                    .inner_margin(egui::Margin::symmetric(20, 9)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("●").color(ACCENT_BRIGHT));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.status)
                                .size(11.0)
                                .color(TEXT_MUTED),
                        )
                        .wrap(),
                    );
                    if let Some(started_at) = self.recording_started_at {
                        let elapsed = started_at.elapsed().as_secs();
                        ui.label(
                            egui::RichText::new(format!(
                                "● REC {:02}:{:02}",
                                elapsed / 60,
                                elapsed % 60
                            ))
                            .color(egui::Color32::from_rgb(198, 53, 78))
                            .strong(),
                        );
                    } else if self.recording_starting {
                        ui.label(
                            egui::RichText::new("● REC 準備中")
                                .color(egui::Color32::from_rgb(198, 53, 78)),
                        );
                    }
                    if let Some(folder) = &self.save_folder {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(folder.display().to_string())
                                    .size(10.0)
                                    .color(TEXT_MUTED),
                            );
                        });
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(238, 242, 250))
                    .inner_margin(egui::Margin::symmetric(20, 16)),
            )
            .show(ctx, |ui| {
                paint_glass_backdrop(ui);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.page {
                        Page::Capture => self.show_capture_page(ui, ctx),
                        Page::Evidence => self.show_evidence_page(ui),
                        Page::Compare => self.show_compare_page(ui),
                        Page::Settings => self.show_settings_page(ui),
                    });
            });

        if let Some(notice) = &self.capture_notice {
            let mut open = true;
            egui::Window::new("撮影完了")
                .id(egui::Id::new("capture_complete_notice"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-18.0, 74.0))
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new("●  撮影しました")
                            .size(15.0)
                            .strong()
                            .color(ACCENT_BRIGHT),
                    );
                    ui.label(format!("{}枚目  ·  {}", notice.sequence, notice.timestamp));
                    ui.label(capture_size_label(notice.width, notice.height));
                    ui.label(notice.path.display().to_string());
                    ui.label(format!(
                        "{} で続けて撮影できます",
                        hotkey_badge_text(&self.capture_hotkey_text)
                    ));
                });
            if !open {
                self.capture_notice = None;
            }
        }
        if let Some((path, timestamp, warning)) = &self.recording_notice {
            let mut open = true;
            egui::Window::new("録画を保存しました")
                .id(egui::Id::new("recording_complete_notice"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-18.0, 236.0))
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new("●  録画を保存しました")
                            .size(15.0)
                            .strong()
                            .color(ACCENT_BRIGHT),
                    );
                    ui.label(timestamp);
                    ui.label(path.display().to_string());
                    if let Some(warning) = warning {
                        ui.label(warning);
                    }
                });
            if !open {
                self.recording_notice = None;
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 640.0])
            .with_transparent(cfg!(target_os = "macos"))
            .with_icon(application_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "MacShot",
        options,
        Box::new(|creation| {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "noto_sans_jp".into(),
                std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                    "../assets/NotoSansJP-Medium.ttf"
                ))),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "noto_sans_jp".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "noto_sans_jp".into());
            creation.egui_ctx.set_fonts(fonts);
            configure_macshot_theme(&creation.egui_ctx);
            creation
                .egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Light));
            Ok(Box::<MacShot>::default())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bundle_path_finds_the_running_app_container() {
        assert_eq!(
            app_bundle_path_from_executable(Path::new(
                "/Applications/MacShot.app/Contents/MacOS/MacShot",
            )),
            Some(PathBuf::from("/Applications/MacShot.app"))
        );
        assert_eq!(
            app_bundle_path_from_executable(Path::new("/tmp/macshot")),
            None
        );
    }

    #[test]
    fn recording_preview_display_size_stays_useful_at_the_bottom_of_the_page() {
        assert_eq!(
            recording_preview_display_size(320, 180, 2_000.0),
            egui::vec2(320.0, 180.0)
        );
        assert_eq!(
            recording_preview_display_size(1_920, 1_080, 2_000.0),
            egui::vec2(720.0, 405.0)
        );
        let portrait = recording_preview_display_size(400, 800, 640.0);
        assert!((portrait.x - 202.5).abs() < 0.01);
        assert!((portrait.y - 405.0).abs() < 0.01);
        assert_eq!(
            recording_preview_display_size(0, 180, 640.0),
            egui::Vec2::ZERO
        );
    }

    #[test]
    fn recording_preview_widget_keeps_its_size_when_vertical_space_is_low() {
        let context = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 120.0),
            )),
            ..Default::default()
        };
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            320,
            180,
            Rgba([40_u8, 160, 220, 255]),
        ));
        let mut texture = None;
        let mut remaining_height = None;
        let mut rendered_size = None;

        let _ = context.run(input, |context| {
            egui::CentralPanel::default().show(context, |ui| {
                ui.add_space(75.0);
                remaining_height = Some(ui.available_height());
                rendered_size = Some(
                    MacShot::show_recording_preview_image(ui, &image, &mut texture)
                        .rect
                        .size(),
                );
            });
        });

        assert!(remaining_height.unwrap() < 30.0);
        assert_eq!(rendered_size.unwrap(), egui::vec2(320.0, 180.0));
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after UNIX epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("macshot-{label}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recording_time_label_formats_elapsed_time() {
        assert_eq!(recording_time_label(Duration::from_secs(0)), "00:00");
        assert_eq!(recording_time_label(Duration::from_secs(9)), "00:09");
        assert_eq!(recording_time_label(Duration::from_secs(61)), "01:01");
        assert_eq!(recording_time_label(Duration::from_secs(3_600)), "60:00");
    }

    #[test]
    fn recording_playback_status_tracks_play_and_pause() {
        assert_eq!(
            recording_playback_status(true),
            "録画をアプリ内で再生しています（映像のみ）。"
        );
        assert_eq!(recording_playback_status(false), "録画を一時停止しました。");
    }

    #[test]
    fn legacy_video_quality_presets_keep_their_values() {
        assert_eq!(VideoQuality::Standard.bitrate(), 6_000_000);
        assert_eq!(VideoQuality::Standard.frame_rate(), 10.0);
        assert_eq!(VideoQuality::High.bitrate(), 12_000_000);
        assert_eq!(VideoQuality::High.frame_rate(), 15.0);
        assert_eq!(VideoQuality::Best.bitrate(), 20_000_000);
        assert_eq!(VideoQuality::Best.frame_rate(), 30.0);
        assert_eq!(VideoQuality::from_setting("high"), VideoQuality::High);
        assert_eq!(VideoQuality::from_setting("best"), VideoQuality::Best);
        assert_eq!(
            VideoQuality::from_setting("unknown"),
            VideoQuality::Standard
        );
    }

    #[test]
    fn recording_quality_loads_independent_values_and_migrates_old_presets() {
        let quality =
            RecordingQuality::from_settings(VideoQuality::High, "18", "48", "92", "24", "720p");
        assert_eq!(quality.mp4_bitrate_mbps, 18);
        assert_eq!(quality.mp4_frame_rate, 48);
        assert_eq!(quality.gif_quality_percent, 92);
        assert_eq!(quality.gif_frame_rate, 24);
        assert_eq!(quality.resolution, RecordingResolution::P720);
        assert_eq!(quality.mp4_bitrate(), 18_000_000);
        assert_eq!(quality.mp4_frame_interval().as_millis(), 20);
        assert_eq!(quality.gif_frame_interval().as_millis(), 41);
        assert_eq!(quality.gif_quantizer_speed(), 4);

        let migrated =
            RecordingQuality::from_settings(VideoQuality::High, "", "bad", "", "", "unknown");
        assert_eq!(migrated.mp4_bitrate_mbps, 12);
        assert_eq!(migrated.mp4_frame_rate, 15);
        assert_eq!(migrated.gif_quality_percent, 70);
        assert_eq!(migrated.gif_frame_rate, 10);
        assert_eq!(migrated.resolution, RecordingResolution::Original);
        assert_eq!(
            RecordingQuality::from_settings(
                VideoQuality::Standard,
                "99",
                "1",
                "0",
                "90",
                "original",
            ),
            RecordingQuality {
                mp4_bitrate_mbps: 30,
                mp4_frame_rate: 5,
                gif_quality_percent: 10,
                gif_frame_rate: 30,
                resolution: RecordingResolution::Original,
            }
        );
    }

    #[test]
    fn recording_size_estimate_tracks_mp4_bitrate_and_gif_resolution_and_fps() {
        let quality = RecordingQuality::from_settings(
            VideoQuality::Standard,
            "10",
            "30",
            "70",
            "10",
            "original",
        );
        let mp4 = estimate_recording_size_per_minute(RecordingFormat::Mp4, quality, 1920, 1080);
        assert_eq!(mp4.low_bytes, mp4.high_bytes);
        assert!(mp4.high_bytes > 75_000_000.0 && mp4.high_bytes < 80_000_000.0);

        let gif = estimate_recording_size_per_minute(RecordingFormat::Gif, quality, 1920, 1080);
        assert!(gif.low_bytes > 0.0);
        assert!(gif.high_bytes > gif.low_bytes);
        let faster = RecordingQuality {
            gif_frame_rate: 20,
            ..quality
        };
        let faster_gif =
            estimate_recording_size_per_minute(RecordingFormat::Gif, faster, 1920, 1080);
        assert!((faster_gif.high_bytes / gif.high_bytes - 2.0).abs() < 0.001);
        let smaller = estimate_recording_size_per_minute(RecordingFormat::Gif, quality, 1280, 720);
        assert!(smaller.high_bytes < gif.high_bytes);
        assert_eq!(format_estimated_size(1_500_000_000.0), "1.5 GB");
        assert_eq!(
            RecordingResolution::P720.output_dimensions(3840, 2160, true),
            (1280, 720)
        );
        assert_eq!(
            RecordingResolution::P2160.output_dimensions(1920, 1080, false),
            (1920, 1080)
        );
    }

    #[test]
    fn display_preferences_have_safe_defaults_and_bounded_font_size() {
        assert_eq!(
            DisplayDensity::from_setting("compact"),
            DisplayDensity::Compact
        );
        assert_eq!(DisplayDensity::from_setting("relaxed").setting(), "relaxed");
        assert_eq!(
            DisplayDensity::from_setting("unknown"),
            DisplayDensity::Standard
        );
        assert!(DisplayDensity::Compact.spacing_scale() < DisplayDensity::Standard.spacing_scale());
        assert!(DisplayDensity::Relaxed.spacing_scale() > DisplayDensity::Standard.spacing_scale());
        assert_eq!(parse_font_size_percent("125"), 125);
        assert_eq!(parse_font_size_percent("bad"), 100);
        assert_eq!(parse_font_size_percent("10"), 80);
        assert_eq!(parse_font_size_percent("200"), 140);
    }

    #[test]
    fn settings_card_layout_round_trips_order_width_and_padding() {
        let mut layout = default_settings_layout();
        assert_eq!(
            layout,
            vec![
                SettingsCardLayout {
                    id: SettingsCardId::Recording,
                    width: SettingsCardWidth::Half,
                    padding: SettingsCardPadding::Standard,
                },
                SettingsCardLayout {
                    id: SettingsCardId::Capture,
                    width: SettingsCardWidth::Half,
                    padding: SettingsCardPadding::Standard,
                },
                SettingsCardLayout {
                    id: SettingsCardId::Shortcuts,
                    width: SettingsCardWidth::Half,
                    padding: SettingsCardPadding::Standard,
                },
                SettingsCardLayout {
                    id: SettingsCardId::Permissions,
                    width: SettingsCardWidth::Half,
                    padding: SettingsCardPadding::Standard,
                },
                SettingsCardLayout {
                    id: SettingsCardId::Display,
                    width: SettingsCardWidth::Full,
                    padding: SettingsCardPadding::Compact,
                },
            ]
        );
        layout.swap(0, 2);
        layout[0].width = SettingsCardWidth::Half;
        layout[0].padding = SettingsCardPadding::Compact;
        layout[1].padding = SettingsCardPadding::Spacious;
        let serialized = serialize_settings_layout(&layout);
        assert_eq!(parse_settings_layout(&serialized), layout);

        let partial = parse_settings_layout(
            "recording:half:compact;recording:full:spacious;unknown:half:compact",
        );
        assert_eq!(partial.len(), SettingsCardId::ALL.len());
        assert_eq!(partial[0].id, SettingsCardId::Recording);
        assert_eq!(partial[0].width, SettingsCardWidth::Half);
        assert_eq!(partial[0].padding, SettingsCardPadding::Compact);
        assert!(
            SettingsCardId::ALL
                .iter()
                .all(|id| partial.iter().any(|card| card.id == *id))
        );
    }

    #[test]
    fn mp4_video_timestamps_preserve_real_capture_delays() {
        assert_eq!(next_video_start_ms(350, 100, 100), 350);
        assert_eq!(next_video_start_ms(150, 100, 100), 200);
        assert_eq!(next_video_start_ms(u64::MAX, u64::MAX - 50, 100), u64::MAX);
    }

    #[test]
    fn continuous_capture_status_shows_saved_count_and_completion() {
        assert_eq!(
            continuous_capture_status_label(true, true, false, 0, 0),
            "連続撮影中 — 保存待ち"
        );
        assert_eq!(
            continuous_capture_status_label(true, true, true, 1, 0),
            "連続撮影中 — 撮影中・保存1枚・失敗0件"
        );
        assert_eq!(
            continuous_capture_status_label(true, true, false, 2, 0),
            "連続撮影中 — 2枚保存済み"
        );
        assert_eq!(
            continuous_capture_status_label(true, true, false, 1, 2),
            "連続撮影中 — 保存1枚・失敗2件"
        );
        assert_eq!(
            continuous_capture_status_label(false, true, true, 2, 1),
            "撮影終了待ち — 保存2枚・失敗1件"
        );
        assert_eq!(
            continuous_capture_status_label(false, false, false, 2, 1),
            "前回の連続撮影 — 2枚保存・失敗1件"
        );
        assert_eq!(
            continuous_capture_status_label(false, false, false, 0, 0),
            "連続撮影 — 停止中"
        );
        assert_eq!(
            continuous_capture_status_label(false, false, false, 0, 1),
            "前回の連続撮影 — 保存0枚・失敗1件"
        );
    }

    #[test]
    fn continuous_capture_menu_bar_title_confirms_each_saved_image() {
        assert_eq!(continuous_capture_title(false, false, 0, 0), None);
        assert_eq!(
            continuous_capture_title(true, false, 0, 0),
            Some("連続".into())
        );
        assert_eq!(
            continuous_capture_title(true, true, 0, 0),
            Some("撮影中".into())
        );
        assert_eq!(
            continuous_capture_title(true, false, 3, 0),
            Some("✓3".into())
        );
        assert_eq!(
            continuous_capture_title(true, false, 0, 1),
            Some("!1".into())
        );
        assert_eq!(
            continuous_capture_title(true, false, 3, 1),
            Some("✓3!1".into())
        );
    }

    #[test]
    fn menu_bar_camera_icon_stays_compact_and_changes_when_recording() {
        let idle = tray_icon_pixels(false);
        let recording = tray_icon_pixels(true);
        let occupied_rows: Vec<usize> = (0..24)
            .filter(|y| (0..24).any(|x| idle[(y * 24 + x) * 4 + 3] != 0))
            .collect();
        assert_eq!(occupied_rows.first(), Some(&5));
        assert_eq!(occupied_rows.last(), Some(&18));
        assert!(occupied_rows.len() <= 14);
        assert_ne!(idle, recording);
        assert_eq!(idle[(13 * 24 + 13) * 4 + 3], 0, "lens center stays open");
        assert_ne!(idle[(13 * 24 + 16) * 4 + 3], 0, "lens ring stays visible");
    }

    #[test]
    fn recording_menu_toggles_when_idle_and_sends_stop_when_active() {
        let (action_tx, action_rx) = mpsc::channel();
        let no_stop = Arc::new(Mutex::new(None));
        dispatch_recording_menu_action(&no_stop, &action_tx);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(TrayAction::ToggleRecording)
        ));

        let (stop_tx, stop_rx) = mpsc::channel();
        let active_stop = Arc::new(Mutex::new(Some(stop_tx)));
        dispatch_recording_menu_action(&active_stop, &action_tx);
        assert!(stop_rx.try_recv().is_ok());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn permission_banner_is_hidden_while_editing_existing_images() {
        assert!(should_show_screen_permission_banner(false, false));
        assert!(!should_show_screen_permission_banner(true, false));
        assert!(!should_show_screen_permission_banner(false, true));
    }

    #[test]
    fn evidence_identity_accepts_hyphenated_prefixes_and_edit_suffixes() {
        let original = Path::new("evidence-final-20261025-123456-789-012.png");
        let marked = Path::new("evidence-final-20261025-123456-789-012-marked.png");
        let crop = Path::new("evidence-final-20261025-123456-789-012-crop.png");

        let identity = ("20261025-123456-789".to_string(), 12);
        assert_eq!(parse_evidence_identity(original), Some(identity.clone()));
        assert_eq!(parse_evidence_identity(marked), Some(identity.clone()));
        assert_eq!(parse_evidence_identity(crop), Some(identity));
    }

    #[test]
    fn filename_prefix_keeps_unicode_and_replaces_path_separators() {
        assert_eq!(sanitize_filename_prefix("証跡_案件-2026"), "証跡_案件-2026");
        assert_eq!(sanitize_filename_prefix("証跡/案件"), "証跡_案件");
        assert_eq!(sanitize_filename_prefix("///"), "evidence");
    }

    #[test]
    fn evidence_sort_keeps_original_before_its_edits() {
        let original = evidence_sort_key(Path::new("capture-20261025-123456-789-012.png"));
        let crop = evidence_sort_key(Path::new("capture-20261025-123456-789-012-crop.png"));
        let repeated_crop =
            evidence_sort_key(Path::new("capture-20261025-123456-789-012-crop-copy-2.png"));
        let marked = evidence_sort_key(Path::new("capture-20261025-123456-789-012-marked.png"));

        assert_eq!(original.0, crop.0);
        assert_eq!(original.1, crop.1);
        assert!(original.2 < crop.2);
        assert_eq!(crop.0, repeated_crop.0);
        assert_eq!(crop.1, repeated_crop.1);
        assert_eq!(crop.2, repeated_crop.2);
        assert!(original.2 < marked.2);
    }

    #[test]
    fn evidence_scan_filters_supported_images_and_orders_newest_first() {
        let directory = TestDirectory::new("evidence-scan");
        let names = [
            "capture-20261025-120000-000-001.png",
            "capture-20261025-120000-000-001-crop.png",
            "capture-20261025-120000-000-001-marked.png",
            "capture-20261025-120001-000-002.JPEG",
            "ignore.txt",
            "ignore.gif",
        ];
        for name in names {
            std::fs::write(directory.path().join(name), b"test").unwrap();
        }

        let files = evidence_images(directory.path());
        let names: Vec<_> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "capture-20261025-120001-000-002.JPEG",
                "capture-20261025-120000-000-001.png",
                "capture-20261025-120000-000-001-crop.png",
                "capture-20261025-120000-000-001-marked.png",
            ]
        );
    }

    #[test]
    fn capture_paths_keep_safe_prefixes_timestamps_and_sequences() {
        let folder = Path::new("/tmp/evidence");
        assert_eq!(
            new_evidence_path(folder, "案件/比較", "20261025-121314-015", 7, "-marked"),
            folder.join("案件_比較-20261025-121314-015-007-marked.png")
        );
        assert_eq!(
            new_recording_path(folder, "20261025-121314-015", 1000),
            folder.join("recording-20261025-121314-015-1000.gif")
        );
    }

    #[test]
    fn repeated_png_saves_choose_a_new_path_without_overwriting() {
        let directory = TestDirectory::new("unique-png-paths");
        let preferred = new_evidence_path(
            directory.path(),
            "evidence",
            "20261025-120001-123",
            4,
            "-crop",
        );
        let original = image::RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255]));
        let repeated = image::RgbaImage::from_pixel(2, 1, Rgba([0, 0, 255, 255]));

        let first = save_png_without_overwrite(directory.path(), &preferred, &original).unwrap();
        let second = save_png_without_overwrite(directory.path(), &preferred, &repeated).unwrap();

        assert_eq!(first, preferred);
        assert_eq!(
            second.file_name().unwrap(),
            "evidence-20261025-120001-123-004-crop-copy-2.png"
        );
        assert_eq!(
            parse_evidence_identity(&first),
            parse_evidence_identity(&second)
        );
        assert_eq!(
            image::open(first).unwrap().to_rgba8().get_pixel(0, 0),
            &Rgba([255, 0, 0, 255])
        );
        assert_eq!(
            image::open(second).unwrap().to_rgba8().get_pixel(0, 0),
            &Rgba([0, 0, 255, 255])
        );
    }

    #[test]
    fn repeated_recording_reservations_do_not_reuse_existing_paths() {
        let directory = TestDirectory::new("unique-recording-paths");
        let preferred = new_recording_path(directory.path(), "20261025-120001-123", 1);
        let (first, mut first_file) = create_unique_file(directory.path(), &preferred).unwrap();
        first_file.write_all(b"existing recording").unwrap();
        let (second, _second_file) = create_unique_file(directory.path(), &preferred).unwrap();

        assert_eq!(first, preferred);
        assert_eq!(
            second.file_name().unwrap(),
            "recording-20261025-120001-123-001-copy-2.gif"
        );
        assert_eq!(std::fs::read(first).unwrap(), b"existing recording");
    }

    #[test]
    fn preferences_round_trip_saved_values_and_keep_equals_in_paths() {
        let settings_layout = default_settings_layout();
        let preferences = Preferences {
            capture_hotkey: "Alt+Shift+KeyS",
            recording_hotkey: "Control+Alt+KeyR",
            capture_target: "window:123",
            timestamp_enabled: false,
            filename_prefix: "案件_β",
            show_capture_notice: false,
            annotation_color: "blue",
            annotation_width: 3.5,
            save_folder: Path::new("/tmp/MacShot=Evidence"),
            recording_format: RecordingFormat::Mp4,
            recording_quality: RecordingQuality {
                mp4_bitrate_mbps: 18,
                mp4_frame_rate: 48,
                gif_quality_percent: 92,
                gif_frame_rate: 24,
                resolution: RecordingResolution::P1080,
            },
            display_density: DisplayDensity::Compact,
            font_size_percent: 125,
            settings_layout: &settings_layout,
        };
        let mut serialized = Vec::new();
        write_preferences(&mut serialized, &preferences).unwrap();
        let serialized = String::from_utf8(serialized).unwrap();

        assert_eq!(
            parse_hotkey_settings(&serialized),
            ("Alt+Shift+KeyS".into(), "Control+Alt+KeyR".into())
        );
        assert_eq!(
            parse_setting(&serialized, "capture_target", "active"),
            "window:123"
        );
        assert_eq!(
            parse_setting(&serialized, "timestamp_enabled", "true"),
            "false"
        );
        assert_eq!(
            parse_setting(&serialized, "filename_prefix", "evidence"),
            "案件_β"
        );
        assert_eq!(
            parse_setting(&serialized, "show_capture_notice", "true"),
            "false"
        );
        assert_eq!(
            parse_setting(&serialized, "annotation_color", "red"),
            "blue"
        );
        assert_eq!(parse_setting(&serialized, "annotation_width", "2"), "3.5");
        assert_eq!(
            parse_setting(&serialized, "save_folder", "/default"),
            "/tmp/MacShot=Evidence"
        );
        assert_eq!(parse_setting(&serialized, "recording_format", "gif"), "mp4");
        assert_eq!(parse_setting(&serialized, "mp4_bitrate_mbps", ""), "18");
        assert_eq!(parse_setting(&serialized, "mp4_frame_rate", ""), "48");
        assert_eq!(parse_setting(&serialized, "gif_quality_percent", ""), "92");
        assert_eq!(parse_setting(&serialized, "gif_frame_rate", ""), "24");
        assert_eq!(
            parse_setting(&serialized, "recording_resolution", ""),
            "1080p"
        );
        assert_eq!(
            parse_setting(&serialized, "display_density", "standard"),
            "compact"
        );
        assert_eq!(
            parse_setting(&serialized, "font_size_percent", "100"),
            "125"
        );
        assert_eq!(
            parse_settings_layout(&parse_setting(&serialized, "settings_layout", "")),
            settings_layout
        );
        assert_eq!(
            parse_setting(&serialized, "missing", "fallback"),
            "fallback"
        );
        assert_eq!(
            parse_hotkey_settings("\n"),
            (
                DEFAULT_CAPTURE_HOTKEY.into(),
                DEFAULT_RECORDING_HOTKEY.into()
            )
        );
    }

    #[test]
    fn window_filter_rejects_menu_bars_and_tooltips() {
        assert!(!has_captureable_window_size(1512, 33));
        assert!(!has_captureable_window_size(66, 20));
        assert!(has_captureable_window_size(708, 464));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_filter_rejects_software_cursor_overlay() {
        assert!(!is_captureable_screen_capture_title("Software Cursor"));
        assert!(!is_captureable_screen_capture_title(" software cursor "));
        assert!(!is_captureable_screen_capture_title("   "));
        assert!(is_captureable_screen_capture_title("Settings"));
    }

    #[test]
    fn gif_frame_delay_tracks_elapsed_capture_time() {
        assert_eq!(gif_delay_centiseconds(Duration::from_millis(1)), 1);
        assert_eq!(gif_delay_centiseconds(Duration::from_millis(100)), 10);
        assert_eq!(gif_delay_centiseconds(Duration::from_millis(255)), 26);
        assert_eq!(
            gif_delay_centiseconds(Duration::from_secs(u64::MAX)),
            u16::MAX
        );
    }

    #[test]
    fn gif_encoder_writes_multiple_frames_with_dimensions_delay_and_repeat() {
        let mut encoded = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut encoded, 2, 1, &[]).unwrap();
            encoder.set_repeat(gif::Repeat::Infinite).unwrap();
            let mut red = vec![255, 0, 0, 255, 255, 0, 0, 255];
            write_gif_frame(&mut encoder, 2, 1, &mut red, 10).unwrap();
            let mut blue = vec![0, 0, 255, 255, 0, 0, 255, 255];
            write_gif_frame(&mut encoder, 2, 1, &mut blue, 25).unwrap();
        }

        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(decoder.width(), 2);
        assert_eq!(decoder.height(), 1);
        assert_eq!(decoder.repeat(), gif::Repeat::Infinite);

        let first = decoder.read_next_frame().unwrap().unwrap();
        assert_eq!(first.delay, 10);
        assert_eq!(first.buffer.len(), 8);
        let first_pixels = first.buffer.to_vec();
        let second = decoder.read_next_frame().unwrap().unwrap();
        assert_eq!(second.delay, 25);
        assert_eq!(second.buffer.len(), 8);
        assert_ne!(first_pixels, second.buffer.as_ref());
        assert!(decoder.read_next_frame().unwrap().is_none());
    }

    #[test]
    fn recording_list_finds_gif_and_mp4_but_not_images() {
        let directory = TestDirectory::new("recording-list");
        for name in ["clip.gif", "movie.mp4", "still.png", "other.mov"] {
            std::fs::write(directory.path().join(name), b"test").unwrap();
        }
        let files = recording_files(directory.path());
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|path| path.ends_with("clip.gif")));
        assert!(files.iter().any(|path| path.ends_with("movie.mp4")));
    }

    #[test]
    fn recording_preview_reports_truncated_media_instead_of_panicking() {
        let directory = TestDirectory::new("truncated-recordings");
        for name in ["truncated.gif", "truncated.mp4"] {
            let path = directory.path().join(name);
            std::fs::write(&path, b"incomplete recording").unwrap();
            assert!(
                recording_preview(&path).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn gif_recording_preview_and_playback_decode_multiple_frames() {
        let directory = TestDirectory::new("gif-preview");
        let path = directory.path().join("clip.gif");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(file, 2, 1, &[]).unwrap();
        let mut pixels = vec![220, 20, 30, 255, 220, 20, 30, 255];
        write_gif_frame(&mut encoder, 2, 1, &mut pixels, 15).unwrap();
        let mut pixels = vec![30, 210, 90, 255, 30, 210, 90, 255];
        write_gif_frame(&mut encoder, 2, 1, &mut pixels, 20).unwrap();
        drop(encoder);
        let (image, details) = recording_preview(&path).unwrap();
        assert_eq!(image.dimensions(), (2, 1));
        assert!(details.contains("GIF・2フレーム・0.35秒"));
        let mut playback = RecordingFrameReader::open(&path).unwrap();
        let (first, first_delay) = playback.next_frame().unwrap().unwrap();
        let (second, second_delay) = playback.next_frame().unwrap().unwrap();
        assert_eq!(first.dimensions(), (2, 1));
        assert_eq!(second.dimensions(), (2, 1));
        assert_ne!(first.get_pixel(0, 0), second.get_pixel(0, 0));
        assert_eq!(first_delay, Duration::from_millis(150));
        assert_eq!(second_delay, Duration::from_millis(200));
        assert!(playback.next_frame().unwrap().is_none());
    }

    #[test]
    fn recording_playback_worker_streams_frames_on_demand() {
        let directory = TestDirectory::new("gif-playback-worker");
        let path = directory.path().join("clip.gif");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(file, 2, 1, &[]).unwrap();
        let mut first = vec![200, 30, 40, 255, 200, 30, 40, 255];
        write_gif_frame(&mut encoder, 2, 1, &mut first, 10).unwrap();
        let mut second = vec![20, 180, 70, 255, 20, 180, 70, 255];
        write_gif_frame(&mut encoder, 2, 1, &mut second, 10).unwrap();
        drop(encoder);

        let playback = start_recording_playback(path);
        playback.request_tx.send(()).unwrap();
        let RecordingPlaybackEvent::Frame(first, _) = playback
            .frame_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
        else {
            panic!("playback worker should deliver its first frame");
        };
        playback.request_tx.send(()).unwrap();
        let RecordingPlaybackEvent::Frame(second, _) = playback
            .frame_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
        else {
            panic!("playback worker should deliver its second frame");
        };
        assert_ne!(first.get_pixel(0, 0), second.get_pixel(0, 0));
        playback.request_tx.send(()).unwrap();
        assert!(matches!(
            playback.frame_rx.recv_timeout(Duration::from_secs(1)),
            Ok(RecordingPlaybackEvent::End)
        ));
    }

    #[test]
    fn gif_playback_honors_infinite_repeat_metadata() {
        let directory = TestDirectory::new("gif-playback-repeat");
        let path = directory.path().join("loop.gif");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(file, 2, 1, &[]).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        let mut first = vec![200, 30, 40, 255, 200, 30, 40, 255];
        write_gif_frame(&mut encoder, 2, 1, &mut first, 10).unwrap();
        let mut second = vec![20, 180, 70, 255, 20, 180, 70, 255];
        write_gif_frame(&mut encoder, 2, 1, &mut second, 10).unwrap();
        drop(encoder);

        let mut playback = RecordingFrameReader::open(&path).unwrap();
        let (first_loop, _) = playback.next_frame().unwrap().unwrap();
        let (_, _) = playback.next_frame().unwrap().unwrap();
        let (second_loop, _) = playback.next_frame().unwrap().unwrap();
        assert_eq!(first_loop.get_pixel(0, 0), second_loop.get_pixel(0, 0));
    }

    #[test]
    fn mp4_recording_preview_and_playback_decode_all_h264_frames() {
        let directory = TestDirectory::new("mp4-preview");
        let path = directory.path().join("clip.mp4");
        let mut encoder = H264Encoder::with_api_config(
            OpenH264API::from_source(),
            EncoderConfig::new()
                .bitrate(BitRate::from_bps(200_000))
                .max_frame_rate(FrameRate::from_hz(10.0))
                .usage_type(UsageType::ScreenContentRealTime),
        )
        .unwrap();
        let first_frame = image::RgbaImage::from_pixel(64, 48, Rgba([80, 140, 220, 255]));
        let second_frame = image::RgbaImage::from_pixel(64, 48, Rgba([220, 80, 60, 255]));
        let (sps, pps, initial_packets) = prime_h264_encoder(&mut encoder, &first_frame).unwrap();
        let config = Mp4Config {
            major_brand: "isom".parse().unwrap(),
            minor_version: 512,
            compatible_brands: vec!["isom".parse().unwrap(), "avc1".parse().unwrap()],
            timescale: 1000,
        };
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = Mp4Writer::write_start(file, &config).unwrap();
        writer
            .add_track(&TrackConfig::from(AvcConfig {
                width: 64,
                height: 48,
                seq_param_set: sps,
                pic_param_set: pps,
            }))
            .unwrap();
        for packet in initial_packets {
            if !packet.sample.is_empty() {
                write_mp4_sample_with_duration(&mut writer, packet, 100).unwrap();
            }
        }
        let second = encode_h264_frame(&mut encoder, &second_frame).unwrap();
        write_mp4_sample_with_duration(&mut writer, second, 100).unwrap();
        writer.write_end().unwrap();
        writer.into_writer().sync_all().unwrap();

        let (image, details) = recording_preview(&path).unwrap();
        assert_eq!(image.dimensions(), (64, 48));
        assert!(details.contains("MP4・H.264・音声トラックなし・0.20秒"));
        let mut playback = RecordingFrameReader::open(&path).unwrap();
        let (first, first_delay) = playback.next_frame().unwrap().unwrap();
        let (second, second_delay) = playback.next_frame().unwrap().unwrap();
        assert_eq!(first.dimensions(), (64, 48));
        assert_eq!(second.dimensions(), (64, 48));
        assert_ne!(first.get_pixel(0, 0), second.get_pixel(0, 0));
        assert_eq!(first_delay, Duration::from_millis(100));
        assert_eq!(second_delay, Duration::from_millis(100));
        assert!(playback.next_frame().unwrap().is_none());
    }

    #[test]
    fn mp4_mux_preserves_both_h264_video_and_aac_audio_tracks() {
        let directory = TestDirectory::new("mp4-audio-video");
        let path = directory.path().join("audio-video.mp4");
        let frame = image::RgbaImage::from_pixel(64, 48, Rgba([80, 140, 220, 255]));
        let mut video_encoder = H264Encoder::with_api_config(
            OpenH264API::from_source(),
            EncoderConfig::new()
                .bitrate(BitRate::from_bps(250_000))
                .max_frame_rate(FrameRate::from_hz(10.0))
                .usage_type(UsageType::ScreenContentRealTime),
        )
        .unwrap();
        let (sps, pps, initial_packets) = prime_h264_encoder(&mut video_encoder, &frame).unwrap();
        let mut audio_encoder = recording_audio::create_aac_encoder().unwrap();
        let mut audio_packets = Vec::new();
        for block in 0..10 {
            let start = block * 1_024;
            let samples = (start..start + 1_024)
                .flat_map(|sample| {
                    let value = (sample as f32 * 440.0 * std::f32::consts::TAU
                        / recording_audio::sample_rate() as f32)
                        .sin()
                        * 0.2;
                    [value, value]
                })
                .collect();
            audio_packets.extend(
                recording_audio::encode_block(&mut *audio_encoder, start, samples).unwrap(),
            );
        }
        audio_packets.extend(recording_audio::finish_encoder(&mut *audio_encoder).unwrap());
        assert!(!audio_packets.is_empty());

        let config = Mp4Config {
            major_brand: "isom".parse().unwrap(),
            minor_version: 512,
            compatible_brands: vec!["isom".parse().unwrap(), "avc1".parse().unwrap()],
            timescale: 1_000,
        };
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = Mp4Writer::write_start(file, &config).unwrap();
        writer
            .add_track(&TrackConfig::from(AvcConfig {
                width: 64,
                height: 48,
                seq_param_set: sps,
                pic_param_set: pps,
            }))
            .unwrap();
        let mut audio_track = TrackConfig::from(AacConfig {
            bitrate: recording_audio::bitrate(),
            profile: AudioObjectType::AacLowComplexity,
            freq_index: SampleFreqIndex::Freq48000,
            chan_conf: ChannelConfig::Stereo,
        });
        audio_track.timescale = recording_audio::sample_rate();
        writer.add_track(&audio_track).unwrap();

        for packet in initial_packets {
            if !packet.sample.is_empty() {
                write_mp4_sample_with_duration(&mut writer, packet, 100).unwrap();
            }
        }
        let packet = encode_h264_frame(&mut video_encoder, &frame).unwrap();
        write_mp4_sample_with_duration(&mut writer, packet, 100).unwrap();
        write_mp4_audio_samples(&mut writer, audio_packets).unwrap();
        writer.write_end().unwrap();
        writer.into_writer().sync_all().unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let size = file.metadata().unwrap().len();
        let mut reader = mp4::Mp4Reader::read_header(file, size).unwrap();
        let video_id = *reader
            .tracks()
            .iter()
            .find(|(_, track)| track.media_type().unwrap() == mp4::MediaType::H264)
            .map(|(id, _)| id)
            .unwrap();
        let audio_id = *reader
            .tracks()
            .iter()
            .find(|(_, track)| track.media_type().unwrap() == mp4::MediaType::AAC)
            .map(|(id, _)| id)
            .unwrap();
        let video_count = reader.sample_count(video_id).unwrap();
        let audio_count = reader.sample_count(audio_id).unwrap();
        assert!(video_count >= 2);
        assert!(audio_count >= 1);
        let audio_samples: Vec<_> = (1..=audio_count)
            .map(|sample_id| reader.read_sample(audio_id, sample_id).unwrap().unwrap())
            .collect();
        assert!(audio_samples.iter().all(|sample| !sample.bytes.is_empty()));
        assert!(
            audio_samples
                .windows(2)
                .all(|pair| pair[0].start_time < pair[1].start_time)
        );
        assert!(reader.duration().as_millis() >= 200);
        let (_, preview_details) = recording_preview(&path).unwrap();
        assert!(preview_details.contains("AAC音声トラックあり"));
    }

    #[test]
    fn all_display_composition_scales_mixed_density_monitors() {
        let retina = ImageBuffer::from_pixel(200, 100, Rgba([255, 0, 0, 255]));
        let standard = ImageBuffer::from_pixel(100, 50, Rgba([0, 255, 0, 255]));

        let combined = combine_display_images(vec![
            (0, 0, 100, 50, 2.0, retina),
            (100, 0, 100, 50, 1.0, standard),
        ])
        .unwrap();

        assert_eq!(combined.dimensions(), (400, 100));
        assert_eq!(*combined.get_pixel(10, 10), Rgba([255, 0, 0, 255]));
        assert_eq!(*combined.get_pixel(210, 10), Rgba([0, 255, 0, 255]));
        assert_eq!(*combined.get_pixel(399, 99), Rgba([0, 255, 0, 255]));
    }

    #[test]
    fn display_capture_uses_pixel_dimensions_for_retina_and_scaled_displays() {
        assert_eq!(
            display_capture_pixel_dimensions(1512, 982, 2.0),
            (3024, 1964)
        );
        assert_eq!(
            display_capture_pixel_dimensions(1920, 1080, 1.0),
            (1920, 1080)
        );
        assert_eq!(
            display_capture_pixel_dimensions(2560, 1440, 1.5),
            (3840, 2160)
        );
        assert_eq!(
            display_capture_pixel_dimensions(1512, 982, f32::NAN),
            (1512, 982)
        );
    }

    #[test]
    fn selected_window_center_maps_to_monitor_with_half_open_edges() {
        assert!(monitor_contains_point(50.0, 30.0, 0, 0, 100, 60));
        assert!(monitor_contains_point(-100.0, 0.0, -100, 0, 100, 60));
        assert!(!monitor_contains_point(100.0, 30.0, 0, 0, 100, 60));
        assert!(!monitor_contains_point(50.0, 60.0, 0, 0, 100, 60));
    }

    #[test]
    fn compare_images_marks_changed_pixels_and_pads_different_sizes() {
        let left = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 1, Rgba([255, 0, 0, 255])));
        let right = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 255, 255])));

        let (diff, overlay, count) = compare_images(&left, &right);

        assert_eq!(diff.dimensions(), (2, 1));
        assert_eq!(overlay.dimensions(), (2, 1));
        assert_eq!(count, 2);
        assert_eq!(*diff.get_pixel(0, 0), Rgba([255, 54, 54, 255]));
        assert_eq!(*diff.get_pixel(1, 0), Rgba([255, 54, 54, 255]));
        assert_eq!(*overlay.get_pixel(0, 0), Rgba([127, 0, 127, 255]));
        assert_eq!(*overlay.get_pixel(1, 0), Rgba([127, 0, 0, 255]));
    }

    #[test]
    fn compare_images_dims_identical_pixels_without_reporting_differences() {
        let image =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([100, 150, 200, 255])));

        let (diff, overlay, count) = compare_images(&image, &image);

        assert_eq!(count, 0);
        assert_eq!(*diff.get_pixel(0, 0), Rgba([20, 30, 40, 255]));
        assert_eq!(*overlay.get_pixel(0, 0), Rgba([100, 150, 200, 255]));
    }

    #[test]
    fn recording_completion_respects_continuous_capture_visibility() {
        assert!(!should_show_window_after_recording(true));
        assert!(should_show_window_after_recording(false));
    }

    #[test]
    fn quit_waits_for_active_or_starting_recording() {
        assert!(should_defer_quit_until_recording_finishes(true, false));
        assert!(should_defer_quit_until_recording_finishes(false, true));
        assert!(!should_defer_quit_until_recording_finishes(false, false));
    }

    #[test]
    fn capture_failure_message_does_not_confuse_missing_windows_with_permission_errors() {
        assert_eq!(
            capture_failure_message("アクティブウィンドウが見つかりません。。"),
            "撮影できませんでした: アクティブウィンドウが見つかりません。"
        );
        assert_eq!(
            capture_failure_message("ScreenCaptureKit: TCCを拒否しました。"),
            "macOSから画面収録を拒否されました。「システム設定 > プライバシーとセキュリティ > 画面収録とシステムオーディオ録音」でMacShotを許可してください。一覧にない場合は「＋」からMacShot.appを追加してオンにし、MacShotを再起動します。既にオンの場合は一度オフ→オンしてから再起動してください。"
        );
    }

    #[test]
    fn continuous_capture_preflight_rejects_stale_targets_and_recording() {
        let windows = [WindowChoice {
            id: 9,
            title: "Calculator".into(),
        }];
        assert_eq!(
            capture_target_preflight_error(CaptureTarget::Display(2), false, false, 2, &windows),
            Some("撮影できる画面が見つかりません。画面収録の許可を確認してください。")
        );
        assert_eq!(
            capture_target_preflight_error(CaptureTarget::Window(10), false, false, 2, &windows),
            Some("撮影対象のウィンドウが見つかりません。ウィンドウ一覧を更新してください。")
        );
        assert_eq!(
            capture_target_preflight_error(CaptureTarget::ActiveWindow, true, false, 2, &windows),
            Some("録画を終了してからスクリーンショットを撮影してください。")
        );
        assert_eq!(
            capture_target_preflight_error(CaptureTarget::Window(9), false, false, 2, &windows),
            None
        );
    }

    #[test]
    fn expired_capture_deadline_wins_over_a_queued_late_result() {
        let mut app = MacShot::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.capture_in_progress = true;
        app.active_capture_sequence = Some(42);
        app.capture_deadline = Some(Instant::now() - Duration::from_millis(1));
        app.capture_cancel = Some(cancelled.clone());
        app.capture_queue.push_back(CaptureTarget::PrimaryDisplay);
        app.continuous_capture_session_active = true;
        app.capture_tx
            .send((42, Err("late capture failure".to_string())))
            .unwrap();

        app.handle_capture_results(&egui::Context::default());

        assert!(cancelled.load(Ordering::Relaxed));
        assert!(!app.capture_in_progress);
        assert_eq!(app.active_capture_sequence, None);
        assert!(app.capture_queue.is_empty());
        assert_eq!(app.continuous_capture_failed_count, 1);
        assert!(!app.continuous_capture_session_active);
        assert_eq!(
            app.tray_capture_status_state,
            Some((false, false, false, 0, 1))
        );
        assert_eq!(app.status, capture_timeout_message());
    }

    #[test]
    fn timed_out_continuous_capture_keeps_mode_active_and_updates_failure_count() {
        let mut app = MacShot {
            continuous_capture: true,
            continuous_capture_session_active: true,
            capture_in_progress: true,
            active_capture_sequence: Some(43),
            capture_deadline: Some(Instant::now() - Duration::from_millis(1)),
            ..MacShot::default()
        };

        app.handle_capture_timeout(&egui::Context::default());

        assert!(app.continuous_capture);
        assert!(app.continuous_capture_session_active);
        assert_eq!(app.continuous_capture_failed_count, 1);
        assert_eq!(
            app.tray_capture_status_state,
            Some((true, true, false, 0, 1))
        );
    }

    #[test]
    fn recording_permission_failure_shows_the_same_settings_guidance() {
        assert!(recording_failure_message(
            "ScreenCaptureKit: Content unavailable: ユーザがアプリケーションを拒否しました (TCC)"
        )
        .starts_with("macOSから画面収録を拒否されました"));
        assert_eq!(
            recording_failure_message("録画する画面が見つかりません。"),
            "録画できませんでした: 録画する画面が見つかりません。"
        );
    }

    #[test]
    fn denied_microphone_recording_error_points_to_microphone_settings() {
        let message = microphone_permission_failure_message(
            "マイクを開始できません: permission denied",
            true,
        )
        .unwrap();
        assert!(message.contains("プライバシーとセキュリティ > マイク"));
        assert!(message.contains("権限状態を更新"));
        assert!(microphone_permission_failure_message("画面収録が拒否されました", true).is_none());
        assert!(microphone_permission_failure_message("マイクを開始できません", false).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn microphone_permission_result_explains_both_outcomes() {
        assert!(microphone_permission_result_message(true).contains("マイク音声を含めます"));
        let denied = microphone_permission_result_message(false);
        assert!(denied.contains("システム音声のみ"));
        assert!(denied.contains("マイク設定"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn avaudioapplication_permission_states_map_to_app_states() {
        assert_eq!(
            microphone_permission_from_record_permission(
                AVAudioApplicationRecordPermission::Undetermined
            ),
            MicrophonePermission::NotDetermined
        );
        assert_eq!(
            microphone_permission_from_record_permission(
                AVAudioApplicationRecordPermission::Denied
            ),
            MicrophonePermission::Denied
        );
        assert_eq!(
            microphone_permission_from_record_permission(
                AVAudioApplicationRecordPermission::Granted
            ),
            MicrophonePermission::Authorized
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn microphone_permission_action_matches_the_os_status() {
        assert_eq!(
            microphone_permission_action_label(MicrophonePermission::NotDetermined),
            Some("macOSに許可を確認")
        );
        assert_eq!(
            microphone_permission_action_label(MicrophonePermission::Denied),
            Some("設定でMacShotを許可")
        );
        assert_eq!(
            microphone_permission_action_label(MicrophonePermission::Authorized),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn undetermined_microphone_state_is_not_misreported_as_a_denial() {
        let label = microphone_permission_label_for(MicrophonePermission::NotDetermined);
        assert!(label.contains("状態未決定"));
        assert!(!label.contains("拒否"));
        assert!(label.contains("システム音声のみ"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn microphone_capture_is_skipped_without_authorization() {
        assert!(should_capture_microphone(MicrophonePermission::Authorized));
        assert!(!should_capture_microphone(
            MicrophonePermission::NotDetermined
        ));
        assert!(!should_capture_microphone(MicrophonePermission::Denied));
        assert!(!should_capture_microphone(MicrophonePermission::Restricted));
    }

    #[test]
    fn recording_permission_warning_survives_in_status() {
        assert_eq!(
            append_recording_warning("録画を保存しました", Some("システム音声のみ")),
            "録画を保存しました — システム音声のみ"
        );
        assert_eq!(
            append_recording_warning("録画を保存しました", None),
            "録画を保存しました"
        );
    }

    #[test]
    fn hotkey_labels_are_canonical_and_badges_ignore_parser_casing() {
        assert_eq!(canonical_hotkey_text("shift+alt+KeyS"), "Alt+Shift+KeyS");
        let alt_label = if cfg!(target_os = "macos") {
            "Option"
        } else {
            "Alt"
        };
        assert_eq!(
            hotkey_badge_text("shift+alt+KeyS"),
            format!("{alt_label}  ⇧  S")
        );
    }

    #[test]
    fn hotkey_pair_validation_requires_modifiers_and_distinct_shortcuts() {
        assert!(parse_hotkey_pair("Alt+Shift+KeyS", "Control+Alt+KeyR").is_ok());
        assert!(
            parse_hotkey_pair("KeyS", "Alt+Shift+KeyR")
                .unwrap_err()
                .contains("修飾キーが1つ以上必要です")
        );
        assert!(
            parse_hotkey_pair("Alt+Shift+KeyS", "Alt+Shift+KeyS")
                .unwrap_err()
                .contains("同じショートカットは使えません")
        );
    }

    #[test]
    fn annotations_draw_expected_colors_without_filling_rectangle() {
        let background = Rgba([10, 20, 30, 255]);
        let mut image = ImageBuffer::from_pixel(100, 100, background);
        draw_annotation(
            &mut image,
            Annotation {
                tool: EditTool::Rectangle,
                start: egui::pos2(0.1, 0.1),
                end: egui::pos2(0.8, 0.8),
            },
            annotation_rgb("red"),
            3.0,
        );
        assert_eq!(*image.get_pixel(10, 10), Rgba([255, 38, 38, 255]));
        assert_eq!(*image.get_pixel(50, 50), background);

        draw_annotation(
            &mut image,
            Annotation {
                tool: EditTool::Arrow,
                start: egui::pos2(0.1, 0.5),
                end: egui::pos2(0.8, 0.5),
            },
            annotation_rgb("blue"),
            3.0,
        );
        assert_eq!(*image.get_pixel(50, 50), Rgba([30, 130, 255, 255]));
    }

    #[test]
    fn timestamp_stamp_preserves_dimensions_and_draws_text() {
        let mut image = ImageBuffer::from_pixel(320, 100, Rgba([0, 0, 0, 255]));
        stamp_timestamp(&mut image, "2026-10-25 19:05:12");

        assert_eq!(image.dimensions(), (320, 100));
        assert!(
            image
                .pixels()
                .any(|pixel| *pixel == Rgba([255, 255, 255, 255]))
        );
    }

    #[test]
    fn normalized_crop_clamps_reverses_and_rejects_tiny_bounds() {
        let image = ImageBuffer::from_fn(6, 4, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let crop = crop_normalized(&image, [0.25, 0.25, 0.75, 0.75]).unwrap();
        assert_eq!(crop.dimensions(), (4, 2));
        assert_eq!(*crop.get_pixel(0, 0), Rgba([1, 1, 0, 255]));
        assert_eq!(*crop.get_pixel(3, 1), Rgba([4, 2, 0, 255]));

        let reversed = crop_normalized(&image, [1.2, 1.2, -0.2, -0.2]).unwrap();
        assert_eq!(reversed.dimensions(), (6, 4));
        assert_eq!(*reversed.get_pixel(5, 3), Rgba([5, 3, 0, 255]));
        assert!(crop_normalized(&image, [0.2, 0.2, 0.3, 0.3]).is_none());
    }

    #[test]
    fn region_selection_normalizes_reversed_and_clipped_drag_coordinates() {
        let screen = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 50.0));

        assert_eq!(
            normalized_region_bounds(screen, egui::pos2(110.0, 70.0), egui::pos2(35.0, 30.0)),
            Some([0.22, 0.14, 1.0, 1.0])
        );
        assert_eq!(
            normalized_region_bounds(screen, egui::pos2(0.0, 10.0), egui::pos2(60.0, 50.0)),
            Some([0.0, 0.0, 0.53, 0.66])
        );
        assert!(
            normalized_region_bounds(screen, egui::pos2(10.0, 20.0), egui::pos2(12.0, 40.0))
                .is_none()
        );
        assert!(
            normalized_region_bounds(
                egui::Rect::NOTHING,
                egui::pos2(0.0, 0.0),
                egui::pos2(10.0, 10.0)
            )
            .is_none()
        );
    }

    #[test]
    fn region_capture_crop_keeps_the_selection_margin_at_retina_scale() {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 50.0));
        let image = ImageBuffer::from_fn(200, 100, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let bounds =
            normalized_region_bounds(screen, egui::pos2(30.0, 10.0), egui::pos2(60.0, 40.0))
                .unwrap();

        let crop = crop_normalized(&image, bounds).unwrap();

        assert_eq!(crop.dimensions(), (72, 72));
        assert_eq!(*crop.get_pixel(0, 0), Rgba([54, 14, 0, 255]));
        assert_eq!(*crop.get_pixel(71, 71), Rgba([125, 85, 0, 255]));
    }

    #[test]
    fn capture_result_shows_saved_pixel_dimensions_in_status_and_notice() {
        let mut app = MacShot {
            show_capture_notice: true,
            capture_in_progress: true,
            active_capture_sequence: Some(7),
            save_folder: Some(PathBuf::from("/tmp/macshot-capture-notice-test")),
            ..MacShot::default()
        };
        app.capture_tx
            .send((
                7,
                Ok((
                    PathBuf::from("/tmp/macshot-capture-notice-test.png"),
                    image::RgbaImage::new(2067, 1327),
                    "2026-10-27 16:04:42".into(),
                    7,
                )),
            ))
            .unwrap();

        app.handle_capture_results(&egui::Context::default());

        assert_eq!(
            app.status,
            "撮影 7 枚目を保存しました (2026-10-27 16:04:42) · 2067×1327 px"
        );
        let notice = app.capture_notice.expect("capture notice should be shown");
        assert_eq!((notice.width, notice.height), (2067, 1327));
        assert_eq!(
            capture_size_label(notice.width, notice.height),
            "画像サイズ: 2067 × 1327 px"
        );
    }
}
