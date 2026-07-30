import type { Metadata } from "next";
import "./globals.css";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

export const metadata: Metadata = {
  metadataBase: new URL("https://bvolpato.github.io/"),
  title: "OmniSession | Open local coding sessions in another agent",
  description:
    "Filter local coding sessions and continue them in Claude Code, Codex, OpenCode, Grok, Cursor, and other agents.",
  icons: {
    icon: `${basePath}/favicon.svg`,
    shortcut: `${basePath}/favicon.svg`,
  },
  openGraph: {
    title: "OmniSession | Open local coding sessions in another agent",
    description:
      "Run omni, filter local sessions, and choose another installed coding agent.",
    type: "website",
    url: "https://bvolpato.github.io/omnisession/",
    siteName: "OmniSession",
  },
  twitter: {
    card: "summary",
    title: "OmniSession | Open local coding sessions in another agent",
    description:
      "Run omni, filter local sessions, and choose another installed coding agent.",
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
