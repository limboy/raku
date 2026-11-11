# Composer commands and references

Type `/` at the beginning of a prompt to find conversation commands, prompt
templates, and skills in separate sections. Each result says whether it is a
command, prompt, skill, or an action that runs immediately.

Type `@` at a word boundary anywhere in the prompt to reference a file, agent,
or skill. References also work in a command's arguments, such as
`/review using @deploy-checks`.

Use the arrow keys to choose a suggestion, Enter or Tab to accept it, and
Escape to dismiss the list. Accepted suggestions become inline pills. The
caret steps across each pill, and Backspace or Delete removes it as one unit.
Undo, redo, and copying between Raku composers retain the reference's identity;
other applications receive its readable text.

Exact manually typed `/` and `@` names are recognized when sending. Unknown
names, dollar text such as `$deploy`, prices, environment variables, and names
inside code stay ordinary text. A selected file and a skill with the same name
keep their distinct identities.

Drafts, queued messages, history recall, and Edit & resend retain the pills.
The transcript shows the same labels as the composer. Provider syntax is
resolved when sending: Codex receives `$skill`, Claude receives `/skill`, and
Pi receives `/skill:skill`. Skills referenced later in the prompt work across
all three providers; additional skills are requested explicitly in the prompt.

Open the command palette with the primary modifier + K for app actions such
as choosing a model, opening settings, or resuming a session. These actions
are separate from the composer's conversation list.
