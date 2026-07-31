import { CopyInstallCommand } from "./copy-install-command";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const installCommand = "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh";

const providers = [
  { id: "claude-code", logo: "claude-code", name: "Claude Code", same: "Resume + fork", cross: "Native session", signal: ">= 2.1.220", tone: "amber" },
  { id: "codex", logo: "codex", name: "Codex", same: "Resume + fork", cross: "App-server import", signal: ">= 0.146.0", tone: "amber" },
  { id: "opencode", logo: "opencode", name: "OpenCode", same: "Resume + fork", cross: "Official import", signal: "OFFICIAL", tone: "cyan" },
  { id: "grok", logo: "grok", name: "Grok", same: "Resume + fork", cross: "ACP import", signal: ">= 0.2.114", tone: "cyan" },
  { id: "hermes", logo: "hermes", name: "Hermes", same: "Resume + fork", cross: "Official import", signal: ">= 0.19.1", tone: "cyan" },
  { id: "antigravity", logo: "antigravity", name: "Antigravity", same: "Resume", cross: "Linux state import", signal: ">= 1.1.8", tone: "magenta" },
  { id: "pi", logo: "pi", name: "Pi", same: "Resume + fork", cross: "v3 JSONL", signal: ">= 0.82.0", tone: "amber" },
  { id: "cursor-agent", logo: "cursor", name: "Cursor Agent", same: "Resume", cross: "Native session", signal: ">= 2026.07.23", tone: "magenta" },
  { id: "cursor-ide", logo: "cursor", name: "Cursor IDE", same: "Restore chat", cross: "Desktop state", signal: ">= 3.12.17", tone: "amber" },
] as const;

type Provider = (typeof providers)[number];
type Tone = "amber" | "cyan" | "magenta" | "green";

function Mark() {
  return <img className="brand-logo" src={`${basePath}/logo.svg`} width="32" height="32" alt="" />;
}

function ProviderLogo({ provider }: { provider: Provider }) {
  const className = provider.logo === "codex" ? "provider-logo provider-logo-color" : "provider-logo";
  return <img aria-hidden="true" className={className} src={`${basePath}/providers/${provider.logo}.svg`} alt="" width="24" height="24" />;
}

function CellMeter({ label, value, lit, total = 12, tone = "amber" }: {
  label: string;
  value: string;
  lit: number;
  total?: number;
  tone?: Tone;
}) {
  return (
    <div className={`cell-meter meter-${tone}`} role="img" aria-label={`${label}: ${value}`}>
      <div className="meter-caption"><span>{label}</span><strong>{value}</strong></div>
      <div className="cell-strip" aria-hidden="true">
        {Array.from({ length: total }, (_, index) => <i className={index < lit ? "lit" : ""} key={index} />)}
      </div>
    </div>
  );
}

function StatusReadout({ label, value, tone = "cyan", compact = false }: {
  label: string;
  value: string;
  tone?: Tone;
  compact?: boolean;
}) {
  return (
    <div className={`status-readout readout-${tone}${compact ? " readout-compact" : ""}`}>
      <span>{label}</span><strong><i aria-hidden="true" />{value}</strong>
    </div>
  );
}

function PanelHead({ label, state, tone = "cyan" }: { label: string; state?: string; tone?: Tone }) {
  return (
    <div className="panel-head">
      <span>{label}</span>
      {state ? <strong className={`state state-${tone}`}><i />{state}</strong> : null}
    </div>
  );
}

const rasterGlyphs = [
  { tone: "c", rows: ["1111", "1001", "1001", "1001", "1001", "1001", "1111"] },
  { tone: "a", rows: ["10001", "11011", "10101", "10001", "10001", "10001", "10001"] },
  { tone: "m", rows: ["10001", "11001", "10101", "10011", "10001", "10001", "10001"] },
  { tone: "c", rows: ["111", "010", "010", "010", "010", "010", "111"] },
] as const;

const raster = [
  ".".repeat(22),
  ...Array.from({ length: 7 }, (_, rowIndex) => `.${rasterGlyphs.map(({ tone, rows }) => (rows[rowIndex] ?? "").replaceAll("1", tone).replaceAll("0", ".")).join(".")}.`),
  ".".repeat(22),
];

function SignalRaster() {
  return (
    <div className="signal-raster" aria-hidden="true">
      {raster.flatMap((row, rowIndex) => [...row].map((cell, cellIndex) => (
        <i className={cell === "a" ? "raster-amber" : cell === "c" ? "raster-cyan" : cell === "m" ? "raster-magenta" : ""} key={`${rowIndex}-${cellIndex}`} />
      )))}
    </div>
  );
}

export default function Home() {
  return (
    <main id="top">
      <nav className="nav shell" aria-label="Primary navigation">
        <a className="brand" href="#top" aria-label="OmniSession home"><Mark /><span>OmniSession</span></a>
        <div className="nav-status"><span>LOCAL INDEX</span><i className="nav-status-dot" aria-hidden="true" /><strong>READY</strong></div>
        <div className="nav-links"><a href="#workflow">Flow</a><a href="#support">Agents</a><a href="#install">Install</a><a href="https://github.com/bvolpato/omnisession">GitHub ↗</a></div>
      </nav>

      <section className="hero shell">
        <div className="hero-copy">
          <p className="kicker kicker-amber">SESSION PORTABILITY / LOCAL</p>
          <h1>Continue local sessions<br />across coding agents.</h1>
          <p className="lede">Run <code>omni</code> to search local history, inspect related sessions, and choose which installed agent opens the continuation.</p>
          <div className="hero-actions">
            <a className="button button-primary" href="#install">Install omni <span>↓</span></a>
            <a className="button button-quiet" href="https://github.com/bvolpato/omnisession">View source ↗</a>
          </div>
          <StatusReadout label="SUPPORTED AGENTS" value="09 READY" tone="cyan" />
        </div>

        <div className="hero-console">
          <PanelHead label="OMNI / CONTINUITY READOUT" state="SIGNAL LOCK" tone="amber" />
          <div className="console-grid">
            <div className="session-reading">
              <span className="reading-label">SELECTED SESSION</span>
              <strong>fix refresh token race</strong>
              <code>~/src/payments · auth-refresh · main</code>
            </div>
            <SignalRaster />
            <div className="route-reading">
              <div className="agent-node"><ProviderLogo provider={providers[1]} /><span><small>SOURCE</small>codex</span></div>
              <div className="route-cells" aria-label="Transfer route connected">{Array.from({ length: 9 }, (_, index) => <i className="lit" style={{ animationDelay: `${index * 180}ms` }} key={index} />)}</div>
              <div className="agent-node active"><ProviderLogo provider={providers[0]} /><span><small>TARGET</small>claude</span></div>
            </div>
            <div className="console-meters">
              <StatusReadout label="VISIBLE HISTORY" value="128 / 128" tone="amber" compact />
              <StatusReadout label="WORKSPACE MATCH" value="100%" tone="cyan" compact />
              <StatusReadout label="SECRETS COPIED" value="00" tone="magenta" compact />
            </div>
          </div>
          <div className="console-command"><span>$</span><code>omni resume d8f7c1a4-2e9b-4c36-a5f1-7b0d2e8c9a44 --in claude</code><i aria-hidden="true" /></div>
        </div>

        <div className="hero-side-readout">
          <span>LINE / 01</span><strong>READ → MAP → VERIFY → OPEN</strong><small>No daemon. Transfers leave source sessions unchanged.</small>
        </div>
      </section>

      <section className="agent-band" aria-label="Supported agents">
        <div className="shell agent-list">
          {providers.map((provider) => (
            <span className="agent-chip" key={provider.id}><ProviderLogo provider={provider} /><span>{provider.name}</span></span>
          ))}
        </div>
      </section>

      <section className="browser-section shell" aria-labelledby="browser-title">
        <div className="section-heading">
          <div><p className="kicker kicker-cyan">LOCAL SESSION INDEX</p><h2 id="browser-title">Search every local session from one screen.</h2></div>
          <p>Filter by title, message, ID, directory, branch, or agent. Results show matching context. Selection details include model, reasoning mode, token count, and related sessions when recorded.</p>
        </div>
        <div className="browser-layout">
          <figure className="screenshot-panel">
            <PanelHead label="SESSION BROWSER / LIVE INDEX" state="WARM" tone="cyan" />
            <div className="screenshot-command" aria-label="Run omni to open session browser">
              <code><span>$</span> omni</code>
              <small>ONE COMMAND / LOCAL INDEX</small>
            </div>
            <img src={`${basePath}/session-browser.png`} width="1564" height="620" loading="lazy" alt="OmniSession terminal picker with a cross-agent session tree and conversation preview" />
            <figcaption>FILTER LEFT / SESSION SIGNAL + LINEAGE RIGHT</figcaption>
          </figure>
          <aside className="browser-meters">
            <StatusReadout label="INDEX LOAD" value="09 / 09" tone="cyan" />
            <StatusReadout label="INDEX STATE" value="WARM" tone="amber" />
            <StatusReadout label="SELECTION DRIFT" value="00" tone="magenta" />
            <p>Warm index renders first. Provider refresh continues without replacing current result set.</p>
          </aside>
        </div>
      </section>

      <section className="workflow-section shell" id="workflow">
        <div className="section-heading narrow">
          <div><p className="kicker kicker-amber">ROUTE SEQUENCE</p><h2>Run omni, pick a session, choose an agent.</h2></div>
        </div>
        <div className="flow-panel">
          <article><span className="flow-number">01</span><div><h3>Open session browser</h3><p>Run <code>omni</code>. Sessions from current workspace appear first.</p></div><StatusReadout label="INDEX" value="READY" tone="cyan" compact /></article>
          <article><span className="flow-number">02</span><div><h3>Search local history</h3><p>Type a title, message, directory, branch, ID, or agent.</p></div><StatusReadout label="MATCH" value="PRECISE" tone="amber" compact /></article>
          <article><span className="flow-number">03</span><div><h3>Choose target agent</h3><p>Resume, fork, or continue in another installed agent.</p></div><StatusReadout label="READ-BACK" value="PASS" tone="green" compact /></article>
        </div>
      </section>

      <section className="transfer-section shell">
        <div className="transfer-copy">
          <p className="kicker kicker-magenta">FIDELITY READOUT</p>
          <h2>See what moved before target opens.</h2>
          <p>OmniSession carries ordered messages and bounded tool history through official imports or verified native writers. Transfer report lists anything summarized or omitted.</p>
          <code className="inline-command">omni markdown &lt;session&gt; -o session.md</code>
        </div>
        <div className="fidelity-panel">
          <PanelHead label="TRANSFER / CLAUDE → CODEX" state="VERIFIED" tone="green" />
          <div className="fidelity-body">
            <CellMeter label="VISIBLE MESSAGES" value="128 / 128" lit={12} tone="amber" />
            <CellMeter label="WORKSPACE STATE" value="EXACT" lit={12} tone="cyan" />
            <CellMeter label="TOOL HISTORY" value="24 DOCUMENTARY" lit={8} tone="amber" />
            <CellMeter label="CREDENTIAL MATERIAL" value="0 COPIED" lit={0} tone="magenta" />
          </div>
          <div className="fidelity-result"><strong>019F… CREATED</strong><span>READ-BACK PASSED</span></div>
        </div>
      </section>

      <section className="support-section shell" id="support">
        <div className="section-heading">
          <div><p className="kicker kicker-cyan">COMPATIBILITY MATRIX</p><h2>Know which transfers work on this machine.</h2></div>
          <p>Each private writer shows its minimum supported version. Newer releases stay enabled unless structural validation or read-back fails. Older targets use a handoff when available or stay out of target picker.</p>
        </div>
        <div className="support-table" role="table" aria-label="Provider support">
          <div className="support-row support-header" role="row"><span role="columnheader">Agent</span><span role="columnheader">Original session</span><span role="columnheader">Cross-agent</span><span role="columnheader">Signal</span></div>
          {providers.map((provider) => (
            <div className="support-row" role="row" key={provider.id}>
              <strong role="cell"><ProviderLogo provider={provider} />{provider.name}</strong>
              <span role="cell">{provider.same}</span><span role="cell">{provider.cross}</span>
              <StatusReadout label="ROUTE" value={provider.signal} tone={provider.tone} compact />
            </div>
          ))}
        </div>
        <div className="support-legend" aria-label="Support signal legend">
          <span><i className="legend-cell cyan" />Documented interface</span>
          <span><i className="legend-cell amber" />Minimum version</span>
          <span><i className="legend-cell magenta" />Minimum version and platform</span>
        </div>
        <p className="trademark-note">Logos identify compatible tools. OmniSession is independent and not endorsed by their owners.</p>
      </section>

      <section className="safety-section shell">
        <div><p className="kicker kicker-green">SOURCE INTEGRITY</p><h2>Transfers leave source sessions unchanged.</h2><p>Native provider stores remain source of truth.</p></div>
        <div className="safety-grid">
          <article><span>01</span><strong>Transfers</strong><p>No source writes</p><StatusReadout label="MUTATIONS" value="00" tone="magenta" compact /></article>
          <article><span>02</span><strong>Historical tools</strong><p>Recorded, never replayed</p><StatusReadout label="REPLAYS" value="00" tone="magenta" compact /></article>
          <article><span>03</span><strong>Target sessions</strong><p>Read back before launch</p><StatusReadout label="VERIFY" value="09 / 09" tone="green" compact /></article>
        </div>
      </section>

      <section className="install-section" id="install">
        <div className="install shell">
          <div className="install-copy"><p className="kicker kicker-amber">LINE READY</p><h2>Install omni in one command.</h2><p>Linux and macOS binaries for x86-64 and ARM64. Local provider shims, no daemon or account.</p></div>
          <div className="install-panel">
            <PanelHead label="INSTALL / LOCAL USER" state="HTTPS" tone="cyan" />
            <div className="install-command"><span>$</span><code>{installCommand}</code><CopyInstallCommand command={installCommand} /></div>
            <StatusReadout label="INSTALL SIGNAL" value="READY" tone="amber" compact />
          </div>
        </div>
      </section>

      <footer className="footer shell">
        <a className="brand" href="#top"><Mark /><span>OmniSession</span></a>
        <p>Local session portability for coding agents.</p>
        <div><a href="https://github.com/bvolpato/omnisession">SOURCE ↗</a><a href="https://github.com/bvolpato/omnisession/blob/main/LICENSE">MIT ↗</a></div>
      </footer>
    </main>
  );
}
