"use client";

import { useState } from "react";

type CopyState = "idle" | "copied" | "failed";

async function copyText(value: string) {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(value);
      return;
    } catch {
      // Fall through for browsers that expose Clipboard API without granting access.
    }
  }

  const textarea = document.createElement("textarea");
  textarea.value = value;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();
  if (!copied) {
    throw new Error("copy failed");
  }
}

export function CopyInstallCommand({ command }: { command: string }) {
  const [state, setState] = useState<CopyState>("idle");

  async function copy() {
    try {
      await copyText(command);
      setState("copied");
    } catch {
      setState("failed");
    }
  }

  const label = state === "copied" ? "Copied" : state === "failed" ? "Try again" : "Copy";

  return (
    <button
      aria-label="Copy install command"
      aria-live="polite"
      className="copy-command"
      onClick={copy}
      type="button"
    >
      <svg aria-hidden="true" viewBox="0 0 16 16">
        <rect x="5" y="5" width="8" height="8" rx="1" />
        <path d="M3 10H2.8A.8.8 0 0 1 2 9.2V2.8A.8.8 0 0 1 2.8 2h6.4a.8.8 0 0 1 .8.8V3" />
      </svg>
      {label}
    </button>
  );
}
