import { Fragment, type ReactNode } from "react";

import { AgentBullet } from "./agent-bullet";
import {
  agentById, agents, basePath, conformancePaths, declares, platformOrder, platformScope, readOnlySources, sources, targets,
  type Agent, type AgentId, type CapabilityKey,
} from "./agents";
import { Command } from "./command";
import { InstallCommand } from "./install-command";
import { PickerDemo } from "./picker-demo";
import { RouteMap } from "./route-map";

const repoUrl = "https://github.com/bvolpato/omnisession";
const links = {
  repo: repoUrl,
  docs: `${repoUrl}#readme`,
  guide: `${repoUrl}/blob/main/docs/GUIDE.md`,
  profiles: `${repoUrl}/blob/main/docs/PROFILES.md`,
  compatibility: `${repoUrl}/blob/main/docs/COMPATIBILITY.md`,
  architecture: `${repoUrl}/blob/main/docs/ARCHITECTURE.md`,
  rfcs: `${repoUrl}/blob/main/docs/rfcs/README.md`,
  changelog: `${repoUrl}/blob/main/CHANGELOG.md`,
  security: `${repoUrl}/blob/main/SECURITY.md`,
  license: `${repoUrl}/blob/main/LICENSE`,
  latestRelease: `${repoUrl}/releases/latest`,
};

const capabilityColumns: readonly { key: CapabilityKey; label: string; detail?: "same" | "cross" }[] = [
  { key: "read_index", label: "Read & index" },
  { key: "clean_start", label: "Clean start" },
  { key: "same_provider_resume", label: "Same-agent resume", detail: "same" },
  { key: "cross_provider_import", label: "Cross-agent import", detail: "cross" },
];

const listFormat = new Intl.ListFormat("en", { style: "long", type: "conjunction" });
const windowsNative = agents.filter((agent) =>
  (["read_index", "clean_start", "same_provider_resume"] as const).every((key) => declares(agent.capabilities[key], "windows")),
);

const claudeGate = agentById("claude-code").signal.replace(">= ", "");

const navItems = [
  ["#map", "Routes"],
  ["#transfer", "How it works"],
  ["#search", "Search"],
  ["#safety", "Safety"],
  ["#faq", "FAQ"],
] as const;

/** One conversation, four agents: the lineage the demo below shows. */
const thread: readonly { agent: AgentId; does: string }[] = [
  { agent: "claude-code", does: "writes the rate limiter" },
  { agent: "grok", does: "finds two bypasses" },
  { agent: "codex", does: "moves buckets to Redis" },
  { agent: "opencode", does: "load-tests it" },
];

const transferStops: readonly { title: string; body: ReactNode }[] = [
  {
    title: "Read",
    body: "The source store is opened read-only. Visible messages and tool records become one ordered history. Hidden reasoning, approvals, and permission state stay out.",
  },
  {
    title: "Write",
    body: "omni creates a new session in the target agent's own format, through its import interface or a writer gated on that agent's version.",
  },
  {
    title: "Read back",
    body: "An independent reader loads the new session and compares it with what was sent. If anything differs, omni removes only what it created.",
  },
  {
    title: "Launch",
    body: "The agent opens with the conversation in place. Tool calls and shell commands arrive as history and never run again.",
  },
];

const searches: readonly { query: string; finds: ReactNode }[] = [
  { query: '"rate limiter"', finds: "Every word must match. Titles, folders, branches, and IDs match fuzzily and rank first. Conversation text follows." },
  { query: "qwen3.8", finds: <>Finds <code>qwen3-8</code> and <code>Qwen3.8-Coder</code>, but not <code>qwen3</code> and <code>8</code> far apart.</> },
  { query: `'"token race"'`, finds: "Quotes inside the query match exactly, spaces and punctuation included. Only case is ignored." },
  { query: "pagination --all-projects --json", finds: "Searches every workspace and prints structured results for scripts." },
  { query: "pagination --cached", finds: "Answers from the local index without opening a single provider store." },
];

const extras: readonly { title: string; command: string; body: ReactNode }[] = [
  { title: "Inspect before you move", command: "omni inspect <session> --target grok", body: "Reports what this route preserves, summarizes, or leaves out." },
  { title: "Launch profiles", command: "omni resume <session> --in claude-gateway", body: <>Name a gateway, wrapper, or set of flags once, then pick it like any agent. <a href={links.profiles}>Profiles</a></> },
  { title: "Permission modes", command: "omni resume <session> --in codex --mode auto", body: "Choose how much the agent may do without asking. The default adds no flags." },
  { title: "Portable bundles", command: "omni export <session> -o session.omnisession", body: "Move a redacted session to another machine, or keep it after its store is gone." },
  { title: "Chat-app handoff", command: "omni handoff <session>", body: "Saves the full context to a private file and copies a compact brief to the clipboard." },
  { title: "Machine check", command: "omni doctor", body: "Shows installed agents, their versions, and which imports are ready." },
];

const safetyItems: readonly { title: string; body: ReactNode }[] = [
  { title: "Read-only sources", body: <>Discovery, search, and transfers open source stores read-only. The one change omni can make to a source is a deletion you select and confirm.</> },
  { title: "Local", body: <>No daemon, telemetry, or hosted service. Background update checks contact GitHub, and <code>OMNI_NO_UPDATE_CHECK=1</code> turns them off.</> },
  { title: "Exact rollback", body: <>If an import fails before launch, or you press <kbd>Ctrl</kbd>+<kbd>C</kbd>, omni rolls back only the records it created and exits without launching.</> },
  { title: "History, not instructions", body: "Imported tool calls and shell commands are historical text. They are never replayed." },
  { title: "Redaction, with limits", body: "Known authentication files are never read, and recognized credentials are redacted. That can't prove every secret is gone, so review sensitive sessions before you transfer them." },
  { title: "Guarded deletion", body: "Delete asks first and targets only the selected session. Private-store deletes validate exact paths and refuse while that agent runs." },
];

const faqItems: readonly { question: string; answer: ReactNode }[] = [
  {
    question: "How is this different from each agent's own resume?",
    answer: "An agent's own resume only sees its own history. omni lists every supported agent together, keeps related sessions grouped across agents, and still uses the agent's own resume or fork when you stay in the same agent.",
  },
  {
    question: "Does OmniSession change my existing sessions?",
    answer: "No. A cross-agent transfer reads the source and continues in a separate new session. If you resume a session in place in its own agent, that agent keeps appending to it as usual.",
  },
  {
    question: "What carries over when I switch agents?",
    answer: <>A native import keeps ordered user and assistant messages plus bounded tool activity. A handoff starts a fresh session from a private file instead. Approvals, hidden reasoning, and permission state are left out. <code>omni inspect</code> reports the details for a specific source and target.</>,
  },
  {
    question: "What if my agent's version isn't supported?",
    answer: <>The native import can&apos;t run. On a terminal, omni says why and asks whether to retry, fork in the source agent, continue with a handoff, or cancel. Scripts fall back to the handoff when that is safe. <code>omni adapters --check-imports</code> shows what is ready before you try.</>,
  },
  {
    question: "Does anything leave my machine?",
    answer: "OmniSession has no telemetry or hosted service. Update checks contact GitHub. The agent that continues a session receives the transferred history and keeps its own network behavior.",
  },
  {
    question: "Can I use it on Windows?",
    answer: (
      <>
        Yes, as a preview. The native Windows <span className="nowrap">x86-64</span> build and its checksum-verifying PowerShell installer are tested in Windows CI.
        {windowsNative.length > 0 ? <> {listFormat.format(windowsNative.map((agent) => agent.name))} are supported there today; other agents are not yet.</> : null}
        {" "}Inside WSL, use the Linux installer.
      </>
    ),
  },
  {
    question: "Do claude, codex, and the other commands still work as before?",
    answer: <>Yes. OmniSession installs only <code>omni</code>. Commands such as <code>claude --continue</code> and <code>omp update</code> go straight to their providers.</>,
  },
];

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

function AgentName({ agent }: { agent: Agent }) {
  return <span className="agent-cell"><AgentBullet agent={agent} size={26} />{agent.name}</span>;
}

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
            <a className="header-link" href={links.docs}>Docs</a>
            <a className="header-link" href={links.repo}>GitHub</a>
            <a className="button button-light" href="#install">Install</a>
          </div>
        </div>
      </header>

      <main id="main">
        <section aria-labelledby="hero-title" className="hero">
          <div className="container">
            <div className="hero-top">
              <h1 id="hero-title">Switch agents. <span>Keep the thread.</span></h1>
              <div className="hero-side">
                <p className="lede">
                  Claude Code, Codex, Cursor, and the rest each keep their own history. <code>omni</code> finds every session on your machine and
                  continues the one you pick in whichever agent you want next.
                </p>
                <InstallCommand variant="compact" />
                <p className="hero-facts">MIT licensed, local, no telemetry. <a href="#install">Windows preview</a>, <a href={links.latestRelease}>latest release</a>.</p>
              </div>
            </div>

            <ol aria-label="One conversation across four agents" className="thread" role="list">
              {thread.map(({ agent: id, does }) => (
                <li className={`thread-stop line-${id}`} key={id}>
                  <AgentBullet agent={agentById(id)} size={36} />
                  <span><strong>{agentById(id).name}</strong> {does}</span>
                </li>
              ))}
            </ol>

            <PickerDemo />
          </div>
        </section>

        <section aria-labelledby="map-title" className="section" id="map">
          <div className="container">
            <div className="section-head">
              <h2 id="map-title">Change here for any agent.</h2>
              <p>
                omni reads sessions from {sources.length} agents and writes native ones into {targets.length}. A conformance run checks all {conformancePaths} cross-agent
                paths between them. Pick two ends and take the route.
              </p>
            </div>
            <RouteMap />
          </div>
        </section>

        <section aria-labelledby="transfer-title" className="section" id="transfer">
          <div className="container">
            <div className="section-head">
              <h2 id="transfer-title">What happens when you press Enter.</h2>
              <p>A transfer is a real session in the target agent, checked before you see it. Staying in the same agent uses that agent&apos;s own resume or fork.</p>
            </div>
            <ol className="stops" role="list">
              {transferStops.map((stop) => (
                <li className="stop-card" key={stop.title}>
                  <h3>{stop.title}</h3>
                  <p>{stop.body}</p>
                </li>
              ))}
            </ol>
            <div className="siding">
              <div>
                <h3>When an import can&apos;t run, you choose.</h3>
                <p>omni never quietly swaps the import you asked for. It says why, then offers the routes that are open. Scripts and pipes fall back to a handoff when that is safe.</p>
              </div>
              <pre className="terminal-note wrap"><code>Claude native import could not run: Claude 2.1.100 is too old for native trajectory import; supported versions: &gt;= {claudeGate}{"\n"}<span className="t-accent">  [r] retry   [f] fork in Codex instead   [h] continue with a handoff file   [q] cancel</span>{"\n"}&gt; <i className="demo-caret idle" /></code></pre>
            </div>
          </div>
        </section>

        <section aria-labelledby="search-title" className="section" id="search">
          <div className="container">
            <div className="section-head">
              <h2 id="search-title">Find the one you half remember.</h2>
              <p>One redacted full-text index on your machine covers every agent. Later runs index only what changed.</p>
            </div>
            <dl className="searches">
              {searches.map(({ query, finds }) => (
                <div key={query}>
                  <dt><Command muted="omni search" text={query} /></dt>
                  <dd>{finds}</dd>
                </div>
              ))}
            </dl>
            <h3 className="extras-title">Also on this line</h3>
            <ul className="extras" role="list">
              {extras.map((extra) => (
                <li key={extra.title}>
                  <h4>{extra.title}</h4>
                  <p>{extra.body}</p>
                  <Command text={extra.command} />
                </li>
              ))}
            </ul>
          </div>
        </section>

        <section aria-labelledby="safety-title" className="section section-sign" id="safety">
          <div className="container">
            <div className="section-head">
              <h2 id="safety-title">Your originals stay original.</h2>
              <p>Transcripts can hold code, tokens, and personal data, so the defaults assume they do. <a href={links.security}>Read the security model</a>.</p>
            </div>
            <ul className="safety-grid" role="list">
              {safetyItems.map((item) => (
                <li key={item.title}>
                  <h3>{item.title}</h3>
                  <p>{item.body}</p>
                </li>
              ))}
            </ul>
          </div>
        </section>

        <section aria-labelledby="agents-title" className="section" id="agents">
          <div className="container">
            <div className="section-head">
              <h2 id="agents-title">{agents.length} agents, platform by platform.</h2>
              <p>
                Generated from the manifest the CLI uses. The version is the oldest release omni imports into. OFFICIAL is an import through the
                agent&apos;s own API. {readOnlySources.length > 0 ? `${listFormat.format(readOnlySources.map((agent) => agent.name))} are read-only: continue their sessions in another agent.` : null}
              </p>
            </div>
            <div className="matrix-wrap">
              <table aria-label="Agent support by platform" className="matrix" role="table">
                <thead role="rowgroup">
                  <tr role="row">
                    <th role="columnheader" scope="col">Agent</th>
                    <th role="columnheader" scope="col">Version / access</th>
                    {capabilityColumns.map((column) => <th key={column.key} role="columnheader" scope="col">{column.label}</th>)}
                  </tr>
                </thead>
                <tbody role="rowgroup">
                  {agents.map((agent) => (
                    <tr key={agent.id} role="row">
                      <th role="rowheader" scope="row"><AgentName agent={agent} /></th>
                      <td className="gate-cell" data-label="Version / access" role="cell"><span className="gate">{agent.signal}</span></td>
                      {capabilityColumns.map((column) => (
                        <td data-label={column.label} key={column.key} role="cell">
                          <PlatformMarks platforms={agent.capabilities[column.key]} />
                          {column.detail ? <span className="cap-detail">{column.detail === "same" ? agent.same : agent.cross}</span> : null}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <p className="matrix-foot">
              Run <code>omni adapters --check-imports</code> to see what is ready on your machine. <a href={links.compatibility}>Compatibility notes</a>
            </p>
          </div>
        </section>

        <section aria-labelledby="install-title" className="section" id="install">
          <div className="container install-grid">
            <div>
              <h2 id="install-title">One binary. One line.</h2>
              <p className="install-lede">Prebuilt for Linux and macOS on <span className="nowrap">x86-64</span> and ARM64. Native Windows <span className="nowrap">x86-64</span> is a preview.</p>
              <InstallCommand variant="full" />
            </div>
            <pre className="terminal-note first-run"><code>{[
              ["omni", "browse every agent's sessions"],
              ["omni search \"rate limiter\"", "search without the picker"],
              ["omni resume <session> --in codex", "continue in another agent"],
              ["omni fork <session>", "branch, and choose where"],
              ["omni doctor", "check this machine"],
            ].map(([command, note]) => (
              <Fragment key={command}><span className="t-ok">$</span> {command}{"\n"}<span className="t-muted">  {note}</span>{"\n"}</Fragment>
            ))}</code></pre>
          </div>
        </section>

        <section aria-labelledby="faq-title" className="section" id="faq">
          <div className="container faq-grid">
            <h2 id="faq-title">Questions.</h2>
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
      </main>

      <footer className="site-footer">
        <div className="container footer-grid">
          <div>
            <a className="brand" href="#top">
              <img alt="" height="28" src={`${basePath}/logo.svg`} width="28" />
              <span>OmniSession</span>
            </a>
            <p>Independent and MIT licensed. Agent names and logos belong to their owners, who do not endorse this project.</p>
          </div>
          <nav aria-label="Project">
            <ul className="footer-links" role="list">
              <li><a href={links.repo}>GitHub</a></li>
              <li><a href={links.guide}>Guide</a></li>
              <li><a href={links.compatibility}>Compatibility</a></li>
              <li><a href={links.architecture}>Architecture</a></li>
              <li><a href={links.rfcs}>RFCs</a></li>
              <li><a href={links.changelog}>Changelog</a></li>
              <li><a href={links.security}>Security</a></li>
              <li><a href={links.license}>License</a></li>
            </ul>
          </nav>
        </div>
      </footer>
    </>
  );
}
