"use client";

import { useState, type KeyboardEvent } from "react";

import { CopyInstallCommand } from "./copy-install-command";

const installers = {
  unix: {
    command: "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh",
    label: "Linux / macOS",
    prompt: "$",
  },
  windows: {
    command: "irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex",
    label: "Windows x86-64 Preview",
    prompt: "PS>",
  },
} as const;

type Platform = keyof typeof installers;
const platforms = Object.keys(installers) as Platform[];

export function InstallCommand() {
  const [platform, setPlatform] = useState<Platform>("unix");
  const installer = installers[platform];

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
    document.getElementById(`install-tab-${next}`)?.focus();
  }

  return (
    <>
      <div aria-label="Installation platform" className="install-tabs" role="tablist">
        {platforms.map((id) => (
          <button
            aria-controls="install-command-panel"
            aria-selected={platform === id}
            className={platform === id ? "active" : undefined}
            id={`install-tab-${id}`}
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
      <div
        aria-labelledby={`install-tab-${platform}`}
        className="install-command"
        id="install-command-panel"
        role="tabpanel"
      >
        <span>{installer.prompt}</span>
        <code>{installer.command}</code>
        <CopyInstallCommand command={installer.command} key={platform} />
      </div>
      <p className="install-platform-note">
        {platform === "unix"
          ? "Default install for Linux and macOS, including Linux inside WSL."
          : <>
              Native Windows preview. Binary installer is CI-tested; provider aliases are opt-in. Uninstall aliases before upgrades, reinstall afterward, then restart shell. Provider fidelity remains provisional. <a href="https://github.com/bvolpato/omnisession#install">Windows setup ↗</a>
            </>}
      </p>
    </>
  );
}
