"use client";

import { useId, useState, type KeyboardEvent, type ReactNode } from "react";

import { CopyInstallCommand } from "./copy-install-command";

const windowsSetupUrl = "https://github.com/bvolpato/omnisession#install";

const installers = {
  unix: {
    command: "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh",
    label: "macOS / Linux",
    prompt: "$",
    hint: "Verifies the release checksum, installs omni, and adds provider shims.",
  },
  windows: {
    command: "irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex",
    label: "Windows x86-64 preview",
    prompt: "PS>",
    hint: "Native Windows preview. Provider fidelity remains provisional.",
  },
} as const;

type Platform = keyof typeof installers;
const platforms = Object.keys(installers) as Platform[];

const detailedNotes: Record<Platform, ReactNode> = {
  unix: (
    <>
      Verifies the release checksum, installs <code>omni</code>, and adds provider shims. Inside WSL, use this installer too.
    </>
  ),
  windows: (
    <>
      Native Windows <span className="nowrap">x86-64</span> preview. The installer verifies the release checksum and installs <code>omni</code> only; provider aliases are opt-in
      with <code>omni shim install</code>. Rerunning the installer upgrades omni and relinks existing aliases. Restart your shell after PATH
      changes. Provider fidelity remains provisional. <a href={windowsSetupUrl}>Windows setup ↗</a>
    </>
  ),
};

export function InstallCommand({ variant = "full" }: { variant?: "compact" | "full" }) {
  const [platform, setPlatform] = useState<Platform>("unix");
  const baseId = useId();
  const installer = installers[platform];
  const tabId = (id: Platform) => `${baseId}-tab-${id}`;
  const panelId = `${baseId}-panel`;

  function selectFromKeyboard(event: KeyboardEvent<HTMLButtonElement>, current: Platform) {
    const currentIndex = platforms.indexOf(current);
    const nextIndex = event.key === "ArrowRight"
      ? (currentIndex + 1) % platforms.length
      : event.key === "ArrowLeft"
        ? (currentIndex - 1 + platforms.length) % platforms.length
        : event.key === "Home"
          ? 0
          : event.key === "End"
            ? platforms.length - 1
            : null;

    if (nextIndex === null) return;
    event.preventDefault();
    const next = platforms[nextIndex];
    setPlatform(next);
    document.getElementById(tabId(next))?.focus();
  }

  return (
    <div className={`install-block install-${variant}`}>
      <div aria-label="Installation platform" className="install-tabs" role="tablist">
        {platforms.map((id) => (
          <button
            aria-controls={panelId}
            aria-selected={platform === id}
            id={tabId(id)}
            key={id}
            onClick={() => setPlatform(id)}
            onKeyDown={(event) => selectFromKeyboard(event, id)}
            role="tab"
            tabIndex={platform === id ? 0 : -1}
            type="button"
          >
            {installers[id].label}
          </button>
        ))}
      </div>
      <div aria-labelledby={tabId(platform)} className="install-command" id={panelId} role="tabpanel">
        <span aria-hidden="true" className="install-prompt">{installer.prompt}</span>
        <code>{installer.command}</code>
        <CopyInstallCommand command={installer.command} key={platform} />
      </div>
      <p className="install-note">{variant === "full" ? detailedNotes[platform] : installer.hint}</p>
    </div>
  );
}
