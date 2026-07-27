import type { Metadata } from "next";
import "./globals.css";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

export const metadata: Metadata = {
  metadataBase: new URL("https://bvolpato.github.io/"),
  title: "OmniSession | Continue coding sessions across agents",
  description:
    "Find and resume coding sessions across Claude Code, Codex, OpenCode, Grok, and Cursor.",
  icons: {
    icon: `${basePath}/favicon.svg`,
    shortcut: `${basePath}/favicon.svg`,
  },
  openGraph: {
    title: "OmniSession | Continue coding sessions across agents",
    description:
      "Find local sessions, check transfer fidelity, and resume work in another coding agent.",
    type: "website",
    url: "https://bvolpato.github.io/omnisession/",
    siteName: "OmniSession",
  },
  twitter: {
    card: "summary_large_image",
    title: "OmniSession | Continue coding sessions across agents",
    description:
      "Find local sessions, check transfer fidelity, and resume work in another coding agent.",
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
