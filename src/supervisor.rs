use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::model::ShellKind;

#[cfg(windows)]
use winreg::enums::{RegType, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
#[cfg(windows)]
use winreg::types::FromRegValue;
#[cfg(windows)]
use winreg::RegKey;

struct Output {
    key: String,
    text: String,
}

struct Running {
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    pid: u32,
    started: Instant,
    script: Option<PathBuf>,
    #[cfg(windows)]
    job: Option<JobGuard>,
}

pub struct RunInfo {
    pub pid: u32,
    pub started: Instant,
}

pub struct Supervisor {
    running: HashMap<String, Running>,
    tx: Sender<Output>,
    rx: Receiver<Output>,
    waker: Arc<dyn Fn() + Send + Sync>,
}

impl Supervisor {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            running: HashMap::new(),
            tx,
            rx,
            waker: Arc::new(|| {}),
        }
    }

    pub fn set_waker(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.waker = waker;
    }

    pub fn is_running(&self, key: &str) -> bool {
        self.running.contains_key(key)
    }

    pub fn any_running(&self) -> bool {
        !self.running.is_empty()
    }

    pub fn info(&self, key: &str) -> Option<RunInfo> {
        self.running.get(key).map(|running| RunInfo {
            pid: running.pid,
            started: running.started,
        })
    }

    pub fn start(
        &mut self,
        key: impl Into<String>,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        shell: ShellKind,
    ) -> Result<(u32, bool), String> {
        let key = key.into();
        if self.running.contains_key(&key) {
            return Err("已经在运行".to_string());
        }
        if !cwd.is_dir() {
            return Err(format!("工作目录不存在：{}", cwd.display()));
        }

        let script = prepare_script(command, cwd, shell)?;
        let mut cmd = shell_command(script.path.as_deref(), command, shell)?;
        cmd.cwd(cwd);
        // 不用启动 RunWeb 的那个进程的环境。否则从编辑器里打开时，
        // 会把空的 GOMODCACHE 之类的变量带进项目，Go 就会重新下载工具链。
        apply_terminal_env(&mut cmd);
        if shell == ShellKind::Bash {
            cmd.env("MSYSTEM", "MINGW64");
        }
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("PYTHONIOENCODING", "utf-8");
        cmd.env("PYTHONUTF8", "1");
        for (name, value) in env {
            cmd.env(name, value);
        }

        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: 48,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| format!("无法创建终端：{err}"))?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| format!("启动失败：{err}"))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| format!("无法读取输出：{err}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| format!("无法连接终端输入：{err}"))?;
        drop(pair.slave);

        let pid = child.process_id().unwrap_or(0);
        #[cfg(windows)]
        let job = child.as_raw_handle().and_then(attach_job);

        let tx = self.tx.clone();
        let waker = Arc::clone(&self.waker);
        let read_key = key.clone();
        std::thread::Builder::new()
            .name("runweb-log".to_string())
            .spawn(move || read_output(read_key, reader, writer, tx, waker))
            .map_err(|err| format!("无法创建日志线程：{err}"))?;

        self.running.insert(
            key,
            Running {
                child,
                _master: pair.master,
                pid,
                started: Instant::now(),
                script: script.path,
                #[cfg(windows)]
                job,
            },
        );
        Ok((pid, script.warning))
    }

    pub fn stop(&mut self, key: &str) {
        if let Some(mut running) = self.running.remove(key) {
            terminate(&mut running);
        }
    }

    pub fn stop_all(&mut self) {
        let keys: Vec<String> = self.running.keys().cloned().collect();
        for key in keys {
            self.stop(&key);
        }
    }

    pub fn poll_output(&mut self) -> Vec<(String, String)> {
        let mut lines = Vec::new();
        while let Ok(output) = self.rx.try_recv() {
            lines.push((output.key, output.text));
        }
        lines
    }

    /// 已经退出的进程。返回 `(key, exit_code)`。
    pub fn poll_exits(&mut self) -> Vec<(String, u32)> {
        let mut finished = Vec::new();
        let keys: Vec<String> = self.running.keys().cloned().collect();
        for key in keys {
            let code = {
                let Some(running) = self.running.get_mut(&key) else {
                    continue;
                };
                match running.child.try_wait() {
                    Ok(Some(status)) => Some(status.exit_code()),
                    Ok(None) => None,
                    Err(_) => Some(1),
                }
            };
            if let Some(code) = code {
                if let Some(running) = self.running.remove(&key) {
                    if let Some(path) = &running.script {
                        let _ = std::fs::remove_file(path);
                    }
                }
                finished.push((key, code));
            }
        }
        finished
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_all();
    }
}

struct PreparedScript {
    path: Option<PathBuf>,
    #[allow(dead_code)]
    warning: bool,
}

fn apply_terminal_env(cmd: &mut CommandBuilder) {
    #[cfg(windows)]
    {
        let fresh = fresh_terminal_env();
        let usable = fresh
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("Path"))
            && fresh
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("SystemRoot"));
        if usable {
            cmd.env_clear();
            for (key, value) in fresh {
                cmd.env(key, value);
            }
            return;
        }
        cmd.env_remove("GOMODCACHE");
        cmd.env_remove("CARGO_TARGET_DIR");
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(windows)]
struct EnvMap {
    entries: std::collections::BTreeMap<String, (String, String)>,
}

#[cfg(windows)]
impl EnvMap {
    fn new() -> Self {
        Self {
            entries: std::collections::BTreeMap::new(),
        }
    }

    fn insert(&mut self, name: String, value: String) {
        let key = name.to_ascii_lowercase();
        self.entries.insert(key, (name, value));
    }

    fn append_path(&mut self, name: String, value: String) {
        let key = name.to_ascii_lowercase();
        if let Some((_, existing)) = self.entries.get_mut(&key) {
            if !existing.is_empty() && !existing.ends_with(';') {
                existing.push(';');
            }
            existing.push_str(&value);
        } else {
            self.entries.insert(key, (name, value));
        }
    }

    fn into_vec(self) -> Vec<(String, String)> {
        self.entries.into_values().collect()
    }
}

#[cfg(windows)]
fn fresh_terminal_env() -> Vec<(String, String)> {
    let mut map = EnvMap::new();
    load_registry_env(
        &mut map,
        HKEY_LOCAL_MACHINE,
        r"System\CurrentControlSet\Control\Session Manager\Environment",
        false,
    );
    load_registry_env(&mut map, HKEY_CURRENT_USER, "Environment", true);
    load_volatile_env(&mut map);
    map.into_vec()
}

#[cfg(windows)]
fn load_registry_env(map: &mut EnvMap, root: winreg::HKEY, path: &str, merge_path: bool) {
    let Ok(key) = RegKey::predef(root).open_subkey(path) else {
        return;
    };
    let skip_username = !merge_path && path.contains("Session Manager");
    load_reg_values(&key, map, merge_path, skip_username);
}

#[cfg(windows)]
fn load_volatile_env(map: &mut EnvMap) {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Volatile Environment") else {
        return;
    };
    load_reg_values(&key, map, false, false);
    for name in key.enum_keys().filter_map(Result::ok) {
        if let Ok(sub) = key.open_subkey(&name) {
            load_reg_values(&sub, map, false, false);
        }
    }
}

#[cfg(windows)]
fn load_reg_values(key: &RegKey, map: &mut EnvMap, merge_path: bool, skip_username: bool) {
    for item in key.enum_values() {
        let Ok((name, value)) = item else {
            continue;
        };
        if skip_username && name.eq_ignore_ascii_case("username") {
            continue;
        }
        let Ok(text) = String::from_reg_value(&value) else {
            continue;
        };
        let text = if value.vtype == RegType::REG_EXPAND_SZ {
            expand_env(&text)
        } else {
            text
        };
        if merge_path && name.eq_ignore_ascii_case("path") {
            map.append_path(name, text);
        } else {
            map.insert(name, text);
        }
    }
}

#[cfg(windows)]
fn expand_env(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_string();
    }
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = std::ffi::OsStr::new(raw)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let size = unsafe { ExpandEnvironmentStringsW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if size == 0 {
        return raw.to_string();
    }
    let mut buf = vec![0u16; size as usize];
    let written =
        unsafe { ExpandEnvironmentStringsW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if written == 0 {
        return raw.to_string();
    }
    let len = (written as usize).saturating_sub(1).min(buf.len());
    std::ffi::OsString::from_wide(&buf[..len])
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn find_git_bash() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        for sub in [
            r"SOFTWARE\GitForWindows",
            r"SOFTWARE\WOW6432Node\GitForWindows",
        ] {
            if let Ok(key) = RegKey::predef(root).open_subkey(sub) {
                if let Ok(dir) = key.get_value::<String, _>("InstallPath") {
                    candidates.push(PathBuf::from(dir).join("bin").join("bash.exe"));
                }
            }
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(dir) = std::env::var(var) {
            candidates.push(PathBuf::from(dir).join("Git").join("bin").join("bash.exe"));
        }
    }
    candidates.push(PathBuf::from(r"D:\Program Files\Git\bin\bash.exe"));
    candidates.push(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"));
    for path in &candidates {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let lower = dir.to_string_lossy().to_ascii_lowercase();
            if lower.contains("\\windows\\system32")
                || lower.contains("\\windows\\winsxs")
                || lower.contains("windowsapps")
            {
                continue;
            }
            let bash = dir.join("bash.exe");
            if bash.is_file() {
                return Ok(bash);
            }
        }
    }
    Err("没有找到 Git Bash。请安装 Git for Windows，或把运行环境改成命令提示符。".to_string())
}

fn to_msys_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//") {
        return format!("//{rest}");
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'/'
    {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        return format!("/{drive}{}", &text[2..]);
    }
    text
}

fn bash_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn prepare_script(command: &str, cwd: &Path, shell: ShellKind) -> Result<PreparedScript, String> {
    #[cfg(windows)]
    {
        let dir = launcher_dir()?;
        match shell {
            ShellKind::Cmd => {
                let path = dir.join(format!("run-{}.cmd", crate::model::new_id()));
                let body = command.replace("\r\n", "\n").replace('\n', "\r\n");
                // 批处理按系统 ANSI（中文 Windows 是 GBK）解析。输出交给 ConPTY 转成 Unicode。
                let script = format!("@echo off\r\n{body}\r\n");
                let (encoded, _, had_errors) = encoding_rs::GBK.encode(&script);
                std::fs::write(&path, &encoded).map_err(|err| format!("无法写入启动脚本：{err}"))?;
                Ok(PreparedScript {
                    path: Some(path),
                    warning: had_errors,
                })
            }
            ShellKind::Bash => {
                let _ = find_git_bash()?;
                let path = dir.join(format!("run-{}.sh", crate::model::new_id()));
                let body = command.replace("\r\n", "\n").replace('\r', "\n");
                let dir_quoted = bash_single_quote(&to_msys_path(cwd));
                // Git Bash 遇到 CRLF 会把 \r 当成命令的一部分。
                let script = format!("cd -- {dir_quoted} || exit $?\n{body}\n");
                std::fs::write(&path, script.as_bytes())
                    .map_err(|err| format!("无法写入启动脚本：{err}"))?;
                Ok(PreparedScript {
                    path: Some(path),
                    warning: false,
                })
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (command, cwd, shell);
        Ok(PreparedScript {
            path: None,
            warning: false,
        })
    }
}

fn launcher_dir() -> Result<PathBuf, String> {
    let mut candidates = vec![
        std::env::temp_dir().join("RunWeb"),
        PathBuf::from(r"C:\Windows\Temp\RunWeb"),
    ];
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join("RunWeb").join("launch"));
    }
    let mut fallback = None;
    for dir in candidates {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        if !path_has_space(&dir) {
            return Ok(dir);
        }
        fallback = Some(dir);
    }
    fallback.ok_or_else(|| "无法创建临时目录".to_string())
}

fn path_has_space(path: &Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '"' || ch == '&')
}

#[cfg(windows)]
fn shorten_path(path: &Path) -> PathBuf {
    if !path_has_space(path) {
        return path.to_path_buf();
    }
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 || len as usize >= buf.len() {
        return path.to_path_buf();
    }
    buf.truncate(len as usize);
    PathBuf::from(std::ffi::OsString::from_wide(&buf))
}

fn shell_command(
    script: Option<&Path>,
    command: &str,
    shell: ShellKind,
) -> Result<CommandBuilder, String> {
    #[cfg(windows)]
    {
        let _ = command;
        let path = script.ok_or_else(|| "缺少启动脚本".to_string())?;
        match shell {
            ShellKind::Cmd => {
                let path = shorten_path(path);
                let mut cmd = CommandBuilder::new("cmd.exe");
                cmd.arg("/D");
                cmd.arg("/C");
                cmd.arg(path);
                Ok(cmd)
            }
            ShellKind::Bash => {
                let bash = find_git_bash()?;
                let mut cmd = CommandBuilder::new(bash);
                cmd.arg("--login");
                cmd.arg(path.to_string_lossy().replace('\\', "/"));
                Ok(cmd)
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (script, shell);
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-lc");
        cmd.arg(command);
        Ok(cmd)
    }
}

fn read_output(
    key: String,
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn std::io::Write + Send>,
    tx: Sender<Output>,
    waker: Arc<dyn Fn() + Send + Sync>,
) {
    let mut buf = [0_u8; 8192];
    let mut pending = Vec::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                answer_terminal_queries(&mut pending, &mut writer);
                let mut produced = false;
                drain_lines(&mut pending, |text| {
                    produced = true;
                    let _ = tx.send(Output {
                        key: key.clone(),
                        text,
                    });
                });
                if produced {
                    waker();
                }
            }
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let text = sanitize_line(&decode_bytes(&pending));
        if !text.is_empty() {
            let _ = tx.send(Output { key, text });
        }
    }
    waker();
}

/// ConPTY 会先询问光标位置（ESC [ 6 n）。不回答的话，cmd 会一直停在那里。
fn answer_terminal_queries(pending: &mut Vec<u8>, writer: &mut dyn std::io::Write) {
    let mut kept = Vec::with_capacity(pending.len());
    let mut index = 0;
    while index < pending.len() {
        if pending[index] == 0x1b {
            if let Some((len, reply)) = terminal_query(&pending[index..]) {
                let _ = writer.write_all(reply);
                let _ = writer.flush();
                index += len;
                continue;
            }
            if terminal_query_prefix(&pending[index..]) {
                kept.extend_from_slice(&pending[index..]);
                break;
            }
        }
        kept.push(pending[index]);
        index += 1;
    }
    *pending = kept;
}

fn terminal_query(bytes: &[u8]) -> Option<(usize, &'static [u8])> {
    if bytes.starts_with(b"\x1b[6n") {
        Some((4, b"\x1b[1;1R"))
    } else if bytes.starts_with(b"\x1b[5n") {
        Some((4, b"\x1b[0n"))
    } else if bytes.starts_with(b"\x1b[0c") {
        Some((4, b"\x1b[?1;0c"))
    } else if bytes.starts_with(b"\x1b[c") {
        Some((3, b"\x1b[?1;0c"))
    } else {
        None
    }
}

fn terminal_query_prefix(bytes: &[u8]) -> bool {
    const QUERIES: &[&[u8]] = &[b"\x1b[6n", b"\x1b[5n", b"\x1b[0c", b"\x1b[c"];
    QUERIES
        .iter()
        .any(|query| query.starts_with(bytes) && bytes.len() < query.len())
}

fn drain_lines(pending: &mut Vec<u8>, mut emit: impl FnMut(String)) {
    loop {
        let Some(pos) = pending.iter().position(|&byte| byte == b'\n') else {
            break;
        };
        let mut line: Vec<u8> = pending.drain(..=pos).collect();
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if let Some(cr) = line.iter().rposition(|&byte| byte == b'\r') {
            line.drain(..=cr);
        }
        let text = sanitize_line(&decode_bytes(&line));
        if !text.is_empty() {
            emit(text);
        }
    }
    if pending.len() > 32 * 1024 {
        let text = sanitize_line(&decode_bytes(pending));
        pending.clear();
        if !text.is_empty() {
            emit(text);
        }
    }
}

fn decode_bytes(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let (decoded, _, _) = encoding_rs::GBK.decode(bytes);
    decoded.into_owned()
}

fn sanitize_line(input: &str) -> String {
    strip_ansi(input)
        .chars()
        .filter(|ch| *ch == '\t' || !ch.is_control())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

fn terminate(running: &mut Running) {
    #[cfg(windows)]
    {
        if running.pid != 0 {
            taskkill_tree(running.pid);
        }
        if let Some(job) = &running.job {
            job.terminate();
        }
    }
    let _ = running.child.kill();
    if let Some(path) = &running.script {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
fn attach_job(handle: std::os::windows::io::RawHandle) -> Option<JobGuard> {
    let job = JobGuard::create()?;
    if job.assign(handle) {
        Some(job)
    } else {
        None
    }
}

#[cfg(windows)]
fn taskkill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(windows)]
struct JobGuard {
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
unsafe impl Send for JobGuard {}

#[cfg(windows)]
impl JobGuard {
    fn create() -> Option<Self> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut info: JobObjectExtendedLimitInformation = std::mem::zeroed();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
            );
            if ok == 0 {
                CloseHandle(handle);
                return None;
            }
            Some(Self { handle })
        }
    }

    fn assign(&self, process: std::os::windows::io::RawHandle) -> bool {
        unsafe { AssignProcessToJobObject(self.handle, process) != 0 }
    }

    fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;

#[cfg(windows)]
#[repr(C)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    _pad0: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    _pad1: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[cfg(windows)]
#[repr(C)]
struct JobObjectExtendedLimitInformation {
    basic: JobObjectBasicLimitInformation,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(windows)]
unsafe extern "system" {
    fn CreateJobObjectW(
        attrs: *mut std::ffi::c_void,
        name: *const u16,
    ) -> *mut std::ffi::c_void;
    fn SetInformationJobObject(
        job: *mut std::ffi::c_void,
        class: u32,
        info: *const std::ffi::c_void,
        len: u32,
    ) -> i32;
    fn AssignProcessToJobObject(
        job: *mut std::ffi::c_void,
        process: *mut std::ffi::c_void,
    ) -> i32;
    fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32;
    fn ExpandEnvironmentStringsW(src: *const u16, dst: *mut u16, size: u32) -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wait_until(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        pred()
    }

    #[test]
    fn strip_ansi_removes_color() {
        let text = strip_ansi("\u{1b}[31merror\u{1b}[0m ok");
        assert_eq!(text, "error ok");
    }

    #[test]
    fn decode_gbk_chinese() {
        let (bytes, _, _) = encoding_rs::GBK.encode("中文日志");
        assert_eq!(decode_bytes(&bytes), "中文日志");
    }

    #[test]
    fn echo_streams_to_log() {
        let mut supervisor = Supervisor::new();
        let cwd = std::env::temp_dir();
        supervisor
            .start("echo", &cwd, "echo hello-runweb", &[], ShellKind::Cmd)
            .unwrap();
        let mut lines = Vec::new();
        let saw = wait_until(
            || {
                lines.extend(supervisor.poll_output());
                lines.iter().any(|(_, line)| line.contains("hello-runweb"))
            },
            Duration::from_secs(8),
        );
        assert!(saw, "没有在日志里看到 hello-runweb，实际={lines:?}");
    }

    #[test]
    fn chinese_output_is_readable() {
        let mut supervisor = Supervisor::new();
        let cwd = std::env::temp_dir();
        supervisor
            .start("zh", &cwd, "echo 中文日志", &[], ShellKind::Cmd)
            .unwrap();
        let mut lines = Vec::new();
        let saw = wait_until(
            || {
                lines.extend(supervisor.poll_output());
                lines.iter().any(|(_, line)| line.contains("中文日志"))
            },
            Duration::from_secs(8),
        );
        supervisor.stop("zh");
        assert!(saw, "中文输出没有正确解码，实际={lines:?}");
    }

    #[test]
    fn stop_kills_process_tree() {
        let mut supervisor = Supervisor::new();
        let cwd = std::env::temp_dir();
        #[cfg(windows)]
        let command = "ping -n 40 127.0.0.1";
        #[cfg(not(windows))]
        let command = "sleep 40";
        let (pid, _) = supervisor
            .start("ping", &cwd, command, &[], ShellKind::Cmd)
            .unwrap();
        assert!(
            wait_until(
                || {
                    let got = supervisor.poll_output();
                    supervisor.is_running("ping") && !got.is_empty()
                },
                Duration::from_secs(15)
            ),
            "进程没有真正启动"
        );

        #[cfg(windows)]
        let children = child_pids(pid);

        supervisor.stop("ping");
        assert!(
            wait_until(|| !supervisor.is_running("ping"), Duration::from_secs(5)),
            "停止后进程仍被标记为运行中"
        );
        #[cfg(windows)]
        {
            assert!(!process_alive(pid), "cmd 进程还在");
            for child in children {
                assert!(!process_alive(child), "子进程 {child} 没有被一起关掉");
            }
        }
    }

    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        if pid == 0 {
            return false;
        }
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }

    #[cfg(windows)]
    fn child_pids(parent: u32) -> Vec<u32> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let script = format!(
            "Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq {parent} }} | ForEach-Object {{ $_.ProcessId }}"
        );
        let output = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|item| item.parse().ok())
            .collect()
    }

    #[test]
    fn msys_path_uses_drive_prefix() {
        assert_eq!(to_msys_path(Path::new(r"D:\code\kwjmai")), "/d/code/kwjmai");
        assert_eq!(
            bash_single_quote("/d/code/it's"),
            r"'/d/code/it'\''s'"
        );
    }

    #[cfg(windows)]
    #[test]
    fn terminal_env_drops_parent_module_cache() {
        let parent = std::env::var("GOMODCACHE").unwrap_or_default();
        let env = fresh_terminal_env();
        let child = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("GOMODCACHE"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("");
        if !parent.is_empty() {
            assert_ne!(
                child, parent,
                "子进程不应该继承启动器的 GOMODCACHE={parent}"
            );
        }
        let profile = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("USERPROFILE"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert!(
            !profile.is_empty(),
            "正常终端环境里应该有 USERPROFILE"
        );
        assert!(
            profile.to_ascii_lowercase().contains("users"),
            "USERPROFILE 应该是登录用户目录，实际={profile}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn child_go_env_uses_user_module_cache() {
        let mut supervisor = Supervisor::new();
        let cwd = std::env::temp_dir();
        supervisor
            .start("goenv", &cwd, "go env GOMODCACHE", &[], ShellKind::Cmd)
            .unwrap();
        let mut lines = Vec::new();
        let saw = wait_until(
            || {
                lines.extend(supervisor.poll_output());
                lines.iter().any(|(_, line)| {
                    let lower = line.to_ascii_lowercase();
                    lower.contains("pkg\\mod") || lower.contains("pkg/mod")
                })
            },
            Duration::from_secs(20),
        );
        supervisor.stop("goenv");
        let joined = lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(saw, "没有看到 GOMODCACHE，实际={joined}");
        assert!(
            !joined.to_ascii_lowercase().contains("cursor-sandbox-cache"),
            "Go 仍在用启动器的空缓存：{joined}"
        );
        assert!(
            !joined.contains("downloading"),
            "查询环境时不应该下载工具链：{joined}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_runs_login_shell() {
        if find_git_bash().is_err() {
            return;
        }
        let mut supervisor = Supervisor::new();
        let cwd = std::env::temp_dir();
        supervisor
            .start(
                "bash",
                &cwd,
                "echo bash-ready-$MSYSTEM",
                &[],
                ShellKind::Bash,
            )
            .unwrap();
        let mut lines = Vec::new();
        let saw = wait_until(
            || {
                lines.extend(supervisor.poll_output());
                lines
                    .iter()
                    .any(|(_, line)| line.contains("bash-ready-MINGW64"))
            },
            Duration::from_secs(20),
        );
        supervisor.stop("bash");
        assert!(saw, "Git Bash 没有按登录环境启动，实际={lines:?}");
    }
}
