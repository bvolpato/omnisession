"use client";

import { useState } from "react";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const demoPath = `${basePath}/demo/fork-session`;

export function ForkDemo() {
  const [playing, setPlaying] = useState(false);
  const extension = playing ? "gif" : "png";

  return (
    <figure className="fork-demo" aria-labelledby="fork-demo-caption">
      <picture key={extension}>
        <source media="(prefers-color-scheme: light)" srcSet={`${demoPath}_light.${extension}`} />
        <img
          alt="Synthetic demo: Codex implements a rate limiter, running omni opens a picker to select that session and Claude Code, and Claude Code writes the README with the visible history. The original Codex session stays intact."
          height={780}
          loading="lazy"
          src={`${demoPath}_dark.${extension}`}
          width={1200}
        />
      </picture>
      <figcaption id="fork-demo-caption">
        <div className="fork-demo-copy">
          <strong>Code in Codex. Docs in Claude.</strong>
          <span>Run omni. Pick the Codex session, then Claude Code. Synthetic sessions and illustrative responses; the original stays intact.</span>
        </div>
        <div className="fork-demo-actions">
          <button
            aria-pressed={playing}
            className="button button-primary button-small"
            onClick={() => setPlaying(!playing)}
            type="button"
          >
            <span aria-hidden="true">{playing ? "■" : "▶"}</span>
            {playing ? "Stop demo" : "Play demo"}
          </button>
          <a className="button button-ghost button-small" href={`${demoPath}_dark.gif`} rel="noopener" target="_blank">Open GIF <span aria-hidden="true">↗</span></a>
        </div>
      </figcaption>
    </figure>
  );
}
