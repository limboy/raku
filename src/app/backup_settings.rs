//! Backup settings. Frames read cached metadata; all snapshot work and scans
//! execute off the UI thread through the daemon.

use super::*;
use crate::ui::ActivationExt;
use raku_client::{Command, ResponsePayload};
use raku_protocol::backup::{AUTOMATIC_BACKUP_LIMIT, BackupInfo, next_automatic_backup_at};

const AUTOMATIC_BACKUP_RETRY: u64 = 5 * 60;

pub(super) struct BackupSettings {
    latest: Option<BackupInfo>,
    focus: [FocusHandle; 4],
    automatic_task: Option<gpui::Task<()>>,
    automatic_generation: u64,
    generation: u64,
    loading: bool,
    dialog_open: bool,
    busy: Option<&'static str>,
    staged: bool,
    message: Option<String>,
    error: Option<String>,
}

impl BackupSettings {
    pub fn is_restoring(&self) -> bool {
        self.staged
    }

    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn new(cx: &mut App) -> Self {
        Self {
            latest: None,
            focus: std::array::from_fn(|_| cx.focus_handle()),
            automatic_task: None,
            automatic_generation: 0,
            generation: 0,
            loading: false,
            dialog_open: false,
            busy: None,
            staged: false,
            message: None,
            error: None,
        }
    }
}

impl Raku {
    pub(super) fn schedule_automatic_backups(&mut self, cx: &mut Context<Self>) {
        self.schedule_automatic_backup_check(0, cx);
    }

    fn schedule_automatic_backup_check(&mut self, mut check_at: u64, cx: &mut Context<Self>) {
        self.backup_settings.automatic_task = None;
        self.backup_settings.automatic_generation =
            self.backup_settings.automatic_generation.wrapping_add(1);
        let generation = self.backup_settings.automatic_generation;
        if self.backup_settings.staged {
            return;
        }
        let Some(directory) = self.state.backup_directory.clone() else {
            return;
        };
        let daemon = self.daemon.client();
        self.backup_settings.automatic_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let now = unix_time();
                if now < check_at {
                    // Check wall time after sleep or a clock adjustment. Idle
                    // ticks do no I/O or serialization and cause no redraw.
                    cx.background_executor()
                        .timer(Duration::from_secs(
                            (check_at - now).min(AUTOMATIC_BACKUP_RETRY),
                        ))
                        .await;
                    continue;
                }
                let client = daemon.clone();
                let path = directory.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        match client.request(
                            Uuid::nil(),
                            Uuid::nil(),
                            Command::ListBackups { directory: path },
                        )? {
                            ResponsePayload::Backups { backups } => Ok(backups),
                            _ => anyhow::bail!("invalid automatic backup list response"),
                        }
                    })
                    .await;
                let next = this
                    .update(cx, |this, cx| {
                        if this.backup_settings.automatic_generation != generation
                            || this.state.backup_directory.as_ref() != Some(&directory)
                            || this.backup_settings.staged
                        {
                            return None;
                        }
                        let now = unix_time();
                        match result {
                            Ok(backups) => {
                                let next = next_automatic_backup_at(&backups);
                                if now < next && backups.len() <= AUTOMATIC_BACKUP_LIMIT {
                                    return Some(next);
                                }
                                if this.backup_settings.busy.is_some()
                                    || this.backup_settings.dialog_open
                                {
                                    return Some(now.saturating_add(60));
                                }
                                this.start_automatic_backup(directory.clone(), cx);
                                if this.backup_settings.busy.is_some() {
                                    None // The completion schedules the next check.
                                } else {
                                    Some(now.saturating_add(AUTOMATIC_BACKUP_RETRY))
                                }
                            }
                            Err(error) => {
                                let message = tr!("backup.error", error = error.to_string());
                                if this.backup_settings.error.as_ref() != Some(&message) {
                                    this.backup_settings.error = Some(message);
                                    cx.notify();
                                }
                                Some(now.saturating_add(AUTOMATIC_BACKUP_RETRY))
                            }
                        }
                    })
                    .ok()
                    .flatten();
                match next {
                    Some(next) => check_at = next,
                    None => return,
                }
            }
        }));
    }

    pub(super) fn refresh_backups(&mut self, cx: &mut Context<Self>) {
        self.backup_settings.generation = self.backup_settings.generation.wrapping_add(1);
        let generation = self.backup_settings.generation;
        let Some(directory) = self.state.backup_directory.clone() else {
            return;
        };
        self.backup_settings.loading = true;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::ListBackups { directory },
                    )? {
                        ResponsePayload::Backups { backups } => Ok(backups.into_iter().next()),
                        _ => anyhow::bail!("invalid backup list response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.backup_settings.generation != generation {
                    return;
                }
                this.backup_settings.loading = false;
                match result {
                    Ok(latest) => this.backup_settings.latest = latest,
                    Err(error) => {
                        this.backup_settings.error =
                            Some(tr!("backup.error", error = error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_backup_directory(&mut self, cx: &mut Context<Self>) {
        if self.backup_settings.busy.is_some() || self.backup_settings.staged {
            return;
        }
        self.backup_settings.dialog_open = true;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr!("backup.choose_directory").into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |this, cx| {
                this.backup_settings.dialog_open = false;
                if let Ok(Ok(Some(paths))) = result
                    && let Some(path) = paths.into_iter().next()
                {
                    this.state.backup_directory = Some(path);
                    this.backup_settings.latest = None;
                    this.backup_settings.message = None;
                    this.backup_settings.error = None;
                    this.save();
                    this.refresh_backups(cx);
                    this.schedule_automatic_backups(cx);
                }
            });
        })
        .detach();
    }

    /// Clearing the directory is the off state: with no destination, the
    /// automatic scheduler never starts and nothing is listed.
    fn turn_off_backups(&mut self, cx: &mut Context<Self>) {
        if self.backup_settings.busy.is_some()
            || self.backup_settings.staged
            || self.state.backup_directory.is_none()
        {
            return;
        }
        self.state.backup_directory = None;
        self.backup_settings.latest = None;
        self.backup_settings.loading = false;
        self.backup_settings.message = None;
        self.backup_settings.error = None;
        // Invalidate in-flight list results and stop the scheduler.
        self.backup_settings.generation = self.backup_settings.generation.wrapping_add(1);
        self.schedule_automatic_backups(cx);
        self.save();
        cx.notify();
    }

    fn reveal_backup_directory(&self, cx: &App) {
        if let Some(directory) = self.state.backup_directory.as_ref() {
            crate::platform::reveal_in_file_manager(directory, cx);
        }
    }

    fn confirm_latest_backup_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.backup_settings.busy.is_some()
            || self.backup_settings.loading
            || self.backup_settings.staged
        {
            return;
        }
        let Some(backup) = self.backup_settings.latest.clone() else {
            return;
        };
        let detail = tr!(
            "backup.confirm_detail",
            path = backup.path.display().to_string()
        );
        self.backup_settings.dialog_open = true;
        let receiver = window.prompt(
            gpui::PromptLevel::Warning,
            &tr!("backup.confirm_title"),
            Some(&detail),
            &[
                tr!("common.cancel").as_str(),
                tr!("backup.restore_and_relaunch").as_str(),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |this, cx| {
                this.backup_settings.dialog_open = false;
                if result.ok() == Some(1) {
                    this.stage_backup_restore(backup.path, cx);
                }
            });
        })
        .detach();
    }

    fn start_automatic_backup(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        if self.backup_settings.busy.is_some() || self.backup_settings.staged {
            return;
        }
        self.capture_current_composer_draft(cx);
        let preparation = self.state.desktop_backup().and_then(|desktop| {
            self.store
                .backup_task_save(&self.state)
                .map(|task_save| (desktop, task_save))
        });
        let (desktop, task_save) = match preparation {
            Ok(preparation) => preparation,
            Err(error) => {
                self.backup_settings.error = Some(tr!("backup.error", error = error.to_string()));
                cx.notify();
                return;
            }
        };
        let drafts = self.composer_drafts.clone();
        let draft_store = self.composer_draft_store.clone();
        self.composer_draft_save_generation = self.composer_draft_save_generation.saturating_add(1);
        let draft_generation = self.composer_draft_save_generation;
        let daemon = self.daemon.client();
        self.backup_settings.busy = Some("backup.creating");
        self.backup_settings.error = None;
        self.backup_settings.message = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // Await the current edits before taking the SQLite snapshot.
                    daemon.request(Uuid::nil(), Uuid::nil(), task_save)?;
                    draft_store.save(drafts, draft_generation)?;
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::CreateAutomaticBackup { directory, desktop },
                    )? {
                        ResponsePayload::AutomaticBackup {
                            backup,
                            next_backup_at,
                        } => Ok((backup, next_backup_at)),
                        _ => anyhow::bail!("invalid automatic backup response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.backup_settings.busy = None;
                match result {
                    Ok((backup, next)) => {
                        if let Some(backup) = backup {
                            this.backup_settings.message = Some(tr!(
                                "backup.created",
                                path = backup.path.display().to_string()
                            ));
                            this.backup_settings.latest = Some(backup);
                        }
                        if this.settings_page == Some(SettingsPage::Backups) {
                            this.refresh_backups(cx);
                        }
                        this.schedule_automatic_backup_check(next, cx);
                    }
                    Err(error) => {
                        this.backup_settings.error =
                            Some(tr!("backup.error", error = error.to_string()));
                        this.schedule_automatic_backup_check(
                            unix_time().saturating_add(AUTOMATIC_BACKUP_RETRY),
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stage_backup_restore(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.backup_settings.busy.is_some() || self.backup_settings.staged {
            return;
        }
        let supervisor = self.daemon.clone();
        let daemon = supervisor.client();
        self.backup_settings.busy = Some("backup.preparing_restore");
        self.backup_settings.error = None;
        self.backup_settings.message = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::StageRestore { path },
                    )? {
                        ResponsePayload::RestoreStaged { backup } => {
                            // Close every old SQLite writer before the new
                            // process applies the staged snapshot.
                            supervisor.shutdown_for_relaunch()?;
                            Ok(backup)
                        }
                        _ => anyhow::bail!("invalid backup restore response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.backup_settings.busy = None;
                match result {
                    Ok(backup) => {
                        this.backup_settings.latest = Some(backup);
                        this.backup_settings.staged = true;
                        this.backup_settings.message = Some(tr!("backup.relaunching"));
                        this.schedule_automatic_backups(cx);
                        cx.restart();
                    }
                    Err(error) => {
                        this.backup_settings.error =
                            Some(tr!("backup.error", error = error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_backup_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let ui = &self.backup_settings;
        let unavailable = ui.busy.is_some() || ui.staged;
        let directory = self
            .state
            .backup_directory
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| tr!("backup.no_directory"));
        let choose = backup_button(
            "backup-directory",
            tr!("backup.choose_directory"),
            &ui.focus[0],
            unavailable,
            theme,
        )
        .on_activation(cx, |this, _, cx| this.choose_backup_directory(cx));
        let reveal = backup_button(
            "backup-reveal",
            tr!("common.reveal_in_finder"),
            &ui.focus[1],
            self.state.backup_directory.is_none(),
            theme,
        )
        .on_activation(cx, |this, _, cx| this.reveal_backup_directory(cx));
        let turn_off = backup_button(
            "backup-turn-off",
            tr!("backup.turn_off"),
            &ui.focus[3],
            unavailable || self.state.backup_directory.is_none(),
            theme,
        )
        .on_activation(cx, |this, _, cx| this.turn_off_backups(cx));
        let restore = backup_button(
            "backup-restore",
            tr!("backup.restore"),
            &ui.focus[2],
            unavailable || ui.loading || ui.latest.is_none(),
            theme,
        )
        .on_activation(cx, |this, window, cx| {
            this.confirm_latest_backup_restore(window, cx)
        });
        let mut latest = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(sp(12.5))
            .line_height(sp(18.0))
            .text_color(theme.text_secondary);
        if ui.loading {
            latest = latest.child(tr!("backup.loading"));
        } else if let Some(backup) = &ui.latest {
            let date = DateTime::from_timestamp(backup.created_at as i64, 0)
                .map(|date| {
                    date.with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
            latest = latest
                .child(
                    div()
                        .text_size(sp(13.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr!("backup.latest")),
                )
                .child(date)
                .child(tr!(
                    "backup.snapshot_detail",
                    projects = backup.project_count,
                    sessions = backup.session_count,
                    size = backup_size(backup.size_bytes)
                ));
        } else {
            latest = latest.child(tr!("backup.empty"));
        }
        div()
            .pt(px(15.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_size(sp(12.5))
                    .line_height(sp(19.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("backup.description")),
            )
            .child(
                div()
                    .p(px(16.0))
                    .rounded(px(10.0))
                    .bg(theme.raised)
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(sp(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!("backup.directory")),
                    )
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .line_height(sp(18.0))
                            .text_color(theme.text_secondary)
                            .child(directory),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .flex_wrap()
                            .child(choose)
                            .child(reveal)
                            .child(restore)
                            .child(turn_off),
                    )
                    .child(div().h(px(1.0)).bg(theme.border))
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .line_height(sp(18.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("backup.automatic_description")),
                    ),
            )
            .child(latest)
            .when_some(ui.busy, |page, key| {
                page.child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_secondary)
                        .child(tr!(key)),
                )
            })
            .when_some(ui.message.as_ref(), |page, message| {
                page.child(
                    div()
                        .text_size(sp(12.5))
                        .line_height(sp(19.0))
                        .text_color(theme.text_secondary)
                        .child(message.clone()),
                )
            })
            .when_some(ui.error.as_ref(), |page, error| {
                page.child(
                    div()
                        .text_size(sp(12.5))
                        .line_height(sp(19.0))
                        .text_color(theme.danger)
                        .child(error.clone()),
                )
            })
            .into_any_element()
    }
}

fn backup_button(
    id: &'static str,
    label: String,
    focus: &FocusHandle,
    disabled: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(focus)
        .tab_index(0)
        .min_h(px(32.0))
        .px(px(12.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.border_strong)
        .flex()
        .items_center()
        .cursor_default()
        .text_size(sp(12.5))
        .text_color(theme.text_secondary)
        .focus_visible(|style| style.border_color(theme.accent))
        .when(disabled, |style| style.opacity(0.45))
        .when(!disabled, |style| {
            style
                .hover(|style| style.bg(theme.overlay))
                .active(|style| style.bg(theme.overlay_strong))
        })
        .child(label)
}

fn backup_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}
