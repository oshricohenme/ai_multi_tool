<!-- mt:start -->
## Code: use mt, and only mt

This repo is set up for **mt**, a code multitool that answers from a live index of the whole
repo. Use it for **all** code work. It returns the code, its callers, its tests and its data with
spans and hashes, in one call, and changes many files in one checked call.

- **Read and search code with mt, not with grep, sed, cat, head, Read, or another code-search tool**
  (graft, graphify, codegraph): where instructions elsewhere in this file say to use one of those
  first for code, mt takes precedence in this repo.
- **Map, then expand once.** `mt context <Name> --map` lists the target, every caller, its unit and
  integration tests, the types and tables it touches, one line each with an id. Read what the task
  needs in one call: `mt expand 3 7 12`.
- **Don't know the name?** `mt search "how are audit rows written"` finds the code by meaning.
- **Many questions, one call:** `mt query` takes a batch of questions and claims (`expect`).
- **Change code in one batch:** `mt apply` writes every edit, then formats, builds and tests, and
  restores the files if a gate step fails.
- **Long output** (builds, tests, logs, JSON, API responses): `mt run -- <cmd>` or `<cmd> | mt compress`;
  then `mt out <id> --json '$.path'`, `--jq '<filter>'` or `mt out <id> "<question>"` for only what you
  need. Never paste a whole JSON document into the conversation.
- The full reference: `mt ref` (sections: `mt ref <name>`).
- Plain tools stay for what is not code: literal text in docs or config, and running programs.
<!-- mt:end -->
