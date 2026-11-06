use super::*;
use base64::Engine as _;

impl Raku {
    pub(super) fn open_text_attachment(
        &mut self,
        session_id: Option<Uuid>,
        detail_id: Uuid,
        name: SharedString,
        text: SharedString,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = session_id {
            self.open_session_link(session_id, cx);
        } else {
            self.open_pasted_text(detail_id, name, text, cx);
        }
    }

    pub(super) fn copy_session_deeplink(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(crate::session_link::url(
            session_id,
        )));
        self.show_success_toast(tr!("session.copied"));
        cx.notify();
    }

    pub(crate) fn open_session_link(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self
            .state
            .sessions
            .iter()
            .any(|session| session.id == session_id)
        {
            self.select_session(session_id, cx);
        } else {
            self.show_toast(tr!("errors.export_session_missing"));
            cx.notify();
        }
    }

    pub(super) fn session_link_import_pending(&self) -> bool {
        if self.session_link_imports.is_empty() {
            return false;
        }
        let Some(owner) = self.selected_composer_draft_key() else {
            return false;
        };
        self.session_link_imports
            .iter()
            .any(|(_, draft)| *draft == owner)
    }

    /// Resolve one link on paste, never from a render or row builder. The
    /// stored file is a snapshot: later changes to the source session cannot
    /// silently change the context attached to a sent message.
    pub(super) fn stage_session_link(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(owner) = self.selected_composer_draft_key() else {
            return;
        };
        if self
            .composer_attachments
            .iter()
            .any(|attachment| attachment.session_id == Some(session_id))
            || self.session_link_imports.contains(&(session_id, owner))
        {
            return;
        }
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            self.show_toast(tr!("errors.export_session_missing"));
            cx.notify();
            return;
        };
        let snapshot = session.detail_loaded.then(|| session.clone());
        let daemon = self.daemon.clone();
        self.session_link_imports.push((session_id, owner));
        cx.notify();
        cx.spawn(async move |raku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let session = match snapshot {
                        Some(session) => session,
                        None => raku_client::persistence::hydrate_session(&daemon, session_id)?
                            .ok_or_else(|| anyhow::anyhow!(tr!("errors.export_session_missing")))?,
                    };
                    let markdown = crate::session_export::markdown(
                        &session,
                        crate::session_export::ExportScope::Conversation,
                    )?;
                    let markdown = format!(
                        "Source: {}\n\n{markdown}",
                        crate::session_link::url(session_id)
                    );
                    let response = daemon.client().request(
                        Uuid::nil(),
                        Uuid::nil(),
                        raku_client::Command::ImportAttachment {
                            name: crate::session_export::suggested_filename(&session),
                            upload: raku_client::attachments::AttachmentUpload::File {
                                data_base64: base64::engine::general_purpose::STANDARD
                                    .encode(markdown.as_bytes()),
                            },
                        },
                    )?;
                    let raku_client::ResponsePayload::AttachmentStored { attachment } = response
                    else {
                        anyhow::bail!("the daemon returned an invalid attachment response");
                    };
                    Ok::<_, anyhow::Error>(ComposerAttachment {
                        mention: attachment.path.display().to_string(),
                        path: attachment.path,
                        client_preview_image: None,
                        name: session.display_title().to_owned().into(),
                        is_dir: false,
                        is_image: false,
                        blob_reference: Some(attachment.reference),
                        pasted_text: None,
                        session_id: Some(session_id),
                    })
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                raku.session_link_imports
                    .retain(|pending| *pending != (session_id, owner));
                match result {
                    Ok(attachment) => {
                        if raku.selected_composer_draft_key() == Some(owner) {
                            raku.composer_attachments.push(attachment);
                            raku.schedule_composer_draft_save(cx);
                        } else if raku.state.sessions.iter().any(|session| {
                            crate::persistence::ComposerDraftKey::for_session(session) == owner
                        }) {
                            raku.store_draft_attachment(Some(owner), attachment, cx);
                        }
                    }
                    Err(error) => raku.show_toast(tr!("errors.attach_session", error = error)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

pub(super) fn session_link_menu_items(
    session_id: Uuid,
    preview: Option<right_panel::AttachmentFile>,
    raku: WeakEntity<Raku>,
) -> Vec<MenuItem> {
    let preview_raku = raku.clone();
    let mut items = vec![MenuItem::new(tr!("session.copy_deeplink"), move |_, cx| {
        let _ = raku.update(cx, |raku, cx| raku.copy_session_deeplink(session_id, cx));
    })];
    if let Some(preview) = preview {
        items.push(MenuItem::new(tr!("session.open_snapshot"), move |_, cx| {
            let _ = preview_raku.update(cx, |raku, cx| {
                raku.open_attachment_file(preview.clone(), cx)
            });
        }));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_links_survive_drafts_queueing_and_sent_message_restoration() {
        let source = Uuid::new_v4();
        let attachment = ComposerAttachment {
            path: "/attachments/session.md".into(),
            client_preview_image: None,
            mention: "/attachments/session.md".into(),
            name: "Earlier conversation".into(),
            is_dir: false,
            is_image: false,
            blob_reference: Some(format!("waku-attachment:{}", Uuid::new_v4())),
            pasted_text: None,
            session_id: Some(source),
        };
        let draft = crate::persistence::ComposerDraftAttachment::from(&attachment);
        let persisted = serde_json::to_value(&draft).unwrap();
        let restored = serde_json::from_value::<crate::persistence::ComposerDraftAttachment>(
            persisted.clone(),
        )
        .unwrap();
        let staged = ComposerAttachment::from(restored);
        let message_attachment = MessageAttachment::from(staged);
        let queued = QueuedMessage::with_presentation(
            "Continue @/attachments/session.md",
            Some("Continue".into()),
            vec![message_attachment.clone()],
        );
        let restored: QueuedMessage =
            serde_json::from_slice(&serde_json::to_vec(&queued).unwrap()).unwrap();
        assert_eq!(restored.visible_content(), "Continue");
        assert_eq!(restored.attachments, vec![message_attachment]);
        let staged = ComposerAttachment::from(restored.attachments[0].clone());
        assert_eq!(staged.session_id, Some(source));
        assert_eq!(staged.name.as_ref(), "Earlier conversation");
        assert_eq!(staged.mention, "/attachments/session.md");
        assert!(!staged.is_image);

        // Older file/snippet attachments remain readable with no migration.
        let mut legacy = persisted;
        legacy.as_object_mut().unwrap().remove("session_id");
        let legacy: crate::persistence::ComposerDraftAttachment =
            serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(legacy.session_id, None);
        let legacy: MessageAttachment =
            serde_json::from_value(serde_json::to_value(legacy).unwrap()).unwrap();
        assert_eq!(legacy.session_id, None);
    }
}
