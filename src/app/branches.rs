use super::*;

impl Raku {
    pub(super) fn sync_branch_picker_rows(&self, rows: &[crate::git_branch::BranchEntry]) {
        let mut cached = self.branch_picker_row_cache.borrow_mut();
        if cached.as_slice() == rows {
            return;
        }
        *cached = rows.to_vec();
        self.branch_picker_list_state
            .reset_with_uniform_height(rows.len(), px(BRANCH_PICKER_ROW_HEIGHT));
    }

    /// Read the selected workspace's cached Git branches, starting one
    /// background fetch on a miss. The previous selected-path snapshot remains
    /// drawable while an invalidation is being refreshed.
    pub(super) fn branch_snapshot_for_workspace(
        &mut self,
        workspace_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<BranchSnapshot> {
        let workspace_path = workspace_path.to_path_buf();
        let fallback = self
            .visible_branch_snapshot
            .as_ref()
            .filter(|(path, _)| path == &workspace_path)
            .map(|(_, snapshot)| snapshot.clone());

        match self.branch_snapshots.read(&workspace_path) {
            Query::Ready(result) => match result.as_ref() {
                Ok(Some(snapshot)) => {
                    let snapshot = snapshot.clone();
                    self.visible_branch_snapshot = Some((workspace_path, snapshot.clone()));
                    Some(snapshot)
                }
                Ok(None) => {
                    if self
                        .visible_branch_snapshot
                        .as_ref()
                        .is_some_and(|(path, _)| path == &workspace_path)
                    {
                        self.visible_branch_snapshot = None;
                    }
                    None
                }
                Err(_) => fallback,
            },
            Query::Pending => fallback,
            Query::Missing(token) => {
                let fetch_path = workspace_path.clone();
                let workspace = raku_client::WorkspaceClient::new(self.daemon.client());
                cx.spawn(async move |raku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn({
                            let fetch_path = fetch_path.clone();
                            async move {
                                match workspace.request(
                                    raku_client::WorkspaceOperation::InspectBranches {
                                        cwd: fetch_path.clone(),
                                    },
                                ) {
                                    Ok(raku_client::WorkspaceResult::Branches { snapshot }) => {
                                        Ok(snapshot)
                                    }
                                    Ok(_) => {
                                        Err("the daemon returned an invalid branch response"
                                            .to_owned())
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            }
                        })
                        .await;
                    let _ = raku.update(cx, |raku, cx| {
                        if !raku.branch_snapshots.fulfill(token, result.clone()) {
                            return;
                        }
                        let selected = raku
                            .selected_workspace_path()
                            .is_some_and(|path| path == fetch_path);
                        if selected {
                            match result {
                                Ok(Some(snapshot)) => {
                                    let mut persisted_branch_changed = false;
                                    if let Some(current) = snapshot.current.as_deref()
                                        && let Some(session) = raku.selected_session_mut()
                                        && let SessionWorkspace::Worktree { branch, .. } =
                                            &mut session.workspace
                                        && branch != current
                                    {
                                        *branch = current.to_owned();
                                        persisted_branch_changed = true;
                                    }
                                    raku.visible_branch_snapshot = Some((fetch_path, snapshot));
                                    if persisted_branch_changed {
                                        raku.save();
                                    }
                                }
                                Ok(None) => raku.visible_branch_snapshot = None,
                                Err(_) => {}
                            }
                            cx.notify();
                        }
                    });
                })
                .detach();
                fallback
            }
        }
    }

    pub(super) fn refresh_selected_branch_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.visible_branch_snapshot = None;
            return;
        };
        self.branch_snapshots.invalidate(&path);
        cx.notify();
    }

    /// Select an existing branch. A planned worktree remembers it as the base
    /// ref without touching the ordinary checkout; concrete workspaces run a
    /// real `git switch` on the background executor.
    ///
    /// `true` asks the caller to dismiss the picker after this entity update
    /// ends. Closing sooner runs the toggle observer, which re-enters `Raku`
    /// and double-leases the entity.
    pub(super) fn choose_workspace_branch(
        &mut self,
        branch: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.selected_session() else {
            return false;
        };
        if session.is_busy() || self.branch_operation_pending {
            return false;
        }
        if matches!(session.workspace, SessionWorkspace::NewWorktree { .. }) {
            let changed = self.selected_session_mut().is_some_and(|session| {
                let SessionWorkspace::NewWorktree { base_branch } = &mut session.workspace else {
                    return false;
                };
                if base_branch.as_deref() == Some(branch.as_str()) {
                    return false;
                }
                *base_branch = Some(branch);
                true
            });
            if changed {
                self.save();
                cx.notify();
            }
            return true;
        }

        let Some(path) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            return false;
        };
        if self
            .visible_branch_snapshot
            .as_ref()
            .filter(|(snapshot_path, _)| snapshot_path == &path)
            .and_then(|(_, snapshot)| snapshot.current.as_deref())
            == Some(branch.as_str())
        {
            return true;
        }
        self.start_branch_checkout(path, branch, cx);
        true
    }

    pub(super) fn move_branch_picker_highlight(
        &mut self,
        key: &str,
        actions: &[BranchPickerAction],
        cx: &mut Context<Self>,
    ) {
        if actions.is_empty() {
            return;
        }
        let current = self
            .branch_picker_highlight
            .filter(|index| *index < actions.len());
        let next = match (key, current) {
            ("up", Some(0)) => actions.len() - 1,
            ("up", Some(index)) => index - 1,
            ("up", None) => actions.len() - 1,
            (_, Some(index)) => (index + 1) % actions.len(),
            (_, None) => 0,
        };
        self.branch_picker_highlight = Some(next);
        if let Some(BranchPickerAction::Checkout(branch)) = actions.get(next)
            && let Some(row) = self
                .branch_picker_row_cache
                .borrow()
                .iter()
                .position(|entry| entry.name == *branch)
        {
            self.branch_picker_list_state.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// Apply the keyboard-selected action, returning whether the caller should
    /// dismiss the picker after releasing its `Raku` update lease.
    pub(super) fn confirm_branch_picker_action(
        &mut self,
        actions: &[BranchPickerAction],
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(action) = actions.get(self.branch_picker_highlight.unwrap_or(0)) else {
            return false;
        };
        match action {
            BranchPickerAction::Checkout(branch) => {
                self.choose_workspace_branch(branch.clone(), cx)
            }
        }
    }

    fn start_branch_checkout(&mut self, path: PathBuf, branch: String, cx: &mut Context<Self>) {
        if self.branch_operation_pending {
            return;
        }
        self.branch_operation_pending = true;
        cx.notify();
        let workspace = raku_client::WorkspaceClient::new(self.daemon.client());
        cx.spawn(async move |raku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        match workspace.request(
                            raku_client::WorkspaceOperation::CheckoutBranch {
                                cwd: path,
                                branch,
                                create: false,
                            },
                        )? {
                            raku_client::WorkspaceResult::BranchChanged { snapshot } => {
                                Ok(snapshot)
                            }
                            _ => anyhow::bail!("the daemon returned an invalid branch response"),
                        }
                    }
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                raku.branch_operation_pending = false;
                match result {
                    Ok(snapshot) => {
                        let current = snapshot.current.clone();
                        raku.visible_branch_snapshot = Some((path.clone(), snapshot));
                        raku.branch_snapshots.invalidate(&path);
                        let selected_path = raku
                            .selected_workspace_path()
                            .map(std::path::Path::to_path_buf);
                        if selected_path.as_ref() == Some(&path) {
                            if let Some(current) = current
                                && let Some(session) = raku.selected_session_mut()
                                && let SessionWorkspace::Worktree { branch, .. } =
                                    &mut session.workspace
                            {
                                *branch = current;
                            }
                            raku.invalidate_workspace_queries(cx);
                            raku.reload_clean_right_panel_file_editors(cx);
                            raku.save();
                        }
                    }
                    Err(error) => {
                        raku.show_toast(tr!("errors.change_branch", error = error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
