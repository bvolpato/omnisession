"use client";

import { useCallback, useEffect, useMemo, useReducer, useRef, useState, type KeyboardEvent, type PointerEvent, type ReactNode } from "react";

import { AgentBullet } from "./agent-bullet";
import { agentById, agents, targets, type Agent, type AgentId } from "./agents";
import { demoSessions, type DemoSession } from "./demo-sessions";

type Screen = "browse" | "target" | "transfer" | "done";

type State = {
  screen: Screen;
  query: string;
  row: number;
  source: number;
  allWorkspaces: boolean;
  filter: string;
  target: number;
  mode: number;
  step: number;
};

type Action =
  | { type: "query"; value: string }
  | { type: "move"; by: number }
  | { type: "row"; to: number }
  | { type: "side"; by: number }
  | { type: "scope" }
  | { type: "enter" }
  | { type: "back" }
  | { type: "step" }
  | { type: "reset" };

type Mode = readonly [name: string, flags: string, summary: string];
type Target = { agent: Agent; fork: boolean };

const workspace = "payments-api";
const sourceOrder: readonly (AgentId | null)[] = [
  null,
  ...agents.map((agent) => agent.id).filter((id) => demoSessions.some((session) => session.agent === id)),
];

const defaultMode: Mode = ["default", "no flags", "the agent's own settings decide"];
const alwaysAsk = "every edit and command asks";
const acceptEdits = "edits apply without asking; commands still ask";
const autoSummary = "safe actions run on their own; risky ones still ask";
const yoloSummary = "nothing asks; every command runs";
const modes: Partial<Record<AgentId, readonly Mode[]>> = {
  codex: [defaultMode, ["auto", "--approve-for-me", autoSummary], ["yolo", "--dangerously-bypass-approvals-and-sandbox", yoloSummary]],
  "claude-code": [defaultMode, ["accept-edits", "--permission-mode acceptEdits", acceptEdits], ["auto", "--permission-mode auto", autoSummary], ["yolo", "--dangerously-skip-permissions", yoloSummary]],
  omp: [defaultMode, ["always-ask", "--approval-mode always-ask", alwaysAsk], ["accept-edits", "--approval-mode write", acceptEdits], ["yolo", "--approval-mode yolo", yoloSummary]],
  grok: [defaultMode, ["accept-edits", "--permission-mode acceptEdits", acceptEdits], ["auto", "--permission-mode auto", autoSummary], ["yolo", "--always-approve", yoloSummary]],
  "cursor-agent": [defaultMode, ["auto", "--auto-review", autoSummary], ["yolo", "--force", yoloSummary]],
  antigravity: [defaultMode, ["accept-edits", "--mode accept-edits", acceptEdits], ["auto", "--mode auto", autoSummary], ["yolo", "--dangerously-skip-permissions", yoloSummary]],
  opencode: [defaultMode, ["yolo", "--auto", yoloSummary]],
  hermes: [defaultMode, ["yolo", "--yolo", yoloSummary]],
};

const initial: State = { screen: "browse", query: "", row: 3, source: 0, allWorkspaces: false, filter: "", target: 0, mode: 0, step: 0 };

function words(value: string) {
  return value.toLowerCase().split(/\s+/).filter(Boolean);
}

/** Where a query matched: titles, folders, branches, and IDs rank before conversation text. */
function matchKind(session: DemoSession, query: string): "meta" | "text" | null {
  const wanted = words(query.replaceAll('"', ""));
  if (wanted.length === 0) return "meta";
  const agent = agentById(session.agent);
  const metaText = `${session.title} ${session.project} ${session.branch} ${agent.cli} ${agent.aliases ?? ""} ${session.id}`.toLowerCase();
  if (wanted.every((word) => metaText.includes(word))) return "meta";
  const all = `${metaText} ${session.text} ${session.answer.toLowerCase()}`;
  return wanted.every((word) => all.includes(word)) ? "text" : null;
}

function visibleSessions(state: State) {
  const source = sourceOrder[state.source];
  const scoped = demoSessions.filter(
    (session) => (state.allWorkspaces || state.query !== "" || session.project === workspace) && (!source || session.agent === source),
  );
  const matched = scoped.map((session) => ({ session, kind: matchKind(session, state.query) })).filter((match) => match.kind !== null);
  return [...matched.filter((match) => match.kind === "meta"), ...matched.filter((match) => match.kind === "text")];
}

/** Same-agent choices first, as the real target page does, then every import target. */
function targetChoices(session: DemoSession | null, filter: string): readonly Target[] {
  const wanted = words(filter);
  const own = session ? targets.find((agent) => agent.id === session.agent) : undefined;
  const choices: Target[] = [
    ...(own ? [{ agent: own, fork: false }, ...(own.same.includes("fork") ? [{ agent: own, fork: true }] : [])] : []),
    ...targets.filter((agent) => agent.id !== own?.id && (session || agent.capabilities.clean_start.length > 0)).map((agent) => ({ agent, fork: false })),
  ];
  return choices.filter(({ agent }) => wanted.every((word) => `${agent.name} ${agent.cli} ${agent.id} ${agent.aliases ?? ""}`.toLowerCase().includes(word)));
}

function selectedSession(state: State) {
  return state.row === 0 ? null : visibleSessions(state)[state.row - 1]?.session ?? null;
}

function clamp(value: number, max: number) {
  return Math.max(0, Math.min(max, value));
}

function reduce(state: State, action: Action): State {
  switch (action.type) {
    case "query": {
      if (state.screen === "target") return { ...state, filter: action.value, target: 0, mode: 0 };
      if (state.screen !== "browse") return state;
      const next = { ...state, query: action.value };
      const count = visibleSessions(next).length;
      return { ...next, row: action.value === "" ? Math.min(initial.row, count) : Math.min(1, count) };
    }
    case "move":
      if (state.screen === "browse") return { ...state, row: clamp(state.row + action.by, visibleSessions(state).length) };
      if (state.screen === "target") {
        const count = targetChoices(selectedSession(state), state.filter).length;
        return count === 0 ? state : { ...state, target: (state.target + action.by + count) % count, mode: 0 };
      }
      return state;
    case "row":
      if (state.screen === "browse") return { ...state, row: clamp(action.to, visibleSessions(state).length) };
      if (state.screen === "target") return { ...state, target: action.to, mode: action.to === state.target ? state.mode : 0 };
      return state;
    case "side": {
      if (state.screen === "browse") {
        const next = { ...state, source: (state.source + action.by + sourceOrder.length) % sourceOrder.length };
        return { ...next, row: Math.min(1, visibleSessions(next).length) };
      }
      if (state.screen === "target") {
        const choice = targetChoices(selectedSession(state), state.filter)[state.target];
        const available = choice ? modes[choice.agent.id] : undefined;
        return available ? { ...state, mode: clamp(state.mode + action.by, available.length - 1) } : state;
      }
      return state;
    }
    case "scope": {
      const next = { ...state, allWorkspaces: !state.allWorkspaces };
      return state.screen === "browse" ? { ...next, row: Math.min(state.row, visibleSessions(next).length) } : state;
    }
    case "enter":
      if (state.screen === "browse") return { ...state, screen: "target", filter: "", target: 0, mode: 0 };
      if (state.screen === "target") {
        return targetChoices(selectedSession(state), state.filter)[state.target] ? { ...state, screen: "transfer", step: 0 } : state;
      }
      return state.screen === "done" ? initial : state;
    case "back":
      if (state.screen === "target") return state.filter ? { ...state, filter: "", target: 0, mode: 0 } : { ...state, screen: "browse" };
      if (state.screen === "browse") {
        if (!state.query) return state;
        const next = { ...state, query: "" };
        return { ...next, row: Math.min(initial.row, visibleSessions(next).length) };
      }
      return initial;
    case "step": {
      if (state.screen !== "transfer") return state;
      const session = selectedSession(state);
      const choice = targetChoices(session, state.filter)[state.target] ?? null;
      const nextStep = state.step + 1;
      if (!choice || nextStep > transferLines(session, choice).length) {
        return { ...state, screen: "done", step: nextStep };
      }
      return { ...state, step: nextStep };
    }
    case "reset":
      return initial;
  }
}

function newId(session: DemoSession | null, target: Agent) {
  let hash = 2166136261;
  for (const char of `${session?.id ?? "new"}${target.id}`) hash = Math.imul(hash ^ char.charCodeAt(0), 16777619) >>> 0;
  return hash.toString(16).padStart(8, "0");
}

/** What the CLI prints for this choice. */
function transferLines(session: DemoSession | null, choice: Target): readonly string[] {
  const { agent, fork } = choice;
  if (!session) return [`Starting new ${agent.launch} session...`];
  if (session.agent === agent.id) return fork ? [`Forking ${agent.cli}:${session.id}…`, `Launching ${agent.launch}...`] : [`Launching ${agent.launch}...`];
  const created = `${agent.cli}:${newId(session, agent)}…`;
  return [
    `Importing ${session.messages + session.tools} trajectory items into ${agent.launch}...`,
    `Verifying imported session \`${created}\`...`,
    `Imported and verified \`${created}\`.`,
    agent.id === "cursor-ide" ? `Created and verified ${created}. Opening imported chat in Cursor IDE.` : `Created and verified ${created}. Launching ${agent.launch}...`,
  ];
}

function actionText(session: DemoSession | null, choice: Target) {
  if (!session) return `Start new session in ${choice.agent.launch}`;
  if (choice.fork) return "Fork session";
  return session.agent === choice.agent.id ? "Continue original session" : `Open continuation in ${choice.agent.launch}`;
}

function Highlight({ text, query }: { text: string; query: string }) {
  const wanted = words(query.replaceAll('"', "")).filter((word) => /^[\w./-]+$/.test(word));
  if (wanted.length === 0) return text;
  const pattern = new RegExp(`(${wanted.map((word) => word.replace(/[.\\/-]/g, "\\$&")).join("|")})`, "gi");
  return text.split(pattern).map((part, index): ReactNode => (index % 2 === 1 ? <mark key={index}>{part}</mark> : part));
}

/** The scripted tour. Each entry waits, then acts. It stops for good at the first human input. */
const script: readonly (readonly [delay: number, action: Action])[] = [
  ...[..."redis"].map((_, index, all): readonly [number, Action] => [index === 0 ? 1600 : 130, { type: "query", value: all.slice(0, index + 1).join("") }]),
  [1300, { type: "enter" }],
  [1100, { type: "move", by: 1 }],
  [380, { type: "move", by: 1 }],
  [1500, { type: "enter" }],
];

export function PickerDemo() {
  const [state, dispatch] = useReducer(reduce, initial);
  const [auto, setAuto] = useState<"idle" | "playing" | "off">("idle");
  const [cursor, setCursor] = useState(0);
  const frame = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);

  const rows = useMemo(() => visibleSessions(state), [state]);
  const session = selectedSession(state);
  const choices = targetChoices(session, state.filter);
  const choice = choices[clamp(state.target, Math.max(0, choices.length - 1))];
  const lines = choice ? transferLines(session, choice) : [];
  const family = session?.family ? demoSessions.filter((candidate) => candidate.family === session.family) : [];

  const takeOver = useCallback(() => setAuto("off"), []);

  useEffect(() => {
    const element = frame.current;
    if (!element || window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry?.isIntersecting) setAuto((current) => (current === "idle" ? "playing" : current));
      },
      { threshold: 0.35 },
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (auto !== "playing") return;
    const entry = script[cursor];
    if (!entry) {
      setAuto("off");
      return;
    }
    const timer = window.setTimeout(() => {
      dispatch(entry[1]);
      setCursor(cursor + 1);
    }, entry[0]);
    return () => window.clearTimeout(timer);
  }, [auto, cursor]);

  // The transfer prints one line at a time, then the agent opens.
  useEffect(() => {
    if (state.screen !== "transfer") return;
    const timer = window.setTimeout(() => dispatch({ type: "step" }), state.step === 0 ? 350 : 620);
    return () => window.clearTimeout(timer);
  }, [state.screen, state.step]);
  const screen: Screen = state.screen;

  useEffect(() => {
    const container = list.current;
    const selected = container?.querySelector<HTMLElement>('[aria-selected="true"]');
    if (!container || !selected) return;
    const top = selected.offsetTop - container.offsetTop;
    if (top < container.scrollTop) container.scrollTop = top;
    else if (top + selected.offsetHeight > container.scrollTop + container.clientHeight) container.scrollTop = top + selected.offsetHeight - container.clientHeight;
  }, [state.row, state.target, state.screen, state.query, state.filter, state.mode]);

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.metaKey || event.altKey || event.nativeEvent.isComposing) return;
    const keys: Record<string, Action> = {
      ArrowUp: { type: "move", by: -1 },
      ArrowDown: { type: "move", by: 1 },
      ArrowLeft: { type: "side", by: -1 },
      ArrowRight: { type: "side", by: 1 },
      Enter: screen === "done" ? { type: "reset" } : { type: "enter" },
      Escape: { type: "back" },
    };
    const control: Record<string, Action> = {
      p: { type: "move", by: -1 },
      n: { type: "move", by: 1 },
      u: { type: "query", value: "" },
    };
    const action = event.ctrlKey ? control[event.key] : event.shiftKey ? undefined : keys[event.key];
    if (!action) return;
    event.preventDefault();
    takeOver();
    dispatch(action);
  }

  function onPointerDown(event: PointerEvent<HTMLDivElement>) {
    takeOver();
    // Keep typing in the terminal after a click on a row.
    if (!(event.target instanceof HTMLButtonElement) && event.pointerType === "mouse") {
      event.preventDefault();
      input.current?.focus({ preventScroll: true });
    }
  }

  function pick(index: number, selected: boolean) {
    takeOver();
    if (selected) dispatch({ type: "enter" });
    else dispatch({ type: "row", to: index });
  }

  function replay() {
    dispatch({ type: "reset" });
    setCursor(0);
    setAuto("playing");
  }

  const optionId = (index: number) => `omni-demo-option-${index}`;
  const source = sourceOrder[state.source];
  const typed = screen === "target" ? state.filter : state.query;

  return (
    <div aria-label="Interactive omni demo with invented sessions" className="demo" id="demo" ref={frame} role="group">
      <div className="demo-window" onPointerDown={onPointerDown}>
        <div className="demo-bar">
          <span aria-hidden="true" className="demo-dots"><i /><i /><i /></span>
          <span className="demo-title">{screen === "done" && choice ? `${choice.agent.short} · ~/src/${session?.project ?? workspace}` : `omni · ~/src/${workspace}`}</span>
          <span className="demo-live">{auto === "playing" ? "Playing" : "Live demo"}</span>
        </div>

        <input
          aria-activedescendant={screen === "browse" ? optionId(state.row) : screen === "target" && choices.length > 0 ? optionId(state.target) : undefined}
          aria-autocomplete="list"
          aria-controls="omni-demo-list"
          aria-expanded={screen === "browse" || screen === "target"}
          aria-label={screen === "target" ? "Filter agents in the demo" : "Search demo sessions"}
          autoCapitalize="none"
          autoComplete="off"
          autoCorrect="off"
          className="demo-input"
          onChange={(event) => {
            takeOver();
            dispatch({ type: "query", value: event.target.value });
          }}
          onFocus={takeOver}
          onKeyDown={onKeyDown}
          ref={input}
          role="combobox"
          spellCheck={false}
          value={typed}
        />

        {screen === "browse" ? (
          <div className="demo-body">
            <p className="t-strong"><span className="demo-wide-inline">OmniSession  </span>SESSION BROWSER  ·  {rows.length} {rows.length === 1 ? "session" : "sessions"}</p>
            <p className="t-accent demo-scope">
              <span>Scope  <button onClick={() => dispatch({ type: "scope" })} tabIndex={-1} type="button">{state.allWorkspaces || state.query ? "all workspaces" : "current workspace"}</button></span>
              <span>Source  <button onClick={() => dispatch({ type: "side", by: 1 })} tabIndex={-1} type="button">{source ? agentById(source).cli : "all sources"} [←/→]</button></span>
              <span className="demo-wide">Target  choose after source</span>
            </p>
            <p className={state.query ? undefined : "t-muted"}>Search › {state.query || <>type to filter<span className="demo-wide-inline"> titles, folders, branches, IDs, and conversation text</span></>}<i className={state.query || auto === "off" ? "demo-caret" : "demo-caret idle"} /></p>
            <div className="demo-panes">
              <div className="demo-list">
                <div aria-hidden="true" className="demo-row demo-head t-muted"><span>AGENT</span><span>TITLE</span><span className="demo-wide">PROJECT</span><span><span className="demo-wide-inline">UPDATED</span></span></div>
                <div aria-label="Demo sessions" className="demo-rows" id="omni-demo-list" ref={list} role="listbox" tabIndex={-1}>
                  {[null, ...rows].map((match, index) => {
                    const selected = index === state.row;
                    return (
                      <div
                        aria-selected={selected}
                        className={selected ? "demo-row selected" : match ? "demo-row" : "demo-row t-accent"}
                        id={optionId(index)}
                        key={match?.session.id ?? "new"}
                        onClick={() => pick(index, selected)}
                        role="option"
                      >
                        <span><b aria-hidden="true">{selected ? "›" : " "}</b>{match?.session.tree && !state.query ? <i aria-hidden="true" className="t-muted">{match.session.tree} </i> : null}{match ? agentById(match.session.agent).cli : "+"}</span>
                        <span>{match ? <Highlight query={state.query} text={match.session.title} /> : "NEW SESSION"}</span>
                        <span className="demo-wide">{match?.session.project ?? workspace}</span>
                        <span>{match?.session.updated ?? ""}</span>
                      </div>
                    );
                  })}
                  {rows.length === 0 ? <p className="t-muted demo-none">No session matches. Try redis, refund, or qwen3.8.</p> : null}
                </div>
              </div>
              <div className="demo-detail">
                {session ? (
                  <>
                    <p className="t-accent">SELECTED SESSION</p>
                    <p className="t-strong">{session.title}</p>
                    <p className="t-muted">{agentById(session.agent).cli} · {session.messages} messages · {session.updated}</p>
                    {rows[state.row - 1]?.kind === "text" ? <p className="demo-match t-ok">Matched in conversation text</p> : null}
                    {family.length > 1 ? (
                      <>
                        <p className="t-accent demo-gap">SESSION TREE · {family.length} sessions</p>
                        {family.map((member) => (
                          <p className={member.id === session.id ? "t-strong demo-tree" : "t-muted demo-tree"} key={member.id}><i aria-hidden="true">{member.tree}</i>{agentById(member.agent).cli}<span>{member.updated}</span></p>
                        ))}
                      </>
                    ) : null}
                    <p className="t-accent demo-gap">WORKSPACE</p>
                    <dl>
                      <div><dt>Directory</dt><dd>~/src/{session.project}</dd></div>
                      <div><dt>Branch</dt><dd>{session.branch}</dd></div>
                      <div><dt>Tokens</dt><dd>{session.tokens} total</dd></div>
                      <div><dt>Session</dt><dd>{agentById(session.agent).cli}:{session.id}…</dd></div>
                    </dl>
                  </>
                ) : (
                  <>
                    <p className="t-accent">NEW SESSION</p>
                    <p className="t-muted">Start clean in any installed agent.</p>
                  </>
                )}
              </div>
            </div>
            <p className="demo-foot t-muted">
              <button onClick={() => dispatch({ type: "move", by: -1 })} tabIndex={-1} type="button">↑</button>
              <button onClick={() => dispatch({ type: "move", by: 1 })} tabIndex={-1} type="button">↓ move</button>
              <button onClick={() => dispatch({ type: "enter" })} tabIndex={-1} type="button">Enter {session ? "continue" : "start"}</button>
              <button onClick={() => dispatch({ type: "back" })} tabIndex={-1} type="button">Esc clear</button>
              <button onClick={() => dispatch({ type: "side", by: 1 })} tabIndex={-1} type="button">←/→ source</button>
            </p>
          </div>
        ) : null}

        {screen === "target" ? (
          <div className="demo-body">
            <p className="t-strong">OmniSession  Choose target</p>
            <p className="t-muted">{session ? `Source: ${agentById(session.agent).cli}:${session.id}…` : "New session"}</p>
            <p className="demo-gap">{session ? "Where should this session open?" : "Where should this new session start?"}</p>
            <p className={state.filter ? undefined : "t-muted"}>Filter › {state.filter || "type an agent name, such as grok or agy"}<i className="demo-caret" /></p>
            <div aria-label="Demo targets" className="demo-rows demo-targets" id="omni-demo-list" ref={list} role="listbox" tabIndex={-1}>
              {choices.map((candidate, index) => {
                const selected = candidate === choice;
                const available = modes[candidate.agent.id];
                const mode = available?.[state.mode];
                return (
                  <div aria-selected={selected} className={selected ? "demo-target selected" : "demo-target"} id={optionId(index)} key={`${candidate.agent.id}${candidate.fork}`} onClick={() => pick(index, selected)} role="option">
                    <AgentBullet agent={candidate.agent} size={26} />
                    <span className="demo-target-text">
                      <strong>{candidate.agent.launch}{candidate.fork ? " · fork" : ""}</strong>
                      <span className={selected ? "t-accent" : "t-muted"}>{actionText(session, candidate)}</span>
                      {selected && available && mode ? (
                        <span className="demo-mode t-muted">
                          Mode {available.map(([name], position) => <b className={position === state.mode ? (name === "yolo" ? "t-danger" : "t-accent") : undefined} key={name}>{position === state.mode ? `[${name}]` : ` ${name} `}</b>)} ←/→
                          <span className={mode[0] === "yolo" ? "t-warn" : undefined}>{mode[1]} · {mode[2]}</span>
                        </span>
                      ) : null}
                    </span>
                  </div>
                );
              })}
              {choices.length === 0 ? <p className="t-muted demo-none">No available target matches this filter.</p> : null}
            </div>
            <p className="demo-foot t-muted">
              <button onClick={() => dispatch({ type: "move", by: -1 })} tabIndex={-1} type="button">↑</button>
              <button onClick={() => dispatch({ type: "move", by: 1 })} tabIndex={-1} type="button">↓ target</button>
              <button onClick={() => {
                const count = choice ? (modes[choice.agent.id]?.length ?? 0) : 0;
                dispatch({ type: "side", by: count > 0 && state.mode >= count - 1 ? -state.mode : 1 });
              }} tabIndex={-1} type="button">←→ mode</button>
              <button onClick={() => dispatch({ type: "enter" })} tabIndex={-1} type="button">Enter open</button>
              <button onClick={() => dispatch({ type: "back" })} tabIndex={-1} type="button">Esc back</button>
            </p>
          </div>
        ) : null}

        {screen === "transfer" ? (
          <div className="demo-body demo-log">
            <p><span className="t-ok">$</span> omni</p>
            {lines.slice(0, state.step).map((line) => <p key={line}>{line}</p>)}
            <i className="demo-caret" />
          </div>
        ) : null}

        {screen === "done" && choice ? (
          <div className={`demo-body demo-done line-${choice.agent.id}`}>
            <p className="demo-done-head">
              <AgentBullet agent={choice.agent} size={34} />
              <span><strong>{choice.agent.name}</strong><span className="t-muted">{session ? `${choice.agent.cli}:${session.agent === choice.agent.id && !choice.fork ? session.id : newId(session, choice.agent)}…` : "new session"}</span></span>
            </p>
            {session ? (
              <>
                <p className="demo-rule t-muted"><span>{session.messages} messages carried over{session.agent === choice.agent.id ? "" : `, ${session.tools} tool records kept as history`}</span></p>
                <p className="demo-ask">{session.title}.</p>
                <p className="demo-answer">{session.answer}</p>
                <p className="demo-ask">{session.next}<i className="demo-caret" /></p>
              </>
            ) : <p className="demo-ask"><i className="demo-caret" /></p>}
            <p className="demo-foot t-muted">
              {session && session.agent !== choice.agent.id ? <span>{agentById(session.agent).cli}:{session.id}… is unchanged.</span> : null}
              <button onClick={() => dispatch({ type: "reset" })} tabIndex={-1} type="button">Enter run it again</button>
            </p>
          </div>
        ) : null}
      </div>

      <p aria-live="polite" className="sr-only">
        {screen === "done" && choice ? `${session ? `${session.title}: continued` : "New session started"} in ${choice.agent.name}.` : ""}
      </p>
      <div className="demo-caption">
        <p><strong>Click it and type.</strong> This is the real flow with invented sessions: search, <kbd>Enter</kbd>, choose an agent, <kbd>Enter</kbd>.</p>
        {auto === "playing"
          ? <button className="text-button" onClick={takeOver} type="button">Pause</button>
          : <button className="text-button" onClick={replay} type="button">Play the tour</button>}
      </div>
    </div>
  );
}
