import { CopyInstallCommand } from "./copy-install-command";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const installCommand = "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh";

const providers = [
  { id: "claude-code", logo: "claude-code", name: "Claude Code", same: "Resume + fork", cross: "Native session", signal: "GATED 3 / 4", lit: 3, tone: "amber" },
  { id: "codex", logo: "codex", name: "Codex", same: "Resume + fork", cross: "App-server import", signal: "GATED 3 / 4", lit: 3, tone: "amber" },
  { id: "opencode", logo: "opencode", name: "OpenCode", same: "Resume + fork", cross: "Official import", signal: "FULL 4 / 4", lit: 4, tone: "cyan" },
  { id: "grok", logo: "grok", name: "Grok", same: "Resume + fork", cross: "ACP import", signal: "FULL 4 / 4", lit: 4, tone: "cyan" },
  { id: "antigravity", logo: "antigravity", name: "Antigravity", same: "Resume", cross: "Exact Linux build", signal: "EXACT 2 / 4", lit: 2, tone: "magenta" },
  { id: "pi", logo: "pi", name: "Pi", same: "Resume + fork", cross: "v3 JSONL", signal: "GATED 3 / 4", lit: 3, tone: "amber" },
  { id: "cursor-agent", logo: "cursor", name: "Cursor Agent", same: "Resume", cross: "Exact build", signal: "EXACT 2 / 4", lit: 2, tone: "magenta" },
  { id: "cursor-ide", logo: "cursor", name: "Cursor IDE", same: "Restore chat", cross: "Exact AppImage", signal: "EXACT 2 / 4", lit: 2, tone: "magenta" },
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

function CellMeter({ label, value, lit, total = 12, tone = "amber", compact = false }: {
  label: string;
  value: string;
  lit: number;
  total?: number;
  tone?: Tone;
  compact?: boolean;
}) {
  return (
    <div className={`cell-meter meter-${tone}${compact ? " meter-compact" : ""}`} role="img" aria-label={`${label}: ${value}`}>
      <div className="meter-caption"><span>{label}</span><strong>{value}</strong></div>
      <div className="cell-strip" aria-hidden="true">
        {Array.from({ length: total }, (_, index) => <i className={index < lit ? "lit" : ""} key={index} />)}
      </div>
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

const raster = [
  "......................",
  "...cc.................",
  "....cc..........aa....",
  ".....cc.......aaa.....",
  "......cc...aaaa.......",
  ".......aaaa...........",
  "....mmm..aa...........",
  "...mm..................",
  "......................",
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
        <div className="nav-status"><span>LOCAL INDEX</span><div className="micro-cells" aria-hidden="true">{Array.from({ length: 8 }, (_, i) => <i className={i < 7 ? "lit" : ""} key={i} />)}</div><strong>READY</strong></div>
        <div className="nav-links"><a href="#workflow">Flow</a><a href="#support">Agents</a><a href="#install">Install</a><a href="https://github.com/bvolpato/omnisession">GitHub ↗</a></div>
      </nav>

      <section className="hero shell">
        <div className="hero-copy">
          <p className="kicker kicker-amber">SESSION PORTABILITY / LOCAL</p>
          <h1>Change agents.<br />Not context.</h1>
          <p className="lede">Search local coding sessions, inspect their lineage, and continue work in another installed agent.</p>
          <div className="hero-actions">
            <a className="button button-primary" href="#install">Install omni <span>↓</span></a>
            <a className="button button-quiet" href="https://github.com/bvolpato/omnisession">View source ↗</a>
          </div>
          <CellMeter label="SUPPORTED AGENTS" value="08 / 08" lit={8} total={8} tone="cyan" />
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
              <div className="route-cells" aria-label="Transfer route connected">{Array.from({ length: 9 }, (_, i) => <i className={i < 8 ? "lit" : ""} key={i} />)}</div>
              <div className="agent-node active"><ProviderLogo provider={providers[0]} /><span><small>TARGET</small>claude</span></div>
            </div>
            <div className="console-meters">
              <CellMeter label="VISIBLE HISTORY" value="128 / 128" lit={12} tone="amber" compact />
              <CellMeter label="WORKSPACE MATCH" value="100%" lit={12} tone="cyan" compact />
              <CellMeter label="SECRETS COPIED" value="00" lit={0} tone="magenta" compact />
            </div>
          </div>
          <div className="console-command"><span>$</span><code>omni resume d8f7c1a4-2e9b-4c36-a5f1-7b0d2e8c9a44 --in claude</code><i aria-hidden="true" /></div>
        </div>

        <div className="hero-side-readout">
          <span>LINE / 01</span><strong>READ → MAP → VERIFY → OPEN</strong><small>No daemon. Source store remains untouched.</small>
        </div>
      </section>

      <section className="agent-band" aria-label="Supported agents">
        <div className="shell agent-list">
          {providers.map((provider, index) => (
            <span className="agent-chip" key={provider.id}><ProviderLogo provider={provider} /><span>{provider.name}</span><span className={`chip-cell ${index < 4 ? "cyan" : "amber"}`} /></span>
          ))}
        </div>
      </section>

      <section className="browser-section shell" aria-labelledby="browser-title">
        <div className="section-heading">
          <div><p className="kicker kicker-cyan">LOCAL SESSION INDEX</p><h2 id="browser-title">Every trajectory on one instrument.</h2></div>
          <p>Filter title, full conversation text, ID, directory, branch, or agent. Search results surface matching context; selection shows model, reasoning, tokens, and lineage when recorded.</p>
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
            <CellMeter label="INDEX LOAD" value="08 / 08" lit={8} total={8} tone="cyan" />
            <CellMeter label="INDEX STATE" value="WARM" lit={8} tone="amber" />
            <CellMeter label="SELECTION DRIFT" value="00" lit={0} tone="magenta" />
            <p>Warm index renders first. Provider refresh continues without replacing current result set.</p>
          </aside>
        </div>
      </section>

      <section className="workflow-section shell" id="workflow">
        <div className="section-heading narrow">
          <div><p className="kicker kicker-amber">ROUTE SEQUENCE</p><h2>One command. Three readings.</h2></div>
        </div>
        <div className="flow-panel">
          <article><span className="flow-number">01</span><div><h3>Call the index</h3><p>Run <code>omni</code>. Current workspace appears first.</p></div><CellMeter label="INDEX" value="READY" lit={4} total={4} tone="cyan" compact /></article>
          <article><span className="flow-number">02</span><div><h3>Tune the session</h3><p>Type any title, message, directory, branch, ID, or agent.</p></div><CellMeter label="MATCH" value="PRECISE" lit={4} total={4} tone="amber" compact /></article>
          <article><span className="flow-number">03</span><div><h3>Route the context</h3><p>Resume, fork, or continue in another installed agent.</p></div><CellMeter label="READ-BACK" value="PASS" lit={4} total={4} tone="green" compact /></article>
        </div>
      </section>

      <section className="transfer-section shell">
        <div className="transfer-copy">
          <p className="kicker kicker-magenta">FIDELITY READOUT</p>
          <h2>Context moves with receipts.</h2>
          <p>OmniSession carries ordered messages and bounded tool history through official imports or version-gated native writers. Every transfer reports loss before launch.</p>
          <code className="inline-command">omni markdown &lt;session&gt; -o session.md</code>
        </div>
        <div className="fidelity-panel">
          <PanelHead label="TRANSFER / CLAUDE → CODEX" state="VERIFIED" tone="green" />
          <div className="fidelity-body">
            <CellMeter label="VISIBLE MESSAGES" value="128 / 128" lit={16} total={16} tone="amber" />
            <CellMeter label="WORKSPACE STATE" value="EXACT" lit={16} total={16} tone="cyan" />
            <CellMeter label="TOOL HISTORY" value="24 DOCUMENTARY" lit={11} total={16} tone="amber" />
            <CellMeter label="CREDENTIAL MATERIAL" value="0 COPIED" lit={0} total={16} tone="magenta" />
          </div>
          <div className="fidelity-result"><strong>019F… CREATED</strong><span>READ-BACK PASSED</span></div>
        </div>
      </section>

      <section className="support-section shell" id="support">
        <div className="section-heading">
          <div><p className="kicker kicker-cyan">COMPATIBILITY MATRIX</p><h2>Installed agents, measured honestly.</h2></div>
          <p>Private formats are version-gated. Unsupported targets disappear or fall back instead of guessing.</p>
        </div>
        <div className="support-table" role="table" aria-label="Provider support">
          <div className="support-row support-header" role="row"><span role="columnheader">Agent</span><span role="columnheader">Original session</span><span role="columnheader">Cross-agent</span><span role="columnheader">Signal</span></div>
          {providers.map((provider) => (
            <div className="support-row" role="row" key={provider.id}>
              <strong role="cell"><ProviderLogo provider={provider} />{provider.name}</strong>
              <span role="cell">{provider.same}</span><span role="cell">{provider.cross}</span>
              <CellMeter label="ROUTE" value={provider.signal} lit={provider.lit} total={4} tone={provider.tone} compact />
            </div>
          ))}
        </div>
        <div className="support-legend" aria-label="Support signal legend">
          <span><i className="legend-cell cyan" />4 / 4 documented interface</span>
          <span><i className="legend-cell amber" />3 / 4 version-gated</span>
          <span><i className="legend-cell magenta" />2 / 4 exact build or platform</span>
        </div>
        <p className="trademark-note">Logos identify compatible tools. OmniSession is independent and not endorsed by their owners.</p>
      </section>

      <section className="safety-section shell">
        <div><p className="kicker kicker-green">SOURCE INTEGRITY</p><h2>Read local. Write new.</h2><p>Native provider stores remain the source of truth.</p></div>
        <div className="safety-grid">
          <article><span>01</span><strong>Source sessions</strong><p>Always read-only</p><CellMeter label="MUTATIONS" value="00" lit={0} total={6} tone="magenta" compact /></article>
          <article><span>02</span><strong>Historical tools</strong><p>Recorded, never replayed</p><CellMeter label="REPLAYS" value="00" lit={0} total={6} tone="magenta" compact /></article>
          <article><span>03</span><strong>Target sessions</strong><p>Read back before launch</p><CellMeter label="VERIFY" value="06 / 06" lit={6} total={6} tone="green" compact /></article>
        </div>
      </section>

      <section className="install-section" id="install">
        <div className="install shell">
          <div className="install-copy"><p className="kicker kicker-amber">LINE READY</p><h2>Install once. Pick up anywhere.</h2><p>Local binary and optional provider shims. No daemon. No account.</p></div>
          <div className="install-panel">
            <PanelHead label="INSTALL / LOCAL USER" state="HTTPS" tone="cyan" />
            <div className="install-command"><span>$</span><code>{installCommand}</code><CopyInstallCommand command={installCommand} /></div>
            <CellMeter label="INSTALL SIGNAL" value="READY" lit={12} total={12} tone="amber" compact />
          </div>
        </div>
      </section>

      <footer className="footer shell">
        <a className="brand" href="#top"><Mark /><span>OmniSession</span></a>
        <p>Switch agents. Keep the thread.</p>
        <div><a href="https://github.com/bvolpato/omnisession">SOURCE ↗</a><a href="https://github.com/bvolpato/omnisession/blob/main/LICENSE">MIT ↗</a></div>
      </footer>
    </main>
  );
}
