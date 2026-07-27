use tauri::Manager;
use tauri::WindowEvent;

async fn unload_ollama_models() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();

    if let Ok(client) = client {
        if let Ok(resp) = client.get("http://localhost:11434/api/ps").send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(models) = json.get("models").and_then(|m| m.as_array()) {
                    for model in models {
                        if let Some(name) = model.get("name").and_then(|n| n.as_str()) {
                            let unload_payload = serde_json::json!({
                                "model": name,
                                "keep_alive": 0
                            });
                            let _ = client
                                .post("http://localhost:11434/api/generate")
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
                    unload_ollama_models().await;
                    app_handle.exit(0);
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
