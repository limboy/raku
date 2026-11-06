//! Shell blocks the agent hands to the user (`sh run` fences).
//!
//! Clicking Run leaves the block untouched and opens a terminal beneath it
//! that runs the command through the user's login shell, so prompts such as
//! a password work and the output keeps its colors. When the command exits
//! the agent picks the conversation back up on its own: a continuation turn
//! with no visible user message carries the exit status and terminal text.

use gpui::Subscription;
use md::render::{CodeRunHooks, CodeRunView};

use crate::terminal::{TerminalCommandFinished, TerminalView};

use super::*;

/// Terminal text sent to the agent: the tail, so a chatty build stays small.
const REPORT_TAIL_CHARS: usize = 16 * 1024;

struct BlockRun {
    session_id: Uuid,
    generation: u64,
    code: String,
    terminal: Entity<TerminalView>,
    running: bool,
    _finished: Subscription,
}

#[derive(Default)]
struct MessageRuns {
    runs: HashMap<usize, BlockRun>,
    snapshot: Rc<HashMap<usize, CodeRunView>>,
}

impl MessageRuns {
    fn refresh_snapshot(&mut self) {
        self.snapshot = Rc::new(
            self.runs
                .iter()
                .map(|(ordinal, run)| {
                    (
                        *ordinal,
                        CodeRunView {
                            running: run.running,
                            terminal: run.terminal.clone().into(),
                        },
                    )
                })
                .collect(),
        );
    }
}

/// Inline command terminals, keyed by message and block. In memory only.
pub(super) struct ShellRuns {
    raku: WeakEntity<Raku>,
    by_message: HashMap<Uuid, MessageRuns>,
    next_generation: u64,
    empty: Rc<HashMap<usize, CodeRunView>>,
}

impl ShellRuns {
    pub(super) fn new(raku: WeakEntity<Raku>) -> Self {
        Self {
            raku,
            by_message: HashMap::new(),
            next_generation: 0,
            empty: Rc::default(),
        }
    }

    /// Render hooks for one settled assistant message. Rendering only clones
    /// handles; terminals are created by the click.
    pub(super) fn hooks(&self, message_id: Uuid) -> CodeRunHooks {
        let runs = self
            .by_message
            .get(&message_id)
            .map(|runs| runs.snapshot.clone())
            .unwrap_or_else(|| self.empty.clone());
        let raku = self.raku.clone();
        CodeRunHooks {
            runs,
            run: Rc::new(move |ordinal, code, window, cx| {
                let _ = raku.update(cx, |raku, cx| {
                    raku.start_block_run(message_id, ordinal, code.to_string(), window, cx)
                });
            }),
        }
    }
}

impl Raku {
    fn start_block_run(
        &mut self,
        message_id: Uuid,
        ordinal: usize,
        code: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self
            .shell_runs
            .by_message
            .get(&message_id)
            .and_then(|runs| runs.runs.get(&ordinal))
            .filter(|run| run.running)
        {
            window.focus(&run.terminal.focus_handle(cx), cx);
            return;
        }
        let Some(session) = self.state.sessions.iter().find(|session| {
            session
                .messages
                .iter()
                .any(|message| message.id == message_id)
        }) else {
            return;
        };
        let session_id = session.id;
        let Some(cwd) = self
            .workspace_path_for_session(session)
            .map(Path::to_path_buf)
        else {
            self.show_toast(tr!("code_run.no_workspace"));
            cx.notify();
            return;
        };

        self.shell_runs.next_generation += 1;
        let generation = self.shell_runs.next_generation;
        let terminal_code = code.clone();
        let terminal = cx.new(|cx| TerminalView::command(cwd, terminal_code, cx));
        let finished = cx.subscribe(
            &terminal,
            move |raku, _, event: &TerminalCommandFinished, cx| {
                raku.finish_block_run(message_id, ordinal, generation, event, cx);
            },
        );
        // Focus follows the command so a password prompt can be answered.
        window.focus(&terminal.focus_handle(cx), cx);
        let runs = self.shell_runs.by_message.entry(message_id).or_default();
        runs.runs.insert(
            ordinal,
            BlockRun {
                session_id,
                generation,
                code,
                terminal,
                running: true,
                _finished: finished,
            },
        );
        runs.refresh_snapshot();
        self.remeasure_block_run_message(message_id);
        cx.notify();
    }

    fn finish_block_run(
        &mut self,
        message_id: Uuid,
        ordinal: usize,
        generation: u64,
        event: &TerminalCommandFinished,
        cx: &mut Context<Self>,
    ) {
        let Some(runs) = self.shell_runs.by_message.get_mut(&message_id) else {
            return;
        };
        let Some(run) = runs
            .runs
            .get_mut(&ordinal)
            .filter(|run| run.generation == generation && run.running)
        else {
            return;
        };
        run.running = false;
        let session_id = run.session_id;
        let report = shell_run_report(&run.code, event.exit_code, &event.output);
        runs.refresh_snapshot();
        self.continue_after_block_run(session_id, report, cx);
        cx.notify();
    }

    /// The agent resumes on its own, as Claude Code does after background
    /// work settles: a turn with no user bubble whose prompt is the result.
    /// A busy session queues it as an ordinary follow-up instead.
    fn continue_after_block_run(
        &mut self,
        session_id: Uuid,
        report: String,
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
        let waiting = session.is_busy()
            || self.submission_preparations.contains(&session_id)
            || self.response_fork_preparations.contains_key(&session_id)
            || self.goal_runtime_starts.contains(&session_id)
            || self.ending_checkpoint_pending(session_id);
        if waiting {
            self.enqueue_follow_up_submission(
                session_id,
                ComposerSubmission {
                    prompt: report,
                    display_content: Some(tr!("code_run.queued_output")),
                    attachments: Vec::new(),
                },
                cx,
            );
            return;
        }
        if let Some(session) = self.state.session_mut(session_id) {
            session.begin_provider_turn();
            session.status = SessionStatus::Connecting;
        }
        self.pending_plans.remove(&session_id);
        if self.state.selected_session == Some(session_id) {
            self.remeasure_transcript_tail();
        }
        self.start_submission_preparation(session_id, ComposerSubmission::plain(report), cx);
    }

    fn remeasure_block_run_message(&self, message_id: Uuid) {
        let index = self.selected_session().and_then(|session| {
            session
                .messages
                .iter()
                .position(|message| message.id == message_id)
        });
        if let Some(index) = index {
            self.remeasure_transcript_message(index);
        }
    }
}

/// The prompt the agent receives. Fences are longer than any backtick run
/// inside, so terminal output cannot close them.
fn shell_run_report(code: &str, exit: Option<i32>, output: &str) -> String {
    let output = output.trim_end();
    let mut cut = output.len().saturating_sub(REPORT_TAIL_CHARS);
    while !output.is_char_boundary(cut) {
        cut += 1;
    }
    let tail = &output[cut..];
    let fence = "`".repeat(longest_backtick_run(&format!("{code}\n{tail}")).max(2) + 1);
    let status = match exit {
        Some(code) => format!("exit code {code}"),
        None => "terminated by a signal".to_owned(),
    };
    let mut report =
        format!("I ran this command in my terminal ({status}):\n\n{fence}sh\n{code}\n{fence}\n\n");
    if tail.is_empty() {
        report.push_str("It printed nothing.");
    } else {
        report.push_str(if cut > 0 {
            "Terminal output (last part):\n\n"
        } else {
            "Terminal output:\n\n"
        });
        report.push_str(&format!("{fence}text\n{tail}\n{fence}"));
    }
    report
}

fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in text.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[cfg(test)]
mod shell_run_tests {
    use super::*;

    #[test]
    fn report_fences_cannot_be_closed_by_output() {
        let report = shell_run_report("echo hi", Some(1), "```\nboom\n");
        assert!(report.starts_with(
            "I ran this command in my terminal (exit code 1):\n\n````sh\necho hi\n````"
        ));
        assert!(report.ends_with("````text\n```\nboom\n````"));
    }

    #[test]
    fn report_keeps_only_the_output_tail() {
        let output = "x".repeat(REPORT_TAIL_CHARS + 10);
        let report = shell_run_report("make", Some(0), &output);
        assert!(report.contains("Terminal output (last part):"));
        assert!(report.len() < REPORT_TAIL_CHARS + 200);
    }
}
