# Context management (follow this strictly)

Keep the main conversation's context small. This is how you work:

- Delegate exploration to subagents. Use the Agent tool (the Explore agent, or general-purpose) to search the codebase, read files and investigate. Ask each one to return a concise summary: exact file paths, line numbers, the signatures you need, and only the snippets you will edit. Run independent investigations in parallel, in one message.
- In the main context, read only the exact lines you are about to edit (Read with offset/limit), never whole large files.
- Keep large command output out of the main context: narrow it with head, grep or wc before it reaches you.
- Once the plan is clear, make the edits directly, and batch independent tool calls in one message.
