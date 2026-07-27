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
        .on_window_event(|_window, event| {
            if let WindowEvent::CloseRequested { .. } = event {
                let handle = tokio::runtime::Handle::try_current();
                if let Ok(h) = handle {
                    h.spawn(async {
                        unload_ollama_models().await;
                    });
                } else {
                    let rt = tokio::runtime::Runtime::new();
                    if let Ok(rt) = rt {
                        rt.block_on(unload_ollama_models());
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
