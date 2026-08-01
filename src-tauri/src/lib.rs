use tauri::Manager;
use tauri::WindowEvent;

/* ============ Ollama bootstrap ============
   Detecting, installing and starting Ollama is the one thing the webview cannot do for
   itself: it needs to look at the filesystem and spawn processes. Everything else in this
   app talks to Ollama's HTTP API directly from JS, and deliberately stays that way.

   Windows only for now. The non-Windows stubs exist so `generate_handler!` compiles
   unchanged; they report "unsupported" and the frontend hides the setup UI entirely. */

// CREATE_NO_WINDOW. Every spawn below carries it, which is the whole reason the install
// runs without a console window flashing up in the user's face.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
// CREATE_NEW_PROCESS_GROUP: keeps `ollama serve` out of our Ctrl+C / console group, so it
// outlives this app rather than dying with it. Closing DeskOllama must never yank the
// server out from under anything else using it.
#[cfg(target_os = "windows")]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Pulls the percentage out of one of install.ps1's progress lines.
///
/// The line looks like `[####------] 42.5%`, so this takes the last whitespace-separated
/// token and only accepts it when it ends in `%` and parses cleanly. Deliberately strict:
/// a wrong guess here would drive a progress bar that lies, and returning None simply leaves
/// the previous reading on screen.
#[cfg(target_os = "windows")]
fn parse_percent(line: &str) -> Option<f64> {
    let tok = line.split_whitespace().last()?;
    let num = tok.strip_suffix('%')?;
    let v: f64 = num.parse().ok()?;
    if (0.0..=100.0).contains(&v) {
        Some(v)
    } else {
        None
    }
}

/// Where Ollama keeps its models. `OLLAMA_MODELS` wins when set, which is the documented way
/// to move them off the system drive; otherwise the default under the home directory.
#[cfg(target_os = "windows")]
fn models_dir() -> Option<std::path::PathBuf> {
    if let Ok(custom) = std::env::var("OLLAMA_MODELS") {
        if !custom.trim().is_empty() {
            return Some(std::path::PathBuf::from(custom));
        }
    }
    let home = std::env::var("USERPROFILE").ok()?;
    Some(std::path::Path::new(&home).join(".ollama").join("models"))
}

/// Digests as they appear in blob filenames: bare hex, no `sha256:` prefix.
#[cfg(target_os = "windows")]
fn bare_digest(d: &str) -> String {
    d.trim().trim_start_matches("sha256:").trim_start_matches("sha256-").to_string()
}

/// Every digest referenced by a manifest already on disk. That is, every blob belonging to
/// a model the user actually has installed.
///
/// This is the authority for what must never be deleted. It is the same set Ollama's own
/// cleanup consults, and it is what makes cancelling one download safe when several models
/// share a layer.
#[cfg(target_os = "windows")]
fn manifest_digests() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Some(root) = models_dir().map(|d| d.join("manifests")) else {
        return out;
    };
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if let Some(d) = json.get("config").and_then(|c| c.get("digest")).and_then(|d| d.as_str()) {
                out.insert(bare_digest(d));
            }
            if let Some(layers) = json.get("layers").and_then(|l| l.as_array()) {
                for l in layers {
                    if let Some(d) = l.get("digest").and_then(|d| d.as_str()) {
                        out.insert(bare_digest(d));
                    }
                }
            }
        }
    }
    out
}

/// Where Ollama's own installer puts the binary, per Ollama's Windows docs.
#[cfg(target_os = "windows")]
fn default_ollama_exe() -> Option<std::path::PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let p = std::path::Path::new(&local)
        .join("Programs")
        .join("Ollama")
        .join("ollama.exe");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Fallback for a non-default install location: ask Windows where `ollama` resolves on PATH.
#[cfg(target_os = "windows")]
fn ollama_on_path() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("where")
        .arg("ollama")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Is Ollama installed, and where? Says nothing about whether the server is running:
/// the frontend answers that by probing the HTTP API it already uses.
#[tauri::command]
fn ollama_detect() -> serde_json::Value {
    #[cfg(target_os = "windows")]
    {
        let exe = default_ollama_exe()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(ollama_on_path);
        serde_json::json!({ "supported": true, "installed": exe.is_some(), "exe": exe })
    }
    #[cfg(not(target_os = "windows"))]
    {
        serde_json::json!({ "supported": false, "installed": false, "exe": null })
    }
}

/// Installs Ollama using its own official install script.
///
/// Deliberately not our own download-and-run: the script is maintained by Ollama, verifies
/// the installer's digital signature before executing it, resolves the current version, and
/// installs per-user so no admin prompt appears. Re-implementing that would mean owning all
/// of it, badly.
///
/// `-ExecutionPolicy Bypass` applies to this one process and changes no machine state.
/// Lines the script prints beginning with ">>>" are forwarded as stage labels; that parsing
/// is best-effort by design, and a miss just leaves the generic label showing.
#[tauri::command]
async fn ollama_install(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::io::{BufRead, BufReader};
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use tauri::Emitter;

        tauri::async_runtime::spawn_blocking(move || {
            let mut child = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    "irm https://ollama.com/install.ps1 | iex",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("Could not start the installer: {e}"))?;

            /* Read on BOTH \r and \n, which is the whole reason this progress works.
               install.ps1 draws its download bar with `Write-Host -NoNewline "\r$bar $pct%"`,
               so every update is a carriage return with no newline behind it. Reading with
               .lines() yields nothing at all until the download ends, which is exactly why
               the bar sat frozen through a 1.5 GB download. */
            if let Some(out) = child.stdout.take() {
                let mut reader = BufReader::new(out);
                let mut chunk: Vec<u8> = Vec::new();
                loop {
                    chunk.clear();
                    // read_until stops on \n; \r is split out of whatever it hands back.
                    let n = reader.read_until(b'\n', &mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&chunk).to_string();
                    for piece in text.split(['\r', '\n']) {
                        let t = piece.trim();
                        if t.is_empty() {
                            continue;
                        }
                        if t.starts_with(">>>") {
                            let _ = app.emit(
                                "ollama-install-stage",
                                t.trim_start_matches('>').trim().to_string(),
                            );
                        } else if let Some(pct) = parse_percent(t) {
                            let _ = app.emit("ollama-install-percent", pct);
                        }
                    }
                }
            }

            // Only read once the pipe is drained, so a chatty install cannot deadlock on a
            // full stderr buffer while we are still reading stdout.
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }

            let status = child
                .wait()
                .map_err(|e| format!("Installer did not finish: {e}"))?;

            /* An exit code is not evidence here.
               A script run through `iex` can fail on almost any line and still leave
               powershell.exe exiting 0, because those errors are non-terminating. Trusting
               the code is how a failed install got reported as a success and then confused
               everyone downstream with "installed but did not start".

               The only thing worth believing is the binary being on disk. */
            if default_ollama_exe().is_some() || ollama_on_path().is_some() {
                return Ok(());
            }
            let detail = err.trim();
            let detail = if detail.is_empty() {
                if status.success() {
                    String::from("the installer reported no error, but Ollama is not on disk")
                } else {
                    format!("installer exited with code {:?}", status.code())
                }
            } else {
                detail.chars().take(300).collect()
            };
            Err(format!("Ollama could not be installed: {detail}"))
        })
        .await
        .map_err(|e| format!("Installer task failed: {e}"))?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Err(String::from(
            "Automatic install is only supported on Windows right now.",
        ))
    }
}

/// Starts `ollama serve` in the background. Returns as soon as it is spawned; the frontend
/// decides it is ready by polling the API, which is the only thing that actually proves it.
#[tauri::command]
fn ollama_start(exe: Option<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let bin = exe
            .filter(|s| !s.trim().is_empty())
            .or_else(|| default_ollama_exe().map(|p| p.to_string_lossy().to_string()))
            .or_else(ollama_on_path)
            .ok_or_else(|| String::from("Ollama is not installed."))?;

        Command::new(bin)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(|_| ())   // handle dropped on purpose: we never wait on it, and never kill it
            .map_err(|e| format!("Could not start Ollama: {e}"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = exe;
        Err(String::from(
            "Starting Ollama is only supported on Windows right now.",
        ))
    }
}

/// Fetches a page from ollama.com and hands back the raw body.
///
/// This exists only because ollama.com sends no CORS headers, so the webview cannot read it
/// directly. Parsing happens in JS with the webview's own DOMParser, which is why no HTML
/// parsing crate is needed here.
///
/// The host check is the important line: without it this command would be an open proxy that
/// any script running in the webview could point anywhere.
///
/// `hx` sends the `HX-Request` header. ollama.com is an htmx site, and its search results
/// paginate ONLY for that header: without it every `?page=N` returns page one, which looks
/// exactly like "there is no pagination" and is why this was first written off as a 20-result
/// ceiling. With it, `?page=N` walks the whole result set.
#[tauri::command]
async fn fetch_page(url: String, hx: Option<bool>) -> Result<String, String> {
    // registry.ollama.ai is what `ollama pull` itself talks to, and is the only place exact
    // sizes and quantization can be read from. The allowlist is still the point: without it
    // this command would be an open proxy for anything running in the webview.
    if !(url.starts_with("https://ollama.com/") || url.starts_with("https://registry.ollama.ai/")) {
        return Err(String::from("Only ollama.com and its registry can be fetched."));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("DeskOllama")
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    if hx.unwrap_or(false) {
        req = req.header("HX-Request", "true");
    }
    let r = req.send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status().as_u16()));
    }
    r.text().await.map_err(|e| e.to_string())
}

/* ============ Ollama transport ============
   Every request to Ollama goes through here rather than through the webview's fetch, and the
   reason is CORS.

   Ollama's default allowed origins are localhost, 127.0.0.1, 0.0.0.0, plus app://, file://,
   tauri:// and the vscode schemes. On Windows a Tauri webview serves from
   `http://tauri.localhost`, which matches none of them: the wildcarded `tauri:` entry covers
   macOS and Linux only. So on a clean Windows machine the browser blocks every call the app
   makes, and the app looks offline next to a perfectly healthy server.

   The usual workaround is to have the user set OLLAMA_ORIGINS. That mutates their machine,
   needs the server restarted, has to be re-checked every launch, and breaks silently if
   anything clears it. Going through Rust removes the whole class of problem instead: native
   HTTP has no origin, so no policy applies and there is nothing to keep in sync. */
fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    // Deliberately no global timeout: a long generation is a normal, healthy stream.
    // Non-streaming callers pass their own.
    C.get_or_init(|| reqwest::Client::builder().build().unwrap_or_default())
}

/// In-flight streams, so the frontend can stop one by id.
type Cancels = std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>,
>;
fn cancels() -> &'static Cancels {
    static C: std::sync::OnceLock<Cancels> = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// One non-streaming request. Returns the status and raw body rather than erroring on a
/// non-2xx, because callers already handle Ollama's error bodies themselves.
#[tauri::command]
async fn ollama_fetch(
    url: String,
    method: String,
    body: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<serde_json::Value, String> {
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut req = http().request(m, &url);
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b);
    }
    if let Some(ms) = timeout_ms {
        req = req.timeout(std::time::Duration::from_millis(ms));
    }
    let r = req.send().await.map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    Ok(serde_json::json!({
        "ok": (200..300).contains(&status),
        "status": status,
        "body": text
    }))
}

/// Streams an NDJSON endpoint (`/api/chat`, `/api/pull`), emitting one complete line at a
/// time over a channel. Splitting lines here rather than in JS means the partial-chunk
/// buffering exists once, in one place.
#[tauri::command]
async fn ollama_stream(
    id: String,
    url: String,
    body: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
) -> Result<(), String> {
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    cancels()
        .lock()
        .map_err(|_| "stream registry poisoned")?
        .insert(id.clone(), notify.clone());

    let result = stream_ndjson(&url, body, &on_event, &notify).await;

    if let Ok(mut map) = cancels().lock() {
        map.remove(&id);
    }
    result
}

async fn stream_ndjson(
    url: &str,
    body: String,
    ch: &tauri::ipc::Channel<serde_json::Value>,
    notify: &tokio::sync::Notify,
) -> Result<(), String> {
    let mut resp = http()
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        let _ = ch.send(serde_json::json!({ "kind": "http", "status": status, "body": text }));
        return Ok(());
    }

    let mut buf: Vec<u8> = Vec::new();
    loop {
        /* select! rather than polling a flag between chunks: a chat request can sit silent
           for seconds while the prompt is evaluated, and a Stop pressed in that window has
           to take effect now, not whenever the first token happens to arrive. */
        let chunk = tokio::select! {
            _ = notify.notified() => {
                let _ = ch.send(serde_json::json!({ "kind": "aborted" }));
                return Ok(());
            }
            c = resp.chunk() => c,
        };
        match chunk {
            Ok(Some(bytes)) => {
                buf.extend_from_slice(&bytes);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let s = String::from_utf8_lossy(&line).trim().to_string();
                    if !s.is_empty() {
                        let _ = ch.send(serde_json::json!({ "kind": "line", "line": s }));
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = ch.send(serde_json::json!({ "kind": "error", "message": e.to_string() }));
                return Ok(());
            }
        }
    }
    // Whatever is left with no trailing newline is still a line.
    let tail = String::from_utf8_lossy(&buf).trim().to_string();
    if !tail.is_empty() {
        let _ = ch.send(serde_json::json!({ "kind": "line", "line": tail }));
    }
    let _ = ch.send(serde_json::json!({ "kind": "end" }));
    Ok(())
}

/// Stops a stream started by `ollama_stream`. Unknown ids are ignored: a stream that has
/// already finished is not an error to cancel.
#[tauri::command]
fn ollama_abort(id: String) {
    if let Ok(map) = cancels().lock() {
        if let Some(n) = map.get(&id) {
            n.notify_one();
        }
    }
}

/// Total bytes of the current Ollama installer.
///
/// install.ps1 reports progress as a bare percentage, which on its own cannot say how large
/// the download is or how fast it is going. One HEAD against the same stable release URL the
/// script uses turns that percentage into real megabytes, a transfer rate and an ETA.
/// Best-effort: 0 means "unknown", and the UI then shows the percentage alone.
#[tauri::command]
async fn installer_size() -> u64 {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("DeskOllama")
        .build()
    {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let url = "https://github.com/ollama/ollama/releases/latest/download/OllamaSetup.exe";
    match client.head(url).send().await {
        Ok(r) => r.content_length().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Total bytes Ollama's blobs occupy, and where they live.
#[tauri::command]
fn models_disk_usage() -> serde_json::Value {
    #[cfg(target_os = "windows")]
    {
        let Some(dir) = models_dir() else {
            return serde_json::json!({ "bytes": 0, "path": "" });
        };
        let blobs = dir.join("blobs");
        let mut total: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(&blobs) {
            for e in entries.flatten() {
                if let Ok(m) = e.metadata() {
                    if m.is_file() {
                        total += m.len();
                    }
                }
            }
        }
        serde_json::json!({ "bytes": total, "path": dir.to_string_lossy() })
    }
    #[cfg(not(target_os = "windows"))]
    {
        serde_json::json!({ "bytes": 0, "path": "" })
    }
}

/// Deletes the data a cancelled download left behind, and nothing else.
///
/// `digests` are the layers of the cancelled pull; `keep` are digests the caller knows are
/// still wanted, meaning the layers of every OTHER pull it has pending. Anything referenced by a
/// manifest already on disk is protected here regardless of what the caller passed.
///
/// The guard is the entire point. Ollama shares identical layers between models, so deleting
/// purely by digest could gut an installed model, or wipe a second paused download that
/// happens to share a layer with the one being cancelled. Partial files are always safe to
/// remove: they belong to a transfer that is no longer running.
///
/// Returns bytes actually freed.
#[tauri::command]
fn purge_blobs(digests: Vec<String>, keep: Vec<String>) -> u64 {
    #[cfg(target_os = "windows")]
    {
        let Some(dir) = models_dir() else { return 0 };
        let blobs = dir.join("blobs");
        let protected = manifest_digests();
        let keep: std::collections::HashSet<String> = keep.iter().map(|d| bare_digest(d)).collect();
        // Listed once rather than per digest: the directory does not change under us, and
        // re-reading it for every layer turned a handful of deletes into a directory scan
        // each.
        let entries: Vec<(String, std::path::PathBuf, u64)> = match std::fs::read_dir(&blobs) {
            Ok(rd) => rd
                .flatten()
                .map(|e| {
                    let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                    (e.file_name().to_string_lossy().to_string(), e.path(), len)
                })
                .collect(),
            Err(_) => return 0,
        };

        let mut freed: u64 = 0;
        for d in digests {
            let hex = bare_digest(&d);
            if hex.is_empty() {
                continue;
            }
            let stem = format!("sha256-{hex}");
            /* `keep` guards partial files just as firmly as finished ones.
               Two downloads paused at once can share a layer, and half of that layer on disk
               belongs to both of them. Cancelling one must not delete the other's progress,
               which is the whole reason the caller passes every other pending pull's digests.

               A finished layer additionally has to clear `protected` (the digests every
               installed model's manifest references), or cancelling would gut a working model. */
            let wanted_elsewhere = keep.contains(&hex);
            let complete_is_safe = !protected.contains(&hex) && !wanted_elsewhere;
            let partial_prefix = format!("{stem}-partial");
            for (name, path, len) in &entries {
                let is_partial = name.starts_with(&partial_prefix) && !wanted_elsewhere;
                let is_complete = *name == stem;
                if !(is_partial || (is_complete && complete_is_safe)) {
                    continue;
                }
                if std::fs::remove_file(path).is_ok() {
                    freed += len;
                }
            }
        }
        freed
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (digests, keep);
        0
    }
}

#[cfg(target_os = "windows")]
fn apply_dark_titlebar(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
    use windows_sys::Win32::Foundation::HWND;

    if let Ok(hwnd) = window.hwnd() {
        let hwnd = hwnd.0 as HWND;
        let dark_mode: i32 = 1;
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &dark_mode as *const _ as *const _,
                std::mem::size_of::<i32>() as u32,
            );
            let color: u32 = 0x00212121; // RGB(33, 33, 33) = #212121
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_CAPTION_COLOR as u32,
                &color as *const _ as *const _,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
}

/* What to do on close, told to us by the frontend because only it knows the settings.
   Previously this handler unloaded unconditionally and against a hardcoded localhost:11434,
   so the "Unload models when the app closes" switch did nothing and anyone running Ollama on
   another port had models left resident. */
struct ClosePolicy {
    unload: std::sync::atomic::AtomicBool,
    base: std::sync::Mutex<String>,
}
fn close_policy() -> &'static ClosePolicy {
    static P: std::sync::OnceLock<ClosePolicy> = std::sync::OnceLock::new();
    P.get_or_init(|| ClosePolicy {
        unload: std::sync::atomic::AtomicBool::new(true),
        base: std::sync::Mutex::new(String::from("http://localhost:11434")),
    })
}

#[tauri::command]
fn set_close_policy(unload: bool, base_url: String) {
    close_policy()
        .unload
        .store(unload, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut b) = close_policy().base.lock() {
        if !base_url.trim().is_empty() {
            *b = base_url.trim_end_matches('/').to_string();
        }
    }
}

async fn unload_ollama_models() {
    let base = close_policy()
        .base
        .lock()
        .map(|b| b.clone())
        .unwrap_or_else(|_| String::from("http://localhost:11434"));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();

    if let Ok(client) = client {
        if let Ok(resp) = client.get(format!("{base}/api/ps")).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(models) = json.get("models").and_then(|m| m.as_array()) {
                    for model in models {
                        if let Some(name) = model.get("name").and_then(|n| n.as_str()) {
                            let unload_payload = serde_json::json!({
                                "model": name,
                                "keep_alive": 0
                            });
                            let _ = client
                                .post(format!("{base}/api/generate"))
                                .json(&unload_payload)
                                .send()
                                .await;
                        }
                    }
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            ollama_detect,
            ollama_install,
            ollama_start,
            fetch_page,
            installer_size,
            models_disk_usage,
            purge_blobs,
            ollama_fetch,
            ollama_stream,
            ollama_abort,
            set_close_policy
        ])
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                #[cfg(target_os = "windows")]
                apply_dark_titlebar(&window);
            }
            Ok(())
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Instant window hide from screen (< 5ms response time)
                let _ = window.hide();
                api.prevent_close();
                let app_handle = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    // Respects the user's setting now, rather than unloading regardless.
                    if close_policy()
                        .unload
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        unload_ollama_models().await;
                    }
                    app_handle.exit(0);
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
