import { InstallCommand } from "./install-command";
import { providers } from "./providers.generated";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const compatibilityUrl = "https://github.com/bvolpato/omnisession/blob/main/docs/COMPATIBILITY.md";
const releasesUrl = "https://github.com/bvolpato/omnisession/releases";

type Provider = (typeof providers)[number];
type Tone = "amber" | "cyan" | "magenta" | "green";

function Mark() {
  return <img className="brand-logo" src={`${basePath}/logo.svg`} width="34" height="34" alt="" />;
}

function ProviderLogo({ provider }: { provider: Provider }) {
  const className = provider.logo === "codex" ? "provider-logo provider-logo-color" : "provider-logo";
  return <img aria-hidden="true" className={className} src={`${basePath}/providers/${provider.logo}.svg`} alt="" width="24" height="24" />;
}

function PanelHead({ label, state, tone = "cyan" }: { label: string; state?: string; tone?: Tone }) {
  return (
    <div className="panel-head">
      <span>{label}</span>
      {state ? <strong className={`state state-${tone}`}><i />{state}</strong> : null}
    </div>
  );
}

function Signal({ label, value, tone = "cyan" }: { label: string; value: string; tone?: Tone }) {
  return (
    <div className={`signal signal-${tone}`}>
      <span>{label}</span>
      <strong><i aria-hidden="true" />{value}</strong>
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
  ...Array.from({ length: 7 }, (_, rowIndex) => `.${rasterGlyphs.map(({ tone, rows }) => rows[rowIndex].replaceAll("1", tone).replaceAll("0", ".")).join(".")}.`),
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

function RouteLine() {
  return (
    <div className="route-line" aria-label="Session route connected">
      {Array.from({ length: 11 }, (_, index) => <i style={{ animationDelay: `${index * 210}ms` }} key={index} />)}
    </div>
  );
}

function FidelityBar({ label, value, fill, tone }: { label: string; value: string; fill: number; tone: Tone }) {
  return (
    <div className={`fidelity-row fidelity-${tone}`}>
      <div><span>{label}</span><strong>{value}</strong></div>
      <div className="fidelity-cells" aria-hidden="true">
        {Array.from({ length: 12 }, (_, index) => <i className={index < fill ? "lit" : ""} key={index} />)}
      </div>
    </div>
  );
}

export default function Home() {
  return (
    <main id="top">
      <nav className="nav shell" aria-label="Primary navigation">
        <a className="brand" href="#top" aria-label="OmniSession home"><Mark /><span>OmniSession</span></a>
        <div className="nav-links">
          <a href="#product">Product</a>
          <a href="#support">Agents</a>
          <a href="#install">Install</a>
          <a href={compatibilityUrl}>Compatibility</a>
          <a href={releasesUrl}>Releases</a>
          <a className="github-link" href="https://github.com/bvolpato/omnisession">GitHub <span>↗</span></a>
        </div>
      </nav>

      <section className="hero shell">
        <div className="hero-copy">
          <p className="eyebrow"><i />One local browser. Nine coding agents.</p>
          <h1>Continue the work.<br /><span>Change the agent.</span></h1>
          <p className="lede">OmniSession finds local coding sessions, preserves their trajectory, and opens the continuation in another installed agent.</p>
          <div className="hero-actions">
            <a className="button button-primary" href="#install">Install omni <span>↓</span></a>
            <a className="text-link" href="#product">See it work <span>↘</span></a>
          </div>
          <div className="hero-proof" aria-label="Product properties">
            <span><i />Local only</span>
            <span><i />Source untouched</span>
            <span><i />MIT licensed</span>
          </div>
        </div>

        <div className="hero-stage">
          <div className="stage-glow" aria-hidden="true" />
          <div className="hero-console">
            <PanelHead label="OMNI / CONTINUITY READOUT" state="ROUTE READY" tone="amber" />
            <div className="console-body">
              <div className="session-reading">
                <span>SELECTED SESSION</span>
                <strong>fix refresh token race</strong>
                <code>~/src/payments · auth-refresh · main</code>
              </div>
              <SignalRaster />
              <div className="route-reading">
                <div className="agent-node"><ProviderLogo provider={providers[1]} /><span><small>SOURCE</small>codex</span></div>
                <RouteLine />
                <div className="agent-node active"><ProviderLogo provider={providers[0]} /><span><small>TARGET</small>claude</span></div>
              </div>
              <div className="console-signals">
                <Signal label="HISTORY" value="128 / 128" tone="amber" />
                <Signal label="WORKSPACE" value="MATCH" tone="cyan" />
                <Signal label="READ-BACK" value="PASSED" tone="green" />
              </div>
            </div>
            <div className="console-command"><span>$</span><code>omni resume d8f7c1a4… --in claude</code><i aria-hidden="true" /></div>
          </div>
          <div className="stage-caption"><span>READ</span><i /> <span>MAP</span><i /> <span>VERIFY</span><i /> <strong>OPEN</strong></div>
        </div>
      </section>

      <section className="agent-band" aria-label="Supported agents">
        <div className="shell agent-list">
          <span className="agent-label">WORKS WITH</span>
          {providers.map((provider) => <span className="agent-chip" key={provider.id}><ProviderLogo provider={provider} /><span>{provider.name}</span></span>)}
        </div>
      </section>

      <section className="product-section shell" id="product" aria-labelledby="product-title">
        <div className="section-intro">
          <p className="kicker kicker-cyan">THE SESSION BROWSER</p>
          <h2 id="product-title">All your trajectories.<br />One fast index.</h2>
          <p>Search messages, tool results, directories, branches, IDs, and agents. Match snippets explain every result. Session details show enough context to pick confidently.</p>
        </div>
        <figure className="screenshot-panel">
          <PanelHead label="OMNI / SESSION BROWSER" state="INDEX WARM" tone="cyan" />
          <div className="screenshot-command" aria-label="Run omni to open session browser">
            <code><span>$</span> omni</code>
            <small>FILTER · INSPECT · CONTINUE</small>
          </div>
          <div className="screenshot-frame">
            <img src={`${basePath}/session-browser.png`} width="1564" height="620" loading="lazy" alt="OmniSession terminal picker with a cross-agent session tree and conversation preview" />
          </div>
          <figcaption>
            <span><i className="cyan" />Full-text search</span>
            <span><i className="amber" />Warm local index</span>
            <span><i className="magenta" />Session lineage</span>
          </figcaption>
        </figure>
      </section>

      <section className="workflow-section shell" id="workflow">
        <div className="section-intro compact">
          <p className="kicker kicker-amber">THREE MOVES</p>
          <h2>Find it. Route it. Keep going.</h2>
        </div>
        <div className="steps">
          <article><span>01</span><h3>Run <code>omni</code></h3><p>Current workspace sessions appear first. Switch to all workspaces whenever needed.</p></article>
          <article><span>02</span><h3>Pick a trajectory</h3><p>Search bounded, redacted visible history. Inspect branch, model, tokens, and lineage.</p></article>
          <article><span>03</span><h3>Choose an agent</h3><p>Resume or fork. OmniSession uses strongest verified transfer path available.</p></article>
        </div>
      </section>

      <section className="fidelity-section shell">
        <div className="fidelity-copy">
          <p className="kicker kicker-magenta">TRANSFER FIDELITY</p>
          <h2>Know what crossed over.</h2>
          <p>Official imports come first. Native writers are version-gated and read back before launch. Transfer report names anything summarized, historical-only, or omitted.</p>
          <ul>
            <li><i className="cyan" />Ordered conversation stays model-visible</li>
            <li><i className="amber" />Tool outcomes stay documentary, never replayed</li>
            <li><i className="magenta" />Credentials and hidden reasoning stay out</li>
          </ul>
          <a className="text-link" href={compatibilityUrl}>Read compatibility notes <span>↗</span></a>
        </div>
        <div className="fidelity-panel">
          <PanelHead label="TRANSFER / CLAUDE → CODEX" state="VERIFIED" tone="green" />
          <div className="fidelity-body">
            <div className="fidelity-summary"><span>VISIBLE TRAJECTORY</span><strong>100%</strong><small>native materialization</small></div>
            <FidelityBar label="MESSAGES" value="128 / 128" fill={12} tone="amber" />
            <FidelityBar label="WORKSPACE STATE" value="EXACT" fill={12} tone="cyan" />
            <FidelityBar label="TOOL OUTCOMES" value="24 / 24" fill={12} tone="amber" />
            <FidelityBar label="SECRETS" value="0 COPIED" fill={0} tone="magenta" />
          </div>
          <div className="fidelity-result"><strong>019F… CREATED</strong><span><i />READ-BACK PASSED</span></div>
        </div>
      </section>

      <section className="support-section shell" id="support">
        <div className="section-intro support-intro">
          <p className="kicker kicker-cyan">COMPATIBILITY</p>
          <h2>Nine agents. One honest signal.</h2>
          <p>Minimum versions mark private writers. Documented interfaces use official signal. Platform-specific routes remain clearly marked.</p>
        </div>
        <div className="support-table" role="table" aria-label="Provider support">
          <div className="support-row support-header" role="row"><span role="columnheader">Agent</span><span role="columnheader">Same agent</span><span role="columnheader">Cross-agent</span><span role="columnheader">Support</span></div>
          {providers.map((provider) => (
            <div className="support-row" role="row" key={provider.id}>
              <strong role="cell"><ProviderLogo provider={provider} />{provider.name}</strong>
              <span role="cell">{provider.same}</span>
              <span role="cell">{provider.cross}</span>
              <span className={`support-signal signal-${provider.tone}`} role="cell"><i />{provider.signal}</span>
            </div>
          ))}
        </div>
        <div className="support-legend" aria-label="Support signal legend">
          <span><i className="cyan" />Documented interface</span>
          <span><i className="amber" />Minimum version</span>
          <span><i className="magenta" />Version + platform</span>
        </div>
        <p className="trademark-note">Logos identify compatible tools. OmniSession is independent and not endorsed by their owners.</p>
      </section>

      <section className="integrity-section shell">
        <div className="integrity-lead">
          <p className="kicker kicker-green">SOURCE INTEGRITY</p>
          <h2>Your originals stay original.</h2>
        </div>
        <div className="integrity-points">
          <article><strong>Source stores</strong><p>Read-only during transfer.</p><span>00 MUTATIONS</span></article>
          <article><strong>Historical tools</strong><p>Recorded, never replayed.</p><span>00 REPLAYS</span></article>
          <article><strong>Target session</strong><p>Read back before launch.</p><span className="pass">VERIFIED</span></article>
        </div>
      </section>

      <section className="install-section" id="install">
        <div className="install shell">
          <div className="install-copy">
            <p className="kicker kicker-amber">READY WHEN YOU ARE</p>
            <h2>Switch agents.<br />Keep the thread.</h2>
            <p>Linux and macOS remain default. Native Windows x86-64 is available as a preview. WSL stays a separate Linux environment.</p>
          </div>
          <div className="install-panel">
            <PanelHead label="INSTALL / LOCAL USER" state="HTTPS" tone="cyan" />
            <InstallCommand />
            <div className="install-meta"><span>ONE COMMAND</span><span>MIT LICENSE</span><span>LOCAL FIRST</span></div>
            <div className="install-links">
              <a className="text-link" href={compatibilityUrl}>Compatibility notes <span>↗</span></a>
              <a className="text-link" href={releasesUrl}>Browse releases <span>↗</span></a>
            </div>
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
