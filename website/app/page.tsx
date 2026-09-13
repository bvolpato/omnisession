import packageJson from "../package.json";
import { InstallCommand } from "./install-command";
import { providers } from "./providers.generated";
import { TerminalPreview } from "./terminal-preview";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const repoUrl = "https://github.com/bvolpato/omnisession";
const links = {
  repo: repoUrl,
  docs: `${repoUrl}#readme`,
  compatibility: `${repoUrl}/blob/main/docs/COMPATIBILITY.md`,
  rfcs: `${repoUrl}/blob/main/docs/rfcs/README.md`,
  changelog: `${repoUrl}/blob/main/CHANGELOG.md`,
  security: `${repoUrl}/blob/main/SECURITY.md`,
  license: `${repoUrl}/blob/main/LICENSE`,
  latestRelease: `${repoUrl}/releases/latest`,
};
const releaseVersion = `v${packageJson.version}`;

type Provider = (typeof providers)[number];
type CapabilityKey = keyof Provider["capabilities"];

const platformOrder = [
  { id: "linux", name: "Linux", short: "Linux" },
  { id: "macos", name: "macOS", short: "macOS" },
  { id: "windows", name: "Windows", short: "Win" },
] as const;

const capabilityColumns: readonly { key: CapabilityKey; label: string; detail?: "same" | "cross" }[] = [
  { key: "read_index", label: "Read & index" },
  { key: "clean_start", label: "Clean start" },
  { key: "same_provider_resume", label: "Same-agent resume", detail: "same" },
  { key: "cross_provider_import", label: "Cross-agent import", detail: "cross" },
];

const listFormat = new Intl.ListFormat("en", { style: "long", type: "conjunction" });

function declares(platforms: readonly string[], platform: string) {
  return platforms.includes(platform);
}

function platformScope(platforms: readonly string[]) {
  if (platforms.length === 0) return "Not guaranteed";
  return platformOrder.filter(({ id }) => declares(platforms, id)).map(({ name }) => name).join(" + ");
}

const readableProviders = providers.filter((provider) => provider.capabilities.read_index.length > 0);
const importTargets = providers.filter((provider) => provider.capabilities.cross_provider_import.length > 0);
const conformancePaths = importTargets.length * (importTargets.length - 1);
const readOnlySources = providers.filter(
  (provider) => provider.capabilities.same_provider_resume.length === 0 && provider.capabilities.cross_provider_import.length === 0,
);
const windowsNative = providers.filter((provider) =>
  (["read_index", "clean_start", "same_provider_resume"] as const).every((key) => declares(provider.capabilities[key], "windows")),
);
const windowsImports = providers.filter((provider) => declares(provider.capabilities.cross_provider_import, "windows"));
const officialImports = providers.filter((provider) => provider.signal === "OFFICIAL");

const codex = providers.find((provider) => provider.id === "codex");
const claude = providers.find((provider) => provider.id === "claude-code");

const navItems = [
  ["#features", "Features"],
  ["#how-it-works", "How it works"],
  ["#agents", "Agents"],
  ["#safety", "Safety"],
  ["#faq", "FAQ"],
] as const;

type IconName = "book" | "check" | "eyeOff" | "history" | "lock" | "route" | "search" | "shield" | "terminal" | "trash" | "undo";

const iconPaths: Record<IconName, React.ReactNode> = {
  book: <path d="M8 4c-2 0-3 1-3 3v2c0 1.2-.8 2-2 2 1.2 0 2 .8 2 2v2c0 2 1 3 3 3M16 4c2 0 3 1 3 3v2c0 1.2.8 2 2 2-1.2 0-2 .8-2 2v2c0 2-1 3-3 3" />,
  check: <><circle cx="12" cy="12" r="8.5" /><path d="m8.5 12 2.5 2.5 4.5-5" /></>,
  eyeOff: <><path d="M3 3l18 18" /><path d="M10.6 5.1A10.4 10.4 0 0 1 12 5c5 0 8.5 4.5 9.5 7a13 13 0 0 1-2.6 3.6M6.6 6.6A13 13 0 0 0 2.5 12c1 2.5 4.5 7 9.5 7 1.6 0 3-.4 4.3-1.1" /><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2" /></>,
  history: <><path d="M3.5 12a8.5 8.5 0 1 0 2.8-6.3" /><path d="M3.5 4.5v4h4" /><path d="M12 8v4l3 2" /></>,
  lock: <><rect x="5" y="11" width="14" height="9" rx="2" /><path d="M8 11V8a4 4 0 0 1 8 0v3" /></>,
  route: <><path d="M4 8h13l-3.5-3.5" /><path d="M20 16H7l3.5 3.5" /></>,
  search: <><circle cx="11" cy="11" r="6.5" /><path d="m20 20-4.2-4.2" /></>,
  shield: <><path d="M12 3 5 6v5.5c0 4.2 2.9 7.6 7 9.5 4.1-1.9 7-5.3 7-9.5V6l-7-3Z" /><path d="m9 12 2 2 4-4" /></>,
  terminal: <><rect x="3" y="4" width="18" height="14" rx="2.5" /><path d="m7 9 3 2.5L7 14M12.5 14H17" /></>,
  trash: <path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3" />,
  undo: <><path d="M9 14 4 9l5-5" /><path d="M4 9h10.5a5.5 5.5 0 0 1 0 11H11" /></>,
};

function Icon({ name }: { name: IconName }) {
  return (
    <svg aria-hidden="true" fill="none" height="20" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.7" viewBox="0 0 24 24" width="20">
      {iconPaths[name]}
    </svg>
  );
}

function ProviderLogo({ provider, size = 20 }: { provider: Provider; size?: number }) {
  const className = provider.logo === "codex" ? "provider-logo provider-logo-color" : "provider-logo";
  return <img alt="" className={className} height={size} src={`${basePath}/providers/${provider.logo}.svg`} width={size} />;
}

function PlatformMarks({ platforms }: { platforms: readonly string[] }) {
  return (
    <span className="platforms">
      <span className="platform-scope sr-only">{platformScope(platforms)}</span>
      {platformOrder.map(({ id, short }) => (
        <span aria-hidden="true" className={declares(platforms, id) ? "platform on" : "platform"} key={id}>{short}</span>
      ))}
    </span>
  );
}

const safetyItems: readonly { icon: IconName; title: string; body: React.ReactNode }[] = [
  {
    icon: "lock",
    title: "Read-only sources",
    body: <>Discovery and transfers open source stores read-only, and SQLite reads run with <code>query_only</code>. A deletion you select and confirm is the only change omni makes to a source store.</>,
  },
  {
    icon: "check",
    title: "Native imports read back",
    body: "A native import writes a new target session and reads it back through an independent adapter before launch. A semantic handoff starts a fresh session from a handoff document instead, so there is nothing to read back.",
  },
  {
    icon: "undo",
    title: "Exact rollback",
    body: "If a native import fails before launch, omni attempts an exact rollback that removes only the records it created. On Linux and macOS, Ctrl+C during a native import from the picker, omni resume, or omni fork triggers the same rollback; shim-routed imports aren't covered yet.",
  },
  {
    icon: "history",
    title: "History, not instructions",
    body: "Tool calls and shell commands arrive as historical text and are never replayed. Approvals, hidden reasoning, and provider permission state are left out.",
  },
  {
    icon: "eyeOff",
    title: "Redaction, with limits",
    body: <>Known authentication files are excluded, and recognized credential fields and patterns are redacted, though redaction can&apos;t prove every secret is gone. <code>omni search</code> prints session references unless you pass <code>--show-text</code>.</>,
  },
  {
    icon: "trash",
    title: "Guarded deletion",
    body: "Delete asks for confirmation and targets only the selected native session, through the agent's own command where one exists. Private-store deletes validate exact paths and refuse while that agent runs.",
  },
];

const faqItems: readonly { question: string; answer: React.ReactNode }[] = [
  {
    question: "How is this different from each agent's own resume?",
    answer: "An agent's own resume only sees its own history. omni indexes every supported agent, keeps related sessions grouped across agents, and uses the agent's own resume, plus fork where the agent has one, when you stay in the same agent.",
  },
  {
    question: "Does OmniSession change my existing sessions?",
    answer: "omni doesn't edit them. Cross-agent transfers open source stores read-only and continue in a separate target session, and the only change omni makes to a source store is a deletion you select and confirm. If you resume a session in place in its own agent, that agent keeps appending to it as usual.",
  },
  {
    question: "What carries over when I switch agents?",
    answer: <>A native import preserves ordered user and assistant messages plus bounded tool activity; a semantic handoff starts a fresh session from a handoff document instead. Tool calls and shell commands stay historical and are never replayed, and approvals, hidden reasoning, and provider permission state are left out. Known authentication files are excluded and recognized credential fields and patterns are redacted, but a credential that matches no pattern can still come along, so redaction can&apos;t prove every secret is gone. <code>omni inspect</code> reports transfer fidelity for a source and target.</>,
  },
  {
    question: "What if my agent's version isn't supported?",
    answer: (
      <>
        Version-gated native writers and import interfaces only run at or above each agent&apos;s minimum version, and every native import must pass
        structural validation and read-back.
        {officialImports.length > 0
          ? <> Official imports ({listFormat.format(officialImports.map((provider) => provider.name))}) have no minimum version and still go through read-back with exact rollback on failure.</>
          : null}
        {" "}When no native import is available, omni uses a semantic handoff if the target can start one, or leaves that target out of the picker.
      </>
    ),
  },
  {
    question: "Does anything leave my machine?",
    answer: <>OmniSession has no daemon, telemetry, or hosted session service. Background update checks contact GitHub; set <code>OMNI_NO_UPDATE_CHECK=1</code> to turn them off. Agents you launch keep their own network behavior.</>,
  },
  {
    question: "Can I use it on Windows?",
    answer: (
      <>
        Yes, as a preview. The native Windows <span className="nowrap">x86-64</span> build, PowerShell installer, and shims run in Windows CI
        {windowsNative.length > 0 ? <>, and {listFormat.format(windowsNative.map((provider) => provider.name))} declare read, clean start, and same-agent resume there</> : null}.
        {windowsImports.length === 0 ? " Continuing in a different agent from the picker uses a semantic handoff for now." : null} Provider fidelity remains
        provisional. Inside WSL, use the Linux installer.
      </>
    ),
  },
];

export default function Home() {
  return (
    <>
      <a className="skip-link" href="#main">Skip to content</a>
      <header className="site-header" id="top">
        <div className="container header-inner">
          <a className="brand" href="#top">
            <img alt="" height="28" src={`${basePath}/logo.svg`} width="28" />
            <span>OmniSession</span>
          </a>
          <nav aria-label="Primary" className="primary-nav">
            <ul role="list">
              {navItems.map(([href, label]) => <li key={href}><a href={href}>{label}</a></li>)}
            </ul>
          </nav>
          <div className="header-actions">
            <a className="button button-quiet button-small header-github" href={links.repo}>GitHub <span aria-hidden="true">↗</span></a>
            <a className="button button-primary button-small" href="#install">Install</a>
          </div>
        </div>
      </header>

      <main id="main">
        <section aria-labelledby="hero-title" className="hero">
          <div className="container">
            <div className="hero-copy">
              <a aria-label={`Read ${releaseVersion} release notes`} className="release-pill" href={links.latestRelease}>
                <span className="release-tag">Alpha</span>
                <span>{releaseVersion} release notes</span>
                <span aria-hidden="true" className="release-arrow">→</span>
              </a>
              <h1 id="hero-title">Continue any session <span className="gradient-text">in any agent.</span></h1>
              <p className="lede">
                OmniSession puts the sessions your coding agents leave on this machine into one fast picker. Pick one and keep going in another
                installed agent, natively where a verified import exists or through a semantic handoff.
              </p>
              <div className="hero-install">
                <InstallCommand variant="compact" />
              </div>
              <div className="hero-actions">
                <a className="button button-secondary" href={links.repo}>View on GitHub <span aria-hidden="true">↗</span></a>
                <a className="button button-ghost" href="#how-it-works">See how it works <span aria-hidden="true">→</span></a>
              </div>
              <ul aria-label="Highlights" className="hero-facts" role="list">
                <li>Local-first, no telemetry</li>
                <li>Linux, macOS, Windows preview</li>
                <li>MIT licensed</li>
              </ul>
            </div>
            <TerminalPreview />
          </div>
        </section>

        <section aria-labelledby="works-with-title" className="works-with">
          <div className="container">
            <h2 className="works-with-title" id="works-with-title">Reads sessions from {readableProviders.length} coding agents</h2>
            <ul className="agent-chips" role="list">
              {providers.map((provider) => (
                <li className="agent-chip" key={provider.id}><ProviderLogo provider={provider} /><span>{provider.name}</span></li>
              ))}
            </ul>
          </div>
        </section>

        <section aria-labelledby="features-title" className="section" id="features">
          <div className="container">
            <div className="section-head center reveal">
              <p className="kicker">Why OmniSession</p>
              <h2 id="features-title">Switch agents. Keep the thread.</h2>
              <p>Every coding agent keeps its own history in its own format. OmniSession reads {readableProviders.length} of them, so work you start in one can keep going in another.</p>
            </div>
            <div className="bento">
              <article className="feature feature-wide tone-cyan reveal">
                <span className="feature-icon"><Icon name="route" /></span>
                <h3>Any session, any agent</h3>
                <p>
                  Start in Codex, finish in Claude Code, hand the tricky part to Grok. Where a native import exists, omni writes a real target session:
                  version-gated writers and import interfaces check the agent&apos;s release first, official imports go through the agent&apos;s own API, and
                  every native import is validated and read back before launch, with an exact rollback attempt if it fails. Otherwise omni starts a fresh
                  session with a semantic handoff. Tool calls come along as history and are never replayed.
                </p>
                <div aria-hidden="true" className="route-demo">
                  {codex ? <span className="route-agent"><ProviderLogo provider={codex} size={18} />codex</span> : null}
                  <span className="route-line"><i /></span>
                  {claude ? <span className="route-agent active"><ProviderLogo provider={claude} size={18} />claude</span> : null}
                </div>
                <ul className="checks" role="list">
                  <li>Version gates where required</li>
                  <li>Structural validation</li>
                  <li>Read-back</li>
                  <li>Exact rollback on failure</li>
                </ul>
                {readOnlySources.length > 0
                  ? <p className="feature-foot">{listFormat.format(readOnlySources.map((provider) => provider.name))} {readOnlySources.length === 1 ? "is a read-only source" : "are read-only sources"}: continue its sessions in another agent.</p>
                  : null}
              </article>

              <article className="feature feature-narrow tone-amber reveal">
                <span className="feature-icon"><Icon name="search" /></span>
                <h3>One picker for every agent&apos;s history</h3>
                <p>
                  Fuzzy-match titles, folders, branches, and IDs, with full-text search over a local, redacted index. Script the same search with{" "}
                  <code>omni search --json</code>. Indexing is incremental, so repeat runs only pick up what changed.
                </p>
                <pre className="snippet"><code><span className="prompt">$</span> omni search &quot;rate limiter&quot;{"\n"}<span className="prompt">$</span> omni search pagination --all-projects --json</code></pre>
              </article>

              <article className="feature tone-green reveal">
                <span className="feature-icon"><Icon name="shield" /></span>
                <h3>Safe by design</h3>
                <p>
                  Discovery and transfers open source stores read-only, and recognized credential fields and patterns are redacted, though redaction
                  can&apos;t prove every secret is gone. On Linux and macOS, Ctrl+C during a picker, resume, or fork import triggers an exact rollback. Deletion always asks first.
                </p>
                <ul className="tags" role="list">
                  <li>Read-only sources</li>
                  <li>Redaction</li>
                  <li>Exact rollback</li>
                  <li>Confirmed deletes</li>
                </ul>
                <a className="text-link" href="#safety">How omni stays safe <span aria-hidden="true">→</span></a>
              </article>

              <article className="feature tone-magenta reveal">
                <span className="feature-icon"><Icon name="terminal" /></span>
                <h3>Works where you work</h3>
                <p>
                  Prebuilt for Linux and macOS on <span className="nowrap">x86-64</span> and ARM64, with a native Windows <span className="nowrap">x86-64</span> preview. Provider shims route{" "}
                  <code>claude --continue</code> into the task you bound, and portable bundles keep a redacted session resumable after its original store is gone.
                </p>
                <ul className="tags" role="list">
                  <li>Linux</li>
                  <li>macOS</li>
                  <li>Windows preview</li>
                  <li>Shims</li>
                  <li>Bundles</li>
                </ul>
              </article>

              <article className="feature tone-cyan reveal">
                <span className="feature-icon"><Icon name="book" /></span>
                <h3>Open and rigorous</h3>
                <p>
                  Design decisions live in public RFCs. Adapters share one canonical event model, a credential-free conformance matrix runs all{" "}
                  {conformancePaths} cross-agent paths between agents that support imports, and one compatibility manifest generates the docs&apos;
                  compatibility table, the CLI&apos;s capability gates, and the matrix on this page.
                </p>
                <dl className="stats">
                  <div><dt>Cross-agent paths</dt><dd>{conformancePaths}</dd></div>
                  <div><dt>Agents, one manifest</dt><dd>{providers.length}</dd></div>
                </dl>
                <p className="feature-links">
                  <a className="text-link" href={links.rfcs}>RFCs <span aria-hidden="true">↗</span></a>
                  <a className="text-link" href={links.compatibility}>Compatibility <span aria-hidden="true">↗</span></a>
                </p>
              </article>
            </div>
          </div>
        </section>

        <section aria-labelledby="how-title" className="section section-alt" id="how-it-works">
          <div className="container">
            <div className="section-head center reveal">
              <p className="kicker">How it works</p>
              <h2 id="how-title">Discover. Search. Continue.</h2>
              <p>Three steps from &ldquo;where was that session?&rdquo; to working in the agent you want.</p>
            </div>
            <ol className="steps" role="list">
              <li className="step reveal">
                <span className="step-num">01</span>
                <h3>Discover</h3>
                <p>Run <code>omni</code>. It finds sessions from every supported agent you have installed and lists the current workspace first. Press <kbd>Tab</kbd> to include every workspace.</p>
                <div aria-hidden="true" className="step-demo"><span className="prompt">$</span> omni</div>
              </li>
              <li className="step reveal">
                <span className="step-num">02</span>
                <h3>Search</h3>
                <p>Type to filter. Titles, folders, branches, and IDs match fuzzily, and conversation text matches from a local index that builds in the background.</p>
                <div aria-hidden="true" className="step-demo"><span className="dim">Search ›</span> token race<i className="caret" /></div>
              </li>
              <li className="step reveal">
                <span className="step-num">03</span>
                <h3>Continue</h3>
                <p>Pick the agent to keep going in. omni uses that agent&apos;s own resume or fork, a native import it reads back before launch, or a semantic handoff into a fresh session.</p>
                <div aria-hidden="true" className="step-demo"><span className="prompt">$</span> omni resume &lt;session&gt; --in claude</div>
              </li>
            </ol>
          </div>
        </section>

        <section aria-labelledby="agents-title" className="section" id="agents">
          <div className="container">
            <div className="section-head reveal">
              <p className="kicker">Supported agents</p>
              <h2 id="agents-title">{providers.length} agents. One honest matrix.</h2>
              <p>
                Declared platform support, generated from the compatibility manifest. A version gate is the minimum release omni writes or imports into
                natively. OFFICIAL marks an import through the agent&apos;s documented API with no minimum version, still verified by read-back with exact
                rollback on failure. READ-ONLY sources can be continued in other agents. Run <code>omni adapters</code> to see what&apos;s ready on this machine.
              </p>
            </div>
            <div className="matrix-wrap reveal">
              <table aria-label="Agent support by platform" className="matrix" role="table">
                <thead role="rowgroup">
                  <tr role="row">
                    <th role="columnheader" scope="col">Agent</th>
                    <th role="columnheader" scope="col">Version / access</th>
                    {capabilityColumns.map((column) => <th key={column.key} role="columnheader" scope="col">{column.label}</th>)}
                  </tr>
                </thead>
                <tbody role="rowgroup">
                  {providers.map((provider) => (
                    <tr key={provider.id} role="row">
                      <th role="rowheader" scope="row"><span className="agent-cell"><ProviderLogo provider={provider} />{provider.name}</span></th>
                      <td className="gate-cell" data-label="Version / access" role="cell"><span className={`gate tone-${provider.tone}`}>{provider.signal}</span></td>
                      {capabilityColumns.map((column) => (
                        <td data-label={column.label} key={column.key} role="cell">
                          <PlatformMarks platforms={provider.capabilities[column.key]} />
                          {column.detail ? <span className="cap-detail">{column.detail === "same" ? provider.same : provider.cross}</span> : null}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="matrix-foot">
              <p><span aria-hidden="true" className="platform on">Linux</span> declared <span aria-hidden="true" className="platform">Win</span> not declared. Windows is a preview, and fidelity there remains provisional.</p>
              <a className="text-link" href={links.compatibility}>Compatibility notes <span aria-hidden="true">↗</span></a>
            </div>
            <p className="trademark-note">Logos identify compatible tools. OmniSession is independent and not endorsed by their owners.</p>
          </div>
        </section>

        <section aria-labelledby="safety-title" className="section section-alt" id="safety">
          <div className="container safety">
            <div className="safety-lead reveal">
              <p className="kicker">Safety</p>
              <h2 id="safety-title">Your originals stay original.</h2>
              <p>omni runs on your machine with no daemon, telemetry, or hosted session service; background update checks contact GitHub. Transcripts can hold code, tokens, and personal data, so the defaults assume they do.</p>
              <a className="text-link" href={links.security}>Security model <span aria-hidden="true">↗</span></a>
            </div>
            <ul className="safety-grid" role="list">
              {safetyItems.map((item) => (
                <li className="safety-item reveal" key={item.title}>
                  <span className="safety-icon"><Icon name={item.icon} /></span>
                  <h3>{item.title}</h3>
                  <p>{item.body}</p>
                </li>
              ))}
            </ul>
          </div>
        </section>

        <section aria-labelledby="install-title" className="section" id="install">
          <div className="container install-grid">
            <div className="install-copy reveal">
              <p className="kicker">Install</p>
              <h2 id="install-title">Install in one line.</h2>
              <p>Prebuilt binaries for Linux and macOS on <span className="nowrap">x86-64</span> and ARM64. Native Windows <span className="nowrap">x86-64</span> is a preview.</p>
              <ol className="install-steps" role="list">
                <li><strong>Run the installer</strong><span>It verifies the release checksum before installing <code>omni</code>.</span></li>
                <li><strong>Run <code>omni</code></strong><span>Pick a session, then pick the agent to continue in.</span></li>
                <li><strong>Check this machine</strong><span><code>omni doctor</code> and <code>omni adapters</code> show which agents are installed and ready.</span></li>
              </ol>
            </div>
            <div className="install-panel reveal">
              <InstallCommand variant="full" />
            </div>
          </div>
        </section>

        <section aria-labelledby="faq-title" className="section section-alt" id="faq">
          <div className="container">
            <div className="section-head center reveal">
              <p className="kicker">FAQ</p>
              <h2 id="faq-title">Questions, answered.</h2>
            </div>
            <div className="faq-list">
              {faqItems.map((item) => (
                <details className="faq-item" key={item.question}>
                  <summary><h3>{item.question}</h3></summary>
                  <p>{item.answer}</p>
                </details>
              ))}
            </div>
          </div>
        </section>

        <section aria-labelledby="cta-title" className="cta">
          <div className="container">
            <div className="cta-inner reveal">
              <h2 id="cta-title">Switch agents without starting from scratch.</h2>
              <p>Install omni, pick a session, keep going.</p>
              <div className="cta-actions">
                <a className="button button-primary" href="#install">Install omni</a>
                <a className="button button-secondary" href={links.repo}>View source on GitHub <span aria-hidden="true">↗</span></a>
              </div>
            </div>
          </div>
        </section>
      </main>

      <footer className="site-footer">
        <div className="container footer-grid">
          <div className="footer-brand">
            <a className="brand" href="#top">
              <img alt="" height="28" src={`${basePath}/logo.svg`} width="28" />
              <span>OmniSession</span>
            </a>
            <p>Continue coding sessions across agents. MIT licensed.</p>
          </div>
          <nav aria-label="Project">
            <ul className="footer-links" role="list">
              <li><a href={links.repo}>GitHub</a></li>
              <li><a href={links.docs}>Docs</a></li>
              <li><a href={links.compatibility}>Compatibility</a></li>
              <li><a href={links.rfcs}>RFCs</a></li>
              <li><a href={links.changelog}>Changelog</a></li>
              <li><a href={links.security}>Security</a></li>
              <li><a href={links.license}>License</a></li>
            </ul>
          </nav>
        </div>
        <p className="container footer-note">{releaseVersion} · Provider names and logos belong to their owners.</p>
      </footer>
    </>
  );
}
