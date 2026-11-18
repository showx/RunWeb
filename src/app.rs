use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::model::{
    command_is_blank, config_path, fill_example_commands, load_config, parse_env, resolve_cwd,
    save_config, service_key, Config, Project, Service, ShellKind, Template,
};
use crate::supervisor::Supervisor;

const MAX_LOGS: usize = 3000;
const SHOW_LOGS: usize = 1000;

const BG: egui::Color32 = egui::Color32::from_rgb(17, 19, 24);
const SIDEBAR: egui::Color32 = egui::Color32::from_rgb(23, 26, 33);
const CARD: egui::Color32 = egui::Color32::from_rgb(32, 36, 46);
const BORDER: egui::Color32 = egui::Color32::from_rgb(52, 58, 72);
const TEXT: egui::Color32 = egui::Color32::from_rgb(232, 236, 244);
const MUTED: egui::Color32 = egui::Color32::from_rgb(148, 156, 172);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(70, 140, 255);
const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 196, 126);
const RED: egui::Color32 = egui::Color32::from_rgb(240, 90, 102);
const AMBER: egui::Color32 = egui::Color32::from_rgb(240, 176, 72);
const LOG_BG: egui::Color32 = egui::Color32::from_rgb(12, 14, 18);
const LOG_TEXT: egui::Color32 = egui::Color32::from_rgb(214, 220, 230);
const SYS: egui::Color32 = egui::Color32::from_rgb(130, 170, 220);

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Output,
    System,
    Error,
}

#[derive(Clone)]
struct LogLine {
    seq: u64,
    service_name: String,
    at: String,
    text: String,
    kind: LogKind,
}

#[derive(Clone)]
struct PendingStart {
    at: Instant,
    project_id: String,
    service_id: String,
    tries: u8,
}

struct RestartAll {
    project_id: String,
    not_before: Instant,
    deadline: Instant,
}

enum Action {
    None,
    RunAll,
    StopAll,
    RestartAll,
    Start(String),
    Stop(String),
    Restart(String),
    Browse(String),
    OpenDir(String),
    FocusLogs(String),
    AddService,
    RemoveService(String),
    MoveService(String, i32),
    Duplicate,
    Delete,
    MoveProject(i32),
    FillExamples,
    SaveLogs,
    CopyLogs,
    ClearLogs,
}

pub struct RunWebApp {
    ctx: egui::Context,
    config: Config,
    config_path: std::path::PathBuf,
    supervisor: Supervisor,
    selected: Option<String>,
    config_mode: bool,
    log_focus: Option<String>,
    filter: String,
    auto_scroll: bool,
    banner: String,
    logs: HashMap<String, VecDeque<LogLine>>,
    seq: u64,
    pending: Vec<PendingStart>,
    restart_all: Option<RestartAll>,
    stopping: HashMap<String, bool>,
    exits: HashMap<String, u32>,
    confirm_delete: bool,
    title: String,
    logo: egui::TextureHandle,
}

impl RunWebApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_theme(&cc.egui_ctx);
        cleanup_old_launchers();
        let logo = cc.egui_ctx.load_texture(
            "runweb-logo",
            crate::logo::color_image(),
            egui::TextureOptions::LINEAR,
        );
        let config_path = config_path();
        let (config, warning) = load_config(&config_path);
        let selected = config.projects.first().map(|project| project.id.clone());
        let mut supervisor = Supervisor::new();
        let ctx = cc.egui_ctx.clone();
        let waker_ctx = ctx.clone();
        supervisor.set_waker(Arc::new(move || waker_ctx.request_repaint()));
        Self {
            ctx,
            config,
            config_path,
            supervisor,
            selected,
            config_mode: false,
            log_focus: None,
            filter: String::new(),
            auto_scroll: true,
            banner: warning.unwrap_or_else(|| "就绪".to_string()),
            logs: HashMap::new(),
            seq: 0,
            pending: Vec::new(),
            restart_all: None,
            stopping: HashMap::new(),
            exits: HashMap::new(),
            confirm_delete: false,
            title: "RunWeb".to_string(),
            logo,
        }
    }

    fn persist(&mut self) {
        if let Err(err) = save_config(&self.config_path, &self.config) {
            self.banner = format!("保存配置失败：{err}");
        }
    }

    fn project_index(&self, id: &str) -> Option<usize> {
        self.config.projects.iter().position(|project| project.id == id)
    }

    fn project(&self, id: &str) -> Option<&Project> {
        self.project_index(id)
            .map(|index| &self.config.projects[index])
    }

    fn service_index(&self, project_id: &str, service_id: &str) -> Option<(usize, usize)> {
        let project_index = self.project_index(project_id)?;
        let service_index = self.config.projects[project_index]
            .services
            .iter()
            .position(|service| service.id == service_id)?;
        Some((project_index, service_index))
    }

    fn pump(&mut self) {
        for (key, text) in self.supervisor.poll_output() {
            let name = self.service_name(&key);
            self.push_log(&key, &name, LogKind::Output, text);
        }
        for (key, code) in self.supervisor.poll_exits() {
            let name = self.service_name(&key);
            let intentional = self.stopping.remove(&key).unwrap_or(false);
            self.exits.insert(key.clone(), code);
            if intentional {
                self.push_log(&key, &name, LogKind::System, "已停止".to_string());
            } else if code == 0 {
                self.push_log(&key, &name, LogKind::System, "已结束".to_string());
            } else {
                self.push_log(
                    &key,
                    &name,
                    LogKind::Error,
                    format!("已退出，代码 {code}"),
                );
                self.banner = format!("{name} 已退出，代码 {code}");
            }
        }

        let now = Instant::now();
        let due: Vec<PendingStart> = self
            .pending
            .iter()
            .filter(|item| item.at <= now)
            .cloned()
            .collect();
        self.pending.retain(|item| item.at > now);
        for mut item in due {
            let key = service_key(&item.project_id, &item.service_id);
            if self.supervisor.is_running(&key) {
                item.tries += 1;
                if item.tries > 20 {
                    let name = self.service_name(&key);
                    self.push_log(
                        &key,
                        &name,
                        LogKind::Error,
                        "停止超时，没能重新启动".to_string(),
                    );
                } else {
                    item.at = Instant::now() + Duration::from_millis(200);
                    self.pending.push(item);
                }
                continue;
            }
            self.start_service(&item.project_id, &item.service_id);
        }

        if let Some(job) = self.restart_all.take() {
            let still = self.project_running(&job.project_id);
            if now >= job.not_before && (!still || now >= job.deadline) {
                self.run_project(&job.project_id);
            } else {
                self.restart_all = Some(job);
            }
        }
    }

    fn service_name(&self, key: &str) -> String {
        let Some((project_id, service_id)) = key.split_once('/') else {
            return "服务".to_string();
        };
        self.project(project_id)
            .and_then(|project| {
                project
                    .services
                    .iter()
                    .find(|service| service.id == service_id)
            })
            .map(|service| {
                if service.name.trim().is_empty() {
                    "未命名服务".to_string()
                } else {
                    service.name.clone()
                }
            })
            .unwrap_or_else(|| "服务".to_string())
    }

    fn push_log(&mut self, key: &str, service_name: &str, kind: LogKind, text: String) {
        let Some((project_id, service_id)) = key.split_once('/') else {
            return;
        };
        self.seq += 1;
        let line = LogLine {
            seq: self.seq,
            service_name: service_name.to_string(),
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            text,
            kind,
        };
        let buf = self.logs.entry(format!("{project_id}/{service_id}")).or_default();
        buf.push_back(line);
        while buf.len() > MAX_LOGS {
            buf.pop_front();
        }
    }

    fn project_running(&self, project_id: &str) -> bool {
        self.project(project_id).is_some_and(|project| {
            project.services.iter().any(|service| {
                self.supervisor
                    .is_running(&service_key(project_id, &service.id))
            })
        })
    }

    fn running_count(&self, project_id: &str) -> usize {
        self.project(project_id).map_or(0, |project| {
            project
                .services
                .iter()
                .filter(|service| {
                    self.supervisor
                        .is_running(&service_key(project_id, &service.id))
                })
                .count()
        })
    }

    fn start_service(&mut self, project_id: &str, service_id: &str) -> bool {
        let Some(service) = self
            .service_index(project_id, service_id)
            .map(|(p, s)| self.config.projects[p].services[s].clone())
        else {
            return false;
        };
        let key = service_key(project_id, service_id);
        let name = if service.name.trim().is_empty() {
            "未命名服务".to_string()
        } else {
            service.name.clone()
        };
        if self.supervisor.is_running(&key) {
            return false;
        }
        let (cwd, env) = match validate_service(&self.config_path, &service) {
            Ok(ready) => ready,
            Err(err) => {
                self.push_log(&key, &name, LogKind::Error, err.clone());
                self.banner = format!("{name}：{err}");
                return false;
            }
        };
        let command = service.command.trim().to_string();
        match self
            .supervisor
            .start(&key, &cwd, &command, &env, service.shell)
        {
            Ok((pid, encoding_warning)) => {
                self.exits.remove(&key);
                self.stopping.remove(&key);
                self.push_log(&key, &name, LogKind::System, format!("已启动，PID {pid}"));
                self.push_log(&key, &name, LogKind::System, format!("目录 {}", cwd.display()));
                self.push_log(&key, &name, LogKind::System, format!("命令 {command}"));
                self.push_log(
                    &key,
                    &name,
                    LogKind::System,
                    format!("环境 {}", service.shell.label()),
                );
                if encoding_warning {
                    self.push_log(
                        &key,
                        &name,
                        LogKind::Error,
                        "命令里有当前系统编码无法保存的字符，可能无法按原样执行".to_string(),
                    );
                }
                self.banner = format!("已启动 {name}");
                true
            }
            Err(err) => {
                self.push_log(&key, &name, LogKind::Error, err.clone());
                self.banner = format!("{name}：{err}");
                false
            }
        }
    }

    fn stop_service(&mut self, project_id: &str, service_id: &str) {
        let key = service_key(project_id, service_id);
        self.pending
            .retain(|item| !(item.project_id == project_id && item.service_id == service_id));
        if self.supervisor.is_running(&key) {
            self.stopping.insert(key.clone(), true);
            let name = self.service_name(&key);
            self.supervisor.stop(&key);
            self.push_log(&key, &name, LogKind::System, "正在停止…".to_string());
            self.banner = format!("正在停止 {name}");
        }
    }

    fn run_project(&mut self, project_id: &str) {
        let Some(services) = self.project(project_id).map(|project| project.services.clone()) else {
            return;
        };
        if services.is_empty() {
            self.banner = "这个项目还没有服务".to_string();
            return;
        }
        let mut cursor = Instant::now();
        let mut immediate = 0;
        let mut queued = 0;
        for service in &services {
            let at = cursor + Duration::from_millis(service.delay_ms);
            cursor = at;
            let key = service_key(project_id, &service.id);
            if self.supervisor.is_running(&key) {
                continue;
            }
            if service.delay_ms == 0 && at <= Instant::now() + Duration::from_millis(5) {
                if self.start_service(project_id, &service.id) {
                    immediate += 1;
                }
            } else {
                self.pending.push(PendingStart {
                    at,
                    project_id: project_id.to_string(),
                    service_id: service.id.clone(),
                    tries: 0,
                });
                queued += 1;
            }
        }
        if immediate == 0 && queued == 0 && self.banner.starts_with("就绪") {
            self.banner = "服务都在运行".to_string();
        } else if queued > 0 && immediate > 0 {
            self.banner = format!("已启动 {immediate} 个，另有 {queued} 个按延迟排队");
        } else if queued > 0 && immediate == 0 && !self.banner.contains('：') {
            self.banner = format!("{queued} 个服务已按延迟排队");
        }
        self.ctx.request_repaint();
    }

    fn stop_project(&mut self, project_id: &str) {
        self.pending.retain(|item| item.project_id != project_id);
        self.restart_all = None;
        let ids = self
            .project(project_id)
            .map(|project| {
                project
                    .services
                    .iter()
                    .map(|service| service.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for service_id in ids {
            self.stop_service(project_id, &service_id);
        }
        self.ctx.request_repaint();
    }

    fn restart_project(&mut self, project_id: &str) {
        self.stop_project(project_id);
        self.restart_all = Some(RestartAll {
            project_id: project_id.to_string(),
            not_before: Instant::now() + Duration::from_millis(450),
            deadline: Instant::now() + Duration::from_secs(4),
        });
        self.banner = "正在重启…".to_string();
        self.ctx.request_repaint();
    }

    fn handle_keys(&mut self, ui: &mut egui::Ui) {
        let typing = ui.ctx().memory(|mem| mem.focused().is_some());
        if typing {
            return;
        }
        let (pressed, shift) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::F5),
                input.modifiers.shift,
            )
        });
        let Some(project_id) = self.selected.clone() else {
            return;
        };
        if !pressed {
            return;
        }
        if shift {
            self.stop_project(&project_id);
        } else {
            self.run_project(&project_id);
        }
    }

    fn sync_title(&mut self) {
        let running = self.supervisor.any_running();
        let count: usize = self
            .config
            .projects
            .iter()
            .map(|project| self.running_count(&project.id))
            .sum();
        let title = if !running || count == 0 {
            "RunWeb".to_string()
        } else {
            format!("RunWeb · {count} 个服务运行中")
        };
        if title != self.title {
            self.title.clone_from(&title);
            self.ctx
                .send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            brand_mark(ui, &self.logo, 42.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(egui::RichText::new("RunWeb").size(20.0).strong().color(TEXT));
                ui.label(
                    egui::RichText::new("本地 Web 项目启动器")
                        .size(12.0)
                        .color(MUTED),
                );
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if accent_button(ui, "新建项目").clicked() {
                self.add_project(Template::FullStack);
            }
        });
        ui.horizontal(|ui| {
            if small_button(ui, "仅后端").clicked() {
                self.add_project(Template::BackendOnly);
            }
            if small_button(ui, "仅前端").clicked() {
                self.add_project(Template::FrontendOnly);
            }
        });
        ui.add_space(6.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let ids: Vec<String> = self
                    .config
                    .projects
                    .iter()
                    .map(|project| project.id.clone())
                    .collect();
                if ids.is_empty() {
                    ui.label(egui::RichText::new("还没有项目").color(MUTED));
                }
                for id in ids {
                    self.project_row(ui, &id);
                }
            });
    }

    fn project_row(&mut self, ui: &mut egui::Ui, id: &str) {
        let Some(project) = self.project(id) else {
            return;
        };
        let name = if project.name.trim().is_empty() {
            "未命名项目".to_string()
        } else {
            project.name.clone()
        };
        let total = project.services.len();
        let running = self.running_count(id);
        let selected = self.selected.as_deref() == Some(id);
        let subtitle = if total == 0 {
            "没有服务".to_string()
        } else if running == 0 {
            format!("{total} 个服务")
        } else {
            format!("{running}/{total} 运行中")
        };
        let dot = if running > 0 { "●" } else { "○" };
        let label = egui::RichText::new(format!("{dot}  {name}\n{subtitle}")).color(TEXT);
        let response = ui.add_sized(
            [ui.available_width(), 52.0],
            egui::Button::selectable(selected, label).corner_radius(egui::CornerRadius::same(8)),
        );
        if response.clicked() {
            if self.selected.as_deref() != Some(id) {
                self.confirm_delete = false;
                self.log_focus = None;
            }
            self.selected = Some(id.to_string());
        }
    }

    fn main_panel(&mut self, ui: &mut egui::Ui) {
        let Some(project_id) = self.selected.clone() else {
            self.empty_state(ui);
            return;
        };
        if self.project_index(&project_id).is_none() {
            self.selected = None;
            self.empty_state(ui);
            return;
        }
        let mut dirty = false;
        let mut action = Action::None;
        self.header(ui, &project_id, &mut dirty, &mut action);
        ui.add_space(8.0);
        if self.config_mode {
            self.editor(ui, &project_id, &mut dirty, &mut action);
        } else {
            self.cards(ui, &project_id, &mut action);
        }
        if dirty {
            self.persist();
        }
        self.apply(&project_id, action);
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.add_space(36.0);
        ui.vertical_centered(|ui| {
            brand_mark(ui, &self.logo, 84.0);
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new("把每个项目的后端和前端放在一起")
                    .size(22.0)
                    .strong()
                    .color(TEXT),
            );
        });
        ui.label(
            egui::RichText::new("配好工作目录和启动命令后，点一次「运行全部」就会一起拉起，日志在下面。")
                .color(MUTED),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if accent_button(ui, "新建前后端项目").clicked() {
                self.add_project(Template::FullStack);
            }
            if small_button(ui, "只建后端").clicked() {
                self.add_project(Template::BackendOnly);
            }
            if small_button(ui, "只建前端").clicked() {
                self.add_project(Template::FrontendOnly);
            }
        });
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new("命令通过系统终端执行，可以直接写 npm run dev、pnpm dev、cargo run。")
                .size(12.0)
                .color(MUTED),
        );
    }

    fn header(&mut self, ui: &mut egui::Ui, project_id: &str, dirty: &mut bool, action: &mut Action) {
        let (name, note) = {
            let project = self.project(project_id).expect("project exists");
            (project.name.clone(), project.note.clone())
        };
        let running = self.running_count(project_id);
        ui.horizontal(|ui| {
            if self.config_mode {
                let width = (ui.available_width() - 420.0).max(180.0);
                if let Some(index) = self.project_index(project_id) {
                    *dirty |= ui
                        .add(
                            egui::TextEdit::singleline(&mut self.config.projects[index].name)
                                .desired_width(width)
                                .hint_text("项目名称"),
                        )
                        .changed();
                }
            } else {
                let title = if name.trim().is_empty() {
                    "未命名项目"
                } else {
                    name.as_str()
                };
                ui.label(egui::RichText::new(title).size(22.0).strong().color(TEXT));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let config_label = if self.config_mode { "完成" } else { "配置" };
                if small_button(ui, config_label).clicked() {
                    self.config_mode = !self.config_mode;
                    self.confirm_delete = false;
                }
                if small_button(ui, "重启").clicked() {
                    *action = Action::RestartAll;
                }
                if ui
                    .add_enabled(running > 0, egui::Button::new("停止全部"))
                    .clicked()
                {
                    *action = Action::StopAll;
                }
                if accent_button(ui, "运行全部").on_hover_text("F5").clicked() {
                    *action = Action::RunAll;
                }
            });
        });
        if self.config_mode {
            if let Some(index) = self.project_index(project_id) {
                *dirty |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.projects[index].note)
                            .desired_width(f32::INFINITY)
                            .hint_text("备注，可选"),
                    )
                    .changed();
            }
        } else if !note.trim().is_empty() {
            ui.label(egui::RichText::new(note).color(MUTED));
        }
        ui.label(
            egui::RichText::new("F5 运行当前项目    Shift+F5 停止")
                .size(11.0)
                .color(MUTED),
        );
    }

    fn cards(&mut self, ui: &mut egui::Ui, project_id: &str, action: &mut Action) {
        let Some(index) = self.project_index(project_id) else {
            return;
        };
        let count = self.config.projects[index].services.len();
        if count == 0 {
            ui.label(egui::RichText::new("还没有服务，点「配置」添加。").color(MUTED));
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for service_index in 0..count {
                    self.service_card(ui, index, service_index, action);
                    ui.add_space(8.0);
                }
            });
    }

    fn service_card(&mut self, ui: &mut egui::Ui, pidx: usize, sidx: usize, action: &mut Action) {
        let project_id = self.config.projects[pidx].id.clone();
        let service = self.config.projects[pidx].services[sidx].clone();
        let key = service_key(&project_id, &service.id);
        let info = self.supervisor.info(&key);
        let exit = self.exits.get(&key).copied();
        let cwd = resolve_cwd(&self.config_path, &service.cwd);
        let cwd_missing = !service.cwd.trim().is_empty() && !cwd.is_dir();
        let (status, color) = match (&info, exit) {
            (Some(info), _) => (
                format!(
                    "运行中 {} · PID {}",
                    format_elapsed(info.started.elapsed()),
                    info.pid
                ),
                GREEN,
            ),
            (None, Some(code)) if code != 0 => (format!("已退出 {code}"), RED),
            (None, Some(_)) => ("已结束".to_string(), MUTED),
            (None, None) => ("未运行".to_string(), MUTED),
        };
        let name = if service.name.trim().is_empty() {
            "未命名服务".to_string()
        } else {
            service.name.clone()
        };
        egui::Frame::new()
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("●").color(color).size(14.0));
                    ui.label(egui::RichText::new(name).strong().size(16.0).color(TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(status).color(color));
                    });
                });
                let cwd_text = if service.cwd.trim().is_empty() {
                    "未设置工作目录".to_string()
                } else {
                    service.cwd.clone()
                };
                let cwd_color = if cwd_missing || service.cwd.trim().is_empty() {
                    AMBER
                } else {
                    MUTED
                };
                ui.label(egui::RichText::new(cwd_text).size(12.0).color(cwd_color));
                let command = command_preview(&service.command);
                ui.label(
                    egui::RichText::new(command)
                        .monospace()
                        .size(13.0)
                        .color(if command_is_blank(&service.command) {
                            AMBER
                        } else {
                            TEXT
                        }),
                );
                ui.label(
                    egui::RichText::new(service.shell.label())
                        .size(11.0)
                        .color(MUTED),
                );
                if service.delay_ms > 0 {
                    ui.label(
                        egui::RichText::new(format!("启动延迟 {} ms", service.delay_ms))
                            .size(11.0)
                            .color(MUTED),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if info.is_some() {
                        if small_button(ui, "停止").clicked() {
                            *action = Action::Stop(service.id.clone());
                        }
                        if small_button(ui, "重启").clicked() {
                            *action = Action::Restart(service.id.clone());
                        }
                    } else if accent_button(ui, "启动").clicked() {
                        *action = Action::Start(service.id.clone());
                    }
                    if small_button(ui, "日志").clicked() {
                        *action = Action::FocusLogs(service.id.clone());
                    }
                    if ui
                        .add_enabled(!service.cwd.trim().is_empty(), egui::Button::new("打开目录"))
                        .clicked()
                    {
                        *action = Action::OpenDir(service.id.clone());
                    }
                });
            });
    }

    fn editor(&mut self, ui: &mut egui::Ui, project_id: &str, dirty: &mut bool, action: &mut Action) {
        ui.label(
            egui::RichText::new(
                "每个服务单独启动。运行全部时按列表顺序，并等待各自的启动延迟。前端可以设 800～2000 ms，等后端先起来。",
            )
            .size(12.0)
            .color(MUTED),
        );
        let Some(index) = self.project_index(project_id) else {
            return;
        };
        let count = self.config.projects[index].services.len();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for service_index in 0..count {
                    self.service_editor(ui, index, service_index, dirty, action);
                    ui.add_space(8.0);
                }
                ui.horizontal_wrapped(|ui| {
                    if accent_button(ui, "添加服务").clicked() {
                        *action = Action::AddService;
                    }
                    if small_button(ui, "填入示例命令").clicked() {
                        *action = Action::FillExamples;
                    }
                    if small_button(ui, "复制项目").clicked() {
                        *action = Action::Duplicate;
                    }
                    if small_button(ui, "上移项目").clicked() {
                        *action = Action::MoveProject(-1);
                    }
                    if small_button(ui, "下移项目").clicked() {
                        *action = Action::MoveProject(1);
                    }
                    let delete_label = if self.confirm_delete {
                        "确认删除项目"
                    } else {
                        "删除项目"
                    };
                    if danger_button(ui, delete_label).clicked() {
                        if self.confirm_delete {
                            *action = Action::Delete;
                        } else {
                            self.confirm_delete = true;
                        }
                    }
                });
            });
    }

    fn service_editor(
        &mut self,
        ui: &mut egui::Ui,
        pidx: usize,
        sidx: usize,
        dirty: &mut bool,
        action: &mut Action,
    ) {
        let service_id = self.config.projects[pidx].services[sidx].id.clone();
        let can_up = sidx > 0;
        let can_down = sidx + 1 < self.config.projects[pidx].services.len();
        egui::Frame::new()
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("名称").color(MUTED));
                    let width = (ui.available_width() - 220.0).max(120.0);
                    *dirty |= ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut self.config.projects[pidx].services[sidx].name,
                            )
                            .desired_width(width)
                            .hint_text("例如 后端"),
                        )
                        .changed();
                    if ui.add_enabled(can_up, egui::Button::new("上移")).clicked() {
                        *action = Action::MoveService(service_id.clone(), -1);
                    }
                    if ui.add_enabled(can_down, egui::Button::new("下移")).clicked() {
                        *action = Action::MoveService(service_id.clone(), 1);
                    }
                });
                ui.label(egui::RichText::new("工作目录").color(MUTED).size(12.0));
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 88.0).max(120.0);
                    *dirty |= ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut self.config.projects[pidx].services[sidx].cwd,
                            )
                            .desired_width(width)
                            .hint_text(r"例如 D:\code\demo\server"),
                        )
                        .changed();
                    if small_button(ui, "浏览…").clicked() {
                        *action = Action::Browse(service_id.clone());
                    }
                });
                ui.label(egui::RichText::new("启动命令").color(MUTED).size(12.0));
                *dirty |= ui
                    .add(
                        egui::TextEdit::multiline(
                            &mut self.config.projects[pidx].services[sidx].command,
                        )
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .hint_text("例如 npm run dev，多行会按顺序执行"),
                    )
                    .changed();
                ui.label(
                    egui::RichText::new("运行环境")
                        .color(MUTED)
                        .size(12.0),
                );
                ui.horizontal(|ui| {
                    let current = self.config.projects[pidx].services[sidx].shell;
                    let mut selected = current;
                    egui::ComboBox::from_id_salt(format!("shell-{service_id}"))
                        .selected_text(selected.label())
                        .width(180.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected, ShellKind::Cmd, ShellKind::Cmd.label());
                            ui.selectable_value(
                                &mut selected,
                                ShellKind::Bash,
                                ShellKind::Bash.label(),
                            );
                        });
                    if selected != current {
                        self.config.projects[pidx].services[sidx].shell = selected;
                        *dirty = true;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "命令提示符适合直接跑 npm、go、cargo。Git Bash 会加载 ~/.bashrc，和单独打开的 Git Bash 一样。",
                    )
                    .size(11.0)
                    .color(MUTED),
                );
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("启动延迟").color(MUTED));
                    *dirty |= ui
                        .add(
                            egui::DragValue::new(
                                &mut self.config.projects[pidx].services[sidx].delay_ms,
                            )
                            .range(0..=600_000_u64)
                            .suffix(" ms"),
                        )
                        .changed();
                    ui.label(
                        egui::RichText::new("运行全部时，相对上一个服务再等这么久")
                            .size(11.0)
                            .color(MUTED),
                    );
                });
                ui.label(
                    egui::RichText::new("环境变量")
                        .color(MUTED)
                        .size(12.0),
                );
                *dirty |= ui
                    .add(
                        egui::TextEdit::multiline(&mut self.config.projects[pidx].services[sidx].env)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .hint_text("一行一个，例如 PORT=3000"),
                    )
                    .changed();
                if danger_button(ui, "删除此服务").clicked() {
                    *action = Action::RemoveService(service_id);
                }
            });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        let Some(project_id) = self.selected.clone() else {
            ui.label(egui::RichText::new("选择一个项目后在这里看日志").color(MUTED));
            return;
        };
        let Some(project) = self.project(&project_id).cloned() else {
            return;
        };
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.log_focus.is_none(), "全部")
                .clicked()
            {
                self.log_focus = None;
            }
            for service in &project.services {
                let name = if service.name.trim().is_empty() {
                    "未命名"
                } else {
                    service.name.as_str()
                };
                let selected = self.log_focus.as_deref() == Some(service.id.as_str());
                if ui.selectable_label(selected, name).clicked() {
                    self.log_focus = Some(service.id.clone());
                }
            }
            ui.separator();
            let width = 180.0;
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(width)
                    .hint_text("过滤"),
            );
            ui.checkbox(&mut self.auto_scroll, "自动滚动");
            if small_button(ui, "复制").clicked() {
                self.apply(&project_id, Action::CopyLogs);
            }
            if small_button(ui, "保存").clicked() {
                self.apply(&project_id, Action::SaveLogs);
            }
            if small_button(ui, "清空").clicked() {
                self.apply(&project_id, Action::ClearLogs);
            }
        });

        let lines = self.visible_logs(&project);
        let hidden = lines.len().saturating_sub(SHOW_LOGS);
        let start = lines.len().saturating_sub(SHOW_LOGS);
        egui::ScrollArea::vertical()
            .stick_to_bottom(self.auto_scroll)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(
                        egui::RichText::new("还没有日志。运行后，后端和前端的输出会显示在这里。")
                            .color(MUTED),
                    );
                    return;
                }
                if hidden > 0 {
                    ui.label(
                        egui::RichText::new(format!("更早的 {hidden} 行已省略，复制或保存可以拿到当前过滤结果"))
                            .size(11.0)
                            .color(MUTED),
                    );
                }
                for line in &lines[start..] {
                    let color = line_color(line);
                    let prefix = if self.log_focus.is_none() && project.services.len() > 1 {
                        format!("{}  {}  ", line.at, line.service_name)
                    } else {
                        format!("{}  ", line.at)
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{prefix}{}", line.text))
                                .monospace()
                                .size(13.0)
                                .color(color),
                        )
                        .selectable(true),
                    );
                }
            });
    }

    fn visible_logs(&self, project: &Project) -> Vec<LogLine> {
        let filter = self.filter.trim().to_lowercase();
        let mut lines = Vec::new();
        for service in &project.services {
            if self
                .log_focus
                .as_ref()
                .is_some_and(|id| id != &service.id)
            {
                continue;
            }
            let key = service_key(&project.id, &service.id);
            if let Some(buf) = self.logs.get(&key) {
                lines.extend(buf.iter().cloned());
            }
        }
        lines.sort_by_key(|line| line.seq);
        if !filter.is_empty() {
            lines.retain(|line| {
                line.text.to_lowercase().contains(&filter)
                    || line.service_name.to_lowercase().contains(&filter)
            });
        }
        lines
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let running: usize = self
                .config
                .projects
                .iter()
                .map(|project| self.running_count(&project.id))
                .sum();
            ui.label(
                egui::RichText::new(format!(
                    "{} 个项目 · {running} 个服务运行中",
                    self.config.projects.len()
                ))
                .color(TEXT),
            );
            ui.separator();
            ui.label(egui::RichText::new(&self.banner).color(MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if small_button(ui, "配置目录").clicked() {
                    if let Some(dir) = self.config_path.parent() {
                        open_in_file_manager(dir);
                    }
                }
                ui.label(
                    egui::RichText::new(self.config_path.display().to_string())
                        .size(11.0)
                        .color(MUTED),
                )
                .on_hover_text(self.config_path.display().to_string());
            });
        });
    }

    fn apply(&mut self, project_id: &str, action: Action) {
        match action {
            Action::None => {}
            Action::RunAll => self.run_project(project_id),
            Action::StopAll => self.stop_project(project_id),
            Action::RestartAll => self.restart_project(project_id),
            Action::Start(id) => {
                self.start_service(project_id, &id);
                self.ctx.request_repaint();
            }
            Action::Stop(id) => self.stop_service(project_id, &id),
            Action::Restart(id) => {
                self.stop_service(project_id, &id);
                self.pending.push(PendingStart {
                    at: Instant::now() + Duration::from_millis(450),
                    project_id: project_id.to_string(),
                    service_id: id,
                    tries: 0,
                });
                self.banner = "正在重启…".to_string();
                self.ctx.request_repaint();
            }
            Action::Browse(id) => self.browse_cwd(project_id, &id),
            Action::OpenDir(id) => {
                if let Some((p, s)) = self.service_index(project_id, &id) {
                    let cwd = self.config.projects[p].services[s].cwd.clone();
                    let path = resolve_cwd(&self.config_path, &cwd);
                    if path.is_dir() {
                        open_in_file_manager(&path);
                    } else {
                        self.banner = format!("目录不存在：{}", path.display());
                    }
                }
            }
            Action::FocusLogs(id) => self.log_focus = Some(id),
            Action::AddService => self.add_service(project_id),
            Action::RemoveService(id) => self.remove_service(project_id, &id),
            Action::MoveService(id, delta) => self.move_service(project_id, &id, delta),
            Action::Duplicate => self.duplicate_project(project_id),
            Action::Delete => self.delete_project(project_id),
            Action::MoveProject(delta) => self.move_project(project_id, delta),
            Action::FillExamples => self.fill_examples(project_id),
            Action::SaveLogs => self.save_logs(project_id),
            Action::CopyLogs => self.copy_logs(project_id),
            Action::ClearLogs => self.clear_logs(project_id),
        }
    }

    fn browse_cwd(&mut self, project_id: &str, service_id: &str) {
        let current = self
            .service_index(project_id, service_id)
            .map(|(p, s)| self.config.projects[p].services[s].cwd.clone())
            .unwrap_or_default();
        let mut dialog = rfd::FileDialog::new().set_title("选择工作目录");
        let existing = resolve_cwd(&self.config_path, &current);
        if existing.is_dir() {
            dialog = dialog.set_directory(existing);
        }
        let Some(folder) = dialog.pick_folder() else {
            return;
        };
        if let Some((p, s)) = self.service_index(project_id, service_id) {
            self.config.projects[p].services[s].cwd = folder.display().to_string();
            self.persist();
            self.banner = "已更新工作目录".to_string();
        }
    }

    fn add_project(&mut self, template: Template) {
        let project = Project::from_template(template);
        let id = project.id.clone();
        self.config.projects.push(project);
        self.selected = Some(id);
        self.config_mode = true;
        self.confirm_delete = false;
        self.log_focus = None;
        self.banner = "已新建项目，填写目录和命令后即可运行".to_string();
        self.persist();
    }

    fn add_service(&mut self, project_id: &str) {
        let Some(index) = self.project_index(project_id) else {
            return;
        };
        let n = self.config.projects[index].services.len() + 1;
        self.config.projects[index]
            .services
            .push(Service::named(&format!("服务 {n}")));
        self.persist();
    }

    fn remove_service(&mut self, project_id: &str, service_id: &str) {
        self.stop_service(project_id, service_id);
        let key = service_key(project_id, service_id);
        self.logs.remove(&key);
        self.exits.remove(&key);
        if self.log_focus.as_deref() == Some(service_id) {
            self.log_focus = None;
        }
        if let Some(index) = self.project_index(project_id) {
            self.config.projects[index]
                .services
                .retain(|service| service.id != service_id);
            self.persist();
        }
    }

    fn move_service(&mut self, project_id: &str, service_id: &str, delta: i32) {
        let Some(index) = self.project_index(project_id) else {
            return;
        };
        let services = &mut self.config.projects[index].services;
        let Some(pos) = services.iter().position(|service| service.id == service_id) else {
            return;
        };
        let next = pos as i32 + delta;
        if next < 0 || next >= services.len() as i32 {
            return;
        }
        services.swap(pos, next as usize);
        self.persist();
    }

    fn duplicate_project(&mut self, project_id: &str) {
        let Some(mut project) = self.project(project_id).cloned() else {
            return;
        };
        project.id = crate::model::new_id();
        project.name = format!("{} 副本", project.name.trim());
        if project.name.trim().is_empty() {
            project.name = "项目副本".to_string();
        }
        for service in &mut project.services {
            service.id = crate::model::new_id();
        }
        let id = project.id.clone();
        self.config.projects.push(project);
        self.selected = Some(id);
        self.persist();
        self.banner = "已复制项目".to_string();
    }

    fn delete_project(&mut self, project_id: &str) {
        self.stop_project(project_id);
        if let Some(project) = self.project(project_id).cloned() {
            for service in &project.services {
                let key = service_key(project_id, &service.id);
                self.logs.remove(&key);
                self.exits.remove(&key);
            }
        }
        self.config.projects.retain(|project| project.id != project_id);
        self.selected = self.config.projects.first().map(|project| project.id.clone());
        self.confirm_delete = false;
        self.config_mode = false;
        self.persist();
        self.banner = "已删除项目".to_string();
    }

    fn move_project(&mut self, project_id: &str, delta: i32) {
        let Some(pos) = self.project_index(project_id) else {
            return;
        };
        let next = pos as i32 + delta;
        if next < 0 || next >= self.config.projects.len() as i32 {
            return;
        }
        self.config.projects.swap(pos, next as usize);
        self.persist();
    }

    fn fill_examples(&mut self, project_id: &str) {
        let Some(index) = self.project_index(project_id) else {
            return;
        };
        fill_example_commands(&mut self.config.projects[index]);
        self.persist();
        self.banner = "已填入示例命令，记得改成你自己的工作目录".to_string();
    }

    fn log_text(&self, project_id: &str) -> String {
        let Some(project) = self.project(project_id).cloned() else {
            return String::new();
        };
        self.visible_logs(&project)
            .iter()
            .map(|line| format!("{}  {}  {}", line.at, line.service_name, line.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn copy_logs(&mut self, project_id: &str) {
        let text = self.log_text(project_id);
        self.ctx.copy_text(text);
        self.banner = "日志已复制".to_string();
    }

    fn save_logs(&mut self, project_id: &str) {
        let name = self
            .project(project_id)
            .map(|project| {
                let name = if project.name.trim().is_empty() {
                    "runweb"
                } else {
                    project.name.trim()
                };
                format!("{name}-log.txt")
            })
            .unwrap_or_else(|| "runweb-log.txt".to_string());
        let text = self.log_text(project_id);
        let Some(path) = rfd::FileDialog::new()
            .set_title("保存日志")
            .set_file_name(name)
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, text) {
            Ok(()) => self.banner = format!("日志已保存到 {}", path.display()),
            Err(err) => self.banner = format!("保存日志失败：{err}"),
        }
    }

    fn clear_logs(&mut self, project_id: &str) {
        let ids = self
            .project(project_id)
            .map(|project| {
                project
                    .services
                    .iter()
                    .map(|service| service.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for service_id in ids {
            if self
                .log_focus
                .as_ref()
                .is_some_and(|focus| focus != &service_id)
            {
                continue;
            }
            self.logs.remove(&service_key(project_id, &service_id));
        }
        self.banner = "日志已清空".to_string();
    }
}

impl eframe::App for RunWebApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let busy = self.supervisor.any_running()
            || !self.pending.is_empty()
            || self.restart_all.is_some();
        self.pump();
        if busy || self.supervisor.any_running() || !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_keys(ui);
        self.sync_title();
        let sidebar = egui::Frame::new()
            .fill(SIDEBAR)
            .inner_margin(egui::Margin::same(12));
        let logs = egui::Frame::new()
            .fill(LOG_BG)
            .inner_margin(egui::Margin::symmetric(12, 8));
        let main = egui::Frame::new()
            .fill(BG)
            .inner_margin(egui::Margin::same(16));
        let status = egui::Frame::new()
            .fill(SIDEBAR)
            .inner_margin(egui::Margin::symmetric(12, 6));
        egui::Panel::bottom("status")
            .resizable(false)
            .exact_size(36.0)
            .show_separator_line(false)
            .frame(status)
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("logs")
            .resizable(true)
            .default_size(320.0)
            .min_size(150.0)
            .frame(logs)
            .show(ui, |ui| self.log_panel(ui));
        egui::Panel::left("projects")
            .resizable(true)
            .default_size(268.0)
            .min_size(220.0)
            .max_size(380.0)
            .frame(sidebar)
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default()
            .frame(main)
            .show(ui, |ui| self.main_panel(ui));
    }

    fn on_exit(&mut self) {
        self.supervisor.stop_all();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }
}

fn validate_service(
    config_file: &std::path::Path,
    service: &Service,
) -> Result<(std::path::PathBuf, Vec<(String, String)>), String> {
    if command_is_blank(&service.command) {
        return Err("请填写启动命令".to_string());
    }
    if service.cwd.trim().is_empty() {
        return Err("请填写工作目录".to_string());
    }
    let cwd = resolve_cwd(config_file, &service.cwd);
    if !cwd.is_dir() {
        return Err(format!("工作目录不存在：{}", cwd.display()));
    }
    let env = parse_env(&service.env)?;
    Ok((cwd, env))
}

fn command_preview(command: &str) -> String {
    let line = command.lines().find(|line| !line.trim().is_empty());
    let Some(line) = line else {
        return "未设置启动命令".to_string();
    };
    let extra = command.lines().filter(|line| !line.trim().is_empty()).count() > 1;
    let mut text: String = line.trim().chars().take(120).collect();
    if line.trim().chars().count() > 120 || extra {
        text.push('…');
    }
    text
}

fn line_color(line: &LogLine) -> egui::Color32 {
    match line.kind {
        LogKind::System => SYS,
        LogKind::Error => RED,
        LogKind::Output => {
            let lower = line.text.to_ascii_lowercase();
            if lower.contains("error")
                || lower.contains("panic")
                || lower.contains("err!")
                || line.text.contains("错误")
                || line.text.contains("失败")
            {
                egui::Color32::from_rgb(255, 150, 140)
            } else if lower.contains("warn") || line.text.contains("警告") {
                AMBER
            } else {
                LOG_TEXT
            }
        }
    }
}

fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 3600 {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}

fn brand_mark(ui: &mut egui::Ui, logo: &egui::TextureHandle, size: f32) {
    let radius = (size * 0.22).round().clamp(6.0, 22.0) as u8;
    ui.add(
        egui::Image::from_texture(logo)
            .fit_to_exact_size(egui::vec2(size, size))
            .corner_radius(egui::CornerRadius::same(radius)),
    );
}

fn accent_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE))
            .fill(GREEN)
            .corner_radius(egui::CornerRadius::same(6)),
    )
}

fn small_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(text)
            .fill(egui::Color32::from_rgb(48, 54, 68))
            .corner_radius(egui::CornerRadius::same(6)),
    )
}

fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE))
            .fill(RED)
            .corner_radius(egui::CornerRadius::same(6)),
    )
}

fn open_in_file_manager(path: &std::path::Path) {
    let result = {
        #[cfg(windows)]
        {
            std::process::Command::new("explorer").arg(path).spawn()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(path).spawn()
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            std::process::Command::new("xdg-open").arg(path).spawn()
        }
    };
    let _ = result;
}

fn cleanup_old_launchers() {
    let dir = std::env::temp_dir().join("RunWeb");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified
            .elapsed()
            .map(|age| age.as_secs() > 60 * 60)
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn install_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals.dark_mode = true;
        style.visuals.override_text_color = Some(TEXT);
        style.visuals.window_fill = BG;
        style.visuals.panel_fill = SIDEBAR;
        style.visuals.extreme_bg_color = LOG_BG;
        style.visuals.faint_bg_color = CARD;
        style.visuals.code_bg_color = CARD;
        style.visuals.warn_fg_color = AMBER;
        style.visuals.error_fg_color = RED;
        style.visuals.hyperlink_color = ACCENT;
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.selection.stroke.color = egui::Color32::WHITE;
        style.visuals.widgets.noninteractive.fg_stroke.color = TEXT;
        style.visuals.widgets.inactive.fg_stroke.color = TEXT;
        style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
        style.visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
        style.visuals.widgets.inactive.weak_bg_fill = CARD;
        style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(48, 56, 72);
        style.visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
        style.visuals.window_corner_radius = egui::CornerRadius::same(8);
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.menu_margin = egui::Margin::same(8);
    });
}
