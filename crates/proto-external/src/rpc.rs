//! JSON-RPC 2.0 по рядках: одне повідомлення = один рядок JSON.
//!
//! Байти файла сюди не кладемо. Шлях у JSON екранується, тож `\n` у ньому
//! не рве кадр — на відміну від «сирого» рядка до переносу.

use serde::{Deserialize, Serialize};

/// Виклик від адаптера до плагіна.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    pub params: serde_json::Value,
}

/// Помилка JSON-RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Параметри `probe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeParams {
    pub source: String,
}

/// Параметри `run`. `cancel` навмисно немає: зупинка = убивство процесу.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunParams {
    pub source: String,
    pub targets: Vec<String>,
}

/// Результат `probe` на дроті.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    #[serde(default)]
    pub final_url: String,
    pub total_size: Option<u64>,
    #[serde(default)]
    pub resumable: bool,
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub files: Vec<RpcFile>,
}

/// Файл у відповіді `probe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcFile {
    pub suggested_name: String,
    pub size: Option<u64>,
    #[serde(default)]
    pub selected: bool,
}

/// Результат `run` на дроті.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunResult {
    #[serde(default)]
    pub resume: Option<Vec<u8>>,
}

/// Рядок зі stdout плагіна.
#[derive(Debug, Deserialize)]
pub struct WireMessage {
    #[serde(default)]
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub method: Option<String>,
    pub params: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
}

/// Сповіщення про поступ (без `id`).
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressDto {
    TotalKnown { total: u64 },
    Advanced { done: u64 },
    Segments { count: usize },
    Checkpoint { resume: Vec<u8> },
}

/// Зібрати рядок відповіді без зайвих переносів усередині.
pub fn ok_line(
    id: &serde_json::Value,
    result: &impl Serialize,
) -> serde_json::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
}

/// Зібрати рядок помилки JSON-RPC.
pub fn error_line(
    id: &serde_json::Value,
    code: i64,
    message: &str,
) -> serde_json::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
}
