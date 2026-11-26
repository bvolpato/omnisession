"use client";

import { useState } from "react";

import { AgentBullet } from "./agent-bullet";
import { agentById, agents, basePath, platformScope, sources, targets, type AgentId } from "./agents";
import { Command } from "./command";
import { CopyCommand } from "./copy-command";

const width = 400;

function rowCenter(index: number, count: number) {
  return ((index + 0.5) / count) * 100;
}

function curve(from: number, to: number) {
  return `M0 ${from} C ${width * 0.5} ${from}, ${width * 0.5} ${to}, ${width} ${to}`;
}

const readOnlyIds: ReadonlySet<AgentId> = new Set(agents.filter((agent) => agent.capabilities.same_provider_resume.length === 0).map((agent) => agent.id));

/** Every declared route, drawn from the compatibility manifest. */
const routes = sources.flatMap((from, fromIndex) =>
  targets.map((to, toIndex) => ({
    key: `${from.id}>${to.id}`,
    from: from.id,
    to: to.id,
    path: curve(rowCenter(fromIndex, sources.length), rowCenter(toIndex, targets.length)),
    middle: (rowCenter(fromIndex, sources.length) + rowCenter(toIndex, targets.length)) / 2,
  })),
);

export function RouteMap() {
  const [from, setFrom] = useState<AgentId>("codex");
  const [to, setTo] = useState<AgentId>("claude-code");
  const source = agentById(from);
  const target = agentById(to);
  const same = from === to;
  const selected = routes.find((route) => route.from === from && route.to === to);
  const command = same
    ? target.same.includes("fork") ? `omni fork ${source.cli}:<session-id> --in ${target.cli}` : `omni resume ${source.cli}:<session-id>`
    : `omni resume ${source.cli}:<session-id> --in ${target.cli}`;
  const gate = target.signal.startsWith(">=") ? `${target.name} ${target.signal.replace(">= ", "")} or newer` : `${target.name}, any version with its official API`;
  const platforms = same ? target.capabilities.same_provider_resume : target.capabilities.cross_provider_import;

  return (
    <div className="routes">
      <div className="route-board">
        <fieldset className="route-stops from">
          <legend>From</legend>
          {sources.map((agent) => (
            <label className={`stop line-${agent.id}`} key={agent.id}>
              <input checked={from === agent.id} name="route-from" onChange={() => setFrom(agent.id)} type="radio" value={agent.id} />
              <span className="stop-name">{agent.short}</span>
              <AgentBullet agent={agent} size={26} />
            </label>
          ))}
        </fieldset>
        <div aria-hidden="true" className="route-lines">
          <svg preserveAspectRatio="none" viewBox={`0 0 ${width} 100`}>
            <defs>
              <linearGradient gradientUnits="userSpaceOnUse" id="route-change" x1="0" x2={width} y1="0" y2="0">
                <stop className={`line-${from}`} offset="0.5" style={{ stopColor: "var(--line)" }} />
                <stop className={`line-${to}`} offset="0.5" style={{ stopColor: "var(--line)" }} />
              </linearGradient>
            </defs>
            {routes.map((route) => (
              <path className={`route-line line-${route.from}${route.from === from ? " fan" : ""}${readOnlyIds.has(route.from) ? " read-only" : ""}`} d={route.path} key={route.key} vectorEffect="non-scaling-stroke" />
            ))}
            {selected ? (
              <g key={selected.key}>
                <path className="route-line casing" d={selected.path} vectorEffect="non-scaling-stroke" />
                <path className="route-line active" d={selected.path} pathLength={1} style={{ stroke: "url(#route-change)" }} vectorEffect="non-scaling-stroke" />
              </g>
            ) : null}
          </svg>
          {selected && !same ? <img alt="" className="interchange" src={`${basePath}/logo.svg`} style={{ top: `${selected.middle}%` }} /> : null}
        </div>
        <fieldset className="route-stops to">
          <legend>To</legend>
          {targets.map((agent) => (
            <label className={`stop line-${agent.id}`} key={agent.id}>
              <input checked={to === agent.id} name="route-to" onChange={() => setTo(agent.id)} type="radio" value={agent.id} />
              <AgentBullet agent={agent} size={26} />
              <span className="stop-name">{agent.short}</span>
            </label>
          ))}
        </fieldset>
      </div>

      <div aria-live="polite" className="sign route-sign">
        <p className="route-pair">
          <AgentBullet agent={source} size={40} />
          <span aria-hidden="true" className="route-arrow">→</span>
          <AgentBullet agent={target} size={40} />
        </p>
        <h3>{same ? `${source.name}, same agent` : `${source.name} to ${target.name}`}</h3>
        <dl>
          <div><dt>Route</dt><dd>{same ? `${target.same}, through ${target.name}'s own commands` : target.cross}</dd></div>
          <div><dt>Needs</dt><dd>{same ? `${target.name} installed` : gate}</dd></div>
          <div><dt>Runs on</dt><dd>{platformScope(platforms)}</dd></div>
        </dl>
        {same ? null : (
          <ul className="route-carry" role="list">
            <li className="stays">Messages, in order</li>
            <li className="stays">Tool calls and results, as history</li>
            <li className="leaves">Hidden reasoning</li>
            <li className="leaves">Approvals and permission state</li>
          </ul>
        )}
        <div className="sign-command">
          <Command text={command} />
          <CopyCommand command={command} key={command} label="Copy route command" />
        </div>
        <p className="sign-note">{same
          ? "Resuming in place keeps appending to the same session. A fork starts a new one."
          : readOnlyIds.has(from)
            ? `${source.name} is a read-only source: omni never writes to it.`
            : "The original session is not changed. If the import cannot run, omni asks what to do."}</p>
      </div>
    </div>
  );
}
