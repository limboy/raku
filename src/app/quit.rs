//! Confirm a quit that would stop running agents or background commands.
//!
//! The daemon that hosts every provider runtime exits with the desktop
//! process, so quitting ends each live turn and every command it detached.

use super::*;

impl Raku {
    /// Tasks whose agents or background commands a quit would stop. A side
    /// chat or subagent counts as the task it belongs to.
    fn tasks_with_running_work(&self) -> usize {
        self.state
            .sessions
            .iter()
            .filter(|session| {
                session.is_busy() || self.session_has_live_background_work(session.id)
            })
            .map(|session| session.parent_session().unwrap_or(session.id))
            .collect::<HashSet<_>>()
            .len()
    }

    /// Quit at once when nothing is running; otherwise ask first. The window
    /// comes forward so a hidden one can host the prompt.
    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.tasks_with_running_work();
        if running == 0 {
            cx.quit();
            return;
        }
        window.activate_window();
        cx.activate(true);
        // A second Quit while the prompt is up leaves the open one to answer.
        if self.quit_prompt_open {
            return;
        }
        self.quit_prompt_open = true;
        let detail = if running == 1 {
            tr!("quit.running_detail_one")
        } else {
            tr!("quit.running_detail_other", count = running)
        };
        let receiver = window.prompt(
            gpui::PromptLevel::Warning,
            &tr!("quit.running_title"),
            Some(&detail),
            &[
                gpui::PromptButton::cancel(tr!("common.cancel")),
                gpui::PromptButton::new(tr!("quit.quit_anyway")),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let quit = receiver.await.ok() == Some(1);
            let _ = this.update(cx, |this, cx| {
                this.quit_prompt_open = false;
                if quit {
                    cx.quit();
                }
            });
        })
        .detach();
    }
}
