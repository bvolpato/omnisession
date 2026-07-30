const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

const providers = [
  { id: "claude-code", logo: "claude-code", name: "Claude Code", same: "Resume + fork", cross: "Native session" },
  { id: "codex", logo: "codex", name: "Codex", same: "Resume + fork", cross: "App-server import" },
  { id: "opencode", logo: "opencode", name: "OpenCode", same: "Resume + fork", cross: "Official import" },
  { id: "grok", logo: "grok", name: "Grok", same: "Resume + fork", cross: "ACP import" },
  { id: "antigravity", logo: "antigravity", name: "Antigravity", same: "Resume", cross: "Exact Linux build" },
  { id: "pi", logo: "pi", name: "Pi", same: "Resume + fork", cross: "v3 JSONL" },
  { id: "cursor-agent", logo: "cursor", name: "Cursor Agent", same: "Resume", cross: "Exact build" },
  { id: "cursor-ide", logo: "cursor", name: "Cursor IDE", same: "Restore chat", cross: "Exact AppImage" },
] as const;

type Provider = (typeof providers)[number];

const codex = providers[1];
const claude = providers[0];
const opencode = providers[2];

const steps = [
  {
    number: "01",
    title: "Run omni",
    body: "Current workspace loads first. Press Tab when you need sessions from every workspace.",
    command: "omni",
  },
  {
    number: "02",
    title: "Filter",
    body: "Search title, conversation text, ID, directory, branch, or provider without losing selection.",
    command: "type to search",
  },
  {
    number: "03",
    title: "Choose an agent",
    body: "Continue original session, fork it, or open a verified continuation in another installed agent.",
    command: "↑ / ↓ choose · Enter open",
  },
];

function Mark() {
  return (
    <img
      className="brand-logo"
      src={`${basePath}/logo.svg`}
      width="32"
      height="32"
      alt=""
    />
  );
}

function ProviderLogo({ provider }: { provider: Provider }) {
  const className = [
    "provider-logo",
    provider.logo === "codex" ? "provider-logo-color" : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <img
      aria-hidden="true"
      className={className}
      src={`${basePath}/providers/${provider.logo}.svg`}
      alt=""
      width="24"
      height="24"
    />
  );
}

function Arrow() {
  return <span aria-hidden="true">↗</span>;
}

export default function Home() {
  return (
    <main id="top">
      <nav className="nav shell" aria-label="Primary navigation">
        <a className="brand" href="#top" aria-label="OmniSession home">
          <Mark />
          <span>OmniSession</span>
        </a>
        <div className="nav-links">
          <a href="#workflow">How it works</a>
          <a href="#support">Agents</a>
          <a href="#safety">Safety</a>
          <a className="nav-github" href="https://github.com/bvolpato/omnisession">
            GitHub <Arrow />
          </a>
        </div>
      </nav>

      <section className="hero shell">
        <div className="hero-copy">
          <p className="eyebrow">
            <span className="status-dot" /> Local · open source · MIT
          </p>
          <h1>Find a session. Open it in another agent.</h1>
          <p className="lede">
            Run <code>omni</code>, filter local coding sessions, and choose where work should continue.
          </p>
          <div className="hero-actions">
            <a className="button button-primary" href="#install">
              Install <span aria-hidden="true">↓</span>
            </a>
            <a className="button button-secondary" href="https://github.com/bvolpato/omnisession">
              GitHub <Arrow />
            </a>
          </div>
          <div className="hero-commands" aria-label="Primary OmniSession commands">
            <code><span>$</span> omni</code>
            <code><span>$</span> omni resume &lt;session&gt; --in codex</code>
          </div>
        </div>

        <div className="browser-card" aria-label="OmniSession session picker preview">
          <div className="browser-titlebar">
            <span>OmniSession · session browser</span>
            <span className="browser-ready"><i /> index ready</span>
          </div>
          <div className="browser-toolbar">
            <span>⌕</span>
            <strong>auth refresh</strong>
            <small>all workspaces</small>
          </div>
          <div className="browser-columns">
            <span>agent</span><span>session</span><span>workspace</span><span>age</span>
          </div>
          <div className="session-list">
            <div className="session-row">
              <span className="session-agent"><ProviderLogo provider={codex} /> codex</span>
              <strong>fix refresh token race</strong>
              <span>payments</span><small>12m</small>
            </div>
            <div className="session-row session-row-selected">
              <span className="session-agent"><ProviderLogo provider={claude} /> claude</span>
              <strong>add concurrent refresh tests</strong>
              <span>payments</span><small>18m</small>
            </div>
            <div className="session-row session-row-child">
              <span className="session-agent"><ProviderLogo provider={opencode} /> opencode</span>
              <strong>review retry behavior</strong>
              <span>payments</span><small>24m</small>
            </div>
          </div>
          <div className="browser-detail">
            <div>
              <span className="browser-detail-label">selected session</span>
              <strong>add concurrent refresh tests</strong>
              <code>~/src/payments · auth-refresh · main</code>
            </div>
            <div className="target-choices">
              <span>open in</span>
              <strong>Codex</strong>
              <strong>OpenCode</strong>
              <strong>Cursor</strong>
            </div>
          </div>
          <div className="route-command"><span>$</span> omni</div>
        </div>
      </section>

      <section className="provider-bar" aria-label="Supported providers">
        <div className="shell provider-list">
          <span className="provider-label">Works with</span>
          {providers.map((provider) => (
            <span className="provider-item" key={provider.id}>
              <ProviderLogo provider={provider} />
              <span>{provider.name}</span>
            </span>
          ))}
        </div>
      </section>

      <section className="section shell" id="workflow">
        <div className="section-intro">
          <p className="kicker">How it works</p>
          <h2>One picker for local agent sessions.</h2>
          <p>OmniSession opens from its local index, refreshes providers in background, and keeps related sessions together.</p>
        </div>
        <div className="steps">
          {steps.map((step) => (
            <article className="step" key={step.number}>
              <span className="step-number">{step.number}</span>
              <h3>{step.title}</h3>
              <p>{step.body}</p>
              <code>{step.command}</code>
            </article>
          ))}
        </div>
      </section>

      <section className="native-section shell">
        <div className="native-copy">
          <p className="kicker">Transfer</p>
          <h2>Target receives visible session history.</h2>
          <p>OmniSession moves ordered user and assistant messages plus bounded tool activity through provider imports or verified native writers. It reads target session back before launch.</p>
          <p className="native-caveat"><code>omni markdown &lt;session&gt; -o session.md</code></p>
        </div>
        <div className="report-card" aria-label="Example fidelity report">
          <div className="report-heading"><span>Transfer report</span><span>claude → codex</span></div>
          <div className="report-row"><span>Messages</span><strong>128 preserved</strong></div>
          <div className="report-row"><span>Workspace</span><strong>exact match</strong></div>
          <div className="report-row"><span>Tool activity</span><strong>24 documentary</strong></div>
          <div className="report-row"><span>Credentials</span><strong>excluded</strong></div>
          <div className="report-row"><span>Read-back</span><strong className="report-good">passed</strong></div>
        </div>
      </section>

      <section className="section shell" id="support">
        <div className="section-intro compact">
          <p className="kicker">Agents</p>
          <h2>Current local support.</h2>
          <p>Picker shows runnable agents found on this machine. Private writers fail closed when provider builds change.</p>
        </div>
        <div className="support-table" role="table" aria-label="Provider support">
          <div className="support-row support-header" role="row">
            <span role="columnheader">Provider</span><span role="columnheader">Original session</span><span role="columnheader">Cross-provider</span>
          </div>
          {providers.map((provider) => (
            <div className="support-row" role="row" key={provider.id}>
              <strong className="support-provider" role="cell"><ProviderLogo provider={provider} /><span>{provider.name}</span></strong>
              <span role="cell">{provider.same}</span><span role="cell">{provider.cross}</span>
            </div>
          ))}
        </div>
        <p className="trademark-note">Logos identify compatible tools. OmniSession is independent and not endorsed by their owners.</p>
      </section>

      <section className="safety shell" id="safety">
        <div><p className="kicker">Safety</p><h2>Source sessions stay untouched.</h2></div>
        <div className="safety-list">
          <article><span>01</span><h3>Read-only sources</h3><p>Original provider history is never edited.</p></article>
          <article><span>02</span><h3>No command replay</h3><p>Tools stay documentary. Approvals and credentials stay out.</p></article>
          <article><span>03</span><h3>Exact selection</h3><p>Picker choice or session ID controls routing. Recency does not.</p></article>
        </div>
      </section>

      <section className="install shell" id="install">
        <div className="install-copy">
          <p className="kicker">Install</p>
          <h2>Install omni and its shims.</h2>
          <p>Installer verifies release checksum before changing PATH.</p>
        </div>
        <div className="terminal">
          <div className="terminal-bar"><span>terminal</span><small>~/project</small></div>
          <div className="terminal-body">
            <p><span>$</span> curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh</p>
            <p><span>$</span> omni<i /></p>
            <p className="terminal-output">filter session · choose agent · continue</p>
          </div>
        </div>
      </section>

      <footer className="footer shell">
        <a className="brand" href="#top"><Mark /><span>OmniSession</span></a>
        <p>Local session portability for coding agents.</p>
        <div><a href="https://github.com/bvolpato/omnisession">GitHub</a><span>MIT</span></div>
      </footer>
    </main>
  );
}
