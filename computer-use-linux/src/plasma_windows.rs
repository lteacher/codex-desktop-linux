use crate::diagnostics::hydrate_session_bus_env;
use crate::terminal::enrich_terminal_windows;
use crate::windows::{WindowBounds, WindowInfo};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{fs, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use zbus::Proxy;

pub const KWIN_SCRIPTING_BACKEND: &str = "kwin-scripting";

// KWin JavaScript template.  {dbus_addr} is replaced at runtime with the calling
// process's unique D-Bus connection name (e.g. ":1.456") so the callDBus reply
// arrives at exactly this connection.
const KWIN_SCRIPT_TEMPLATE: &str = r#"
(function() {
    try {
        var wins = workspace.windowList();
        var out = [];
        for (var i = 0; i < wins.length; i++) {
            var w = wins[i];
            var g = w.frameGeometry;
            out.push({
                id: String(w.internalId),
                caption: w.caption || "",
                pid: w.pid || 0,
                resourceClass: w.resourceClass || "",
                resourceName: w.resourceName || "",
                minimized: !!w.minimized,
                active: !!w.active,
                desktopWindow: !!w.desktopWindow,
                skipTaskbar: !!w.skipTaskbar,
                x: g ? Math.round(g.x) : 0,
                y: g ? Math.round(g.y) : 0,
                width: g ? Math.round(g.width) : 0,
                height: g ? Math.round(g.height) : 0
            });
        }
        callDBus(
            "{dbus_addr}",
            "/KWinCallback",
            "org.kde.Codex.KWinCallback",
            "ReceiveWindowList",
            JSON.stringify(out)
        );
    } catch(e) {
        callDBus(
            "{dbus_addr}",
            "/KWinCallback",
            "org.kde.Codex.KWinCallback",
            "ReceiveError",
            String(e)
        );
    }
})();
"#;

#[derive(Deserialize)]
struct RawWindow {
    id: String,
    caption: String,
    pid: u32,
    #[serde(rename = "resourceClass")]
    resource_class: String,
    minimized: bool,
    active: bool,
    #[serde(rename = "desktopWindow")]
    desktop_window: bool,
    #[serde(rename = "skipTaskbar")]
    skip_taskbar: bool,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct KWinCallback {
    tx: mpsc::UnboundedSender<Result<String>>,
}

#[zbus::interface(name = "org.kde.Codex.KWinCallback")]
impl KWinCallback {
    // zbus converts receive_window_list → ReceiveWindowList (PascalCase) on the wire.
    fn receive_window_list(&self, data: String) {
        let _ = self.tx.send(Ok(data));
    }

    fn receive_error(&self, message: String) {
        let _ = self
            .tx
            .send(Err(anyhow::anyhow!("KWin script error: {message}")));
    }
}

pub async fn list_kwin_windows() -> Result<Vec<WindowInfo>> {
    hydrate_session_bus_env();

    let conn = zbus::Connection::session()
        .await
        .context("failed to connect to session D-Bus")?;

    let unique_name = conn
        .unique_name()
        .context("D-Bus connection has no unique name")?
        .to_string();

    // Register the callback object BEFORE loading the script to avoid a race.
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<String>>();
    conn.object_server()
        .at("/KWinCallback", KWinCallback { tx })
        .await
        .context("failed to register KWin callback object on session bus")?;

    // Write the script to a temp file with our D-Bus address embedded.
    let script_body = KWIN_SCRIPT_TEMPLATE.replace("{dbus_addr}", &unique_name);
    let script_path = write_temp_script(&script_body)?;
    let script_name = format!(
        "codex-windows-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let scripting_proxy = Proxy::new(
        &conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await
    .context("failed to create KWin scripting proxy (is KWin running?)")?;

    // loadScript returns an int32 script ID.
    let script_id: i32 = scripting_proxy
        .call(
            "loadScript",
            &(
                script_path.to_str().unwrap_or("/tmp/codex-kwin.js"),
                script_name.as_str(),
            ),
        )
        .await
        .context("KWin loadScript failed")?;

    let script_obj_path = format!("/Scripting/Script{script_id}");
    let script_proxy = Proxy::new(
        &conn,
        "org.kde.KWin",
        script_obj_path.as_str(),
        "org.kde.kwin.Script",
    )
    .await
    .context("failed to create KWin script proxy")?;

    let _: () = script_proxy
        .call("run", &())
        .await
        .context("KWin script run() failed")?;

    // Wait up to 10 s for the script to call back with window data.
    let json = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .context("timed out waiting for KWin script callback")?
        .context("KWin callback channel closed unexpectedly")?
        .context("KWin script reported an error")?;

    // Clean up: stop script and delete temp file.
    let _: zbus::Result<()> = script_proxy.call("stop", &()).await;
    let _: zbus::Result<bool> = scripting_proxy
        .call("unloadScript", &(script_name.as_str(),))
        .await;
    let _ = fs::remove_file(&script_path);

    let raw: Vec<RawWindow> =
        serde_json::from_str(&json).context("failed to parse KWin window JSON")?;

    let mut windows: Vec<WindowInfo> = raw
        .into_iter()
        .filter(|w| !w.desktop_window && !w.skip_taskbar)
        .map(window_info_from_raw)
        .collect();

    windows.sort_by_key(|w| w.window_id);
    enrich_terminal_windows(&mut windows);
    Ok(windows)
}

fn window_info_from_raw(w: RawWindow) -> WindowInfo {
    let bounds = if w.width > 0 || w.height > 0 {
        Some(WindowBounds {
            x: Some(w.x),
            y: Some(w.y),
            width: w.width,
            height: w.height,
        })
    } else {
        None
    };
    WindowInfo {
        window_id: uuid_to_u64(&w.id),
        title: non_empty(w.caption),
        app_id: non_empty(w.resource_class),
        wm_class: None,
        pid: if w.pid > 0 { Some(w.pid) } else { None },
        bounds,
        workspace: None,
        focused: w.active,
        hidden: w.minimized,
        client_type: None,
        backend: KWIN_SCRIPTING_BACKEND.to_string(),
        terminal: None,
    }
}

// Convert a KWin internalId UUID string to a stable u64.
// The UUID looks like "{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}".
// We take the first 16 hex digits (= 8 bytes) as a u64 in big-endian order.
fn uuid_to_u64(uuid: &str) -> u64 {
    let hex: String = uuid
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(16)
        .collect();
    u64::from_str_radix(&hex, 16).unwrap_or_else(|_| {
        // Fall back to a simple hash of the whole string.
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in uuid.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    })
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn write_temp_script(content: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("codex-kwin-{}.js", std::process::id()));
    fs::write(&path, content)
        .with_context(|| format!("failed to write KWin script to {}", path.display()))?;
    Ok(path)
}

/// Check whether KWin scripting is accessible on the session bus.
/// Used by the doctor report.
pub(crate) fn probe_availability() -> Result<String> {
    hydrate_session_bus_env();
    let conn = zbus::blocking::Connection::session()
        .context("failed to connect to session D-Bus")?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .context("failed to create KWin scripting proxy")?;
    // isScriptLoaded is a harmless read-only call that tells us KWin scripting is alive.
    let _: bool = proxy
        .call("isScriptLoaded", &("__probe__",))
        .context("KWin scripting D-Bus interface not available")?;
    Ok("KWin scripting is available via org.kde.kwin.Scripting".to_string())
}
