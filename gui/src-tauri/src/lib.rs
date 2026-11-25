use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command},
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::{Manager, State};

/// Fixed for now; a free-port search is a reasonable future improvement if a second
/// instance or a port conflict ever becomes a real problem in practice.
const API_PORT: u16 = 8787;

struct ApiProcess(Mutex<Option<Child>>);

#[derive(Clone, serde::Serialize)]
struct ApiConfig {
    base: String,
    token: String,
    #[serde(skip)]
    token_path: PathBuf,
}

/// `CLATTERFORGE_STATE`, then legacy `CONDUCTOR_STATE`, then `<base>/.clatterforge` unless only
/// a legacy `<base>/.conductor` exists (kept in place, never moved).
fn state_dir_in(base: &std::path::Path) -> PathBuf {
    if let Some(explicit) =
        std::env::var_os("CLATTERFORGE_STATE").or_else(|| std::env::var_os("CONDUCTOR_STATE"))
    {
        return PathBuf::from(explicit);
    }
    let current = base.join(".clatterforge");
    let legacy = base.join(".conductor");
    if !current.exists() && legacy.is_dir() {
        legacy
    } else {
        current
    }
}

/// State for a bundle layout (installed or portable): an explicit `CLATTERFORGE_STATE`, else
/// state that already sits beside the executable (portable bundles), else the per-user data
/// folder, so installing, updating or uninstalling the app never touches it (ADR 0018).
fn bundle_state_dir(exe_dir: &std::path::Path) -> PathBuf {
    let beside = state_dir_in(exe_dir);
    let explicit = std::env::var_os("CLATTERFORGE_STATE").is_some()
        || std::env::var_os("CONDUCTOR_STATE").is_some();
    if explicit || beside.exists() {
        return beside;
    }
    user_state_dir().unwrap_or(beside)
}

/// The per-user data folder for installed builds (ADR 0018).
#[cfg(windows)]
fn user_state_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join("Clatterforge").join("state"))
}

/// The per-user data folder for installed builds: `~/Library/Application Support`.
#[cfg(target_os = "macos")]
fn user_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/Clatterforge/state"))
}

/// The per-user data folder for installed builds: `$XDG_DATA_HOME`, else `~/.local/share`.
/// An installed binary sits in `/usr/bin` or a read-only AppImage mount, so state never goes
/// beside it.
#[cfg(all(unix, not(target_os = "macos")))]
fn user_state_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(data.join("clatterforge/state"))
}

/// Resolves the backend CLI executable and state directory (`.clatterforge`, or a legacy `.conductor`).
///
/// Supports two layouts:
/// 1. **Standalone release bundle**: `clatterforge-desktop.exe` sits alongside `clatterforge.exe`
///    (or the pre-rename `dev-conductor.exe`) in the same folder. Uses `CLATTERFORGE_STATE`
///    (or legacy `CONDUCTOR_STATE`) if set, otherwise `<exe_dir>/.clatterforge`, falling back to
///    an existing legacy `<exe_dir>/.conductor`.
/// 2. **Development checkout**: `gui/src-tauri/target/{debug,release}/clatterforge-desktop.exe` five directories
///    below the repo root, using `<repo_root>/target/{release,debug}/clatterforge.exe` (or a
///    pre-rename `dev-conductor.exe`) and the same state-directory rule under the repo root.
fn resolve_backend_and_state() -> Result<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().context("resolve current_exe")?;
    if let Some(exe_dir) = exe.parent() {
        for candidate in [
            "clatterforge.exe",
            "dev-conductor.exe",
            "clatterforge",
            "dev-conductor",
        ] {
            let bin = exe_dir.join(candidate);
            if bin.is_file() {
                return Ok((bin, bundle_state_dir(exe_dir)));
            }
        }
    }

    let root = exe.ancestors().nth(5).map(|p| p.to_path_buf()).context(
        "could not locate sibling clatterforge binary or repo root from running executable",
    )?;
    let state_dir = state_dir_in(&root);
    // EXE_SUFFIX keeps the development layout working on Linux/macOS, where the core
    // binary has no ".exe". When both profiles exist, use whichever was built last so a
    // stale release build does not shadow the debug binary being worked on.
    // Prefer the renamed binary; fall back to a pre-rename build if that is all there is.
    let named = |stem: &str| format!("{stem}{}", std::env::consts::EXE_SUFFIX);
    let core = ["clatterforge", "dev-conductor"]
        .into_iter()
        .map(named)
        .find(|name| {
            root.join("target/release").join(name).is_file()
                || root.join("target/debug").join(name).is_file()
        })
        .unwrap_or_else(|| named("clatterforge"));
    let release_bin = root.join("target/release").join(&core);
    let debug_bin = root.join("target/debug").join(&core);
    let built = |path: &PathBuf| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let binary = match (built(&release_bin), built(&debug_bin)) {
        (Some(release), Some(debug)) if debug > release => debug_bin,
        (Some(_), _) => release_bin,
        _ => debug_bin,
    };
    Ok((binary, state_dir))
}

/// True if something is already listening on `port`.
fn server_already_running(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Verifies that `token` actually authenticates against the server on `127.0.0.1:<port>`
/// before reusing it. Without this check, launching a standalone `dist/` bundle while
/// another state root's managed host is already bound to `8787` would read a stale token
/// from the bundle's own `<state>/serve-token` and hit 401 on every request.
fn token_works_on_port(port: u16, token: &str) -> bool {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return false;
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
    let req = format!(
        "GET /api/tasks HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {trimmed}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let Ok(n) = stream.read(&mut buf) else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .context("bind ephemeral loopback port")?;
    Ok(listener.local_addr()?.port())
}

/// Spawns `clatterforge serve` against the resolved state directory, waits for it to
/// write its fresh bearer token on disk, and hands back what the frontend needs to call it.
///
/// If a server is already running on `API_PORT` AND accepts `state_dir/serve-token`, reuses
/// it without managing its process lifecycle. If `API_PORT` is occupied by a *different*
/// instance (whose token does not match `state_dir/serve-token`), automatically selects a
/// free loopback port so this standalone desktop app starts cleanly with zero 401s or port
/// collisions.
fn desktop_log(
    state_dir: &std::path::Path,
    level: &str,
    event: &str,
    message: &str,
    fields: serde_json::Value,
) {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let record = serde_json::json!({
        "ts_ms": ts_ms,
        "level": level,
        "component": "desktop",
        "event": event,
        "pid": std::process::id(),
        "message": message,
        "fields": fields,
    });
    if let Ok(line) = serde_json::to_string(&record) {
        let log_dir = state_dir.join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("clatterforge.jsonl"))
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn spawn_api(app: &tauri::AppHandle) -> Result<ApiConfig> {
    let (binary, state_dir) = resolve_backend_and_state()?;
    let token_path = state_dir.join("serve-token");

    let target_port = if server_already_running(API_PORT) {
        if let Ok(token) = std::fs::read_to_string(&token_path)
            && token_works_on_port(API_PORT, &token)
        {
            log::info!("clatterforge serve already running on {API_PORT}; reusing it");
            desktop_log(
                &state_dir,
                "info",
                "backend.reuse",
                "reusing running backend",
                serde_json::json!({ "port": API_PORT, "state_root": state_dir.display().to_string() }),
            );
            return Ok(ApiConfig {
                base: format!("http://127.0.0.1:{API_PORT}"),
                token: token.trim().to_string(),
                token_path,
            });
        }
        let free = pick_free_port()?;
        log::info!(
            "port {API_PORT} is owned by another instance; spawning dedicated backend on {free}"
        );
        desktop_log(
            &state_dir,
            "info",
            "backend.port_fallback",
            "default port owned by another instance; using free port",
            serde_json::json!({ "default_port": API_PORT, "selected_port": free }),
        );
        free
    } else {
        API_PORT
    };

    ensure!(
        binary.is_file(),
        "core binary not found at {}; place clatterforge.exe next to clatterforge-desktop.exe or run `cargo build` from the repo root",
        binary.display()
    );
    let _ = std::fs::create_dir_all(&state_dir);
    // Preserve any existing `<state>/serve-token` so the spawned server reuses it; the readiness
    // loop below still verifies `token_works_on_port(target_port, &token)` against the live socket.

    let mut cmd = Command::new(&binary);
    let log_level = std::env::var("CLATTERFORGE_LOG")
        .or_else(|_| std::env::var("CONDUCTOR_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    cmd.arg("--state")
        .arg(&state_dir)
        .arg("--log-level")
        .arg(&log_level)
        .arg("serve")
        .arg("--port")
        .arg(target_port.to_string());

    // If a compiled `ui/index.html` ships with the app, serve it on the same origin so
    // Tailscale/LAN browsers get unified UI + SameSite=Strict cookies. Installed bundles keep
    // it in the resource folder (beside the app on Windows, `/usr/lib/Clatterforge` on Linux);
    // a portable bundle keeps it beside the backend.
    let bundled_ui = app
        .path()
        .resource_dir()
        .ok()
        .map(|dir| dir.join("ui"))
        .into_iter()
        .chain(binary.parent().map(|dir| dir.join("ui")))
        .find(|dir| dir.join("index.html").is_file());
    if let Some(ui) = bundled_ui {
        cmd.arg("--ui-assets").arg(ui);
    }

    // Stay loopback-only by default. If CLATTERFORGE_BIND is set, honor it; otherwise
    // bind 0.0.0.0 only when `<state>/http-access.json` explicitly configures remote hosts.
    let bind_addr = std::env::var("CLATTERFORGE_BIND").ok().or_else(|| {
        let access_path = state_dir.join("http-access.json");
        let raw = std::fs::read_to_string(access_path).ok()?;
        let body = raw.trim_start_matches('\u{feff}');
        let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
        let has_http = parsed
            .get("trusted_http_hosts")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        let has_https = parsed
            .get("https_origins")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        (has_http || has_https).then(|| "0.0.0.0".to_string())
    });
    if let Some(bind) = bind_addr {
        cmd.arg("--bind").arg(bind);
    }

    let log_dir = state_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let stderr_stdio = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("backend-stderr.log"))
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_stdio);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context("failed to spawn clatterforge serve")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    let token = loop {
        if let Ok(token) = std::fs::read_to_string(&token_path)
            && !token.trim().is_empty()
            && token_works_on_port(target_port, &token)
        {
            break token.trim().to_string();
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!("clatterforge serve on port {target_port} exited early with status {status}");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("clatterforge serve did not become ready on port {target_port} within 10s");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    app.manage(ApiProcess(Mutex::new(Some(child))));
    Ok(ApiConfig {
        base: format!("http://127.0.0.1:{target_port}"),
        token,
        token_path,
    })
}

#[tauri::command]
fn get_api_config(config: State<ApiConfig>) -> std::result::Result<ApiConfig, String> {
    let token = std::fs::read_to_string(&config.token_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| config.token.clone());
    Ok(ApiConfig {
        base: config.base.clone(),
        token,
        token_path: config.token_path.clone(),
    })
}

/// True when this build carries both an update public key and an endpoint. Release builds
/// get them from `scripts/release/build-installer.ps1`; development builds never check.
fn updates_configured(app: &tauri::AppHandle) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .is_some_and(|updater| {
            updater["pubkey"]
                .as_str()
                .is_some_and(|key| !key.trim().is_empty())
                && updater["endpoints"]
                    .as_array()
                    .is_some_and(|endpoints| !endpoints.is_empty())
        })
}

/// Checks the signed release manifest, asks before installing, then stops the backend so
/// its executable can be replaced, installs and restarts on the new version. Anything that
/// fails to verify against the embedded public key is refused by the updater itself.
async fn check_for_update(app: tauri::AppHandle) -> Result<()> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    use tauri_plugin_updater::UpdaterExt;

    if !updates_configured(&app) {
        return Ok(());
    }
    let Some(update) = app.updater()?.check().await? else {
        return Ok(());
    };
    let (answer_tx, answer_rx) = std::sync::mpsc::channel();
    app.dialog()
        .message(format!(
            "Clatterforge {} is available (this is {}). Install it now? The app restarts on \
             the new version; your forge state is kept.",
            update.version, update.current_version
        ))
        .title("Update available")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Install and restart".into(),
            "Later".into(),
        ))
        .show(move |accepted| {
            let _ = answer_tx.send(accepted);
        });
    let accepted = tauri::async_runtime::spawn_blocking(move || answer_rx.recv().unwrap_or(false))
        .await
        .unwrap_or(false);
    if !accepted {
        return Ok(());
    }
    let bytes = update.download(|_, _| {}, || {}).await?;
    if let Some(state) = app.try_state::<ApiProcess>()
        && let Some(mut child) = state.0.lock().unwrap().take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
    update.install(bytes)?;
    app.restart();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            let config = spawn_api(app.handle())?;
            app.manage(config);
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = check_for_update(handle).await {
                    log::warn!("update check failed: {error:#}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_api_config])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event
                && let Some(state) = app_handle.try_state::<ApiProcess>()
                && let Some(mut child) = state.0.lock().unwrap().take()
            {
                let _ = child.kill();
            }
        });
}
