use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            projects: Vec::new(),
        }
    }
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Cmd,
    Bash,
}

impl Default for ShellKind {
    fn default() -> Self {
        Self::Cmd
    }
}

impl ShellKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cmd => "命令提示符",
            Self::Bash => "Git Bash",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub command: String,
    /// 用哪种终端执行命令。旧配置没有这一项时按命令提示符处理。
    #[serde(default)]
    pub shell: ShellKind,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub delay_ms: u64,
}

#[derive(Clone, Copy)]
pub enum Template {
    FullStack,
    BackendOnly,
    FrontendOnly,
}

impl Project {
    pub fn from_template(template: Template) -> Self {
        let services = match template {
            Template::FullStack => vec![Service::named("后端"), Service::named("前端")],
            Template::BackendOnly => vec![Service::named("后端")],
            Template::FrontendOnly => vec![Service::named("前端")],
        };
        Self {
            id: new_id(),
            name: "新项目".to_string(),
            note: String::new(),
            services,
        }
    }
}

impl Service {
    pub fn named(name: &str) -> Self {
        Self {
            id: new_id(),
            name: name.to_string(),
            cwd: String::new(),
            command: String::new(),
            shell: ShellKind::Cmd,
            env: String::new(),
            delay_ms: 0,
        }
    }
}

pub fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{millis:x}{n:x}")
}

pub fn service_key(project_id: &str, service_id: &str) -> String {
    format!("{project_id}/{service_id}")
}

/// 配置文件位置。
/// 可执行文件旁边如果已经有 `config.json`，就用它（方便整目录拷走）。
/// 否则用 `%APPDATA%\RunWeb\config.json`。
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let portable = dir.join("config.json");
            if portable.is_file() {
                return portable;
            }
        }
    }
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("RunWeb").join("config.json")
}

pub fn load_config(path: &Path) -> (Config, Option<String>) {
    if !path.exists() {
        return (Config::default(), None);
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            return (
                Config::default(),
                Some(format!("无法读取配置：{err}")),
            );
        }
    };
    match serde_json::from_str::<Config>(&text) {
        Ok(config) => (config, None),
        Err(err) => {
            let backup = path.with_extension("json.bak");
            let _ = fs::copy(path, &backup);
            (
                Config::default(),
                Some(format!(
                    "配置文件无法解析，已备份到 {}。{err}",
                    backup.display()
                )),
            )
        }
    }
}

pub fn save_config(path: &Path, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut config = config.clone();
    config.version = CONFIG_VERSION;
    let data = serde_json::to_vec_pretty(&config).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data).map_err(|err| err.to_string())?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path).map_err(|err| err.to_string())
}

pub fn resolve_cwd(config_file: &Path, cwd: &str) -> PathBuf {
    let trimmed = cwd.trim();
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        let base = config_file.parent().unwrap_or_else(|| Path::new("."));
        base.join(path)
    }
}

pub fn command_is_blank(command: &str) -> bool {
    command.lines().all(|line| line.trim().is_empty())
}

pub fn parse_env(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("环境变量第 {} 行缺少等号：{line}", idx + 1));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("环境变量第 {} 行的名字是空的", idx + 1));
        }
        if key.contains([' ', '\t']) {
            return Err(format!("环境变量第 {} 行的名字不能有空格", idx + 1));
        }
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

pub fn fill_example_commands(project: &mut Project) {
    for service in &mut project.services {
        if !command_is_blank(&service.command) {
            continue;
        }
        service.command = if service.name.contains('前') {
            "npm run dev".to_string()
        } else if service.name.contains("后") {
            "cargo run".to_string()
        } else {
            "npm run dev".to_string()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut config = Config::default();
        config.projects.push(Project::from_template(Template::FullStack));
        let text = serde_json::to_string_pretty(&config).unwrap();
        let loaded: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].services.len(), 2);
        assert_eq!(loaded.projects[0].services[0].name, "后端");
        assert_eq!(loaded.projects[0].services[0].shell, ShellKind::Cmd);
    }

    #[test]
    fn old_config_without_shell_defaults_to_cmd() {
        let text = r#"{
            "version": 1,
            "projects": [{
                "id": "p",
                "name": "demo",
                "services": [{
                    "id": "s",
                    "name": "后端",
                    "cwd": "D:\\code\\demo",
                    "command": "go run main.go"
                }]
            }]
        }"#;
        let loaded: Config = serde_json::from_str(text).unwrap();
        assert_eq!(loaded.projects[0].services[0].shell, ShellKind::Cmd);
    }

    #[test]
    fn parse_env_skips_comments() {
        let env = parse_env("# comment\nPORT=3000\n\nNAME=hello world\n").unwrap();
        assert_eq!(
            env,
            vec![
                ("PORT".to_string(), "3000".to_string()),
                ("NAME".to_string(), "hello world".to_string())
            ]
        );
    }

    #[test]
    fn parse_env_reports_bad_line() {
        let err = parse_env("NOT_VALID").unwrap_err();
        assert!(err.contains("缺少等号"));
    }
}
