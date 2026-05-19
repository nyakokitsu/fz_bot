use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::{tungstenite::protocol::Message, client_async};
use tokio_rustls::TlsConnector as TokioTlsConnector;
use rustls::{ClientConfig, client::ServerCertVerifier, client::ServerCertVerified, Error as RustlsError};
use std::time::SystemTime;
use tokio_tungstenite::tungstenite::http::Request as WsRequest;
use tokio::net::TcpStream;
use std::convert::TryFrom;
use tokio::sync::RwLock;

const FACTORIO_ZONE_ENDPOINT: &str = "factorio.zone";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ServerStatus {
    OFFLINE,
    STARTING,
    STOPPING,
    RUNNING,
}

#[derive(Deserialize)]
struct StopperResponse {
    #[serde(rename = "statusCode")]
    status_code: u8,
}

#[derive(Deserialize)]
struct LauncherResponse {
    #[serde(rename = "statusCode")]
    status_code: u8,
    #[serde(rename = "launchId")]
    launch_id: u64,
    #[serde(rename = "statusReason")]
    status_reason: Option<String>,
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub struct FZClientState {
    pub visit_secret: Option<String>,
    pub user_token: String,
    pub server_status: ServerStatus,
    pub server_address: Option<String>,
    pub launch_id: Option<String>,
    pub mods: Vec<Value>,
    pub saves: Value,
    pub mods_sync: bool,
    pub saves_sync: bool,
}

#[derive(Clone)]
pub struct FZClient {
    pub state: Arc<RwLock<FZClientState>>,
    http_client: reqwest::Client,
}

impl FZClient {
    pub fn new(token: &str) -> Self {
        Self {
            state: Arc::new(RwLock::new(FZClientState {
                visit_secret: None,
                user_token: token.to_string(),
                server_status: ServerStatus::OFFLINE,
                server_address: None,
                launch_id: None,
                mods: Vec::new(),
                saves: Value::Null,
                mods_sync: false,
                saves_sync: false,
            })),
            http_client: reqwest::Client::new(),
        }
    }

    pub async fn connect(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut reconnect_delay = Duration::from_secs(1);
        const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

        loop {
            if let Err(e) = self.connect_internal().await {
                log::error!("WebSocket connection error: {:?}. Reconnecting in {:?}...", e, reconnect_delay);
                tokio::time::sleep(reconnect_delay).await;
                
                // Exponential backoff: double the delay, capped at MAX_RECONNECT_DELAY
                reconnect_delay = std::cmp::min(reconnect_delay.mul_f32(2.0), MAX_RECONNECT_DELAY);
            } else {
                // Connection closed normally, reset backoff
                reconnect_delay = Duration::from_secs(1);
                log::info!("WebSocket disconnected, attempting to reconnect...");
            }
        }
    }

    async fn connect_internal(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("wss://{}/ws", FACTORIO_ZONE_ENDPOINT);

        // Create a TLS config that skips certificate verification.
        // This is insecure and should only be used for testing or when
        // you control the connection target. Prefer adding the proper
        // CA to the root store in production.
        struct NoVerifier;
        impl ServerCertVerifier for NoVerifier {
            fn verify_server_cert(
                &self,
                _end_entity: &rustls::Certificate,
                _intermediates: &[rustls::Certificate],
                _server_name: &rustls::client::ServerName,
                _scts: &mut dyn Iterator<Item = &[u8]>,
                _ocsp_response: &[u8],
                _now: SystemTime,
            ) -> Result<ServerCertVerified, RustlsError> {
                Ok(ServerCertVerified::assertion())
            }
        }

        let mut cfg = ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();

        let tls_connector = TokioTlsConnector::from(Arc::new(cfg));

        // Connect TCP then perform TLS handshake manually and upgrade to WS
        let addr = format!("{}:443", FACTORIO_ZONE_ENDPOINT);
        let tcp = TcpStream::connect(addr).await?;
        let server_name = rustls::ServerName::try_from(FACTORIO_ZONE_ENDPOINT)
            .map_err(|_| "invalid dns name")?;

        let tls_stream = tls_connector.connect(server_name, tcp).await?;

        let req = WsRequest::builder().uri(&url).header("Host", FACTORIO_ZONE_ENDPOINT).body(())?;
        let (mut ws_stream, _resp) = client_async(req, tls_stream).await?;
        println!("Connected to Factorio Zone WS");

        while let Some(msg) = ws_stream.next().await {
            let msg = match msg {
                Ok(Message::Text(text)) => text,
                _ => continue,
            };

            if let Ok(data) = serde_json::from_str::<Value>(&msg) {
                let msg_type = data["type"].as_str().unwrap_or("");
                let mut state = self.state.write().await;

                match msg_type {
                    "visit" => {
                        state.visit_secret = data["secret"].as_str().map(String::from);
                        drop(state);
                        let _ = self.login().await;
                    }
                    "options" => {
                        if data.get("name").and_then(|v| v.as_str()) == Some("saves") {
                            state.saves = data.get("options").cloned().unwrap_or_else(|| Value::Null);
                            state.saves_sync = true;
                        }
                    }
                    "mods" => {
                        state.mods = data["mods"].as_array().cloned().unwrap_or_default();
                        state.mods_sync = true;
                    }
                    "idle" => {
                        state.launch_id = None;
                        state.server_status = ServerStatus::OFFLINE;
                        state.server_address = None;
                    }
                    "starting" => {
                        state.launch_id = data.get("launchId").and_then(|v| v.as_i64()).map(|n| n.to_string());
                        state.server_status = ServerStatus::STARTING;
                    }
                    "stopping" => {
                        state.launch_id = data.get("launchId").and_then(|v| v.as_i64()).map(|n| n.to_string());
                        state.server_status = ServerStatus::STOPPING;
                    }
                    "running" => {
                        state.launch_id = data.get("launchId").and_then(|v| v.as_i64()).map(|n| n.to_string());
                        state.server_address = data.get("socket").and_then(|v| v.as_str()).map(String::from);
                        state.server_status = ServerStatus::RUNNING;
                    }
                    "info" => {
                        if let Some(line) = data.get("line").and_then(|v| v.as_str()) {
                            let re = regex::Regex::new(r"selecting connection (\d+\.\d+\.\d+\.\d+:\d+)").unwrap();
                            if let Some(caps) = re.captures(line) {
                                state.server_address = Some(caps[1].to_string());
                                state.server_status = ServerStatus::STARTING;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn login(&self) -> Result<(), reqwest::Error> {
        let state = self.state.read().await;
        let mut params = HashMap::new();
        params.insert("userToken", state.user_token.clone());
        params.insert("visitSecret", state.visit_secret.clone().unwrap_or_default());
        params.insert("reconnected", "false".to_string());
        drop(state);

        let url = format!("https://{}/api/user/login", FACTORIO_ZONE_ENDPOINT);
        if let Ok(res) = self.http_client.post(&url).form(&params).send().await {
            if let Ok(body) = res.json::<Value>().await {
                if let Some(token) = body["userToken"].as_str() {
                    self.state.write().await.user_token = token.to_string();
                }
            }
        }
        Ok(())
    }

    pub async fn toggle_mod(&self, mod_id: i64, enabled: bool) -> Result<(), reqwest::Error> {
        let state = self.state.read().await;
        let mut params = HashMap::new();
        params.insert("visitSecret", state.visit_secret.clone().unwrap_or_default());
        params.insert("modId", mod_id.to_string());
        params.insert("enabled", enabled.to_string());
        drop(state);

        let url = format!("https://{}/api/mod/toggle", FACTORIO_ZONE_ENDPOINT);
        self.http_client.post(&url).form(&params).send().await?;
        Ok(())
    }

    pub async fn download_save_slot(&self, slot: &str) -> Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>> {
        let state = self.state.read().await;
        let mut params = HashMap::new();
        params.insert("visitSecret", state.visit_secret.clone().unwrap_or_default());
        params.insert("save", slot.to_string());
        drop(state);

        let url = format!("https://{}/api/save/download", FACTORIO_ZONE_ENDPOINT);
        let res = self.http_client.post(&url).form(&params).send().await?;
        
        if res.status().is_success() {
            Ok(res.bytes().await?)
        } else {
            Err("Failed to download save".into())
        }
    }

    pub async fn start_instance(&self, region: &str, version: &str, save: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = self.state.read().await;
        let secret = state.visit_secret.clone().unwrap_or_default();
        drop(state);

        let modlist = if save == "slot2" {
            r#"{"elevated-rails":true,"quality":true,"space-age":true}"#
        } else {
            r#"{"elevated-rails":false,"quality":false,"space-age":false}"#
        };

        let mut params = HashMap::new();
        params.insert("visitSecret", secret);
        params.insert("region", region.to_string());
        params.insert("version", version.to_string());
        params.insert("save", save.to_string());
        params.insert("options", modlist.to_string());

        let url = format!("https://{}/api/instance/start", FACTORIO_ZONE_ENDPOINT);
        let res = self.http_client.post(&url).form(&params).send().await?;
        let res_text = res.text().await.unwrap_or_default();
        match serde_json::from_str::<LauncherResponse>(&res_text) {
            Ok(s) if s.status_code == 200 => {
                self.state.write().await.launch_id = Some(s.launch_id.to_string());
                println!("Instance started successfully with launch ID: {}", s.launch_id);
                self.state.write().await.server_status = ServerStatus::STARTING
            },
            Ok(s) => {
                let error_msg = s.status_reason.as_deref().unwrap_or("Unknown error");
                println!("Failed to start instance: [{}] {}", s.status_code, error_msg);
            },
            Err(_) => println!("Failed to send start request: HTTP {}", res_text),
        }
        Ok(())
    }

    pub async fn stop_instance(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = self.state.read().await;
        let mut params = HashMap::new();
        params.insert("visitSecret", state.visit_secret.clone().unwrap_or_default());
        params.insert("launchId", state.launch_id.clone().unwrap_or_default());
        drop(state);

        let url = format!("https://{}/api/instance/stop", FACTORIO_ZONE_ENDPOINT);
        let ans = self.http_client.post(&url).form(&params).timeout(Duration::from_secs(3600)).send().await?;
        let ans_text = ans.text().await.unwrap_or_default();
        match serde_json::from_str::<StopperResponse>(&ans_text) {
            Ok(s) if s.status_code == 200 => self.state.write().await.server_status = ServerStatus::OFFLINE,
            _ => println!("Failed to send stop request: HTTP {}", ans_text),
        }
        Ok(())
    }
}