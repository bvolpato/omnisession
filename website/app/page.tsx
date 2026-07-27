const providers = ["Claude Code", "Codex", "OpenCode", "Grok", "Cursor"];

const steps = [
  {
    number: "01",
    title: "Find the session",
    body: "Search every installed agent by exact ID or list sessions for the current repository.",
    command: "omnis list --project .",
  },
  {
    number: "02",
    title: "Check the transfer",
    body: "See which history, tools, and workspace details the target can accept before anything runs.",
    command: "omnis resume <id> --in opencode --dry-run",
  },
  {
    number: "03",
    title: "Resume in the target",
    body: "Create a verified native session when the target supports imports. Otherwise, start with a redacted handoff.",
    command: "omnis resume <id> --in opencode",
  },
];

const support = [
  ["Claude Code", "Read + resume", "OpenCode import"],
  ["Codex", "Read + resume", "OpenCode import"],
  ["OpenCode", "Read + resume", "Native import"],
  ["Grok", "Read + resume", "OpenCode import"],
  ["Cursor", "Metadata + resume", "Handoff"],
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
            <span className="status-dot" /> Open source · MIT · v0.3.0
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
            <div className="agent-badge">CX</div>
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
            <div className="agent-badge agent-badge-target">OC</div>
            <div>
              <strong>OpenCode</strong>
              <code>new native session</code>
            </div>
          </div>
          <dl className="fidelity">
            <div><dt>Visible messages</dt><dd>preserved</dd></div>
            <div><dt>Tool calls</dt><dd>history only</dd></div>
            <div><dt>Credentials</dt><dd>excluded</dd></div>
            <div><dt>Read-back</dt><dd className="passed">passed</dd></div>
          </dl>
          <div className="route-command">
            <span>$</span> omnis resume 019f42ab... --in opencode
          </div>
        </div>
      </section>

      <section className="provider-bar" aria-label="Supported providers">
        <div className="shell provider-list">
          <span className="provider-label">Works with</span>
          {providers.map((provider) => <span key={provider}>{provider}</span>)}
        </div>
      </section>

      <section className="section shell" id="workflow">
        <div className="section-intro">
          <p className="kicker">Workflow</p>
          <h2>Use an ID you already have.</h2>
          <p>
            Provider prefixes are optional when an ID is unique. OmniSession fails
            clearly when it cannot find a session or finds the same ID twice.
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
          <p className="kicker">Native OpenCode import</p>
          <h2>OpenCode receives the visible conversation history.</h2>
          <p>
            OmniSession imports bounded visible user and assistant history through
            OpenCode&apos;s CLI, exports the result, and checks every message before launch.
            Failed imports remove the exact session they created.
          </p>
          <p className="native-caveat">
            Shell commands, tool calls, approvals, and hidden reasoning are never replayed.
          </p>
        </div>
        <div className="report-card" aria-label="Example fidelity report">
          <div className="report-heading">
            <span>Fidelity report</span>
            <span>codex → opencode</span>
          </div>
          <div className="report-row"><span>Messages</span><strong>128 imported</strong></div>
          <div className="report-row"><span>Repository</span><strong>exact match</strong></div>
          <div className="report-row"><span>Tool history</span><strong>documentary</strong></div>
          <div className="report-row"><span>Permission grants</span><strong>reset</strong></div>
          <div className="report-row"><span>Verification</span><strong className="report-good">passed</strong></div>
        </div>
      </section>

      <section className="section shell" id="support">
        <div className="section-intro compact">
          <p className="kicker">Provider support</p>
          <h2>Current import and resume support.</h2>
          <p>Private session formats change without notice, so OmniSession does not write them.</p>
        </div>
        <div className="support-table" role="table" aria-label="Provider support">
          <div className="support-row support-header" role="row">
            <span role="columnheader">Provider</span>
            <span role="columnheader">Same provider</span>
            <span role="columnheader">Cross-provider</span>
          </div>
          {support.map(([provider, same, cross]) => (
            <div className="support-row" role="row" key={provider}>
              <strong role="cell">{provider}</strong>
              <span role="cell">{same}</span>
              <span role="cell">{cross}</span>
            </div>
          ))}
        </div>
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
            <p>Imported commands and approvals remain records of what happened.</p>
          </article>
          <article>
            <span>03</span>
            <h3>Exact routing</h3>
            <p>Task bindings and session IDs decide where to resume. Recency does not.</p>
          </article>
        </div>
      </section>

      <section className="install shell" id="install">
        <div className="install-copy">
          <p className="kicker">Install</p>
          <h2>Add `omnis` and its agent shims.</h2>
          <p>The installer checks the downloaded release before placing anything on your PATH.</p>
        </div>
        <div className="terminal">
          <div className="terminal-bar">
            <span>terminal</span>
            <small>~/project</small>
          </div>
          <div className="terminal-body">
            <p><span>$</span> curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh</p>
            <p><span>$</span> omnis doctor</p>
            <p className="terminal-output">6 providers found · ready</p>
            <p><span>$</span> omnis resume &lt;session-id&gt; --in opencode<i /></p>
          </div>
        </div>
      </section>

      <footer className="footer shell">
        <a className="brand" href="#top">
          <Mark />
          <span>OmniSession</span>
        </a>
        <p>Continue coding sessions across agent CLIs.</p>
        <div>
          <a href="https://github.com/bvolpato/omnisession">GitHub</a>
          <span>MIT</span>
        </div>
      </footer>
    </main>
  );
}
