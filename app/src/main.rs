// PathoSync – records up to 4 microphones at once with push-to-talk, saving one mixed file and
// one multitrack file per recording. This file is the desktop shell: settings, keys, commands
// for the screen (ui/), and launch options for opening it from other software:
//
//   PathoSync.exe --case S26-1234 --folder "D:\Recordings" --exit-after-save

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use pathosync_core::{list_input_devices, DeviceInfo, Engine, EngineConfig, EngineStatus, MicConfig, RecordingResult};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const MAX_MICS: usize = 4;
const SETTINGS_FILE: &str = "PathoSync.settings.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct MicSlot {
    enabled: bool,
    name: String,
    device_id: String,
    device_name: String,
    input_channel: u16,
    /// Push-to-talk key, e.g. "F1" or "Control+Shift+KeyA" (a mic's own button can send a key).
    key: String,
    gain_db: f32,
    offset_ms: f32,
}

impl Default for MicSlot {
    fn default() -> Self {
        Self { enabled: false, name: String::new(), device_id: String::new(), device_name: String::new(), input_channel: 0, key: String::new(), gain_db: 0.0, offset_ms: 0.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    save_folder: String,
    preroll_ms: u32,
    tail_ms: u32,
    mics: Vec<MicSlot>,
}

impl Default for Settings {
    fn default() -> Self {
        let docs = dirs_documents().join("PathoSync Recordings");
        Self {
            save_folder: docs.to_string_lossy().into_owned(),
            preroll_ms: 500,
            tail_ms: 300,
            mics: (0..MAX_MICS).map(|i| MicSlot { name: format!("Mic {}", i + 1), key: format!("F{}", i + 1), ..Default::default() }).collect(),
        }
    }
}

impl Settings {
    fn normalized(mut self) -> Self {
        self.mics.truncate(MAX_MICS);
        while self.mics.len() < MAX_MICS {
            let i = self.mics.len();
            self.mics.push(MicSlot { name: format!("Mic {}", i + 1), key: format!("F{}", i + 1), ..Default::default() });
        }
        self.preroll_ms = self.preroll_ms.min(3000);
        self.tail_ms = self.tail_ms.min(3000);
        for m in &mut self.mics {
            m.name = m.name.trim().chars().take(40).collect();
            m.gain_db = m.gain_db.clamp(-24.0, 24.0);
            m.offset_ms = m.offset_ms.clamp(0.0, 500.0);
        }
        self
    }

    /// Slots that take part in recording, in order (their position = multitrack channel).
    fn active(&self) -> Vec<usize> {
        self.mics.iter().enumerate().filter(|(_, m)| m.enabled && !m.device_id.is_empty()).map(|(i, _)| i).collect()
    }
}

fn dirs_documents() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Documents")
}

/// Settings live next to the program (so a copied PathoSync folder keeps its setup);
/// if that folder is read-only (e.g. Program Files) they go to the user's AppData.
fn settings_path() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let probe = dir.join(".pathosync-write-test");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return dir.join(SETTINGS_FILE);
        }
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    let dir = base.unwrap_or_else(|| PathBuf::from(".")).join("PathoSync");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(SETTINGS_FILE)
}

#[derive(Clone, Debug, Default, Serialize)]
struct LaunchArgs {
    case: Option<String>,
    folder: Option<String>,
    exit_after_save: bool,
}

fn parse_args() -> LaunchArgs {
    let mut a = LaunchArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--case" => a.case = it.next(),
            "--folder" => a.folder = it.next(),
            "--exit-after-save" => a.exit_after_save = true,
            _ => {}
        }
    }
    a
}

struct App {
    /// Shared so slow calls (saving a recording) never block the screen's status updates.
    engine: Mutex<Option<Arc<Engine>>>,
    engine_error: Mutex<Option<String>>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    launch: LaunchArgs,
    /// shortcut → settings slot
    shortcuts: Mutex<Vec<(Shortcut, usize)>>,
    key_warnings: Mutex<Vec<String>>,
}

impl App {
    fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.lock().clone()
    }

    /// Engine channel of a settings slot (only enabled slots with a device are recorded).
    fn channel_of(&self, slot: usize) -> Option<usize> {
        self.settings.lock().active().iter().position(|&s| s == slot)
    }

    fn ptt(&self, slot: usize, pressed: bool) {
        if let Some(ch) = self.channel_of(slot) {
            if let Some(e) = self.engine() {
                e.ptt(ch, pressed);
            }
        }
    }

    fn save_settings_file(&self) -> Result<(), String> {
        let s = self.settings.lock().clone();
        let json = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
        std::fs::write(&self.settings_path, json).map_err(|e| format!("Could not save settings to {}: {e}", self.settings_path.display()))
    }

    /// (Re)opens the microphones with the current settings.
    fn restart_engine(&self) {
        let mut engine = self.engine.lock();
        *engine = None; // closes the previous devices first
        let s = self.settings.lock().clone();
        let active = s.active();
        if active.is_empty() {
            *self.engine_error.lock() = Some("No microphone is switched on. Open Setup to choose your microphones.".into());
            return;
        }
        let cfg = EngineConfig {
            mics: active
                .iter()
                .map(|&i| {
                    let m = &s.mics[i];
                    MicConfig {
                        name: if m.name.is_empty() { format!("Mic {}", i + 1) } else { m.name.clone() },
                        device_id: m.device_id.clone(),
                        device_name: m.device_name.clone(),
                        input_channel: m.input_channel,
                        gain_db: m.gain_db,
                        offset_ms: m.offset_ms,
                    }
                })
                .collect(),
            preroll_ms: s.preroll_ms,
            tail_ms: s.tail_ms,
        };
        match Engine::start(cfg) {
            Ok(e) => {
                *engine = Some(Arc::new(e));
                *self.engine_error.lock() = None;
            }
            Err(e) => *self.engine_error.lock() = Some(e),
        }
    }

    fn register_keys(&self, app: &AppHandle) {
        let gs = app.global_shortcut();
        let _ = gs.unregister_all();
        let mut map = self.shortcuts.lock();
        map.clear();
        let mut warnings = Vec::new();
        let s = self.settings.lock().clone();
        for i in s.active() {
            let m = &s.mics[i];
            if m.key.trim().is_empty() {
                continue;
            }
            let label = if m.name.is_empty() { format!("Mic {}", i + 1) } else { m.name.clone() };
            match m.key.parse::<Shortcut>() {
                Ok(sc) => {
                    if map.iter().any(|(x, _)| *x == sc) {
                        warnings.push(format!("{label}: the key {} is already used by another mic.", m.key));
                        continue;
                    }
                    match gs.register(sc) {
                        Ok(()) => map.push((sc, i)),
                        Err(e) => warnings.push(format!("{label}: the key {} could not be used ({e}). Another program may be using it.", m.key)),
                    }
                }
                Err(_) => warnings.push(format!("{label}: \"{}\" is not a key PathoSync can use.", m.key)),
            }
        }
        *self.key_warnings.lock() = warnings;
    }
}

#[derive(Serialize)]
struct StateOut {
    settings: Settings,
    devices: Vec<DeviceInfo>,
    device_error: Option<String>,
    launch: LaunchArgs,
    settings_path: String,
    engine_error: Option<String>,
    key_warnings: Vec<String>,
    active: Vec<usize>,
    version: &'static str,
}

fn state_out(app: &App) -> StateOut {
    let (devices, device_error) = match list_input_devices() {
        Ok(d) => (d, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let settings = app.settings.lock().clone();
    StateOut {
        active: settings.active(),
        settings,
        devices,
        device_error,
        launch: app.launch.clone(),
        settings_path: app.settings_path.to_string_lossy().into_owned(),
        engine_error: app.engine_error.lock().clone(),
        key_warnings: app.key_warnings.lock().clone(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
fn get_state(app: State<'_, App>) -> StateOut {
    state_out(&app)
}

#[tauri::command(async)]
fn save_settings(handle: AppHandle, app: State<'_, App>, settings: Settings) -> Result<StateOut, String> {
    if app.engine().is_some_and(|e| e.is_recording()) {
        return Err("Stop the recording before changing the setup.".into());
    }
    *app.settings.lock() = settings.normalized();
    app.save_settings_file()?;
    app.restart_engine();
    app.register_keys(&handle);
    Ok(state_out(&app))
}

#[derive(Serialize)]
struct StatusOut {
    #[serde(flatten)]
    engine: EngineStatus,
    running: bool,
    /// settings slot of each engine channel
    slots: Vec<usize>,
}

#[tauri::command]
fn status(app: State<'_, App>) -> StatusOut {
    let slots = app.settings.lock().active();
    match app.engine() {
        Some(e) => StatusOut { engine: e.status(), running: true, slots },
        None => StatusOut { engine: EngineStatus::default(), running: false, slots },
    }
}

#[tauri::command]
fn ptt(app: State<'_, App>, slot: usize, pressed: bool) {
    app.ptt(slot, pressed);
}

#[tauri::command(async)]
fn start_recording(app: State<'_, App>, case: String) -> Result<String, String> {
    let folder = app.launch.folder.clone().unwrap_or_else(|| app.settings.lock().save_folder.clone());
    if folder.trim().is_empty() {
        return Err("Choose a folder to save recordings in (Setup).".into());
    }
    let e = app.engine().ok_or_else(|| app.engine_error.lock().clone().unwrap_or_else(|| "The microphones are not ready.".into()))?;
    e.start_recording(Path::new(&folder), &case).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command(async)]
fn stop_recording(app: State<'_, App>) -> Result<RecordingResult, String> {
    let e = app.engine().ok_or("The microphones are not ready.")?;
    e.stop_recording()
}

#[tauri::command(async)]
fn pick_folder(handle: AppHandle, app: State<'_, App>) -> Option<String> {
    let current = app.settings.lock().save_folder.clone();
    let mut dlg = handle.dialog().file().set_title("Where should PathoSync save recordings?");
    if Path::new(&current).is_dir() {
        dlg = dlg.set_directory(&current);
    }
    dlg.blocking_pick_folder().and_then(|f| f.into_path().ok()).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("That folder no longer exists.".into());
    }
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(&p).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(&p).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(&p).spawn();
    r.map(|_| ()).map_err(|e| e.to_string())
}

/// Turns the push-to-talk keys off while the user picks a new key in Setup.
#[tauri::command]
fn pause_keys(handle: AppHandle, app: State<'_, App>, paused: bool) {
    if paused {
        let _ = handle.global_shortcut().unregister_all();
        app.shortcuts.lock().clear();
    } else {
        app.register_keys(&handle);
    }
}

#[tauri::command]
fn check_key(key: String) -> Result<(), String> {
    key.parse::<Shortcut>().map(|_| ()).map_err(|_| format!("\"{key}\" can't be used as a push-to-talk key."))
}

#[tauri::command]
fn clap_test_start(app: State<'_, App>) -> Result<(), String> {
    let e = app.engine().ok_or("The microphones are not ready.")?;
    if e.config().mics.len() < 2 {
        return Err("The clap test needs at least two microphones switched on.".into());
    }
    if e.is_recording() {
        return Err("Stop the recording first.".into());
    }
    e.start_clap_test(4.0);
    Ok(())
}

/// When the clap test is done: saves each mic's delay and reopens the mics. `None` = still running.
#[tauri::command(async)]
fn clap_test_result(handle: AppHandle, app: State<'_, App>) -> Result<Option<StateOut>, String> {
    let result = match app.engine() {
        Some(e) => e.clap_test_result(),
        None => return Err("The microphones are not ready.".into()),
    };
    let Some(result) = result else { return Ok(None) };
    let offsets = result?;
    {
        let mut s = app.settings.lock();
        let active = s.active();
        for (k, &slot) in active.iter().enumerate() {
            if let Some(ms) = offsets.get(k) {
                s.mics[slot].offset_ms = (*ms * 10.0).round() / 10.0;
            }
        }
    }
    app.save_settings_file()?;
    app.restart_engine();
    app.register_keys(&handle);
    Ok(Some(state_out(&app)))
}

#[tauri::command]
fn exit_app(handle: AppHandle) {
    handle.exit(0);
}

fn main() {
    let settings_path = settings_path();
    let mut settings: Settings = std::fs::read_to_string(&settings_path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    settings = settings.normalized();
    // First start: use the computer's default microphone as Mic 1 so it works straight away.
    if !settings_path.exists() {
        if let Ok(devs) = list_input_devices() {
            if let Some(d) = devs.iter().find(|d| d.is_default).or(devs.first()) {
                let m = &mut settings.mics[0];
                m.enabled = true;
                m.device_id = d.id.clone();
                m.device_name = d.name.clone();
            }
        }
    }
    let app_state = App {
        engine: Mutex::new(None),
        engine_error: Mutex::new(None),
        settings: Mutex::new(settings),
        settings_path,
        launch: parse_args(),
        shortcuts: Mutex::new(Vec::new()),
        key_warnings: Mutex::new(Vec::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|handle, shortcut, event| {
                    let app = handle.state::<App>();
                    let slot = app.shortcuts.lock().iter().find(|(s, _)| s == shortcut).map(|(_, i)| *i);
                    if let Some(slot) = slot {
                        app.ptt(slot, event.state == ShortcutState::Pressed);
                    }
                })
                .build(),
        )
        .manage(app_state)
        .setup(|app| {
            let state = app.state::<App>();
            let _ = state.save_settings_file();
            state.restart_engine();
            state.register_keys(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_settings,
            status,
            ptt,
            start_recording,
            stop_recording,
            pick_folder,
            open_folder,
            pause_keys,
            check_key,
            clap_test_start,
            clap_test_result,
            exit_app
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // closing the window saves a running recording (Engine::drop)
                let app = window.app_handle().state::<App>();
                *app.engine.lock() = None;
            }
        })
        .run(tauri::generate_context!())
        .expect("PathoSync could not start");
}
