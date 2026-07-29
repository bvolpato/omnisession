const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

const providers = [
  {
    id: "claude-code",
    logo: "claude-code",
    name: "Claude Code",
    same: "Read + resume",
    cross: "Native trajectory target",
  },
  {
    id: "codex",
    logo: "codex",
    name: "Codex",
    same: "Read + resume",
    cross: "Native trajectory target",
  },
  {
    id: "opencode",
    logo: "opencode",
    name: "OpenCode",
    same: "Read + resume",
    cross: "Native trajectory target",
  },
  {
    id: "grok",
    logo: "grok",
    name: "Grok",
    same: "Read + resume",
    cross: "Native trajectory target",
  },
  {
    id: "antigravity",
    logo: "antigravity",
    name: "Antigravity",
    same: "Read + resume",
    cross: "Linux exact-version target",
  },
  {
    id: "pi",
    logo: "pi",
    name: "Pi",
    same: "Read + resume + fork",
    cross: "v3 JSONL native target",
  },
  {
    id: "cursor-agent",
    logo: "cursor",
    name: "Cursor Agent",
    same: "Read + resume",
    cross: "Native trajectory target",
  },
  {
    id: "cursor-ide",
    logo: "cursor",
    name: "Cursor IDE",
    same: "Read conversations + tools",
    cross: "Exact-build private target",
  },
] as const;

type Provider = (typeof providers)[number];

const codex = providers[1];
const opencode = providers[2];

const steps = [
  {
    number: "01",
    title: "Open the picker",
    body: "Start with this workspace. Search titles, trajectories, full IDs, branches, providers, or directory paths.",
    command: "omnis",
  },
  {
    number: "02",
    title: "Choose the source",
    body: "Related sessions stay grouped as a root-to-leaf tree. Continued branches use their first post-handoff prompt as the title.",
    command: "type to search · Tab for every workspace",
  },
  {
    number: "03",
    title: "Choose the target",
    body: "Pick another agent, continue the original session, or fork it into a new session.",
    command: "↑ / ↓ choose · Enter open",
  },
];

function Mark() {
  return (
    <span className="mark" aria-hidden="true">
      <span />
      <span />
      <span />
      <span />
    </span>
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
          <a href="#workflow">Workflow</a>
          <a href="#support">Support</a>
          <a href="#safety">Safety</a>
          <a className="nav-github" href="https://github.com/bvolpato/omnisession">
            GitHub <Arrow />
          </a>
        </div>
      </nav>

      <section className="hero shell">
        <div className="hero-copy">
          <p className="eyebrow">
            <span className="status-dot" /> Open source · MIT · v0.8.25
          </p>
          <h1>Pick up the same coding session in another agent.</h1>
          <p className="lede">
            OmniSession finds local sessions, checks what the next agent can accept,
            and resumes the work without changing the source history.
          </p>
          <div className="hero-actions">
            <a className="button button-primary" href="#install">
              Install OmniSession <span aria-hidden="true">↓</span>
            </a>
            <a className="button button-secondary" href="https://github.com/bvolpato/omnisession">
              Read the source <Arrow />
            </a>
          </div>
          <p className="hero-note">Runs locally. No account or background service.</p>
        </div>

        <div className="route-card" aria-label="Example session transfer report">
          <div className="route-titlebar">
            <span>transfer.plan</span>
            <span className="route-ready"><i /> ready</span>
          </div>
          <div className="route-endpoint">
            <span className="route-label">source</span>
            <div className="agent-badge"><ProviderLogo provider={codex} /></div>
            <div>
              <strong>Codex</strong>
              <code>019f42ab...7c21</code>
            </div>
          </div>
          <div className="route-connector">
            <span />
            <small>official import</small>
            <span />
          </div>
          <div className="route-endpoint">
            <span className="route-label">target</span>
            <div className="agent-badge agent-badge-target"><ProviderLogo provider={opencode} /></div>
            <div>
              <strong>OpenCode</strong>
              <code>new native session</code>
            </div>
          </div>
          <dl className="fidelity">
            <div><dt>Visible messages</dt><dd>preserved</dd></div>
            <div><dt>Tool calls</dt><dd>documentary</dd></div>
            <div><dt>Credentials</dt><dd>excluded</dd></div>
            <div><dt>Read-back</dt><dd className="passed">passed</dd></div>
          </dl>
          <div className="route-command">
            <span>$</span> omnis
          </div>
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
          <p className="kicker">Workflow</p>
          <h2>Pick the session you meant.</h2>
          <p>
            Current workspace comes first. Search metadata and redacted trajectory content
            already read by OmniSession, widen to every workspace when needed, then choose.
          </p>
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
          <p className="kicker">Native targets</p>
          <h2>Verified targets receive model-visible history.</h2>
          <p>
            OmniSession imports ordered user, assistant, and bounded tool history only through
            accepted provider interfaces or exact-build writers. Every target is read back before
            launch or materialize-only completion. Failed imports remove only created records.
          </p>
          <p className="native-caveat">
            Manual handoff: <code>omnis markdown &lt;id&gt; -o session.md</code>
          </p>
        </div>
        <div className="report-card" aria-label="Example fidelity report">
          <div className="report-heading">
            <span>Fidelity report</span>
            <span>claude → codex</span>
          </div>
          <div className="report-row"><span>Messages</span><strong>128 imported</strong></div>
          <div className="report-row"><span>Repository</span><strong>exact match</strong></div>
          <div className="report-row"><span>Tool history</span><strong>24 documentary</strong></div>
          <div className="report-row"><span>Permission grants</span><strong>reset</strong></div>
          <div className="report-row"><span>Verification</span><strong className="report-good">passed</strong></div>
        </div>
      </section>

      <section className="section shell" id="support">
        <div className="section-intro compact">
          <p className="kicker">Provider support</p>
          <h2>Current read, resume, and target support.</h2>
          <p>Source stores stay read-only. Private target writers fail closed after provider updates.</p>
        </div>
        <div className="support-table" role="table" aria-label="Provider support">
          <div className="support-row support-header" role="row">
            <span role="columnheader">Provider</span>
            <span role="columnheader">Same provider</span>
            <span role="columnheader">Cross-provider</span>
          </div>
          {providers.map((provider) => (
            <div className="support-row" role="row" key={provider.id}>
              <strong className="support-provider" role="cell">
                <ProviderLogo provider={provider} />
                <span>{provider.name}</span>
              </strong>
              <span role="cell">{provider.same}</span>
              <span role="cell">{provider.cross}</span>
            </div>
          ))}
        </div>
        <p className="trademark-note">
          Antigravity targets require its exact Linux build. Cursor IDE targets require its exact verified AppImage. Pi writes documented v3 JSONL.
        </p>
        <p className="trademark-note">
          Logos identify compatible tools. OmniSession is independent and not endorsed by their owners.
        </p>
      </section>

      <section className="safety shell" id="safety">
        <div>
          <p className="kicker">Safety</p>
          <h2>Source sessions stay untouched.</h2>
        </div>
        <div className="safety-list">
          <article>
            <span>01</span>
            <h3>Read-only source stores</h3>
            <p>OmniSession reads provider history but never edits the original session.</p>
          </article>
          <article>
            <span>02</span>
            <h3>No command replay</h3>
            <p>Imported commands stay documentary. Approvals and credentials stay out.</p>
          </article>
          <article>
            <span>03</span>
            <h3>Exact routing</h3>
            <p>Picker selection or an exact session ID decides what to resume. Recency does not.</p>
          </article>
        </div>
      </section>

      <section className="install shell" id="install">
        <div className="install-copy">
          <p className="kicker">Install</p>
          <h2>Install CLI and agent shims.</h2>
          <p>The installer checks the downloaded release before placing anything on your PATH.</p>
        </div>
        <div className="terminal">
          <div className="terminal-bar">
            <span>terminal</span>
            <small>~/project</small>
          </div>
          <div className="terminal-body">
            <p><span>$</span> curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh</p>
            <p><span>$</span> omnis<i /></p>
            <p className="terminal-output">Choose a session · choose an agent · continue</p>
          </div>
        </div>
      </section>

      <footer className="footer shell">
        <a className="brand" href="#top">
          <Mark />
          <span>OmniSession</span>
        </a>
        <p>Continue coding sessions across agents and IDEs.</p>
        <div>
          <a href="https://github.com/bvolpato/omnisession">GitHub</a>
          <span>MIT</span>
        </div>
      </footer>
    </main>
  );
}
