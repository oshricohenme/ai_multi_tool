// Regex classifier for one Claude Code turn: prompt → actions → reply. v3.
// Built against 4,081 real turns and a full read of every one of them (five audits).
// Every label carries its evidence: the rule and the exact text it matched.
//   source    who sent the prompt, and its sub-type
//   intents   everything the prompt asks for; primary = the leading imperative, else the earliest match
//   actions   each tool call: category, and for Bash every program it ran
//   outcome   how the turn ended (+ flags)
//   facts     what an engineer looks for: project, files, PRs, commits, tests, builds, risks
// Plain data + functions: the same file scores the corpus in Node and runs in the browser.
(function () {
  // ── preprocessing ─────────────────────────────────────────────────────────
  const WRAP = /^\s*(q\s+)?(<pasted_content[^>]*>|<artifact-content-authored-by-others\/>|<artifact-view-context[^>]*>[\s\S]*?<\/artifact-view-context>)\s*/i;
  const PASTE = /<pasted_content[\s\S]*?(<\/pasted_content>|$)/gi;
  const INBOX = /^\s*(?:You have \d+ orchestration messages?\. Run `[^`]*`\.?\s*)+/;
  const MACHINE_IDS = /\b(term|ctx|task|msg|run)_[0-9a-f]{8,}/g;
  // the person's own words: pastes, URLs, paths and images blanked so they don't fire rules
  const ownWords = s => s.replace(PASTE, ' ').replace(/https?:\/\/\S+/g, ' ').replace(/(?:~|\.{0,2})?\/?(?:[\w.@-]+\/)+[\w.@-]*/g, ' ').replace(/\[Image #\d+\]/g, ' ');

  // ── source ────────────────────────────────────────────────────────────────
  const SOURCE = [
    ['continuation', /^[\s\S]{0,300}?This session is being continued from a previous conversation/i],
    ['automation', /^\s*Generate a short git branch name|^\s*<task-notification\b/i],
    ['agent-message', /^\s*Another Claude session sent a message|^\s*<teammate-message|^\s*<agent-message\b|^\s*\[?Subagent hand-?back/i],
    ['orchestrator', /^\s*(Please carry out this task from my Orca coordinator|You are (a dedicated Orca worker|a fresh, scoped implementation owner|the coordinator session|implementing Task\b|an? [\w-]+ (monitor|agent|reviewer|worker)\b)|Coordinator:|COORDINATOR follow-up|Fix ONE (bug|thing) in this worktree|You have \d+ orchestration messages?\.|From (the )?[\w -]*coordinator:|From SCHEDULER:|[A-Z-]*COORDINATOR\b[^\n]{0,30}:|USER DIRECTIVES?\b|INTERRUPT FOR USER|URGENT (USER )?OWNERSHIP|Orchestrator decisions?|Read and execute \S+\.md|FYI from [A-Z]{2,}:|New precise shared integration failure|Run tests only in this worktree|Read-only monitor|Poll the |RESUMING: a previous session|Repeat your .* results)/i],
    ['system', /^\s*(Goal set:|<artifact-view-context)/i],
    ['interrupt', /^\s*\[Request interrupted by user/i],
    ['slash', /^\s*\/[a-z][\w:-]*(?=[\s,.:]|$)/i],
    ['human', /[\s\S]/]
  ];
  const SUBSOURCE = {
    'agent-message': [
      ['limit-failed', /"idleReason":"failed"/], ['idle-cc', /"type":"idle_notification"[^}]*"summary":"\[to /], ['idle-result', /"type":"idle_notification"[\s\S]*?"result":/],
      ['question', /<teammate-message[^>]*summary="[^"]*(\?|\b(ask(ing)? lead|questions?|who owns|need user|confirm|awaiting|waiting (for|on)|ready for GO|stop it)\b)/i],
      ['blocker', /<teammate-message[^>]*summary="[^"]*\b(blocked|blocker|STUCK|URGENT|alert|fails?\b|failed|crash(ed)?|denied|refused|wiped|not green|check-all red)/i],
      ['progress', /<teammate-message[^>]*summary="[^"]*(\bchunks? \d+[ab]?(\/\d+)?|Status|\bAck\b|standing (by|down)|\bstarting\b|\brunning\b|heads-up)/i],
      ['result', /<teammate-message[^>]*summary="[^"]*\b(done|DONE|complete|landed|fixed|committed|written|ready|PASS|green|verified)\b/],
      ['hand-back', /^\s*<agent-message\b|Subagent hand-?back/i],
      ['ping', /^\s*[A-Z][A-Z0-9-]{2,30}( [A-Z0-9-]+){0,3}:\s/], ['message', /[\s\S]/]],
    orchestrator: [
      ['task-brief', /^\s*Please carry out this task from my Orca coordinator/i], ['inbox-notice', /^\s*You have \d+ orchestration messages?\./i],
      ['worker-brief', /^\s*(You are (a dedicated Orca worker|a fresh, scoped|implementing Task|an? [\w-]+ (monitor|agent|reviewer|worker))|Read and execute|Run tests only|Read-only monitor|Poll the )/i], ['fix-one', /^\s*Fix ONE (bug|thing)/i],
      ['limit-resume', /^\s*Coordinator: the Claude usage limit has reset/i], ['directive', /USER DIRECTIVES?|Orchestrator decision|INTERRUPT FOR USER|OWNERSHIP/i],
      ['sync', /\b(messages crossed|slot is yours|taking the (single )?(game )?slot|confirm(ed|ing)\b)/i], ['relay', /[\s\S]/]],
    automation: [['task-notification', /^\s*<task-notification\b/i], ['branch-name', /[\s\S]/]], continuation: [['context-reset', /[\s\S]/]], system: [['goal', /^\s*Goal set:/i], ['artifact-view', /[\s\S]/]]
  };

  // ── intents ───────────────────────────────────────────────────────────────
  // Primary = the leading imperative (LEAD) if any, else a short-reply rule, else the earliest
  // match in the prompt. Every other match is kept as a secondary intent with its evidence.
  const INTENT = [
    ['attach', /^\s*(\[Image #\d+\]\s*)+$/],
    ['choose', /^\s*(option\s*)?([a-e]|[1-9])\)?(\s*(,|and|&|\+)\s*([a-e]|[1-9]))*(?=[\s.,)!]|$)(?![\w'])|^\s*(both|all of (them|it)|neither|none|the (first|second|latter|former)|first one|second one)\s*[.!]?\s*$|^\s*i (prefer|like|'?d go with)\b|^\s*only (the|display|show)\b/i],
    ['approve', /^\s*(y|yes|yep|yeah|ok(ay)?|sure|go( ahead| for it)?|do it|proceed|lgtm|sounds good|i approve|approved?|ship it|perfect|great|nice|i like it|looks good|thanks?( you)?|ty|noted|pl\w*se? execute|execute( it)?( please)?|let'?s (do it|go)|go go|do (it )?(all|everything)( in order)?|all good|let it finish)\b/i],
    ['stop', /^\s*(pause|stop|hold( on)?|wait|cancel|abort|halt|forget (that|it|the last)|never ?mind|give me a (sec|minute))\b|\b(let'?s|please|we) (pause|stop|hold)\b|\buntil further notice\b/i],
    ['status', /^\s*(so|and|well|now|ok so)?\s*\?+\s*$|^\s*(so|and|well|ok so)\s*[?.!]*\s*$|^\s*(status|progress|any update|update\??|eta|give me progress)\b|\b(are|r) y?o?u (still |all )?(done|finished|stuck|running|building|working|testing|iterating|there)\b|\b(you|agents?|they|it) (are |is )?stuck\b|\bwhat('?s| is| are you) (going on|left|remaining|happening|the status|doing|working on|waiting (for|on))\b|\b(so )?what'?s left\b|\bhow much (is )?(longer|time|left)\b|\bwhere are we( at)?\b|\bhas everything been\b|\bis (it|everything|the \w+) (done|running|finished|ready|compiled)\b|\bare the \w+( \w+)? ready\b|\bstill working\b|\bstalled\?|\b(is|seems (like )?(it'?s|the \w+ is)?) stuck\b/i],
    ['verify', /^\s*(but\s+)?(did|have|has) (you|it|all|the|everything)\b(?! finish)/i],
    ['resume', /^\s*(resume|continue|keep (going|running)|carry on|go on|next|retry|try again|rerun( it)?|re-run|finish( it| up)?)\b[^a-z]{0,3}$|^\s*keep \w+ing\b|\b(continue|go)\s*[.!]?\s*$|\bresume (all )?(the )?work\b|^\s*resume\b/i],
    ['correct', /^\s*(no\b|nope|nah\b|wrong|that'?s not|not what i|this is not|i meant\b|instead of\b|you need to\b|i thought you\b)|\b(still (broken|failing|wrong|not)|does ?n'?o?t work|didn'?t work|isn'?t working|you (didn'?t|forgot|missed|broke|ignored)|undo|revert that|i didn'?t ask|no such thing)\b|\bshouldn'?t be\b[^.]*\brather\b|\bthere (are|is) still\b/i],
    ['challenge', /^\s*(so )?why (are|did|do|would|is) you\b|\bwhy don'?t you\b|\bare you (actually|really)\b|\blet me do my work\b/i],
    ['coordinate', /\b(use|start|spawn|launch|spin up|open|add|ask|get|tell|take|close|offload to|hand (it )?(off|over) to)\s+(up to\s+)?(an?|the|another|other|more|multiple|many|all( the)?|as many|\d+)?\s*(additional\s+|finished\s+|live\s+)?(claude |codex |coding |haiku |opus |sonnet )?(sessions?|agents?|sub-?agents?|workers?|workflows?|teammates?|coordinator)\b|\bas many agents\b|\bin parallel\b|\bfan (it )?out\b|\borchestrat\w*|\bcoordinate\b|\btake over from\b|\bhand ?over\b|\bcontinue the development\b|\btell (the |teh |all the )?(coordinator|agents?|sessions?|other)\b|\b(ownership|sole (live )?(owner|importer))\b/i],
    ['route-model', /\b(use|switch to|with|on|try)\s+(opus|sonnet|haiku|fable|gpt[\w.-]*|a smarter (one|model))\b|\b(opus|sonnet|haiku|fable)\s*\d(\.\d)?\b/i],
    ['policy', /\b(from now on|always remember|remember (that|this)|add to memory|standing rule|every ?time|every (major )?change should|going forward|you must|must (always|never|not)|should only|only use|by no means|no need to|is allowed (only )?for|never (use|touch|edit|read|change|commit|push)|always (use|run|ask|check)|don'?t forget|keep (this|it) in mind)\b|^\s*(also,?\s+|and,?\s+|but,?\s+|please,?\s+)?(don'?t|do not|never|always|avoid)\b|\bi (don'?t|do not) (want|care)\b/i],
    ['suggest', /\b(why not( just)?|how about|what if|maybe (we|you)|wouldn'?t it be (easier|better)|pretty sure (we|you) can|could we (just )?)\b/i],
    ['report', /^\s*i (have|ran|did|reopened|restarted|installed|opened|see|got|added|changed|just|still have|already|closed|stopped|enabled|turned|logged|left|accepted|quit|set|think|will)\b|\b(feedback given|(has|have) been (generated|delivered|uploaded|added))\b|^\s*(unlocked|done on my side|you have (full )?access|this project is|fyi\b|(code )?freeze lifted|\w+ is (connected|installed|live)|they (all )?seem)/i],
    ['interview', /\bask me\b[^.]{0,30}\bquestions?\b|\bquestion me\b|\binterview me\b|\bask (me )?(all |exhaustive )?questions\b/i],
    ['show', /\b(show|give|send) me\b|\bwhat'?s the (url|link)\b|\bopen (it )?in (the )?(orca )?browser\b|\bgive me the (link|url)\b|\bin the chat\b|\bscreenshots?\b|\bclickable\b|\bso I can (see|view|review)\b/i],
    ['howto', /^\s*(how (do|can|should) i|give me the (command|steps)|what('?s| is) the (command|url|port|endpoint)|which file do i)\b/i],
    ['ship', /\b(commit|comm?i ?t\w*|(open|make|create|raise) (a |the )?pr|pull request|merge\s+(it|this|the (pr|branch)|into|main|master|all|(\w+ )?(into|from))|rebase|cherry-?pick|tag (a )?release|deploy\w*|release it|publish)\b|\bpush\w*\b(?!\s+to the (job )?queue)/i],
    ['review', /^\s*(please\s+)?(review|audit|critique|assess|evaluate)\b|\b(review|audit) (the|this|my|our) (code(base)?|pr|diff|changes?|branch|work)\b|\b(code|security) review\b|\b(play-?test|look over|sanity check|seo sweep)\b/i],
    ['perf', /\b(performance|fps|frame ?rate|stutter\w*|too slow(ly)?|lag(gy|s)?|cpu|gpu|memory (used|usage|leak)|gets? slower|(is|so|really) slow)\b/i],
    ['debug', /\b(fix|bugs?|buggy|broken|error|crash(ed|es|ing)?|exception|stack ?trace|not working|regression|flaky|debug|diagnos\w*|hang(s|ing)?|not loading|freez\w*|flash(es|ing)?|flicker\w*|black screen|stopped working|just stopped|something is wrong|not respon\w*|unable to|anymore|keeps (freezing|crashing|hanging))\b(?! (him|her|me|you))|(?<!(doesn'?t|won'?t|not|never) )\bfail(s|ing|ed)?\b|\b(does not|doesn'?t) seem to (work|load|show)\b|^\s*i (get|got|see) this\b|\b(i'?m|i am) not seeing\b|\bnot (refreshing|updating|clearing|showing|responding|loading|working)\b|\b(doesn'?t|don'?t|won'?t|isn'?t|no longer)\s+(\w+\s){0,2}(work|show|load|appear|close|come|save|stop|trigger|display)\w*/i],
    ['test', /\b((write|add|run|fix) (the )?(unit |e2e |integration )?tests?|test (suite|coverage)|tests? (pass|fail)|jest|vitest|pytest|playwright|cypress|run the test script|unit tests?|verify (it|that|this))\b/i],
    ['research', /\b(research|investigate|look ?up|find out|search (the web|online)|compare|benchmark|best practices?|state of the art|alternatives?|is there (a|an|any) (better|more|other)|pull the|scrape)\b/i],
    ['document', /\b(document(ation)?|readme|write ?up|written out|changelog|summary doc|in a separate doc\w*)\b/i],
    ['plan', /(?<!(learning|lesson|claude|meal) )\b(plan (it|this|out|how|for)|make a plan|write (a|the) plan|let'?s (re)?(plan|design)|(please|help me) (re)?(plan|design)|approach|proposal|brainstorm|ideate|strategy|roadmap|trade-?offs?|options for|let'?s think|(software|system|code) architecture|solution design)\b|^\s*(re)?design\b/i],
    ['refactor', /\b(refactor|clean ?up|simplify|reorganize|restructure|dedupe|deduplicate|migrate|upgrade|bump|optimi[sz]e)\b/i],
    ['data', /\b(analy[sz]e|analysis|stats|statistics|metrics|query|csv|dataset)\b/i],
    ['run', /^\s*(please\s+)?(can you\s+)?(re)?(runt?|start|launch|open|turn (it )?on|boot|restart|serve|spin up|self-play)\b|\b(re)?(run|start|launch|boot)\s+(it|the|this|all|\w+ing)\b|\b(restart|relaunch|reopen|re-?build and|open (the (app|game|site))|install|import into|export|package (it|the)|set ?up(?!\s+script)|configure|login)\b|\bI want you to (self-play|run|play)\b|^\s*(kill|stop|close) all\b|\breload\b/i],
    ['request', /^\s*(but\s+)?(i (also )?(want|would like|'d like|need)|we need)\b|\b(it|this|that|the \w+|they|everything|all \w+) (should|must|needs? to) (also )?(be|have|act|apply|not|only|always|show|work)\b|\bdo (multiple|the next)\b|^\s*make sure\b|\b(should|must) (maintain|match|be able|allow|not|only|also|always|never|open|have)\b|\b(the \w+|it|they|characters?|users?|players?) (will|cannot|can'?t|must|should) (be able|be|have|go|span|equip|fall|walk|open|load|run)\b|^\s*(use|seed|kill) /i],
    ['explain', /\?(\s|"|$)|^\s*(so |oh,? |and |also )?(what|why|how|where|when|who|which|is there|is it|are there|does|do you|am i|could|should|would)\b|\b(explain|walk me through|what does|how does|tell me about|summari[sz]e|i want to understand|what you are saying)\b/i],
    ['build', /\b(add|build|create|implement|make(?! sure)|write|generate|scaffold|introduce|integrate|wire( up)?|hook up|enable|expand|extend|let'?s have|load up|regenerate|recompile|copy|gitignore|organi[sz]e|place|give it)\b/i],
    ['change', /^\s*(please\s+)?(also\s+)?(change|update|move|replace|tweak|adjust|increase|decrease|reduce|raise|lower|remove|delete|drop|hide|rename|swap)\b|\b(change|replace|tweak|adjust|increase|decrease|reduce|remove|delete|rename)\b/i],
    ['feedback', /\b(ugly|generic|terrible|awful|not good|looks (off|wrong|weird|bad)|too (big|biig|small|much|many|long|busy|early|late)|far too|not (clear|great|right)|i (don'?t|do not|can'?t) see|something is missing|confusing|hard to (read|see)|doesn'?t look|madness|ridiculous|waste of|taking (so|too) long|mean nothing|can do (so )?much better|looks (really )?good,? but)\b|^\s*(it|this|that|the \w+) (looks|is|feels|seems) (bad|off|wrong|ugly|weird|missing|generic)\b/i]
  ];
  const LEAD = /^\s*(?:(?:ok|okay|good|great|perfect|yes|so|now|also|then|and|first|next|please|let'?s|i (?:need|want) you to|(?:can|could|would|will) (?:you|we)(?: please)?(?: also)?)[,\s]+)*(implement|build|create|add|make(?! sure)|write|generate|fix|debug|diagnose|review|evaluate|audit|test|run|start|launch|open|restart|remove|delete|rename|change|update|replace|move|commit|push|deploy|merge|research|investigate|compare|plan|design|redesign|explain|summari[sz]e|document|refactor|simplify|clean up|install|set up|configure|show|give(?! me a (sec|minute|moment))|find|list|integrate)\b/i;
  const LEAD_MAP = { implement: 'build', build: 'build', create: 'build', add: 'build', make: 'build', write: 'build', generate: 'build', integrate: 'build', fix: 'debug', debug: 'debug', diagnose: 'debug', review: 'review', evaluate: 'review', audit: 'review', test: 'test', run: 'run', start: 'run', launch: 'run', open: 'run', restart: 'run', install: 'run', 'set up': 'run', configure: 'run', remove: 'change', delete: 'change', rename: 'change', change: 'change', update: 'change', replace: 'change', move: 'change', commit: 'ship', push: 'ship', deploy: 'ship', merge: 'ship', research: 'research', investigate: 'research', compare: 'research', find: 'research', list: 'show', plan: 'plan', design: 'plan', redesign: 'plan', explain: 'explain', summarise: 'explain', summarize: 'explain', document: 'document', refactor: 'refactor', simplify: 'refactor', 'clean up': 'refactor', show: 'show', give: 'show' };
  const SHORT_FIRST = ['attach', 'choose', 'approve', 'stop', 'status', 'verify', 'resume'];

  // ── actions ───────────────────────────────────────────────────────────────
  const PROG = [
    ['test', /^(jest|vitest|pytest|mocha|playwright|cypress|rspec|phpunit|bats)$/, null], ['test', /^(npm|pnpm|yarn|bun)$/, /^\s*\S+\s+(run\s+)?test\b/], ['test', /^(go|cargo|swift|dotnet|deno)$/, /^\s*\S+\s+test\b/], ['test', /^claude$/, /\bplugin test\b/],
    ['check', /^(tsc|eslint|ruff|mypy|gofmt|prettier|shellcheck|biome|vet|golangci-lint)$/, null], ['check', /^(npm|pnpm|yarn|bun)$/, /\b(lint|typecheck|type-check|check|format)\b/], ['check', /^claude$/, /\bplugin validate\b/],
    ['build', /^(make|gradle|mvn|xcodebuild|webpack|esbuild|cmake|ninja)$/, null], ['build', /^(npm|pnpm|yarn|bun|go|cargo|swift|dotnet|vite|next)$/, /\bbuild\b/],
    ['git-write', /^git$/, /^\s*git\s+(-C\s+\S+\s+)?(add|commit|push|reset|checkout|switch|merge|rebase|cherry-pick|stash|tag|rm|mv|restore|clean|worktree add|branch -[dDmM])\b/],
    ['git-read', /^git$/, null], ['github', /^gh$/, null],
    ['package', /^(npm|pnpm|yarn|bun|pip3?|brew|cargo|go|gem|uv|poetry|dotnet)$/, /\b(i|install|add|ci|get|mod|restore|update|upgrade)\b/], ['package', /^npx$/, null],
    ['serve', /^(npm|pnpm|yarn|bun)$/, /\b(dev|start|serve|preview)\b/], ['serve', /^(uvicorn|gunicorn|flask|next|vite|http-server|serve)$/, null],
    ['container', /^(docker|docker-compose|podman|kubectl|helm)$/, null],
    ['code-search', /^(graft|graphify)$/, null],
    ['agent-spawn', /^orca$/, /\b(agent launch|worker-start|worktree create)\b/], ['agent-message', /^orca$/, /\b(terminal send|orchestration (send|reply|check))\b/],
    ['agent-task', /^orca$/, /\borchestration task/], ['agent-reap', /^orca$/, /\bterminal close\b/], ['agents', /^(orca|claude|codex)$/, null],
    ['app', /^(Blender|blender|Unity|unity|open|osascript|ffmpeg|magick|convert|sips|xcrun|simctl|adb)$/, null],
    ['net', /^(curl|wget|http|ssh|scp|rsync|nc|ping|dig)$/, null],
    ['process', /^(ps|pgrep|pkill|kill|lsof|top|killall|launchctl|jobs|wait)$/, null],
    ['wait', /^(sleep|until|while)$/, null],
    ['write', /^(mkdir|mv|cp|rm|touch|chmod|chown|ln|tee|unzip|tar|zip|rmdir|install)$/, null], ['write', /^sed$/, /\bsed\s+-i\b/],
    ['script', /^(python3?|node|bun|deno|ruby|perl|php|bash|sh|zsh|swift|go)$/, null],
    ['search', /^(grep|rg|ag|find|fd|ack|locate)$/, null],
    ['read', /^(cat|head|tail|less|more|wc|sed|awk|jq|cut|sort|uniq|tr|stat|file|du|df|diff|tree|ls|xxd|od|shasum|md5|readlink|realpath|basename|dirname|column|nl|strings)$/, null],
    ['env', /^(pwd|which|whoami|uname|date|env|printenv|echo|printf|export|set|source|\.|test|\[|true|false|cd|type|command|hash|ulimit)$/, null]
  ];
  const RANK = ['test', 'check', 'build', 'github', 'git-write', 'git-read', 'package', 'serve', 'container', 'agent-spawn', 'agent-task', 'agent-message', 'agent-reap', 'agents', 'app', 'net', 'script-edit', 'script', 'write', 'process', 'code-search', 'search', 'read', 'wait', 'env', 'other'];
  function bashParts(cmd) {
    cmd = String(cmd || '');
    const heredoc = cmd.match(/<<-?\s*['"]?\w+['"]?([\s\S]*)/);
    const head = cmd.split(/<<-?\s*['"]?\w+['"]?/)[0];
    // quoted strings are arguments (often whole scripts): blank them before splitting; a clipped one runs to the end
    const bare = head.replace(/"(?:[^"\\]|\\.)*("|$)/g, '""').replace(/'[^']*('|$)/g, "''");
    const parts = [];
    bare.split(/&&|\|\||;|\||\n|\$\(|`/).map(s => s.trim()).filter(Boolean).forEach(s => {
      s = s.replace(/^[({!\s]+/, '').replace(/^(\w+=("[^"]*"|'[^']*'|\S*)\s*)+/, '').replace(/^(sudo|time|nohup|env|exec|command|caffeinate|timeout\s+\S+|for\s+\w+\s+in\s+[^;]*|do|then|else|if|while|until)\s+/, '');
      const prog = ((s.match(/^"([^"]+)"|^'([^']+)'|^(\S+)/) || []).slice(1).find(Boolean) || '').split('/').pop();
      if (!prog || /^(do|done|fi|then|else|esac|in|\}|\)|""|''|[0-9]+>?.*|>.*|<.*)$/.test(prog)) return;
      let cat = 'other';
      for (const [k, re, sub] of PROG) if (re.test(prog) && (!sub || sub.test(s))) { cat = k; break; }
      parts.push({ prog, cat });
    });
    // an inline script that writes files is an edit, not just a script
    const scriptText = (heredoc ? heredoc[1] : '') + ' ' + (head.match(/-[ce]\s+("[\s\S]*|'[\s\S]*)/) || [''])[0];
    if (heredoc || /\b(python3?|node)\s+-[ce]\b/.test(head)) parts.push({ prog: heredoc ? 'heredoc' : 'inline', cat: /open\([^)]*['"][wa]['"]|\.write(_text)?\(|writeFileSync|fs\.write/.test(scriptText) ? 'script-edit' : 'script' });
    const cat = parts.map(p => p.cat).sort((a, b) => RANK.indexOf(a) - RANK.indexOf(b))[0] || 'other';
    return { cat, parts };
  }
  const TOOL = [
    ['read', /^(Read|NotebookRead)$/], ['search', /^(Grep|Glob|LS|ToolSearch|LSP)$|^mcp__(graphify|graft)__/],
    ['edit', /^(Edit|MultiEdit|NotebookEdit)$/], ['write', /^Write$/],
    ['web', /^(WebSearch|WebFetch)$/], ['browser', /^mcp__(claude-in-chrome|playwright|browser|computer-use)/],
    ['delegate', /^(Agent|Task|Workflow)$/], ['message', /^(SendMessage|ListAgents|ReadNotifications)$/], ['agent-control', /^(TaskStop|TaskOutput)$/],
    ['ask', /^AskUserQuestion$/], ['plan', /^(ExitPlanMode|EnterPlanMode|TodoWrite|TaskCreate|TaskUpdate)$/],
    ['publish', /^(Artifact|ArtifactComments|ArtifactData|SendUserFile|PushNotification)$|^mcp__claude_ai_Claude_Docs__/],
    ['wait', /^(Monitor|ScheduleWakeup|CronCreate|CronDelete)$/],
    ['3d', /^mcp__(blender|mixar|meshy|lift)__/], ['skill', /^Skill$/], ['memory', /__(remember|recall)$/], ['mcp', /^mcp__/]
  ];
  const ROLL = { read: 'explore', search: 'explore', edit: 'edit', write: 'edit', web: 'research', browser: 'browser', delegate: 'delegate', message: 'delegate', 'agent-control': 'delegate', ask: 'ask', plan: 'plan', publish: 'publish', wait: 'wait', '3d': '3d', skill: 'skill', memory: 'memory', mcp: 'mcp',
    'bash:test': 'verify', 'bash:check': 'verify', 'bash:build': 'verify', 'bash:git-read': 'explore', 'bash:git-write': 'git', 'bash:github': 'git', 'bash:read': 'explore', 'bash:search': 'explore', 'bash:code-search': 'explore', 'bash:env': 'explore', 'bash:script': 'script', 'bash:script-edit': 'edit', 'bash:write': 'shell', 'bash:package': 'setup', 'bash:serve': 'run', 'bash:container': 'run', 'bash:app': 'run', 'bash:agents': 'delegate', 'bash:agent-spawn': 'delegate', 'bash:agent-message': 'delegate', 'bash:agent-task': 'delegate', 'bash:agent-reap': 'delegate', 'bash:net': 'net', 'bash:process': 'run', 'bash:wait': 'wait', 'bash:other': 'shell' };

  // ── outcome ───────────────────────────────────────────────────────────────
  // where: 'reply' = whole reply, 'end' = last two sentences, 'tail' = last 1200 chars (negated "done" removed)
  const NEG_DONE = /\b(not|n't|nothing|never|no|haven'?t|until|once|when|after|before|will|i'?ll|yet)\b[^.]{0,60}?\b(done|committed|pushed|merged|deployed|added|fixed|live|complete[d]?|finished|updated|shipped)\b/gi;
  const OUTCOME = [
    ['error', /^\s*(API Error|Connection (lost|error)|Request timed out)/i, 'reply'],
    ['limited', /^\s*(You'?ve (hit|reached) your [\w -]*limit|Not logged in)/i, 'reply'],
    ['silent', /^\s*No response requested\.?\s*$/i, 'reply'],
    ['asked', /\?\s*["')\]]*\s*$|\b(tell me which|say (the word|"\w+"|if you)|reply "\w+"|let me know (if|whether|which)|should i|do you want|would you like|shall i|want me to)\b[^.]*\?/i, 'end'],
    ['needs-user', /\b(please (reload|open|try|run|send|paste|restart|unlock|approve|allow)|waiting (on|for) you(r)?|still need(s)? (you|your)|needs? your (go-?ahead|approval|decision)|your (call|go-ahead|decision)|you'?ll need to|until you say|restart claude code)\b/i, 'end'],
    ['pending', /:\s*$|\b(still (running|working|going|watching|finishing|compiling|building|generating|in progress)|is running( now)?|underway|in progress|in the background|(I'?ll|I will) (post|send|report|publish|merge|check|resume|commit|rebuild|relaunch|integrate|review|tell you|confirm|start|run|rerun|pick)\b[^.]*\b(once|when|after|as soon as)|when (it|they|each|all) (finish|report|land)|once (it|they|the \w+) (finish|land|complete))\b/i, 'end'],
    ['blocked', /\bI (still )?(can'?t|cannot|couldn'?t|was unable to|wasn'?t able to|am blocked)\b(?![^.]*\b(simulate|confirm|be sure|tell|honestly|explain|put a number)\b)|\bpermission (was )?denied\b|\bblocked (by|on) (a |the )?(permission|sandbox|policy|auth|login)/i, 'end'],
    ['planned', /^\s*(#+\s*)?(plan|here'?s (the|my) plan|proposed plan)\b/i, 'reply'],
    ['done', /\b(I'?ve|I have|I) (fixed|implemented|added|created|updated|removed|deleted|committed|pushed|opened|wrote|saved|launched|installed|built|shipped|merged|deployed|finished|completed|moved|renamed|replaced)\b|\bPR (#\d+ )?opened\b|\ball (\d+ )?(tests )?pass(ed|ing)?\b|\b(is|are) (now )?(live|done|fixed|working|in place|committed|pushed|merged)\b|^\s*(done|fixed|shipped)\b/im, 'tail'],
    ['acknowledged', /^\s*(got it|understood|saved|noted|will do)\b|saved (it|that) (as a standing rule|to memory)/i, 'reply'],
    ['answered', /\S/, 'reply']
  ];

  // ── facts (all regex-extracted, all optional) ────────────────────────────────
  const FACTS = {
    pr: /github\.com\/[\w.-]+\/[\w.-]+\/pull\/\d+|\bPR #\d+/gi,
    commit: /`([0-9a-f]{7,12})`|\bcommit(?:ted)? (?:locally )?(?:as )?`?([0-9a-f]{7,12})\b/gi,
    tests: /\b(\d+)(?:\/\d+)? (?:\w+ )?(?:tests?|specs?|suites?) (?:pass(?:ed|ing)?|fail(?:ed|ing)?)\b|\bAll (\d+) tests? pass/gi,
    build: /\bBuild ?(\d+)\b/g,
    url: /https?:\/\/(?:127\.0\.0\.1|localhost):\d+\S*/gi,
    artifact: /claude\.ai\/(?:code\/)?artifact\/[\w-]+/gi,
    models: /\b(opus|sonnet|haiku|fable)\s*\d(?:\.\d)?\b/gi,
    cost: /\$\d+(?:\.\d+)?|\b\d+(?:\.\d+)?[kKmM]? tokens\b|\b\d+% of (?:your )?(?:weekly |5-hour )?(?:limit|usage)\b|\bweekly limit\b/gi
  };
  const SECRET = /\b(sk-(?:ant-)?[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|xox[abp]-[A-Za-z0-9-]{20,}|AKIA[0-9A-Z]{16}|msy_[A-Za-z0-9]{16,}|re_[A-Za-z0-9]{20,}|AIza[0-9A-Za-z_-]{30,})/;
  const FRUSTRATION = /\b(madness|ridiculous|horrible|this is bad|for no reason|it hurts?|seriously|wtf|clearly)\b|\b(again|still)\?/i;
  const DESTRUCTIVE = /\brm -rf\b|\bgit (reset --hard|push (-f|--force)|clean -fd)\b|\bpkill\b|\bkillall\b|\bdrop (table|database)\b/i;
  const FOLLOWUP = /^\s*(also|and|but|then|instead|in addition|same (for|with|on)|likewise|it|this|that|they|them|oops|otherwise|so)\b/i;
  const UNVERIFIED = /\b(not (verified|tested|run|measured|confirmed)|haven'?t (run|tested|checked|confirmed|opened|verified)|wasn'?t (run|tested)|untested|unverified)\b/i;
  const SHIP_STATE = /\b(committed locally|push (failed|is blocked)|not pushed|nothing (is |has been )?committed|uncommitted|PR (#\d+ )?(opened|merged)|pushed to \S+|merged (into|to) \S+)\b/i;
  const WAITING_ON = /\bwaiting (for|on) (you|the [\w -]{3,30}?|\w+)(?=[.,;]|\s(to|before|until)|$)/i;

  const hit = (re, s) => { re.lastIndex = 0; const m = re.exec(s); return m ? { at: m.index, text: m[0].trim().slice(0, 80) } : null; };
  const all = (re, s) => { const out = new Set(); let m; re.lastIndex = 0; while ((m = re.exec(s))) { out.add((m.slice(1).find(Boolean) || m[0]).slice(0, 80)); if (out.size > 20 || m[0] === '') break; } return [...out]; };

  // prev: { outcome, prompt } of the previous turn in the same session, if known
  function classify(turn, prev) {
    const raw = String(turn.prompt || '');
    const wrapper = (raw.match(WRAP) || [''])[0];
    const p = raw.replace(WRAP, '');
    let source = 'human', sourceEv = null, sub = null, failed = null;
    for (const [k, re] of SOURCE) { const m = hit(re, k === 'system' ? raw : p); if (m) { source = k; sourceEv = k === 'human' ? '' : m.text; break; } }
    let text = p;
    if (source === 'orchestrator' && INBOX.test(p)) {
      const rest = p.replace(INBOX, '').trim();
      if (rest && !/^(From |[A-Z-]*COORDINATOR|New precise)/.test(rest)) { source = 'human'; sourceEv = 'text after an inbox notice'; text = rest; }
    }
    // an agent's ping ("ENGINE: …") carries machine ids or is long; a person's "TODO: …" is not
    if (source === 'human' && /^\s*[A-Z][A-Z0-9-]{2,30}( [A-Z0-9-]+){0,3}:\s/.test(p) && ((p.match(MACHINE_IDS) || []).length >= 1 || p.length > 600)) { source = 'agent-message'; sourceEv = (p.match(/^\s*[A-Z][\w -]{2,40}:/) || [''])[0].trim(); }
    if (source === 'human' && /<pasted_content/i.test(wrapper) && !ownWords(p).trim()) source = 'pasted';
    if (SUBSOURCE[source]) for (const [k, re] of SUBSOURCE[source]) if (re.test(p)) { sub = k; break; }
    if (source !== 'human') for (const [k, re] of [['not-logged-in', /^\s*Not logged in/i], ['limit', /You'?ve hit your [\w -]*limit|usage limit/i]]) if (re.test(String(turn.reply || ''))) { failed = k; break; }

    // intents, on the person's own words only
    const intents = [];
    const own = ownWords(text).slice(0, 2000), short = own.trim();
    if (source === 'human' || source === 'pasted') {
      INTENT.forEach(([k, re]) => {
        if (SHORT_FIRST.includes(k) && k !== 'status' && k !== 'verify' && k !== 'stop' && short.length > 160) return;
        const m = hit(re, own); if (m) intents.push({ k, ev: m.text, at: m.at });
      });
      const polite = /^\s*(also\s+)?(can|could|would|will)\s+(you|we)\s+(please\s+)?(also\s+)?(?!explain|tell)/i.test(short);
      const usable = intents.filter(x => !(polite && x.k === 'explain'));
      let primary = null;
      const lead = short.match(LEAD);
      if (lead) primary = { k: LEAD_MAP[lead[1].toLowerCase()] || 'build', ev: lead[0].trim().slice(0, 80), at: 0 };
      if (!primary) {
        const solo = usable.find(x => SHORT_FIRST.includes(x.k) && short.length <= 120);
        primary = solo || usable.slice().sort((a, b) => a.at - b.at || INTENT.findIndex(r => r[0] === a.k) - INTENT.findIndex(r => r[0] === b.k))[0];
      }
      // an answer to Claude's own question
      if (prev && prev.outcome === 'asked' && short.length < 120 && !lead && (!primary || ['choose', 'approve', 'explain', 'request', 'report'].includes(primary.k))) {
        const r = { k: 'reply', ev: 'answers the previous turn\'s question', at: -1 };
        if (!primary || primary.k !== 'choose') primary = r; else intents.push(r);
      }
      if (!primary && /\b(should|must|needs? to|has to|only|always|never|whenever|when|once|it'?s|is|are|will)\b/i.test(short) && short.length > 20) primary = { k: 'spec', ev: (short.match(/\b(should|must|needs? to|has to|only|always|never|whenever|when|once|it'?s|is|are|will)\b/i) || [''])[0], at: 0 };
      if (!primary && polite) primary = { k: 'request', ev: (short.match(/^\s*\S+\s+\S+/) || [''])[0], at: 0 };
      if (!primary) primary = { k: short ? 'unclassified' : (/\[Image #/.test(text) ? 'attach' : 'context'), ev: '', at: 0 };
      const rest = intents.filter(x => x.k !== primary.k);
      intents.length = 0; intents.push(primary, ...rest);
    } else if (source === 'slash') intents.push({ k: 'command', ev: (p.match(/^\s*\/[\w:-]+/) || [''])[0] });
    else intents.push({ k: source, ev: sourceEv });

    // actions
    const actions = (turn.actions || []).map(a => {
      if (a.tool === 'Bash') { const b = bashParts(a.arg); return { tool: 'Bash', cat: 'bash:' + b.cat, roll: ROLL['bash:' + b.cat], progs: b.parts.map(x => x.prog), arg: a.arg, err: !!a.err }; }
      let cat = 'other'; for (const [k, re] of TOOL) if (re.test(a.tool || '')) { cat = k; break; }
      return { tool: a.tool, cat, roll: ROLL[cat] || 'other', progs: [], arg: a.arg, err: !!a.err };
    });
    const roll = {}; actions.forEach(a => { roll[a.roll] = (roll[a.roll] || 0) + 1; });
    const dominant = (Object.entries(roll).sort((a, b) => b[1] - a[1])[0] || ['none'])[0];

    // outcome
    const reply = String(turn.reply || '');
    const tail = reply.slice(-1200).replace(NEG_DONE, ' ');
    const end = tail.trim().split(/(?<=[.!?:])\s+/).slice(-2).join(' ');
    let outcome = reply.trim() ? 'answered' : 'silent', outcomeEv = '';
    for (const [k, re, where] of OUTCOME) { const m = hit(re, where === 'reply' ? reply : where === 'end' ? end : tail); if (m) { outcome = k; outcomeEv = k === 'answered' ? '' : m.text; break; } }
    const last = actions[actions.length - 1];
    if (actions.some(a => a.cat === 'ask') && !['error', 'limited'].includes(outcome)) { outcome = last && last.cat === 'ask' && last.err ? 'interrupted' : 'asked'; outcomeEv = 'AskUserQuestion'; }
    else if (last && last.err && reply.length < 200 && !['error', 'limited', 'asked', 'pending'].includes(outcome)) { outcome = 'blocked'; outcomeEv = 'last action failed: ' + last.tool; }
    if (actions.length && actions.every(a => a.roll === 'delegate') && ['answered', 'done'].includes(outcome)) { outcome = 'delegated'; outcomeEv = actions.length + ' delegation calls, no other work'; }
    if (turn.open) { outcome = 'in progress'; outcomeEv = 'the last step has not finished'; } // live turns only
    const doneRe = OUTCOME.find(o => o[0] === 'done')[1];
    const flags = {
      offered: outcome === 'asked' && hit(doneRe, tail) ? true : undefined,
      cutOff: reply && /[,]\s*$|\b(the|and|to|a|of)\s*$/.test(reply) ? true : undefined,
      unverified: (reply.match(UNVERIFIED) || [])[0],
      shipState: (reply.match(SHIP_STATE) || [])[0],
      waitingOn: (reply.match(WAITING_ON) || [])[0]
    };

    // facts
    const cmds = actions.map(a => a.arg || '').join('\n');
    const files = [...new Set(actions.filter(a => ['edit', 'write', 'bash:script-edit'].includes(a.cat)).map(a => (String(a.arg).match(/(?:[\w.@\[\]-]+\/)*[\w.@-]+\.\w{1,6}/) || [''])[0]).filter(Boolean))];
    const where = (cmds.match(/\/(?:orca|conductor)\/workspaces\/([\w -]+?)\/([\w.-]+)/) || []);
    const fr = source === 'human' ? hit(FRUSTRATION, own) : null;
    const facts = {
      project: String(turn.project || '').replace(/^-Users-[^-]+-/, '').replace(/^(orca|conductor)-(workspaces-)?/, ''),
      worktree: where[2] || undefined,
      files, read: [...new Set(actions.filter(a => a.cat === 'read').map(a => String(a.arg)).filter(Boolean))].slice(0, 20),
      pr: all(FACTS.pr, reply), commits: all(FACTS.commit, reply), tests: all(FACTS.tests, reply), builds: all(FACTS.build, reply + ' ' + text),
      urls: all(FACTS.url, reply), artifacts: all(FACTS.artifact, reply), models: all(FACTS.models, text), cost: all(FACTS.cost, reply),
      ids: all(MACHINE_IDS, raw + ' ' + cmds).slice(0, 10),
      images: (raw.match(/\[Image #\d+\]/g) || []).length, pasted: /<pasted_content/i.test(raw),
      secret: SECRET.test(raw) || undefined, frustration: fr ? fr.text : undefined,
      destructive: (cmds.match(DESTRUCTIVE) || [])[0], followup: (source === 'human' && FOLLOWUP.test(own)) || undefined,
      fanout: actions.filter(a => a.roll === 'delegate').length || undefined,
      repeat: (prev && prev.prompt && prev.prompt.trim() === raw.trim()) || undefined
    };
    return { source, sourceEv, sub, failed, intent: intents[0].k, intents, actions, roll, dominant, outcome, outcomeEv, flags, facts, errs: actions.filter(a => a.err).length, n: actions.length };
  }

  const api = { classify, bashParts, SOURCE, SUBSOURCE, INTENT, PROG, TOOL, OUTCOME };
  if (typeof module !== 'undefined') module.exports = api;
  if (typeof window !== 'undefined') window.CLASSIFY = api;
})();

// ── turns from the live dev-tools event log (same shape as the transcript corpus) ──
(function () {
  const api = typeof module !== 'undefined' ? module.exports : window.CLASSIFY;
  const argOf = payload => {
    let a; try { a = JSON.parse(payload || '{}'); } catch (e) { return String(payload || '').slice(0, 300); }
    for (const k of ['command', 'file_path', 'path', 'pattern', 'url', 'query', 'description', 'prompt', 'skill', 'notebook_path']) if (typeof a[k] === 'string' && a[k]) return a[k].slice(0, 300);
    return JSON.stringify(a).slice(0, 200);
  };
  const answerOf = result => { const m = String(result || '').match(/ANSWER\n([\s\S]*?)(\n\nTOOL USES\n|\n\nSERVER TOOLS\n|$)/); return m ? m[1] : ''; };
  // one turn per main-thread prompt; subagent work stays inside the Agent call that spawned it
  api.fromEvents = function (events, sid, project) {
    const ev = events.slice().sort((a, b) => a.start - b.start);
    const turns = [], byTurn = new Map();
    ev.forEach(e => {
      if (e.agentId) return;
      if (e.kind === 'user') { const t = { sid, project, ts: e.start, end: e.end, turnId: e.turnId, id: e.id, prompt: e.payload || e.title || '', actions: [], reply: '', calls: 0, open: false }; turns.push(t); byTurn.set(e.turnId, t); return; }
      const t = byTurn.get(e.turnId); if (!t) return;
      if (e.kind === 'tool') t.actions.push({ tool: e.tool, arg: argOf(e.payload), err: !!e.isError, out: String(e.result || '').slice(0, 240), id: e.id });
      else if (e.kind === 'assistant') { t.calls++; const a = answerOf(e.result); if (a.trim()) t.reply = a.slice(-3000); }
      if (e.kind === 'tool' || e.kind === 'assistant') t.open = e.end === undefined || e.end === null; // the last step decides: still running?
    });
    turns.forEach((t, i) => { if (i < turns.length - 1) t.open = false; }); // only the latest turn can still be running
    return turns;
  };
  // classify a session's turns in order, so "answers the previous question" and repeats work
  api.classifyAll = function (turns) {
    const last = {};
    return turns.map(t => { const c = api.classify(t, last[t.sid]); last[t.sid] = { outcome: c.outcome, prompt: t.prompt }; return c; });
  };
  // label groups for the view
  api.GROUPS = {
    ask: ['build', 'request', 'spec', 'run', 'debug', 'ship', 'change', 'review', 'research', 'document', 'plan', 'perf', 'test', 'data', 'refactor', 'howto', 'show'],
    steer: ['status', 'reply', 'resume', 'approve', 'choose', 'stop', 'correct', 'challenge', 'feedback', 'verify', 'suggest'],
    context: ['policy', 'report', 'interview', 'route-model', 'coordinate', 'attach', 'context'],
    other: ['unclassified', 'command']
  };
  api.groupOf = k => Object.keys(api.GROUPS).find(g => api.GROUPS[g].includes(k)) || 'machine';
})();
