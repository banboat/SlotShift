#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::net::TcpListener;

const PORT: u16 = 5678;
const HTML: &str = include_str!("ui.html");

const MAX_MODELS: usize = 8;

/// One selectable model in Claude's picker. `id` is the gateway route Claude
/// validates (and the model name our proxy receives), `name` is what Claude
/// shows for it.
#[derive(Clone)]
struct Slot {
    id: String,
    name: String,
    description: String,
}

/// Used when Claude's model catalogue cannot be read. Kept in the same order
/// as the official picker (main section first, then overflow).
const FALLBACK_SLOTS: &[(&str, &str, &str)] = &[
    ("claude-opus-5-5", "Opus 5.5", "For complex work and everyday tasks"),
    ("claude-sonnet-5-5", "Sonnet 5.5", "Most efficient for simpler tasks"),
    ("claude-fable-5-2", "Fable 5.2", "For your toughest challenges"),
    ("claude-haiku-5-5", "Haiku 5.5", "Fastest for quick answers"),
    ("claude-fable-5-1", "Fable 5.1", ""),
    ("claude-haiku-4-5-20251001", "Haiku 4.5", ""),
    ("claude-sonnet-5", "Sonnet 5", ""),
    ("claude-opus-5", "Opus 5", ""),
    ("claude-fable-5", "Fable 5", ""),
    ("claude-opus-4-8", "Opus 4.8", ""),
    ("claude-opus-4-7", "Opus 4.7", ""),
    ("claude-opus-4-6", "Opus 4.6", ""),
    ("claude-sonnet-4-6", "Sonnet 4.6", ""),
];

fn fallback_slots() -> Vec<Slot> {
    FALLBACK_SLOTS
        .iter()
        .map(|(id, name, description)| Slot {
            id: id.to_string(),
            name: name.to_string(),
            description: description.to_string(),
        })
        .collect()
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Config {
    #[serde(default)]
    providers: Vec<Provider>,
    #[serde(default)]
    show_model_names: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Provider {
    #[serde(default)]
    target_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    models: Vec<ModelEntry>,
    /// "", "auto", "deepseek3", "claude5" or "off".
    #[serde(default)]
    effort_mode: String,
    /// Legacy fixed-effort override. Migrated into `effort_mode` on load.
    #[serde(default)]
    thinking_effort: String,
    /// Turn Claude's Low level into a direct answer with no thinking at all.
    #[serde(default)]
    fast_low: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ModelEntry {
    #[serde(default)]
    name: String,
    /// Optional display name shown in Claude's picker instead of the official
    /// slot name.
    #[serde(default)]
    label: String,
}

#[derive(Serialize, Clone)]
struct LogEntry {
    time: String,
    model: String,
    status: u16,
    #[serde(default)]
    effort: String,
}

struct AppState {
    config: RwLock<Config>,
    slots: RwLock<Vec<Slot>>,
    client: Client,
    logs: RwLock<Vec<LogEntry>>,
}

const MAX_LOGS: usize = 100;

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".slotshift")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

fn legacy_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".claude-model-proxy").join("config.json")
}

fn load_config() -> Config {
    let path = config_path();
    let mut config = if path.exists() {
        let data = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        // SlotShift was previously shipped as ModelSwitch.
        let legacy = legacy_config_path();
        if legacy.exists() {
            let data = std::fs::read_to_string(&legacy).unwrap_or_default();
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Config::default()
        }
    };
    normalize_config(&mut config);
    config
}

fn friendly_write_error(e: &std::io::Error, path: &PathBuf) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied =>
            format!("Permission denied: {}. Please check folder permissions or try running as administrator.", path.display()),
        std::io::ErrorKind::NotFound =>
            format!("Path not found: {}. Please ensure the parent directory exists.", path.display()),
        _ if e.raw_os_error() == Some(32) || e.raw_os_error() == Some(33) =>
            format!("File is locked: {}. Please close Claude Desktop first and try again.", path.display()),
        _ => format!("Write failed ({}): {}", path.display(), e),
    }
}

fn write_with_retry(path: &PathBuf, data: &str) -> Result<(), String> {
    for attempt in 0..3 {
        match std::fs::write(path, data) {
            Ok(()) => return Ok(()),
            Err(e) if (e.raw_os_error() == Some(32) || e.raw_os_error() == Some(33)) && attempt < 2 => {
                eprintln!("[write] file locked, retrying in 1s...");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(friendly_write_error(&e, path)),
        }
    }
    Err(format!("Failed after retries: {}", path.display()))
}

fn save_config_file(config: &Config) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("Cannot create config directory: {}. Permission denied.", dir.display())
        } else {
            format!("Cannot create config directory: {}", e)
        }
    })?;
    let data = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let target = config_path();
    let tmp = target.with_extension("json.tmp");
    write_with_retry(&tmp, &data)?;
    std::fs::rename(&tmp, &target).map_err(|e| friendly_write_error(&e, &target))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

struct ResolvedModel {
    model: String,
    target_url: String,
    api_key: String,
    effort_mode: String,
    fast_low: bool,
}

struct FlatEntry {
    slot: String,
    slot_name: String,
    label: String,
    name: String,
    url: String,
    key: String,
    effort_mode: String,
    max_effort: Option<String>,
    fast_low: bool,
}

/// How Claude's effort levels are translated for this provider.
///
/// - `deepseek3`: Claude offers Low/Medium/High (via `maxEffort: high`) and the
///   proxy sends low/high/max respectively — DeepSeek's three declared levels.
/// - `claude5`: pass Claude's five levels through unchanged.
/// - `off`: disable thinking.
fn resolve_effort_mode(provider: &Provider) -> &'static str {
    match provider.effort_mode.as_str() {
        "deepseek3" => "deepseek3",
        "claude5" => "claude5",
        "off" => "off",
        _ => {
            if provider.target_url.to_ascii_lowercase().contains("deepseek") {
                "deepseek3"
            } else {
                "claude5"
            }
        }
    }
}

fn max_effort_for(mode: &str) -> Option<&'static str> {
    match mode {
        "deepseek3" => Some("high"),
        "off" => Some("low"),
        _ => None,
    }
}

/// Claude's three visible levels -> DeepSeek's declared low/high/max.
/// Levels outside the visible range are clamped to the nearest edge.
fn map_deepseek3(effort: &str) -> &'static str {
    match effort.to_ascii_lowercase().as_str() {
        "none" => "none",
        "minimal" | "low" => "low",
        "medium" => "high",
        "high" | "xhigh" | "max" | "ultra" => "max",
        _ => "high",
    }
}

/// Appended to the system prompt only in fast-low mode. Without it the model
/// tends to write a long derivation even with thinking disabled, which can hit
/// the output limit before it states an answer.
const FAST_LOW_DIRECTIVE: &str = "Fast mode: give the final result first, then at most 3 short sentences of justification or the essential code notes. Do not show a long derivation.";

fn append_fast_directive(data: &mut serde_json::Value) {
    let already = match data.get("system") {
        Some(serde_json::Value::String(s)) => s.contains("Fast mode:"),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .map(|t| t.contains("Fast mode:"))
                .unwrap_or(false)
        }),
        _ => false,
    };
    if already {
        return;
    }
    match data.get_mut("system") {
        Some(serde_json::Value::String(s)) => {
            s.push_str("\n\n");
            s.push_str(FAST_LOW_DIRECTIVE);
        }
        Some(serde_json::Value::Array(blocks)) => {
            blocks.push(serde_json::json!({"type": "text", "text": FAST_LOW_DIRECTIVE}));
        }
        Some(_) => {}
        None => {
            data["system"] = serde_json::json!([{"type": "text", "text": FAST_LOW_DIRECTIVE}]);
        }
    }
}

/// Rewrite the outgoing request's thinking/effort fields for the provider.
/// Returns the effective upstream effort for the request log.
fn apply_effort(data: &mut serde_json::Value, mode: &str, fast_low: bool) -> String {
    let requested = data
        .get("output_config")
        .and_then(|c| c.get("effort"))
        .and_then(|e| e.as_str())
        .map(|e| e.to_string());

    match mode {
        "off" => {
            data["thinking"] = serde_json::json!({"type": "disabled"});
            if let Some(obj) = data.as_object_mut() {
                obj.remove("output_config");
            }
            "off".to_string()
        }
        "deepseek3" => match requested {
            Some(effort) => {
                if fast_low && effort.eq_ignore_ascii_case("low") {
                    data["thinking"] = serde_json::json!({"type": "disabled"});
                    if let Some(obj) = data.as_object_mut() {
                        obj.remove("output_config");
                    }
                    append_fast_directive(data);
                    eprintln!("  effort: low -> direct answer (thinking off)");
                    return "off".to_string();
                }
                let mapped = map_deepseek3(&effort);
                if data.get("output_config").and_then(|v| v.as_object()).is_none() {
                    data["output_config"] = serde_json::json!({});
                }
                data["output_config"]["effort"] = serde_json::json!(mapped);
                let disabled = data
                    .get("thinking")
                    .and_then(|t| t.get("type"))
                    .and_then(|v| v.as_str())
                    == Some("disabled");
                if !disabled {
                    // Let `effort` choose the budget instead of forwarding a
                    // budget computed for a different level.
                    data["thinking"] = serde_json::json!({"type": "enabled"});
                }
                eprintln!("  effort: {} -> {}", effort, mapped);
                mapped.to_string()
            }
            None => String::new(),
        },
        _ => requested.unwrap_or_default(),
    }
}

fn normalize_config(config: &mut Config) {
    for p in &mut config.providers {
        if p.effort_mode.trim().is_empty() {
            p.effort_mode = match p.thinking_effort.as_str() {
                "off" => "off".to_string(),
                _ => "auto".to_string(),
            };
        }
        p.thinking_effort.clear();
    }
}

fn flatten_config(config: &Config, slots: &[Slot]) -> Vec<FlatEntry> {
    let mut result = Vec::new();
    for provider in &config.providers {
        let mode = resolve_effort_mode(provider);
        let max_effort = max_effort_for(mode).map(|m| m.to_string());
        for m in &provider.models {
            if result.len() >= MAX_MODELS || result.len() >= slots.len() {
                return result;
            }
            if m.name.trim().is_empty() {
                continue;
            }
            let slot = &slots[result.len()];
            let label = if !m.label.trim().is_empty() {
                m.label.trim().to_string()
            } else if config.show_model_names {
                m.name.trim().to_string()
            } else {
                String::new()
            };
            result.push(FlatEntry {
                slot: slot.id.clone(),
                slot_name: slot.name.clone(),
                label,
                name: m.name.trim().to_string(),
                url: provider.target_url.clone(),
                key: provider.api_key.clone(),
                effort_mode: mode.to_string(),
                max_effort: max_effort.clone(),
                fast_low: provider.fast_low,
            });
        }
    }
    result
}

/// The `inferenceModels` array Claude reads. One entry per upstream model: no
/// `supports1m`, because current Claude models already have a 1M context
/// natively and the 1M variant would otherwise duplicate every row.
fn build_inference_models(config: &Config, slots: &[Slot]) -> Vec<serde_json::Value> {
    flatten_config(config, slots)
        .iter()
        .map(|e| {
            let mut model = serde_json::json!({ "name": e.slot });
            if !e.label.is_empty() {
                model["labelOverride"] = serde_json::json!(e.label);
            }
            if let Some(max_effort) = &e.max_effort {
                model["maxEffort"] = serde_json::json!(max_effort);
            }
            model
        })
        .collect()
}

fn slot_family(id: &str) -> Option<&'static str> {
    let id = id.to_lowercase();
    ["fable", "mythos", "opus", "sonnet", "haiku"]
        .into_iter()
        .find(|family| id.contains(family))
}

/// Resolve both current slots and stale aliases from older versions (e.g.
/// `claude-3-5-sonnet-latest`) or Claude Code's bare tier aliases (`opus`).
fn resolve_model(model: &str, config: &Config, slots: &[Slot]) -> ResolvedModel {
    let base = model
        .strip_suffix("[1m]")
        .or_else(|| model.strip_suffix("[1M]"))
        .unwrap_or(model)
        .trim();

    let flat = flatten_config(config, slots);

    if let Some(e) = flat.iter().find(|e| base.eq_ignore_ascii_case(&e.slot)) {
        return resolved_from(e);
    }

    if let Some(family) = slot_family(base) {
        let preferences: &[&str] = match family {
            "haiku" => &["haiku", "sonnet", "opus", "fable"],
            "sonnet" => &["sonnet", "opus", "haiku", "fable"],
            "fable" => &["fable", "opus", "sonnet", "haiku"],
            _ => &["opus", "sonnet", "haiku", "fable"],
        };
        for wanted in preferences {
            if let Some(e) = flat.iter().find(|e| e.slot.contains(wanted)) {
                eprintln!("  alias: {} -> {}", model, e.slot);
                return resolved_from(e);
            }
        }
    }

    if let Some(e) = flat.iter().find(|e| e.name.eq_ignore_ascii_case(base)) {
        return resolved_from(e);
    }

    if let Some(e) = flat.first() {
        eprintln!("  fallback: {} -> {}", model, e.name);
        return resolved_from(e);
    }

    ResolvedModel {
        model: model.to_string(),
        target_url: String::new(),
        api_key: String::new(),
        effort_mode: "claude5".to_string(),
        fast_low: false,
    }
}

fn resolved_from(e: &FlatEntry) -> ResolvedModel {
    ResolvedModel {
        model: e.name.clone(),
        target_url: e.url.clone(),
        api_key: e.key.clone(),
        effort_mode: e.effort_mode.clone(),
        fast_low: e.fast_low,
    }
}

fn claude_3p_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let home = PathBuf::from(home);

    #[cfg(target_os = "macos")]
    let dir = home.join("Library/Application Support/Claude-3p");

    #[cfg(target_os = "windows")]
    let dir = {
        // Prefer Microsoft Store sandbox path if Store version is installed
        let store_dir = (|| -> Option<PathBuf> {
            let localappdata = std::env::var("LOCALAPPDATA").ok()?;
            let packages = PathBuf::from(localappdata).join("Packages");
            let known = packages.join("Claude_pzs8sxrjxfjjc");
            if known.exists() {
                return Some(known.join("LocalCache").join("Roaming").join("Claude-3p"));
            }
            std::fs::read_dir(&packages).ok()?.flatten()
                .find(|e| e.file_name().to_string_lossy().starts_with("Claude_"))
                .map(|e| e.path().join("LocalCache").join("Roaming").join("Claude-3p"))
        })();

        store_dir.unwrap_or_else(|| {
            // Non-Store: check %LOCALAPPDATA%\Claude-3p first (newer installs)
            let localappdata = std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Local"));
            let local_dir = localappdata.join("Claude-3p");
            if local_dir.exists() {
                return local_dir;
            }
            // Then check %APPDATA%\Claude-3p (older installs)
            let appdata = std::env::var("APPDATA").ok().map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"));
            let roaming_dir = appdata.join("Claude-3p");
            if roaming_dir.exists() {
                return roaming_dir;
            }
            // Neither exists yet — default to APPDATA (Roaming)
            roaming_dir
        })
    };

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let dir = home.join(".config/Claude-3p");

    Some(dir)
}

fn catalog_path() -> Option<PathBuf> {
    Some(claude_3p_dir()?.join("model-catalog").join("published.json"))
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for b in input.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Read the model catalogue that Claude Desktop itself downloaded. Claude
/// refuses gateway routes that are not current Claude models, so this is the
/// source of truth for which model names the picker can display.
fn slots_from_catalog() -> Option<Vec<Slot>> {
    let path = catalog_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let wrapper: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let document = wrapper.get("documentBytes")?.as_str()?;
    let bytes = base64_decode(document)?;
    let catalog: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let models = catalog
        .pointer("/surfaces/cc/model_selector_config/0/models")?
        .as_array()?;

    let mut ranked: Vec<(u8, Slot)> = Vec::new();
    for m in models {
        let id = match m.get("id").and_then(|v| v.as_str()) {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => continue,
        };
        let section = m.get("section").and_then(|v| v.as_str()).unwrap_or("");
        if section == "deprecated" {
            continue;
        }
        if let Some(offered) = m.get("offered_on").and_then(|v| v.as_array()) {
            let gateway = offered.iter().any(|v| v.as_str() == Some("gateway"));
            if !gateway {
                continue;
            }
        }
        if ranked.iter().any(|(_, s)| s.id == id) {
            continue;
        }
        let name = m
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let description = m
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let rank = match section {
            "main" => 0,
            "overflow" => 1,
            _ => 2,
        };
        ranked.push((
            rank,
            Slot {
                id,
                name,
                description,
            },
        ));
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    let slots: Vec<Slot> = ranked.into_iter().map(|(_, s)| s).collect();
    if slots.len() < 2 {
        return None;
    }
    Some(slots)
}

fn official_slots() -> Vec<Slot> {
    match slots_from_catalog() {
        Some(slots) => {
            eprintln!(
                "[slots] {} official model slots from Claude catalog",
                slots.len()
            );
            slots
        }
        None => {
            eprintln!("[slots] Claude catalog unavailable, using built-in model slots");
            fallback_slots()
        }
    }
}

fn ensure_claude_desktop_gateway() {
    let claude_dir = match claude_3p_dir() {
        Some(d) => d,
        None => {
            eprintln!("[auto-config] FAIL: cannot determine home directory");
            return;
        }
    };
    eprintln!("[auto-config] Claude-3p dir: {}", claude_dir.display());

    let config_lib = claude_dir.join("configLibrary");
    if let Err(e) = std::fs::create_dir_all(&config_lib) {
        eprintln!("[auto-config] FAIL: cannot create {}: {}", config_lib.display(), e);
        return;
    }

    let our_id = "a0a0a0a0-b1b1-4c2c-9d3d-e4e4e4e4e4e4";
    let meta_path = config_lib.join("_meta.json");
    let mut meta: serde_json::Value = if meta_path.exists() {
        let content = std::fs::read_to_string(&meta_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let applied_id = meta.get("appliedId").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let target_id = if !applied_id.is_empty() && config_lib.join(format!("{}.json", applied_id)).exists() {
        applied_id
    } else {
        our_id.to_string()
    };

    let config_file = config_lib.join(format!("{}.json", target_id));
    let mut existing: serde_json::Value = if config_file.exists() {
        let content = std::fs::read_to_string(&config_file).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    existing["coworkEgressAllowedHosts"] = serde_json::json!(["*"]);
    existing["inferenceProvider"] = serde_json::json!("gateway");
    existing["inferenceGatewayBaseUrl"] = serde_json::json!(format!("http://127.0.0.1:{}", PORT));
    existing["inferenceGatewayApiKey"] = serde_json::json!("proxy");
    existing["inferenceGatewayAuthScheme"] = serde_json::json!("bearer");
    if existing.get("inferenceModels").is_none() {
        existing["inferenceModels"] = serde_json::json!([]);
    }

    match serde_json::to_string_pretty(&existing) {
        Ok(data) => match write_with_retry(&config_file, &data) {
            Ok(()) => eprintln!("[auto-config] wrote {}", config_file.display()),
            Err(e) => eprintln!("[auto-config] FAIL write {}: {}", config_file.display(), e),
        },
        Err(e) => { eprintln!("[auto-config] FAIL serialize: {}", e); return; }
    }

    if target_id == our_id {
        meta["appliedId"] = serde_json::json!(our_id);
        let entries = meta.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        let mut new_entries = entries;
        if let Some(entry) = new_entries
            .iter_mut()
            .find(|e| e.get("id").and_then(|i| i.as_str()) == Some(our_id))
        {
            entry["name"] = serde_json::json!("SlotShift");
        } else {
            new_entries.push(serde_json::json!({"id": our_id, "name": "SlotShift"}));
        }
        meta["entries"] = serde_json::json!(new_entries);
    }

    if let Ok(meta_data) = serde_json::to_string_pretty(&meta) {
        let meta_tmp = meta_path.with_extension("json.tmp");
        let _ = std::fs::write(&meta_tmp, &meta_data);
        let _ = std::fs::rename(&meta_tmp, &meta_path);
    }

    fn write_desktop_config(path: &PathBuf) {
        let tmp = path.with_extension("json.tmp");
        let mut json: serde_json::Value = if path.exists() {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
        } else {
            serde_json::json!({})
        };
        json["deploymentMode"] = serde_json::json!("3p");
        if let Ok(out) = serde_json::to_string_pretty(&json) {
            let _ = std::fs::write(&tmp, &out);
            let _ = std::fs::rename(&tmp, path);
        }
        eprintln!("[auto-config] wrote config: {}", path.display());
    }

    write_desktop_config(&claude_dir.join("claude_desktop_config.json"));

    #[cfg(target_os = "windows")]
    {
        let normal_dir = claude_dir.parent()
            .map(|p| p.join("Claude"))
            .unwrap_or_else(|| {
                let home = std::env::var("APPDATA").unwrap_or_default();
                PathBuf::from(home).join("Claude")
            });
        if let Err(e) = std::fs::create_dir_all(&normal_dir) {
            eprintln!("[auto-config] FAIL create {}: {}", normal_dir.display(), e);
        } else {
            eprintln!("[auto-config] Claude dir: {}", normal_dir.display());
        }

        write_desktop_config(&normal_dir.join("claude_desktop_config.json"));

        let dev_settings = normal_dir.join("developer_settings.json");
        if !dev_settings.exists() {
            match std::fs::write(&dev_settings, r#"{"allowDevTools":true}"#) {
                Ok(()) => eprintln!("[auto-config] wrote {}", dev_settings.display()),
                Err(e) => eprintln!("[auto-config] FAIL write {}: {}", dev_settings.display(), e),
            }
        }

        let normal_config = normal_dir.join("config.json");
        if !normal_config.exists() {
            let _ = std::fs::write(&normal_config, r#"{"locale":"zh-CN","hasTrackedInitialActivation":true}"#);
        }

        let p3_dev = claude_dir.join("developer_settings.json");
        if !p3_dev.exists() {
            let _ = std::fs::write(&p3_dev, r#"{"allowDevTools":true}"#);
        }

        let p3_config = claude_dir.join("config.json");
        if !p3_config.exists() {
            let _ = std::fs::write(&p3_config, r#"{"locale":"zh-CN","hasTrackedInitialActivation":true}"#);
        }

        // Also write to other possible paths as fallback
        let appdata = PathBuf::from(std::env::var("APPDATA").unwrap_or_default());
        let localappdata = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default());
        let fallback_dirs = [
            (appdata.join("Claude-3p"), appdata.join("Claude")),
            (localappdata.join("Claude-3p"), localappdata.join("Claude")),
        ];
        for (fb_3p, fb_claude) in &fallback_dirs {
            if fb_3p == &*claude_dir { continue; }
            let _ = std::fs::create_dir_all(fb_claude);
            let _ = std::fs::create_dir_all(fb_3p);
            write_desktop_config(&fb_claude.join("claude_desktop_config.json"));
            write_desktop_config(&fb_3p.join("claude_desktop_config.json"));
            let dev = fb_claude.join("developer_settings.json");
            if !dev.exists() { let _ = std::fs::write(&dev, r#"{"allowDevTools":true}"#); }
            let dev3p = fb_3p.join("developer_settings.json");
            if !dev3p.exists() { let _ = std::fs::write(&dev3p, r#"{"allowDevTools":true}"#); }
        }
    }


    eprintln!("[auto-config] done.");
}

fn apply_to_claude_desktop(config: &Config, slots: &[Slot]) -> Result<String, String> {
    if config.providers.is_empty() {
        return Err("Please add at least one provider.".to_string());
    }
    for (i, p) in config.providers.iter().enumerate() {
        if p.target_url.is_empty() {
            return Err(format!("Provider {} has no API URL.", i + 1));
        }
        if !p.target_url.starts_with("http://") && !p.target_url.starts_with("https://") {
            return Err(format!("Provider {} URL must start with http:// or https://", i + 1));
        }
        if p.api_key.is_empty() {
            return Err(format!("Provider {} has no API key.", i + 1));
        }
        if p.models.is_empty() {
            return Err(format!("Provider {} has no models.", i + 1));
        }
        for m in &p.models {
            if m.name.trim().is_empty() {
                return Err(format!("Provider {} has a model with empty name.", i + 1));
            }
        }
    }

    let claude_dir = claude_3p_dir().ok_or("Cannot find home directory")?;
    let config_lib = claude_dir.join("configLibrary");
    std::fs::create_dir_all(&config_lib).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("Cannot create directory: {}. Permission denied. Try running as administrator.", config_lib.display())
        } else {
            format!("Cannot create directory: {}", e)
        }
    })?;

    let flat = flatten_config(config, slots);
    let models: Vec<serde_json::Value> = build_inference_models(config, slots);
    eprintln!(
        "[apply] models: {}",
        flat.iter()
            .map(|e| {
                if e.label.is_empty() {
                    format!("{} <- {}", e.slot_name, e.name)
                } else {
                    format!("{} ({}) <- {}", e.label, e.slot_name, e.name)
                }
            })
            .collect::<Vec<_>>()
            .join(" | ")
    );

    let meta_path = config_lib.join("_meta.json");
    let mut meta: serde_json::Value = if meta_path.exists() {
        let content = std::fs::read_to_string(&meta_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let applied_id = meta.get("appliedId").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let our_id = "a0a0a0a0-b1b1-4c2c-9d3d-e4e4e4e4e4e4";

    let target_id = if !applied_id.is_empty() && config_lib.join(format!("{}.json", applied_id)).exists() {
        applied_id.clone()
    } else {
        our_id.to_string()
    };

    let config_file = config_lib.join(format!("{}.json", target_id));
    let mut existing: serde_json::Value = if config_file.exists() {
        let content = std::fs::read_to_string(&config_file).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    existing["coworkEgressAllowedHosts"] = serde_json::json!(["*"]);
    existing["inferenceProvider"] = serde_json::json!("gateway");
    existing["inferenceGatewayBaseUrl"] = serde_json::json!(format!("http://127.0.0.1:{}", PORT));
    existing["inferenceGatewayApiKey"] = serde_json::json!("proxy");
    existing["inferenceGatewayAuthScheme"] = serde_json::json!("bearer");
    existing["inferenceModels"] = serde_json::json!(models);

    let data = serde_json::to_string_pretty(&existing).map_err(|e| e.to_string())?;
    write_with_retry(&config_file, &data)?;

    if target_id != our_id && !config_lib.join(format!("{}.json", our_id)).exists() {
    } else if target_id == our_id {
        meta["appliedId"] = serde_json::json!(our_id);
        let entries = meta.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        let mut new_entries: Vec<serde_json::Value> = entries
            .into_iter()
            .filter(|e| {
                if let Some(id) = e.get("id").and_then(|i| i.as_str()) {
                    id == our_id || config_lib.join(format!("{}.json", id)).exists()
                } else {
                    false
                }
            })
            .collect();
        if let Some(entry) = new_entries
            .iter_mut()
            .find(|e| e.get("id").and_then(|i| i.as_str()) == Some(our_id))
        {
            entry["name"] = serde_json::json!("SlotShift");
        } else {
            new_entries.push(serde_json::json!({"id": our_id, "name": "SlotShift"}));
        }
        meta["entries"] = serde_json::json!(new_entries);
    }

    let meta_data = serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?;
    let meta_tmp = meta_path.with_extension("json.tmp");
    std::fs::write(&meta_tmp, &meta_data).map_err(|e| format!("Cannot write _meta.json: {}", e))?;
    std::fs::rename(&meta_tmp, &meta_path).map_err(|e| format!("Cannot update _meta.json: {}", e))?;

    let _ = std::fs::remove_file(config_lib.join("model-proxy.json"));

    let desktop_cfg_path = claude_dir.join("claude_desktop_config.json");
    let desktop_tmp = desktop_cfg_path.with_extension("json.tmp");
    if desktop_cfg_path.exists() {
        let content = std::fs::read_to_string(&desktop_cfg_path).unwrap_or_default();
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if json.get("deploymentMode").and_then(|v| v.as_str()) != Some("3p") {
                let mut json = json;
                json["deploymentMode"] = serde_json::json!("3p");
                if let Ok(out) = serde_json::to_string_pretty(&json) {
                    let _ = std::fs::write(&desktop_tmp, &out);
                    let _ = std::fs::rename(&desktop_tmp, &desktop_cfg_path);
                }
            }
        }
    } else {
        let json = serde_json::json!({"deploymentMode": "3p"});
        if let Ok(out) = serde_json::to_string_pretty(&json) {
            let _ = std::fs::write(&desktop_tmp, &out);
            let _ = std::fs::rename(&desktop_tmp, &desktop_cfg_path);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let normal_dir = claude_dir.parent()
            .map(|p| p.join("Claude"))
            .unwrap_or_else(|| {
                let home = std::env::var("APPDATA").unwrap_or_default();
                PathBuf::from(home).join("Claude")
            });
        let _ = std::fs::create_dir_all(&normal_dir);

        let dev_settings = normal_dir.join("developer_settings.json");
        if !dev_settings.exists() {
            let _ = std::fs::write(&dev_settings, r#"{"allowDevTools":true}"#);
        }

        let normal_config = normal_dir.join("config.json");
        if !normal_config.exists() {
            let _ = std::fs::write(&normal_config, r#"{"locale":"zh-CN","hasTrackedInitialActivation":true}"#);
        }

        let normal_cfg = normal_dir.join("claude_desktop_config.json");
        let normal_tmp = normal_cfg.with_extension("json.tmp");
        if normal_cfg.exists() {
            let content = std::fs::read_to_string(&normal_cfg).unwrap_or_default();
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if json.get("deploymentMode").and_then(|v| v.as_str()) != Some("3p") {
                    let mut json = json;
                    json["deploymentMode"] = serde_json::json!("3p");
                    if let Ok(out) = serde_json::to_string_pretty(&json) {
                        let _ = std::fs::write(&normal_tmp, &out);
                        let _ = std::fs::rename(&normal_tmp, &normal_cfg);
                    }
                }
            }
        } else {
            let json = serde_json::json!({"deploymentMode": "3p"});
            if let Ok(out) = serde_json::to_string_pretty(&json) {
                let _ = std::fs::write(&normal_tmp, &out);
                let _ = std::fs::rename(&normal_tmp, &normal_cfg);
            }
        }

        let p3_dev = claude_dir.join("developer_settings.json");
        if !p3_dev.exists() {
            let _ = std::fs::write(&p3_dev, r#"{"allowDevTools":true}"#);
        }

        let p3_config = claude_dir.join("config.json");
        if !p3_config.exists() {
            let _ = std::fs::write(&p3_config, r#"{"locale":"zh-CN","hasTrackedInitialActivation":true}"#);
        }

        // Also write to other possible paths as fallback
        let appdata = PathBuf::from(std::env::var("APPDATA").unwrap_or_default());
        let localappdata = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default());
        let fallback_dirs = [
            (appdata.join("Claude-3p"), appdata.join("Claude")),
            (localappdata.join("Claude-3p"), localappdata.join("Claude")),
        ];
        for (fb_3p, fb_claude) in &fallback_dirs {
            if fb_3p == &*claude_dir { continue; }
            let _ = std::fs::create_dir_all(fb_claude);
            let _ = std::fs::create_dir_all(fb_3p);
            let deploy_json = serde_json::json!({"deploymentMode": "3p"});
            if let Ok(out) = serde_json::to_string_pretty(&deploy_json) {
                let _ = std::fs::write(fb_claude.join("claude_desktop_config.json"), &out);
                let _ = std::fs::write(fb_3p.join("claude_desktop_config.json"), &out);
            }
            let dev = fb_claude.join("developer_settings.json");
            if !dev.exists() { let _ = std::fs::write(&dev, r#"{"allowDevTools":true}"#); }
        }
    }


    Ok(format!("Written to {}", config_file.display()))
}

// === Handlers ===

async fn ui_handler() -> Html<&'static str> {
    Html(HTML)
}

async fn get_config_handler(State(state): State<Arc<AppState>>) -> Json<Config> {
    let config = state.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    Json(config)
}

async fn save_config_handler(
    State(state): State<Arc<AppState>>,
    Json(mut new_config): Json<Config>,
) -> Json<serde_json::Value> {
    normalize_config(&mut new_config);
    if let Err(e) = save_config_file(&new_config) {
        return Json(serde_json::json!({"ok": false, "message": e}));
    }
    *state.config.write().unwrap_or_else(|e| e.into_inner()) = new_config;
    eprintln!("[config] saved");
    Json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
struct TestRequest {
    target_url: String,
    api_key: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    model: String,
}

async fn test_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TestRequest>,
) -> Json<serde_json::Value> {
    let mut models: Vec<String> = req
        .models
        .into_iter()
        .chain(std::iter::once(req.model))
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    let mut unique: Vec<String> = Vec::new();
    for m in models.drain(..) {
        if !unique.iter().any(|u| u.eq_ignore_ascii_case(&m)) {
            unique.push(m);
        }
    }
    if req.target_url.is_empty() || req.api_key.is_empty() || unique.is_empty() {
        return Json(serde_json::json!({"ok": false, "message": "Please fill in URL, Key, and model name."}));
    }
    if !req.target_url.starts_with("http://") && !req.target_url.starts_with("https://") {
        return Json(serde_json::json!({"ok": false, "message": "URL must start with http:// or https://"}));
    }

    let base = req.target_url.trim_end_matches('/');
    let url = format!("{}/v1/messages", base);

    let test_client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| state.client.clone());

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut ok_count = 0usize;
    for model in &unique {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });

        let resp = test_client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", req.api_key))
            .header("anthropic-version", "2023-06-01")
            .body(serde_json::to_vec(&body).unwrap_or_default())
            .send()
            .await;

        let (ok, message) = match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                if status == 200 {
                    (true, format!("HTTP {}", status))
                } else {
                    let body = r.text().await.unwrap_or_default();
                    let msg = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| {
                            v.get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .map(String::from)
                        })
                        .unwrap_or_else(|| format!("HTTP {}", status));
                    (false, msg)
                }
            }
            Err(e) => {
                let msg = if e.is_connect() {
                    "Cannot connect. Check the URL.".to_string()
                } else if e.is_timeout() {
                    "Connection timed out.".to_string()
                } else {
                    format!("Error: {}", e)
                };
                (false, msg)
            }
        };
        if ok {
            ok_count += 1;
        }
        results.push(serde_json::json!({
            "model": model,
            "ok": ok,
            "message": message,
        }));
    }

    let all_ok = ok_count == unique.len();
    let message = if all_ok {
        format!("{}/{} models available", ok_count, unique.len())
    } else {
        format!("{}/{} models available — check the failing model IDs", ok_count, unique.len())
    };
    Json(serde_json::json!({"ok": all_ok, "message": message, "results": results}))
}

async fn slots_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let slots = state.slots.read().unwrap_or_else(|e| e.into_inner()).clone();
    let data: Vec<serde_json::Value> = slots
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "description": s.description,
            })
        })
        .collect();
    Json(serde_json::Value::Array(data))
}

async fn apply_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let config = state.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let slots = official_slots();
    *state.slots.write().unwrap_or_else(|e| e.into_inner()) = slots.clone();
    match apply_to_claude_desktop(&config, &slots) {
        Ok(msg) => {
            eprintln!("[apply] {}", msg);
            restart_claude_desktop();
            Json(serde_json::json!({"ok": true, "message": "Applied! Claude Desktop is restarting..."}))
        }
        Err(e) => Json(serde_json::json!({"ok": false, "message": e})),
    }
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let offset_secs: i64 = {
        #[cfg(target_os = "macos")]
        {
            let mut now: libc::time_t = 0;
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            unsafe { libc::time(&mut now); libc::localtime_r(&now, &mut tm); }
            tm.tm_gmtoff
        }
        #[cfg(not(target_os = "macos"))]
        { 8 * 3600 }
    };
    let local = (d as i64 + offset_secs) as u64;
    let h = (local % 86400) / 3600;
    let m = (local % 3600) / 60;
    let s = local % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

async fn logs_handler(State(state): State<Arc<AppState>>) -> Json<Vec<LogEntry>> {
    let logs = state.logs.read().unwrap_or_else(|e| e.into_inner()).clone();
    Json(logs)
}

fn autostart_plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Library/LaunchAgents/com.slotshift.plist")
}

fn legacy_autostart_plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Library/LaunchAgents/com.modelswitch.plist")
}

/// ModelSwitch wrote a `com.modelswitch.plist` pointing at the old bundle.
/// Rewrite it for the new app so login autostart keeps working.
fn migrate_autostart() {
    let legacy = legacy_autostart_plist_path();
    if legacy.exists() {
        let _ = std::fs::remove_file(&legacy);
        match set_autostart(true) {
            Ok(()) => eprintln!("[autostart] migrated to com.slotshift.plist"),
            Err(e) => eprintln!("[autostart] migration failed: {}", e),
        }
    }
}

fn is_autostart_enabled() -> bool {
    autostart_plist_path().exists()
}

fn set_autostart(enabled: bool) -> Result<(), String> {
    let plist_path = autostart_plist_path();
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe_str = exe.display().to_string()
            .replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
            .replace('"', "&quot;").replace('\'', "&apos;");
        let content = format!(
r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.slotshift</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#, exe_str);
        let dir = plist_path.parent().ok_or("Invalid path")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        std::fs::write(&plist_path, content).map_err(|e| e.to_string())?;
    } else {
        let _ = std::fs::remove_file(&plist_path);
    }
    Ok(())
}

#[derive(Deserialize)]
struct AutostartRequest { enabled: bool }

async fn autostart_get_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({"enabled": is_autostart_enabled()}))
}

async fn autostart_set_handler(Json(req): Json<AutostartRequest>) -> Json<serde_json::Value> {
    match set_autostart(req.enabled) {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "message": e})),
    }
}

struct ScopeGuard<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> Drop for ScopeGuard<F> {
    fn drop(&mut self) { if let Some(f) = self.0.take() { f(); } }
}
fn scopeguard<F: FnOnce()>(f: F) -> ScopeGuard<F> { ScopeGuard(Some(f)) }

static RESTARTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn restart_claude_desktop() {
    if RESTARTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let _guard = scopeguard(|| RESTARTING.store(false, std::sync::atomic::Ordering::SeqCst));
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("osascript")
                .args(["-e", "tell application \"Claude\" to quit"])
                .output();
            for _ in 0..15 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let out = std::process::Command::new("pgrep")
                    .args(["-x", "Claude"])
                    .output();
                if let Ok(o) = out {
                    if o.stdout.is_empty() {
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = std::process::Command::new("open")
                .args(["-a", "Claude"])
                .output();
            eprintln!("[restart] Claude Desktop restarted.");
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("powershell")
                .args(["-WindowStyle", "Hidden", "-Command", r#"
                    $proc = Get-Process -Name 'Claude' -ErrorAction SilentlyContinue | Select-Object -First 1
                    $path = if ($proc) { $proc.Path } else { $null }
                    Stop-Process -Name 'Claude' -Force -ErrorAction SilentlyContinue
                    Start-Sleep -Seconds 3
                    if ($path -like '*WindowsApps*') {
                        $pkg = Get-AppxPackage | Where-Object { $path.StartsWith($_.InstallLocation) } | Select-Object -First 1
                        if ($pkg) { explorer.exe "shell:AppsFolder\$($pkg.PackageFamilyName)!Claude" }
                    } elseif ($path) {
                        Start-Process $path
                    }
                "#])
                .output();
            eprintln!("[restart] Claude Desktop restarted.");
        }
    });
}

async fn proxy_fallback(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<Body>,
) -> axum::response::Response {
    let (parts, body) = req.into_parts();

    if parts.method == Method::GET && parts.uri.path().contains("/v1/models") {
        let config = state.config.read().unwrap_or_else(|e| e.into_inner()).clone();
        let slots = state.slots.read().unwrap_or_else(|e| e.into_inner()).clone();
        let flat = flatten_config(&config, &slots);
        let models: Vec<serde_json::Value> = flat
            .iter()
            .map(|e| {
                let display = if e.label.is_empty() {
                    e.slot_name.clone()
                } else {
                    e.label.clone()
                };
                serde_json::json!({
                    "type": "model",
                    "id": e.slot,
                    "display_name": display,
                    "created_at": "2025-01-01T00:00:00Z"
                })
            })
            .collect();
        let first = models.first().and_then(|m| m.get("id")).cloned();
        let last = models.last().and_then(|m| m.get("id")).cloned();
        let resp = serde_json::json!({
            "data": models,
            "has_more": false,
            "first_id": first,
            "last_id": last,
        });
        return Json(resp).into_response();
    }

    if parts.method != Method::POST {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let config = state.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let slots = state.slots.read().unwrap_or_else(|e| e.into_inner()).clone();

    let mut data: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(d) => d,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let resolved = if let Some(model) = data.get("model").and_then(|m| m.as_str()) {
        let r = resolve_model(model, &config, &slots);
        eprintln!("  model: {} -> {} ({})", model, r.model, r.target_url);
        data["model"] = serde_json::json!(r.model);
        r
    } else {
        ResolvedModel {
            model: String::new(),
            target_url: String::new(),
            api_key: String::new(),
            effort_mode: "claude5".to_string(),
            fast_low: false,
        }
    };

    let effective_effort = apply_effort(&mut data, &resolved.effort_mode, resolved.fast_low);

    if resolved.target_url.is_empty() {
        eprintln!("  error: no target URL configured for this model");
        return (StatusCode::BAD_GATEWAY, "No API URL configured for this model. Please configure the provider in the proxy app.").into_response();
    }

    let base = resolved.target_url.trim_end_matches('/');
    let url = format!("{}{}", base, parts.uri.path());

    let mut req_builder = state
        .client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", resolved.api_key))
        .header(
            "anthropic-version",
            parts
                .headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("2023-06-01"),
        );

    for h in ["anthropic-beta", "x-api-key", "user-agent"] {
        if let Some(v) = parts.headers.get(h).and_then(|v| v.to_str().ok()) {
            req_builder = req_builder.header(h, v);
        }
    }

    let resp = match req_builder
        .body(serde_json::to_vec(&data).unwrap_or_default())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  proxy error: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response();
        }
    };

    let raw_status = resp.status().as_u16();
    let status = StatusCode::from_u16(raw_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    if let Some(model) = data.get("model").and_then(|m| m.as_str()) {
        let entry = LogEntry {
            time: chrono_now(),
            model: model.to_string(),
            status: raw_status,
            effort: effective_effort.clone(),
        };
        let mut logs = state.logs.write().unwrap_or_else(|e| e.into_inner());
        logs.push(entry);
        let len = logs.len();
        if len > MAX_LOGS { logs.drain(0..len - MAX_LOGS); }
    }

    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        if k != "transfer-encoding" && k != "connection" {
            headers.insert(k.clone(), v.clone());
        }
    }

    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);

    (status, headers, body).into_response()
}

fn start_server() -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("Failed to create runtime: {}", e))?;
    rt.block_on(async {
        let config = load_config();
        let _ = save_config_file(&config);
        let slots = official_slots();

        eprintln!("SlotShift v1.2");
        eprintln!("Proxy: http://127.0.0.1:{}", PORT);
        eprintln!("Providers: {}", config.providers.len());
        eprintln!(
            "Model slots: {}",
            slots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
        );

        let state = Arc::new(AppState {
            config: RwLock::new(config),
            slots: RwLock::new(slots),
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .map_err(|e| format!("Failed to create HTTP client: {}", e))?,
            logs: RwLock::new(Vec::new()),
        });

        let app = Router::new()
            .route("/", get(ui_handler))
            .route("/api/config", get(get_config_handler).post(save_config_handler))
            .route("/api/slots", get(slots_handler))
            .route("/api/test", post(test_handler))
            .route("/api/apply", post(apply_handler))
            .route("/api/logs", get(logs_handler))
            .route("/api/autostart", get(autostart_get_handler).post(autostart_set_handler))
            .fallback(proxy_fallback)
            .with_state(state);

        let listener = TcpListener::bind(format!("127.0.0.1:{}", PORT))
            .await
            .map_err(|e| format!("Port {} already in use: {}. Please close the other instance first.", PORT, e))?;

        eprintln!("Server ready.");
        axum::serve(listener, app).await.map_err(|e| format!("Server error: {}", e))
    })
}

fn make_tray_icon() -> tray_icon::Icon {
    let size = 22usize;
    let mut rgba = vec![0u8; size * size * 4];
    let center = size as f64 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            if (dx * dx + dy * dy).sqrt() <= 8.0 {
                let i = (y * size + x) * 4;
                rgba[i] = 0xD9;
                rgba[i + 1] = 0x77;
                rgba[i + 2] = 0x57;
                rgba[i + 3] = 0xFF;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, size as u32, size as u32).unwrap()
}

fn main() {
    use tao::{
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder},
        window::WindowBuilder,
    };
    use wry::WebViewBuilder;
    use muda::{Menu, MenuItem, MenuEvent, Submenu, PredefinedMenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    if std::env::args().any(|a| a == "--print-models") {
        let config = load_config();
        let slots = official_slots();
        let flat = flatten_config(&config, &slots);
        for e in &flat {
            if e.label.is_empty() {
                println!("{}  <-  {}", e.slot_name, e.name);
            } else {
                println!("{} ({})  <-  {}", e.label, e.slot_name, e.name);
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&build_inference_models(&config, &slots)).unwrap_or_default()
        );
        return;
    }

    migrate_autostart();
    ensure_claude_desktop_gateway();

    let server_err = Arc::new(std::sync::Mutex::new(None::<String>));
    let server_err_clone = server_err.clone();
    std::thread::spawn(move || {
        if let Err(e) = start_server() {
            eprintln!("Server failed: {}", e);
            *server_err_clone.lock().unwrap() = Some(e);
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(800));

    if let Some(err) = server_err.lock().unwrap().as_ref() {
        eprintln!("Cannot start: {}", err);
        #[cfg(target_os = "macos")]
        {
            let safe_err = err.replace('\\', "\\\\").replace('"', "\\\"");
            let _ = std::process::Command::new("osascript")
                .args(["-e", &format!("display dialog \"{}\" buttons {{\"OK\"}} with title \"SlotShift\" with icon stop", safe_err)])
                .output();
        }
        std::process::exit(1);
    }

    #[cfg(target_os = "macos")]
    use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

    let mut event_loop = EventLoopBuilder::new().build();
    #[cfg(target_os = "macos")]
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let app_menu = Menu::new();
    let edit_submenu = Submenu::new("Edit", true);
    let _ = edit_submenu.append(&PredefinedMenuItem::undo(None));
    let _ = edit_submenu.append(&PredefinedMenuItem::redo(None));
    let _ = edit_submenu.append(&PredefinedMenuItem::separator());
    let _ = edit_submenu.append(&PredefinedMenuItem::cut(None));
    let _ = edit_submenu.append(&PredefinedMenuItem::copy(None));
    let _ = edit_submenu.append(&PredefinedMenuItem::paste(None));
    let _ = edit_submenu.append(&PredefinedMenuItem::select_all(None));
    let _ = app_menu.append(&edit_submenu);

    #[cfg(target_os = "macos")]
    app_menu.init_for_nsapp();

    let window = WindowBuilder::new()
        .with_title("SlotShift")
        .with_inner_size(tao::dpi::LogicalSize::new(860.0, 760.0))
        .build(&event_loop)
        .expect("Failed to create window");

    let _webview = WebViewBuilder::new()
        .with_url(format!("http://127.0.0.1:{}", PORT))
        .build(&window)
        .expect("Failed to create webview");

    let tray_menu = Menu::new();
    let show_item = MenuItem::new("显示窗口", true, None);
    let quit_item = MenuItem::new("退出", true, None);
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    tray_menu.append(&show_item).unwrap();
    tray_menu.append(&quit_item).unwrap();

    let _tray = TrayIconBuilder::new()
        .with_icon(make_tray_icon())
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(false)
        .with_tooltip("SlotShift")
        .build()
        .expect("Failed to create tray icon");

    let menu_rx = MenuEvent::receiver().clone();
    let tray_rx = TrayIconEvent::receiver().clone();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Ok(ev) = menu_rx.try_recv() {
            if ev.id == show_id {
                window.set_visible(true);
                window.set_focus();
            } else if ev.id == quit_id {
                *control_flow = ControlFlow::Exit;
            }
        }

        if let Ok(event) = tray_rx.try_recv() {
            if let TrayIconEvent::Click { button: tray_icon::MouseButton::Left, .. } = event {
                window.set_visible(true);
                window.set_focus();
            }
        }

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                window.set_visible(false);
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_two_models() -> Config {
        Config {
            show_model_names: false,
            providers: vec![Provider {
                target_url: "https://api.deepseek.com/anthropic".to_string(),
                api_key: "key".to_string(),
                effort_mode: "auto".to_string(),
                thinking_effort: String::new(),
                fast_low: false,
                models: vec![
                    ModelEntry {
                        name: "deepseek-v4-pro".to_string(),
                        label: String::new(),
                    },
                    ModelEntry {
                        name: "deepseek-flash".to_string(),
                        label: String::new(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn fallback_slots_start_with_current_models() {
        let slots = fallback_slots();
        assert_eq!(slots[0].id, "claude-opus-5-5");
        assert_eq!(slots[0].name, "Opus 5.5");
        assert_eq!(slots[1].id, "claude-sonnet-5-5");
        assert_eq!(slots[2].id, "claude-fable-5-2");
        assert_eq!(slots[3].id, "claude-haiku-5-5");
        let mut ids: Vec<&str> = slots.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), slots.len());
    }

    #[test]
    fn base64_decodes_catalog_payloads() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("eyJhIjoxfQ==").unwrap(), br#"{"a":1}"#);
    }

    #[test]
    fn inference_models_are_one_per_model_without_1m_variants() {
        let slots = fallback_slots();
        let config = config_with_two_models();
        let models = build_inference_models(&config, &slots);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["name"], "claude-opus-5-5");
        assert_eq!(models[1]["name"], "claude-sonnet-5-5");
        for m in &models {
            assert!(m.get("supports1m").is_none());
            assert!(m.get("labelOverride").is_none());
            assert_eq!(m["maxEffort"], "high");
        }
    }

    #[test]
    fn real_model_names_become_labels_when_enabled() {
        let slots = fallback_slots();
        let mut config = config_with_two_models();
        config.show_model_names = true;
        let models = build_inference_models(&config, &slots);
        assert_eq!(models[0]["labelOverride"], "deepseek-v4-pro");
        assert_eq!(models[1]["labelOverride"], "deepseek-flash");
    }

    #[test]
    fn custom_labels_win_over_generated_names() {
        let slots = fallback_slots();
        let mut config = config_with_two_models();
        config.show_model_names = true;
        config.providers[0].models[0].label = "DeepSeek V4 Pro".to_string();
        let models = build_inference_models(&config, &slots);
        assert_eq!(models[0]["labelOverride"], "DeepSeek V4 Pro");
    }

    #[test]
    fn resolves_current_and_stale_slot_names() {
        let slots = fallback_slots();
        let config = config_with_two_models();

        let r = resolve_model("claude-opus-5-5", &config, &slots);
        assert_eq!(r.model, "deepseek-v4-pro");

        let r = resolve_model("claude-opus-5-5[1m]", &config, &slots);
        assert_eq!(r.model, "deepseek-v4-pro");

        // A session that still has an old Claude 3 model selected.
        let r = resolve_model("claude-3-5-sonnet-latest", &config, &slots);
        assert_eq!(r.model, "deepseek-flash");

        let r = resolve_model("haiku", &config, &slots);
        assert_eq!(r.model, "deepseek-flash");
    }

    #[test]
    fn apply_writes_one_model_entry_per_upstream_model() {
        let old_home = std::env::var("HOME").ok();
        let dir = std::env::temp_dir().join(format!("slotshift-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);

        let slots = fallback_slots();
        let config = config_with_two_models();
        apply_to_claude_desktop(&config, &slots).unwrap();

        let cfg_path = dir
            .join("Library/Application Support/Claude-3p/configLibrary")
            .join("a0a0a0a0-b1b1-4c2c-9d3d-e4e4e4e4e4e4.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        let models = json["inferenceModels"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["name"], "claude-opus-5-5");
        assert_eq!(models[1]["name"], "claude-sonnet-5-5");
        assert!(models[0].get("supports1m").is_none());
        assert!(models[0].get("prefer1m").is_none());
        assert!(models[0].get("labelOverride").is_none());
        assert_eq!(models[0]["maxEffort"], "high");
        assert_eq!(json["inferenceProvider"], "gateway");
        assert_eq!(json["inferenceGatewayBaseUrl"], "http://127.0.0.1:5678");

        if let Some(home) = old_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deepseek_mode_caps_effort_and_maps_levels() {
        assert_eq!(map_deepseek3("low"), "low");
        assert_eq!(map_deepseek3("medium"), "high");
        assert_eq!(map_deepseek3("high"), "max");
        assert_eq!(map_deepseek3("xhigh"), "max");
        assert_eq!(map_deepseek3("max"), "max");

        let mut req = serde_json::json!({
            "model": "deepseek-v4-pro",
            "thinking": {"type": "enabled", "budget_tokens": 32000},
            "output_config": {"effort": "medium"}
        });
        let effective = apply_effort(&mut req, "deepseek3", false);
        assert_eq!(effective, "high");
        assert_eq!(req["output_config"]["effort"], "high");
        assert_eq!(req["thinking"], serde_json::json!({"type": "enabled"}));
    }

    #[test]
    fn claude5_mode_passes_effort_through() {
        let mut req = serde_json::json!({"output_config": {"effort": "xhigh"}});
        assert_eq!(apply_effort(&mut req, "claude5", false), "xhigh");
        assert_eq!(req["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn off_mode_disables_thinking() {
        let mut req = serde_json::json!({
            "thinking": {"type": "enabled"},
            "output_config": {"effort": "max"}
        });
        assert_eq!(apply_effort(&mut req, "off", false), "off");
        assert_eq!(req["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(req.get("output_config").is_none());
    }

    #[test]
    fn legacy_fixed_effort_migrates() {
        let mut config = config_with_two_models();
        config.providers[0].effort_mode.clear();
        config.providers[0].thinking_effort = "off".to_string();
        normalize_config(&mut config);
        assert_eq!(config.providers[0].effort_mode, "off");
        assert!(config.providers[0].thinking_effort.is_empty());
    }

    #[test]
    fn fast_low_turns_low_into_direct_answer() {
        let mut req = serde_json::json!({
            "thinking": {"type": "enabled", "budget_tokens": 8192},
            "output_config": {"effort": "low"}
        });
        assert_eq!(apply_effort(&mut req, "deepseek3", true), "off");
        assert_eq!(req["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(req.get("output_config").is_none());

        let mut high = serde_json::json!({"output_config": {"effort": "high"}});
        assert_eq!(apply_effort(&mut high, "deepseek3", true), "max");
    }

    #[test]
    fn fast_low_adds_concise_directive() {
        let mut string_system = serde_json::json!({
            "system": "You are Claude Code.",
            "output_config": {"effort": "low"}
        });
        apply_effort(&mut string_system, "deepseek3", true);
        assert!(string_system["system"].as_str().unwrap().contains("Fast mode:"));

        let mut block_system = serde_json::json!({
            "system": [{"type": "text", "text": "You are Claude Code."}],
            "output_config": {"effort": "low"}
        });
        apply_effort(&mut block_system, "deepseek3", true);
        assert!(block_system["system"][1]["text"]
            .as_str()
            .unwrap()
            .contains("Fast mode:"));
    }
}
