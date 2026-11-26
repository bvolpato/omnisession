"use client";

import { useId, useState, type KeyboardEvent, type ReactNode } from "react";

import { CopyCommand } from "./copy-command";

const windowsSetupUrl = "https://github.com/bvolpato/omnisession/blob/main/docs/GUIDE.md#windows-x86-64-preview";

const installers = {
  unix: {
    command: "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh",
    label: "macOS / Linux",
    prompt: "$",
  },
  windows: {
    command: "irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex",
    label: "Windows x86-64 preview",
    prompt: "PS>",
  },
} as const;

type Platform = keyof typeof installers;
const platforms = Object.keys(installers) as Platform[];

const notes: Record<Platform, ReactNode> = {
  unix: (
    <>
      Verifies the release checksum, then installs <code>omni</code> into <code>~/.local/bin</code>. Fresh installs leave shell profiles unchanged and
      keep provider commands native, so add that directory to <code>PATH</code> if needed. Use this installer inside WSL too.
    </>
  ),
  windows: (
    <>
      Verifies the release checksum, installs <code>omni</code>, and adds its directory to <code>PATH</code>; provider commands remain native.
      Restart your shell afterwards. <a href={windowsSetupUrl}>Windows setup</a>
    </>
  ),
};

/** The compact block shows one command for the hero; the full block adds platform tabs and notes. */
export function InstallCommand({ variant = "full" }: { variant?: "compact" | "full" }) {
  const [platform, setPlatform] = useState<Platform>("unix");
  const baseId = useId();
  const installer = installers[variant === "compact" ? "unix" : platform];
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

    const next = nextIndex === null ? undefined : platforms[nextIndex];
    if (!next) return;
    event.preventDefault();
    setPlatform(next);
    document.getElementById(tabId(next))?.focus();
  }

  if (variant === "compact") {
    return (
      <div className="install-block install-compact">
        <div className="install-command">
          <span aria-hidden="true" className="install-prompt">{installer.prompt}</span>
          <code>{installer.command}</code>
          <CopyCommand command={installer.command} label="Copy install command" />
        </div>
      </div>
    );
  }

  return (
    <div className="install-block install-full">
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
        <CopyCommand command={installer.command} key={platform} label="Copy install command" />
      </div>
      <p className="install-note">{notes[platform]}</p>
    </div>
  );
}
