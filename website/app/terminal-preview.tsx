import { providers } from "./providers.generated";

type Tone = "amber" | "cyan" | "green" | "magenta";

// Synthetic sessions only. The preview must never show transcript text.
const sessions: readonly { agent: string; branch: string; project: string; selected?: boolean; title: string; tone: Tone; updated: string }[] = [
  { agent: "codex", branch: "┌─", project: "payments", selected: true, title: "Fix refresh-token race", tone: "amber", updated: "8m" },
  { agent: "claude", branch: " └─", project: "payments", title: "Add jitter to token retries", tone: "magenta", updated: "5m" },
  { agent: "pi", branch: "   └─", project: "payments", title: "Cover refresh race in tests", tone: "cyan", updated: "2m" },
  { agent: "grok", branch: "", project: "gateway", title: "Sliding-window rate limiter", tone: "green", updated: "1h" },
  { agent: "cursor-agent", branch: "", project: "storefront", title: "Paginate the orders API", tone: "cyan", updated: "3h" },
  { agent: "opencode", branch: "", project: "storefront", title: "Move CI to pnpm 11", tone: "amber", updated: "1d" },
  { agent: "hermes", branch: "", project: "platform", title: "Draft v2 migration plan", tone: "magenta", updated: "2d" },
  { agent: "antigravity", branch: "", project: "platform", title: "Tidy feature flags", tone: "green", updated: "4d" },
];

const tree = sessions.filter((session) => session.branch !== "");

export function TerminalPreview() {
  const claudeGate = providers.find((provider) => provider.id === "claude-code")?.signal;

  return (
    <div
      aria-label="Illustration of the omni session browser: synthetic sessions from eight agents in one list, and a Codex session continuing in Claude Code after version gate, structural validation, and read-back checks."
      className="preview"
      role="img"
    >
      <div className="terminal">
        <div className="terminal-bar">
          <i />
          <i />
          <i />
          <span>omni</span>
        </div>
        <div className="terminal-body">
          <div className="picker">
            <p className="picker-title"><strong>OmniSession</strong><span>SESSION BROWSER · {sessions.length} sessions</span></p>
            <p className="picker-meta">
              <span><b>Scope</b> current workspace [Tab]</span>
              <span><b>Source</b> all sources [←/→]</span>
            </p>
            <p className="picker-search">Search › <span className="picker-placeholder">title, folder, branch, or ID</span><i className="caret" /></p>
            <div className="picker-table">
              <div className="picker-row picker-header"><span>AGENT</span><span>TITLE</span><span>PROJECT</span><span>UPDATED</span></div>
              <div className="picker-row picker-new"><span>+</span><span>NEW SESSION</span><span>payments</span><span /></div>
              {sessions.map((session) => (
                <div className={session.selected ? "picker-row selected" : "picker-row"} key={session.title}>
                  <span><span className="picker-branch">{session.branch}</span><span className={`agent-${session.tone}`}>{session.agent}</span></span>
                  <span>{session.title}</span>
                  <span>{session.project}</span>
                  <span>{session.updated}</span>
                </div>
              ))}
            </div>
          </div>
          <div className="picker-detail">
            <p className="detail-label">SELECTED SESSION</p>
            <p className="detail-title">Fix refresh-token race</p>
            <p className="detail-dim">codex · payments · 8m</p>
            <p className="detail-label">SESSION TREE · {tree.length} sessions · {tree.length} agents</p>
            <ul className="detail-tree">
              {tree.map((session, index) => (
                <li key={session.title}>
                  <span className="picker-branch">{"  ".repeat(index)}{index === 0 ? "●" : "└ ○"}</span>
                  <span className={`agent-${session.tone}`}>{session.agent}</span>
                  <span>{session.title}</span>
                </li>
              ))}
            </ul>
            <dl className="detail-meta">
              <div><dt>Directory</dt><dd>~/src/payments</dd></div>
              <div><dt>Branch</dt><dd>fix/token-race</dd></div>
            </dl>
          </div>
        </div>
        <p className="terminal-foot">↵ select · Tab all workspaces · ←/→ source · ? help</p>
      </div>
      <div className="report-card">
        <p className="report-command"><span>$</span> omni resume codex:8f3c… --in claude</p>
        <ul>
          <li><b>✓</b> version gate<em>{claudeGate}</em></li>
          <li><b>✓</b> structural validation</li>
          <li><b>✓</b> read-back verified</li>
          <li className="report-launch"><b>→</b> launching claude</li>
        </ul>
      </div>
    </div>
  );
}
