#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ab_glyph::{FontRef, PxScale};
#[cfg(target_os = "macos")]
use block2::RcBlock;
use bytes::Bytes;
use chrono::{DateTime, Local};
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
use objc2_av_foundation::AVAsset;
#[cfg(target_os = "macos")]
use objc2_avf_audio::{AVAudioApplication, AVAudioApplicationRecordPermission};
#[cfg(target_os = "macos")]
use objc2_core_media::CMTime;
#[cfg(target_os = "macos")]
use objc2_foundation_06::{NSString, NSURL};
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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};
use xcap::Monitor;
#[cfg(not(target_os = "macos"))]
use xcap::Window;

#[cfg(target_os = "macos")]
mod macos_video;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod recording_audio;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
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
    language: AppLanguage,
    monitors: Vec<Monitor>,
    windows: Vec<WindowChoice>,
    window_search_query: String,
    capture_target: CaptureTarget,
    capture_target_picker_open: bool,
    capture_shortcut_behavior: CaptureShortcutBehavior,
    continuous_capture: bool,
    keep_region_in_continuous: bool,
    continuous_region: Option<[f32; 4]>,
    capture_queue: VecDeque<CaptureRequest>,
    region_selecting: bool,
    region_drag_start: Option<egui::Pos2>,
    region_drag_current: Option<egui::Pos2>,
    region_overlay_position: egui::Pos2,
    region_overlay_size: egui::Vec2,
    region_snapshot_tx: Sender<Result<image::RgbaImage, String>>,
    region_snapshot_rx: Receiver<Result<image::RgbaImage, String>>,
    #[cfg(target_os = "windows")]
    region_geometry_tx: Sender<Result<(f32, f32, f32, f32), String>>,
    #[cfg(target_os = "windows")]
    region_geometry_rx: Receiver<Result<(f32, f32, f32, f32), String>>,
    region_selection_deadline: Option<Instant>,
    #[cfg(target_os = "windows")]
    region_geometry_pending: bool,
    region_background_texture: Option<egui::TextureHandle>,
    save_folder: Option<PathBuf>,
    auto_import_native_captures: bool,
    #[cfg(target_os = "macos")]
    native_capture_watch: Option<NativeCaptureWatch>,
    #[cfg(target_os = "macos")]
    native_capture_import_tx: Sender<NativeCaptureImportResult>,
    #[cfg(target_os = "macos")]
    native_capture_import_rx: Receiver<NativeCaptureImportResult>,
    evidence_files: Vec<PathBuf>,
    recording_files: Vec<PathBuf>,
    recording_preview_path: Option<PathBuf>,
    recording_preview_image: Option<DynamicImage>,
    recording_preview_texture: Option<egui::TextureHandle>,
    recording_preview_details: Option<String>,
    recording_player_controls_rect: Option<egui::Rect>,
    recording_player_last_pointer_pos: Option<egui::Pos2>,
    recording_player_last_pointer_activity: Option<Instant>,
    recording_playback: Option<RecordingPlayback>,
    recording_player_error: Option<String>,
    recording_rename_dialog: Option<RecordingRenameDialog>,
    recording_duration_cache: HashMap<PathBuf, Option<Duration>>,
    recording_metadata_pending: HashSet<PathBuf>,
    recording_metadata_tx: Sender<RecordingMetadataResult>,
    recording_metadata_rx: Receiver<RecordingMetadataResult>,
    timestamp_enabled: bool,
    timestamp_format: String,
    timestamp_position: String,
    timestamp_background: String,
    filename_prefix: String,
    show_capture_notice: bool,
    annotation_color: String,
    annotation_width: f32,
    annotation_text: String,
    status: String,
    capture_in_progress: bool,
    active_capture_is_region: bool,
    region_annotation_pending: bool,
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
    tray_capture_target_item: Option<MenuItem>,
    tray_capture_target_text: Option<String>,
    tray_target_selection_items: Vec<(MenuItem, CaptureTarget)>,
    tray_capture_active_item: Option<MenuItem>,
    tray_capture_primary_item: Option<MenuItem>,
    tray_capture_all_item: Option<MenuItem>,
    tray_capture_current_target_item: Option<MenuItem>,
    tray_capture_shortcut_target_item: Option<MenuItem>,
    tray_capture_shortcut_mode_item: Option<MenuItem>,
    tray_capture_status_item: Option<MenuItem>,
    tray_window_selection_item: Option<MenuItem>,
    tray_continuous_capture_item: Option<MenuItem>,
    tray_recording_status_item: Option<MenuItem>,
    tray_recording_action_item: Option<MenuItem>,
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
    notifications: Vec<AppNotification>,
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
    mosaic_preview_texture: Option<egui::TextureHandle>,
    mosaic_preview_key: Option<(Vec<Annotation>, Option<PathBuf>)>,
    crop_selection: Option<egui::Rect>,
    crop_drag_start: Option<egui::Pos2>,
    edit_tool: EditTool,
    capture_editor_expanded: bool,
    overwrite_confirmation: bool,
    annotations: Vec<Annotation>,
    live_annotation: Option<Annotation>,
    editing_queue: Vec<PathBuf>,
    editing_index: Option<usize>,
    pending_edit_navigation: Option<PendingEditNavigation>,
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

#[derive(Clone, Copy)]
struct CaptureRequest {
    target: CaptureTarget,
    region: Option<[f32; 4]>,
}

#[derive(Clone)]
enum PendingEditNavigation {
    Queue { direction: isize },
    OpenEvidence { queue: Vec<PathBuf>, index: usize },
}

#[derive(Clone, Copy, PartialEq)]
enum CaptureShortcutBehavior {
    SelectedTarget,
    ChooseMode,
}

impl CaptureShortcutBehavior {
    fn from_setting(value: &str) -> Self {
        match value {
            "choose_mode" => Self::ChooseMode,
            _ => Self::SelectedTarget,
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Self::SelectedTarget => "selected_target",
            Self::ChooseMode => "choose_mode",
        }
    }
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
    Ellipse,
    Arrow,
    Text,
    Timestamp,
    Mosaic,
}

#[derive(Clone, PartialEq)]
struct Annotation {
    tool: EditTool,
    start: egui::Pos2,
    end: egui::Pos2,
    text: String,
}

struct CaptureNotice {
    sequence: u64,
    timestamp: String,
    path: PathBuf,
    width: u32,
    height: u32,
}

#[derive(Clone)]
struct AppNotification {
    kind: NotificationKind,
    timestamp: String,
    path: PathBuf,
    width: Option<u32>,
    height: Option<u32>,
    detail: Option<String>,
}

#[derive(Clone, Copy)]
enum NotificationKind {
    Capture(u64),
    Recording,
}

#[derive(Clone)]
struct WindowChoice {
    id: u32,
    app_name: String,
    title: String,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Capture,
    Evidence,
    Notifications,
    Compare,
    Settings,
}

fn switch_page(current: &mut Page, next: Page, status: &mut String) -> bool {
    if *current == next {
        return false;
    }
    *current = next;
    status.clear();
    true
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AppLanguage {
    #[default]
    Japanese,
    English,
    SimplifiedChinese,
    TraditionalChinese,
}

impl AppLanguage {
    fn text(self, japanese: &str) -> String {
        if self == Self::Japanese {
            return japanese.to_owned();
        }

        let translated = match japanese {
            "撮影・編集" => ["Capture & Edit", "截图与编辑", "螢幕截圖與編輯"],
            "証跡一覧" => ["Evidence", "证据列表", "證據清單"],
            "通知履歴" => ["Notifications", "通知历史", "通知記錄"],
            "通知" => ["Alerts", "通知", "通知"],
            "ここに撮影・録画の完了通知が保存されます。" => [
                "Capture and recording completion notices will appear here.",
                "截图和录屏完成通知会显示在这里。",
                "截圖和錄影完成通知會顯示在這裡。",
            ],
            "通知履歴はこのMacShot内に最大200件保存されます。" => [
                "Up to 200 notifications are saved locally by MacShot.",
                "MacShot 会在本机保存最多 200 条通知。",
                "MacShot 會在本機儲存最多 200 則通知。",
            ],
            "撮影しました" => ["Screenshot captured", "截图已完成", "截圖已完成"],
            "履歴を消去" => ["Clear History", "清除历史", "清除記錄"],
            "通知履歴を消去しました" => [
                "Notification history cleared.",
                "已清除通知历史。",
                "已清除通知記錄。",
            ],
            "通知履歴を消去できません" => [
                "Could not clear notification history",
                "无法清除通知历史",
                "無法清除通知記錄",
            ],
            "通知履歴を保存できません" => [
                "Could not save notification history",
                "无法保存通知历史",
                "無法儲存通知記錄",
            ],
            "Finderで表示できません" => [
                "Could not reveal file",
                "无法在 Finder 中显示文件",
                "無法在 Finder 中顯示檔案",
            ],
            "Finderで表示" => ["Show in Finder", "在 Finder 中显示", "在 Finder 中顯示"],
            "ファイルが見つかりません" => {
                ["File not found", "找不到文件", "找不到檔案"]
            }
            "画像比較" => ["Compare", "图像比较", "圖片比較"],
            "設定" => ["Settings", "设置", "設定"],
            "録画を終了" => ["Stop Recording", "停止录制", "停止錄製"],
            "録画" => ["Record", "录制", "錄製"],
            "■  録画を終了" => ["■  Stop Recording", "■  停止录制", "■  停止錄製"],
            "●  録画" => ["●  Record", "●  录制", "●  錄製"],
            "撮影中…" => ["Capturing…", "正在截图…", "正在截圖…"],
            "撮影してプレビュー" => ["Capture & Preview", "截图并预览", "截圖並預覽"],
            "クリックして拡大・編集" => [
                "Click to enlarge and edit",
                "点击放大并编辑",
                "點擊放大並編輯",
            ],
            "画像を拡大して編集" => {
                ["Enlarged Image Editor", "放大图片并编辑", "放大圖片並編輯"]
            }
            "撮影対象" => ["Capture Target", "截图目标", "截圖目標"],
            "ショートカットの動作" => ["Shortcut Behavior", "快捷键行为", "快速鍵行為"],
            "この対象を撮影" => ["Capture This Target", "截取此目标", "擷取此目標"],
            "撮影対象選択を優先" => {
                ["Prioritize Selected Target", "优先所选目标", "優先所選目標"]
            }
            "各モード優先" => [
                "Choose a Capture Mode",
                "优先选择截图模式",
                "優先選擇截圖模式",
            ],
            "選択中の対象をすぐ撮影します" => [
                "Capture the selected target immediately.",
                "立即截取所选目标。",
                "立即擷取所選目標。",
            ],
            "ショートカット後に撮影方法を選びます" => [
                "Choose a capture mode after pressing the shortcut.",
                "按下快捷键后选择截图模式。",
                "按下快速鍵後選擇截圖模式。",
            ],
            "撮影方法を選んで撮影…" => {
                ["Choose a Capture Mode…", "选择截图模式…", "選擇截圖模式…"]
            }
            "ウィンドウを選んで撮影…" => [
                "Click a Window to Capture…",
                "点选窗口并截图…",
                "點選視窗並截圖…",
            ],
            "撮影するウィンドウをクリックしてください。Escでキャンセルできます。" => {
                [
                    "Click the window to capture. Press Esc to cancel.",
                    "请点按要截取的窗口。按 Esc 取消。",
                    "請點選要擷取的視窗。按 Esc 取消。",
                ]
            }
            "ウィンドウ選択をキャンセルしました。" => [
                "Window selection canceled.",
                "已取消窗口选择。",
                "已取消視窗選取。",
            ],
            "Esc  キャンセル" => ["Esc  Cancel", "Esc  取消", "Esc  取消"],
            "撮影方法を選択" => ["Choose Capture Mode", "选择截图方式", "選擇截圖方式"],
            "撮影対象を選んでから撮影してください" => [
                "Choose what to capture, then take the screenshot.",
                "选择截图目标后再进行截图。",
                "選擇截圖目標後再進行截圖。",
            ],
            "ショートカットで開きました。撮影対象を選べます。" => [
                "Opened from the shortcut. Choose what to capture.",
                "已通过快捷键打开。请选择截图目标。",
                "已透過快捷鍵開啟。請選擇截圖目標。",
            ],
            "ディスプレイ" => ["Displays", "显示器", "顯示器"],
            "選択ウィンドウ（背面でも選択できます）" => [
                "Select a Window (including windows behind others)",
                "选择窗口（包括被遮挡的窗口）",
                "選擇視窗（包含被遮住的視窗）",
            ],
            "選択中" => ["Selected", "已选择", "已選取"],
            "ウィンドウ一覧を更新" => {
                ["Refresh Window List", "刷新窗口列表", "重新整理視窗清單"]
            }
            "前後に関係なく、選んだウィンドウの内容を撮影します" => [
                "Captures the selected window even when it is behind other windows.",
                "即使窗口被其他窗口遮挡，也会截取所选窗口内容。",
                "即使視窗被其他視窗遮住，也會擷取所選視窗內容。",
            ],
            "アクティブウィンドウ" => ["Active Window", "活动窗口", "作用中視窗"],
            "メインディスプレイ" => ["Main Display", "主显示器", "主要顯示器"],
            "すべてのディスプレイ" => ["All Displays", "所有显示器", "所有顯示器"],
            "Display" => ["Display", "显示器", "顯示器"],
            "Full Display" => ["Full Display", "整个显示器", "整個顯示器"],
            "Window" => ["Window", "窗口", "視窗"],
            "アプリ名・ウィンドウ名を検索" => [
                "Search app or window name",
                "搜索应用或窗口名称",
                "搜尋應用程式或視窗名稱",
            ],
            "一致するウィンドウがありません" => {
                ["No matching windows", "没有匹配的窗口", "沒有符合的視窗"]
            }
            "ウィンドウ候補がありません。一覧を更新してください。" => [
                "No windows found. Refresh the list.",
                "没有找到窗口，请刷新列表。",
                "找不到視窗，請重新整理清單。",
            ],
            "ウィンドウを選択" => ["Select a Window", "选择窗口", "選取視窗"],
            "デスクトップ（メイン画面）" => [
                "Desktop (Main Display)",
                "桌面（主显示器）",
                "桌面（主要顯示器）",
            ],
            "全画面（全ディスプレイ）" => ["All Displays", "所有显示器", "所有顯示器"],
            "範囲を選択" => ["Select Region", "选择区域", "選取區域"],
            "↻  ウィンドウ一覧を更新" => [
                "↻  Refresh Window List",
                "↻  刷新窗口列表",
                "↻  重新整理視窗清單",
            ],
            "追加の操作" => ["More Actions", "更多操作", "更多操作"],
            "連続撮影を終了" => ["Stop Continuous Capture", "停止连续截图", "停止連續截圖"],
            "連続撮影を開始" => ["Start Continuous Capture", "开始连续截图", "開始連續截圖"],
            "連続撮影で範囲を維持する" => [
                "Keep selected area for continuous captures",
                "连续截图时保留所选区域",
                "連續截圖時保留所選範圍",
            ],
            "✓ 撮影完了" => ["✓ Finish capture", "✓ 完成截图", "✓ 完成截圖"],
            "注釈を保存して撮影を完了します。" => [
                "Save annotations and finish this capture.",
                "保存标注并完成此次截图。",
                "儲存註記並完成此次截圖。",
            ],
            "範囲撮影を完了しました。" => [
                "Region capture completed.",
                "区域截图已完成。",
                "區域截圖已完成。",
            ],
            "macOS標準の撮影・録画…" => [
                "Open macOS Screenshot…",
                "打开 macOS 截图…",
                "開啟 macOS 螢幕截圖…",
            ],
            "撮影" => ["Capture", "截图", "截圖"],
            "保存先を変更" => ["Change Save Location", "更改保存位置", "變更儲存位置"],
            "画面撮影・録画には画面収録の許可が必要です。" => [
                "Screen Recording permission is required to capture or record the screen.",
                "截屏或录屏需要屏幕录制权限。",
                "擷取或錄製螢幕需要螢幕錄製權限。",
            ],
            "権限状態は設定画面で確認できます。オンでも撮影が拒否される場合はMacShot.appを登録し直して再起動してください。" => {
                [
                    "Check permission in Settings. If capture is still denied, add MacShot.app again and restart it.",
                    "请在设置中查看权限。如果仍无法截图，请重新添加 MacShot.app 并重启。",
                    "請在設定中查看權限。如果仍無法截圖，請重新加入 MacShot.app 並重新啟動。",
                ]
            }
            "画面収録設定を開く" => [
                "Open Screen Recording Settings",
                "打开屏幕录制设置",
                "開啟螢幕錄製設定",
            ],
            "macOSに画面収録を要求" => [
                "Request Screen Recording",
                "请求屏幕录制权限",
                "要求螢幕錄製權限",
            ],
            "FinderでMacShotを表示" => [
                "Show MacShot in Finder",
                "在 Finder 中显示 MacShot",
                "在 Finder 中顯示 MacShot",
            ],
            "権限状態を更新" => [
                "Refresh Permission Status",
                "刷新权限状态",
                "重新整理權限狀態",
            ],
            "▦  撮影済みを開く" => {
                ["▦  Open Captures", "▦  打开已截取项目", "▦  開啟已擷取項目"]
            }
            "‹ 前へ" => ["‹ Previous", "‹ 上一张", "‹ 上一張"],
            "次へ ›" => ["Next ›", "下一张 ›", "下一張 ›"],
            "保存して次へ" => ["Save & Next", "保存并继续", "儲存並繼續"],
            "未保存の編集があります" => {
                ["Unsaved Edits", "有未保存的编辑", "有未儲存的編輯"]
            }
            "移動する前に、現在の注釈や切り抜きを保存しますか？" => [
                "Save the current annotations or crop before moving?",
                "移动前要保存当前标注或裁剪吗？",
                "移動前要儲存目前的註記或裁切嗎？",
            ],
            "保存して移動" => ["Save & Move", "保存并移动", "儲存並移動"],
            "破棄して移動" => ["Discard & Move", "放弃并移动", "捨棄並移動"],
            "編集に戻る" => ["Keep Editing", "继续编辑", "繼續編輯"],
            "編集内容を保存するか消去してから撮影してください。" => [
                "Save or clear the current edits before taking another screenshot.",
                "请先保存或清除当前编辑，再进行截图。",
                "請先儲存或清除目前編輯，再進行截圖。",
            ],
            "編集ツール" => ["Editing Tools", "编辑工具", "編輯工具"],
            "⌗  切り出し" => ["⌗  Crop", "⌗  裁剪", "⌗  裁切"],
            "□  赤枠" => ["□  Red Box", "□  红框", "□  紅框"],
            "↗  矢印" => ["↗  Arrow", "↗  箭头", "↗  箭頭"],
            "注釈を消去" => ["Clear Annotations", "清除标注", "清除註記"],
            "注釈画像を別名で保存" => {
                ["Save Annotated Copy", "另存为标注图片", "另存為註記圖片"]
            }
            "上書き保存" => ["Overwrite", "覆盖保存", "覆寫儲存"],
            "上書き保存の確認" => ["Confirm Overwrite", "确认覆盖", "確認覆寫"],
            "画像ファイル" => ["Image file", "图像文件", "圖像檔案"],
            "注釈を反映して、現在の画像ファイルを上書きします。" => [
                "Apply the annotations and overwrite the current image file.",
                "应用标注并覆盖当前图像文件。",
                "套用註記並覆寫目前的圖片檔案。",
            ],
            "別名の画像は作成されません。上書きしますか？" => [
                "No separate copy will be created. Overwrite this file?",
                "不会创建副本。要覆盖此文件吗？",
                "不會建立副本。要覆寫此檔案嗎？",
            ],
            "現在のPNG画像を注釈付きで上書き" => [
                "Overwrite this PNG with the annotations",
                "用标注覆盖此 PNG 图像",
                "以註記覆寫此 PNG 圖片",
            ],
            "選択解除" => ["Clear Selection", "清除选区", "清除選取範圍"],
            "切り出しを別名で保存" => {
                ["Save Cropped Copy", "另存为裁剪图像", "另存為裁切影像"]
            }
            "撮影した画面は、ここで切り出しや注釈を追加できます。" => [
                "Crop captures and add annotations here.",
                "在这里裁剪截图并添加标注。",
                "在這裡裁切截圖並新增註記。",
            ],
            "ショートカット" => ["Shortcut", "快捷键", "快速鍵"],
            "2枚の画面を並べて、変更点を確認します。" => [
                "Place two images side by side to review changes.",
                "并排查看两张图像以确认更改。",
                "並排檢視兩張圖片以確認變更。",
            ],
            "差分ピクセル" => ["Changed Pixels", "差异像素", "差異像素"],
            "＋  基準画像" => ["＋  Reference Image", "＋  基准图像", "＋  參考圖片"],
            "＋  比較画像" => ["＋  Comparison Image", "＋  比较图像", "＋  比較圖片"],
            "画像を選択してください" => ["Select an image", "请选择图像", "請選取圖片"],
            "比較する2枚の画像を選んでください" => [
                "Choose two images to compare",
                "请选择两张要比较的图像",
                "請選取兩張要比較的圖片",
            ],
            "録画は一覧からプレビュー、画像は編集して保存できます。" => {
                [
                    "Preview recordings or edit and save images from this list.",
                    "可在列表中预览录屏，或编辑并保存图像。",
                    "可在清單中預覽錄影，或編輯並儲存圖片。",
                ]
            }
            "▣  保存先を開く" => [
                "▣  Open Save Location",
                "▣  打开保存位置",
                "▣  開啟儲存位置",
            ],
            "↻  更新" => ["↻  Refresh", "↻  刷新", "↻  重新整理"],
            "録画一覧" => ["Recordings", "录屏列表", "錄影清單"],
            "録画は別ウィンドウで再生・名前変更、画像は編集して保存できます。" => {
                [
                    "Play recordings in a separate window, rename them, or edit and save images.",
                    "在独立窗口播放录屏、重命名录屏，或编辑并保存图像。",
                    "在獨立視窗播放錄影、重新命名錄影，或編輯並儲存圖片。",
                ]
            }
            "GIF・MP4・対応MOVを別ウィンドウで再生できます。対応外の形式は外部プレイヤーを使います。録画名も変更できます。" => {
                [
                    "Play supported GIF, MP4, and MOV recordings in a separate window. Use an external player for other formats. You can also rename recordings.",
                    "GIF、MP4 和受支持的 MOV 录屏可在独立窗口播放。其他格式请使用外部播放器。也可重命名录屏。",
                    "GIF、MP4 與支援的 MOV 錄影可在獨立視窗播放。其他格式請使用外部播放器。也可重新命名錄影。",
                ]
            }
            "時間を確認中…" => ["Reading duration…", "正在读取时长…", "正在讀取長度…"],
            "時間不明" => ["Duration unavailable", "时长未知", "長度不明"],
            "再生" => ["Play", "播放", "播放"],
            "編集" => ["Edit", "编辑", "編輯"],
            "▶ 再生" => ["▶ Play", "▶ 播放", "▶ 播放"],
            "録画再生" => ["Recording Player", "录屏播放", "錄影播放"],
            "録画を新しいウィンドウで再生します。" => [
                "Playing the recording in a new window.",
                "正在新窗口中播放录屏。",
                "正在新視窗中播放錄影。",
            ],
            "録画名を編集" => ["Edit Recording Name", "编辑录屏名称", "編輯錄影名稱"],
            "ファイル名" => ["File name", "文件名", "檔案名稱"],
            "録画名を入力してください" => {
                ["Enter a recording name", "请输入录屏名称", "請輸入錄影名稱"]
            }
            "保存" => ["Save", "保存", "儲存"],
            "同じ名前の録画がすでにあります。" => [
                "A recording with that name already exists.",
                "已存在同名录屏。",
                "已存在同名錄影。",
            ],
            "録画名を入力してください。" => [
                "Enter a recording name.",
                "请输入录屏名称。",
                "請輸入錄影名稱。",
            ],
            "録画名は180バイト以内にしてください。" => [
                "Keep the recording name under 180 bytes.",
                "录屏名称请控制在180字节以内。",
                "錄影名稱請控制在180位元組內。",
            ],
            "録画名を変更しました。" => [
                "Recording name changed.",
                "录屏名称已更改。",
                "錄影名稱已變更。",
            ],
            "録画名は変更されていません。" => [
                "Recording name is unchanged.",
                "录屏名称未更改。",
                "錄影名稱未變更。",
            ],
            "録画名を変更できません" => [
                "Unable to change recording name",
                "无法更改录屏名称",
                "無法變更錄影名稱",
            ],
            "GIF・MP4・対応MOVはアプリ内でプレビューできます。" => [
                "Preview supported GIF, MP4, and MOV files in the app.",
                "可在应用内预览受支持的 GIF、MP4 和 MOV 文件。",
                "可在 App 內預覽支援的 GIF、MP4 與 MOV 檔案。",
            ],
            "録画が見つかりません。保存先を確認するか「更新」を押してください。" => {
                [
                    "No recordings found. Check the save location or select Refresh.",
                    "未找到录屏。请检查保存位置或点按“刷新”。",
                    "找不到錄影。請檢查儲存位置或點按「重新整理」。",
                ]
            }
            "MOV形式は「外部で開く」から再生できます。" => [
                "MOV files can be played with Open Externally.",
                "MOV 文件可通过“外部打开”播放。",
                "MOV 檔案可透過「以外部程式開啟」播放。",
            ],
            "録画情報を読み込めません" => [
                "Unable to read recording information",
                "无法读取录屏信息",
                "無法讀取錄影資訊",
            ],
            "証跡一覧を更新しました" => [
                "Evidence list refreshed",
                "已更新取证记录列表",
                "已更新證據清單",
            ],
            "外部で開く" => ["Open Externally", "外部打开", "以外部程式開啟"],
            "⏸ 一時停止" => ["⏸ Pause", "⏸ 暂停", "⏸ 暫停"],
            "音量" => ["Volume", "音量", "音量"],
            "音声レベル" => ["Audio level", "音频电平", "音訊音量"],
            "音声の有無" => ["Audio by time", "音频时间分布", "音訊時間分布"],
            "音声あり" => ["Audio", "有声", "有聲"],
            "無音" => ["Silence", "静音", "靜音"],
            "音声データを解析中…" => {
                ["Analyzing audio…", "正在分析音频…", "正在分析音訊…"]
            }
            "音声データを表示できません" => [
                "Audio data unavailable",
                "无法显示音频数据",
                "無法顯示音訊資料",
            ],
            "音声データなし" => ["No audio data", "无音频数据", "沒有音訊資料"],
            "ミュート" => ["Mute", "静音", "靜音"],
            "ミュート解除" => ["Unmute", "取消静音", "取消靜音"],
            "MOV音声を準備しています…" => [
                "Preparing MOV audio…",
                "正在准备 MOV 音频…",
                "正在準備 MOV 音訊…",
            ],
            "GIFには音声がありません" => {
                ["GIF has no audio", "GIF 没有音频", "GIF 沒有音訊"]
            }
            "音声トラックなし、または音声出力を利用できません" => [
                "No audio track or audio output is unavailable",
                "没有音轨或音频输出不可用",
                "沒有音軌或音訊輸出無法使用",
            ],
            "▶ アプリ内再生" => ["▶ Play in App", "▶ 在应用内播放", "▶ 在 App 內播放"],
            "まだ画像がありません" => ["No images yet", "尚无图像", "尚無圖片"],
            "撮影した画像はここに並びます。保存先を変更した場合は設定を確認してください。" => {
                [
                    "Captured images appear here. Check Settings if you changed the save location.",
                    "截图会显示在这里。如果更改了保存位置，请检查设置。",
                    "截圖會顯示在這裡。如果變更了儲存位置，請檢查設定。",
                ]
            }
            "画像を編集" => ["Edit Image", "编辑图像", "編輯圖片"],
            "アプリの言語" => ["App language", "应用语言", "應用程式語言"],
            "言語表示を再読み込み" => [
                "Reload language display",
                "重新加载语言显示",
                "重新載入語言顯示",
            ],
            "言語表示を再読み込みしました。" => [
                "Language display reloaded.",
                "已重新加载语言显示。",
                "已重新載入語言顯示。",
            ],
            "MacShotを自分の撮影手順に合わせます。" => [
                "Configure MacShot for your capture workflow.",
                "根据你的截图流程配置 MacShot。",
                "依照你的擷取流程設定 MacShot。",
            ],
            "MacShot 範囲選択" => [
                "MacShot Region Selection",
                "MacShot 区域选择",
                "MacShot 區域選取",
            ],
            "画面全体またはウィンドウを撮影すると、ここにプレビューが表示されます。\n画像をドラッグして必要な範囲を切り出せます。" => {
                [
                    "Capture the full screen or a window to preview it here.\nDrag on the image to select a crop area.",
                    "截取整个屏幕或窗口后，预览会显示在这里。\n拖动图像即可选择裁剪区域。",
                    "擷取整個螢幕或視窗後，預覽會顯示於此。\n拖曳圖片即可選取裁切範圍。",
                ]
            }
            "↻  権限状態を更新" => [
                "↻  Refresh Permission Status",
                "↻  刷新权限状态",
                "↻  重新整理權限狀態",
            ],
            "macOS標準の撮影・録画を開く" => [
                "Open macOS Screenshot",
                "打开 macOS 截图",
                "開啟 macOS 螢幕截圖",
            ],
            "▣  保存先を変更" => [
                "▣  Change Save Location",
                "▣  更改保存位置",
                "▣  變更儲存位置",
            ],
            "起動中のMacShot" => ["Running MacShot", "正在运行的 MacShot", "執行中的 MacShot"],
            "元の画面サイズ" => ["Original Screen Size", "原始屏幕尺寸", "原始螢幕大小"],
            "画面収録の許可を設定してください。撮影画面の案内から要求できます。" => {
                [
                    "Set up Screen Recording permission from the capture screen guidance.",
                    "请根据截图页面的提示设置屏幕录制权限。",
                    "請依照截圖頁面的提示設定螢幕錄製權限。",
                ]
            }
            "録画を再生できませんでした。" => [
                "Could not play recording.",
                "无法播放录屏。",
                "無法播放錄影。",
            ],
            "録画の再生を開始できませんでした。" => [
                "Could not start recording playback.",
                "无法开始播放录屏。",
                "無法開始播放錄影。",
            ],
            "録画の再生が終わりました。もう一度再生できます。" => [
                "Recording playback ended. You can play it again.",
                "录屏播放已结束。你可以再次播放。",
                "錄影播放已結束。你可以再次播放。",
            ],
            "録画時間 00:00" => ["Recording time 00:00", "录制时间 00:00", "錄製時間 00:00"],
            "● REC 準備中" => ["● REC Preparing", "● REC 准备中", "● REC 準備中"],
            "キャンセル" => ["Cancel", "取消", "取消"],
            "保存して固定" => ["Save & Lock Layout", "保存并固定", "儲存並固定"],
            "カード配置を調整" => ["Arrange Cards", "调整卡片布局", "調整卡片配置"],
            "撮影設定を保存" => ["Save Capture Settings", "保存截图设置", "儲存截圖設定"],
            "カード配置" => ["Card Layout", "卡片布局", "卡片配置"],
            "上へ" => ["Move Up", "上移", "上移"],
            "下へ" => ["Move Down", "下移", "下移"],
            "録画設定" => ["Recording Settings", "录制设置", "錄製設定"],
            "保存形式" => ["Format", "格式", "格式"],
            "録画解像度" => ["Recording Resolution", "录制分辨率", "錄製解析度"],
            "選んだ高さを上限にして縮小します。元の画面より大きくはしません。" => {
                [
                    "The selected height is a maximum; output will not exceed the source display size.",
                    "所选高度为上限，输出不会大于原始屏幕。",
                    "所選高度為上限，輸出不會大於原始螢幕。",
                ]
            }
            "GIFの画質" => ["GIF Quality", "GIF 画质", "GIF 畫質"],
            "色の再現を上げるほど、変換に時間がかかることがあります。" => {
                [
                    "Higher color fidelity can increase conversion time.",
                    "色彩还原度越高，转换时间可能越长。",
                    "色彩還原度越高，轉換時間可能越長。",
                ]
            }
            "色の再現" => ["Color Fidelity", "色彩还原", "色彩還原"],
            "なめらかさ" => ["Frame Rate", "帧率", "影格率"],
            "GIFは音声なしです。" => [
                "GIF recordings do not include audio.",
                "GIF 录制不包含音频。",
                "GIF 錄製不包含音訊。",
            ],
            "画質・容量" => ["Quality / Size", "画质 / 容量", "畫質 / 容量"],
            "システム音声を録音し、マイク許可時はマイク音声も追加します。" => {
                [
                    "Records system audio and adds microphone audio when permission is granted.",
                    "录制系统音频；获得麦克风权限后也会录制麦克风音频。",
                    "錄製系統音訊；取得麥克風權限後也會錄製麥克風音訊。",
                ]
            }
            "撮影の動作" => ["Capture Behavior", "截图行为", "截圖行為"],
            "撮影対象や画像に記録する情報を調整できます。設定は次回起動後も引き継がれます。" => {
                [
                    "Choose capture targets and information saved in images. Settings persist across launches.",
                    "可设置截图目标和图像中记录的信息。设置会在下次启动后保留。",
                    "可設定截圖目標和圖片中記錄的資訊。設定會在下次啟動後保留。",
                ]
            }
            "撮影とファイル名" => ["Capture & Filename", "截图与文件名", "截圖與檔名"],
            "既定の撮影対象" => ["Default Capture Target", "默认截图目标", "預設截圖目標"],
            "ファイル名の先頭" => ["Filename Prefix", "文件名前缀", "檔名前綴"],
            "画像に撮影日時を表示" => [
                "Add Capture Date and Time to Image",
                "在图像中显示截图时间",
                "在圖片中顯示截圖時間",
            ],
            "タイムスタンプ形式" => ["Timestamp Format", "时间戳格式", "時間戳格式"],
            "日時" => ["Date and Time", "日期和时间", "日期和時間"],
            "日付のみ" => ["Date Only", "仅日期", "僅日期"],
            "時刻のみ" => ["Time Only", "仅时间", "僅時間"],
            "日時（ミリ秒）" => [
                "Date and Time (Milliseconds)",
                "日期和时间（毫秒）",
                "日期和時間（毫秒）",
            ],
            "日本語日時" => ["Japanese Date and Time", "日文日期和时间", "日文日期和時間"],
            "表示位置" => ["Position", "显示位置", "顯示位置"],
            "タイムスタンプ背景" => ["Timestamp Background", "时间戳背景", "時間戳背景"],
            "背景なし" => ["No Background", "无背景", "無背景"],
            "薄い背景" => ["Subtle Background", "浅色背景", "淺色背景"],
            "濃い背景" => ["Dark Background", "深色背景", "深色背景"],
            "◷  撮影時刻" => ["◷  Capture Time", "◷  截图时间", "◷  截圖時間"],
            "左上" => ["Top Left", "左上", "左上"],
            "右上" => ["Top Right", "右上", "右上"],
            "左下" => ["Bottom Left", "左下", "左下"],
            "右下" => ["Bottom Right", "右下", "右下"],
            "撮影後に完了小窓を表示" => [
                "Show Completion Notice After Capture",
                "截图后显示完成提示",
                "截圖後顯示完成提示",
            ],
            "macOS標準パネルの撮影結果を自動で取り込む" => [
                "Automatically Import Captures from macOS Screenshot",
                "自动导入 macOS 截图工具的结果",
                "自動匯入 macOS 螢幕截圖的結果",
            ],
            "オンにすると、MacShotから標準パネルを開いた後に保存された画像・録画を探し、保存先へコピーします。対象はデスクトップ、書類、ダウンロード、ピクチャ、ムービーです。" => {
                [
                    "After opening the macOS capture panel from MacShot, saved images and recordings are found and copied to the save location. Folders checked: Desktop, Documents, Downloads, Pictures, and Movies.",
                    "启用后，从 MacShot 打开 macOS 截图工具时，会查找保存的图像和录屏并复制到保存位置。扫描桌面、文稿、下载、图片和影片文件夹。",
                    "啟用後，從 MacShot 開啟 macOS 螢幕截圖時，會尋找已儲存的圖片和錄影並複製到儲存位置。掃描桌面、文件、下載項目、圖片和影片資料夾。",
                ]
            }
            "注釈" => ["Annotations", "标注", "註記"],
            "色" => ["Color", "颜色", "顏色"],
            "線の太さ" => ["Line Width", "线条粗细", "線條粗細"],
            "○  楕円" => ["○  Ellipse", "○  椭圆", "○  橢圓"],
            "T  テキスト" => ["T  Text", "T  文字", "T  文字"],
            "▦  モザイク" => ["▦  Mosaic", "▦  马赛克", "▦  馬賽克"],
            "注釈テキスト" => ["Annotation Text", "标注文字", "註記文字"],
            "保存先" => ["Save Location", "保存位置", "儲存位置"],
            "権限とデバイス" => ["Permissions & Devices", "权限与设备", "權限與裝置"],
            "許可済み" => ["Allowed", "已允许", "已允許"],
            "未許可、またはmacOSへの反映待ち" => [
                "Not allowed, or waiting for macOS to apply the change",
                "未允许，或等待 macOS 应用更改",
                "未允許，或等待 macOS 套用變更",
            ],
            "画面収録・システム音声" => [
                "Screen Recording & System Audio",
                "屏幕录制与系统音频",
                "螢幕錄製與系統音訊",
            ],
            "マイク" => ["Microphone", "麦克风", "麥克風"],
            "マイクテスト中…" => {
                ["Testing Microphone…", "正在测试麦克风…", "正在測試麥克風…"]
            }
            "マイク入力をテスト（5秒）" => [
                "Test Microphone Input (5 sec)",
                "测试麦克风输入（5 秒）",
                "測試麥克風輸入（5 秒）",
            ],
            "画面収録の一覧にMacShotがない場合は「＋」から追加できます。マイク一覧には追加ボタンがありません。MacShotから許可を要求すると一覧に表示されます。拒否済みで一覧にもない場合は「拒否記録を解除」を使ってください。" => {
                [
                    "If MacShot is missing from Screen Recording, add it with +. Microphone has no add button; request permission from MacShot to make it appear. If permission was denied and it is missing, reset the denied permission first.",
                    "如果屏幕录制列表中没有 MacShot，可点按“+”添加。麦克风列表没有添加按钮；请从 MacShot 请求权限以使其显示。如果权限已拒绝且列表中仍没有，请先重置已拒绝的权限。",
                    "如果螢幕錄製清單中沒有 MacShot，可點按「+」加入。麥克風清單沒有加入按鈕；請從 MacShot 要求權限，讓它顯示於清單中。如果權限已拒絕且清單中仍沒有，請先重設已拒絕的權限。",
                ]
            }
            "MP4は既定の再生デバイスとマイクを使います。Windowsのマイク設定でデスクトップアプリへのアクセスも確認してください。" => {
                [
                    "MP4 uses the default playback device and microphone. Check desktop app access in Windows microphone settings.",
                    "MP4 使用默认播放设备和麦克风。请在 Windows 麦克风设置中检查桌面应用访问权限。",
                    "MP4 使用預設播放裝置和麥克風。請在 Windows 麥克風設定中檢查桌面 App 存取權限。",
                ]
            }
            "状態未決定（MP4はシステム音声のみ）" => [
                "Not determined (MP4 records system audio only)",
                "尚未确定（MP4 仅录制系统音频）",
                "尚未決定（MP4 僅錄製系統音訊）",
            ],
            "拒否されています（MP4はシステム音声のみ）。マイク設定で許可してください" => {
                [
                    "Denied (MP4 records system audio only). Allow access in Microphone Settings.",
                    "已拒绝（MP4 仅录制系统音频）。请在麦克风设置中允许访问。",
                    "已拒絕（MP4 僅錄製系統音訊）。請在麥克風設定中允許存取。",
                ]
            }
            "このMacではマイク利用が制限されています" => [
                "Microphone access is restricted on this Mac",
                "此 Mac 的麦克风访问受到限制",
                "此 Mac 的麥克風存取受到限制",
            ],
            "既定のマイクが見つかりません" => [
                "No default microphone found",
                "找不到默认麦克风",
                "找不到預設麥克風",
            ],
            "Windowsでは個別の画面収録許可は不要です" => [
                "Windows does not require separate screen recording permission",
                "Windows 无需单独授予屏幕录制权限",
                "Windows 不需要個別授予螢幕錄製權限",
            ],
            "Windowsでは個別の画面収録許可は不要です。" => [
                "Windows does not require separate screen recording permission.",
                "Windows 无需单独授予屏幕录制权限。",
                "Windows 不需要個別授予螢幕錄製權限。",
            ],
            "このOSでは状態を確認できません" => [
                "Status is unavailable on this OS",
                "无法在此操作系统上查看状态",
                "無法在此作業系統上查看狀態",
            ],
            "macOSに許可を確認" => [
                "Request Permission from macOS",
                "向 macOS 请求权限",
                "向 macOS 要求權限",
            ],
            "設定でMacShotを許可" => [
                "Allow MacShot in Settings",
                "在设置中允许 MacShot",
                "在設定中允許 MacShot",
            ],
            "マイク設定を開く" => [
                "Open Microphone Settings",
                "打开麦克风设置",
                "開啟麥克風設定",
            ],
            "マイク設定を開きました。変更後に「権限状態を更新」を押してください。" => {
                [
                    "Opened Microphone Settings. Refresh the permission status after making changes.",
                    "已打开麦克风设置。更改后请刷新权限状态。",
                    "已開啟麥克風設定。變更後請重新整理權限狀態。",
                ]
            }
            "このMacではマイク利用が制限されています。管理者に確認してください。" => {
                [
                    "Microphone use is restricted on this Mac. Check with your administrator.",
                    "此 Mac 的麦克风使用受到限制。请联系管理员。",
                    "此 Mac 的麥克風使用受到限制。請洽詢管理者。",
                ]
            }
            "画面・音声の設定を開く" => [
                "Open Screen & Audio Settings",
                "打开屏幕与音频设置",
                "開啟螢幕與音訊設定",
            ],
            "拒否記録を解除" => [
                "Reset Denied Permission",
                "重置已拒绝的权限",
                "重設已拒絕的權限",
            ],
            "マイクの拒否記録を解除" => [
                "Reset Denied Microphone Permission",
                "重置已拒绝的麦克风权限",
                "重設已拒絕的麥克風權限",
            ],
            "MacShotだけのマイク拒否記録を解除して、macOSに許可を確認します。解除してもマイクは許可されません。確認画面が表示された場合は、そこで許可を選んでください。" => {
                [
                    "This resets only MacShot's denied microphone record and asks macOS for permission again. Resetting does not grant permission. If prompted, choose Allow.",
                    "此操作仅重置 MacShot 的麦克风拒绝记录并重新向 macOS 请求权限。重置不会直接授予权限；出现提示时请选择允许。",
                    "此操作僅重設 MacShot 的麥克風拒絕記錄，並重新向 macOS 要求權限。重設不會直接授予權限；出現提示時請選擇允許。",
                ]
            }
            "解除して許可を確認" => [
                "Reset and Request Permission",
                "重置并请求权限",
                "重設並要求權限",
            ],
            "5秒間テストします。マイクに向かって話してください。" => [
                "Test for 5 seconds. Speak into the microphone.",
                "测试 5 秒钟。请对着麦克风说话。",
                "測試 5 秒鐘。請對著麥克風說話。",
            ],
            "MacShotのウィンドウを表示しました。" => [
                "MacShot window is now visible.",
                "已显示 MacShot 窗口。",
                "已顯示 MacShot 視窗。",
            ],
            "macOS標準パネルを開きました。撮影後の画像・録画を保存先へ自動で取り込みます。" => {
                [
                    "Opened the macOS capture panel. Captures will be imported to the save location automatically.",
                    "已打开 macOS 截图面板。截图后会自动导入到保存位置。",
                    "已開啟 macOS 螢幕截圖面板。截圖後會自動匯入至儲存位置。",
                ]
            }
            "macOS標準の撮影パネルを開きました。撮影・録画の対象とオプションを選べます。" => {
                [
                    "Opened the macOS capture panel. Choose a capture target and options.",
                    "已打开 macOS 截图面板。可选择截图目标和选项。",
                    "已開啟 macOS 螢幕截圖面板。可選擇截圖目標和選項。",
                ]
            }
            "macOS標準の撮影パネルはmacOSで利用できます。" => [
                "The macOS capture panel is available on macOS.",
                "macOS 截图面板仅可在 macOS 上使用。",
                "macOS 螢幕截圖面板僅可在 macOS 上使用。",
            ],
            "画面収録の許可設定はmacOSで利用できます。" => [
                "Screen Recording settings are available on macOS.",
                "屏幕录制设置仅可在 macOS 上使用。",
                "螢幕錄製設定僅可在 macOS 上使用。",
            ],
            "マイク設定への移動はこのOSでは利用できません。" => [
                "Microphone Settings navigation is unavailable on this OS.",
                "此操作系统不支持打开麦克风设置。",
                "此作業系統不支援開啟麥克風設定。",
            ],
            "マイクは既に許可されています。" => [
                "Microphone access is already allowed.",
                "麦克风访问已获允许。",
                "麥克風存取已獲允許。",
            ],
            "マイクが拒否されています。設定でMacShotを許可してください。" => {
                [
                    "Microphone access is denied. Allow MacShot in Settings.",
                    "麦克风访问已被拒绝。请在设置中允许 MacShot。",
                    "麥克風存取已遭拒絕。請在設定中允許 MacShot。",
                ]
            }
            "Windowsのマイク設定を開きました。デスクトップアプリのアクセスを確認してください。" => {
                [
                    "Opened Windows microphone settings. Check desktop app access.",
                    "已打开 Windows 麦克风设置。请检查桌面应用访问权限。",
                    "已開啟 Windows 麥克風設定。請檢查桌面 App 存取權限。",
                ]
            }
            "録画を保存しています…" => {
                ["Saving recording…", "正在保存录屏…", "正在儲存錄影…"]
            }
            "録画をアプリ内で再生しています。" => [
                "Playing recording in the app.",
                "正在应用内播放录屏。",
                "正在 App 內播放錄影。",
            ],
            "録画を一時停止しました。" => [
                "Recording playback paused.",
                "录屏播放已暂停。",
                "錄影播放已暫停。",
            ],
            "連続撮影を終了しました。" => [
                "Continuous capture stopped.",
                "已停止连续截图。",
                "已停止連續截圖。",
            ],
            "範囲撮影をキャンセルしました。" => [
                "Region capture cancelled.",
                "已取消区域截图。",
                "已取消區域截圖。",
            ],
            "範囲選択の準備が時間切れになりました。画面収録の許可と他の撮影アプリを確認して、もう一度お試しください。" => {
                [
                    "Region selection timed out. Check Screen Recording permission and other capture apps, then try again.",
                    "区域选择准备超时。请检查屏幕录制权限和其他截图应用，然后重试。",
                    "區域選取準備逾時。請檢查螢幕錄製權限和其他截圖 App，然後再試一次。",
                ]
            }
            "言語設定を保存しました。" => [
                "Language preference saved.",
                "已保存语言设置。",
                "已儲存語言設定。",
            ],
            "カード配置を保存して固定しました。" => [
                "Card layout saved and locked.",
                "已保存并固定卡片布局。",
                "已儲存並固定卡片配置。",
            ],
            "表示設定を保存しました。" => [
                "Display settings saved.",
                "已保存显示设置。",
                "已儲存顯示設定。",
            ],
            "録画設定を保存しました。" => [
                "Recording settings saved.",
                "已保存录制设置。",
                "已儲存錄製設定。",
            ],
            "撮影設定を保存しました。" => [
                "Capture settings saved.",
                "已保存截图设置。",
                "已儲存截圖設定。",
            ],
            "ショートカットを保存しました。" => {
                ["Shortcuts saved.", "已保存快捷键。", "已儲存快速鍵。"]
            }
            "設定を保存できません" => {
                ["Unable to save settings", "无法保存设置", "無法儲存設定"]
            }
            "撮影完了" => ["Capture Complete", "截图完成", "截圖完成"],
            "●  撮影しました" => ["●  Capture Saved", "●  已保存截图", "●  已儲存截圖"],
            "録画を保存しました" => ["Recording Saved", "录屏已保存", "錄影已儲存"],
            "●  録画を保存しました" => {
                ["●  Recording Saved", "●  录屏已保存", "●  錄影已儲存"]
            }
            "で続けて撮影できます" => {
                ["Press to capture again", "可按此继续截图", "可按此繼續截圖"]
            }
            "ドラッグして撮影範囲を選択　·　Escでキャンセル" => [
                "Drag to select a capture region · Esc to cancel",
                "拖动以选择截图区域 · 按 Esc 取消",
                "拖曳以選取截圖範圍 · 按 Esc 取消",
            ],
            "ドラッグして撮影範囲を選択してください。Escでキャンセルできます。" => {
                [
                    "Drag to select a capture region. Press Esc to cancel.",
                    "请拖动以选择截图区域。按 Esc 可取消。",
                    "請拖曳以選取截圖範圍。按 Esc 可取消。",
                ]
            }
            "範囲選択を準備しています…画面の確認に時間がかかる場合は自動で戻ります。" => {
                [
                    "Preparing region selection… It will return automatically if screen inspection takes too long.",
                    "正在准备区域选择…如果屏幕检查耗时过长，将自动返回。",
                    "正在準備區域選取…如果螢幕檢查耗時過長，將自動返回。",
                ]
            }
            "録画または撮影が終わってから範囲撮影を開始してください。" => {
                [
                    "Wait for recording or capture to finish before selecting a region.",
                    "请等待录制或截图完成后再选择区域。",
                    "請等待錄製或截圖完成後再選取範圍。",
                ]
            }
            "録画を終了してからスクリーンショットを撮影してください。" => {
                [
                    "Stop recording before taking a screenshot.",
                    "请先停止录制再截图。",
                    "請先停止錄製再截圖。",
                ]
            }
            "選択範囲が小さすぎます。ドラッグして範囲を選択してください。" => {
                [
                    "The selection is too small. Drag to select a larger region.",
                    "选区太小。请拖动以选择更大的区域。",
                    "選取範圍太小。請拖曳以選取更大的範圍。",
                ]
            }
            "切り出す範囲が小さすぎます。もう一度ドラッグしてください。" => {
                [
                    "The crop area is too small. Drag again.",
                    "裁剪区域太小。请重新拖动。",
                    "裁切範圍太小。請重新拖曳。",
                ]
            }
            "先にマイクを許可してください。" => [
                "Allow microphone access first.",
                "请先允许麦克风访问。",
                "請先允許麥克風存取。",
            ],
            "マイク入力を検出できませんでした。入力デバイスと入力音量を確認してください。" => {
                [
                    "No microphone input detected. Check the input device and input volume.",
                    "未检测到麦克风输入。请检查输入设备和输入音量。",
                    "未偵測到麥克風輸入。請檢查輸入裝置和輸入音量。",
                ]
            }
            "設定でMacShotをオンにして再起動してください。オンでも許可されない場合は、一度オフ→オンして再起動します。" => {
                [
                    "Enable MacShot in Settings and restart. If access is still denied, turn it off and on, then restart.",
                    "请在设置中启用 MacShot 并重新启动。如果仍未获允许，请先关闭再开启，然后重新启动。",
                    "請在設定中啟用 MacShot 並重新啟動。如果仍未獲允許，請先關閉再開啟，然後重新啟動。",
                ]
            }
            "画面収録の許可を確認しました。反映のためMacShotを再起動してください。" => {
                [
                    "Screen Recording permission is confirmed. Restart MacShot to apply it.",
                    "已确认屏幕录制权限。请重新启动 MacShot 以应用更改。",
                    "已確認螢幕錄製權限。請重新啟動 MacShot 以套用變更。",
                ]
            }
            "画面収録の許可設定を開きました。MacShotをオンにして再起動してください。" => {
                [
                    "Opened Screen Recording settings. Enable MacShot and restart it.",
                    "已打开屏幕录制设置。请启用 MacShot 并重新启动。",
                    "已開啟螢幕錄製設定。請啟用 MacShot 並重新啟動。",
                ]
            }
            "macOSにマイクの許可を確認しています。表示された画面で選択してください。" => {
                [
                    "Requesting microphone permission from macOS. Choose an option in the prompt.",
                    "正在向 macOS 请求麦克风权限。请在显示的提示中选择。",
                    "正在向 macOS 要求麥克風權限。請在顯示的提示中選擇。",
                ]
            }
            "MacShot.appの場所を特定できません。アプリケーション版からお試しください。" => {
                [
                    "Could not locate MacShot.app. Try the application bundle.",
                    "找不到 MacShot.app。请尝试从应用程序包运行。",
                    "找不到 MacShot.app。請嘗試從 App 套件執行。",
                ]
            }
            "MacShotのアプリ識別子を確認できず、マイク許可をリセットできません。" => {
                [
                    "Could not determine MacShot's bundle identifier; microphone permission was not reset.",
                    "无法确定 MacShot 的应用标识符，因此未重置麦克风权限。",
                    "無法確認 MacShot 的 App 識別碼，因此未重設麥克風權限。",
                ]
            }
            "録画を保存しました ({finished_at})" => [
                "Recording saved ({finished_at})",
                "录屏已保存（{finished_at}）",
                "錄影已儲存（{finished_at}）",
            ],
            "コンパクト" => ["Compact", "紧凑", "精簡"],
            "日時不明" => ["Date and time unknown", "日期和时间未知", "日期和時間未知"],
            "画像サイズ: " => ["Image size: ", "图像尺寸：", "圖片尺寸："],
            "文字サイズ" => ["Text Size", "文字大小", "文字大小"],
            "表示量" => ["Density", "显示密度", "顯示密度"],
            "表示" => ["Display", "显示", "顯示"],
            "標準" => ["Standard", "标准", "標準"],
            "ゆったり" => ["Relaxed", "宽松", "寬鬆"],
            "文字サイズと画面の密度" => [
                "Text Size & Density",
                "文字大小与显示密度",
                "文字大小與顯示密度",
            ],
            "コンパクトにすると一画面に多く表示し、文字サイズを上げると全体を拡大します。" => {
                [
                    "Compact density shows more at once. Increase text size to enlarge the interface.",
                    "紧凑模式可在一屏显示更多内容。增大文字可放大整个界面。",
                    "精簡模式可在一個畫面顯示更多內容。調大文字可放大整個介面。",
                ]
            }
            "配置" => ["Layout", "布局", "配置"],
            "↑ 上へ" => ["↑ Move Up", "↑ 上移", "↑ 上移"],
            "↓ 下へ" => ["↓ Move Down", "↓ 下移", "↓ 下移"],
            "幅: 半分" => ["Width: Half", "宽度：一半", "寬度：一半"],
            "幅: 全体" => ["Width: Full", "宽度：全宽", "寬度：全寬"],
            "余白: 小" => ["Padding: Small", "边距：小", "邊距：小"],
            "余白: 中" => ["Padding: Medium", "边距：中", "邊距：中"],
            "余白: 大" => ["Padding: Large", "边距：大", "邊距：大"],
            "赤色" => ["Red", "红色", "紅色"],
            "黄色" => ["Yellow", "黄色", "黃色"],
            "青色" => ["Blue", "蓝色", "藍色"],
            "スクリーンショット" => ["Screenshot", "屏幕截图", "螢幕截圖"],
            "録画の開始／終了" => [
                "Start / Stop Recording",
                "开始 / 停止录制",
                "開始 / 停止錄製",
            ],
            "どのアプリを使っていても呼び出せます。設定後すぐに反映されます." => {
                [
                    "Available from any app. Changes take effect immediately.",
                    "可在任何应用中调用。设置后立即生效。",
                    "可在任何 App 中呼叫。設定後立即生效。",
                ]
            }
            "例: Alt+Shift+KeyS、Control+Alt+KeyF、Super+Shift+Key4" => [
                "Example: Alt+Shift+KeyS, Control+Alt+KeyF, Super+Shift+Key4",
                "示例：Alt+Shift+KeyS、Control+Alt+KeyF、Super+Shift+Key4",
                "範例：Alt+Shift+KeyS、Control+Alt+KeyF、Super+Shift+Key4",
            ],
            "macOSのAltはOptionキーです。WindowsではAltキーとして動作します。" => {
                [
                    "Alt is Option on macOS and Alt on Windows.",
                    "Alt 在 macOS 上对应 Option，在 Windows 上对应 Alt。",
                    "Alt 在 macOS 上對應 Option，在 Windows 上對應 Alt。",
                ]
            }
            "設定を保存して反映" => [
                "Save and Apply Settings",
                "保存并应用设置",
                "儲存並套用設定",
            ],
            "予想ファイルサイズ（1分録画）" => [
                "Estimated File Size (1 min)",
                "预计文件大小（录制 1 分钟）",
                "預估檔案大小（錄製 1 分鐘）",
            ],
            "映像ビットレート・AAC音声・コンテナ分を含む目安です。" => [
                "Estimate includes video bitrate, AAC audio, and container overhead.",
                "估算包含视频比特率、AAC 音频和容器开销。",
                "估算包含視訊位元率、AAC 音訊和容器額外資料。",
            ],
            "画面の動きや色で大きく変わります。解像度とfpsからの概算です。" => {
                [
                    "Varies with motion and color; estimated from resolution and frame rate.",
                    "大小会因画面运动和颜色而异；根据分辨率和帧率估算。",
                    "大小會因畫面動態和色彩而異；依解析度和影格率估算。",
                ]
            }
            "メイン画面を仮定" => {
                ["Main display assumed", "假设为主显示器", "假設為主要顯示器"]
            }
            "録画件数（GIF / MP4 / MOV）" => [
                "recordings (GIF / MP4 / MOV)",
                "个录屏（GIF / MP4 / MOV）",
                "個錄影（GIF / MP4 / MOV）",
            ],
            "基準画像" => ["Reference Image", "基准图像", "參考圖片"],
            "比較画像" => ["Comparison Image", "比较图像", "比較圖片"],
            "約" => ["About", "约", "約"],
            _ => return japanese.to_owned(),
        };
        translated[match self {
            Self::Japanese => unreachable!(),
            Self::English => 0,
            Self::SimplifiedChinese => 1,
            Self::TraditionalChinese => 2,
        }]
        .to_owned()
    }

    fn message(self, japanese: &str) -> String {
        let exact = self.text(japanese);
        if exact != japanese {
            return exact;
        }
        if self == Self::Japanese {
            return japanese.to_owned();
        }

        const PREFIXES: &[(&str, [&str; 3])] = &[
            (
                "撮影に失敗しました: ",
                ["Capture failed: ", "截图失败：", "截圖失敗："],
            ),
            (
                "撮影できません: ",
                ["Unable to capture: ", "无法截图：", "無法截圖："],
            ),
            (
                "画面を撮影できません: ",
                [
                    "Unable to capture screen: ",
                    "无法截取屏幕：",
                    "無法擷取螢幕：",
                ],
            ),
            (
                "画面を確認できません: ",
                [
                    "Unable to inspect screen: ",
                    "无法检查屏幕：",
                    "無法檢查螢幕：",
                ],
            ),
            (
                "画面収録の許可設定を開けません: ",
                [
                    "Unable to open Screen Recording settings: ",
                    "无法打开屏幕录制设置：",
                    "無法開啟螢幕錄製設定：",
                ],
            ),
            (
                "画面収録の許可設定を開けませんでした (",
                [
                    "Could not open Screen Recording settings (",
                    "无法打开屏幕录制设置（",
                    "無法開啟螢幕錄製設定（",
                ],
            ),
            (
                "マイク設定を開けません: ",
                [
                    "Unable to open Microphone settings: ",
                    "无法打开麦克风设置：",
                    "無法開啟麥克風設定：",
                ],
            ),
            (
                "マイク設定を開けませんでした (",
                [
                    "Could not open Microphone settings (",
                    "无法打开麦克风设置（",
                    "無法開啟麥克風設定（",
                ],
            ),
            (
                "Finderでアプリを表示できません: ",
                [
                    "Unable to reveal app in Finder: ",
                    "无法在 Finder 中显示应用：",
                    "無法在 Finder 中顯示 App：",
                ],
            ),
            (
                "Finderでアプリを表示できませんでした (",
                [
                    "Could not reveal app in Finder (",
                    "无法在 Finder 中显示应用（",
                    "無法在 Finder 中顯示 App（",
                ],
            ),
            (
                "macOS標準の撮影パネルを開けません: ",
                [
                    "Unable to open the macOS Screenshot panel: ",
                    "无法打开 macOS 截图面板：",
                    "無法開啟 macOS 螢幕截圖面板：",
                ],
            ),
            (
                "macOS標準の撮影パネルを開けませんでした (",
                [
                    "Could not open the macOS Screenshot panel (",
                    "无法打开 macOS 截图面板（",
                    "無法開啟 macOS 螢幕截圖面板（",
                ],
            ),
            (
                "画像を開けません: ",
                ["Unable to open image: ", "无法打开图像：", "無法開啟圖片："],
            ),
            (
                "録画を読み込めませんでした: ",
                [
                    "Unable to load recording: ",
                    "无法加载录屏：",
                    "無法載入錄影：",
                ],
            ),
            (
                "録画を再生できませんでした: ",
                [
                    "Unable to play recording: ",
                    "无法播放录屏：",
                    "無法播放錄影：",
                ],
            ),
            (
                "保存先を作成できません: ",
                [
                    "Unable to create save location: ",
                    "无法创建保存位置：",
                    "無法建立儲存位置：",
                ],
            ),
            (
                "設定を保存できません: ",
                [
                    "Unable to save settings: ",
                    "无法保存设置：",
                    "無法儲存設定：",
                ],
            ),
            (
                "表示設定を保存できません: ",
                [
                    "Unable to save display settings: ",
                    "无法保存显示设置：",
                    "無法儲存顯示設定：",
                ],
            ),
            (
                "録画設定を保存できません: ",
                [
                    "Unable to save recording settings: ",
                    "无法保存录制设置：",
                    "無法儲存錄製設定：",
                ],
            ),
            (
                "撮影設定を保存できません: ",
                [
                    "Unable to save capture settings: ",
                    "无法保存截图设置：",
                    "無法儲存截圖設定：",
                ],
            ),
            (
                "録画形式を保存できません: ",
                [
                    "Unable to save recording format: ",
                    "无法保存录制格式：",
                    "無法儲存錄製格式：",
                ],
            ),
            (
                "録画解像度を保存できません: ",
                [
                    "Unable to save recording resolution: ",
                    "无法保存录制分辨率：",
                    "無法儲存錄製解析度：",
                ],
            ),
            (
                "言語設定を保存できません: ",
                [
                    "Unable to save language setting: ",
                    "无法保存语言设置：",
                    "無法儲存語言設定：",
                ],
            ),
            (
                "カード配置を保存できません: ",
                [
                    "Unable to save card layout: ",
                    "无法保存卡片布局：",
                    "無法儲存卡片配置：",
                ],
            ),
            (
                "ショートカットを初期化できません: ",
                [
                    "Unable to initialize shortcuts: ",
                    "无法初始化快捷键：",
                    "無法初始化快速鍵：",
                ],
            ),
            (
                "注釈画像を保存できません: ",
                [
                    "Unable to save annotated image: ",
                    "无法保存标注图像：",
                    "無法儲存註記圖片：",
                ],
            ),
            (
                "切り出し画像を保存できません: ",
                [
                    "Unable to save cropped image: ",
                    "无法保存裁剪图像：",
                    "無法儲存裁切圖片：",
                ],
            ),
            (
                "録画を保存できません: ",
                [
                    "Unable to save recording: ",
                    "无法保存录屏：",
                    "無法儲存錄影：",
                ],
            ),
            (
                "撮影メニューを作成できません: ",
                [
                    "Unable to create capture menu: ",
                    "无法创建截图菜单：",
                    "無法建立截圖選單：",
                ],
            ),
            (
                "メニューバーのメニューを作成できません: ",
                [
                    "Unable to create menu bar menu: ",
                    "无法创建菜单栏菜单：",
                    "無法建立選單列選單：",
                ],
            ),
            (
                "メニューバーを表示できません: ",
                [
                    "Unable to show menu bar: ",
                    "无法显示菜单栏：",
                    "無法顯示選單列：",
                ],
            ),
            (
                "メニューバーアイコンを更新できません: ",
                [
                    "Unable to update menu bar icon: ",
                    "无法更新菜单栏图标：",
                    "無法更新選單列圖示：",
                ],
            ),
            (
                "録画を読み込みました: ",
                ["Loaded recording: ", "已加载录屏：", "已載入錄影："],
            ),
            (
                "撮影済み画像を保存しました: ",
                ["Saved capture: ", "已保存截图：", "已儲存截圖："],
            ),
            (
                "切り出して保存しました: ",
                ["Cropped and saved: ", "已裁剪并保存：", "已裁切並儲存："],
            ),
            (
                "注釈付き画像を保存しました: ",
                [
                    "Saved annotated image: ",
                    "已保存标注图像：",
                    "已儲存註記圖片：",
                ],
            ),
            ("保存先: ", ["Save location: ", "保存位置：", "儲存位置："]),
            ("読み込みました: ", ["Loaded: ", "已加载：", "已載入："]),
        ];

        for (prefix, translated) in PREFIXES {
            if let Some(suffix) = japanese.strip_prefix(prefix) {
                let localized = translated[match self {
                    Self::Japanese => unreachable!(),
                    Self::English => 0,
                    Self::SimplifiedChinese => 1,
                    Self::TraditionalChinese => 2,
                }];
                return format!("{localized}{suffix}");
            }
        }
        if let Some((count, _)) = japanese.split_once("枚目を保存しています…")
            && let Some(count) = count.strip_prefix("撮影 ")
        {
            let template = match self {
                Self::Japanese => unreachable!(),
                Self::English => "Saving capture #{}…",
                Self::SimplifiedChinese => "正在保存第 {} 张截图…",
                Self::TraditionalChinese => "正在儲存第 {} 張截圖…",
            };
            return template.replacen("{}", count.trim(), 1);
        }
        if let Some(count) = japanese.strip_suffix("個のウィンドウを確認しました。")
        {
            let template = match self {
                Self::Japanese => unreachable!(),
                Self::English => "Found {} windows.",
                Self::SimplifiedChinese => "已找到 {} 个窗口。",
                Self::TraditionalChinese => "已找到 {} 個視窗。",
            };
            return template.replacen("{}", count, 1);
        }
        if let Some(count) = japanese.strip_suffix(" 枚の画像") {
            let template = match self {
                Self::Japanese => unreachable!(),
                Self::English => "{} images",
                Self::SimplifiedChinese => "{} 张图像",
                Self::TraditionalChinese => "{} 張圖片",
            };
            return template.replacen("{}", count, 1);
        }
        if let Some(value) = japanese
            .strip_prefix("録画形式を")
            .and_then(|message| message.strip_suffix("に変更しました"))
        {
            let template = match self {
                Self::Japanese => unreachable!(),
                Self::English => "Recording format changed to {}",
                Self::SimplifiedChinese => "录制格式已更改为 {}",
                Self::TraditionalChinese => "錄製格式已變更為 {}",
            };
            return template.replacen("{}", value, 1);
        }
        if let Some(value) = japanese
            .strip_prefix("録画解像度を")
            .and_then(|message| message.strip_suffix("に変更しました"))
        {
            let template = match self {
                Self::Japanese => unreachable!(),
                Self::English => "Recording resolution changed to {}",
                Self::SimplifiedChinese => "录制分辨率已更改为 {}",
                Self::TraditionalChinese => "錄製解析度已變更為 {}",
            };
            return template.replacen("{}", value, 1);
        }
        if let Some(rest) = japanese.strip_prefix("撮影 ")
            && let Some((sequence, details)) = rest.split_once(" 枚目を保存しました (")
            && let Some((timestamp, size)) = details.split_once(") · ")
        {
            let template = match self {
                Self::Japanese => unreachable!(),
                Self::English => "Capture #{} saved ({}) · {}",
                Self::SimplifiedChinese => "第 {} 张截图已保存（{}）· {}",
                Self::TraditionalChinese => "第 {} 張截圖已儲存（{}）· {}",
            };
            return template
                .replacen("{}", sequence, 1)
                .replacen("{}", timestamp, 1)
                .replacen("{}", size, 1);
        }

        const FRAGMENTS: &[(&str, [&str; 3])] = &[
            (
                "画面収録の許可をmacOSに要求しました。設定でMacShotをオンにしてください。反映しない場合はメニューバーのMacShotから終了して再起動してください。",
                [
                    "Requested Screen Recording access. Enable MacShot in Settings, then quit from the menu-bar MacShot menu and reopen it if access does not update.",
                    "已请求屏幕录制权限。请在设置中启用 MacShot；如果权限未更新，请从菜单栏 MacShot 菜单退出后重新打开。",
                    "已要求螢幕錄製權限。請在設定中啟用 MacShot；如果權限未更新，請從選單列 MacShot 選單結束後重新開啟。",
                ],
            ),
            (
                "画面収録の許可を確認しました。",
                [
                    "Screen Recording access is available.",
                    "屏幕录制权限已生效。",
                    "螢幕錄製權限已生效。",
                ],
            ),
            (
                "システム設定 > プライバシーとセキュリティ > 画面収録とシステムオーディオ録音",
                [
                    "System Settings > Privacy & Security > Screen & System Audio Recording",
                    "系统设置 > 隐私与安全性 > 屏幕录制与系统音频录制",
                    "系統設定 > 隱私權與安全性 > 螢幕與系統音訊錄製",
                ],
            ),
            (
                "画面収録の許可設定を開きました。MacShotをオンにして再起動してください。",
                [
                    "Opened Screen Recording settings. Enable MacShot and restart it.",
                    "已打开屏幕录制设置。请启用 MacShot 并重新启动。",
                    "已開啟螢幕錄製設定。請啟用 MacShot 並重新啟動。",
                ],
            ),
            (
                "画面収録の許可を確認しました。反映のためMacShotを再起動してください。",
                [
                    "Screen Recording permission is confirmed. Restart MacShot to apply it.",
                    "已确认屏幕录制权限。请重新启动 MacShot 以应用更改。",
                    "已確認螢幕錄製權限。請重新啟動 MacShot 以套用變更。",
                ],
            ),
            (
                "拒否されています（MP4はシステム音声のみ）。マイク設定で許可してください",
                [
                    "Denied (MP4 records system audio only). Allow in Microphone Settings.",
                    "已拒绝（MP4 仅录制系统音频）。请在麦克风设置中允许访问。",
                    "已拒絕（MP4 僅錄製系統音訊）。請在麥克風設定中允許存取。",
                ],
            ),
            (
                "MacShotのマイク許可状態をリセットしました。許可確認を表示します。",
                [
                    "Reset MacShot microphone permission. A new permission prompt will appear.",
                    "已重置 MacShot 麦克风权限。将显示新的权限提示。",
                    "已重設 MacShot 麥克風權限。將顯示新的權限提示。",
                ],
            ),
            (
                "始点を指定しました。終点をクリックするかドラッグしてください。",
                [
                    "Start point set. Click or drag to set the end point.",
                    "已设置起点。请点按或拖动以设置终点。",
                    "已設定起點。請點按或拖曳以設定終點。",
                ],
            ),
            (
                "オンでも許可されない場合は、一度オフ→オンして再起動します。",
                [
                    "If still denied, turn it off and on, then restart.",
                    "如果仍未获允许，请先关闭再开启，然后重新启动。",
                    "如果仍未獲允許，請先關閉再開啟，然後重新啟動。",
                ],
            ),
            (
                "MacShotのマイク許可状態をリセットできませんでした (",
                [
                    "Could not reset MacShot microphone permission (",
                    "无法重置 MacShot 麦克风权限（",
                    "無法重設 MacShot 麥克風權限（",
                ],
            ),
            (
                "システム設定 > プライバシーとセキュリティ > マイク",
                [
                    "System Settings > Privacy & Security > Microphone",
                    "系统设置 > 隐私与安全性 > 麦克风",
                    "系統設定 > 隱私權與安全性 > 麥克風",
                ],
            ),
            (
                "録画を終了してからスクリーンショットを撮影してください。",
                [
                    "Stop recording before taking a screenshot.",
                    "请先停止录制再截图。",
                    "請先停止錄製再截圖。",
                ],
            ),
            (
                "FinderでMacShot.appを表示しました: ",
                [
                    "Revealed MacShot.app in Finder: ",
                    "已在 Finder 中显示 MacShot.app：",
                    "已在 Finder 中顯示 MacShot.app：",
                ],
            ),
            (
                "MacShotのマイク許可状態をリセットできません: ",
                [
                    "Could not reset MacShot microphone permission: ",
                    "无法重置 MacShot 麦克风权限：",
                    "無法重設 MacShot 麥克風權限：",
                ],
            ),
            (
                "MacShotのウィンドウを表示しました。",
                [
                    "MacShot window is now visible.",
                    "MacShot 窗口已显示。",
                    "MacShot 視窗已顯示。",
                ],
            ),
            (
                "状態未決定（MP4はシステム音声のみ）",
                [
                    "Not determined (MP4 records system audio only)",
                    "尚未确定（MP4 仅录制系统音频）",
                    "尚未決定（MP4 僅錄製系統音訊）",
                ],
            ),
            (
                "「画面収録」の許可を確認してください。",
                [
                    "Check Screen Recording permission.",
                    "请检查屏幕录制权限。",
                    "請檢查螢幕錄製權限。",
                ],
            ),
            (
                "macOSから画面収録を拒否されました",
                [
                    "macOS denied Screen Recording access",
                    "macOS 已拒绝屏幕录制权限",
                    "macOS 已拒絕螢幕錄製權限",
                ],
            ),
            (
                "マイクテストを開始できませんでした: ",
                [
                    "Could not start microphone test: ",
                    "无法开始麦克风测试：",
                    "無法開始麥克風測試：",
                ],
            ),
            (
                "未許可、またはmacOSへの反映待ち",
                [
                    "Not allowed or waiting for macOS",
                    "未允许，或等待 macOS 应用更改",
                    "未允許，或等待 macOS 套用變更",
                ],
            ),
            (
                "アクティブウィンドウが見つかりません",
                [
                    "No active window found",
                    "找不到活动窗口",
                    "找不到作用中視窗",
                ],
            ),
            (
                "カード配置を保存して固定しました。",
                [
                    "Card layout saved and locked.",
                    "卡片布局已保存并固定。",
                    "卡片配置已儲存並固定。",
                ],
            ),
            (
                "範囲選択の背景を撮影できません: ",
                [
                    "Unable to capture region selection background: ",
                    "无法截取区域选择背景：",
                    "無法擷取區域選取背景：",
                ],
            ),
            (
                "マイクの使用が拒否されています。",
                [
                    "Microphone access was denied.",
                    "麦克风访问已被拒绝。",
                    "麥克風存取已遭拒絕。",
                ],
            ),
            (
                "画面収録の許可を設定してください",
                [
                    "Set up Screen Recording permission",
                    "请设置屏幕录制权限",
                    "請設定螢幕錄製權限",
                ],
            ),
            (
                "ショートカットを保存しました。",
                ["Shortcuts saved.", "快捷键已保存。", "快速鍵已儲存。"],
            ),
            (
                "撮影できる画面が見つかりません",
                [
                    "No capturable display found",
                    "找不到可截图的显示器",
                    "找不到可截圖的顯示器",
                ],
            ),
            (
                "選択した画面が見つかりません",
                [
                    "Selected display not found",
                    "找不到所选显示器",
                    "找不到所選顯示器",
                ],
            ),
            (
                "録画または撮影が終わってから",
                [
                    "Wait for recording or capture to finish before",
                    "请等待录制或截图完成后再",
                    "請等待錄製或截圖完成後再",
                ],
            ),
            (
                "撮影を順番待ちに追加しました",
                ["Capture added to queue", "截图已加入队列", "截圖已加入佇列"],
            ),
            (
                "切り出し画像を保存できません",
                [
                    "Unable to save cropped image",
                    "无法保存裁剪图像",
                    "無法儲存裁切圖片",
                ],
            ),
            (
                "録画を再生できませんでした",
                ["Unable to play recording", "无法播放录屏", "無法播放錄影"],
            ),
            (
                "録画を読み込めませんでした",
                ["Unable to load recording", "无法加载录屏", "無法載入錄影"],
            ),
            (
                "録画解像度を保存できません",
                [
                    "Unable to save recording resolution",
                    "无法保存录制分辨率",
                    "無法儲存錄製解析度",
                ],
            ),
            (
                "切り出す範囲が小さすぎます",
                ["Crop area is too small", "裁剪区域太小", "裁切範圍太小"],
            ),
            (
                "注釈付き画像を保存しました",
                ["Saved annotated image", "已保存标注图像", "已儲存註記圖片"],
            ),
            (
                "録画形式を保存できません",
                [
                    "Unable to save recording format",
                    "无法保存录制格式",
                    "無法儲存錄製格式",
                ],
            ),
            (
                "録画設定を保存できません",
                [
                    "Unable to save recording settings",
                    "无法保存录制设置",
                    "無法儲存錄製設定",
                ],
            ),
            (
                "撮影設定を保存できません",
                [
                    "Unable to save capture settings",
                    "无法保存截图设置",
                    "無法儲存截圖設定",
                ],
            ),
            (
                "表示設定を保存できません",
                [
                    "Unable to save display settings",
                    "无法保存显示设置",
                    "無法儲存顯示設定",
                ],
            ),
            (
                "言語設定を保存できません",
                [
                    "Unable to save language preference",
                    "无法保存语言设置",
                    "無法儲存語言設定",
                ],
            ),
            (
                "言語設定を保存しました。",
                [
                    "Language preference saved.",
                    "已保存语言设置。",
                    "已儲存語言設定。",
                ],
            ),
            (
                "撮影設定を保存しました。",
                [
                    "Capture settings saved.",
                    "截图设置已保存。",
                    "截圖設定已儲存。",
                ],
            ),
            (
                "表示設定を保存しました。",
                [
                    "Display settings saved.",
                    "显示设置已保存。",
                    "顯示設定已儲存。",
                ],
            ),
            (
                "録画設定を保存しました。",
                [
                    "Recording settings saved.",
                    "录制设置已保存。",
                    "錄製設定已儲存。",
                ],
            ),
            (
                "編集する画像を開きました",
                [
                    "Opened image for editing",
                    "已打开图像以进行编辑",
                    "已開啟圖片以供編輯",
                ],
            ),
            (
                "注釈画像を保存できません",
                [
                    "Unable to save annotated image",
                    "无法保存标注图像",
                    "無法儲存註記圖片",
                ],
            ),
            (
                "マイク入力を検出しました",
                [
                    "Microphone input detected",
                    "已检测到麦克风输入",
                    "已偵測到麥克風輸入",
                ],
            ),
            (
                "連続撮影を開始しました。",
                [
                    "Continuous capture started.",
                    "已开始连续截图。",
                    "已開始連續截圖。",
                ],
            ),
            (
                "画面収録・システム音声",
                [
                    "Screen Recording & System Audio",
                    "屏幕录制与系统音频",
                    "螢幕錄製與系統音訊",
                ],
            ),
            (
                "保存先を作成できません",
                [
                    "Unable to create save location",
                    "无法创建保存位置",
                    "無法建立儲存位置",
                ],
            ),
            (
                "切り出して保存しました",
                ["Cropped and saved", "已裁剪并保存", "已裁切並儲存"],
            ),
            (
                "録画を保存しました (",
                ["Recording saved (", "录屏已保存（", "錄影已儲存（"],
            ),
            (
                "録画を開始しています（",
                ["Starting recording (", "正在开始录制（", "正在開始錄製（"],
            ),
            (
                "撮影できませんでした",
                ["Capture failed", "截图失败", "截圖失敗"],
            ),
            (
                "録画できませんでした",
                ["Recording failed", "录制失败", "錄製失敗"],
            ),
            (
                "録画を読み込みました",
                ["Loaded recording", "已加载录屏", "已載入錄影"],
            ),
            (
                "設定を保存できません",
                ["Unable to save settings", "无法保存设置", "無法儲存設定"],
            ),
            (
                "設定を保存しました。",
                ["Settings saved.", "设置已保存。", "設定已儲存。"],
            ),
            (
                "録画を開始しています",
                ["Starting recording", "正在开始录制", "正在開始錄製"],
            ),
            (
                "前回の連続撮影 — ",
                [
                    "Last continuous capture — ",
                    "上次连续截图 — ",
                    "上次連續截圖 — ",
                ],
            ),
            ("読み込みました: ", ["Loaded: ", "已加载：", "已載入："]),
            (
                "撮影終了待ち — ",
                [
                    "Waiting for capture to finish — ",
                    "等待截图完成 — ",
                    "等待截圖完成 — ",
                ],
            ),
            (
                "録画を終了してから",
                [
                    "Stop recording before",
                    "请先停止录制，再",
                    "請先停止錄製，再",
                ],
            ),
            (
                "画像を開けません",
                ["Unable to open image", "无法打开图像", "無法開啟圖片"],
            ),
            (
                "連続撮影中 — ",
                ["Continuous capture — ", "连续截图 — ", "連續截圖 — "],
            ),
            ("画像サイズ: ", ["Image size: ", "图像尺寸：", "圖片尺寸："]),
            (
                "録画解像度を",
                ["Recording resolution ", "录制分辨率", "錄製解析度"],
            ),
            ("録画中 — ", ["Recording — ", "录制中 — ", "錄製中 — "]),
            (
                "枚保存・失敗",
                [" saved · failed", " 张已保存 · 失败", " 張已儲存 · 失敗"],
            ),
            ("録画形式を", ["Recording format ", "录制格式", "錄製格式"]),
            (
                "連続撮影中",
                [
                    "Continuous capture active",
                    "连续截图进行中",
                    "連續截圖進行中",
                ],
            ),
            ("録画中 —", ["Recording —", "录制中 —", "錄製中 —"]),
            ("保存先: ", ["Save location: ", "保存位置：", "儲存位置："]),
            ("枚保存済み", [" captures saved", " 张已保存", " 張已儲存"]),
            ("録画時間 ", ["Recording time ", "录制时间 ", "錄製時間 "]),
            ("保存待ち", ["Waiting to capture", "等待截图", "等待截圖"]),
            ("枚・失敗", [" · failed ", " 张 · 失败 ", " 張 · 失敗 "]),
            ("枚保存", [" saved", " 张已保存", " 張已儲存"]),
            ("撮影中", ["Capturing", "正在截图", "正在截圖"]),
        ];

        let mut localized = japanese.to_owned();
        for (source, translations) in FRAGMENTS {
            let translation = translations[match self {
                Self::Japanese => unreachable!(),
                Self::English => 0,
                Self::SimplifiedChinese => 1,
                Self::TraditionalChinese => 2,
            }];
            localized = localized.replace(source, translation);
        }
        localized
    }

    fn from_setting(value: &str) -> Self {
        match value {
            "en" => Self::English,
            "zh-Hans" => Self::SimplifiedChinese,
            "zh-Hant" => Self::TraditionalChinese,
            _ => Self::Japanese,
        }
    }

    fn setting_value(self) -> &'static str {
        match self {
            Self::Japanese => "ja",
            Self::English => "en",
            Self::SimplifiedChinese => "zh-Hans",
            Self::TraditionalChinese => "zh-Hant",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Japanese => "日本語",
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
            Self::TraditionalChinese => "繁體中文",
        }
    }

    fn language_heading(self) -> &'static str {
        match self {
            Self::Japanese => "アプリの言語",
            Self::English => "App language",
            Self::SimplifiedChinese => "应用语言",
            Self::TraditionalChinese => "應用程式語言",
        }
    }

    fn page_heading(self, page: Page) -> &'static str {
        match (self, page) {
            (Self::Japanese, Page::Capture) => "証跡の撮影を始めましょう",
            (Self::Japanese, Page::Evidence) => "撮影済みの画像・録画",
            (Self::Japanese, Page::Notifications) => "通知履歴",
            (Self::Japanese, Page::Compare) => "画像比較",
            (Self::Japanese, Page::Settings) => "設定",
            (Self::English, Page::Capture) => "Start capturing evidence",
            (Self::English, Page::Evidence) => "Captured Images & Recordings",
            (Self::English, Page::Notifications) => "Notifications",
            (Self::English, Page::Compare) => "Compare Images",
            (Self::English, Page::Settings) => "Settings",
            (Self::SimplifiedChinese, Page::Capture) => "开始截取证据",
            (Self::SimplifiedChinese, Page::Evidence) => "已截取的图像与录屏",
            (Self::SimplifiedChinese, Page::Notifications) => "通知历史",
            (Self::SimplifiedChinese, Page::Compare) => "图像比较",
            (Self::SimplifiedChinese, Page::Settings) => "设置",
            (Self::TraditionalChinese, Page::Capture) => "開始擷取證據",
            (Self::TraditionalChinese, Page::Evidence) => "已擷取的圖片與錄影",
            (Self::TraditionalChinese, Page::Notifications) => "通知記錄",
            (Self::TraditionalChinese, Page::Compare) => "圖片比較",
            (Self::TraditionalChinese, Page::Settings) => "設定",
        }
    }

    fn compare_mode_label(self, mode: CompareMode) -> &'static str {
        match (self, mode) {
            (Self::Japanese, CompareMode::SideBySide) => "左右",
            (Self::Japanese, CompareMode::Overlay) => "重ね合わせ",
            (Self::Japanese, CompareMode::Difference) => "差分",
            (Self::English, CompareMode::SideBySide) => "Side by Side",
            (Self::English, CompareMode::Overlay) => "Overlay",
            (Self::English, CompareMode::Difference) => "Difference",
            (Self::SimplifiedChinese, CompareMode::SideBySide) => "并排",
            (Self::SimplifiedChinese, CompareMode::Overlay) => "叠加",
            (Self::SimplifiedChinese, CompareMode::Difference) => "差异",
            (Self::TraditionalChinese, CompareMode::SideBySide) => "並排",
            (Self::TraditionalChinese, CompareMode::Overlay) => "疊加",
            (Self::TraditionalChinese, CompareMode::Difference) => "差異",
        }
    }

    fn page_label(self, page: Page) -> &'static str {
        match (self, page) {
            (Self::Japanese, Page::Capture) => "◉  撮影・編集",
            (Self::Japanese, Page::Evidence) => "▦  証跡一覧",
            (Self::Japanese, Page::Notifications) => "♧  通知履歴",
            (Self::Japanese, Page::Compare) => "⇄  画像比較",
            (Self::Japanese, Page::Settings) => "⚙  設定",
            (Self::English, Page::Capture) => "◉  Capture & Edit",
            (Self::English, Page::Evidence) => "▦  Evidence",
            (Self::English, Page::Notifications) => "♧  Alerts",
            (Self::English, Page::Compare) => "⇄  Compare",
            (Self::English, Page::Settings) => "⚙  Settings",
            (Self::SimplifiedChinese, Page::Capture) => "◉  截图与编辑",
            (Self::SimplifiedChinese, Page::Evidence) => "▦  证据列表",
            (Self::SimplifiedChinese, Page::Notifications) => "♧  通知",
            (Self::SimplifiedChinese, Page::Compare) => "⇄  图像比较",
            (Self::SimplifiedChinese, Page::Settings) => "⚙  设置",
            (Self::TraditionalChinese, Page::Capture) => "◉  螢幕截圖與編輯",
            (Self::TraditionalChinese, Page::Evidence) => "▦  證據清單",
            (Self::TraditionalChinese, Page::Notifications) => "♧  通知",
            (Self::TraditionalChinese, Page::Compare) => "⇄  圖片比較",
            (Self::TraditionalChinese, Page::Settings) => "⚙  設定",
        }
    }

    fn tray_label(self, key: &str) -> &'static str {
        match (self, key) {
            (Self::Japanese, "show") => "MacShotを開く",
            (Self::Japanese, "capture") => "スクリーンショット",
            (Self::Japanese, "area") => "範囲を選択して撮影…",
            (Self::Japanese, "active") => "アクティブウィンドウを撮影",
            (Self::Japanese, "primary") => "デスクトップ（メイン画面）を撮影",
            (Self::Japanese, "all") => "すべてのディスプレイを撮影",
            (Self::Japanese, "window_select") => "ウィンドウを選んで撮影…",
            (Self::Japanese, "selected") => "選択中の撮影対象で撮影",
            (Self::Japanese, "choose_mode") => "撮影方法を選んで撮影…",
            (Self::Japanese, "shortcut_target") => "撮影対象選択を優先",
            (Self::Japanese, "shortcut_mode") => "各モード優先",
            (Self::Japanese, "current_target") => "現在の撮影方法",
            (Self::Japanese, "target_menu") => "撮影対象を選択",
            (Self::Japanese, "continuous_start") => "連続撮影を開始",
            (Self::Japanese, "continuous_stop") => "連続撮影を終了",
            (Self::Japanese, "native") => "macOS標準の撮影・録画を開く…",
            (Self::Japanese, "capture_stopped") => "連続撮影 — 停止中",
            (Self::Japanese, "record_time") => "録画時間 00:00",
            (Self::Japanese, "recording") => "録画中",
            (Self::Japanese, "record_start") => "録画を開始",
            (Self::Japanese, "record_stop") => "録画を停止",
            (Self::Japanese, "quit") => "終了",
            (Self::English, "show") => "Open MacShot",
            (Self::English, "capture") => "Screenshot",
            (Self::English, "area") => "Select Region…",
            (Self::English, "active") => "Capture Active Window",
            (Self::English, "primary") => "Capture Desktop (Main Display)",
            (Self::English, "all") => "Capture All Displays",
            (Self::English, "window_select") => "Click a Window to Capture…",
            (Self::English, "selected") => "Capture Selected Target",
            (Self::English, "choose_mode") => "Choose a Capture Mode…",
            (Self::English, "shortcut_target") => "Prioritize Selected Target",
            (Self::English, "shortcut_mode") => "Choose a Capture Mode",
            (Self::English, "current_target") => "Current Capture Method",
            (Self::English, "target_menu") => "Select Capture Target",
            (Self::English, "continuous_start") => "Start Continuous Capture",
            (Self::English, "continuous_stop") => "Stop Continuous Capture",
            (Self::English, "native") => "Open macOS Screenshot…",
            (Self::English, "capture_stopped") => "Continuous Capture — Stopped",
            (Self::English, "record_time") => "Recording Time 00:00",
            (Self::English, "recording") => "Recording",
            (Self::English, "record_start") => "Start Recording",
            (Self::English, "record_stop") => "Stop Recording",
            (Self::English, "quit") => "Quit",
            (Self::SimplifiedChinese, "show") => "打开 MacShot",
            (Self::SimplifiedChinese, "capture") => "屏幕截图",
            (Self::SimplifiedChinese, "area") => "选择区域并截图…",
            (Self::SimplifiedChinese, "active") => "截取活动窗口",
            (Self::SimplifiedChinese, "primary") => "截取桌面（主显示器）",
            (Self::SimplifiedChinese, "all") => "截取所有显示器",
            (Self::SimplifiedChinese, "window_select") => "点选窗口并截图…",
            (Self::SimplifiedChinese, "selected") => "截取所选目标",
            (Self::SimplifiedChinese, "choose_mode") => "选择截图模式…",
            (Self::SimplifiedChinese, "shortcut_target") => "优先所选目标",
            (Self::SimplifiedChinese, "shortcut_mode") => "优先选择截图模式",
            (Self::SimplifiedChinese, "current_target") => "当前截图方式",
            (Self::SimplifiedChinese, "target_menu") => "选择截图目标",
            (Self::SimplifiedChinese, "continuous_start") => "开始连续截图",
            (Self::SimplifiedChinese, "continuous_stop") => "停止连续截图",
            (Self::SimplifiedChinese, "native") => "打开 macOS 标准截图…",
            (Self::SimplifiedChinese, "capture_stopped") => "连续截图 — 已停止",
            (Self::SimplifiedChinese, "record_time") => "录制时间 00:00",
            (Self::SimplifiedChinese, "recording") => "正在录制",
            (Self::SimplifiedChinese, "record_start") => "开始录制",
            (Self::SimplifiedChinese, "record_stop") => "停止录制",
            (Self::SimplifiedChinese, "quit") => "退出",
            (Self::TraditionalChinese, "show") => "開啟 MacShot",
            (Self::TraditionalChinese, "capture") => "螢幕截圖",
            (Self::TraditionalChinese, "area") => "選取區域並截圖…",
            (Self::TraditionalChinese, "active") => "擷取作用中視窗",
            (Self::TraditionalChinese, "primary") => "擷取桌面（主要顯示器）",
            (Self::TraditionalChinese, "all") => "擷取所有顯示器",
            (Self::TraditionalChinese, "window_select") => "點選視窗並截圖…",
            (Self::TraditionalChinese, "selected") => "擷取所選目標",
            (Self::TraditionalChinese, "choose_mode") => "選擇截圖模式…",
            (Self::TraditionalChinese, "shortcut_target") => "優先所選目標",
            (Self::TraditionalChinese, "shortcut_mode") => "優先選擇截圖模式",
            (Self::TraditionalChinese, "current_target") => "目前截圖方式",
            (Self::TraditionalChinese, "target_menu") => "選擇截圖目標",
            (Self::TraditionalChinese, "continuous_start") => "開始連續截圖",
            (Self::TraditionalChinese, "continuous_stop") => "停止連續截圖",
            (Self::TraditionalChinese, "native") => "開啟 macOS 標準截圖…",
            (Self::TraditionalChinese, "capture_stopped") => "連續截圖 — 已停止",
            (Self::TraditionalChinese, "record_time") => "錄製時間 00:00",
            (Self::TraditionalChinese, "recording") => "正在錄製",
            (Self::TraditionalChinese, "record_start") => "開始錄製",
            (Self::TraditionalChinese, "record_stop") => "停止錄製",
            (Self::TraditionalChinese, "quit") => "結束",
            _ => "MacShot",
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn macos_app_menu_label(self, key: &str) -> Option<&'static str> {
        match (self, key) {
            (Self::Japanese, "about") => Some("MacShotについて"),
            (Self::Japanese, "hide") => Some("MacShotを隠す"),
            (Self::Japanese, "hide_others") => Some("ほかを隠す"),
            (Self::Japanese, "show_all") => Some("すべてを表示"),
            (Self::Japanese, "services") => Some("サービス"),
            (Self::Japanese, "quit") => Some("MacShotを終了"),
            (Self::English, "about") => Some("About MacShot"),
            (Self::English, "hide") => Some("Hide MacShot"),
            (Self::English, "hide_others") => Some("Hide Others"),
            (Self::English, "show_all") => Some("Show All"),
            (Self::English, "services") => Some("Services"),
            (Self::English, "quit") => Some("Quit MacShot"),
            (Self::SimplifiedChinese, "about") => Some("关于 MacShot"),
            (Self::SimplifiedChinese, "hide") => Some("隐藏 MacShot"),
            (Self::SimplifiedChinese, "hide_others") => Some("隐藏其他"),
            (Self::SimplifiedChinese, "show_all") => Some("显示全部"),
            (Self::SimplifiedChinese, "services") => Some("服务"),
            (Self::SimplifiedChinese, "quit") => Some("退出 MacShot"),
            (Self::TraditionalChinese, "about") => Some("關於 MacShot"),
            (Self::TraditionalChinese, "hide") => Some("隱藏 MacShot"),
            (Self::TraditionalChinese, "hide_others") => Some("隱藏其他程式"),
            (Self::TraditionalChinese, "show_all") => Some("顯示全部"),
            (Self::TraditionalChinese, "services") => Some("服務"),
            (Self::TraditionalChinese, "quit") => Some("結束 MacShot"),
            _ => None,
        }
    }
}

fn should_show_completion_notice(page: Page) -> bool {
    page == Page::Capture
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
    CaptureCurrentTarget,
    CaptureSelectedWindow,
    CaptureActiveWindow,
    CapturePrimaryDisplay,
    CaptureAllDisplays,
    SelectCaptureTarget(CaptureTarget),
    SetCaptureShortcutBehavior(CaptureShortcutBehavior),
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
    Frame(DynamicImage, Duration, Duration, bool),
    End,
    Error(String),
}

enum RecordingPlaybackCommand {
    NextFrame,
    Seek(Duration),
}

struct RecordingPlayback {
    request_tx: Sender<RecordingPlaybackCommand>,
    frame_rx: Receiver<RecordingPlaybackEvent>,
    playing: bool,
    ended: bool,
    frame_due: Instant,
    awaiting_frame: bool,
    duration: Duration,
    position: Duration,
    _audio_stream: Option<rodio::MixerDeviceSink>,
    audio_player: Option<rodio::Player>,
    audio_level: Option<Arc<AtomicU32>>,
    audio_setup_rx: Option<Receiver<Result<PathBuf, String>>>,
    audio_waveform_rx: Option<Receiver<Result<Vec<f32>, String>>>,
    audio_waveform: Option<Vec<f32>>,
    audio_waveform_error: Option<String>,
    audio_temp_path: Option<PathBuf>,
    volume: f32,
    muted: bool,
}

impl Drop for RecordingPlayback {
    fn drop(&mut self) {
        if let Some(path) = self.audio_temp_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct RecordingMetadataResult {
    path: PathBuf,
    duration: Option<Duration>,
}

struct RecordingRenameDialog {
    path: PathBuf,
    name: String,
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
            // GIF size depends heavily on how much the screen changes. The range is
            // calibrated against saved 1512x982 MacShot recordings (about 1.46–1.48
            // encoded bits per pixel per frame); retain wide bounds for different
            // content. Frame rate and pixel count drive the estimate.
            let pixels = f64::from(width.max(1)) * f64::from(height.max(1));
            let frames = f64::from(quality.gif_frame_rate) * MINUTE_SECONDS;
            let quality_factor = 0.9 + f64::from(quality.gif_quality_percent) / 1000.0;
            let estimate =
                |bits_per_pixel: f64| pixels * frames * bits_per_pixel / 8.0 * quality_factor;
            RecordingSizeEstimate {
                low_bytes: estimate(0.5),
                high_bytes: estimate(2.0),
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

#[cfg(test)]
fn recording_playback_status(playing: bool) -> &'static str {
    if playing {
        "録画をアプリ内で再生しています。"
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
    const SCALE: usize = 4;
    let mut pixels = vec![0_u8; 24 * 24 * 4];
    let color = if recording {
        [220, 35, 45, 255]
    } else {
        [24, 24, 24, 255]
    };
    // Supersampling keeps the camera's small curves clean at menu-bar size.
    for y in 0..24 {
        for x in 0..24 {
            let mut coverage = 0_u16;
            for sy in 0..SCALE {
                for sx in 0..SCALE {
                    let px = x as f32 + (sx as f32 + 0.5) / SCALE as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SCALE as f32;
                    let body = rounded_rect_contains(px, py, 3.0, 7.5, 21.0, 18.5, 2.0)
                        || rounded_rect_contains(px, py, 6.5, 5.5, 14.0, 9.0, 1.5);
                    let dx = px - 13.0;
                    let dy = py - 13.0;
                    let lens_distance = (dx * dx + dy * dy).sqrt();
                    let lens_ring = (3.0..=5.6).contains(&lens_distance);
                    if (body && lens_distance > 3.0) || lens_ring {
                        coverage += 1;
                    }
                }
            }
            if coverage > 0 {
                let alpha = (coverage * 255 / (SCALE * SCALE) as u16) as u8;
                let index = (y * 24 + x) * 4;
                pixels[index..index + 4].copy_from_slice(&[color[0], color[1], color[2], alpha]);
            }
        }
    }
    pixels
}

fn rounded_rect_contains(
    x: f32,
    y: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    radius: f32,
) -> bool {
    let center_x = (left + right) * 0.5;
    let center_y = (top + bottom) * 0.5;
    let half_width = (right - left) * 0.5 - radius;
    let half_height = (bottom - top) * 0.5 - radius;
    let dx = (x - center_x).abs() - half_width;
    let dy = (y - center_y).abs() - half_height;
    dx.max(0.0).powi(2) + dy.max(0.0).powi(2) <= radius * radius
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
        let (region_snapshot_tx, region_snapshot_rx) = mpsc::channel();
        #[cfg(target_os = "windows")]
        let (region_geometry_tx, region_geometry_rx) = mpsc::channel();
        let (hotkey_tx, hotkey_rx) = mpsc::channel();
        let (tray_action_tx, tray_action_rx) = mpsc::channel();
        let (recording_tx, recording_rx) = mpsc::channel();
        let (recording_metadata_tx, recording_metadata_rx) = mpsc::channel();
        #[cfg(target_os = "macos")]
        let (native_capture_import_tx, native_capture_import_rx) = mpsc::channel();
        let (capture_hotkey_text, recording_hotkey_text) = load_hotkey_settings();
        let capture_hotkey_text = canonical_hotkey_text(&capture_hotkey_text);
        let recording_hotkey_text = canonical_hotkey_text(&recording_hotkey_text);
        let folder_setting = load_setting("save_folder", "");
        let save_folder = (!folder_setting.is_empty()).then(|| PathBuf::from(folder_setting));
        let auto_import_native_captures =
            load_setting("auto_import_native_captures", "true") != "false";
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
        let capture_shortcut_behavior = CaptureShortcutBehavior::from_setting(&load_setting(
            "capture_shortcut_behavior",
            "selected_target",
        ));
        let timestamp_enabled = load_setting("timestamp_enabled", "true") != "false";
        let timestamp_format =
            normalized_timestamp_format(&load_setting("timestamp_format", "datetime"));
        let timestamp_position =
            normalized_timestamp_position(&load_setting("timestamp_position", "bottom_right"));
        let timestamp_background =
            normalized_timestamp_background(&load_setting("timestamp_background", "subtle"));
        let filename_prefix = load_setting("filename_prefix", "evidence");
        let show_capture_notice = load_setting("show_capture_notice", "true") != "false";
        let annotation_color = load_setting("annotation_color", "red");
        let annotation_width = load_setting("annotation_width", "3.0")
            .parse::<f32>()
            .unwrap_or(3.0)
            .clamp(1.0, 8.0);
        let annotation_text = load_setting("annotation_text", "注釈");
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
        let language = AppLanguage::from_setting(&load_setting("language", "ja"));
        let settings_layout = parse_settings_layout(&load_setting(
            "settings_layout",
            &serialize_settings_layout(&default_settings_layout()),
        ));
        Self {
            language,
            monitors,
            windows,
            window_search_query: String::new(),
            capture_target,
            capture_target_picker_open: false,
            capture_shortcut_behavior,
            continuous_capture: false,
            keep_region_in_continuous: load_setting("keep_region_in_continuous", "true") != "false",
            continuous_region: None,
            capture_queue: VecDeque::new(),
            region_selecting: false,
            region_drag_start: None,
            region_drag_current: None,
            region_overlay_position: egui::Pos2::ZERO,
            region_overlay_size: egui::Vec2::ZERO,
            region_snapshot_tx,
            region_snapshot_rx,
            #[cfg(target_os = "windows")]
            region_geometry_tx,
            #[cfg(target_os = "windows")]
            region_geometry_rx,
            region_selection_deadline: None,
            #[cfg(target_os = "windows")]
            region_geometry_pending: false,
            region_background_texture: None,
            save_folder,
            auto_import_native_captures,
            #[cfg(target_os = "macos")]
            native_capture_watch: None,
            #[cfg(target_os = "macos")]
            native_capture_import_tx,
            #[cfg(target_os = "macos")]
            native_capture_import_rx,
            evidence_files: Vec::new(),
            recording_files: Vec::new(),
            recording_preview_path: None,
            recording_preview_image: None,
            recording_preview_texture: None,
            recording_preview_details: None,
            recording_player_controls_rect: None,
            recording_player_last_pointer_pos: None,
            recording_player_last_pointer_activity: None,
            recording_playback: None,
            recording_player_error: None,
            recording_rename_dialog: None,
            recording_duration_cache: HashMap::new(),
            recording_metadata_pending: HashSet::new(),
            recording_metadata_tx,
            recording_metadata_rx,
            timestamp_enabled,
            timestamp_format,
            timestamp_position,
            timestamp_background,
            filename_prefix,
            show_capture_notice,
            annotation_color,
            annotation_width,
            annotation_text,
            status,
            capture_in_progress: false,
            active_capture_is_region: false,
            region_annotation_pending: false,
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
            tray_capture_target_item: None,
            tray_capture_target_text: None,
            tray_target_selection_items: Vec::new(),
            tray_capture_active_item: None,
            tray_capture_primary_item: None,
            tray_capture_all_item: None,
            tray_capture_current_target_item: None,
            tray_capture_shortcut_target_item: None,
            tray_capture_shortcut_mode_item: None,
            tray_capture_status_item: None,
            tray_window_selection_item: None,
            tray_continuous_capture_item: None,
            tray_recording_status_item: None,
            tray_recording_action_item: None,
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
            notifications: load_notifications(),
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
            mosaic_preview_texture: None,
            mosaic_preview_key: None,
            crop_selection: None,
            crop_drag_start: None,
            edit_tool: EditTool::Crop,
            capture_editor_expanded: false,
            overwrite_confirmation: false,
            annotations: Vec::new(),
            live_annotation: None,
            editing_queue: Vec::new(),
            editing_index: None,
            pending_edit_navigation: None,
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
                let application = window.owning_application();
                let app_name = application
                    .as_ref()
                    .map(|application| application.application_name())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "Window".to_owned());
                let frame = window.frame();
                Some(WindowChoice {
                    id: window.window_id(),
                    app_name,
                    title: window.title()?,
                    width: frame.size.width.max(0.0) as u32,
                    height: frame.size.height.max(0.0) as u32,
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
                let app_name = window
                    .app_name()
                    .ok()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "Window".to_owned());
                let native_title = window.title().ok();
                let title = window_title_or_app_name(native_title.as_deref(), &app_name);
                let width = window.width().unwrap_or_default();
                let height = window.height().unwrap_or_default();
                if title.is_empty() || !is_captureable_window(&window) {
                    return None;
                }
                Some(WindowChoice {
                    id,
                    app_name,
                    title,
                    width,
                    height,
                })
            })
            .collect())
    }
}

#[cfg(any(not(target_os = "macos"), test))]
fn window_title_or_app_name(title: Option<&str>, app_name: &str) -> String {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| app_name.trim())
        .to_owned()
}

fn window_choice_label(window: &WindowChoice) -> String {
    if window.app_name.eq_ignore_ascii_case(&window.title) {
        window.app_name.clone()
    } else {
        format!("{} — {}", window.app_name, window.title)
    }
}

fn window_search_matches(window: &WindowChoice, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty()
        || window.app_name.to_lowercase().contains(&query)
        || window.title.to_lowercase().contains(&query)
    {
        return true;
    }

    let identity = format!("{} {}", window.app_name, window.title).to_lowercase();
    [
        (
            &["notepad", "notepad.exe"][..],
            &["メモ帳", "记事本", "記事本"][..],
        ),
        (
            &["calculator", "calculatorapp.exe", "calc.exe"][..],
            &["計算機", "電卓", "计算器", "計算器"][..],
        ),
        (
            &["explorer.exe", "file explorer"][..],
            &["エクスプローラー", "文件资源管理器", "檔案總管"][..],
        ),
    ]
    .iter()
    .any(|(english_names, localized_names)| {
        english_names.iter().any(|name| identity.contains(name))
            && localized_names
                .iter()
                .any(|localized_name| localized_name.contains(&query))
    })
}

fn has_annotation_drag_distance(start: egui::Pos2, end: egui::Pos2) -> bool {
    start.distance(end) >= 0.002
}

fn show_window_choice(ui: &mut egui::Ui, target: &mut CaptureTarget, window: &WindowChoice) {
    let selected = *target == CaptureTarget::Window(window.id);
    let description = format!(
        "{} — {}\n{} × {} px",
        window.app_name, window.title, window.width, window.height
    );
    if ui
        .selectable_label(selected, egui::RichText::new(description).size(13.0))
        .clicked()
    {
        *target = CaptureTarget::Window(window.id);
        egui::Popup::close_all(ui.ctx());
    }
}

fn selectable_capture_target(
    ui: &mut egui::Ui,
    target: &mut CaptureTarget,
    value: CaptureTarget,
    label: impl Into<egui::WidgetText>,
) {
    if ui.selectable_label(*target == value, label).clicked() {
        *target = value;
        egui::Popup::close_all(ui.ctx());
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

fn notifications_path() -> PathBuf {
    settings_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("notifications.tsv")
}

fn encode_notification_field(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn decode_notification_field(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn load_notifications() -> Vec<AppNotification> {
    let Ok(contents) = std::fs::read_to_string(notifications_path()) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let kind = fields.next()?;
            let timestamp = decode_notification_field(fields.next()?)?;
            let path = PathBuf::from(decode_notification_field(fields.next()?)?);
            let width = fields.next()?.parse::<u32>().ok();
            let height = fields.next()?.parse::<u32>().ok();
            let detail =
                decode_notification_field(fields.next()?).filter(|value| !value.is_empty());
            let kind = if let Some(sequence) = kind.strip_prefix("capture:") {
                NotificationKind::Capture(sequence.parse().ok()?)
            } else if kind == "recording" {
                NotificationKind::Recording
            } else {
                return None;
            };
            Some(AppNotification {
                kind,
                timestamp,
                path,
                width,
                height,
                detail,
            })
        })
        .take(200)
        .collect()
}

fn save_notifications(notifications: &[AppNotification]) -> Result<(), String> {
    let path = notifications_path();
    let parent = path
        .parent()
        .ok_or_else(|| "通知履歴フォルダを取得できません。".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    for notification in notifications.iter().take(200) {
        let kind = match notification.kind {
            NotificationKind::Capture(sequence) => format!("capture:{sequence}"),
            NotificationKind::Recording => "recording".into(),
        };
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}\t{}",
            kind,
            encode_notification_field(&notification.timestamp),
            encode_notification_field(&notification.path.to_string_lossy()),
            notification
                .width
                .map_or_else(String::new, |value| value.to_string()),
            notification
                .height
                .map_or_else(String::new, |value| value.to_string()),
            encode_notification_field(notification.detail.as_deref().unwrap_or_default()),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
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
    language: AppLanguage,
    capture_hotkey: &'a str,
    recording_hotkey: &'a str,
    capture_target: &'a str,
    capture_shortcut_behavior: CaptureShortcutBehavior,
    timestamp_enabled: bool,
    timestamp_format: &'a str,
    timestamp_position: &'a str,
    timestamp_background: &'a str,
    filename_prefix: &'a str,
    show_capture_notice: bool,
    keep_region_in_continuous: bool,
    annotation_color: &'a str,
    annotation_width: f32,
    annotation_text: &'a str,
    save_folder: &'a Path,
    auto_import_native_captures: bool,
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
    writeln!(
        file,
        "capture_shortcut_behavior={}",
        preferences.capture_shortcut_behavior.setting()
    )
    .map_err(|error| error.to_string())?;
    writeln!(file, "timestamp_enabled={}", preferences.timestamp_enabled)
        .map_err(|error| error.to_string())?;
    writeln!(file, "timestamp_format={}", preferences.timestamp_format)
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "timestamp_position={}",
        preferences.timestamp_position
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "timestamp_background={}",
        preferences.timestamp_background
    )
    .map_err(|error| error.to_string())?;
    writeln!(file, "filename_prefix={}", preferences.filename_prefix)
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "show_capture_notice={}",
        preferences.show_capture_notice
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "keep_region_in_continuous={}",
        preferences.keep_region_in_continuous
    )
    .map_err(|error| error.to_string())?;
    writeln!(file, "annotation_color={}", preferences.annotation_color)
        .map_err(|error| error.to_string())?;
    writeln!(file, "annotation_width={}", preferences.annotation_width)
        .map_err(|error| error.to_string())?;
    writeln!(file, "annotation_text={}", preferences.annotation_text)
        .map_err(|error| error.to_string())?;
    writeln!(file, "save_folder={}", preferences.save_folder.display())
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "auto_import_native_captures={}",
        preferences.auto_import_native_captures
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "recording_format={}",
        preferences.recording_format.extension()
    )
    .map_err(|error| error.to_string())?;
    writeln!(file, "language={}", preferences.language.setting_value())
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
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "gif" | "mp4" | "mov"
                    )
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

fn recording_duration(path: &Path) -> Option<Duration> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if extension == "gif" {
        let file = std::fs::File::open(path).ok()?;
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        let mut decoder = options.read_info(file).ok()?;
        let mut duration_ms = 0_u64;
        let mut frame_count = 0_u64;
        while let Some(frame) = decoder.read_next_frame().ok()? {
            frame_count += 1;
            duration_ms += u64::from(frame.delay) * 10;
        }
        return (frame_count > 0).then(|| Duration::from_millis(duration_ms));
    }
    if extension == "mov" {
        #[cfg(target_os = "macos")]
        return native_mov_duration(path);
        #[cfg(not(target_os = "macos"))]
        return None;
    }
    if extension == "mp4" {
        let file = std::fs::File::open(path).ok()?;
        let size = file.metadata().ok()?.len();
        let reader = mp4::Mp4Reader::read_header(file, size).ok()?;
        return Some(reader.duration());
    }
    None
}

#[cfg(target_os = "macos")]
fn native_mov_duration(path: &Path) -> Option<Duration> {
    let path = NSString::from_str(path.to_str()?);
    let url = NSURL::fileURLWithPath(&path);
    // SAFETY: AVFoundation creates a read-only asset for this local URL.
    let asset = unsafe { AVAsset::assetWithURL(&url) };
    // SAFETY: Reading the asset's duration does not mutate the media or user data.
    let duration: CMTime = unsafe { asset.duration() };
    // SAFETY: CMTimeGetSeconds accepts the value returned by AVAsset.duration.
    let seconds = unsafe { duration.seconds() };
    duration_from_seconds(seconds)
}

#[cfg(any(target_os = "macos", test))]
fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

fn is_mp4_recording(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
}

fn is_mov_recording(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mov"))
}

fn format_recording_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        recording_time_label(duration)
    }
}

fn human_recording_stem(timestamp: &str, sequence: u64) -> String {
    let mut parts = timestamp.split('-');
    let Some(date) = parts.next() else {
        return format!("録画_{timestamp}_{sequence:03}");
    };
    let Some(time) = parts.next() else {
        return format!("録画_{timestamp}_{sequence:03}");
    };
    let millis = parts.next().unwrap_or("000");
    if date.len() == 8
        && time.len() == 6
        && millis.len() == 3
        && date.chars().all(|value| value.is_ascii_digit())
        && time.chars().all(|value| value.is_ascii_digit())
        && millis.chars().all(|value| value.is_ascii_digit())
    {
        format!(
            "録画_{}-{}-{}_{}-{}-{}_{}_{:03}",
            &date[..4],
            &date[4..6],
            &date[6..8],
            &time[..2],
            &time[2..4],
            &time[4..6],
            millis,
            sequence
        )
    } else {
        format!("録画_{timestamp}_{sequence:03}")
    }
}

fn recording_display_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if let Some(rest) = stem.strip_prefix("recording-") {
        let mut parts = rest.split('-');
        if let (Some(date), Some(time), Some(_millis), Some(sequence)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && date.len() == 8
            && time.len() == 6
            && date.chars().all(|value| value.is_ascii_digit())
            && time.chars().all(|value| value.is_ascii_digit())
        {
            return format!(
                "録画 {}-{}-{} {}:{}:{} #{}",
                &date[..4],
                &date[4..6],
                &date[6..8],
                &time[..2],
                &time[2..4],
                &time[4..6],
                sequence
            );
        }
    }
    if let Some(rest) = stem.strip_prefix("録画_") {
        let mut parts = rest.split('_');
        if let (Some(date), Some(time), Some(_millis), Some(sequence)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && let Some((hour, remainder)) = time.split_once('-')
            && let Some((minute, second)) = remainder.split_once('-')
        {
            return format!("録画 {date} {hour}:{minute}:{second} #{sequence}");
        }
    }
    stem.replace('_', " ")
}

fn sanitize_recording_name(name: &str) -> Result<String, String> {
    let name: String = name
        .trim()
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
                '-'
            } else {
                character
            }
        })
        .collect();
    let name = name.trim_matches([' ', '.']).trim();
    if name.is_empty() {
        return Err("録画名を入力してください。".into());
    }
    if name.len() > 180 {
        return Err("録画名は180バイト以内にしてください。".into());
    }
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    let upper = name.to_ascii_uppercase();
    if reserved.contains(&upper.as_str())
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
    {
        return Ok(format!("_{name}"));
    }
    Ok(name.to_string())
}

enum RecordingRenameOutcome {
    Unchanged,
    Renamed(PathBuf),
}

fn rename_recording_file(path: &Path, name: &str) -> Result<RecordingRenameOutcome, String> {
    let name = sanitize_recording_name(name)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let file_name = if extension.is_empty() {
        name
    } else {
        format!("{name}.{extension}")
    };
    let parent = path
        .parent()
        .ok_or_else(|| "録画の保存先を確認できません。".to_string())?;
    let target = parent.join(file_name);
    if target == path {
        return Ok(RecordingRenameOutcome::Unchanged);
    }
    if target.exists() {
        return Err("同じ名前の録画がすでにあります。".into());
    }
    std::fs::rename(path, &target).map_err(|error| error.to_string())?;
    Ok(RecordingRenameOutcome::Renamed(target))
}

#[cfg(any(target_os = "macos", test))]
fn is_native_capture_file(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
    {
        return false;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "mp4" | "mov"
            )
        })
}

#[cfg(any(target_os = "macos", test))]
fn copy_native_capture(source: &Path, folder: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    let name = source
        .file_name()
        .ok_or_else(|| "撮影ファイル名を取得できません。".to_string())?;
    let preferred = folder.join(name);
    let (destination, mut output) = create_unique_file(folder, &preferred)?;
    let copied =
        std::fs::File::open(source).and_then(|mut input| std::io::copy(&mut input, &mut output));
    match copied {
        Ok(_) => Ok(destination),
        Err(error) => {
            let _ = std::fs::remove_file(&destination);
            Err(error.to_string())
        }
    }
}

#[cfg(target_os = "macos")]
struct NativeCaptureWatch {
    started_at: Instant,
    known_files: HashSet<PathBuf>,
    in_flight: HashSet<PathBuf>,
    pending_sizes: HashMap<PathBuf, (u64, u8)>,
}

#[cfg(target_os = "macos")]
struct NativeCaptureImportResult {
    source: PathBuf,
    result: Result<PathBuf, String>,
}

#[cfg(target_os = "macos")]
fn spawn_native_capture_import(
    source: PathBuf,
    destination_folder: PathBuf,
    import_tx: Sender<NativeCaptureImportResult>,
) {
    thread::spawn(move || {
        let result = if source.parent() == Some(destination_folder.as_path()) {
            Ok(source.clone())
        } else {
            copy_native_capture(&source, &destination_folder)
        };
        let _ = import_tx.send(NativeCaptureImportResult { source, result });
    });
}

#[cfg(target_os = "macos")]
fn native_capture_directories(save_folder: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut directories = vec![
        save_folder.to_path_buf(),
        home.join("Desktop"),
        home.join("Documents"),
        home.join("Downloads"),
        home.join("Pictures"),
        home.join("Movies"),
    ];
    directories.sort();
    directories.dedup();
    directories
}

#[cfg(target_os = "macos")]
fn scan_native_capture_files(directories: &[PathBuf]) -> HashMap<PathBuf, u64> {
    let mut files = HashMap::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_native_capture_file(&path) {
                continue;
            }
            if let Ok(metadata) = entry.metadata()
                && metadata.is_file()
            {
                files.insert(path, metadata.len());
            }
        }
    }
    files
}

#[cfg(target_os = "macos")]
fn completed_native_capture_files(
    watch: &mut NativeCaptureWatch,
    current: HashMap<PathBuf, u64>,
) -> Vec<PathBuf> {
    let mut ready = Vec::new();
    for (path, size) in &current {
        if watch.known_files.contains(path) || watch.in_flight.contains(path) {
            continue;
        }
        let stable = match watch.pending_sizes.get_mut(path) {
            Some((previous_size, stable_scans)) if *previous_size == *size => {
                *stable_scans = stable_scans.saturating_add(1);
                *stable_scans >= 2
            }
            Some((previous_size, stable_scans)) => {
                *previous_size = *size;
                *stable_scans = 0;
                false
            }
            None => {
                watch.pending_sizes.insert(path.clone(), (*size, 0));
                false
            }
        };
        if stable && *size > 0 {
            ready.push(path.clone());
        }
    }
    watch
        .pending_sizes
        .retain(|path, _| current.contains_key(path));
    ready
}

#[cfg(test)]
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

fn recording_player_controls_anchor(
    video_rect: Option<egui::Rect>,
    bounds: egui::Rect,
    controls_width: f32,
) -> egui::Pos2 {
    let video = video_rect.unwrap_or(bounds);
    let x = (video.center().x - controls_width * 0.5)
        .clamp(bounds.left() + 16.0, bounds.right() - controls_width - 16.0);
    egui::pos2(x, video.bottom() - 8.0)
}

fn recording_player_controls_width(video_rect: Option<egui::Rect>, bounds: egui::Rect) -> f32 {
    let maximum = (bounds.width() - 32.0).max(1.0);
    video_rect
        .map(|video| video.width())
        .unwrap_or(maximum)
        .clamp(760.0_f32.min(maximum), maximum)
}

fn recording_player_controls_visible(
    video_rect: Option<egui::Rect>,
    controls_rect: Option<egui::Rect>,
    bounds: egui::Rect,
    pointer: Option<egui::Pos2>,
    last_pointer_activity: Option<Instant>,
    now: Instant,
    interacting: bool,
) -> bool {
    const IDLE_TIMEOUT: Duration = Duration::from_millis(1_200);
    pointer.is_some_and(|position| {
        let inside_player = video_rect.unwrap_or(bounds).contains(position)
            || controls_rect.is_some_and(|rect| rect.contains(position));
        inside_player
            && (interacting
                || last_pointer_activity.is_some_and(|at| now.duration_since(at) < IDLE_TIMEOUT))
    })
}

#[derive(Default)]
struct GifFrameCompositor {
    canvas: Option<image::RgbaImage>,
    previous_frame: Option<GifPreviousFrame>,
}

struct GifPreviousFrame {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    dispose: gif::DisposalMethod,
    canvas_before_frame: Option<image::RgbaImage>,
}

impl GifFrameCompositor {
    fn draw(
        &mut self,
        canvas_width: u32,
        canvas_height: u32,
        frame: &gif::Frame<'_>,
    ) -> Result<image::RgbaImage, String> {
        let canvas = self
            .canvas
            .get_or_insert_with(|| image::RgbaImage::new(canvas_width, canvas_height));
        if canvas.dimensions() != (canvas_width, canvas_height) {
            *canvas = image::RgbaImage::new(canvas_width, canvas_height);
            self.previous_frame = None;
        }

        if let Some(previous) = self.previous_frame.take() {
            match previous.dispose {
                gif::DisposalMethod::Any | gif::DisposalMethod::Keep => {}
                gif::DisposalMethod::Background => {
                    let right = previous
                        .left
                        .saturating_add(previous.width)
                        .min(canvas_width);
                    let bottom = previous
                        .top
                        .saturating_add(previous.height)
                        .min(canvas_height);
                    for y in previous.top.min(canvas_height)..bottom {
                        for x in previous.left.min(canvas_width)..right {
                            canvas.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
                        }
                    }
                }
                gif::DisposalMethod::Previous => {
                    if let Some(before) = previous.canvas_before_frame {
                        *canvas = before;
                    }
                }
            }
        }

        let frame_width = u32::from(frame.width);
        let frame_height = u32::from(frame.height);
        let frame_image =
            image::RgbaImage::from_raw(frame_width, frame_height, frame.buffer.to_vec())
                .ok_or_else(|| "GIFフレームを画像に変換できません。".to_string())?;
        let canvas_before_frame =
            (frame.dispose == gif::DisposalMethod::Previous).then(|| canvas.clone());
        let left = u32::from(frame.left);
        let top = u32::from(frame.top);
        for (x, y, pixel) in frame_image.enumerate_pixels() {
            let canvas_x = left.saturating_add(x);
            let canvas_y = top.saturating_add(y);
            if canvas_x < canvas_width && canvas_y < canvas_height && pixel[3] != 0 {
                canvas.put_pixel(canvas_x, canvas_y, *pixel);
            }
        }
        self.previous_frame = Some(GifPreviousFrame {
            left,
            top,
            width: frame_width,
            height: frame_height,
            dispose: frame.dispose,
            canvas_before_frame,
        });
        Ok(canvas.clone())
    }
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
    if extension == "mov" {
        #[cfg(target_os = "macos")]
        {
            let mut reader = macos_video::MovFrameReader::open(path, Duration::ZERO)?;
            let (frame, _, _) = reader
                .next_frame()?
                .ok_or_else(|| "MOVに表示できる映像フレームがありません。".to_string())?;
            return Ok((
                frame,
                format!(
                    "MOV・{:.2}秒・{}",
                    reader.duration().as_secs_f64(),
                    format_file_size(file_size)
                ),
            ));
        }
        #[cfg(not(target_os = "macos"))]
        return Err("このMOVは外部プレイヤーで再生してください。".into());
    }
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
            .ok_or_else(|| "GIFに表示できるフレームがありません。".to_string())?;
        let image =
            GifFrameCompositor::default().draw(u32::from(width), u32::from(height), first)?;
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
        compositor: GifFrameCompositor,
        repeat: gif::Repeat,
        completed_repetitions: u16,
        frames_in_current_loop: u32,
        position: Duration,
    },
    Mp4 {
        reader: mp4::Mp4Reader<std::fs::File>,
        track_id: u32,
        sample_id: u32,
        sample_count: u32,
        timescale: u32,
        decoder: H264Decoder,
        position: Duration,
    },
    #[cfg(target_os = "macos")]
    Mov(macos_video::MovFrameReader),
}

impl RecordingFrameReader {
    fn open(path: &Path) -> Result<Self, String> {
        Self::open_at(path, Duration::ZERO)
    }

    fn open_at(path: &Path, _position: Duration) -> Result<Self, String> {
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
                compositor: GifFrameCompositor::default(),
                repeat,
                completed_repetitions: 0,
                frames_in_current_loop: 0,
                position: Duration::ZERO,
            });
        }
        if extension == "mov" {
            #[cfg(target_os = "macos")]
            return Ok(Self::Mov(macos_video::MovFrameReader::open(
                path, _position,
            )?));
            #[cfg(not(target_os = "macos"))]
            return Err("このMOVは外部プレイヤーで再生してください。".into());
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
            position: Duration::ZERO,
        })
    }

    fn next_frame(&mut self) -> Result<Option<(DynamicImage, Duration, Duration)>, String> {
        match self {
            Self::Gif {
                path,
                decoder,
                compositor,
                repeat,
                completed_repetitions,
                frames_in_current_loop,
                position,
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
                    *position = Duration::ZERO;
                    *decoder = open_gif_playback_decoder(path)?;
                    *compositor = GifFrameCompositor::default();
                    continue;
                };
                *frames_in_current_loop = frames_in_current_loop.saturating_add(1);
                let delay = Duration::from_millis(u64::from(frame.delay.max(2)) * 10);
                let frame_position = *position;
                *position = position.saturating_add(delay);
                let image = compositor.draw(width, height, frame)?;
                return Ok(Some((
                    DynamicImage::ImageRgba8(image),
                    delay,
                    frame_position,
                )));
            },
            Self::Mp4 {
                reader,
                track_id,
                sample_id,
                sample_count,
                timescale,
                decoder,
                position,
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
                    let frame_position = *position;
                    *position = position.saturating_add(delay);
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
                    return Ok(Some((
                        DynamicImage::ImageRgb8(image),
                        delay,
                        frame_position,
                    )));
                }
                Ok(None)
            }
            #[cfg(target_os = "macos")]
            Self::Mov(reader) => reader.next_frame(),
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
    let duration = recording_duration(&path).unwrap_or_default();
    let worker_path = path.clone();
    let (request_tx, request_rx) = mpsc::channel::<RecordingPlaybackCommand>();
    let (frame_tx, frame_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = match RecordingFrameReader::open(&worker_path) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = frame_tx.send(RecordingPlaybackEvent::Error(error));
                return;
            }
        };
        while let Ok(command) = request_rx.recv() {
            let (next, is_seek) = match command {
                RecordingPlaybackCommand::NextFrame => (reader.next_frame(), false),
                RecordingPlaybackCommand::Seek(target) => {
                    reader = match RecordingFrameReader::open_at(&worker_path, target) {
                        Ok(reader) => reader,
                        Err(error) => {
                            let _ = frame_tx.send(RecordingPlaybackEvent::Error(error));
                            return;
                        }
                    };
                    if is_mov_recording(&worker_path) {
                        (reader.next_frame(), true)
                    } else {
                        (
                            loop {
                                match reader.next_frame() {
                                    Ok(Some((_frame, _delay, position))) if position < target => {
                                        continue;
                                    }
                                    other => break other,
                                }
                            },
                            true,
                        )
                    }
                }
            };
            let event = match next {
                Ok(Some((frame, delay, position))) => {
                    RecordingPlaybackEvent::Frame(frame, delay, position, is_seek)
                }
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
    let mut audio_stream = None;
    let mut audio_player = None;
    let mut audio_level = None;
    #[cfg(target_os = "macos")]
    let mut audio_setup_rx = None;
    #[cfg(not(target_os = "macos"))]
    let audio_setup_rx = None;
    let mut audio_temp_path = None;
    let mut audio_waveform_rx = None;
    let audio_waveform = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
        .then(Vec::new);
    let mut audio_waveform_error = None;
    if is_mp4_recording(&path) {
        match extract_mp4_audio_to_temp(&path) {
            Ok(audio_path) => {
                audio_waveform_rx =
                    Some(start_audio_waveform_analysis(audio_path.clone(), duration));
                audio_temp_path = Some(audio_path.clone());
                if let Ok(stream) = rodio::DeviceSinkBuilder::open_default_sink() {
                    let decoder = std::fs::File::open(&audio_path)
                        .ok()
                        .and_then(|file| rodio::Decoder::new_aac(file).ok());
                    if let Some(decoder) = decoder {
                        let player = rodio::Player::connect_new(stream.mixer());
                        let level = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
                        player.append(MeteredAudioSource {
                            source: decoder,
                            level: level.clone(),
                            sum_squares: 0.0,
                            samples: 0,
                        });
                        player.play();
                        audio_player = Some(player);
                        audio_level = Some(level);
                        audio_stream = Some(stream);
                    }
                }
            }
            Err(error) => audio_waveform_error = Some(error),
        }
    }
    #[cfg(target_os = "macos")]
    if is_mov_recording(&path) {
        let input_path = path.clone();
        let (audio_ready_tx, audio_ready_rx) = mpsc::channel();
        thread::spawn(
            move || match extract_native_mov_audio_to_temp(&input_path) {
                Ok(audio_path) => {
                    if audio_ready_tx.send(Ok(audio_path.clone())).is_err() {
                        let _ = std::fs::remove_file(audio_path);
                    }
                }
                Err(error) => {
                    let _ = audio_ready_tx.send(Err(error));
                }
            },
        );
        audio_setup_rx = Some(audio_ready_rx);
    }
    RecordingPlayback {
        request_tx,
        frame_rx,
        playing: true,
        ended: false,
        frame_due: Instant::now(),
        awaiting_frame: false,
        duration,
        position: Duration::ZERO,
        _audio_stream: audio_stream,
        audio_player,
        audio_level,
        audio_setup_rx,
        audio_waveform_rx,
        audio_waveform,
        audio_waveform_error,
        audio_temp_path,
        volume: 1.0,
        muted: false,
    }
}

fn start_audio_waveform_analysis(
    audio_path: PathBuf,
    duration: Duration,
) -> Receiver<Result<Vec<f32>, String>> {
    let (result_tx, result_rx) = mpsc::channel();
    let is_m4a = audio_path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("m4a"));
    match std::fs::File::open(audio_path) {
        Ok(file) => {
            thread::spawn(move || {
                let result = audio_waveform_levels_from_file(file, is_m4a, duration);
                let _ = result_tx.send(result);
            });
        }
        Err(error) => {
            let _ = result_tx.send(Err(error.to_string()));
        }
    }
    result_rx
}

fn audio_waveform_levels_from_file(
    file: std::fs::File,
    is_m4a: bool,
    duration: Duration,
) -> Result<Vec<f32>, String> {
    let decoder = if is_m4a {
        rodio::Decoder::new_mp4(file).map_err(|error| error.to_string())?
    } else {
        rodio::Decoder::new_aac(file).map_err(|error| error.to_string())?
    };
    let channels = rodio::Source::channels(&decoder).get().max(1) as usize;
    let sample_rate = rodio::Source::sample_rate(&decoder).get();
    Ok(audio_waveform_levels(
        decoder,
        channels,
        sample_rate,
        duration,
        2,
    ))
}

fn audio_waveform_levels(
    samples: impl Iterator<Item = f32>,
    channels: usize,
    sample_rate: u32,
    duration: Duration,
    bins_per_second: usize,
) -> Vec<f32> {
    const MAX_WAVEFORM_BINS: usize = 24_000;
    let bin_count = (duration.as_secs_f64() * bins_per_second.max(1) as f64)
        .ceil()
        .clamp(1.0, MAX_WAVEFORM_BINS as f64) as usize;
    let frames_per_bin = (duration.as_secs_f64() * sample_rate.max(1) as f64 / bin_count as f64)
        .ceil()
        .max(1.0) as usize;
    let mut sums = vec![0.0_f64; bin_count];
    let mut counts = vec![0_u64; bin_count];
    for (sample_index, sample) in samples.enumerate() {
        let frame = sample_index / channels.max(1);
        if frame as f64 >= duration.as_secs_f64() * sample_rate.max(1) as f64 {
            break;
        }
        let bin = (frame / frames_per_bin).min(bin_count - 1);
        if sample.is_finite() {
            sums[bin] += f64::from(sample) * f64::from(sample);
        }
        counts[bin] += 1;
    }
    sums.into_iter()
        .zip(counts)
        .map(|(sum, count)| {
            if count == 0 {
                0.0
            } else {
                (sum / count as f64).sqrt().min(1.0) as f32
            }
        })
        .collect()
}

fn audio_waveform_has_sound(level: f32) -> bool {
    level.is_finite() && level > 0.001
}

#[cfg(target_os = "macos")]
fn extract_native_mov_audio_to_temp(path: &Path) -> Result<PathBuf, String> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let output_path = std::env::temp_dir().join(format!(
        "macshot-mov-audio-{}-{unique}.m4a",
        std::process::id()
    ));
    let output = Command::new("/usr/bin/afconvert")
        .args(["-f", "m4af", "-d", "aac"])
        .arg(path)
        .arg(&output_path)
        .output()
        .map_err(|error| format!("macOSの音声変換を開始できません: {error}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&output_path);
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            "MOVの音声をAACに変換できません。".to_string()
        } else {
            detail
        });
    }
    Ok(output_path)
}

fn extract_mp4_audio_to_temp(path: &Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let size = file.metadata().map_err(|error| error.to_string())?.len();
    let mut reader = mp4::Mp4Reader::read_header(file, size).map_err(|error| error.to_string())?;
    let (audio_track_id, profile, freq_index, chan_conf) = {
        let (track_id, track) = reader
            .tracks()
            .iter()
            .find(|(_, track)| {
                track
                    .media_type()
                    .is_ok_and(|kind| kind == mp4::MediaType::AAC)
            })
            .ok_or_else(|| "MP4にAAC音声トラックがありません。".to_string())?;
        (
            *track_id,
            track.audio_profile().map_err(|error| error.to_string())?,
            track
                .sample_freq_index()
                .map_err(|error| error.to_string())?,
            track.channel_config().map_err(|error| error.to_string())?,
        )
    };
    let profile = profile as u8 - 1;
    let freq_index = freq_index as u8;
    let channel_config = chan_conf as u8;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let output_path = std::env::temp_dir().join(format!(
        "macshot-playback-{}-{unique}.aac",
        std::process::id()
    ));
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|error| error.to_string())?;
    let write_result = (|| {
        let sample_count = reader
            .sample_count(audio_track_id)
            .map_err(|error| error.to_string())?;
        for sample_id in 1..=sample_count {
            if let Some(sample) = reader
                .read_sample(audio_track_id, sample_id)
                .map_err(|error| error.to_string())?
            {
                let payload_len = sample.bytes.len();
                let frame_len = payload_len + 7;
                if frame_len > 0x1fff {
                    return Err("AAC音声フレームが大きすぎます。".to_string());
                }
                let header = [
                    0xff,
                    0xf1,
                    (profile << 6) | (freq_index << 2) | (channel_config >> 2),
                    ((channel_config & 0x03) << 6) | ((frame_len >> 11) as u8 & 0x03),
                    (frame_len >> 3) as u8,
                    ((frame_len as u8 & 0x07) << 5) | 0x1f,
                    0xfc,
                ];
                output
                    .write_all(&header)
                    .map_err(|error| error.to_string())?;
                output
                    .write_all(&sample.bytes)
                    .map_err(|error| error.to_string())?;
            }
        }
        output.sync_all().map_err(|error| error.to_string())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&output_path);
        return Err(error);
    }
    Ok(output_path)
}

fn format_playback_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 * 60 {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn playback_position_for_display(
    current: Duration,
    playing: bool,
    audio_position: Option<Duration>,
    duration: Duration,
) -> Duration {
    if playing {
        audio_position.unwrap_or(current).min(duration)
    } else {
        current.min(duration)
    }
}

fn should_update_playback_position(playing: bool, is_seek_frame: bool) -> bool {
    playing || is_seek_frame
}

fn playback_audio_requires_recreation(audio_finished: bool, audio_path: Option<&Path>) -> bool {
    audio_finished || audio_path.is_some()
}

fn seek_recording_playback_audio(
    playback: &mut RecordingPlayback,
    target: Duration,
) -> Result<(), String> {
    let audio_finished = playback
        .audio_player
        .as_ref()
        .is_some_and(rodio::Player::empty);
    if !playback_audio_requires_recreation(audio_finished, playback.audio_temp_path.as_deref()) {
        if let Some(player) = &playback.audio_player {
            player
                .try_seek(target)
                .map_err(|error| format!("録画音声をシークできません: {error}"))?;
        }
        return Ok(());
    }

    let (Some(audio_path), Some(stream)) = (
        playback.audio_temp_path.as_ref(),
        playback._audio_stream.as_ref(),
    ) else {
        return Ok(());
    };
    let file = std::fs::File::open(audio_path).map_err(|error| error.to_string())?;
    let decoder = if audio_path.extension().and_then(|value| value.to_str()) == Some("m4a") {
        rodio::Decoder::new_mp4(file).map_err(|error| error.to_string())?
    } else {
        rodio::Decoder::new_aac(file).map_err(|error| error.to_string())?
    };
    let player = rodio::Player::connect_new(stream.mixer());
    let level = playback
        .audio_level
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicU32::new(0.0_f32.to_bits())));
    player.append(MeteredAudioSource {
        source: decoder,
        level: level.clone(),
        sum_squares: 0.0,
        samples: 0,
    });
    player.set_volume(if playback.muted { 0.0 } else { playback.volume });
    player
        .try_seek(target)
        .map_err(|error| format!("録画音声をシークできません: {error}"))?;
    if playback.playing {
        player.play();
    } else {
        player.pause();
    }
    playback.audio_player = Some(player);
    playback.audio_level = Some(level);
    Ok(())
}

fn audio_meter_value(rms: f32) -> (f32, f32) {
    let dbfs = if rms > 0.000_001 {
        20.0 * rms.log10()
    } else {
        -60.0
    }
    .clamp(-60.0, 0.0);
    ((dbfs + 60.0) / 60.0, dbfs)
}

fn show_audio_presence_timeline(
    ui: &mut egui::Ui,
    levels: &[f32],
    duration: Duration,
    position: Duration,
) {
    let width = ui.available_width().max(100.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 32.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(244, 244, 248));

    let columns = ((rect.width() / 3.0).floor() as usize)
        .clamp(1, 1_200)
        .min(levels.len());
    if columns > 0 {
        let column_width = rect.width() / columns as f32;
        let center_y = rect.center().y;
        let max_half_height = rect.height() * 0.44;
        let strongest_level = levels
            .iter()
            .copied()
            .filter(|level| audio_waveform_has_sound(*level))
            .fold(0.0_f32, f32::max);
        for column in 0..columns {
            let start = column * levels.len() / columns;
            let end = ((column + 1) * levels.len() / columns).max(start + 1);
            let rms = levels[start..end.min(levels.len())]
                .iter()
                .copied()
                .filter(|level| level.is_finite())
                .fold(0.0_f32, f32::max);
            let has_sound = audio_waveform_has_sound(rms);
            let height = if has_sound {
                (0.25 + 0.75 * (rms / strongest_level.max(0.001)).sqrt()) * max_half_height
            } else {
                1.0
            };
            let x = rect.left() + column as f32 * column_width;
            let bar = egui::Rect::from_min_max(
                egui::pos2(x + 0.6, center_y - height),
                egui::pos2((x + column_width - 0.6).max(x + 1.0), center_y + height),
            );
            painter.rect_filled(
                bar,
                1.0,
                if has_sound {
                    egui::Color32::from_rgb(76, 163, 116)
                } else {
                    egui::Color32::from_rgb(195, 199, 207)
                },
            );
        }
    }

    if !duration.is_zero() {
        let fraction = (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0) as f32;
        let x = rect.left() + rect.width() * fraction;
        painter.line_segment(
            [
                egui::pos2(x, rect.top() + 1.0),
                egui::pos2(x, rect.bottom() - 1.0),
            ],
            egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(130, 105, 230)),
        );
    }
}

struct MeteredAudioSource<S> {
    source: S,
    level: Arc<AtomicU32>,
    sum_squares: f32,
    samples: u32,
}

impl<S: Iterator<Item = f32>> Iterator for MeteredAudioSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.source.next()?;
        self.sum_squares += sample * sample;
        self.samples += 1;
        if self.samples >= 2048 {
            self.level.store(
                (self.sum_squares / self.samples as f32).sqrt().to_bits(),
                Ordering::Relaxed,
            );
            self.sum_squares = 0.0;
            self.samples = 0;
        }
        Some(sample)
    }
}

impl<S: rodio::Source<Item = f32>> rodio::Source for MeteredAudioSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }
    fn channels(&self) -> rodio::ChannelCount {
        self.source.channels()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        self.source.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.source.try_seek(pos)
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
        "{}.{}",
        human_recording_stem(timestamp, sequence),
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

fn overwrite_png(path: &Path, image: &image::RgbaImage) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "保存先のフォルダを特定できません。".to_string())?;
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("evidence");
    let temporary_path = parent.join(format!(".{stem}-macshot-overwrite.png"));
    let (temporary_path, mut file) = create_unique_file(parent, &temporary_path)?;
    let encoded = image::codecs::png::PngEncoder::new(&mut file).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    );
    if let Err(error) = encoded {
        drop(file);
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error.to_string());
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error.to_string());
    }
    drop(file);

    #[cfg(not(target_os = "windows"))]
    {
        std::fs::rename(&temporary_path, path).map_err(|error| {
            let _ = std::fs::remove_file(&temporary_path);
            error.to_string()
        })?;
    }

    #[cfg(target_os = "windows")]
    {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let backup_path = parent.join(format!(".{stem}-macshot-backup-{nonce}.png"));
        std::fs::rename(path, &backup_path).map_err(|error| {
            let _ = std::fs::remove_file(&temporary_path);
            error.to_string()
        })?;
        if let Err(error) = std::fs::rename(&temporary_path, path) {
            let restore_error = std::fs::rename(&backup_path, path).err();
            let _ = std::fs::remove_file(&temporary_path);
            return Err(match restore_error {
                Some(restore_error) => format!(
                    "置き換えに失敗しました: {error}。元画像は {} に退避されています ({restore_error})",
                    backup_path.display()
                ),
                None => format!("置き換えに失敗したため元画像を復元しました: {error}"),
            });
        }
        let _ = std::fs::remove_file(backup_path);
    }
    Ok(())
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
    #[cfg(not(target_os = "macos"))]
    {
        let is_candidate = |window: &Window| is_captureable_window(window);

        #[cfg(target_os = "windows")]
        {
            for _ in 0..10 {
                let windows = Window::all().map_err(|error| error.to_string())?;
                let focused_index = windows.iter().position(|window| {
                    is_candidate(window) && window.is_focused().unwrap_or(false)
                });
                let foreground_pid = foreground_window_process_id();
                let foreground_process_index = foreground_pid.and_then(|pid| {
                    windows
                        .iter()
                        .position(|window| is_candidate(window) && window.pid().ok() == Some(pid))
                });
                if let Some(index) = focused_index.or(foreground_process_index) {
                    return windows
                        .into_iter()
                        .nth(index)
                        .ok_or_else(|| "アクティブウィンドウが見つかりません。".to_string());
                }
                thread::sleep(Duration::from_millis(50));
            }
            return Err("アクティブウィンドウが見つかりません。".to_string());
        }

        #[cfg(not(target_os = "windows"))]
        {
            let windows = Window::all().map_err(|error| error.to_string())?;
            let focused_index = windows
                .iter()
                .position(|window| is_candidate(window) && window.is_focused().unwrap_or(false));
            return focused_index
                .and_then(|index| windows.into_iter().nth(index))
                .ok_or_else(|| "アクティブウィンドウが見つかりません。".to_string());
        }
    }
}

#[cfg(target_os = "windows")]
fn foreground_window_process_id() -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd, &mut process_id);
        (process_id != 0).then_some(process_id)
    }
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

fn recording_target_uses_window_capture(target: CaptureTarget) -> bool {
    matches!(
        target,
        CaptureTarget::ActiveWindow | CaptureTarget::Window(_)
    )
}

struct RecordingCapture {
    display: Option<DisplayCapture>,
    #[cfg(target_os = "macos")]
    window: Option<WindowRecordingCapture>,
    #[cfg(not(target_os = "macos"))]
    window: Option<Window>,
}

#[cfg(target_os = "macos")]
struct WindowRecordingCapture {
    filter: SCContentFilter,
    configuration: SCStreamConfiguration,
}

impl RecordingCapture {
    fn new(target: CaptureTarget) -> Result<Self, String> {
        if recording_target_uses_window_capture(target) {
            #[cfg(target_os = "macos")]
            {
                let window_id = match target {
                    CaptureTarget::ActiveWindow => find_active_screen_capture_window_id()?,
                    CaptureTarget::Window(window_id) => window_id,
                    _ => unreachable!("window capture target was checked above"),
                };
                let content = SCShareableContent::get().map_err(|error| {
                    format!("ScreenCaptureKitで録画対象のウィンドウ一覧を取得できません: {error}")
                })?;
                let window = content
                    .windows()
                    .into_iter()
                    .find(|window| window.window_id() == window_id)
                    .ok_or_else(|| "録画するウィンドウが見つかりません。".to_string())?;
                let filter = SCContentFilter::create()
                    .with_window(&window)
                    .build()
                    .map_err(|error| format!("ウィンドウ録画の準備に失敗しました: {error}"))?;
                let (width, height) = SCShareableContentInfo::for_filter(&filter)
                    .map(|info| info.pixel_size())
                    .filter(|(width, height)| *width > 0 && *height > 0)
                    .ok_or_else(|| "録画するウィンドウのサイズを取得できません。".to_string())?;
                let configuration = SCStreamConfiguration::new()
                    .with_width(width)
                    .with_height(height);
                Ok(Self {
                    display: None,
                    window: Some(WindowRecordingCapture {
                        filter,
                        configuration,
                    }),
                })
            }
            #[cfg(not(target_os = "macos"))]
            {
                let window = match target {
                    CaptureTarget::ActiveWindow => find_active_window()?,
                    CaptureTarget::Window(window_id) => Window::all()
                        .map_err(|error| error.to_string())?
                        .into_iter()
                        .find(|window| window.id().ok() == Some(window_id))
                        .ok_or_else(|| "録画するウィンドウが見つかりません。".to_string())?,
                    _ => unreachable!("window capture target was checked above"),
                };
                Ok(Self {
                    display: None,
                    window: Some(window),
                })
            }
        } else {
            let monitor = monitor_for_recording(target)?;
            Ok(Self {
                display: Some(DisplayCapture::new(&monitor)?),
                window: None,
            })
        }
    }

    fn capture_image(&self) -> Result<image::RgbaImage, String> {
        #[cfg(target_os = "macos")]
        if let Some(window) = &self.window {
            let captured =
                SCScreenshotManager::capture_image(&window.filter, &window.configuration).map_err(
                    |error| format!("ScreenCaptureKitでウィンドウを録画できません: {error}"),
                )?;
            let width = u32::try_from(captured.width())
                .map_err(|_| "録画画像の幅が上限を超えています。".to_string())?;
            let height = u32::try_from(captured.height())
                .map_err(|_| "録画画像の高さが上限を超えています。".to_string())?;
            let pixels = captured
                .rgba_data()
                .map_err(|error| format!("録画画像を読み取れません: {error}"))?;
            return image::RgbaImage::from_raw(width, height, pixels)
                .ok_or_else(|| "録画画像のサイズとピクセルデータが一致しません。".to_string());
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(window) = &self.window {
            return window.capture_image().map_err(|error| error.to_string());
        }
        self.display
            .as_ref()
            .ok_or_else(|| "録画する画面が見つかりません。".to_string())?
            .capture_image()
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
fn capture_window_by_user_selection(
    cancelled: &Arc<AtomicBool>,
) -> Result<image::RgbaImage, String> {
    let path = std::env::temp_dir().join(format!(
        "macshot-window-selection-{}-{}.png",
        std::process::id(),
        Local::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&path);
    let mut child = Command::new("/usr/sbin/screencapture")
        .args(["-i", "-w"])
        .arg(&path)
        .spawn()
        .map_err(|error| format!("ウィンドウ選択を開始できません: {error}"))?;

    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&path);
            return Err(capture_timeout_message().to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&path);
                return Err(format!("ウィンドウ選択を確認できません: {error}"));
            }
        }
    };

    if !status.success() || !path.is_file() {
        let _ = std::fs::remove_file(&path);
        return Err("__MACSHOT_WINDOW_SELECTION_CANCELLED__".into());
    }
    let image = image::open(&path)
        .map(image::DynamicImage::into_rgba8)
        .map_err(|error| format!("選択したウィンドウ画像を読み込めません: {error}"));
    let _ = std::fs::remove_file(&path);
    image
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

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveWindowCandidate {
    window_id: u32,
    owning_process_id: Option<i32>,
    captureable: bool,
    stage_manager_active: bool,
}

#[cfg(any(target_os = "macos", test))]
fn select_active_window_candidate(
    candidates: &[ActiveWindowCandidate],
    frontmost_process_id: i32,
) -> Option<u32> {
    candidates
        .iter()
        .filter(|candidate| {
            candidate.captureable && candidate.owning_process_id == Some(frontmost_process_id)
        })
        .find(|candidate| candidate.stage_manager_active)
        .or_else(|| {
            candidates.iter().find(|candidate| {
                candidate.captureable && candidate.owning_process_id == Some(frontmost_process_id)
            })
        })
        .map(|candidate| candidate.window_id)
}

#[cfg(target_os = "macos")]
fn find_active_screen_capture_window_id() -> Result<u32, String> {
    use objc2_app_kit::NSWorkspace;

    // Hiding the MacShot window is asynchronous. Prefer the frontmost app's
    // shareable window, then fall back to the first visible external window.
    for _ in 0..50 {
        let workspace = unsafe { NSWorkspace::sharedWorkspace() };
        let frontmost_process_id = unsafe { workspace.frontmostApplication() }
            .map(|application| unsafe { application.processIdentifier() });
        let content = SCShareableContent::get().map_err(|error| {
            format!("ScreenCaptureKitでウィンドウ一覧を取得できません: {error}")
        })?;
        let candidates = content
            .windows()
            .iter()
            .map(|window| ActiveWindowCandidate {
                window_id: window.window_id(),
                owning_process_id: window
                    .owning_application()
                    .map(|application| application.process_id()),
                captureable: is_captureable_screen_capture_window(window),
                stage_manager_active: window.is_active(),
            })
            .collect::<Vec<_>>();
        if let Some(window_id) =
            select_active_window_candidate(&candidates, frontmost_process_id.unwrap_or_default())
        {
            return Ok(window_id);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    Err("アクティブウィンドウを特定できません。撮影または録画したいウィンドウを前面にして再試行してください。".to_string())
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
fn set_standard_quit_items_enabled(enabled: bool, language: AppLanguage) {
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
    let about_action: Sel = sel!(orderFrontStandardAboutPanel:);
    let hide_action: Sel = sel!(hide:);
    let hide_others_action: Sel = sel!(hideOtherApplications:);
    let show_all_action: Sel = sel!(unhideAllApplications:);
    unsafe {
        for menu_item in main_menu.itemArray().iter() {
            let Some(submenu) = menu_item.submenu() else {
                continue;
            };
            for action_item in submenu.itemArray().iter() {
                let localized_title = match action_item.action() {
                    Some(action) if action == about_action => {
                        language.macos_app_menu_label("about")
                    }
                    Some(action) if action == hide_action => language.macos_app_menu_label("hide"),
                    Some(action) if action == hide_others_action => {
                        language.macos_app_menu_label("hide_others")
                    }
                    Some(action) if action == show_all_action => {
                        language.macos_app_menu_label("show_all")
                    }
                    _ if action_item.submenu().is_some() => {
                        language.macos_app_menu_label("services")
                    }
                    Some(action) if action == terminate_action => {
                        language.macos_app_menu_label("quit")
                    }
                    _ => None,
                };
                if let Some(title) = localized_title {
                    action_item.setTitle(&NSString::from_str(title));
                }
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

fn retained_continuous_region(
    continuous_capture: bool,
    keep_region: bool,
    region: Option<[f32; 4]>,
) -> Option<[f32; 4]> {
    (continuous_capture && keep_region)
        .then_some(region)
        .flatten()
}

fn update_region_drag_state(
    start: &mut Option<egui::Pos2>,
    current: &mut Option<egui::Pos2>,
    pressed: bool,
    down: bool,
    released: bool,
    pointer: Option<egui::Pos2>,
    press_origin: Option<egui::Pos2>,
) {
    if pressed {
        *start = press_origin.or(pointer);
        *current = *start;
    }
    if (down || released) && pointer.is_some() {
        *current = pointer;
    }
}

fn capture_size_label(width: u32, height: u32) -> String {
    format!("画像サイズ: {width} × {height} px")
}

fn capture_saved_status(sequence: u64, timestamp: &str, width: u32, height: u32) -> String {
    format!("撮影 {sequence} 枚目を保存しました ({timestamp}) · {width}×{height} px")
}

fn normalized_timestamp_format(value: &str) -> String {
    match value {
        "date" | "time" | "datetime_millis" | "japanese" => value.to_string(),
        _ => "datetime".into(),
    }
}

fn normalized_timestamp_position(value: &str) -> String {
    match value {
        "top_left" | "top_right" | "bottom_left" => value.to_string(),
        _ => "bottom_right".into(),
    }
}

fn normalized_timestamp_background(value: &str) -> String {
    match value {
        "none" | "dark" => value.to_string(),
        _ => "subtle".into(),
    }
}

fn capture_image_timestamp(captured_at: &DateTime<Local>, format: &str) -> String {
    match format {
        "date" => captured_at.format("%Y-%m-%d").to_string(),
        "time" => captured_at.format("%H:%M:%S").to_string(),
        "datetime_millis" => captured_at.format("%Y-%m-%d %H:%M:%S.%3f").to_string(),
        "japanese" => captured_at.format("%Y年%m月%d日 %H:%M:%S").to_string(),
        _ => captured_at.format("%Y-%m-%d %H:%M:%S").to_string(),
    }
}

fn timestamp_for_evidence_path(path: &Path, format: &str) -> String {
    let Some((stamp, _)) = parse_evidence_identity(path) else {
        let modified = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(DateTime::<Local>::from)
            .unwrap_or_else(Local::now);
        return capture_image_timestamp(&modified, format);
    };
    let date = &stamp[..8];
    let time = &stamp[9..15];
    let millis = &stamp[16..19];
    match format {
        "date" => format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]),
        "time" => format!("{}:{}:{}", &time[..2], &time[2..4], &time[4..6]),
        "datetime_millis" => format!(
            "{}-{}-{} {}:{}:{}.{}",
            &date[..4],
            &date[4..6],
            &date[6..8],
            &time[..2],
            &time[2..4],
            &time[4..6],
            millis
        ),
        "japanese" => format!(
            "{}年{}月{}日 {}:{}:{}",
            &date[..4],
            &date[4..6],
            &date[6..8],
            &time[..2],
            &time[2..4],
            &time[4..6]
        ),
        _ => format!(
            "{}-{}-{} {}:{}:{}",
            &date[..4],
            &date[4..6],
            &date[6..8],
            &time[..2],
            &time[2..4],
            &time[4..6]
        ),
    }
}

fn blend_black_rect(
    image: &mut image::RgbaImage,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    alpha: u8,
) {
    if alpha == 0 {
        return;
    }
    let start_x = x.max(0) as u32;
    let start_y = y.max(0) as u32;
    let end_x = (x.max(0) as u32).saturating_add(width).min(image.width());
    let end_y = (y.max(0) as u32).saturating_add(height).min(image.height());
    for py in start_y..end_y {
        for px in start_x..end_x {
            let pixel = image.get_pixel_mut(px, py);
            for channel in 0..3 {
                pixel[channel] =
                    ((u16::from(pixel[channel]) * (255 - u16::from(alpha))) / 255) as u8;
            }
        }
    }
}

fn timestamp_background_alpha(style: &str) -> u8 {
    match style {
        "none" => 0,
        "dark" => 112,
        _ => 64,
    }
}

fn stamp_timestamp(
    image: &mut image::RgbaImage,
    timestamp: &str,
    position: &str,
    background: &str,
) {
    let font = FontRef::try_from_slice(include_bytes!("../assets/NotoSansJP-SemiBold.ttf"))
        .expect("bundled timestamp font must be valid");
    let scale = PxScale::from((image.width().min(image.height()) as f32 / 45.0).clamp(18.0, 34.0));
    let (text_width, text_height) = text_size(scale, &font, timestamp);
    let padding = (scale.y * 0.45).round() as i32;
    let right = position.ends_with("right");
    let bottom = position.starts_with("bottom");
    let left = if right {
        (image.width() as i32 - text_width as i32 - padding * 3).max(padding)
    } else {
        padding * 2
    };
    let top = if bottom {
        (image.height() as i32 - text_height as i32 - padding * 3).max(padding)
    } else {
        padding * 2
    };
    blend_black_rect(
        image,
        left - padding,
        top - padding,
        text_width + (padding * 2) as u32,
        text_height + (padding * 2) as u32,
        timestamp_background_alpha(background),
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

fn mosaic_block_size(width: u32, height: u32, annotation_width: f32) -> u32 {
    ((width.min(height) as f32 / 36.0) * (annotation_width / 3.0).clamp(0.7, 2.0))
        .round()
        .clamp(14.0, 64.0) as u32
}

fn draw_annotation(
    image: &mut image::RgbaImage,
    annotation: &Annotation,
    color: [u8; 3],
    annotation_width: f32,
    timestamp_background: &str,
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
        EditTool::Ellipse => {
            let center_x = (x1 + x2) * 0.5;
            let center_y = (y1 + y2) * 0.5;
            let radius_x = (x2 - x1).abs() * 0.5;
            let radius_y = (y2 - y1).abs() * 0.5;
            let segments = 72;
            let mut previous = (center_x + radius_x, center_y);
            for step in 1..=segments {
                let angle = std::f32::consts::TAU * step as f32 / segments as f32;
                let next = (
                    center_x + radius_x * angle.cos(),
                    center_y + radius_y * angle.sin(),
                );
                draw_line_segment_mut(image, previous, next, color);
                previous = next;
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
        EditTool::Text => {
            if annotation.text.trim().is_empty() {
                return;
            }
            let font = FontRef::try_from_slice(include_bytes!("../assets/NotoSansJP-SemiBold.ttf"))
                .expect("bundled annotation font must be valid");
            let scale = PxScale::from(
                (image.width().min(image.height()) as f32 / 28.0).clamp(18.0, 52.0)
                    * (annotation_width / 3.0).clamp(0.6, 2.5),
            );
            let (text_width, text_height) = text_size(scale, &font, &annotation.text);
            let padding = (scale.y * 0.35).round() as i32;
            let left = (x1.round() as i32).clamp(
                padding,
                (image.width() as i32 - text_width as i32 - padding).max(padding),
            );
            let top = (y1.round() as i32).clamp(
                padding,
                (image.height() as i32 - text_height as i32 - padding).max(padding),
            );
            draw_filled_rect_mut(
                image,
                ImageRect::at(left - padding, top - padding).of_size(
                    text_width.saturating_add((padding * 2) as u32),
                    text_height.saturating_add((padding * 2) as u32),
                ),
                Rgba([color[0], color[1], color[2], 220]),
            );
            draw_text_mut(
                image,
                Rgba([255, 255, 255, 255]),
                left,
                top,
                scale,
                &font,
                &annotation.text,
            );
        }
        EditTool::Timestamp => {
            if annotation.text.trim().is_empty() {
                return;
            }
            let font = FontRef::try_from_slice(include_bytes!("../assets/NotoSansJP-SemiBold.ttf"))
                .expect("bundled timestamp font must be valid");
            let scale =
                PxScale::from((image.width().min(image.height()) as f32 / 45.0).clamp(18.0, 34.0));
            let (text_width, text_height) = text_size(scale, &font, &annotation.text);
            let padding = (scale.y * 0.4).round() as i32;
            let left = (x1.round() as i32).clamp(
                padding,
                (image.width() as i32 - text_width as i32 - padding).max(padding),
            );
            let top = (y1.round() as i32).clamp(
                padding,
                (image.height() as i32 - text_height as i32 - padding).max(padding),
            );
            blend_black_rect(
                image,
                left - padding,
                top - padding,
                text_width.saturating_add((padding * 2) as u32),
                text_height.saturating_add((padding * 2) as u32),
                timestamp_background_alpha(timestamp_background),
            );
            draw_text_mut(
                image,
                Rgba([255, 255, 255, 255]),
                left,
                top,
                scale,
                &font,
                &annotation.text,
            );
        }
        EditTool::Mosaic => {
            let image_width = image.width();
            let image_height = image.height();
            if image_width == 0 || image_height == 0 {
                return;
            }
            let start_x = (x1.min(x2).floor() as u32).min(image_width - 1);
            let end_x = (x1.max(x2).ceil() as u32).clamp(start_x + 1, image_width);
            let start_y = (y1.min(y2).floor() as u32).min(image_height - 1);
            let end_y = (y1.max(y2).ceil() as u32).clamp(start_y + 1, image_height);
            let block_size = mosaic_block_size(image_width, image_height, annotation_width);
            let mut block_y = start_y;
            while block_y < end_y {
                let mut block_x = start_x;
                let block_bottom = (block_y + block_size).min(end_y);
                while block_x < end_x {
                    let block_right = (block_x + block_size).min(end_x);
                    let mut totals = [0_u64; 4];
                    let mut count = 0_u64;
                    for y in block_y..block_bottom {
                        for x in block_x..block_right {
                            let pixel = image.get_pixel(x, y).0;
                            for channel in 0..4 {
                                totals[channel] += u64::from(pixel[channel]);
                            }
                            count += 1;
                        }
                    }
                    if count > 0 {
                        let average = Rgba([
                            (totals[0] / count) as u8,
                            (totals[1] / count) as u8,
                            (totals[2] / count) as u8,
                            (totals[3] / count) as u8,
                        ]);
                        for y in block_y..block_bottom {
                            for x in block_x..block_right {
                                image.put_pixel(x, y, average);
                            }
                        }
                    }
                    block_x = block_right;
                }
                block_y = block_bottom;
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
    timestamp_background: &str,
) -> image::RgbaImage {
    let mut output = image.to_rgba8();
    for annotation in annotations {
        draw_annotation(
            &mut output,
            annotation,
            color,
            annotation_width,
            timestamp_background,
        );
    }
    output
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(132, 115, 174);
const ACCENT_BRIGHT: egui::Color32 = egui::Color32::from_rgb(112, 96, 153);
const LAVENDER_SURFACE: egui::Color32 = egui::Color32::from_rgb(248, 246, 252);
const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(91, 88, 105);

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
    let foreground = egui::Color32::from_rgb(37, 35, 45);
    visuals.panel_fill = egui::Color32::from_rgb(250, 249, 252);
    visuals.window_fill = egui::Color32::from_rgb(255, 255, 255);
    visuals.extreme_bg_color = egui::Color32::from_rgb(247, 246, 249);
    visuals.faint_bg_color = egui::Color32::from_rgba_unmultiplied(132, 115, 174, 18);
    visuals.override_text_color = Some(foreground);
    visuals.selection.bg_fill = egui::Color32::from_rgb(226, 220, 239);
    visuals.hyperlink_color = ACCENT_BRIGHT;
    visuals.weak_text_color = Some(TEXT_MUTED);
    visuals.window_corner_radius = egui::CornerRadius::same(18);
    visuals.window_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(14));
    visuals.widgets.noninteractive.bg_fill = LAVENDER_SURFACE;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(10);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(251, 249, 254);
    visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(247, 245, 250);
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(232, 228, 239));
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(239, 234, 247);
    visuals.widgets.hovered.bg_stroke =
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(187, 174, 211));
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(232, 225, 243);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, ACCENT_BRIGHT);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
    visuals.widgets.open.bg_fill = egui::Color32::from_rgb(241, 237, 248);
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(10);
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(15.0, 9.0);
    style.spacing.interact_size = egui::vec2(34.0, 34.0);
    ctx.set_style(style);
}

#[cfg(target_os = "macos")]
fn remove_macshot_titlebar_separator() {
    use objc2_app_kit::{NSApplication, NSTitlebarSeparatorStyle};
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    let target_title = NSString::from_str("MacShot");
    let windows = application.windows();
    for index in 0..windows.count() {
        let window = unsafe { windows.objectAtIndex(index) };
        if unsafe { window.title().isEqualToString(&target_title) } {
            unsafe { window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None) };
            return;
        }
    }
}

fn glass_frame() -> egui::Frame {
    glass_frame_with_margin(16)
}

fn settings_card_frame(padding: SettingsCardPadding) -> egui::Frame {
    glass_frame_with_margin(padding.margin())
}

fn glass_frame_with_margin(margin: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(255, 255, 255))
        .stroke(egui::Stroke::NONE)
        .corner_radius(egui::CornerRadius::same(16))
        .inner_margin(egui::Margin::same(margin))
}

fn primary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(label.into().color(egui::Color32::from_rgb(64, 54, 82)))
        .corner_radius(egui::CornerRadius::same(11))
        .min_size(egui::vec2(112.0, 42.0))
}

fn paint_glass_backdrop(ui: &egui::Ui) {
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(249, 248, 251));
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

fn tray_capture_method_label(
    language: AppLanguage,
    label_key: &str,
    selected: bool,
    hotkey: &str,
) -> String {
    let marker = if selected { "✓ " } else { "" };
    let shortcut = if selected && !hotkey.is_empty() {
        format!("  ({})", hotkey_badge_text(hotkey))
    } else {
        String::new()
    };
    format!("{marker}{}{shortcut}", language.tray_label(label_key))
}

fn tray_target_choice_label(
    language: AppLanguage,
    target: CaptureTarget,
    selected: bool,
    monitors: &[Monitor],
    windows: &[WindowChoice],
) -> String {
    let label = match target {
        CaptureTarget::ActiveWindow => language.text("アクティブウィンドウ"),
        CaptureTarget::PrimaryDisplay => language.text("デスクトップ（メイン画面）"),
        CaptureTarget::AllDisplays => language.text("全画面（全ディスプレイ）"),
        CaptureTarget::Display(index) => {
            let name = monitors
                .get(index)
                .and_then(|monitor| monitor.name().ok())
                .unwrap_or_else(|| format!("{} {}", language.text("Display"), index + 1));
            format!("{} — {name}", language.text("Full Display"))
        }
        CaptureTarget::Window(id) => windows
            .iter()
            .find(|window| window.id == id)
            .map(|window| {
                format!(
                    "{} — {} ({} × {} px)",
                    window.app_name, window.title, window.width, window.height
                )
            })
            .unwrap_or_else(|| language.text("Window")),
    };
    if selected {
        format!("✓ {label}")
    } else {
        label
    }
}

fn append_tray_target_item(
    submenu: &Submenu,
    items: &mut Vec<(MenuItem, CaptureTarget)>,
    id: String,
    target: CaptureTarget,
    selected: CaptureTarget,
    language: AppLanguage,
    monitors: &[Monitor],
    windows: &[WindowChoice],
) -> Result<(), String> {
    let item = MenuItem::with_id(
        id,
        tray_target_choice_label(language, target, selected == target, monitors, windows),
        true,
        None,
    );
    submenu.append(&item).map_err(|error| error.to_string())?;
    items.push((item, target));
    Ok(())
}

fn build_tray_target_submenu(
    language: AppLanguage,
    selected: CaptureTarget,
    monitors: &[Monitor],
    windows: &[WindowChoice],
) -> Result<(Submenu, Vec<(MenuItem, CaptureTarget)>), String> {
    let submenu = Submenu::with_id(
        "macshot.capture.targets",
        language.tray_label("target_menu"),
        true,
    );
    let mut items = Vec::new();
    for (id, target) in [
        (
            "macshot.target.active".to_owned(),
            CaptureTarget::ActiveWindow,
        ),
        (
            "macshot.target.primary".to_owned(),
            CaptureTarget::PrimaryDisplay,
        ),
        ("macshot.target.all".to_owned(), CaptureTarget::AllDisplays),
    ] {
        append_tray_target_item(
            &submenu, &mut items, id, target, selected, language, monitors, windows,
        )?;
    }

    if !monitors.is_empty() {
        submenu
            .append(&PredefinedMenuItem::separator())
            .map_err(|error| error.to_string())?;
        for index in 0..monitors.len() {
            append_tray_target_item(
                &submenu,
                &mut items,
                format!("macshot.target.display.{index}"),
                CaptureTarget::Display(index),
                selected,
                language,
                monitors,
                windows,
            )?;
        }
    }

    if !windows.is_empty() {
        submenu
            .append(&PredefinedMenuItem::separator())
            .map_err(|error| error.to_string())?;
        for window in windows {
            append_tray_target_item(
                &submenu,
                &mut items,
                format!("macshot.target.window.{}", window.id),
                CaptureTarget::Window(window.id),
                selected,
                language,
                monitors,
                windows,
            )?;
        }
    }

    Ok((submenu, items))
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

#[allow(clippy::too_many_arguments)]
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
    let recording_capture = RecordingCapture::new(target)?;
    let first = quality
        .resolution
        .resize(recording_capture.capture_image()?, false);
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
            .resize(recording_capture.capture_image()?, false);
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
    let recording_capture = RecordingCapture::new(target)?;
    let first = quality
        .resolution
        .resize(recording_capture.capture_image()?, true);
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
            .usage_type(UsageType::ScreenContentRealTime)
            // OpenH264 ignores these defaults for screen-content encoding and logs a
            // warning each time the encoder starts. Keep the effective behavior explicit.
            .adaptive_quantization(false)
            .background_detection(false),
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
            .resize(recording_capture.capture_image()?, true);
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
                } else if self.continuous_capture {
                    // Keep repeated captures immediate; reopening the mode picker
                    // would discard the remembered region and make users redraw it.
                    self.begin_capture(ctx);
                } else if self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode {
                    self.open_capture_target_picker(ctx);
                } else {
                    self.begin_capture(ctx);
                }
            } else if event.id == self.recording_hotkey.id() {
                self.toggle_recording(ctx);
            }
        }
    }

    fn reset_tray_menu(&mut self) {
        self._tray_icon = None;
        self.tray_initialized = false;
        self.tray_capture_target_item = None;
        self.tray_capture_target_text = None;
        self.tray_target_selection_items.clear();
        self.tray_capture_active_item = None;
        self.tray_capture_primary_item = None;
        self.tray_capture_all_item = None;
        self.tray_capture_current_target_item = None;
        self.tray_capture_shortcut_target_item = None;
        self.tray_capture_shortcut_mode_item = None;
        self.tray_capture_status_item = None;
        self.tray_window_selection_item = None;
        self.tray_continuous_capture_item = None;
        self.tray_recording_status_item = None;
        self.tray_recording_action_item = None;
        self.tray_recording_state = None;
        self.tray_recording_seconds = None;
        self.tray_capture_status_state = None;
    }

    fn initialize_tray(&mut self, ctx: &egui::Context) {
        if self.tray_initialized {
            return;
        }
        self.tray_initialized = true;
        let show_window_item = MenuItem::with_id(
            "macshot.window.show",
            self.language.tray_label("show"),
            true,
            None,
        );
        let capture_target_item = MenuItem::with_id(
            "macshot.capture.target",
            format!(
                "{}: {}",
                self.language.tray_label("current_target"),
                self.target_caption()
            ),
            false,
            None,
        );
        let (target_selection_submenu, target_selection_items) = match build_tray_target_submenu(
            self.language,
            self.capture_target,
            &self.monitors,
            &self.windows,
        ) {
            Ok(menu) => menu,
            Err(error) => {
                self.status = format!("撮影対象メニューを作成できません: {error}");
                return;
            }
        };
        let capture_area_item = MenuItem::with_id(
            "macshot.capture.area",
            self.language.tray_label("area"),
            true,
            None,
        );
        let capture_active_selected = matches!(self.capture_target, CaptureTarget::ActiveWindow);
        let capture_active_item = MenuItem::with_id(
            "macshot.capture.active",
            tray_capture_method_label(self.language, "active", capture_active_selected, ""),
            true,
            None,
        );
        let capture_primary_selected = matches!(self.capture_target, CaptureTarget::PrimaryDisplay);
        let capture_primary_item = MenuItem::with_id(
            "macshot.capture.primary",
            tray_capture_method_label(self.language, "primary", capture_primary_selected, ""),
            true,
            None,
        );
        let capture_all_selected = matches!(self.capture_target, CaptureTarget::AllDisplays);
        let capture_all_item = MenuItem::with_id(
            "macshot.capture.all",
            tray_capture_method_label(self.language, "all", capture_all_selected, ""),
            true,
            None,
        );
        let capture_current_target_item = MenuItem::with_id(
            "macshot.capture.current-target",
            format!(
                "{}  ({})",
                self.language.tray_label(
                    if self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode {
                        "choose_mode"
                    } else {
                        "selected"
                    }
                ),
                hotkey_badge_text(&self.capture_hotkey_text)
            ),
            true,
            None,
        );
        let shortcut_target_item = MenuItem::with_id(
            "macshot.capture.shortcut-target",
            format!(
                "{}{}",
                if self.capture_shortcut_behavior == CaptureShortcutBehavior::SelectedTarget {
                    "✓  "
                } else {
                    "    "
                },
                self.language.tray_label("shortcut_target")
            ),
            true,
            None,
        );
        let shortcut_mode_item = MenuItem::with_id(
            "macshot.capture.shortcut-mode",
            format!(
                "{}{}",
                if self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode {
                    "✓  "
                } else {
                    "    "
                },
                self.language.tray_label("shortcut_mode")
            ),
            true,
            None,
        );
        let capture_window_selection_item = MenuItem::with_id(
            "macshot.capture.window-selection",
            self.language.tray_label("window_select"),
            true,
            None,
        );
        let continuous_capture_item = MenuItem::with_id(
            "macshot.capture.continuous",
            self.language.tray_label(if self.continuous_capture {
                "continuous_stop"
            } else {
                "continuous_start"
            }),
            true,
            None,
        );
        let native_screenshot_item = MenuItem::with_id(
            "macshot.capture.native",
            self.language.tray_label("native"),
            cfg!(target_os = "macos"),
            None,
        );
        let capture_status_item = MenuItem::with_id(
            "macshot.capture.status",
            self.language.tray_label("capture_stopped"),
            false,
            None,
        );
        let recording_status_item = MenuItem::with_id(
            "macshot.recording.status",
            self.language.tray_label("record_time"),
            false,
            None,
        );
        let recording_item = MenuItem::with_id(
            "macshot.record",
            format!(
                "{}  ({})",
                self.language
                    .tray_label(if self.recording || self.recording_starting {
                        "record_stop"
                    } else {
                        "record_start"
                    }),
                hotkey_badge_text(&self.recording_hotkey_text)
            ),
            true,
            None,
        );
        let quit_item =
            MenuItem::with_id("macshot.quit", self.language.tray_label("quit"), true, None);
        let capture_separator = PredefinedMenuItem::separator();
        let recording_separator = PredefinedMenuItem::separator();
        let quit_separator = PredefinedMenuItem::separator();
        let menu = match Menu::with_items(&[
            &show_window_item,
            &capture_target_item,
            &target_selection_submenu,
            &capture_separator,
            &capture_area_item,
            &capture_active_item,
            &capture_primary_item,
            &capture_all_item,
            &capture_current_target_item,
            &shortcut_target_item,
            &shortcut_mode_item,
            &capture_window_selection_item,
            &native_screenshot_item,
            &recording_separator,
            &continuous_capture_item,
            &capture_status_item,
            &quit_separator,
            &recording_item,
            &recording_status_item,
            &PredefinedMenuItem::separator(),
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
        let capture_primary_id = capture_primary_item.id().0.clone();
        let capture_all_id = capture_all_item.id().0.clone();
        let capture_current_target_id = capture_current_target_item.id().0.clone();
        let shortcut_target_id = shortcut_target_item.id().0.clone();
        let shortcut_mode_id = shortcut_mode_item.id().0.clone();
        let capture_window_selection_id = capture_window_selection_item.id().0.clone();
        let native_screenshot_id = native_screenshot_item.id().0.clone();
        let continuous_capture_id = continuous_capture_item.id().0.clone();
        let record_id = recording_item.id().0.clone();
        let quit_id = quit_item.id().0.clone();
        let target_actions: HashMap<_, _> = target_selection_items
            .iter()
            .map(|(item, target)| (item.id().0.clone(), *target))
            .collect();
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
                id if id == capture_primary_id => Some(TrayAction::CapturePrimaryDisplay),
                id if id == capture_all_id => Some(TrayAction::CaptureAllDisplays),
                id if id == capture_current_target_id => Some(TrayAction::CaptureCurrentTarget),
                id if id == shortcut_target_id => Some(TrayAction::SetCaptureShortcutBehavior(
                    CaptureShortcutBehavior::SelectedTarget,
                )),
                id if id == shortcut_mode_id => Some(TrayAction::SetCaptureShortcutBehavior(
                    CaptureShortcutBehavior::ChooseMode,
                )),
                id if id == capture_window_selection_id => Some(TrayAction::CaptureSelectedWindow),
                id if id == native_screenshot_id => Some(TrayAction::OpenNativeScreenshot),
                id if id == continuous_capture_id => Some(TrayAction::ToggleContinuousCapture),
                id if id == record_id => Some(TrayAction::ToggleRecording),
                id if id == quit_id => Some(TrayAction::Quit),
                id => target_actions
                    .get(id)
                    .copied()
                    .map(TrayAction::SelectCaptureTarget),
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
                self.tray_capture_target_item = Some(capture_target_item);
                self.tray_target_selection_items = target_selection_items;
                self.tray_capture_status_item = Some(capture_status_item);
                self.tray_capture_active_item = Some(capture_active_item);
                self.tray_capture_primary_item = Some(capture_primary_item);
                self.tray_capture_all_item = Some(capture_all_item);
                self.tray_capture_current_target_item = Some(capture_current_target_item);
                self.tray_capture_shortcut_target_item = Some(shortcut_target_item);
                self.tray_capture_shortcut_mode_item = Some(shortcut_mode_item);
                self.tray_window_selection_item = Some(capture_window_selection_item);
                self.tray_continuous_capture_item = Some(continuous_capture_item);
                self.tray_recording_status_item = Some(recording_status_item);
                self.tray_recording_action_item = Some(recording_item);
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
        if let Some(item) = &self.tray_continuous_capture_item {
            item.set_text(self.language.tray_label(if self.continuous_capture {
                "continuous_stop"
            } else {
                "continuous_start"
            }));
        }
        if let Some(item) = &self.tray_recording_action_item {
            item.set_text(format!(
                "{}  ({})",
                self.language.tray_label(if active {
                    "record_stop"
                } else {
                    "record_start"
                }),
                hotkey_badge_text(&self.recording_hotkey_text)
            ));
        }
        let label = if active {
            format!(
                "{} {}",
                self.language.tray_label("recording"),
                recording_time_label(Duration::from_secs(seconds))
            )
        } else {
            self.language.text("録画時間 00:00")
        };
        if let Some(item) = &self.tray_recording_status_item {
            // Keep elapsed time visible while recording; otherwise show a direct start action.
            item.set_text(if active {
                label
            } else {
                self.language.tray_label("record_time").to_owned()
            });
        }
        if let Some(item) = &self.tray_capture_status_item {
            item.set_text(self.language.message(&continuous_capture_status_label(
                self.continuous_capture,
                self.continuous_capture_session_active,
                self.capture_in_progress,
                self.continuous_capture_saved_count,
                self.continuous_capture_failed_count,
            )));
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

    fn update_tray_capture_target(&mut self) {
        let selected_target = self.target_caption();
        let status_label = format!(
            "{}: {}",
            self.language.tray_label("current_target"),
            selected_target
        );
        let target_state = format!(
            "{status_label}\n{}\n{}",
            self.capture_hotkey_text,
            self.capture_shortcut_behavior.setting()
        );
        if self.tray_capture_target_text.as_deref() != Some(target_state.as_str()) {
            if let Some(item) = &self.tray_capture_target_item {
                item.set_text(status_label);
            }
            self.tray_capture_target_text = Some(target_state);

            let is_active = matches!(self.capture_target, CaptureTarget::ActiveWindow);
            let is_primary = matches!(self.capture_target, CaptureTarget::PrimaryDisplay);
            let is_all = matches!(self.capture_target, CaptureTarget::AllDisplays);
            if let Some(item) = &self.tray_capture_active_item {
                item.set_text(tray_capture_method_label(
                    self.language,
                    "active",
                    is_active,
                    "",
                ));
            }
            if let Some(item) = &self.tray_capture_primary_item {
                item.set_text(tray_capture_method_label(
                    self.language,
                    "primary",
                    is_primary,
                    "",
                ));
            }
            if let Some(item) = &self.tray_capture_all_item {
                item.set_text(tray_capture_method_label(self.language, "all", is_all, ""));
            }
            if let Some(item) = &self.tray_capture_current_target_item {
                item.set_text(format!(
                    "{}  ({})",
                    self.language.tray_label(
                        if self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode {
                            "choose_mode"
                        } else {
                            "selected"
                        }
                    ),
                    hotkey_badge_text(&self.capture_hotkey_text)
                ));
            }
            if let Some(item) = &self.tray_capture_shortcut_target_item {
                item.set_text(format!(
                    "{}{}",
                    if self.capture_shortcut_behavior == CaptureShortcutBehavior::SelectedTarget {
                        "✓  "
                    } else {
                        "    "
                    },
                    self.language.tray_label("shortcut_target")
                ));
            }
            if let Some(item) = &self.tray_capture_shortcut_mode_item {
                item.set_text(format!(
                    "{}{}",
                    if self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode {
                        "✓  "
                    } else {
                        "    "
                    },
                    self.language.tray_label("shortcut_mode")
                ));
            }
            if let Some(item) = &self.tray_window_selection_item {
                item.set_text(self.language.tray_label("window_select"));
            }
            for (item, target) in &self.tray_target_selection_items {
                item.set_text(tray_target_choice_label(
                    self.language,
                    *target,
                    self.capture_target == *target,
                    &self.monitors,
                    &self.windows,
                ));
            }
        }
    }

    fn handle_tray_actions(&mut self, ctx: &egui::Context) {
        while let Ok(action) = self.tray_action_rx.try_recv() {
            match action {
                TrayAction::ShowWindow => {
                    #[cfg(target_os = "macos")]
                    set_macos_activation_policy(
                        objc2_app_kit::NSApplicationActivationPolicy::Regular,
                    );
                    self.status = "MacShotのウィンドウを表示しました。".into();
                    restore_main_window(ctx);
                }
                TrayAction::CaptureSelectedWindow => {
                    if self.recording || self.recording_starting {
                        self.report_capture_failure(
                            "録画を終了してからスクリーンショットを撮影してください。",
                        );
                    } else {
                        self.begin_interactive_window_capture(ctx);
                    }
                }
                TrayAction::CaptureCurrentTarget => {
                    if self.recording || self.recording_starting {
                        self.report_capture_failure(
                            "録画を終了してからスクリーンショットを撮影してください。",
                        );
                    } else if self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode
                    {
                        self.open_capture_target_picker(ctx);
                    } else {
                        self.begin_capture(ctx);
                    }
                }
                TrayAction::CaptureActiveWindow => {
                    self.begin_capture_for(ctx, CaptureTarget::ActiveWindow, None);
                }
                TrayAction::CapturePrimaryDisplay => {
                    self.begin_capture_for(ctx, CaptureTarget::PrimaryDisplay, None);
                }
                TrayAction::CaptureAllDisplays => {
                    self.begin_capture_for(ctx, CaptureTarget::AllDisplays, None);
                }
                TrayAction::SelectCaptureTarget(target) => {
                    let previous_target = self.capture_target;
                    self.capture_target = target;
                    match self.save_preferences() {
                        Ok(()) => {
                            self.status =
                                format!("撮影対象を「{}」に変更しました。", self.target_caption());
                            self.tray_capture_target_text = None;
                            self.update_tray_capture_target();
                        }
                        Err(error) => {
                            self.capture_target = previous_target;
                            self.tray_capture_target_text = None;
                            self.settings_error =
                                Some(format!("撮影対象を保存できません: {error}"));
                        }
                    }
                }
                TrayAction::SetCaptureShortcutBehavior(behavior) => {
                    self.set_capture_shortcut_behavior(behavior);
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

    fn set_capture_shortcut_behavior(&mut self, behavior: CaptureShortcutBehavior) {
        if self.capture_shortcut_behavior == behavior {
            return;
        }
        let previous = self.capture_shortcut_behavior;
        self.capture_shortcut_behavior = behavior;
        match self.save_preferences() {
            Ok(()) => {
                self.tray_capture_target_text = None;
                self.update_tray_capture_target();
                self.status = format!(
                    "ショートカットの動作を「{}」に変更しました。",
                    self.language.text(match behavior {
                        CaptureShortcutBehavior::SelectedTarget => "撮影対象選択を優先",
                        CaptureShortcutBehavior::ChooseMode => "各モード優先",
                    })
                );
            }
            Err(error) => {
                self.capture_shortcut_behavior = previous;
                self.settings_error =
                    Some(format!("ショートカットの動作を保存できません: {error}"));
                self.tray_capture_target_text = None;
                self.update_tray_capture_target();
            }
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
                    if self.auto_import_native_captures {
                        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                        let directories = native_capture_directories(&folder);
                        let known_files = scan_native_capture_files(&directories)
                            .into_keys()
                            .collect();
                        self.native_capture_watch = Some(NativeCaptureWatch {
                            started_at: Instant::now(),
                            known_files,
                            in_flight: HashSet::new(),
                            pending_sizes: HashMap::new(),
                        });
                        self.status = "macOS標準パネルを開きました。撮影後の画像・録画を保存先へ自動で取り込みます。".into();
                    } else {
                        self.status = "macOS標準の撮影パネルを開きました。撮影・録画の対象とオプションを選べます。".into();
                    }
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

    #[cfg(target_os = "macos")]
    fn poll_native_capture_imports(&mut self, ctx: &egui::Context) {
        let mut imported = 0;
        let mut failures = Vec::new();
        while let Ok(event) = self.native_capture_import_rx.try_recv() {
            if let Some(watch) = self.native_capture_watch.as_mut() {
                watch.in_flight.remove(&event.source);
                match event.result {
                    Ok(_) => {
                        watch.known_files.insert(event.source.clone());
                        watch.pending_sizes.remove(&event.source);
                        imported += 1;
                    }
                    Err(error) => {
                        // Recheck a stable file after a short delay if a transient copy failure
                        // occurred (for example, the OS still held the file open).
                        watch.pending_sizes.remove(&event.source);
                        failures.push(error);
                    }
                }
            }
        }

        if !self.auto_import_native_captures {
            self.native_capture_watch = None;
            return;
        }

        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        if let Some(watch) = self.native_capture_watch.as_mut() {
            if watch.started_at.elapsed() > Duration::from_secs(12 * 60 * 60)
                && watch.in_flight.is_empty()
            {
                self.native_capture_watch = None;
            } else {
                let directories = native_capture_directories(&folder);
                let current = scan_native_capture_files(&directories);
                let ready = completed_native_capture_files(watch, current);
                for source in ready {
                    watch.in_flight.insert(source.clone());
                    let destination_folder = folder.clone();
                    let import_tx = self.native_capture_import_tx.clone();
                    spawn_native_capture_import(source, destination_folder, import_tx);
                }
            }
        }

        if imported > 0 {
            self.evidence_files = evidence_images(&folder);
            self.recording_files = recording_files(&folder);
            let failure_note = if failures.is_empty() {
                String::new()
            } else {
                format!(" 一部失敗: {}", failures.join("、"))
            };
            self.status = format!(
                "macOS標準パネルの撮影結果を{imported}件取り込みました。証跡一覧に追加しました。{failure_note}"
            );
        } else if !failures.is_empty() {
            self.status = format!(
                "標準パネルの撮影結果を取り込めませんでした: {}",
                failures.join("、")
            );
        }
        if self.native_capture_watch.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
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
    fn request_screen_recording_access(&mut self) {
        let granted = unsafe { CGRequestScreenCaptureAccess() };
        self.status = if granted {
            "画面収録の許可を確認しました。".into()
        } else {
            "画面収録の許可をmacOSに要求しました。設定でMacShotをオンにしてください。反映しない場合はメニューバーのMacShotから終了して再起動してください。".into()
        };
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
                            self.add_notification(AppNotification {
                                kind: NotificationKind::Recording,
                                timestamp: finished_at.clone(),
                                path: path.clone(),
                                width: None,
                                height: None,
                                detail: recording_warning.clone(),
                            });
                            self.recording_notice = Some((path, finished_at, recording_warning));
                            let folder =
                                self.save_folder.clone().unwrap_or_else(default_save_folder);
                            self.recording_files = recording_files(&folder);
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
        self.tray_recording_state = None;
        self.update_tray_recording_status();
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
            language: self.language,
            capture_hotkey: &self.capture_hotkey_text,
            recording_hotkey: &self.recording_hotkey_text,
            capture_target: &self.capture_target_setting(),
            capture_shortcut_behavior: self.capture_shortcut_behavior,
            timestamp_enabled: self.timestamp_enabled,
            timestamp_format: &self.timestamp_format,
            timestamp_position: &self.timestamp_position,
            timestamp_background: &self.timestamp_background,
            filename_prefix: &self.filename_prefix,
            show_capture_notice: self.show_capture_notice,
            keep_region_in_continuous: self.keep_region_in_continuous,
            annotation_color: &self.annotation_color,
            annotation_width: self.annotation_width,
            annotation_text: &self.annotation_text,
            save_folder: &folder,
            auto_import_native_captures: self.auto_import_native_captures,
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
        if self.has_pending_edit() {
            self.pending_edit_navigation =
                Some(PendingEditNavigation::OpenEvidence { queue, index });
            return;
        }
        self.load_evidence_image(queue, index);
    }

    fn load_evidence_image(&mut self, queue: Vec<PathBuf>, index: usize) {
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
                self.pending_edit_navigation = None;
                switch_page(&mut self.page, Page::Capture, &mut self.status);
                self.status = format!("編集する画像を開きました: {}", path.display());
                self.refresh_diff();
            }
            Err(error) => self.status = format!("画像を開けません: {error}"),
        }
    }

    fn move_in_edit_queue(&mut self, direction: isize) {
        self.commit_live_annotation();
        let Some(index) = self.editing_index else {
            return;
        };
        let next = index as isize + direction;
        if next < 0 || next >= self.editing_queue.len() as isize {
            return;
        }
        if self.has_pending_edit() {
            self.pending_edit_navigation = Some(PendingEditNavigation::Queue { direction });
            return;
        }
        self.navigate_in_edit_queue(direction, false);
    }

    fn navigate_in_edit_queue(&mut self, direction: isize, save_pending_edit: bool) {
        self.commit_live_annotation();
        let Some(index) = self.editing_index else {
            return;
        };
        let next = index as isize + direction;
        if next < 0 || next >= self.editing_queue.len() as isize {
            return;
        }
        if save_pending_edit {
            let saved = if self.crop_selection.is_some() {
                self.save_crop()
            } else if !self.annotations.is_empty() {
                self.save_annotated_image()
            } else {
                true
            };
            if !saved {
                return;
            }
        }
        self.pending_edit_navigation = None;
        self.load_evidence_image(self.editing_queue.clone(), next as usize);
    }

    fn save_then_next(&mut self) {
        self.navigate_in_edit_queue(1, true);
    }

    fn finish_region_capture(&mut self, ctx: &egui::Context) {
        let saved = if self.crop_selection.is_some() {
            self.save_crop()
        } else if !self.annotations.is_empty() {
            self.save_annotated_image()
        } else {
            true
        };
        if saved {
            self.region_annotation_pending = false;
            self.status = "範囲撮影を完了しました。".into();
            if self.continuous_capture {
                minimize_main_window_for_capture(ctx);
            }
        }
    }

    fn has_pending_edit(&self) -> bool {
        self.crop_selection.is_some()
            || !self.annotations.is_empty()
            || self.live_annotation.is_some()
    }

    fn commit_live_annotation(&mut self) {
        if let Some(annotation) = self.live_annotation.take() {
            self.annotations.push(annotation);
        }
    }

    fn show_pending_edit_navigation_confirmation(&mut self, ctx: &egui::Context) {
        let Some(navigation) = self.pending_edit_navigation.clone() else {
            return;
        };
        let mut open = true;
        let mut save_and_move = false;
        let mut discard_and_move = false;
        let mut keep_editing = false;
        egui::Window::new(self.language.text("未保存の編集があります"))
            .id(egui::Id::new("pending_edit_navigation_confirmation"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    self.language
                        .text("移動する前に、現在の注釈や切り抜きを保存しますか？"),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(primary_button(self.language.text("保存して移動")))
                        .clicked()
                    {
                        save_and_move = true;
                    }
                    if ui.button(self.language.text("破棄して移動")).clicked() {
                        discard_and_move = true;
                    }
                    if ui.button(self.language.text("編集に戻る")).clicked() {
                        keep_editing = true;
                    }
                });
            });

        if keep_editing {
            open = false;
        }
        if save_and_move || discard_and_move {
            let save = save_and_move;
            let saved = if save && self.has_pending_edit() {
                if self.crop_selection.is_some() {
                    self.save_crop()
                } else if !self.annotations.is_empty() {
                    self.save_annotated_image()
                } else {
                    true
                }
            } else {
                true
            };
            if saved {
                self.pending_edit_navigation = None;
                match navigation {
                    PendingEditNavigation::Queue { direction } => {
                        self.navigate_in_edit_queue(direction, false);
                    }
                    PendingEditNavigation::OpenEvidence { queue, index } => {
                        self.load_evidence_image(queue, index);
                    }
                }
            }
        } else if !open {
            self.pending_edit_navigation = None;
        }
    }

    fn begin_capture(&mut self, ctx: &egui::Context) {
        // A retained range belongs to the continuous-capture session and is
        // always measured against the primary display, even if the regular
        // screenshot target changed before the session began.
        let retained_region = retained_continuous_region(
            self.continuous_capture,
            self.keep_region_in_continuous,
            self.continuous_region,
        );
        if let Some(region) = retained_region {
            self.begin_capture_for(ctx, CaptureTarget::PrimaryDisplay, Some(region));
            return;
        }
        let region = None;
        self.begin_capture_for(ctx, self.capture_target, region);
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
        while let Some(request) = self.capture_queue.pop_front() {
            self.begin_capture_for(ctx, request.target, request.region);
            if self.capture_in_progress {
                return;
            }
        }
        if self.region_annotation_pending {
            restore_main_window(ctx);
        } else if !self.continuous_capture {
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
        self.commit_live_annotation();
        if self.has_pending_edit() {
            self.status = self
                .language
                .text("編集内容を保存するか消去してから撮影してください。")
                .into();
            return;
        }
        if self.recording || self.recording_starting || self.capture_in_progress {
            self.report_capture_failure("録画または撮影が終わってから範囲撮影を開始してください。");
            return;
        }
        #[cfg(target_os = "windows")]
        self.begin_region_selection_windows(ctx);
        #[cfg(target_os = "macos")]
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
            self.region_background_texture = None;
            self.region_selection_deadline = Some(Instant::now() + Duration::from_secs(15));
            self.region_selecting = false;
            self.status =
                "範囲選択を準備しています…画面の確認に時間がかかる場合は自動で戻ります。".into();
            minimize_main_window_for_capture(ctx);
            let sender = self.region_snapshot_tx.clone();
            let repaint = ctx.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(350));
                let result = capture_target_image(CaptureTarget::PrimaryDisplay);
                let _ = sender.send(result);
                repaint.request_repaint();
            });
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            self.status = "この環境では範囲撮影を利用できません。".into();
        }
    }

    fn handle_region_snapshot(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "windows")]
        while let Ok(result) = self.region_geometry_rx.try_recv() {
            if !self.region_geometry_pending {
                continue;
            }
            #[cfg(target_os = "windows")]
            {
                self.region_geometry_pending = false;
            }
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
            #[cfg(target_os = "windows")]
            {
                self.region_geometry_pending = false;
            }
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
        let language = self.language;
        let title = language.text("MacShot 範囲選択");
        let viewport_id = egui::ViewportId::from_hash_of("macshot-region-selector");
        let builder = egui::ViewportBuilder::default()
            .with_title(&title)
            .with_active(true)
            .with_position(self.region_overlay_position)
            .with_inner_size(self.region_overlay_size)
            .with_min_inner_size(self.region_overlay_size)
            .with_max_inner_size(self.region_overlay_size)
            .with_decorations(false)
            .with_transparent(false)
            .with_always_on_top()
            .with_mouse_passthrough(false)
            .with_resizable(false);
        let mut action = None;
        ctx.show_viewport_immediate(viewport_id, builder, |viewport_ctx, _class| {
            #[cfg(target_os = "macos")]
            raise_window_above_system_bars(&title);
            viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);

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
                    let (pressed, down, released, pointer, press_origin) =
                        viewport_ctx.input(|input| {
                            (
                                input.pointer.button_pressed(egui::PointerButton::Primary),
                                input.pointer.button_down(egui::PointerButton::Primary),
                                input.pointer.button_released(egui::PointerButton::Primary),
                                input.pointer.interact_pos(),
                                input.pointer.press_origin(),
                            )
                        });
                    update_region_drag_state(
                        &mut self.region_drag_start,
                        &mut self.region_drag_current,
                        pressed,
                        down,
                        released,
                        pointer,
                        press_origin,
                    );
                    let painter = ui.painter();
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
                            language.text("ドラッグして撮影範囲を選択　·　Escでキャンセル"),
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
                    let cancel_rect = egui::Rect::from_min_size(
                        egui::pos2(screen.right() - 164.0, screen.top() + 16.0),
                        egui::vec2(148.0, 38.0),
                    );
                    if ui
                        .put(
                            cancel_rect,
                            egui::Button::new(language.text("Esc  キャンセル")),
                        )
                        .clicked()
                    {
                        action = Some(None);
                        viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            viewport_ctx.request_repaint_after(Duration::from_millis(16));
        });
        if let Some(selection) = action {
            self.region_selecting = false;
            self.region_background_texture = None;
            restore_main_window(ctx);
            if let Some(bounds) = selection {
                if self.keep_region_in_continuous {
                    self.continuous_region = Some(bounds);
                } else {
                    self.continuous_region = None;
                }
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
        self.begin_capture_task(ctx, target, region, false);
    }

    fn begin_interactive_window_capture(&mut self, ctx: &egui::Context) {
        if self.recording || self.recording_starting {
            self.report_capture_failure("録画を終了してからウィンドウ撮影を開始してください。");
            return;
        }
        if self.capture_in_progress {
            self.status = "撮影の保存が終わってからウィンドウを選んでください。".into();
            return;
        }
        #[cfg(target_os = "macos")]
        self.begin_capture_task(ctx, CaptureTarget::ActiveWindow, None, true);
        #[cfg(not(target_os = "macos"))]
        self.open_capture_target_picker(ctx);
    }

    fn begin_capture_task(
        &mut self,
        ctx: &egui::Context,
        target: CaptureTarget,
        region: Option<[f32; 4]>,
        interactive_window_selection: bool,
    ) {
        self.commit_live_annotation();
        if self.has_pending_edit() {
            self.status = self
                .language
                .text("編集内容を保存するか消去してから撮影してください。")
                .into();
            return;
        }
        if self.capture_in_progress {
            self.capture_queue
                .push_back(CaptureRequest { target, region });
            self.status = format!(
                "撮影を順番待ちに追加しました（残り{}枚）。",
                self.capture_queue.len()
            );
            return;
        }
        if !interactive_window_selection {
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
        }

        self.capture_sequence += 1;
        let sequence = self.capture_sequence;
        self.active_capture_sequence = Some(sequence);
        self.last_capture_sequence = sequence;
        let timestamp_format = self.timestamp_format.clone();
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        let filename_prefix = self.filename_prefix.clone();
        let timestamp_enabled = self.timestamp_enabled;
        let timestamp_position = self.timestamp_position.clone();
        let timestamp_background = self.timestamp_background.clone();
        let sender = self.capture_tx.clone();
        let ctx = ctx.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let timeout = if interactive_window_selection {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(15)
        };
        {
            self.capture_deadline = Some(Instant::now() + timeout);
            self.capture_cancel = Some(cancelled.clone());
            let watchdog_context = ctx.clone();
            let watchdog_cancelled = cancelled.clone();
            thread::spawn(move || {
                thread::sleep(timeout);
                if !watchdog_cancelled.load(Ordering::Relaxed) {
                    restore_main_window(&watchdog_context);
                    watchdog_context.request_repaint();
                }
            });
        }
        self.capture_in_progress = true;
        self.active_capture_is_region = region.is_some();
        self.region_annotation_pending = false;
        self.status = if interactive_window_selection {
            "撮影するウィンドウをクリックしてください。Escでキャンセルできます。".into()
        } else {
            format!("撮影 {} 枚目を保存しています…", sequence)
        };
        minimize_main_window_for_capture(&ctx);
        self.update_tray_recording_status();
        ctx.request_repaint_after(Duration::from_millis(100));

        thread::spawn(move || {
            // Give the window manager time to reveal the previously active application.
            thread::sleep(Duration::from_millis(350));
            let capture = if cancelled.load(Ordering::Relaxed) {
                Err(capture_timeout_message().to_string())
            } else {
                let capture_result = if interactive_window_selection {
                    #[cfg(target_os = "macos")]
                    {
                        capture_window_by_user_selection(&cancelled)
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        capture_target_image(target)
                    }
                } else {
                    capture_target_image(target)
                };
                capture_result.and_then(|image| {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(capture_timeout_message().to_string());
                    }
                    // Stamp and name the evidence when the target was actually
                    // captured. Interactive window selection can take a while.
                    let captured_at = Local::now();
                    let timestamp = captured_at.format("%Y-%m-%d %H:%M:%S").to_string();
                    let image_timestamp = capture_image_timestamp(&captured_at, &timestamp_format);
                    let file_timestamp = captured_at.format("%Y%m%d-%H%M%S-%3f").to_string();
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
                        stamp_timestamp(
                            &mut image,
                            &image_timestamp,
                            &timestamp_position,
                            &timestamp_background,
                        );
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
                    self.region_annotation_pending = self.active_capture_is_region;
                    self.active_capture_is_region = false;
                    let (width, height) = image.dimensions();
                    self.status = capture_saved_status(sequence, &timestamp, width, height);
                    self.add_notification(AppNotification {
                        kind: NotificationKind::Capture(sequence),
                        timestamp: timestamp.clone(),
                        path: path.clone(),
                        width: Some(width),
                        height: Some(height),
                        detail: None,
                    });
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
                    self.active_capture_is_region = false;
                    self.region_annotation_pending = false;
                    self.status = if error == "__MACSHOT_WINDOW_SELECTION_CANCELLED__" {
                        "ウィンドウ選択をキャンセルしました。".into()
                    } else {
                        capture_failure_message(&error)
                    };
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
            self.active_capture_is_region = false;
            self.region_annotation_pending = false;
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
                        switch_page(&mut self.page, Page::Compare, &mut self.status);
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
            &self.timestamp_background,
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

    fn can_overwrite_current_png(&self) -> bool {
        self.left_path.as_ref().is_some_and(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        })
    }

    fn overwrite_annotated_image(&mut self) -> bool {
        let Some(path) = self.left_path.clone() else {
            self.status = "上書きする画像ファイルがありません。".into();
            return false;
        };
        let Some(image) = &self.left_image else {
            return false;
        };
        if self.annotations.is_empty() {
            return false;
        }
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            self.status = "上書き保存はPNG画像に対応しています。別名保存をご利用ください。".into();
            return false;
        }
        let output = annotated_image(
            image,
            &self.annotations,
            annotation_rgb(&self.annotation_color),
            self.annotation_width,
            &self.timestamp_background,
        );
        if let Err(error) = overwrite_png(&path, &output) {
            self.status = format!("画像を上書きできません: {error}");
            return false;
        }
        self.left_image = Some(DynamicImage::ImageRgba8(output));
        self.left_texture = None;
        self.mosaic_preview_texture = None;
        self.mosaic_preview_key = None;
        self.annotations.clear();
        self.live_annotation = None;
        self.refresh_diff();
        self.status = format!("画像を上書き保存しました: {}", path.display());
        true
    }

    fn show_overwrite_confirmation(&mut self, ctx: &egui::Context) {
        if !self.overwrite_confirmation {
            return;
        }
        let mut open = true;
        let mut confirmed = false;
        let mut cancelled = false;
        let filename = self
            .left_path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.language.text("画像ファイル").to_string());
        egui::Window::new(self.language.text("上書き保存の確認"))
            .id(egui::Id::new("overwrite_annotation_confirmation"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    self.language
                        .text("注釈を反映して、現在の画像ファイルを上書きします。"),
                );
                ui.label(egui::RichText::new(filename).strong());
                ui.label(
                    self.language
                        .text("別名の画像は作成されません。上書きしますか？"),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(primary_button(self.language.text("上書き保存")))
                        .clicked()
                    {
                        confirmed = true;
                    }
                    if ui.button(self.language.text("キャンセル")).clicked() {
                        cancelled = true;
                    }
                });
            });
        if cancelled {
            open = false;
        }
        self.overwrite_confirmation = open;
        if confirmed {
            self.overwrite_confirmation = false;
            self.overwrite_annotated_image();
        }
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
            &self.timestamp_background,
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

    fn show_clickable_capture_preview(&mut self, ui: &mut egui::Ui) {
        let Some(image) = &self.left_image else {
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
        let (rect, response) = ui.allocate_exact_size(display_size, egui::Sense::click());
        ui.painter().image(
            self.left_texture.as_ref().unwrap().id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        if response.clicked() {
            self.capture_editor_expanded = true;
        }
        response.on_hover_text(self.language.text("クリックして拡大・編集"));
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(self.language.text("クリックして拡大・編集"))
                .size(11.0)
                .color(TEXT_MUTED),
        );
    }

    #[cfg(test)]
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
                ui.label(self.language.text("画面全体またはウィンドウを撮影すると、ここにプレビューが表示されます。\n画像をドラッグして必要な範囲を切り出せます。"));
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
            if !matches!(self.edit_tool, EditTool::Text | EditTool::Timestamp)
                && response.drag_started()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                self.crop_drag_start = Some(to_normalized(pointer));
            }
            if !matches!(self.edit_tool, EditTool::Text | EditTool::Timestamp)
                && response.dragged()
                && let (Some(start), Some(pointer)) =
                    (self.crop_drag_start, response.interact_pointer_pos())
            {
                self.live_annotation = Some(Annotation {
                    tool: self.edit_tool,
                    start,
                    end: to_normalized(pointer),
                    text: self.annotation_text.clone(),
                });
            }
            if response.drag_stopped() {
                let final_pointer = response
                    .interact_pointer_pos()
                    .or_else(|| response.hover_pos());
                let annotation = self.live_annotation.take().or_else(|| {
                    let start = self.crop_drag_start.take()?;
                    let pointer = final_pointer?;
                    let end = to_normalized(pointer);
                    has_annotation_drag_distance(start, end).then(|| Annotation {
                        tool: self.edit_tool,
                        start,
                        end,
                        text: self.annotation_text.clone(),
                    })
                });
                if let Some(annotation) = annotation {
                    self.annotations.push(annotation);
                }
                self.crop_drag_start = None;
            }
            if response.clicked()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let point = to_normalized(pointer);
                if self.edit_tool == EditTool::Text {
                    self.annotations.push(Annotation {
                        tool: EditTool::Text,
                        start: point,
                        end: point,
                        text: self.annotation_text.clone(),
                    });
                } else if self.edit_tool == EditTool::Timestamp {
                    let timestamp = self
                        .left_path
                        .as_deref()
                        .map(|path| timestamp_for_evidence_path(path, &self.timestamp_format))
                        .unwrap_or_else(|| {
                            capture_image_timestamp(&Local::now(), &self.timestamp_format)
                        });
                    self.annotations.push(Annotation {
                        tool: EditTool::Timestamp,
                        start: point,
                        end: point,
                        text: timestamp,
                    });
                } else if let Some(start) = self.crop_drag_start.take() {
                    self.annotations.push(Annotation {
                        tool: self.edit_tool,
                        start,
                        end: point,
                        text: self.annotation_text.clone(),
                    });
                } else {
                    self.crop_drag_start = Some(point);
                    self.status =
                        "始点を指定しました。終点をクリックするかドラッグしてください。".into();
                }
            }
        }

        let mosaic_annotations: Vec<_> = self
            .annotations
            .iter()
            .chain(self.live_annotation.iter())
            .filter(|annotation| annotation.tool == EditTool::Mosaic)
            .cloned()
            .collect();
        if mosaic_annotations.is_empty() {
            self.mosaic_preview_key = None;
            self.mosaic_preview_texture = None;
        } else {
            let cache_key = (mosaic_annotations.clone(), self.left_path.clone());
            if self.mosaic_preview_key.as_ref() != Some(&cache_key)
                || self.mosaic_preview_texture.is_none()
            {
                let preview = annotated_image(
                    image,
                    &mosaic_annotations,
                    annotation_rgb(&self.annotation_color),
                    self.annotation_width,
                    &self.timestamp_background,
                );
                let color = egui::ColorImage::from_rgba_unmultiplied(size, preview.as_raw());
                self.mosaic_preview_texture = Some(ui.ctx().load_texture(
                    "mosaic-annotation-preview",
                    color,
                    egui::TextureOptions::LINEAR,
                ));
                self.mosaic_preview_key = Some(cache_key);
            }
        }
        let preview_texture = self
            .mosaic_preview_texture
            .as_ref()
            .or(self.left_texture.as_ref())
            .expect("capture preview texture is initialized");
        ui.painter().image(
            preview_texture.id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

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
        for annotation in self.annotations.iter().chain(self.live_annotation.iter()) {
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
                EditTool::Ellipse => {
                    let bounds = egui::Rect::from_two_pos(start, end);
                    let points: Vec<_> = (0..=48)
                        .map(|step| {
                            let angle = std::f32::consts::TAU * step as f32 / 48.0;
                            egui::pos2(
                                bounds.center().x + bounds.width() * 0.5 * angle.cos(),
                                bounds.center().y + bounds.height() * 0.5 * angle.sin(),
                            )
                        })
                        .collect();
                    ui.painter().add(egui::Shape::line(points, stroke));
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
                EditTool::Text => {
                    ui.painter().text(
                        start,
                        egui::Align2::LEFT_TOP,
                        &annotation.text,
                        egui::FontId::proportional(18.0 * (self.annotation_width / 3.0)),
                        egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
                    );
                }
                EditTool::Timestamp => {
                    let galley = ui.painter().layout_no_wrap(
                        annotation.text.clone(),
                        egui::FontId::proportional(15.0),
                        egui::Color32::WHITE,
                    );
                    let padding = egui::vec2(5.0, 3.0);
                    let label_rect =
                        egui::Rect::from_min_size(start - padding, galley.size() + padding * 2.0);
                    ui.painter().rect_filled(
                        label_rect,
                        3.0,
                        egui::Color32::from_black_alpha(timestamp_background_alpha(
                            &self.timestamp_background,
                        )),
                    );
                    ui.painter().galley(start, galley, egui::Color32::WHITE);
                }
                EditTool::Mosaic => {}
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
    fn add_notification(&mut self, notification: AppNotification) {
        self.notifications.insert(0, notification);
        self.notifications.truncate(200);
        if let Err(error) = save_notifications(&self.notifications) {
            self.status = format!(
                "{}: {error}",
                self.language.text("通知履歴を保存できません")
            );
        }
    }

    fn clear_notification_history(&mut self) {
        self.notifications.clear();
        if let Err(error) = save_notifications(&self.notifications) {
            self.status = format!(
                "{}: {error}",
                self.language.text("通知履歴を消去できません")
            );
        } else {
            self.status = self.language.text("通知履歴を消去しました");
        }
    }

    fn reveal_notification_file(&mut self, path: &Path) {
        if !path.exists() {
            self.status = self.language.text("ファイルが見つかりません");
            return;
        }
        #[cfg(target_os = "macos")]
        let result = Command::new("open").arg("-R").arg(path).spawn();
        #[cfg(target_os = "windows")]
        let result = Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let result = Command::new("xdg-open")
            .arg(path.parent().unwrap_or_else(|| Path::new(".")))
            .spawn();
        if let Err(error) = result {
            self.status = format!("{}: {error}", self.language.text("Finderで表示できません"));
        }
    }

    fn target_caption(&self) -> String {
        match self.capture_target {
            CaptureTarget::ActiveWindow => self.language.text("アクティブウィンドウ"),
            CaptureTarget::PrimaryDisplay => self.language.text("メインディスプレイ"),
            CaptureTarget::AllDisplays => self.language.text("すべてのディスプレイ"),
            CaptureTarget::Display(index) => self
                .monitors
                .get(index)
                .and_then(|monitor| monitor.name().ok())
                .unwrap_or_else(|| format!("{} {}", self.language.text("Display"), index + 1)),
            CaptureTarget::Window(id) => self
                .windows
                .iter()
                .find(|window| window.id == id)
                .map(window_choice_label)
                .unwrap_or_else(|| self.language.text("ウィンドウを選択")),
        }
    }

    fn open_capture_target_picker(&mut self, ctx: &egui::Context) {
        self.page = Page::Capture;
        self.capture_target_picker_open = true;
        self.status = self
            .language
            .text("ショートカットで開きました。撮影対象を選べます。");
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
        restore_main_window(ctx);
    }

    fn select_capture_target(&mut self, target: CaptureTarget) {
        let previous_target = self.capture_target;
        self.capture_target = target;
        match self.save_preferences() {
            Ok(()) => {
                self.tray_capture_target_text = None;
                self.update_tray_capture_target();
            }
            Err(error) => {
                self.capture_target = previous_target;
                self.settings_error = Some(format!("撮影対象を保存できません: {error}"));
            }
        }
    }

    fn show_capture_target_picker(&mut self, ctx: &egui::Context) {
        if !self.capture_target_picker_open {
            return;
        }

        let mut open = true;
        let mut cancel = false;
        let mut capture = false;
        let mut select_region = false;
        let language = self.language;
        egui::Window::new(language.text("撮影方法を選択"))
            .id(egui::Id::new("capture_target_picker"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .default_width(620.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(language.text("撮影対象を選んでから撮影してください"));
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .selectable_label(false, language.text("範囲を選択"))
                        .clicked()
                    {
                        select_region = true;
                    }
                    for (target, label) in [
                        (CaptureTarget::ActiveWindow, "アクティブウィンドウ"),
                        (CaptureTarget::PrimaryDisplay, "デスクトップ（メイン画面）"),
                        (CaptureTarget::AllDisplays, "全画面（全ディスプレイ）"),
                    ] {
                        let selected = self.capture_target == target;
                        if ui
                            .selectable_label(selected, language.text(label))
                            .clicked()
                        {
                            self.select_capture_target(target);
                        }
                    }
                });

                if !self.monitors.is_empty() {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(language.text("ディスプレイ")).strong());
                    ui.horizontal_wrapped(|ui| {
                        let displays = self
                            .monitors
                            .iter()
                            .enumerate()
                            .map(|(index, monitor)| {
                                (
                                    index,
                                    monitor.name().unwrap_or_else(|_| {
                                        format!("{} {}", language.text("Display"), index + 1)
                                    }),
                                )
                            })
                            .collect::<Vec<_>>();
                        for (index, name) in displays {
                            let target = CaptureTarget::Display(index);
                            if ui
                                .selectable_label(
                                    self.capture_target == target,
                                    format!("{} — {name}", language.text("Full Display")),
                                )
                                .clicked()
                            {
                                self.select_capture_target(target);
                            }
                        }
                    });
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(
                            language.text("選択ウィンドウ（背面でも選択できます）"),
                        )
                        .strong(),
                    );
                    ui.label(
                        egui::RichText::new(
                            language.text("前後に関係なく、選んだウィンドウの内容を撮影します"),
                        )
                        .color(TEXT_MUTED),
                    );
                    if ui
                        .button(language.text("↻  ウィンドウ一覧を更新"))
                        .clicked()
                    {
                        match discover_windows() {
                            Ok(windows) => {
                                self.windows = windows;
                                self.reset_tray_menu();
                            }
                            Err(error) => self.status = capture_failure_message(&error),
                        }
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("capture_picker_windows")
                    .max_height(250.0)
                    .show(ui, |ui| {
                        let windows = self.windows.clone();
                        for window in &windows {
                            let target = CaptureTarget::Window(window.id);
                            let label = format!(
                                "{} — {}\n{} × {} px",
                                window.app_name, window.title, window.width, window.height
                            );
                            if ui
                                .selectable_label(self.capture_target == target, label)
                                .clicked()
                            {
                                self.select_capture_target(target);
                            }
                        }
                        if self.windows.is_empty() {
                            ui.label(
                                language
                                    .text("ウィンドウ候補がありません。一覧を更新してください。"),
                            );
                        }
                    });

                ui.add_space(10.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{}: {}",
                        language.text("選択中"),
                        self.target_caption()
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(primary_button(language.text("この対象を撮影")))
                            .clicked()
                        {
                            capture = true;
                        }
                        if ui.button(language.text("キャンセル")).clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if cancel {
            open = false;
        }
        self.capture_target_picker_open = open;
        if select_region {
            self.capture_target_picker_open = false;
            self.begin_region_selection(ctx);
        } else if capture && !self.capture_in_progress {
            self.capture_target_picker_open = false;
            self.begin_capture(ctx);
        }
    }

    fn show_header(&mut self, ctx: &egui::Context) {
        let recording = self.recording || self.recording_starting;
        egui::TopBottomPanel::top("macshot_header")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(24, 16)),
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
                        (Page::Capture, self.language.page_label(Page::Capture)),
                        (Page::Evidence, self.language.page_label(Page::Evidence)),
                        (
                            Page::Notifications,
                            self.language.page_label(Page::Notifications),
                        ),
                        (Page::Compare, self.language.page_label(Page::Compare)),
                        (Page::Settings, self.language.page_label(Page::Settings)),
                    ] {
                        let selected = self.page == page;
                        let fill = if selected {
                            egui::Color32::from_rgb(239, 234, 247)
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let stroke = if selected {
                            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(210, 201, 226))
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
                            switch_page(&mut self.page, page, &mut self.status);
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
                            self.language.text("■  録画を終了")
                        } else {
                            self.language.text("●  録画")
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
                            self.language.text("撮影中…")
                        } else {
                            self.language.text("撮影してプレビュー")
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
                        ui.label(egui::RichText::new(self.language.text("撮影対象")).size(11.0).color(TEXT_MUTED));
                        ui.add_space(4.0);
                        let target_popup_height = (ui.ctx().content_rect().height() * 0.84).min(900.0);
                        egui::ComboBox::from_id_salt("capture_target")
                            .selected_text(self.target_caption())
                            .width(250.0)
                            .height(target_popup_height)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show_ui(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.window_search_query)
                                        .desired_width(360.0)
                                        .hint_text(self.language.text("アプリ名・ウィンドウ名を検索")),
                                );
                                ui.separator();
                                selectable_capture_target(
                                    ui,
                                    &mut self.capture_target,
                                    CaptureTarget::ActiveWindow,
                                    self.language.text("アクティブウィンドウ"),
                                );
                                selectable_capture_target(
                                    ui,
                                    &mut self.capture_target,
                                    CaptureTarget::PrimaryDisplay,
                                    self.language.text("デスクトップ（メイン画面）"),
                                );
                                selectable_capture_target(
                                    ui,
                                    &mut self.capture_target,
                                    CaptureTarget::AllDisplays,
                                    self.language.text("全画面（全ディスプレイ）"),
                                );
                                ui.separator();
                                for (index, monitor) in self.monitors.iter().enumerate() {
                                    let name = monitor
                                        .name()
                                        .unwrap_or_else(|_| format!("{} {}", self.language.text("Display"), index + 1));
                                    selectable_capture_target(
                                        ui,
                                        &mut self.capture_target,
                                        CaptureTarget::Display(index),
                                        format!("{} — {name}", self.language.text("Full Display")),
                                    );
                                }
                                if !self.windows.is_empty() {
                                    ui.separator();
                                    let query = self.window_search_query.trim().to_lowercase();
                                    let windows: Vec<_> = self
                                        .windows
                                        .iter()
                                        .filter(|window| window_search_matches(window, &query))
                                        .cloned()
                                        .collect();
                                    if windows.is_empty() {
                                        ui.label(self.language.text("一致するウィンドウがありません"));
                                    }
                                    for window in &windows {
                                        show_window_choice(
                                            ui,
                                            &mut self.capture_target,
                                            window,
                                        );
                                    }
                                } else {
                                    ui.label(self.language.text("ウィンドウ候補がありません。一覧を更新してください。"));
                                }
                            });
                    });
                    if ui.add(primary_button(self.language.text("範囲を選択"))).clicked() {
                        self.begin_region_selection(ui.ctx());
                    }
                    if ui
                        .add(primary_button(self.language.text("この対象を撮影")))
                        .clicked()
                    {
                        self.begin_capture(ui.ctx());
                    }
                    if ui.button(self.language.text("ウィンドウを選んで撮影…")).clicked() {
                        #[cfg(target_os = "macos")]
                        self.begin_interactive_window_capture(ui.ctx());
                        #[cfg(target_os = "windows")]
                        self.open_capture_target_picker(ui.ctx());
                    }
                    if ui.button(self.language.text("↻  ウィンドウ一覧を更新")).clicked() {
                        match discover_windows() {
                            Ok(windows) => {
                                self.windows = windows;
                                self.reset_tray_menu();
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
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(self.language.text("ショートカットの動作"))
                            .size(11.0)
                            .color(TEXT_MUTED),
                    );
                    let target_priority = ui.selectable_label(
                        self.capture_shortcut_behavior == CaptureShortcutBehavior::SelectedTarget,
                        self.language.text("撮影対象選択を優先"),
                    );
                    if target_priority.clicked() {
                        self.set_capture_shortcut_behavior(CaptureShortcutBehavior::SelectedTarget);
                    }
                    let mode_priority = ui.selectable_label(
                        self.capture_shortcut_behavior == CaptureShortcutBehavior::ChooseMode,
                        self.language.text("各モード優先"),
                    );
                    if mode_priority.clicked() {
                        self.set_capture_shortcut_behavior(CaptureShortcutBehavior::ChooseMode);
                    }
                    ui.label(
                        egui::RichText::new(self.language.text(match self.capture_shortcut_behavior {
                            CaptureShortcutBehavior::SelectedTarget => "選択中の対象をすぐ撮影します",
                            CaptureShortcutBehavior::ChooseMode => "ショートカット後に撮影方法を選びます",
                        }))
                        .size(10.0)
                        .color(TEXT_MUTED),
                    );
                });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(self.language.text("追加の操作")).size(11.0).color(TEXT_MUTED));
                    let continuous_label = if self.continuous_capture {
                        self.language.text("連続撮影を終了")
                    } else {
                        self.language.text("連続撮影を開始")
                    };
                    if ui.button(continuous_label).clicked() {
                        self.toggle_continuous_capture(ui.ctx());
                    }
                    if cfg!(target_os = "macos")
                        && ui.button(self.language.text("macOS標準の撮影・録画…")).clicked()
                    {
                        self.open_native_screenshot();
                    }
                    ui.separator();
                    for (title, value) in [
                        (self.language.text("撮影"), self.capture_hotkey_text.as_str()),
                        (self.language.text("録画"), self.recording_hotkey_text.as_str()),
                    ] {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 145))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(22)))
                            .corner_radius(egui::CornerRadius::same(10))
                            .inner_margin(egui::Margin::symmetric(10, 7))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(&title).size(10.0).color(TEXT_MUTED));
                                    ui.label(
                                        egui::RichText::new(hotkey_badge_text(value))
                                            .size(11.0)
                                            .strong(),
                                    );
                                });
                            });
                    }
                    if ui.button(self.language.text("▣  保存先を変更")).clicked() {
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
                        egui::RichText::new(self.language.text("画面撮影・録画には画面収録の許可が必要です。"))
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(self.language.text("権限状態は設定画面で確認できます。オンでも撮影が拒否される場合はMacShot.appを登録し直して再起動してください。"))
                            .size(12.0)
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        #[cfg(target_os = "macos")]
                        if ui.button(self.language.text("macOSに画面収録を要求")).clicked() {
                            self.request_screen_recording_access();
                        }
                        if ui.add(primary_button(self.language.text("画面収録設定を開く"))).clicked() {
                            self.open_screen_recording_settings();
                        }
                        #[cfg(target_os = "macos")]
                        if ui.button(self.language.text("FinderでMacShotを表示")).clicked() {
                            self.reveal_macshot_app();
                        }
                        if ui.button(self.language.text("権限状態を更新")).clicked() {
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
                if ui.button(self.language.text("▦  撮影済みを開く")).clicked() {
                    switch_page(&mut self.page, Page::Evidence, &mut self.status);
                    let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                    self.evidence_files = evidence_images(&folder);
                    self.recording_files = recording_files(&folder);
                }
                if self.region_annotation_pending {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(self.language.text("注釈を保存して撮影を完了します。"))
                            .size(11.0)
                            .color(TEXT_MUTED),
                    );
                    if ui
                        .add(primary_button(self.language.text("✓ 撮影完了")))
                        .clicked()
                    {
                        self.finish_region_capture(ctx);
                    }
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
                        .add_enabled(index > 0, egui::Button::new(self.language.text("‹ 前へ")))
                        .clicked()
                    {
                        self.move_in_edit_queue(-1);
                    }
                    let has_next = index + 1 < self.editing_queue.len();
                    if ui
                        .add_enabled(has_next, egui::Button::new(self.language.text("次へ ›")))
                        .clicked()
                    {
                        self.move_in_edit_queue(1);
                    }
                    if has_next
                        && self.has_pending_edit()
                        && ui
                            .add(primary_button(self.language.text("保存して次へ")))
                            .clicked()
                    {
                        self.save_then_next();
                    }
                }
                ui.label(
                    egui::RichText::new(self.language.text("編集ツール"))
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                ui.separator();
                for (tool, label) in [
                    (EditTool::Crop, self.language.text("⌗  切り出し")),
                    (EditTool::Rectangle, self.language.text("□  赤枠")),
                    (EditTool::Ellipse, self.language.text("○  楕円")),
                    (EditTool::Arrow, self.language.text("↗  矢印")),
                    (EditTool::Text, self.language.text("T  テキスト")),
                    (EditTool::Timestamp, self.language.text("◷  撮影時刻")),
                    (EditTool::Mosaic, self.language.text("▦  モザイク")),
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
                if self.edit_tool == EditTool::Text {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.annotation_text)
                            .hint_text(self.language.text("注釈テキスト"))
                            .desired_width(170.0),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.annotations.is_empty() {
                        if ui.button(self.language.text("注釈を消去")).clicked() {
                            self.annotations.clear();
                            self.live_annotation = None;
                        }
                        if ui
                            .button(self.language.text("注釈画像を別名で保存"))
                            .clicked()
                        {
                            self.save_annotated_image();
                        }
                        if ui
                            .add_enabled(
                                self.can_overwrite_current_png(),
                                egui::Button::new(self.language.text("上書き保存")),
                            )
                            .on_hover_text(self.language.text("現在のPNG画像を注釈付きで上書き"))
                            .clicked()
                        {
                            self.overwrite_confirmation = true;
                        }
                    }
                    if self.edit_tool == EditTool::Crop && self.crop_selection.is_some() {
                        if ui.button(self.language.text("選択解除")).clicked() {
                            self.crop_selection = None;
                            self.crop_drag_start = None;
                        }
                        if ui
                            .add(primary_button(self.language.text("切り出しを別名で保存")))
                            .clicked()
                        {
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
                ui.centered_and_justified(|ui| self.show_clickable_capture_preview(ui));
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space((preview_height * 0.2).max(36.0));
                    ui.label(egui::RichText::new("◉").size(64.0).color(ACCENT));
                    ui.add_space(7.0);
                    ui.label(
                        egui::RichText::new(self.language.page_heading(Page::Capture))
                            .size(22.0)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(
                            self.language
                                .text("撮影した画面は、ここで切り出しや注釈を追加できます。"),
                        )
                        .size(13.0)
                        .color(TEXT_MUTED),
                    );
                    ui.add_space(18.0);
                    if ui
                        .add(primary_button(self.language.text("撮影してプレビュー")))
                        .clicked()
                    {
                        self.begin_capture(ctx);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  {}",
                            self.language.text("ショートカット"),
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
        let language = self.language;
        glass_frame().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(self.language.page_heading(Page::Compare))
                            .size(20.0)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(
                            language.text("2枚の画面を並べて、変更点を確認します。"),
                        )
                        .color(TEXT_MUTED),
                    );
                });
                ui.separator();
                for mode in [
                    CompareMode::SideBySide,
                    CompareMode::Overlay,
                    CompareMode::Difference,
                ] {
                    let label = self.language.compare_mode_label(mode);
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
                    egui::RichText::new(format!(
                        "{}  {}",
                        language.text("差分ピクセル"),
                        self.diff_count
                    ))
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
                if ui.button(language.text("＋  基準画像")).clicked() {
                    self.choose_image(true);
                }
                if ui.button(language.text("＋  比較画像")).clicked() {
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
                                    language.text("基準画像")
                                } else {
                                    language.text("比較画像")
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
                                    compare_empty_state(
                                        ui,
                                        &language.text("画像を選択してください"),
                                    );
                                }
                            } else if let Some(image) = &self.right_image {
                                Self::show_image(ui, image, &mut self.right_texture, "right");
                            } else {
                                compare_empty_state(ui, &language.text("画像を選択してください"));
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
                        compare_empty_state(
                            ui,
                            &language.text("比較する2枚の画像を選んでください"),
                        );
                    }
                });
            }
            CompareMode::Difference => {
                glass_frame().show(ui, |ui| {
                    if let Some(image) = &self.diff_image {
                        Self::show_image(ui, image, &mut self.diff_texture, "diff");
                    } else {
                        compare_empty_state(
                            ui,
                            &language.text("比較する2枚の画像を選んでください"),
                        );
                    }
                });
            }
        }
    }

    fn handle_recording_metadata(&mut self) {
        while let Ok(result) = self.recording_metadata_rx.try_recv() {
            self.recording_metadata_pending.remove(&result.path);
            self.recording_duration_cache
                .insert(result.path, result.duration);
        }
    }

    fn request_recording_metadata(&mut self, ctx: &egui::Context) {
        let pending: Vec<_> = self
            .recording_files
            .iter()
            .filter(|path| {
                !self.recording_duration_cache.contains_key(*path)
                    && !self.recording_metadata_pending.contains(*path)
            })
            .cloned()
            .collect();
        if pending.is_empty() {
            return;
        }
        self.recording_metadata_pending
            .extend(pending.iter().cloned());
        let sender = self.recording_metadata_tx.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            for path in pending {
                let _ = sender.send(RecordingMetadataResult {
                    duration: recording_duration(&path),
                    path,
                });
                repaint.request_repaint();
            }
        });
    }

    fn open_recording_player(&mut self, path: &Path) {
        let is_native_mov = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("mov"));
        if is_native_mov && !cfg!(target_os = "macos") {
            self.status = self
                .language
                .text("MOV形式は「外部で開く」から再生できます。");
            return;
        }
        match recording_preview(path) {
            Ok((image, details)) => {
                self.recording_playback = None;
                self.recording_preview_path = Some(path.to_path_buf());
                self.recording_preview_image = Some(image);
                self.recording_preview_texture = None;
                self.recording_preview_details = Some(details);
                self.recording_player_error = None;
                self.recording_playback = Some(start_recording_playback(path.to_path_buf()));
                self.status = self.language.text("録画を新しいウィンドウで再生します。");
            }
            Err(error) => {
                self.status = format!(
                    "{}: {error}",
                    self.language.message("録画を読み込めませんでした")
                );
            }
        }
    }

    fn close_recording_player(&mut self) {
        self.recording_playback = None;
        self.recording_preview_path = None;
        self.recording_preview_image = None;
        self.recording_preview_texture = None;
        self.recording_preview_details = None;
        self.recording_player_controls_rect = None;
        self.recording_player_last_pointer_pos = None;
        self.recording_player_last_pointer_activity = None;
        self.recording_player_error = None;
        self.status.clear();
    }

    fn show_capture_editor_viewport(&mut self, ctx: &egui::Context) {
        if !self.capture_editor_expanded {
            return;
        }
        let Some(image) = self.left_image.as_ref() else {
            self.capture_editor_expanded = false;
            return;
        };
        let (width, height) = image.dimensions();
        let title = format!(
            "{} — {} × {} px",
            self.language.text("画像を拡大して編集"),
            width,
            height
        );
        let viewport_id = egui::ViewportId::from_hash_of("macshot-capture-editor");
        let builder = egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1480.0, 960.0])
            .with_min_inner_size([720.0, 520.0])
            .with_resizable(true);
        let mut close = false;
        ctx.show_viewport_immediate(viewport_id, builder, |editor_ctx, _class| {
            if editor_ctx.input(|input| input.viewport().close_requested()) {
                close = true;
                editor_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            egui::TopBottomPanel::top("capture_editor_header")
                .frame(
                    egui::Frame::new()
                        .fill(egui::Color32::WHITE)
                        .inner_margin(egui::Margin::symmetric(18, 10)),
                )
                .show(editor_ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(&title).size(15.0).strong());
                        if let Some(path) = &self.left_path {
                            ui.label(
                                egui::RichText::new(
                                    path.file_name().unwrap_or_default().to_string_lossy(),
                                )
                                .size(11.0)
                                .color(TEXT_MUTED),
                            );
                        }
                        if ui.button(self.language.text("閉じる")).clicked() {
                            close = true;
                            editor_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(self.language.text("編集ツール"))
                                .size(11.0)
                                .color(TEXT_MUTED),
                        );
                        for (tool, label) in [
                            (EditTool::Crop, self.language.text("⌗  切り出し")),
                            (EditTool::Rectangle, self.language.text("□  赤枠")),
                            (EditTool::Ellipse, self.language.text("○  楕円")),
                            (EditTool::Arrow, self.language.text("↗  矢印")),
                            (EditTool::Text, self.language.text("T  テキスト")),
                            (EditTool::Timestamp, self.language.text("◷  撮影時刻")),
                            (EditTool::Mosaic, self.language.text("▦  モザイク")),
                        ] {
                            if ui.selectable_label(self.edit_tool == tool, label).clicked() {
                                self.edit_tool = tool;
                            }
                        }
                        if self.edit_tool == EditTool::Text {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.annotation_text)
                                    .hint_text(self.language.text("注釈テキスト"))
                                    .desired_width(170.0),
                            );
                        }
                        if self.edit_tool == EditTool::Crop && self.crop_selection.is_some() {
                            if ui.button(self.language.text("選択解除")).clicked() {
                                self.crop_selection = None;
                                self.crop_drag_start = None;
                            }
                            if ui
                                .add(primary_button(self.language.text("切り出しを別名で保存")))
                                .clicked()
                            {
                                self.save_crop();
                            }
                        }
                        if !self.annotations.is_empty() {
                            if ui.button(self.language.text("注釈を消去")).clicked() {
                                self.annotations.clear();
                                self.live_annotation = None;
                            }
                            if ui
                                .add(primary_button(self.language.text("注釈画像を別名で保存")))
                                .clicked()
                            {
                                self.save_annotated_image();
                            }
                            if ui
                                .add_enabled(
                                    self.can_overwrite_current_png(),
                                    egui::Button::new(self.language.text("上書き保存")),
                                )
                                .on_hover_text(
                                    self.language.text("現在のPNG画像を注釈付きで上書き"),
                                )
                                .clicked()
                            {
                                self.overwrite_confirmation = true;
                            }
                        }
                    });
                });
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(249, 248, 251))
                        .inner_margin(egui::Margin::symmetric(16, 14)),
                )
                .show(editor_ctx, |ui| {
                    if self.left_image.is_some() {
                        ui.centered_and_justified(|ui| self.show_croppable_image(ui));
                    }
                });
            self.show_overwrite_confirmation(editor_ctx);
        });
        if close {
            self.capture_editor_expanded = false;
        }
    }

    fn show_recording_player_viewport(&mut self, ctx: &egui::Context) {
        let Some(path) = self.recording_preview_path.clone() else {
            return;
        };
        let filename = recording_display_name(&path);
        let title = format!("{} — {filename}", self.language.text("録画再生"));
        let viewport_id = egui::ViewportId::from_hash_of("macshot-recording-player");
        let builder = egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1120.0, 680.0])
            .with_min_inner_size([720.0, 480.0])
            .with_resizable(true);
        let mut close = false;
        ctx.show_viewport_immediate(viewport_id, builder, |player_ctx, _class| {
            if player_ctx.input(|input| input.viewport().close_requested()) {
                close = true;
                player_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(egui::Color32::BLACK).inner_margin(0.0))
                .show(player_ctx, |ui| {
                    let bounds = ui.max_rect();
                    let _ = ui.allocate_rect(bounds, egui::Sense::hover());
                    let available = bounds.size();
                    let image_size = self.recording_preview_image.as_ref().map(|image| {
                        let image_size = egui::vec2(image.width() as f32, image.height() as f32);
                        let scale = (available.x / image_size.x)
                            .min(available.y / image_size.y)
                            .min(1.0);
                        image_size * scale
                    });
                    let video_rect = image_size.map(|size| {
                        egui::Rect::from_center_size(bounds.center(), size)
                    });
                    if let (Some(image), Some(rect)) =
                        (&self.recording_preview_image, video_rect)
                    {
                        let size = [image.width() as usize, image.height() as usize];
                        if self.recording_preview_texture.is_none() {
                            let rgba = image.to_rgba8();
                            let color = egui::ColorImage::from_rgba_unmultiplied(
                                size,
                                rgba.as_raw(),
                            );
                            self.recording_preview_texture = Some(ui.ctx().load_texture(
                                "recording-player-frame",
                                color,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        if let Some(texture) = &self.recording_preview_texture {
                            ui.painter().image(
                                texture.id(),
                                rect,
                                egui::Rect::from_min_max(
                                    egui::Pos2::ZERO,
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    let pointer = player_ctx.input(|input| input.pointer.hover_pos());
                    let now = Instant::now();
                    if let Some(position) = pointer {
                        if self.recording_player_last_pointer_pos != Some(position) {
                            self.recording_player_last_pointer_activity = Some(now);
                        }
                        self.recording_player_last_pointer_pos = Some(position);
                    } else {
                        self.recording_player_last_pointer_pos = None;
                    }
                    let pointer_down = player_ctx.input(|input| input.pointer.any_down());
                    let controls_visible = recording_player_controls_visible(
                        video_rect,
                        self.recording_player_controls_rect,
                        bounds,
                        pointer,
                        self.recording_player_last_pointer_activity,
                        now,
                        pointer_down,
                    );
                    if controls_visible {
                        let control_width =
                            recording_player_controls_width(video_rect, bounds);
                        let anchor =
                            recording_player_controls_anchor(video_rect, bounds, control_width);
                        let controls_area = egui::Area::new(egui::Id::new("recording-player-controls"))
                            .order(egui::Order::Foreground)
                            .pivot(egui::Align2::LEFT_BOTTOM)
                            .fixed_pos(anchor)
                            .show(player_ctx, |ui| {
                                egui::Frame::new()
                                    .fill(egui::Color32::from_black_alpha(218))
                                    .corner_radius(egui::CornerRadius::same(12))
                                    .inner_margin(egui::Margin::symmetric(18, 12))
                                    .show(ui, |ui| {
                                        ui.set_width(control_width);
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(&filename)
                                                    .size(15.0)
                                                    .strong()
                                                    .color(egui::Color32::WHITE),
                                            );
                                            if let Some(details) = &self.recording_preview_details {
                                                ui.label(
                                                    egui::RichText::new(details)
                                                        .size(11.0)
                                                        .color(egui::Color32::LIGHT_GRAY),
                                                );
                                            }
                                        });
                                        let mut restart_playback = false;
                                        if let Some(playback) = &mut self.recording_playback {
                        let is_gif = path
                            .extension()
                            .and_then(|value| value.to_str())
                            .is_some_and(|value| value.eq_ignore_ascii_case("gif"));
                        let audio_position = playback
                            .audio_player
                            .as_ref()
                            .map(|player| player.get_pos());
                        playback.position = playback_position_for_display(
                            playback.position,
                            playback.playing,
                            audio_position,
                            playback.duration,
                        );
                        let total = playback.duration.as_secs_f64();
                        let mut seconds = playback.position.as_secs_f64().min(total);
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new(self.language.text("音声の有無")).color(egui::Color32::WHITE));
                            ui.label(
                                egui::RichText::new(format!(
                                    "● {}",
                                    self.language.text("音声あり")
                                ))
                                .color(egui::Color32::from_rgb(76, 163, 116)),
                            );
                            ui.label(
                                egui::RichText::new(format!("— {}", self.language.text("無音")))
                                    .color(egui::Color32::from_rgb(155, 160, 170)),
                            );
                                            if let Some(levels) = playback.audio_waveform.as_deref() {
                                                if levels.is_empty() {
                                                    ui.label(egui::RichText::new(self.language.text(if is_gif {
                                                        "GIFには音声がありません"
                                                    } else {
                                                        "音声データなし"
                                                    })).color(egui::Color32::LIGHT_GRAY));
                                                } else {
                                                    show_audio_presence_timeline(
                                                        ui,
                                                        levels,
                                                        playback.duration,
                                                        playback.position,
                                                    );
                                                }
                                            } else if playback.audio_waveform_rx.is_some()
                                                || playback.audio_setup_rx.is_some()
                                            {
                                                ui.label(egui::RichText::new(self.language.text("音声データを解析中…")).color(egui::Color32::LIGHT_GRAY));
                                            } else if playback.audio_waveform_error.is_some() {
                                                ui.label(egui::RichText::new(self.language.text("音声データを表示できません")).color(egui::Color32::LIGHT_GRAY));
                                            } else {
                                                ui.label(egui::RichText::new(self.language.text("音声データなし")).color(egui::Color32::LIGHT_GRAY));
                                            }
                                        });
                                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(self.language.text("音量")).color(egui::Color32::WHITE));
                            if ui
                                .add_sized(
                                    [150.0, 20.0],
                                    egui::Slider::new(&mut playback.volume, 0.0..=1.0)
                                        .step_by(0.01)
                                        .show_value(false),
                                )
                                .changed()
                                && let Some(player) = &playback.audio_player
                            {
                                player.set_volume(if playback.muted {
                                    0.0
                                } else {
                                    playback.volume
                                });
                            }
                            ui.label(egui::RichText::new(format!("{:.0}%", playback.volume * 100.0)).color(egui::Color32::WHITE));
                            if ui
                                .button(self.language.text(if playback.muted {
                                    "ミュート解除"
                                } else {
                                    "ミュート"
                                }))
                                .clicked()
                                && let Some(player) = &playback.audio_player
                            {
                                playback.muted = !playback.muted;
                                player.set_volume(if playback.muted {
                                    0.0
                                } else {
                                    playback.volume
                                });
                            }
                            if let Some(level) = &playback.audio_level {
                                let rms = f32::from_bits(level.load(Ordering::Relaxed)).clamp(0.0, 1.0);
                                let (meter, dbfs) = audio_meter_value(rms);
                                ui.label(egui::RichText::new(self.language.text("音声レベル")).color(egui::Color32::WHITE));
                                let meter_color = if dbfs >= -6.0 {
                                    egui::Color32::from_rgb(220, 75, 75)
                                } else if dbfs >= -18.0 {
                                    egui::Color32::from_rgb(230, 170, 55)
                                } else {
                                    egui::Color32::from_rgb(88, 174, 125)
                                };
                                ui.add(egui::ProgressBar::new(meter).desired_width(120.0).fill(meter_color));
                                ui.label(egui::RichText::new(if rms <= 0.000_001 { "−∞ dBFS".to_owned() } else { format!("{dbfs:.0} dBFS") }).color(egui::Color32::WHITE));
                            } else {
                                ui.label(egui::RichText::new(self.language.text(if is_gif {
                                    "GIFには音声がありません"
                                } else if playback.audio_setup_rx.is_some() {
                                    "MOV音声を準備しています…"
                                } else {
                                    "音声トラックなし、または音声出力を利用できません"
                                })).color(egui::Color32::LIGHT_GRAY));
                            }
                            let playing = playback.playing;
                            let play_label = if playing {
                                self.language.text("⏸ 一時停止")
                            } else {
                                self.language.text("▶ 再生")
                            };
                            if ui.button(play_label).clicked() {
                                if playback.ended {
                                    restart_playback = true;
                                } else {
                                    playback.playing = !playback.playing;
                                    if playback.playing {
                                        playback.frame_due = Instant::now();
                                        if playback
                                            .audio_player
                                            .as_ref()
                                            .is_some_and(rodio::Player::empty)
                                            && let Err(error) = seek_recording_playback_audio(
                                                playback,
                                                playback.position,
                                            )
                                        {
                                            self.status = format!("{} {error}", self.language.text("録画を再生できませんでした。"));
                                        }
                                        if let Some(player) = &playback.audio_player { player.play(); }
                                    } else {
                                        let target = playback.position;
                                        if let Some(player) = &playback.audio_player { player.pause(); }
                                        if let Err(error) = seek_recording_playback_audio(playback, target) {
                                            self.status = format!("{} {error}", self.language.text("録画を再生できませんでした。"));
                                        }
                                        if playback.request_tx.send(RecordingPlaybackCommand::Seek(target)).is_ok() {
                                            playback.awaiting_frame = true;
                                            playback.frame_due = Instant::now();
                                        }
                                    }
                                }
                            }
                            if ui.button(self.language.text("外部で開く")).clicked() {
                                #[cfg(target_os = "macos")]
                                let command = "open";
                                #[cfg(target_os = "windows")]
                                let command = "explorer";
                                let _ = Command::new(command).arg(&path).spawn();
                            }
                                        });
                        // Keep seeking at the bottom edge of the video so it remains the primary control.
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format_playback_time(playback.position)).color(egui::Color32::WHITE));
                            ui.spacing_mut().slider_width = (control_width - 250.0).max(120.0);
                            let response = ui.add(
                                egui::Slider::new(&mut seconds, 0.0..=total.max(0.01))
                                    .show_value(false)
                                    .trailing_fill(true),
                            );
                            ui.label(egui::RichText::new(format_playback_time(playback.duration)).color(egui::Color32::WHITE));
                            if response.changed() { playback.position = Duration::from_secs_f64(seconds.min(total)); }
                            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                                let target = playback.position;
                                if playback.request_tx.send(RecordingPlaybackCommand::Seek(target)).is_ok() {
                                    playback.awaiting_frame = true;
                                    playback.frame_due = Instant::now();
                                }
                                if let Err(error) = seek_recording_playback_audio(playback, target) {
                                    self.status = format!("{} {error}", self.language.text("録画を再生できませんでした。"));
                                }
                                if playback.playing && let Some(player) = &playback.audio_player { player.play(); }
                            }
                        });
                        }
                                        if restart_playback {
                                            self.recording_playback =
                                                Some(start_recording_playback(path.clone()));
                                        }
                                        if let Some(error) = &self.recording_player_error {
                                            ui.label(egui::RichText::new(error).color(egui::Color32::LIGHT_RED));
                                        }
                                    });
                            });
                        self.recording_player_controls_rect = Some(controls_area.response.rect);
                        if !pointer_down {
                            player_ctx.request_repaint_after(Duration::from_millis(100));
                        }
                    } else {
                        self.recording_player_controls_rect = None;
                    }
            });
            if player_ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                close = true;
                player_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        if close {
            self.close_recording_player();
        }
    }

    fn begin_recording_rename(&mut self, path: PathBuf) {
        self.recording_rename_dialog = Some(RecordingRenameDialog {
            name: recording_display_name(&path),
            path,
        });
    }

    fn show_recording_rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.recording_rename_dialog else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new(self.language.text("録画名を編集"))
            .id(egui::Id::new("recording_rename_dialog"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(self.language.text("ファイル名"));
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .desired_width(340.0)
                        .hint_text(self.language.text("録画名を入力してください")),
                );
                ui.horizontal(|ui| {
                    if ui.button(self.language.text("保存")).clicked() {
                        save = true;
                    }
                    if ui.button(self.language.text("キャンセル")).clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel || !open {
            self.recording_rename_dialog = None;
        } else if save {
            let dialog = self.recording_rename_dialog.take().unwrap();
            match rename_recording_file(&dialog.path, &dialog.name) {
                Ok(RecordingRenameOutcome::Unchanged) => {
                    self.status = self.language.text("録画名は変更されていません。");
                }
                Ok(RecordingRenameOutcome::Renamed(path)) => {
                    if self.recording_preview_path.as_ref() == Some(&dialog.path) {
                        self.close_recording_player();
                    }
                    let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                    self.recording_files = recording_files(&folder);
                    self.recording_duration_cache.remove(&dialog.path);
                    self.recording_duration_cache.remove(&path);
                    self.status = self.language.text("録画名を変更しました。");
                }
                Err(error) => {
                    self.status =
                        format!("{}: {error}", self.language.text("録画名を変更できません"));
                }
            }
        }
    }

    fn advance_recording_playback(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut event = None;
        let mut request_failed = false;
        let mut audio_setup = None;
        if let Some(playback) = &mut self.recording_playback {
            let waveform_result = playback.audio_waveform_rx.as_ref().and_then(|receiver| {
                match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("音声データを解析できません。".into()))
                    }
                    Err(mpsc::TryRecvError::Empty) => None,
                }
            });
            if let Some(result) = waveform_result {
                playback.audio_waveform_rx = None;
                match result {
                    Ok(levels) => playback.audio_waveform = Some(levels),
                    Err(error) => playback.audio_waveform_error = Some(error),
                }
            }
            if let Some(receiver) = &playback.audio_setup_rx {
                match receiver.try_recv() {
                    Ok(result) => audio_setup = Some(result),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        audio_setup = Some(Err("MOVの音声準備が中断されました。".into()));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if audio_setup.is_some() {
                playback.audio_setup_rx = None;
            }
            while let Ok(next) = playback.frame_rx.try_recv() {
                event = Some(next);
            }
            if playback.playing && !playback.awaiting_frame && now >= playback.frame_due {
                if playback
                    .request_tx
                    .send(RecordingPlaybackCommand::NextFrame)
                    .is_ok()
                {
                    playback.awaiting_frame = true;
                } else {
                    request_failed = true;
                }
            }
            if playback.playing
                || playback.awaiting_frame
                || playback.audio_setup_rx.is_some()
                || playback.audio_waveform_rx.is_some()
            {
                ctx.request_repaint_after(Duration::from_millis(10));
            }
        }

        if let Some(result) = audio_setup {
            match result {
                Ok(audio_path) => {
                    if let Some(playback) = &mut self.recording_playback {
                        playback.audio_waveform_rx = Some(start_audio_waveform_analysis(
                            audio_path.clone(),
                            playback.duration,
                        ));
                        playback.audio_temp_path = Some(audio_path.clone());
                    }
                    let audio_stream = rodio::DeviceSinkBuilder::open_default_sink();
                    let decoder = std::fs::File::open(&audio_path)
                        .map_err(|error| error.to_string())
                        .and_then(|file| {
                            rodio::Decoder::new_mp4(file).map_err(|error| error.to_string())
                        });
                    match (audio_stream, decoder) {
                        (Ok(stream), Ok(decoder)) => {
                            if let Some(playback) = &mut self.recording_playback {
                                let player = rodio::Player::connect_new(stream.mixer());
                                let level = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
                                player.append(MeteredAudioSource {
                                    source: decoder,
                                    level: level.clone(),
                                    sum_squares: 0.0,
                                    samples: 0,
                                });
                                player.set_volume(if playback.muted {
                                    0.0
                                } else {
                                    playback.volume
                                });
                                let _ = player.try_seek(playback.position);
                                if playback.playing {
                                    player.play();
                                }
                                playback._audio_stream = Some(stream);
                                playback.audio_player = Some(player);
                                playback.audio_level = Some(level);
                            } else {
                                let _ = std::fs::remove_file(audio_path);
                            }
                        }
                        (Err(error), _) => {
                            let error = error.to_string();
                            if let Some(playback) = &mut self.recording_playback {
                                playback.audio_waveform_error = Some(error.clone());
                            } else {
                                let _ = std::fs::remove_file(audio_path);
                            }
                            self.status = format!(
                                "{} {error}",
                                self.language.text("録画を再生できませんでした。")
                            );
                        }
                        (_, Err(error)) => {
                            if let Some(playback) = &mut self.recording_playback {
                                playback.audio_waveform_error = Some(error.clone());
                            } else {
                                let _ = std::fs::remove_file(audio_path);
                            }
                            self.status = format!(
                                "{} {error}",
                                self.language.text("録画を再生できませんでした。")
                            );
                        }
                    }
                }
                Err(error) => {
                    if let Some(playback) = &mut self.recording_playback {
                        playback.audio_waveform_error = Some(error.clone());
                    }
                    self.status = format!(
                        "{} {error}",
                        self.language.text("録画を再生できませんでした。")
                    );
                }
            }
        }

        let mut finish = false;
        if let Some(event) = event {
            if let Some(playback) = &mut self.recording_playback {
                playback.awaiting_frame = false;
            }
            match event {
                RecordingPlaybackEvent::Frame(image, delay, position, is_seek) => {
                    self.recording_preview_image = Some(image);
                    self.recording_preview_texture = None;
                    if let Some(playback) = &mut self.recording_playback {
                        if should_update_playback_position(playback.playing, is_seek) {
                            playback.position = position;
                        }
                        playback.frame_due = Instant::now() + delay.max(Duration::from_millis(10));
                    }
                }
                RecordingPlaybackEvent::End => {
                    self.status = self
                        .language
                        .text("録画の再生が終わりました。もう一度再生できます。");
                    if let Some(playback) = &mut self.recording_playback {
                        playback.playing = false;
                        playback.ended = true;
                        playback.position = playback.duration;
                        if let Some(player) = &playback.audio_player {
                            player.pause();
                        }
                    }
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
        let language = self.language;
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        self.request_recording_metadata(ui.ctx());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(self.language.page_heading(Page::Evidence))
                        .size(24.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(
                        language.text(
                            "録画は別ウィンドウで再生・名前変更、画像は編集して保存できます。",
                        ),
                    )
                    .color(TEXT_MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(language.text("▣  保存先を開く")).clicked() {
                    #[cfg(target_os = "macos")]
                    let command = "open";
                    #[cfg(target_os = "windows")]
                    let command = "explorer";
                    let _ = std::process::Command::new(command).arg(&folder).spawn();
                }
                if ui.button(language.text("↻  更新")).clicked() {
                    self.evidence_files = evidence_images(&folder);
                    self.recording_files = recording_files(&folder);
                    self.status = language.text("証跡一覧を更新しました");
                }
            });
        });
        ui.add_space(14.0);
        glass_frame().show(ui, |ui| {
            ui.label(egui::RichText::new(folder.display().to_string()).size(11.0).color(TEXT_MUTED));
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(language.text("録画一覧")).size(19.0).strong());
                ui.label(egui::RichText::new(format!("{} {}", self.recording_files.len(), language.text("録画件数（GIF / MP4 / MOV）"))).color(TEXT_MUTED));
            });
            ui.label(egui::RichText::new(language.text("GIF・MP4・対応MOVを別ウィンドウで再生できます。対応外の形式は外部プレイヤーを使います。録画名も変更できます。")).size(11.0).color(TEXT_MUTED));
            ui.add_space(8.0);
            if self.recording_files.is_empty() {
                ui.label(egui::RichText::new(language.text("録画が見つかりません。保存先を確認するか「更新」を押してください。")).color(TEXT_MUTED));
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("evidence_recording_list")
                    .max_height(220.0)
                    .show(ui, |ui| {
                    let files = self.recording_files.clone();
                    for path in files {
                        let selected = self.recording_preview_path.as_ref() == Some(&path);
                        let name = recording_display_name(&path);
                        let recording_summary = std::fs::metadata(&path)
                            .map(|metadata| {
                                let format = path
                                    .extension()
                                    .and_then(|extension| extension.to_str())
                                    .unwrap_or_default()
                                    .to_ascii_uppercase();
                                let duration = if self.recording_metadata_pending.contains(&path) {
                                    language.text("時間を確認中…")
                                } else {
                                    self.recording_duration_cache
                                        .get(&path)
                                        .copied()
                                        .flatten()
                                        .map(format_recording_duration)
                                        .unwrap_or_else(|| language.text("時間不明"))
                                };
                                format!("{format} · {duration} · {}", format_file_size(metadata.len()))
                            })
                            .unwrap_or_else(|_| language.text("録画情報を読み込めません"));
                        let is_native_mov = path
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("mov"));
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 135))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(18)))
                            .corner_radius(egui::CornerRadius::same(12))
                            .inner_margin(egui::Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        if ui.selectable_label(selected, format!("▧  {name}")).clicked() {
                                            self.open_recording_player(&path);
                                        }
                                        ui.label(
                                            egui::RichText::new(recording_summary)
                                                .size(10.0)
                                                .color(TEXT_MUTED),
                                        );
                                    });
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button(language.text("編集")).clicked() {
                                            self.begin_recording_rename(path.clone());
                                        }
                                        if ui
                                            .add_enabled(
                                                !is_native_mov || cfg!(target_os = "macos"),
                                                egui::Button::new(language.text("再生")),
                                            )
                                            .clicked()
                                        {
                                            self.open_recording_player(&path);
                                        }
                                        if ui.small_button(language.text("外部で開く")).clicked() {
                                            #[cfg(target_os = "macos")]
                                            let command = "open";
                                            #[cfg(target_os = "windows")]
                                            let command = "explorer";
                                            let _ = std::process::Command::new(command).arg(&path).spawn();
                                        }
                                    });
                                });
                            });
                        ui.add_space(5.0);
                    }
                });
            }
            ui.add_space(16.0);
            ui.separator();
            if self.evidence_files.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(34.0);
                    ui.label(egui::RichText::new("▦").size(42.0).color(ACCENT_BRIGHT));
                    ui.label(egui::RichText::new(language.text("まだ画像がありません")).size(17.0).strong());
                    ui.label(egui::RichText::new(language.text("撮影した画像はここに並びます。保存先を変更した場合は設定を確認してください。")).color(TEXT_MUTED));
                    ui.add_space(24.0);
                });
            } else {
                ui.label(language.message(&format!("{} 枚の画像", self.evidence_files.len())));
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .id_salt("evidence_image_list")
                    .max_height(570.0)
                    .show(ui, |ui| {
                    let files = self.evidence_files.clone();
                    for (index, path) in files.into_iter().enumerate() {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        let modified = std::fs::metadata(&path)
                            .and_then(|metadata| metadata.modified())
                            .ok()
                            .map(chrono::DateTime::<chrono::Local>::from)
                            .map(|date| date.format("%Y-%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| language.text("日時不明"));
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
                                        if ui.add(primary_button(language.text("画像を編集"))).clicked() {
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

    fn show_notifications_page(&mut self, ui: &mut egui::Ui) {
        let language = self.language;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(language.page_heading(Page::Notifications))
                        .size(24.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(
                        language.text("通知履歴はこのMacShot内に最大200件保存されます。"),
                    )
                    .color(TEXT_MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        !self.notifications.is_empty(),
                        egui::Button::new(language.text("履歴を消去")),
                    )
                    .clicked()
                {
                    self.clear_notification_history();
                }
            });
        });
        ui.add_space(14.0);
        if self.notifications.is_empty() {
            glass_frame().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(38.0);
                    ui.label(egui::RichText::new("♧").size(40.0).color(ACCENT_BRIGHT));
                    ui.label(
                        egui::RichText::new(language.text("通知履歴"))
                            .size(18.0)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(
                            language.text("ここに撮影・録画の完了通知が保存されます。"),
                        )
                        .color(TEXT_MUTED),
                    );
                    ui.add_space(38.0);
                });
            });
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("notification_history")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let notifications = self.notifications.clone();
                for notification in notifications {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 150))
                        .corner_radius(egui::CornerRadius::same(13))
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    let title = match notification.kind {
                                        NotificationKind::Capture(sequence) => format!(
                                            "{}  ·  {} {}",
                                            language.text("撮影しました"),
                                            language.text("撮影"),
                                            sequence
                                        ),
                                        NotificationKind::Recording => {
                                            language.text("録画を保存しました")
                                        }
                                    };
                                    ui.label(
                                        egui::RichText::new(title).strong().color(ACCENT_BRIGHT),
                                    );
                                    ui.label(
                                        egui::RichText::new(&notification.timestamp)
                                            .color(TEXT_MUTED),
                                    );
                                    if let (Some(width), Some(height)) =
                                        (notification.width, notification.height)
                                    {
                                        ui.label(
                                            language.message(&capture_size_label(width, height)),
                                        );
                                    }
                                    ui.label(
                                        egui::RichText::new(
                                            notification.path.display().to_string(),
                                        )
                                        .size(12.0),
                                    );
                                    if let Some(detail) = notification.detail.as_deref() {
                                        ui.label(
                                            egui::RichText::new(language.message(detail))
                                                .size(11.0)
                                                .color(TEXT_MUTED),
                                        );
                                    }
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button(language.text("Finderで表示")).clicked()
                                        {
                                            self.reveal_notification_file(&notification.path);
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(7.0);
                }
            });
    }

    fn show_settings_page(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(self.language.page_heading(Page::Settings))
                .size(24.0)
                .strong(),
        );
        let previous_language = self.language;
        let mut refresh_language = false;
        ui.horizontal(|ui| {
            ui.label(self.language.language_heading());
            refresh_language = ui
                .button("↻")
                .on_hover_text(self.language.text("言語表示を再読み込み"))
                .clicked();
            egui::ComboBox::from_id_salt("app_language")
                .selected_text(self.language.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.language, AppLanguage::Japanese, "日本語");
                    ui.selectable_value(&mut self.language, AppLanguage::English, "English");
                    ui.selectable_value(
                        &mut self.language,
                        AppLanguage::SimplifiedChinese,
                        "简体中文",
                    );
                    ui.selectable_value(
                        &mut self.language,
                        AppLanguage::TraditionalChinese,
                        "繁體中文",
                    );
                });
        });
        if refresh_language {
            self.language = AppLanguage::from_setting(&load_setting("language", "ja"));
            self.settings_error = None;
            self.status = "言語表示を再読み込みしました。".into();
            self._tray_icon = None;
            self.tray_initialized = false;
            self.tray_capture_target_item = None;
            self.tray_capture_target_text = None;
            self.tray_target_selection_items.clear();
            self.tray_capture_active_item = None;
            self.tray_capture_primary_item = None;
            self.tray_capture_all_item = None;
            self.tray_capture_current_target_item = None;
            self.tray_capture_shortcut_target_item = None;
            self.tray_capture_shortcut_mode_item = None;
            self.tray_capture_status_item = None;
            self.tray_window_selection_item = None;
            self.tray_continuous_capture_item = None;
            self.tray_recording_status_item = None;
            self.tray_recording_action_item = None;
            self.tray_recording_state = None;
            self.tray_recording_seconds = None;
            self.tray_capture_status_state = None;
        }
        if self.language != previous_language {
            if let Err(error) = self.save_preferences() {
                self.language = previous_language;
                self.settings_error = Some(format!("言語設定を保存できません: {error}"));
            } else {
                self.settings_error = None;
                self.status = "言語設定を保存しました。".into();
                self._tray_icon = None;
                self.tray_initialized = false;
                self.tray_capture_target_item = None;
                self.tray_capture_target_text = None;
                self.tray_target_selection_items.clear();
                self.tray_capture_active_item = None;
                self.tray_capture_primary_item = None;
                self.tray_capture_all_item = None;
                self.tray_capture_current_target_item = None;
                self.tray_capture_shortcut_target_item = None;
                self.tray_capture_shortcut_mode_item = None;
                self.tray_capture_status_item = None;
                self.tray_window_selection_item = None;
                self.tray_continuous_capture_item = None;
                self.tray_recording_status_item = None;
                self.tray_recording_action_item = None;
                self.tray_recording_state = None;
                self.tray_recording_seconds = None;
                self.tray_capture_status_state = None;
            }
        }
        if self.language != previous_language {
            ui.ctx().set_fonts(app_font_definitions(self.language));
        }
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(self.language.text("MacShotを自分の撮影手順に合わせます。"))
                    .color(TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.editing_settings_layout {
                    if ui.button(self.language.text("キャンセル")).clicked() {
                        self.settings_layout = self.saved_settings_layout.clone();
                        self.editing_settings_layout = false;
                    }
                    if ui
                        .add(primary_button(self.language.text("保存して固定")))
                        .clicked()
                    {
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
                } else if ui.button(self.language.text("カード配置を調整")).clicked() {
                    self.saved_settings_layout = self.settings_layout.clone();
                    self.editing_settings_layout = true;
                }
            });
        });
        if let Some(error) = &self.settings_error {
            ui.colored_label(
                egui::Color32::from_rgb(188, 51, 73),
                self.language.message(error),
            );
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
                    egui::RichText::new(format!(
                        "{} · {}",
                        self.language.text(id.label()),
                        self.language.text("配置")
                    ))
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
                if ui
                    .add_enabled(index > 0, egui::Button::new(self.language.text("↑ 上へ")))
                    .clicked()
                {
                    self.settings_layout.swap(index, index - 1);
                }
                if ui
                    .add_enabled(
                        index + 1 < self.settings_layout.len(),
                        egui::Button::new(self.language.text("↓ 下へ")),
                    )
                    .clicked()
                {
                    self.settings_layout.swap(index, index + 1);
                }
                if let Some(card) = self.settings_layout.iter_mut().find(|card| card.id == id) {
                    let width_label = match card.width {
                        SettingsCardWidth::Half => self.language.text("幅: 半分"),
                        SettingsCardWidth::Full => self.language.text("幅: 全体"),
                    };
                    if ui.button(width_label).clicked() {
                        card.width = match card.width {
                            SettingsCardWidth::Half => SettingsCardWidth::Full,
                            SettingsCardWidth::Full => SettingsCardWidth::Half,
                        };
                    }
                    let padding_label = match card.padding {
                        SettingsCardPadding::Compact => self.language.text("余白: 小"),
                        SettingsCardPadding::Standard => self.language.text("余白: 中"),
                        SettingsCardPadding::Spacious => self.language.text("余白: 大"),
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
            ui.label(
                egui::RichText::new(self.language.text("表示"))
                    .size(18.0)
                    .strong(),
            );
            ui.label(
                egui::RichText::new(self.language.text("文字サイズと画面の密度")).color(TEXT_MUTED),
            );
        });
        ui.add_space(8.0);
        let display_context = ui.ctx().clone();
        ui.columns(2, |columns| {
            columns[0].horizontal(|ui| {
                ui.label(self.language.text("表示量"));
                let previous = self.display_density;
                egui::ComboBox::from_id_salt("display_density")
                    .selected_text(self.language.text(self.display_density.label()))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.display_density,
                            DisplayDensity::Compact,
                            self.language.text(DisplayDensity::Compact.label()),
                        );
                        ui.selectable_value(
                            &mut self.display_density,
                            DisplayDensity::Standard,
                            self.language.text(DisplayDensity::Standard.label()),
                        );
                        ui.selectable_value(
                            &mut self.display_density,
                            DisplayDensity::Relaxed,
                            self.language.text(DisplayDensity::Relaxed.label()),
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
                    ui.label(self.language.text("文字サイズ"));
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
            egui::RichText::new(self.language.text(
                "コンパクトにすると一画面に多く表示し、文字サイズを上げると全体を拡大します。",
            ))
            .size(11.0)
            .color(TEXT_MUTED),
        );
    }

    fn show_shortcut_settings_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(self.language.text("ショートカット"))
                .size(18.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new(
                self.language
                    .text("どのアプリを使っていても呼び出せます。設定後すぐに反映されます."),
            )
            .color(TEXT_MUTED),
        );
        ui.add_space(12.0);
        egui::Grid::new("shortcut_settings")
            .num_columns(2)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label(self.language.text("スクリーンショット"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.capture_hotkey_text)
                        .desired_width(220.0)
                        .hint_text("Alt+Shift+KeyS"),
                );
                ui.end_row();
                ui.label(self.language.text("録画の開始／終了"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.recording_hotkey_text)
                        .desired_width(220.0)
                        .hint_text("Alt+Shift+KeyR"),
                );
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                self.language
                    .text("例: Alt+Shift+KeyS、Control+Alt+KeyF、Super+Shift+Key4"),
            )
            .size(11.0)
            .color(TEXT_MUTED),
        );
        ui.label(
            egui::RichText::new(
                self.language
                    .text("macOSのAltはOptionキーです。WindowsではAltキーとして動作します。"),
            )
            .size(11.0)
            .color(TEXT_MUTED),
        );
        ui.add_space(10.0);
        if ui
            .add(primary_button(self.language.text("設定を保存して反映")))
            .clicked()
        {
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
        ui.label(egui::RichText::new(self.language.text("予想ファイルサイズ（1分録画）")).strong());
        match self.recording_format {
            RecordingFormat::Mp4 => {
                ui.label(format!(
                    "{} {}",
                    self.language.text("約"),
                    format_estimated_size(estimate.high_bytes)
                ));
                ui.label(
                    egui::RichText::new(
                        self.language
                            .text("映像ビットレート・AAC音声・コンテナ分を含む目安です。"),
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
            }
            RecordingFormat::Gif => {
                ui.label(format!(
                    "{} {}–{}",
                    self.language.text("約"),
                    format_estimated_size(estimate.low_bytes),
                    format_estimated_size(estimate.high_bytes)
                ));
                ui.label(
                    egui::RichText::new(
                        self.language
                            .text("画面の動きや色で大きく変わります。解像度とfpsからの概算です。"),
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
            }
        }
        let dimensions_note = if approximate {
            format!(
                "{}×{} px ({})",
                width,
                height,
                self.language.text("メイン画面を仮定")
            )
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
        ui.label(
            egui::RichText::new(self.language.text("録画"))
                .size(18.0)
                .strong(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(self.language.text("保存形式"));
            let previous = self.recording_format;
            egui::ComboBox::from_id_salt("recording_format")
                .selected_text(self.language.text(self.recording_format.label()))
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
            ui.label(self.language.text("録画解像度"));
            let previous = self.recording_quality.resolution;
            egui::ComboBox::from_id_salt("recording_resolution")
                .selected_text(
                    self.language
                        .text(self.recording_quality.resolution.label()),
                )
                .show_ui(ui, |ui| {
                    for resolution in RecordingResolution::ALL {
                        ui.selectable_value(
                            &mut self.recording_quality.resolution,
                            resolution,
                            self.language.text(resolution.label()),
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
            egui::RichText::new(
                self.language
                    .text("選んだ高さを上限にして縮小します。元の画面より大きくはしません。"),
            )
            .size(11.0)
            .color(TEXT_MUTED),
        );
        match self.recording_format {
            RecordingFormat::Gif => {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(self.language.text("GIFの画質")).strong());
                ui.label(
                    egui::RichText::new(
                        self.language
                            .text("色の再現を上げるほど、変換に時間がかかることがあります。"),
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
                let quality = ui.add(
                    egui::Slider::new(&mut self.recording_quality.gif_quality_percent, 10..=100)
                        .suffix("%")
                        .text(self.language.text("色の再現")),
                );
                self.save_settings_after_slider_change(&quality, ui, "録画設定を保存しました。");
                let frame_rate = ui.add(
                    egui::Slider::new(&mut self.recording_quality.gif_frame_rate, 1..=30)
                        .suffix(" fps")
                        .text(self.language.text("なめらかさ")),
                );
                self.save_settings_after_slider_change(&frame_rate, ui, "録画設定を保存しました。");
                ui.label(
                    egui::RichText::new(self.language.text("GIFは音声なしです。"))
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
                        .text(self.language.text("画質・容量")),
                );
                self.save_settings_after_slider_change(&bitrate, ui, "録画設定を保存しました。");
                let frame_rate = ui.add(
                    egui::Slider::new(&mut self.recording_quality.mp4_frame_rate, 5..=60)
                        .suffix(" fps")
                        .text(self.language.text("なめらかさ")),
                );
                self.save_settings_after_slider_change(&frame_rate, ui, "録画設定を保存しました。");
                ui.label(
                    egui::RichText::new(
                        self.language
                            .text("システム音声を録音し、マイク許可時はマイク音声も追加します。"),
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
                self.show_recording_size_estimate(ui);
            }
        }
        if cfg!(target_os = "macos") {
            ui.add_space(8.0);
            if ui
                .button(self.language.text("macOS標準の撮影・録画を開く"))
                .clicked()
            {
                self.open_native_screenshot();
            }
        }
    }

    fn show_capture_settings_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(self.language.text("撮影の動作"))
                .size(18.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new(self.language.text(
                "撮影対象や画像に記録する情報を調整できます。設定は次回起動後も引き継がれます。",
            ))
            .color(TEXT_MUTED),
        );
        ui.add_space(12.0);
        ui.columns(2, |columns| {
            glass_frame().show(&mut columns[0], |ui| {
                ui.label(egui::RichText::new(self.language.text("撮影とファイル名")).size(16.0).strong());
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(self.language.text("既定の撮影対象"))
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                let target_popup_height = (ui.ctx().content_rect().height() * 0.84).min(900.0);
                egui::ComboBox::from_id_salt("settings_capture_target")
                    .selected_text(self.target_caption())
                    .width(260.0)
                    .height(target_popup_height)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show_ui(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.window_search_query)
                                .desired_width(360.0)
                                .hint_text(self.language.text("アプリ名・ウィンドウ名を検索")),
                        );
                        ui.separator();
                        selectable_capture_target(
                            ui,
                            &mut self.capture_target,
                            CaptureTarget::ActiveWindow,
                            self.language.text("アクティブウィンドウ"),
                        );
                        selectable_capture_target(
                            ui,
                            &mut self.capture_target,
                            CaptureTarget::PrimaryDisplay,
                            self.language.text("デスクトップ（メイン画面）"),
                        );
                        selectable_capture_target(
                            ui,
                            &mut self.capture_target,
                            CaptureTarget::AllDisplays,
                            self.language.text("全画面（全ディスプレイ）"),
                        );
                        for (index, monitor) in self.monitors.iter().enumerate() {
                            let name = monitor
                                .name()
                                .unwrap_or_else(|_| format!("{} {}", self.language.text("Display"), index + 1));
                            selectable_capture_target(
                                ui,
                                &mut self.capture_target,
                                CaptureTarget::Display(index),
                                format!("{} — {name}", self.language.text("Full Display")),
                            );
                        }
                        let query = self.window_search_query.trim().to_lowercase();
                        let windows: Vec<_> = self
                            .windows
                            .iter()
                            .filter(|window| window_search_matches(window, &query))
                            .cloned()
                            .collect();
                        if windows.is_empty() {
                            let empty_message = if query.is_empty() {
                                "ウィンドウ候補がありません。一覧を更新してください。"
                            } else {
                                "一致するウィンドウがありません"
                            };
                            ui.label(self.language.text(empty_message));
                        }
                        for window in &windows {
                            show_window_choice(
                                ui,
                                &mut self.capture_target,
                                window,
                            );
                        }
                    });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(self.language.text("ファイル名の先頭"))
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                ui.add(egui::TextEdit::singleline(&mut self.filename_prefix).desired_width(240.0));
                ui.add_space(8.0);
                ui.checkbox(&mut self.timestamp_enabled, self.language.text("画像に撮影日時を表示"));
                ui.add_space(6.0);
                ui.label(egui::RichText::new(self.language.text("タイムスタンプ形式")).size(11.0).color(TEXT_MUTED));
                egui::ComboBox::from_id_salt("timestamp_format_setting")
                    .selected_text(match self.timestamp_format.as_str() {
                        "date" => self.language.text("日付のみ"),
                        "time" => self.language.text("時刻のみ"),
                        "datetime_millis" => self.language.text("日時（ミリ秒）"),
                        "japanese" => self.language.text("日本語日時"),
                        _ => self.language.text("日時"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.timestamp_format, "datetime".into(), self.language.text("日時"));
                        ui.selectable_value(&mut self.timestamp_format, "date".into(), self.language.text("日付のみ"));
                        ui.selectable_value(&mut self.timestamp_format, "time".into(), self.language.text("時刻のみ"));
                        ui.selectable_value(&mut self.timestamp_format, "datetime_millis".into(), self.language.text("日時（ミリ秒）"));
                        ui.selectable_value(&mut self.timestamp_format, "japanese".into(), self.language.text("日本語日時"));
                    });
                ui.label(egui::RichText::new(self.language.text("表示位置")).size(11.0).color(TEXT_MUTED));
                egui::ComboBox::from_id_salt("timestamp_position_setting")
                    .selected_text(match self.timestamp_position.as_str() {
                        "top_left" => self.language.text("左上"),
                        "top_right" => self.language.text("右上"),
                        "bottom_left" => self.language.text("左下"),
                        _ => self.language.text("右下"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.timestamp_position, "top_left".into(), self.language.text("左上"));
                        ui.selectable_value(&mut self.timestamp_position, "top_right".into(), self.language.text("右上"));
                        ui.selectable_value(&mut self.timestamp_position, "bottom_left".into(), self.language.text("左下"));
                        ui.selectable_value(&mut self.timestamp_position, "bottom_right".into(), self.language.text("右下"));
                    });
                ui.label(egui::RichText::new(self.language.text("タイムスタンプ背景")).size(11.0).color(TEXT_MUTED));
                egui::ComboBox::from_id_salt("timestamp_background_setting")
                    .selected_text(match self.timestamp_background.as_str() {
                        "none" => self.language.text("背景なし"),
                        "dark" => self.language.text("濃い背景"),
                        _ => self.language.text("薄い背景"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.timestamp_background, "none".into(), self.language.text("背景なし"));
                        ui.selectable_value(&mut self.timestamp_background, "subtle".into(), self.language.text("薄い背景"));
                        ui.selectable_value(&mut self.timestamp_background, "dark".into(), self.language.text("濃い背景"));
                    });
                ui.checkbox(&mut self.show_capture_notice, self.language.text("撮影後に完了小窓を表示"));
                let keep_region_changed = ui.checkbox(
                    &mut self.keep_region_in_continuous,
                    self.language.text("連続撮影で範囲を維持する"),
                );
                if keep_region_changed.changed() {
                    if !self.keep_region_in_continuous {
                        self.continuous_region = None;
                    }
                    match self.save_preferences() {
                        Ok(()) => self.status = "撮影設定を保存しました。".into(),
                        Err(error) => self.settings_error = Some(format!("撮影設定を保存できません: {error}")),
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut self.auto_import_native_captures,
                        self.language.text("macOS標準パネルの撮影結果を自動で取り込む"),
                    );
                    ui.label(
                        egui::RichText::new(
                            self.language.text("オンにすると、MacShotから標準パネルを開いた後に保存された画像・録画を探し、保存先へコピーします。対象はデスクトップ、書類、ダウンロード、ピクチャ、ムービーです。"),
                        )
                        .size(10.0)
                        .color(TEXT_MUTED),
                    );
                }
            });
            glass_frame().show(&mut columns[1], |ui| {
                ui.label(egui::RichText::new(self.language.text("注釈")).size(16.0).strong());
                ui.add_space(8.0);
                ui.label(egui::RichText::new(self.language.text("色")).size(11.0).color(TEXT_MUTED));
                egui::ComboBox::from_id_salt("annotation_color_setting")
                    .selected_text(match self.annotation_color.as_str() {
                        "yellow" => self.language.text("黄色"),
                        "blue" => self.language.text("青色"),
                        _ => self.language.text("赤色"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.annotation_color, "red".into(), self.language.text("赤色"));
                        ui.selectable_value(&mut self.annotation_color, "yellow".into(), self.language.text("黄色"));
                        ui.selectable_value(&mut self.annotation_color, "blue".into(), self.language.text("青色"));
                    });
                ui.add_space(10.0);
                ui.add(egui::Slider::new(&mut self.annotation_width, 1.0..=8.0).text(self.language.text("線の太さ")));
                ui.add_space(18.0);
                let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                ui.label(egui::RichText::new(self.language.text("保存先")).size(11.0).color(TEXT_MUTED));
                ui.label(
                    egui::RichText::new(folder.display().to_string())
                        .size(12.0)
                        .monospace(),
                );
                ui.add_space(6.0);
                if ui.button(self.language.text("保存先を変更")).clicked() {
                    self.choose_folder();
                }
            });
        });
        ui.add_space(10.0);
        if ui
            .add(primary_button(self.language.text("撮影設定を保存")))
            .clicked()
        {
            match self.save_preferences() {
                Ok(()) => self.status = "撮影設定を保存しました。".into(),
                Err(error) => self.settings_error = Some(format!("設定を保存できません: {error}")),
            }
        }
    }

    fn show_permission_panel(&mut self, ui: &mut egui::Ui, padding: SettingsCardPadding) {
        let language = self.language;
        settings_card_frame(padding).show(ui, |ui| {
            if self.editing_settings_layout {
                self.show_settings_card_controls(ui, SettingsCardId::Permissions);
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(language.text("権限とデバイス")).size(18.0).strong());
                if ui.button(language.text("↻  権限状態を更新")).clicked() {
                    self.refresh_permission_status();
                }
            });
            ui.add_space(6.0);

            #[cfg(target_os = "macos")]
            if let Some(app_path) = current_macos_app_bundle_path() {
                let running_copy = format!(
                        "{}: v{} · {}",
                        language.text("起動中のMacShot"),
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
                        ui.label(language.text("画面収録・システム音声"));
                        if screen_granted {
                            ui.colored_label(egui::Color32::from_rgb(38, 132, 88), language.text("許可済み"));
                        } else {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 105, 32),
                                language.text("未許可、またはmacOSへの反映待ち"),
                            );
                        }
                    });
                    #[cfg(target_os = "macos")]
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(language.text("macOSに画面収録を要求")).clicked() {
                            self.request_screen_recording_access();
                        }
                        if ui.button(language.text("画面・音声の設定を開く")).clicked() {
                            self.open_screen_recording_settings();
                        }
                        if ui.button(language.text("FinderでMacShotを表示")).clicked() {
                            self.reveal_macshot_app();
                        }
                    });
                    #[cfg(target_os = "windows")]
                    ui.label(egui::RichText::new(language.text("Windowsでは個別の画面収録許可は不要です")).color(TEXT_MUTED));
                });
                columns[1].vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(language.text("マイク"));
                        #[cfg(target_os = "macos")]
                        {
                            let permitted = microphone_state == MicrophonePermission::Authorized;
                            ui.colored_label(
                                if permitted {
                                    egui::Color32::from_rgb(38, 132, 88)
                                } else {
                                    egui::Color32::from_rgb(190, 105, 32)
                                },
                                language.text(&microphone),
                            );
                        }
                        #[cfg(target_os = "windows")]
                        ui.label(egui::RichText::new(language.text(&microphone)).color(TEXT_MUTED));
                        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                        ui.label(egui::RichText::new(language.text(&microphone)).color(TEXT_MUTED));
                        #[cfg(target_os = "macos")]
                        if let Some(label) = microphone_permission_action_label(microphone_state)
                            && ui.button(language.text(label)).clicked()
                        {
                            if microphone_state == MicrophonePermission::Denied {
                                self.open_microphone_settings();
                            } else {
                                self.request_microphone_permission(&ctx);
                            }
                        }
                        #[cfg(target_os = "macos")]
                        if microphone_state == MicrophonePermission::Denied
                            && ui.button(language.text("拒否記録を解除")).clicked()
                        {
                            self.microphone_reset_confirmation = true;
                        }
                        if ui.button(language.text("マイク設定を開く")).clicked() {
                            self.open_microphone_settings();
                        }
                    });

                    #[cfg(target_os = "macos")]
                    ui.horizontal_wrapped(|ui| {
                        let testing = self.microphone_test_rx.is_some();
                        if ui
                            .add_enabled(
                                microphone_state == MicrophonePermission::Authorized && !testing,
                                egui::Button::new(if testing { language.text("マイクテスト中…") } else { language.text("マイク入力をテスト（5秒）") }),
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
                            ui.label(egui::RichText::new(language.message(message)).color(TEXT_MUTED));
                        }
                    });
                });
            });

            #[cfg(target_os = "macos")]
            ui.label(
                egui::RichText::new(language.text("画面収録の一覧にMacShotがない場合は「＋」から追加できます。マイク一覧には追加ボタンがありません。MacShotから許可を要求すると一覧に表示されます。拒否済みで一覧にもない場合は「拒否記録を解除」を使ってください。"))
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
            #[cfg(target_os = "windows")]
            ui.label(
                egui::RichText::new(language.text("MP4は既定の再生デバイスとマイクを使います。Windowsのマイク設定でデスクトップアプリへのアクセスも確認してください。"))
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
        });

        #[cfg(target_os = "macos")]
        if self.microphone_reset_confirmation {
            let mut open = true;
            let mut confirmed = false;
            let mut cancelled = false;
            egui::Window::new(language.text("マイクの拒否記録を解除"))
                .id(egui::Id::new("microphone_permission_reset_confirmation"))
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.label(language.text("MacShotだけのマイク拒否記録を解除して、macOSに許可を確認します。解除してもマイクは許可されません。確認画面が表示された場合は、そこで許可を選んでください。"));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(primary_button(language.text("解除して許可を確認"))).clicked() {
                            confirmed = true;
                        }
                        if ui.button(language.text("キャンセル")).clicked() {
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
        [1.0, 1.0, 1.0, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        remove_macshot_titlebar_separator();
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
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
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
        self.update_tray_capture_target();
        self.handle_hotkey_requests(ctx);
        self.handle_tray_actions(ctx);
        self.handle_capture_results(ctx);
        self.handle_region_snapshot(ctx);
        self.handle_recording_events(ctx);
        self.handle_recording_metadata();
        #[cfg(target_os = "macos")]
        self.poll_native_capture_imports(ctx);
        #[cfg(target_os = "macos")]
        set_standard_quit_items_enabled(!self.recording && !self.recording_starting, self.language);
        self.update_tray_recording_status();
        if self.recording || self.recording_starting {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.region_selecting {
            self.show_region_overlay(ctx);
            return;
        }

        self.show_header(ctx);

        let status_message = self.language.message(&self.status);
        egui::TopBottomPanel::bottom("macshot_status")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(24, 11)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if !status_message.is_empty() {
                        ui.label(egui::RichText::new("●").color(ACCENT_BRIGHT));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&status_message)
                                    .size(11.0)
                                    .color(TEXT_MUTED),
                            )
                            .wrap(),
                        );
                    }
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
                            egui::RichText::new(self.language.text("● REC 準備中"))
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
                    .fill(egui::Color32::from_rgb(249, 248, 251))
                    .inner_margin(egui::Margin::symmetric(24, 20)),
            )
            .show(ctx, |ui| {
                paint_glass_backdrop(ui);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.page {
                        Page::Capture => self.show_capture_page(ui, ctx),
                        Page::Evidence => self.show_evidence_page(ui),
                        Page::Notifications => self.show_notifications_page(ui),
                        Page::Compare => self.show_compare_page(ui),
                        Page::Settings => self.show_settings_page(ui),
                    });
            });

        self.show_capture_editor_viewport(ctx);
        self.show_pending_edit_navigation_confirmation(ctx);
        if !self.capture_editor_expanded {
            self.show_overwrite_confirmation(ctx);
        }
        self.show_recording_player_viewport(ctx);
        self.show_recording_rename_dialog(ctx);
        self.show_capture_target_picker(ctx);

        if should_show_completion_notice(self.page) {
            if let Some(notice) = &self.capture_notice {
                let mut open = true;
                egui::Window::new(self.language.text("撮影完了"))
                    .id(egui::Id::new("capture_complete_notice"))
                    .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-18.0, 74.0))
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(
                            egui::RichText::new(self.language.text("●  撮影しました"))
                                .size(15.0)
                                .strong()
                                .color(ACCENT_BRIGHT),
                        );
                        ui.label(format!(
                            "{} {}  ·  {}",
                            self.language.text("撮影"),
                            notice.sequence,
                            notice.timestamp
                        ));
                        ui.label(
                            self.language
                                .message(&capture_size_label(notice.width, notice.height)),
                        );
                        ui.label(notice.path.display().to_string());
                        ui.label(format!(
                            "{} {}",
                            hotkey_badge_text(&self.capture_hotkey_text),
                            self.language.text("で続けて撮影できます")
                        ));
                    });
                if !open {
                    self.capture_notice = None;
                }
            }
            if let Some((path, timestamp, warning)) = &self.recording_notice {
                let mut open = true;
                egui::Window::new(self.language.text("録画を保存しました"))
                    .id(egui::Id::new("recording_complete_notice"))
                    .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-18.0, 236.0))
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(
                            egui::RichText::new(self.language.text("●  録画を保存しました"))
                                .size(15.0)
                                .strong()
                                .color(ACCENT_BRIGHT),
                        );
                        ui.label(timestamp);
                        ui.label(path.display().to_string());
                        if let Some(warning) = warning {
                            ui.label(self.language.message(warning));
                        }
                    });
                if !open {
                    self.recording_notice = None;
                }
            }
        }
    }
}

fn app_font_definitions(language: AppLanguage) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto_sans_jp".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/NotoSansJP-SemiBold.ttf"
        ))),
    );
    fonts.font_data.insert(
        "noto_sans_sc".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/NotoSansSC.ttf"
        ))),
    );

    let (primary, fallback) = if language == AppLanguage::SimplifiedChinese {
        ("noto_sans_sc", "noto_sans_jp")
    } else {
        ("noto_sans_jp", "noto_sans_sc")
    };
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let family_fonts = fonts.families.entry(family).or_default();
        family_fonts.insert(0, primary.into());
        family_fonts.insert(1, fallback.into());
    }
    fonts
}

#[cfg(target_os = "macos")]
fn disable_automatic_window_tabbing() {
    use objc2_app_kit::NSWindow;
    use objc2_foundation::MainThreadMarker;

    if let Some(main_thread) = MainThreadMarker::new() {
        NSWindow::setAllowsAutomaticWindowTabbing(false, main_thread);
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn cancel_dock_termination_and_hide(
    _delegate: *mut std::ffi::c_void,
    _selector: *mut std::ffi::c_void,
    sender: *mut std::ffi::c_void,
) -> i64 {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    if let Some(main_thread) = objc2_foundation::MainThreadMarker::new() {
        // SAFETY: AppKit invokes this callback with its NSApplication instance on the main thread.
        let application = unsafe { &*sender.cast::<NSApplication>() };
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        application.hide(None);
        let _ = main_thread;
    }
    0 // NSTerminateCancel: keep the menu-bar process running.
}

#[cfg(target_os = "macos")]
fn set_macos_activation_policy(policy: objc2_app_kit::NSApplicationActivationPolicy) {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    if let Some(main_thread) = MainThreadMarker::new() {
        NSApplication::sharedApplication(main_thread).setActivationPolicy(policy);
    }
}

#[cfg(target_os = "macos")]
fn keep_running_when_quit_from_dock() {
    use objc2::{msg_send, runtime::Sel, sel};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;
    use std::ffi::{c_char, c_void};

    #[link(name = "objc")]
    unsafe extern "C" {
        fn class_addMethod(
            class: *mut c_void,
            selector: *mut c_void,
            implementation: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i64,
            types: *const c_char,
        ) -> bool;
    }

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    // eframe/winit owns the delegate. Add AppKit's optional termination callback to that
    // existing delegate class so window/event-loop handling remains under winit's control.
    let Some(delegate) = (unsafe { application.delegate() }) else {
        return;
    };
    let delegate_class: *mut c_void = unsafe { msg_send![&*delegate, class] };
    let selector: Sel = sel!(applicationShouldTerminate:);
    let added = unsafe {
        class_addMethod(
            delegate_class,
            selector.as_ptr().cast_mut().cast(),
            cancel_dock_termination_and_hide,
            c"q@:@".as_ptr(),
        )
    };
    if !added {
        eprintln!("MacShot could not install the Dock-quit resident behavior");
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "macos")]
    disable_automatic_window_tabbing();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 640.0])
            .with_transparent(false)
            .with_icon(application_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "MacShot",
        options,
        Box::new(|creation| {
            #[cfg(target_os = "macos")]
            keep_running_when_quit_from_dock();
            let language = AppLanguage::from_setting(&load_setting("language", "ja"));
            creation.egui_ctx.set_fonts(app_font_definitions(language));
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
    fn recording_rename_distinguishes_unchanged_names_from_file_moves() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "macshot-rename-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let original = directory.join("original.mp4");
        std::fs::write(&original, b"test recording").unwrap();

        assert!(matches!(
            rename_recording_file(&original, "original").unwrap(),
            RecordingRenameOutcome::Unchanged
        ));
        assert!(original.exists());

        let renamed = directory.join("renamed.mp4");
        assert!(matches!(
            rename_recording_file(&original, "renamed").unwrap(),
            RecordingRenameOutcome::Renamed(path) if path == renamed
        ));
        assert_eq!(std::fs::read(&renamed).unwrap(), b"test recording");

        std::fs::remove_file(renamed).unwrap();
        std::fs::remove_dir(directory).unwrap();
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
    fn recording_player_controls_anchor_stays_inside_window_at_video_bottom() {
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 680.0));
        let video = egui::Rect::from_center_size(bounds.center(), egui::vec2(1046.0, 680.0));

        let width = recording_player_controls_width(Some(video), bounds);
        let anchor = recording_player_controls_anchor(Some(video), bounds, width);
        assert_eq!(anchor.x, video.left());
        assert_eq!(anchor.y, 672.0);
        assert!(bounds.contains(anchor));
        let portrait = egui::Rect::from_center_size(bounds.center(), egui::vec2(260.0, 520.0));
        let portrait_width = recording_player_controls_width(Some(portrait), bounds);
        let portrait_anchor =
            recording_player_controls_anchor(Some(portrait), bounds, portrait_width);
        assert_eq!(portrait_width, 760.0);
        assert_eq!(portrait_anchor.x, 180.0);
        assert_eq!(portrait_anchor.y, 592.0);
        assert_eq!(
            recording_player_controls_anchor(None, bounds, 1088.0),
            egui::pos2(16.0, 672.0)
        );
    }

    #[test]
    fn recording_player_controls_hide_when_pointer_leaves_video_and_controls() {
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 680.0));
        let video = egui::Rect::from_min_max(egui::pos2(40.0, 50.0), egui::pos2(1080.0, 630.0));
        let controls = egui::Rect::from_min_max(egui::pos2(40.0, 470.0), egui::pos2(1080.0, 670.0));
        let now = Instant::now();
        assert!(recording_player_controls_visible(
            Some(video),
            Some(controls),
            bounds,
            Some(egui::pos2(560.0, 340.0)),
            Some(now),
            now,
            false,
        ));
        assert!(recording_player_controls_visible(
            Some(video),
            Some(controls),
            bounds,
            Some(egui::pos2(560.0, 600.0)),
            Some(now),
            now,
            false,
        ));
        assert!(!recording_player_controls_visible(
            Some(video),
            Some(controls),
            bounds,
            Some(egui::pos2(1100.0, 30.0)),
            Some(now),
            now,
            false,
        ));
        assert!(!recording_player_controls_visible(
            Some(video),
            Some(controls),
            bounds,
            Some(egui::pos2(560.0, 340.0)),
            Some(now),
            now + Duration::from_secs(3),
            false,
        ));
        assert!(recording_player_controls_visible(
            Some(video),
            Some(controls),
            bounds,
            Some(egui::pos2(560.0, 600.0)),
            Some(now),
            now + Duration::from_secs(3),
            true,
        ));
        assert!(!recording_player_controls_visible(
            Some(video),
            Some(controls),
            bounds,
            None,
            Some(now),
            now,
            false,
        ));
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
    fn duration_from_seconds_rejects_invalid_asset_metadata() {
        assert_eq!(
            duration_from_seconds(12.5),
            Some(Duration::from_millis(12_500))
        );
        assert_eq!(duration_from_seconds(0.0), None);
        assert_eq!(duration_from_seconds(-1.0), None);
        assert_eq!(duration_from_seconds(f64::NAN), None);
        assert_eq!(duration_from_seconds(f64::INFINITY), None);
    }

    #[test]
    fn playback_time_formats_minute_and_hour_boundaries() {
        assert_eq!(format_playback_time(Duration::from_secs(9)), "00:09");
        assert_eq!(format_playback_time(Duration::from_secs(61)), "01:01");
        assert_eq!(format_playback_time(Duration::from_secs(3_600)), "01:00:00");
        assert_eq!(format_playback_time(Duration::from_secs(3_661)), "01:01:01");
    }

    #[test]
    fn paused_playback_keeps_the_selected_seek_position() {
        let selected = Duration::from_secs(4);
        assert_eq!(
            playback_position_for_display(
                selected,
                false,
                Some(Duration::from_secs(12)),
                Duration::from_secs(14),
            ),
            selected
        );
    }

    #[test]
    fn paused_playback_ignores_stale_frames_but_accepts_seek_frames() {
        assert!(!should_update_playback_position(false, false));
        assert!(should_update_playback_position(false, true));
        assert!(should_update_playback_position(true, false));
    }

    #[test]
    fn playback_audio_recreation_handles_file_backed_and_finished_tracks() {
        assert!(playback_audio_requires_recreation(
            false,
            Some(Path::new("/tmp/macshot.mov-audio.m4a"))
        ));
        assert!(playback_audio_requires_recreation(
            false,
            Some(Path::new("/tmp/macshot-playback.aac"))
        ));
        assert!(playback_audio_requires_recreation(
            true,
            Some(Path::new("/tmp/macshot-playback.aac"))
        ));
        assert!(!playback_audio_requires_recreation(false, None));
    }

    #[test]
    fn playing_playback_tracks_audio_and_clamps_to_video_duration() {
        assert_eq!(
            playback_position_for_display(
                Duration::from_secs(4),
                true,
                Some(Duration::from_secs(12)),
                Duration::from_secs(10),
            ),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn playback_audio_meter_scales_rms_to_decibels() {
        let (silence, silence_dbfs) = audio_meter_value(0.0);
        assert_eq!(silence, 0.0);
        assert_eq!(silence_dbfs, -60.0);

        let (half_scale, half_scale_dbfs) = audio_meter_value(0.5);
        assert!((half_scale_dbfs - -6.0206).abs() < 0.01);
        assert!((half_scale - 0.8997).abs() < 0.001);

        let (clipped, clipped_dbfs) = audio_meter_value(1.5);
        assert_eq!(clipped, 1.0);
        assert_eq!(clipped_dbfs, 0.0);
    }

    #[test]
    fn audio_waveform_marks_silent_and_audible_time_bins() {
        let samples = [0.0, 0.0, 0.0, 0.0, 0.5, -0.5, 0.5, -0.5];
        let levels = audio_waveform_levels(samples.into_iter(), 2, 4, Duration::from_secs(1), 2);
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0], 0.0);
        assert!((levels[1] - 0.5).abs() < 0.001);
        assert!(!audio_waveform_has_sound(levels[0]));
        assert!(audio_waveform_has_sound(levels[1]));
        assert!(!audio_waveform_has_sound(f32::NAN));
    }

    #[test]
    fn audio_waveform_has_a_bounded_fallback_for_unknown_duration() {
        assert_eq!(
            audio_waveform_levels([0.25].into_iter(), 1, 1, Duration::ZERO, 4),
            vec![0.0]
        );
    }

    #[test]
    fn playback_audio_meter_reports_rms_level() {
        let level = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let source = rodio::buffer::SamplesBuffer::new(
            1_u16.try_into().unwrap(),
            48_000_u32.try_into().unwrap(),
            vec![0.5; 2_048],
        );
        let mut metered = MeteredAudioSource {
            source,
            level: level.clone(),
            sum_squares: 0.0,
            samples: 0,
        };
        assert_eq!(metered.by_ref().count(), 2_048);
        assert!((f32::from_bits(level.load(Ordering::Relaxed)) - 0.5).abs() < 0.001);
    }

    #[test]
    fn recording_playback_status_tracks_play_and_pause() {
        assert_eq!(
            recording_playback_status(true),
            "録画をアプリ内で再生しています。"
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
        for observed in [15_724_879.0 / 11.23 * 60.0, 19_825_087.0 / 13.96 * 60.0] {
            assert!((gif.low_bytes..=gif.high_bytes).contains(&observed));
        }
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
    fn completion_notices_are_limited_to_the_capture_page() {
        assert!(should_show_completion_notice(Page::Capture));
        assert!(!should_show_completion_notice(Page::Evidence));
        assert!(!should_show_completion_notice(Page::Compare));
        assert!(!should_show_completion_notice(Page::Settings));
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
            folder.join("録画_2026-10-25_12-13-14_015_1000.gif")
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
            "録画_2026-10-25_12-00-01_123_001-copy-2.gif"
        );
        assert_eq!(std::fs::read(first).unwrap(), b"existing recording");
    }

    #[test]
    fn preferences_round_trip_saved_values_and_keep_equals_in_paths() {
        let settings_layout = default_settings_layout();
        let preferences = Preferences {
            language: AppLanguage::TraditionalChinese,
            capture_hotkey: "Alt+Shift+KeyS",
            recording_hotkey: "Control+Alt+KeyR",
            capture_target: "window:123",
            capture_shortcut_behavior: CaptureShortcutBehavior::ChooseMode,
            timestamp_enabled: false,
            timestamp_format: "datetime_millis",
            timestamp_position: "top_left",
            timestamp_background: "none",
            filename_prefix: "案件_β",
            show_capture_notice: false,
            keep_region_in_continuous: true,
            annotation_color: "blue",
            annotation_width: 3.5,
            annotation_text: "確認",
            save_folder: Path::new("/tmp/MacShot=Evidence"),
            auto_import_native_captures: true,
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
            parse_setting(&serialized, "capture_shortcut_behavior", "selected_target"),
            "choose_mode"
        );
        assert_eq!(
            parse_setting(&serialized, "timestamp_enabled", "true"),
            "false"
        );
        assert_eq!(
            parse_setting(&serialized, "timestamp_format", "datetime"),
            "datetime_millis"
        );
        assert_eq!(
            parse_setting(&serialized, "timestamp_position", "bottom_right"),
            "top_left"
        );
        assert_eq!(
            parse_setting(&serialized, "timestamp_background", "subtle"),
            "none"
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
            parse_setting(&serialized, "annotation_text", "注釈"),
            "確認"
        );
        assert_eq!(
            parse_setting(&serialized, "save_folder", "/default"),
            "/tmp/MacShot=Evidence"
        );
        assert_eq!(
            parse_setting(&serialized, "auto_import_native_captures", "false"),
            "true"
        );
        assert_eq!(parse_setting(&serialized, "language", "ja"), "zh-Hant");
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
    fn app_language_values_and_primary_navigation_cover_all_four_locales() {
        for (language, setting) in [
            (AppLanguage::Japanese, "ja"),
            (AppLanguage::English, "en"),
            (AppLanguage::SimplifiedChinese, "zh-Hans"),
            (AppLanguage::TraditionalChinese, "zh-Hant"),
        ] {
            assert_eq!(AppLanguage::from_setting(setting), language);
            assert_eq!(language.setting_value(), setting);
            for page in [
                Page::Capture,
                Page::Evidence,
                Page::Notifications,
                Page::Compare,
                Page::Settings,
            ] {
                assert!(!language.page_label(page).is_empty());
            }
        }
        assert_eq!(AppLanguage::from_setting("unknown"), AppLanguage::Japanese);
    }

    #[test]
    fn changing_pages_clears_context_specific_status_but_same_page_keeps_it() {
        let mut page = Page::Capture;
        let mut status =
            "始点を指定しました。終点をクリックするかドラッグしてください。".to_string();

        assert!(switch_page(&mut page, Page::Evidence, &mut status));
        assert!(page == Page::Evidence);
        assert!(status.is_empty());

        status = "証跡一覧を更新しました".into();
        assert!(!switch_page(&mut page, Page::Evidence, &mut status));
        assert_eq!(status, "証跡一覧を更新しました");
    }

    #[test]
    fn macos_application_menu_uses_the_selected_app_language() {
        for (language, expected) in [
            (
                AppLanguage::Japanese,
                ["MacShotについて", "MacShotを隠す", "MacShotを終了"],
            ),
            (
                AppLanguage::English,
                ["About MacShot", "Hide MacShot", "Quit MacShot"],
            ),
            (
                AppLanguage::SimplifiedChinese,
                ["关于 MacShot", "隐藏 MacShot", "退出 MacShot"],
            ),
            (
                AppLanguage::TraditionalChinese,
                ["關於 MacShot", "隱藏 MacShot", "結束 MacShot"],
            ),
        ] {
            assert_eq!(language.macos_app_menu_label("about"), Some(expected[0]));
            assert_eq!(language.macos_app_menu_label("hide"), Some(expected[1]));
            assert_eq!(language.macos_app_menu_label("quit"), Some(expected[2]));
            assert!(language.macos_app_menu_label("unknown").is_none());
        }
    }

    #[test]
    fn simplified_chinese_prioritizes_its_matching_cjk_font() {
        let simplified = app_font_definitions(AppLanguage::SimplifiedChinese);
        let japanese = app_font_definitions(AppLanguage::Japanese);
        assert_eq!(
            simplified.families[&egui::FontFamily::Proportional][0],
            "noto_sans_sc"
        );
        assert_eq!(
            simplified.families[&egui::FontFamily::Monospace][0],
            "noto_sans_sc"
        );
        assert_eq!(
            japanese.families[&egui::FontFamily::Proportional][0],
            "noto_sans_jp"
        );
    }

    #[test]
    fn app_language_localizes_dynamic_status_and_empty_states() {
        let empty_preview = "画面全体またはウィンドウを撮影すると、ここにプレビューが表示されます。\n画像をドラッグして必要な範囲を切り出せます。";
        let failed_recording = "録画を再生できませんでした: decoder failed";
        let continuous_status = continuous_capture_status_label(true, true, true, 2, 1);

        assert_eq!(AppLanguage::Japanese.text(empty_preview), empty_preview);
        assert_eq!(
            AppLanguage::Japanese.message(failed_recording),
            failed_recording
        );
        assert_eq!(
            AppLanguage::English.text("元の画面サイズ"),
            "Original Screen Size"
        );
        assert_eq!(
            AppLanguage::English.text("証跡一覧を更新しました"),
            "Evidence list refreshed"
        );
        assert_eq!(
            AppLanguage::SimplifiedChinese.text("証跡一覧を更新しました"),
            "已更新取证记录列表"
        );
        assert_eq!(
            AppLanguage::TraditionalChinese.text("証跡一覧を更新しました"),
            "已更新證據清單"
        );
        for (language, annotated_copy, cropped_copy) in [
            (
                AppLanguage::English,
                "Save Annotated Copy",
                "Save Cropped Copy",
            ),
            (
                AppLanguage::SimplifiedChinese,
                "另存为标注图片",
                "另存为裁剪图像",
            ),
            (
                AppLanguage::TraditionalChinese,
                "另存為註記圖片",
                "另存為裁切影像",
            ),
        ] {
            assert_eq!(language.text("注釈画像を別名で保存"), annotated_copy);
            assert_eq!(language.text("切り出しを別名で保存"), cropped_copy);
        }
        assert!(
            !AppLanguage::English
                .message("画面収録の許可を設定してください。撮影画面の案内から要求できます。")
                .contains("撮影画面")
        );
        for (language, expected) in [
            (
                AppLanguage::English,
                "Opened Microphone Settings. Refresh the permission status",
            ),
            (AppLanguage::SimplifiedChinese, "已打开麦克风设置。更改后"),
            (AppLanguage::TraditionalChinese, "已開啟麥克風設定。變更後"),
        ] {
            assert!(
                language
                    .message("マイク設定を開きました。変更後に「権限状態を更新」を押してください。")
                    .starts_with(expected)
            );
        }
        assert!(
            AppLanguage::English
                .text(empty_preview)
                .starts_with("Capture the full screen")
        );
        assert!(
            AppLanguage::SimplifiedChinese
                .text(empty_preview)
                .starts_with("截取整个屏幕")
        );
        assert!(
            AppLanguage::TraditionalChinese
                .text(empty_preview)
                .starts_with("擷取整個螢幕")
        );
        assert!(
            AppLanguage::English
                .message(failed_recording)
                .starts_with("Unable to play recording:")
        );
        assert!(
            AppLanguage::SimplifiedChinese
                .message(failed_recording)
                .starts_with("无法播放录屏：")
        );
        assert!(
            AppLanguage::TraditionalChinese
                .message(failed_recording)
                .starts_with("無法播放錄影：")
        );
        assert!(
            AppLanguage::English
                .message(&continuous_status)
                .starts_with("Continuous capture")
        );
        assert!(
            AppLanguage::SimplifiedChinese
                .message(&continuous_status)
                .starts_with("连续截图")
        );
        assert!(
            AppLanguage::TraditionalChinese
                .message(&continuous_status)
                .starts_with("連續截圖")
        );
    }

    #[test]
    fn window_filter_rejects_menu_bars_and_tooltips() {
        assert!(!has_captureable_window_size(1512, 33));
        assert!(!has_captureable_window_size(66, 20));
        assert!(has_captureable_window_size(708, 464));
    }

    #[test]
    fn localized_window_search_matches_common_windows_apps() {
        let notepad = WindowChoice {
            id: 1,
            app_name: "Notepad.exe".into(),
            title: "Untitled".into(),
            width: 800,
            height: 600,
        };
        assert!(window_search_matches(&notepad, "メモ帳"));
        assert!(window_search_matches(&notepad, "notepad"));
        assert!(!window_search_matches(&notepad, "電卓"));
    }

    #[test]
    fn annotation_drag_requires_a_visible_distance() {
        assert!(!has_annotation_drag_distance(
            egui::pos2(0.2, 0.2),
            egui::pos2(0.201, 0.2)
        ));
        assert!(has_annotation_drag_distance(
            egui::pos2(0.2, 0.2),
            egui::pos2(0.205, 0.2)
        ));
    }

    #[test]
    fn continuous_capture_reuses_only_an_enabled_saved_region() {
        let region = Some([0.1, 0.2, 0.6, 0.7]);
        assert_eq!(retained_continuous_region(true, true, region), region);
        assert_eq!(retained_continuous_region(true, false, region), None);
        assert_eq!(retained_continuous_region(false, true, region), None);
    }

    #[test]
    fn untitled_windows_keep_the_application_name_as_a_choice() {
        assert_eq!(window_title_or_app_name(None, "Notepad"), "Notepad");
        assert_eq!(window_title_or_app_name(Some("  "), "Notepad"), "Notepad");
        assert_eq!(
            window_title_or_app_name(Some(" Untitled - Notepad "), "Notepad"),
            "Untitled - Notepad"
        );
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
    fn window_and_active_recordings_use_window_frames() {
        assert!(recording_target_uses_window_capture(
            CaptureTarget::ActiveWindow
        ));
        assert!(recording_target_uses_window_capture(CaptureTarget::Window(
            42
        )));
        assert!(!recording_target_uses_window_capture(
            CaptureTarget::PrimaryDisplay
        ));
        assert!(!recording_target_uses_window_capture(
            CaptureTarget::AllDisplays
        ));
        assert!(!recording_target_uses_window_capture(
            CaptureTarget::Display(0)
        ));
    }

    #[test]
    fn active_window_selection_stays_with_the_frontmost_application() {
        let candidates = [
            ActiveWindowCandidate {
                window_id: 1,
                owning_process_id: Some(200),
                captureable: true,
                stage_manager_active: true,
            },
            ActiveWindowCandidate {
                window_id: 2,
                owning_process_id: Some(100),
                captureable: true,
                stage_manager_active: false,
            },
            ActiveWindowCandidate {
                window_id: 3,
                owning_process_id: Some(100),
                captureable: true,
                stage_manager_active: true,
            },
            ActiveWindowCandidate {
                window_id: 4,
                owning_process_id: Some(100),
                captureable: false,
                stage_manager_active: true,
            },
        ];

        assert_eq!(select_active_window_candidate(&candidates, 100), Some(3));
        assert_eq!(select_active_window_candidate(&candidates, 200), Some(1));
        assert_eq!(select_active_window_candidate(&candidates, 300), None);
    }

    #[test]
    fn recording_list_finds_gif_mp4_and_mov_but_not_images() {
        let directory = TestDirectory::new("recording-list");
        for name in [
            "clip.gif",
            "movie.mp4",
            "movie-uppercase.MP4",
            "still.png",
            "other.mov",
        ] {
            std::fs::write(directory.path().join(name), b"test").unwrap();
        }
        let files = recording_files(directory.path());
        assert_eq!(files.len(), 4);
        assert!(files.iter().any(|path| path.ends_with("clip.gif")));
        assert!(files.iter().any(|path| path.ends_with("movie.mp4")));
        assert!(
            files
                .iter()
                .any(|path| path.ends_with("movie-uppercase.MP4"))
        );
        assert!(files.iter().any(|path| path.ends_with("other.mov")));
    }

    #[test]
    fn mp4_audio_playback_recognizes_case_insensitive_extensions() {
        assert!(is_mp4_recording(Path::new("clip.mp4")));
        assert!(is_mp4_recording(Path::new("clip.MP4")));
        assert!(!is_mp4_recording(Path::new("clip.mov")));
        assert!(!is_mp4_recording(Path::new("clip.gif")));
    }

    #[test]
    fn native_capture_import_accepts_images_and_recordings_only() {
        for name in [
            "capture.png",
            "capture.jpeg",
            "recording.mov",
            "recording.mp4",
            "clip.gif",
        ] {
            assert!(is_native_capture_file(Path::new(name)));
        }
        for name in ["notes.txt", ".capture.mov", "folder"] {
            assert!(!is_native_capture_file(Path::new(name)));
        }
    }

    #[test]
    fn native_capture_import_copies_without_overwriting_existing_files() {
        let source_dir = TestDirectory::new("native-import-source");
        let destination_dir = TestDirectory::new("native-import-destination");
        let source = source_dir.path().join("Screen Recording.mov");
        std::fs::write(&source, b"new native recording").unwrap();
        let existing = destination_dir.path().join("Screen Recording.mov");
        std::fs::write(&existing, b"older recording").unwrap();

        let imported = copy_native_capture(&source, destination_dir.path()).unwrap();
        assert_ne!(imported, existing);
        assert_eq!(std::fs::read(&existing).unwrap(), b"older recording");
        assert_eq!(std::fs::read(imported).unwrap(), b"new native recording");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_capture_import_worker_returns_the_copied_file() {
        let source_dir = TestDirectory::new("native-import-worker-source");
        let destination_dir = TestDirectory::new("native-import-worker-destination");
        let source = source_dir.path().join("Screen Recording.mov");
        std::fs::write(&source, b"native video fixture").unwrap();
        let (import_tx, import_rx) = mpsc::channel();

        spawn_native_capture_import(
            source.clone(),
            destination_dir.path().to_path_buf(),
            import_tx,
        );

        let event = import_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(event.source, source);
        let imported = event.result.unwrap();
        assert_eq!(imported.file_name(), source.file_name());
        assert_eq!(std::fs::read(imported).unwrap(), b"native video fixture");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_capture_scan_finds_images_and_video_in_monitored_folders() {
        let directory = TestDirectory::new("native-import-scan");
        for name in [
            "Screenshot.png",
            "Screen Recording.mov",
            "notes.txt",
            ".temporary.mp4",
        ] {
            std::fs::write(directory.path().join(name), b"test").unwrap();
        }
        let files = scan_native_capture_files(&[directory.path().to_path_buf()]);
        assert_eq!(files.len(), 2);
        assert!(files.contains_key(&directory.path().join("Screenshot.png")));
        assert!(files.contains_key(&directory.path().join("Screen Recording.mov")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_capture_import_waits_for_a_stable_file_before_copying() {
        let path = PathBuf::from("/tmp/Screen Recording.mov");
        let mut watch = NativeCaptureWatch {
            started_at: Instant::now(),
            known_files: HashSet::new(),
            in_flight: HashSet::new(),
            pending_sizes: HashMap::new(),
        };
        let sizes = |size| HashMap::from([(path.clone(), size)]);
        assert!(completed_native_capture_files(&mut watch, sizes(10)).is_empty());
        assert!(completed_native_capture_files(&mut watch, sizes(20)).is_empty());
        assert!(completed_native_capture_files(&mut watch, sizes(20)).is_empty());
        let completed = completed_native_capture_files(&mut watch, sizes(20));
        assert_eq!(completed.as_slice(), std::slice::from_ref(&path));
        watch.in_flight.insert(path.clone());
        assert!(completed_native_capture_files(&mut watch, sizes(20)).is_empty());
        watch.in_flight.remove(&path);
        watch.pending_sizes.remove(&path);
        assert!(completed_native_capture_files(&mut watch, sizes(20)).is_empty());
        assert!(completed_native_capture_files(&mut watch, sizes(20)).is_empty());
        assert_eq!(
            completed_native_capture_files(&mut watch, sizes(20)).as_slice(),
            std::slice::from_ref(&path),
            "a failed copy should become eligible again after the file is stable"
        );
        watch.known_files.insert(path.clone());
        assert!(completed_native_capture_files(&mut watch, sizes(20)).is_empty());
        assert!(completed_native_capture_files(&mut watch, HashMap::new()).is_empty());
        assert!(watch.pending_sizes.is_empty());
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
        let (first, first_delay, _) = playback.next_frame().unwrap().unwrap();
        let (second, second_delay, _) = playback.next_frame().unwrap().unwrap();
        assert_eq!(first.dimensions(), (2, 1));
        assert_eq!(second.dimensions(), (2, 1));
        assert_ne!(first.get_pixel(0, 0), second.get_pixel(0, 0));
        assert_eq!(first_delay, Duration::from_millis(150));
        assert_eq!(second_delay, Duration::from_millis(200));
        assert!(playback.next_frame().unwrap().is_none());
    }

    #[test]
    fn gif_playback_composites_cropped_frames_onto_the_canvas() {
        let directory = TestDirectory::new("gif-cropped-frames");
        let path = directory.path().join("cropped.gif");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(file, 4, 2, &[]).unwrap();
        let mut base = [220, 20, 30, 255].repeat(8);
        write_gif_frame(&mut encoder, 4, 2, &mut base, 10).unwrap();

        let mut patch = vec![30, 210, 90, 255];
        let mut frame = gif::Frame::from_rgba(1, 1, &mut patch);
        frame.left = 2;
        frame.top = 0;
        frame.delay = 10;
        encoder.write_frame(&frame).unwrap();
        drop(encoder);

        let (preview, _) = recording_preview(&path).unwrap();
        assert_eq!(preview.dimensions(), (4, 2));
        let mut playback = RecordingFrameReader::open(&path).unwrap();
        let first = playback.next_frame().unwrap().unwrap().0;
        let second = playback.next_frame().unwrap().unwrap().0;
        assert_eq!(first.dimensions(), (4, 2));
        assert_eq!(second.dimensions(), (4, 2));
        assert_eq!(second.get_pixel(0, 0), image::Rgba([220, 20, 30, 255]));
        assert_eq!(second.get_pixel(2, 0), image::Rgba([30, 210, 90, 255]));
        assert_eq!(second.get_pixel(3, 1), image::Rgba([220, 20, 30, 255]));
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
        playback
            .request_tx
            .send(RecordingPlaybackCommand::Seek(Duration::from_millis(100)))
            .unwrap();
        let RecordingPlaybackEvent::Frame(seeked, _, position, is_seek) = playback
            .frame_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
        else {
            panic!("playback worker should deliver its first frame");
        };
        assert_eq!(position, Duration::from_millis(100));
        assert!(is_seek);
        assert_eq!(seeked.get_pixel(0, 0), image::Rgba([20, 180, 70, 255]));
        playback
            .request_tx
            .send(RecordingPlaybackCommand::NextFrame)
            .unwrap();
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
        let (first_loop, _, _) = playback.next_frame().unwrap().unwrap();
        let (_, _, _) = playback.next_frame().unwrap().unwrap();
        let (second_loop, _, _) = playback.next_frame().unwrap().unwrap();
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
                .usage_type(UsageType::ScreenContentRealTime)
                .adaptive_quantization(false)
                .background_detection(false),
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
        let (first, first_delay, _) = playback.next_frame().unwrap().unwrap();
        let (second, second_delay, _) = playback.next_frame().unwrap().unwrap();
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
                .usage_type(UsageType::ScreenContentRealTime)
                .adaptive_quantization(false)
                .background_detection(false),
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
        let audio_path = extract_mp4_audio_to_temp(&path).unwrap();
        let decoder = rodio::Decoder::new_aac(std::fs::File::open(&audio_path).unwrap()).unwrap();
        let mut seek_decoder =
            rodio::Decoder::new_aac(std::fs::File::open(&audio_path).unwrap()).unwrap();
        rodio::Source::try_seek(&mut seek_decoder, Duration::from_millis(100)).unwrap();
        let meter = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let mut metered = MeteredAudioSource {
            source: decoder,
            level: meter.clone(),
            sum_squares: 0.0,
            samples: 0,
        };
        let decoded: Vec<_> = metered.by_ref().collect();
        assert!(decoded.len() > 2_048);
        assert!(decoded.iter().any(|sample| sample.abs() > 0.001));
        assert!(f32::from_bits(meter.load(Ordering::Relaxed)) > 0.001);
        std::fs::remove_file(audio_path).unwrap();
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
            app_name: "Calculator".into(),
            title: "Calculator".into(),
            width: 640,
            height: 480,
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
        app.capture_queue.push_back(CaptureRequest {
            target: CaptureTarget::PrimaryDisplay,
            region: None,
        });
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
            &Annotation {
                tool: EditTool::Rectangle,
                start: egui::pos2(0.1, 0.1),
                end: egui::pos2(0.8, 0.8),
                text: String::new(),
            },
            annotation_rgb("red"),
            3.0,
            "subtle",
        );
        assert_eq!(*image.get_pixel(10, 10), Rgba([255, 38, 38, 255]));
        assert_eq!(*image.get_pixel(50, 50), background);

        draw_annotation(
            &mut image,
            &Annotation {
                tool: EditTool::Arrow,
                start: egui::pos2(0.1, 0.5),
                end: egui::pos2(0.8, 0.5),
                text: String::new(),
            },
            annotation_rgb("blue"),
            3.0,
            "subtle",
        );
        assert_eq!(*image.get_pixel(50, 50), Rgba([30, 130, 255, 255]));
    }

    #[test]
    fn timestamp_stamp_preserves_dimensions_and_draws_text() {
        let mut image = ImageBuffer::from_pixel(320, 100, Rgba([0, 0, 0, 255]));
        stamp_timestamp(&mut image, "2026-10-25 19:05:12", "bottom_right", "subtle");

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
    fn region_drag_keeps_press_origin_when_press_and_release_arrive_together() {
        let mut start = None;
        let mut current = None;
        let press_origin = egui::pos2(20.0, 25.0);
        let release_position = egui::pos2(80.0, 65.0);

        update_region_drag_state(
            &mut start,
            &mut current,
            true,
            false,
            true,
            Some(release_position),
            Some(press_origin),
        );

        assert_eq!(start, Some(press_origin));
        assert_eq!(current, Some(release_position));
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        assert!(normalized_region_bounds(screen, start.unwrap(), current.unwrap()).is_some());
    }

    #[test]
    fn region_drag_updates_end_position_across_pointer_frames() {
        let mut start = None;
        let mut current = None;
        let press_origin = egui::pos2(15.0, 18.0);
        let drag_position = egui::pos2(48.0, 52.0);

        update_region_drag_state(
            &mut start,
            &mut current,
            true,
            true,
            false,
            Some(press_origin),
            Some(press_origin),
        );
        update_region_drag_state(
            &mut start,
            &mut current,
            false,
            true,
            false,
            Some(drag_position),
            Some(press_origin),
        );
        update_region_drag_state(
            &mut start,
            &mut current,
            false,
            false,
            true,
            Some(drag_position),
            Some(press_origin),
        );

        assert_eq!(start, Some(press_origin));
        assert_eq!(current, Some(drag_position));
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

    #[test]
    fn moving_between_evidence_keeps_pending_annotations_until_resolved() {
        let current = PathBuf::from("current.png");
        let mut app = MacShot {
            left_path: Some(current.clone()),
            editing_queue: vec![current.clone(), PathBuf::from("next.png")],
            editing_index: Some(0),
            annotations: vec![Annotation {
                tool: EditTool::Rectangle,
                start: egui::pos2(0.1, 0.1),
                end: egui::pos2(0.8, 0.8),
                text: String::new(),
            }],
            ..MacShot::default()
        };

        app.move_in_edit_queue(1);

        assert_eq!(app.left_path, Some(current));
        assert_eq!(app.annotations.len(), 1);
        assert!(matches!(
            app.pending_edit_navigation,
            Some(PendingEditNavigation::Queue { direction: 1 })
        ));
    }

    #[test]
    fn opening_another_evidence_defers_while_edits_are_pending() {
        let current = PathBuf::from("current.png");
        let target = PathBuf::from("target.png");
        let mut app = MacShot {
            left_path: Some(current.clone()),
            annotations: vec![Annotation {
                tool: EditTool::Mosaic,
                start: egui::pos2(0.2, 0.2),
                end: egui::pos2(0.4, 0.4),
                text: String::new(),
            }],
            ..MacShot::default()
        };

        app.open_evidence_image(vec![target.clone()], 0);

        assert_eq!(app.left_path, Some(current));
        assert_eq!(app.annotations.len(), 1);
        assert!(matches!(
            app.pending_edit_navigation,
            Some(PendingEditNavigation::OpenEvidence { queue, index: 0 }) if queue == vec![target]
        ));
    }
}
