use super::*;
use crate::session_export::{self, ExportScope};

impl Raku {
    pub(super) fn export_conversation_action(
        &mut self,
        _: &crate::ExportConversation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = self.state.selected_session {
            self.export_session(session_id, ExportScope::Conversation, cx);
        }
    }

    pub(super) fn export_all_action(
        &mut self,
        _: &crate::ExportAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = self.state.selected_session {
            self.export_session(session_id, ExportScope::All, cx);
        }
    }

    pub(super) fn copy_conversation_action(
        &mut self,
        _: &crate::CopyConversation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = self.state.selected_session {
            self.copy_session(session_id, ExportScope::Conversation, cx);
        }
    }

    pub(super) fn copy_all_action(
        &mut self,
        _: &crate::CopyAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = self.state.selected_session {
            self.copy_session(session_id, ExportScope::All, cx);
        }
    }

    pub(super) fn copy_session(
        &mut self,
        session_id: Uuid,
        scope: ExportScope,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let snapshot = (session.clone(), self.daemon.clone());
        cx.spawn(async move |raku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (session, daemon) = snapshot;
                    let session = if session.detail_loaded {
                        session
                    } else {
                        raku_client::persistence::hydrate_session(&daemon, session_id)?
                            .ok_or_else(|| anyhow::anyhow!(tr!("errors.export_session_missing")))?
                    };
                    session_export::markdown(&session, scope)
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                match result {
                    Ok(markdown) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
                        raku.show_success_toast(tr!("session.copied"));
                    }
                    Err(error) => raku.show_toast(tr!("errors.copy_session", error = error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn export_session(
        &mut self,
        session_id: Uuid,
        scope: ExportScope,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let filename = session_export::suggested_filename(session);
        let directory = dirs::document_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        let path = cx.prompt_for_new_path(&directory, Some(&filename));
        cx.spawn(async move |raku, cx| {
            let path = match path.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = raku.update(cx, |raku, cx| {
                        raku.show_toast(tr!("errors.export_session", error = error));
                        cx.notify();
                    });
                    return;
                }
            };
            // Capture the target by id after the save panel closes. Switching
            // sessions while a panel is open must never export the new selection.
            let Ok(snapshot) = raku.update(cx, |raku, _| {
                let session = raku
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)?
                    .clone();
                Some((session, raku.daemon.clone()))
            }) else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (session, daemon) = snapshot
                        .ok_or_else(|| anyhow::anyhow!(tr!("errors.export_session_missing")))?;
                    let session = if session.detail_loaded {
                        session
                    } else {
                        raku_client::persistence::hydrate_session(&daemon, session_id)?
                            .ok_or_else(|| anyhow::anyhow!(tr!("errors.export_session_missing")))?
                    };
                    let markdown = session_export::markdown(&session, scope)?;
                    write_export(&path, &markdown)?;
                    anyhow::Ok(path)
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                match result {
                    Ok(path) => {
                        raku.show_success_toast(tr!("session.exported", path = path.display()))
                    }
                    Err(error) => raku.show_toast(tr!("errors.export_session", error = error)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn write_export(path: &Path, markdown: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let temporary = path.with_file_name(format!(".raku-export-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(markdown.as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_replaces_files_and_removes_temporary_files_when_saving_fails() {
        let directory = std::env::temp_dir().join(format!("raku-export-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("session.md");
        std::fs::write(&path, "old export").unwrap();
        write_export(&path, "# 新会话\n\nNew export\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# 新会话\n\nNew export\n"
        );
        let blocked = directory.join("directory.md");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_export(&blocked, "must not replace a directory").is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
