"use client";

import { useId, useState, type KeyboardEvent, type ReactNode } from "react";

import { CopyInstallCommand } from "./copy-install-command";

const windowsSetupUrl = "https://github.com/bvolpato/omnisession#install";

const installers = {
  unix: {
    command: "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh",
    label: "macOS / Linux",
    prompt: "$",
    hint: "Verifies the release checksum, installs omni, and leaves provider commands unchanged.",
  },
  windows: {
    command: "irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex",
    label: "Windows x86-64 preview",
    prompt: "PS>",
    hint: "Verifies the release checksum and installs omni. Provider commands remain native.",
  },
} as const;

type Platform = keyof typeof installers;
const platforms = Object.keys(installers) as Platform[];

const detailedNotes: Record<Platform, ReactNode> = {
  unix: (
    <>
      Verifies the release checksum and installs <code>omni</code>; fresh installs leave shell profiles unchanged and keep provider commands native. Add its install directory to <code>PATH</code> if needed. If OmniSession updated itself from 0.8.x, rerun this installer once; the in-app updater replaces only <code>omni</code>. The installer removes old OmniSession aliases and the exact generated PATH block; <code>OMNI_NO_MODIFY_PATH=1</code> leaves profiles untouched, so the old block may remain. Inside WSL, use this installer too.
    </>
  ),
  windows: (
    <>
      Native Windows <span className="nowrap">x86-64</span> preview. The installer verifies the release checksum, installs <code>omni</code>, and adds its install directory to <code>PATH</code>; provider commands remain native. If OmniSession updated itself from 0.8.x, rerun this installer once to remove aliases from earlier versions; the in-app updater replaces only <code>omni</code>. Restart your shell after PATH
      changes. <a href={windowsSetupUrl}>Windows setup ↗</a>
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
