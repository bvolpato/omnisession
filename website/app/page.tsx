const providers = ["Claude", "Codex", "OpenCode", "Grok", "Cursor"];

const features = [
  {
    number: "01",
    title: "Discover",
    body: "See sessions across installed agents, filtered to the repository you are already in.",
    command: "omnis list --project .",
  },
  {
    number: "02",
    title: "Handoff",
    body: "Turn visible context into a bounded semantic handoff. Tool calls stay history, never instructions.",
    command: "omnis resume claude:<id> --in codex",
  },
  {
    number: "03",
    title: "Continue",
    body: "Keep explicit task lineage across agents, branches, and native provider sessions.",
    command: "omnis checkout auth-refactor",
  },
];

function Mark() {
  return (
    <span className="mark" aria-hidden="true">
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
    <main>
      <nav className="nav shell" aria-label="Primary navigation">
        <a className="brand" href="#top" aria-label="OmniSession home">
          <Mark />
          <span>OmniSession</span>
        </a>
        <div className="nav-links">
          <a href="#how">How it works</a>
          <a href="#safety">Safety</a>
          <a
            className="nav-github"
            href="https://github.com/bvolpato/omnisession"
          >
            GitHub <Arrow />
          </a>
        </div>
      </nav>

      <section className="hero shell" id="top">
        <div className="hero-copy">
          <p className="eyebrow">
            <span className="live-dot" /> Local-first session fabric
          </p>
          <h1>
            Switch agents.
            <br />
            <em>Keep the thread.</em>
          </h1>
          <p className="lede">
            Move coding work between Claude, Codex, OpenCode, Grok, and Cursor
            without losing task lineage or repository context.
          </p>
          <div className="hero-actions">
            <a className="button button-primary" href="#install">
              Get started <span aria-hidden="true">↓</span>
            </a>
            <a
              className="button button-secondary"
              href="https://github.com/bvolpato/omnisession"
            >
              View on GitHub <Arrow />
            </a>
          </div>
        </div>

        <div className="handoff-stage" aria-label="Session handoff illustration">
          <div className="orbit orbit-one" />
          <div className="orbit orbit-two" />
          <div className="provider provider-claude">Claude</div>
          <div className="provider provider-codex">Codex</div>
          <div className="provider provider-cursor">Cursor</div>
          <div className="provider provider-grok">Grok</div>
          <div className="provider provider-opencode">OpenCode</div>
          <div className="session-core">
            <div className="core-pulse" />
            <Mark />
            <span className="core-label">One task</span>
            <span className="core-detail">continuous context</span>
          </div>
          <div className="handoff-path path-a" />
          <div className="handoff-path path-b" />
          <div className="handoff-path path-c" />
        </div>
      </section>

      <section className="provider-strip" aria-label="Supported providers">
        <div className="provider-track">
          {[...providers, ...providers].map((provider, index) => (
            <span key={`${provider}-${index}`}>
              {provider} <i>✦</i>
            </span>
          ))}
        </div>
      </section>

      <section className="section shell" id="how">
        <div className="section-heading">
          <p className="kicker">One thread, any agent</p>
          <h2>Context should travel with the work.</h2>
          <p>
            OmniSession adds a small, explicit coordination layer around native
            sessions. Providers keep owning their data. You keep owning your
            flow.
          </p>
        </div>

        <div className="steps">
          {features.map((feature) => (
            <article className="step" key={feature.number}>
              <span className="step-number">{feature.number}</span>
              <h3>{feature.title}</h3>
              <p>{feature.body}</p>
              <code>{feature.command}</code>
            </article>
          ))}
        </div>
      </section>

      <section className="flow-section shell">
        <div className="flow-card">
          <div className="flow-copy">
            <p className="kicker">Semantic handoff</p>
            <h2>Pass intent, not baggage.</h2>
            <p>
              A focused handoff carries recent visible context, repository
              fingerprints, and explicit lineage. Commands, approvals, and tool
              calls remain inert history.
            </p>
          </div>
          <div className="flow-visual" aria-label="Claude to Codex handoff">
            <div className="flow-agent">
              <span className="agent-icon">C</span>
              <span>
                <strong>Claude</strong>
                <small>source session</small>
              </span>
            </div>
            <div className="flow-line">
              <span>semantic handoff</span>
              <i />
            </div>
            <div className="flow-agent">
              <span className="agent-icon agent-icon-dark">X</span>
              <span>
                <strong>Codex</strong>
                <small>new session</small>
              </span>
            </div>
          </div>
        </div>
      </section>

      <section className="section safety shell" id="safety">
        <div className="safety-graphic" aria-hidden="true">
          <div className="vault-ring vault-ring-outer" />
          <div className="vault-ring vault-ring-inner" />
          <div className="vault-core">
            <Mark />
          </div>
          <span className="vault-note vault-note-one">read-only</span>
          <span className="vault-note vault-note-two">local</span>
          <span className="vault-note vault-note-three">explicit</span>
        </div>
        <div className="safety-copy">
          <p className="kicker">Safety by architecture</p>
          <h2>Your sessions stay where they are.</h2>
          <p>
            OmniSession reads provider stores without rewriting them. Routing
            uses canonical workspace paths and repository fingerprints, never
            recency guesses.
          </p>
          <ul>
            <li>Provider stores remain read-only</li>
            <li>Authentication files are never read</li>
            <li>Native target writers stay disabled</li>
          </ul>
        </div>
      </section>

      <section className="install shell" id="install">
        <div className="install-heading">
          <p className="kicker">Start local</p>
          <h2>One command. Every thread in reach.</h2>
        </div>
        <div className="terminal">
          <div className="terminal-bar">
            <span />
            <span />
            <span />
            <small>~/your-project</small>
          </div>
          <div className="terminal-body">
            <p>
              <span className="prompt">$</span>{" "}curl --proto &apos;=https&apos;
              --tlsv1.2 -LsSf
              https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh
              | sh
            </p>
            <p>
              <span className="prompt">$</span> omnis --version
            </p>
            <p className="terminal-output">omnis 0.2.1</p>
            <p>
              <span className="prompt">$</span> omnis list --project .
              <span className="cursor-block" />
            </p>
          </div>
        </div>
      </section>

      <footer className="footer shell">
        <a className="brand" href="#top">
          <Mark />
          <span>OmniSession</span>
        </a>
        <p>Switch agents. Keep the thread.</p>
        <div>
          <a href="https://github.com/bvolpato/omnisession">GitHub</a>
          <span>MIT licensed</span>
        </div>
      </footer>
    </main>
  );
}
