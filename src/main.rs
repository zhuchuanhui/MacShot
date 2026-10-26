#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ab_glyph::{FontRef, PxScale};
use chrono::Local;
use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgba};
use imageproc::{
    drawing::{
        draw_filled_rect_mut, draw_hollow_rect_mut, draw_line_segment_mut, draw_text_mut, text_size,
    },
    rect::Rect as ImageRect,
};
#[cfg(target_os = "macos")]
use screencapturekit::{
    screenshot_manager::{CGImageExt, SCScreenshotManager},
    shareable_content::{SCShareableContent, SCShareableContentInfo, SCWindow},
    stream::{configuration::SCStreamConfiguration, content_filter::SCContentFilter},
};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::{
    collections::VecDeque,
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

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
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

fn should_show_screen_permission_banner(
    access_granted: bool,
    editing_existing_image: bool,
) -> bool {
    !access_granted && !editing_existing_image
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
    region_background_texture: Option<egui::TextureHandle>,
    save_folder: Option<PathBuf>,
    evidence_files: Vec<PathBuf>,
    timestamp_enabled: bool,
    filename_prefix: String,
    show_capture_notice: bool,
    annotation_color: String,
    annotation_width: f32,
    status: String,
    capture_in_progress: bool,
    capture_tx: Sender<CaptureResult>,
    capture_rx: Receiver<CaptureResult>,
    _hotkey_manager: Option<GlobalHotKeyManager>,
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
    capture_notice: Option<CaptureNotice>,
    recording_rx: Receiver<RecordingEvent>,
    recording_tx: Sender<RecordingEvent>,
    recording_stop_tx: Arc<Mutex<Option<Sender<()>>>>,
    recording: bool,
    recording_starting: bool,
    recording_started_at: Option<Instant>,
    recording_notice: Option<(PathBuf, String)>,
    record_pending_stop: bool,
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

type CaptureResult = Result<(PathBuf, image::RgbaImage, String, u64), String>;

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
    Started { path: PathBuf },
    Finished(Result<PathBuf, String>),
}

const DEFAULT_CAPTURE_HOTKEY: &str = "Alt+Shift+KeyS";
const DEFAULT_RECORDING_HOTKEY: &str = "Alt+Shift+KeyR";

fn recording_time_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
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
            region_background_texture: None,
            save_folder,
            evidence_files: Vec::new(),
            timestamp_enabled,
            filename_prefix,
            show_capture_notice,
            annotation_color,
            annotation_width,
            status,
            capture_in_progress: false,
            capture_tx,
            capture_rx,
            _hotkey_manager: None,
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
            capture_notice: None,
            recording_rx,
            recording_tx,
            recording_stop_tx: Arc::new(Mutex::new(None)),
            recording: false,
            recording_starting: false,
            recording_started_at: None,
            recording_notice: None,
            record_pending_stop: false,
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

fn parse_evidence_identity(path: &Path) -> Option<(String, u64)> {
    let stem = path.file_stem()?.to_str()?;
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

fn new_recording_path(folder: &Path, timestamp: &str, sequence: u64) -> PathBuf {
    folder.join(format!("recording-{timestamp}-{sequence:03}.gif"))
}

fn gif_delay_centiseconds(elapsed: Duration) -> u16 {
    elapsed.as_millis().div_ceil(10).clamp(1, u16::MAX as u128) as u16
}

fn should_show_window_after_recording(continuous_capture: bool) -> bool {
    !continuous_capture
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

fn recording_failure_message(error: &str) -> String {
    if cfg!(target_os = "macos") && error.contains("TCC") && error.contains("拒否") {
        capture_failure_message(error)
    } else {
        format!("録画できませんでした: {error}")
    }
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
            let configuration = SCStreamConfiguration::new()
                .with_width(display.width())
                .with_height(display.height());
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
    if title.trim().is_empty() {
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
    use objc2_app_kit::{NSApplication, NSStatusWindowLevel};
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    let windows = application.windows();
    let target_title = NSString::from_str(title);
    for index in 0..windows.count() {
        let window = unsafe { windows.objectAtIndex(index) };
        let current_title = window.title();
        if unsafe { current_title.isEqualToString(&target_title) } {
            window.setLevel(NSStatusWindowLevel + 1);
            return;
        }
    }
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
    Some([
        (selection.left() - screen.left()) / screen.width(),
        (selection.top() - screen.top()) / screen.height(),
        (selection.right() - screen.left()) / screen.width(),
        (selection.bottom() - screen.top()) / screen.height(),
    ])
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
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 170))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 200),
        ))
        .corner_radius(egui::CornerRadius::same(20))
        .inner_margin(egui::Margin::same(16))
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
                "⌥"
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

fn record_screen_to_gif(
    target: CaptureTarget,
    folder: PathBuf,
    file_timestamp: String,
    sequence: u64,
    stop_rx: Receiver<()>,
    event_tx: Sender<RecordingEvent>,
    ctx: egui::Context,
) {
    let result = (|| -> Result<PathBuf, String> {
        std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
        let monitor = monitor_for_recording(target)?;
        let display_capture = DisplayCapture::new(&monitor)?;
        let first = display_capture.capture_image()?;
        if first.width() > u16::MAX as u32 || first.height() > u16::MAX as u32 {
            return Err("画面サイズがGIFの上限を超えています。".into());
        }
        let width = first.width() as u16;
        let height = first.height() as u16;
        let path = new_recording_path(&folder, &file_timestamp, sequence);
        let _ = event_tx.send(RecordingEvent::Started { path: path.clone() });

        let file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
        let mut encoder =
            gif::Encoder::new(file, width, height, &[]).map_err(|error| error.to_string())?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .map_err(|error| error.to_string())?;

        let mut first_pixels = first.into_raw();
        write_gif_frame(&mut encoder, width, height, &mut first_pixels, 10)
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
            match stop_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let frame = display_capture.capture_image()?;
            if frame.width() != width as u32
                || frame.height() != height as u32
                || last_written.elapsed() < Duration::from_millis(100)
            {
                continue;
            }
            let mut pixels = frame.into_raw();
            write_gif_frame(
                &mut encoder,
                width,
                height,
                &mut pixels,
                gif_delay_centiseconds(last_written.elapsed()),
            )
            .map_err(|error| error.to_string())?;
            last_written = Instant::now();
        }
        encoder
            .into_inner()
            .map_err(|error| error.to_string())?
            .sync_all()
            .map_err(|error| error.to_string())?;
        Ok(path)
    })();

    if let Err(error) = &result {
        let _ = event_tx.send(RecordingEvent::Finished(Err(error.clone())));
    } else {
        let _ = event_tx.send(RecordingEvent::Finished(result));
    }
    ctx.request_repaint();
}

fn write_gif_frame<W: Write>(
    encoder: &mut gif::Encoder<W>,
    width: u16,
    height: u16,
    rgba: &mut [u8],
    delay: u16,
) -> Result<(), gif::EncodingError> {
    let mut frame = gif::Frame::from_rgba_speed(width, height, rgba, 10);
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
            Ok(manager) => manager,
            Err(error) => {
                self.status = format!("撮影ショートカットを設定できません: {error}");
                return;
            }
        };
        let hotkeys = [self.capture_hotkey, self.recording_hotkey];
        if let Err(error) = manager.register_all(&hotkeys) {
            self.status = format!("撮影ショートカットを登録できません: {error}");
            return;
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
        self._hotkey_manager = Some(manager);
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
                TrayAction::Quit => {
                    if let Ok(stop) = self.recording_stop_tx.lock()
                        && let Some(stop) = stop.as_ref()
                    {
                        let _ = stop.send(());
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
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
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        let now = Local::now();
        let file_timestamp = now.format("%Y%m%d-%H%M%S-%3f").to_string();
        let (stop_tx, stop_rx) = mpsc::channel();
        if let Ok(mut recording_stop_tx) = self.recording_stop_tx.lock() {
            *recording_stop_tx = Some(stop_tx);
        }
        self.recording_starting = true;
        self.record_pending_stop = false;
        self.status = format!("録画を開始しています（GIF・{}枚目）…", sequence);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        let sender = self.recording_tx.clone();
        let context = ctx.clone();
        thread::spawn(move || {
            record_screen_to_gif(
                target,
                folder,
                file_timestamp,
                sequence,
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
                RecordingEvent::Started { path } => {
                    self.recording_starting = false;
                    self.recording = true;
                    self.recording_started_at = Some(Instant::now());
                    self.status = format!(
                        "録画中 — {}。メニューバーまたは録画ショートカットで終了",
                        path.display()
                    );
                    if self.record_pending_stop {
                        self.stop_recording();
                    }
                }
                RecordingEvent::Finished(result) => {
                    self.recording = false;
                    self.recording_starting = false;
                    self.recording_started_at = None;
                    if let Ok(mut recording_stop_tx) = self.recording_stop_tx.lock() {
                        *recording_stop_tx = None;
                    }
                    self.record_pending_stop = false;
                    let should_show_window =
                        should_show_window_after_recording(self.continuous_capture);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(should_show_window));
                    if should_show_window {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    match result {
                        Ok(path) => {
                            let finished_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                            self.status = format!("録画を保存しました ({finished_at})");
                            self.recording_notice = Some((path, finished_at));
                        }
                        Err(error) => self.status = recording_failure_message(&error),
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
        if let Some(manager) = &self._hotkey_manager {
            let previous = [self.capture_hotkey, self.recording_hotkey];
            if let Err(error) = manager.unregister_all(&previous) {
                self.settings_error =
                    Some(format!("現在のショートカットを解除できません: {error}"));
                return;
            }
            if let Err(error) = manager.register_all(&[capture, recording]) {
                let _ = manager.unregister_all(&[capture, recording]);
                let _ = manager.register_all(&previous);
                self.settings_error = Some(format!("ショートカットを登録できません: {error}"));
                return;
            }
        }
        self.capture_hotkey = capture;
        self.recording_hotkey = recording;
        let capture_text = canonical_hotkey_text(&capture.to_string());
        let recording_text = canonical_hotkey_text(&recording.to_string());
        self.capture_hotkey_text = capture_text.clone();
        self.recording_hotkey_text = recording_text.clone();
        match self.save_preferences() {
            Ok(()) => {
                self.settings_error = None;
                self.status = "ショートカットを保存しました。".into();
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
        })
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        } else {
            self.status = "連続撮影を終了しました。".into();
            if !self.capture_in_progress && self.capture_queue.is_empty() {
                self.continuous_capture_session_active = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        self.update_tray_recording_status();
    }

    fn begin_region_selection(&mut self, ctx: &egui::Context) {
        if self.recording || self.recording_starting || self.capture_in_progress {
            self.report_capture_failure("録画または撮影が終わってから範囲撮影を開始してください。");
            return;
        }
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
        #[cfg(target_os = "macos")]
        let (x, y, overlay_width, overlay_height) = (
            monitor.x().unwrap_or(0) as f32,
            monitor.y().unwrap_or(0) as f32,
            width as f32,
            height as f32,
        );
        #[cfg(target_os = "windows")]
        let (x, y, overlay_width, overlay_height) = (
            {
                let scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
                monitor.x().unwrap_or(0) as f32 / scale
            },
            {
                let scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
                monitor.y().unwrap_or(0) as f32 / scale
            },
            {
                let scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
                width as f32 / scale
            },
            {
                let scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
                height as f32 / scale
            },
        );
        self.region_overlay_position = egui::pos2(x, y);
        self.region_overlay_size = egui::vec2(overlay_width, overlay_height);
        self.region_drag_start = None;
        self.region_drag_current = None;
        self.status = "ドラッグして撮影範囲を選択してください。Escでキャンセルできます。".into();
        #[cfg(target_os = "windows")]
        {
            self.region_background_texture = None;
            let sender = self.region_snapshot_tx.clone();
            let repaint = ctx.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            thread::spawn(move || {
                // Capture the desktop after the main window has been hidden. Windows may
                // composite a transparent child viewport as black, so use this image as
                // the selector's opaque background instead of relying on transparency.
                thread::sleep(Duration::from_millis(350));
                let result = capture_target_image(CaptureTarget::PrimaryDisplay);
                let _ = sender.send(result);
                repaint.request_repaint();
            });
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.region_selecting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }
    }

    #[cfg(target_os = "windows")]
    fn handle_region_snapshot(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.region_snapshot_rx.try_recv() {
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
            ctx.request_repaint();
        }
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
        self.last_capture_sequence = sequence;
        let captured_at = Local::now();
        let timestamp = captured_at.format("%Y-%m-%d %H:%M:%S").to_string();
        let file_timestamp = captured_at.format("%Y%m%d-%H%M%S-%3f").to_string();
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        let filename_prefix = self.filename_prefix.clone();
        let timestamp_enabled = self.timestamp_enabled;
        let sender = self.capture_tx.clone();
        let ctx = ctx.clone();
        self.capture_in_progress = true;
        self.status = format!("撮影 {} 枚目を保存しています…", sequence);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.update_tray_recording_status();

        thread::spawn(move || {
            // Give the window manager time to reveal the previously active application.
            thread::sleep(Duration::from_millis(350));
            let capture = capture_target_image(target).and_then(|image| {
                std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
                let path =
                    new_evidence_path(&folder, &filename_prefix, &file_timestamp, sequence, "");
                let mut image = if let Some(bounds) = region {
                    crop_normalized(&image, bounds).ok_or_else(|| {
                        "選択範囲が小さすぎます。ドラッグして範囲を選択してください。".to_string()
                    })?
                } else {
                    image
                };
                if timestamp_enabled {
                    stamp_timestamp(&mut image, &timestamp);
                }
                image.save(&path).map_err(|e| e.to_string())?;
                Ok((path, image, timestamp, sequence))
            });

            let _ = sender.send(capture);
            ctx.request_repaint();
        });
    }

    fn handle_capture_results(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.capture_rx.try_recv() {
            self.capture_in_progress = false;
            match result {
                Ok((path, image, timestamp, sequence)) => {
                    self.status = format!("撮影 {} 枚目を保存しました ({timestamp})", sequence);
                    if self.continuous_capture_session_active {
                        self.continuous_capture_saved_count =
                            self.continuous_capture_saved_count.saturating_add(1);
                    }
                    if self.show_capture_notice && !self.continuous_capture {
                        self.capture_notice = Some(CaptureNotice {
                            sequence,
                            timestamp,
                            path: path.clone(),
                        });
                    }
                    self.editing_queue.clear();
                    self.editing_index = None;
                    self.left_path = Some(path);
                    self.left_image = Some(DynamicImage::ImageRgba8(image));
                    let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                    self.evidence_files = evidence_images(&folder);
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
        if let Err(error) = cropped.save(&path) {
            self.status = format!("切り出し画像を保存できません: {error}");
            return false;
        }
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
        if let Err(error) = output.save(&path) {
            self.status = format!("注釈画像を保存できません: {error}");
            return false;
        }
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
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("撮影対象").size(11.0).color(TEXT_MUTED));
                    ui.label(egui::RichText::new(self.target_caption()).size(15.0).strong());
                });
                egui::ComboBox::from_id_salt("capture_target")
                    .selected_text("対象を選択")
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
                if ui.add(primary_button("範囲を選択")).clicked() {
                    self.begin_region_selection(ui.ctx());
                }
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
                if ui.button("↻  ウィンドウ一覧").clicked() {
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("▣  保存先").clicked() {
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
                        egui::RichText::new("MacShotをオンにして再起動してください。既にオンでも案内が消えない場合は、一度オフ→オンして再起動します。")
                            .size(12.0)
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui.add(primary_button("画面収録設定を開く")).clicked() {
                            self.open_screen_recording_settings();
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

    fn show_evidence_page(&mut self, ui: &mut egui::Ui) {
        let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("撮影済みの証跡").size(24.0).strong());
                ui.label(egui::RichText::new("編集を始めたら、保存して次へで連続作業できます。元画像を残して編集版を保存します。").color(TEXT_MUTED));
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
                }
            });
        });
        ui.add_space(14.0);
        glass_frame().show(ui, |ui| {
            ui.label(egui::RichText::new(folder.display().to_string()).size(11.0).color(TEXT_MUTED));
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
                                        if ui.add(primary_button("連続編集を開始")).clicked() {
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
        ui.label(egui::RichText::new("MacShotを自分の撮影手順に合わせます。").color(TEXT_MUTED));
        ui.add_space(14.0);
        ui.columns(2, |columns| {
            glass_frame().show(&mut columns[0], |ui| {
                ui.label(egui::RichText::new("ショートカット").size(18.0).strong());
                ui.label(
                    egui::RichText::new("どのアプリを使っていても呼び出せます。設定後すぐに反映されます。")
                        .color(TEXT_MUTED),
                );
                ui.add_space(16.0);
                egui::Grid::new("shortcut_settings")
                    .num_columns(2)
                    .spacing([18.0, 14.0])
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
                ui.add_space(10.0);
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
                ui.add_space(14.0);
                if ui.add(primary_button("設定を保存して反映")).clicked() {
                    self.apply_hotkey_settings();
                }
                if let Some(error) = &self.settings_error {
                    ui.colored_label(egui::Color32::from_rgb(188, 51, 73), error);
                }
            });
            glass_frame().show(&mut columns[1], |ui| {
                ui.label(egui::RichText::new("録画と保存先").size(18.0).strong());
                ui.add_space(8.0);
                ui.label("録画は対象ウィンドウのある画面をGIFで保存します。全画面を選ぶとメイン画面を録画します。");
                ui.label(
                    egui::RichText::new("1画面・10fps・音声なし")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
                if cfg!(target_os = "macos") {
                    ui.add_space(10.0);
                    ui.label("macOS標準ツールでは、画面・ウィンドウ・範囲の撮影／録画、タイマー、マイク、ポインタ、クリック表示、保存先、フローティングサムネールを利用できます。");
                    if ui.button("macOS標準の撮影・録画を開く").clicked() {
                        self.open_native_screenshot();
                    }
                }
                ui.add_space(16.0);
                let folder = self.save_folder.clone().unwrap_or_else(default_save_folder);
                ui.label(egui::RichText::new("保存先").size(11.0).color(TEXT_MUTED));
                ui.label(egui::RichText::new(folder.display().to_string()).size(12.0));
                ui.add_space(6.0);
                if ui.button("保存先を変更").clicked() {
                    self.choose_folder();
                }
                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new(format!(
                        "メニューバーからも撮影・録画できます。\n撮影 {}　録画 {}",
                        hotkey_badge_text(&self.capture_hotkey_text),
                        hotkey_badge_text(&self.recording_hotkey_text)
                    ))
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
            });
        });
        ui.add_space(12.0);
        glass_frame().show(ui, |ui| {
            ui.label(egui::RichText::new("撮影の動作").size(18.0).strong());
            ui.label(egui::RichText::new("撮影対象や画像に記録する情報を調整できます。設定は次回起動後も引き継がれます。").color(TEXT_MUTED));
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("既定の撮影対象").size(11.0).color(TEXT_MUTED));
                    egui::ComboBox::from_id_salt("settings_capture_target")
                        .selected_text(self.target_caption())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.capture_target, CaptureTarget::ActiveWindow, "アクティブウィンドウ");
                            ui.selectable_value(&mut self.capture_target, CaptureTarget::PrimaryDisplay, "デスクトップ（メイン画面）");
                            ui.selectable_value(&mut self.capture_target, CaptureTarget::AllDisplays, "全画面（全ディスプレイ）");
                            for (index, monitor) in self.monitors.iter().enumerate() {
                                let name = monitor.name().unwrap_or_else(|_| format!("画面 {}", index + 1));
                                ui.selectable_value(&mut self.capture_target, CaptureTarget::Display(index), format!("画面全体 — {name}"));
                            }
                            for window in &self.windows {
                                ui.selectable_value(&mut self.capture_target, CaptureTarget::Window(window.id), format!("ウィンドウ — {}", window.title));
                            }
                        });
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("ファイル名の先頭").size(11.0).color(TEXT_MUTED));
                    ui.add(egui::TextEdit::singleline(&mut self.filename_prefix).desired_width(180.0));
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.timestamp_enabled, "画像に撮影日時を表示");
                    ui.checkbox(&mut self.show_capture_notice, "撮影後に完了小窓を表示");
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("注釈の色と太さ").size(11.0).color(TEXT_MUTED));
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
                    ui.add(egui::Slider::new(&mut self.annotation_width, 1.0..=8.0).text("太さ"));
                });
                ui.separator();
                if ui.add(primary_button("撮影設定を保存")).clicked() {
                    match self.save_preferences() {
                        Ok(()) => self.status = "撮影設定を保存しました。".into(),
                        Err(error) => self.settings_error = Some(format!("設定を保存できません: {error}")),
                    }
                }
            });
        });
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
        if let Some((path, timestamp)) = &self.recording_notice {
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
        let marked = evidence_sort_key(Path::new("capture-20261025-123456-789-012-marked.png"));

        assert_eq!(original.0, crop.0);
        assert_eq!(original.1, crop.1);
        assert!(original.2 < crop.2);
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
    fn preferences_round_trip_saved_values_and_keep_equals_in_paths() {
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
    fn hotkey_labels_are_canonical_and_badges_ignore_parser_casing() {
        assert_eq!(canonical_hotkey_text("shift+alt+KeyS"), "Alt+Shift+KeyS");
        let alt_label = if cfg!(target_os = "macos") {
            "⌥"
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
            Some([0.25, 0.2, 1.0, 1.0])
        );
        assert_eq!(
            normalized_region_bounds(screen, egui::pos2(0.0, 10.0), egui::pos2(60.0, 50.0)),
            Some([0.0, 0.0, 0.5, 0.6])
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
}
