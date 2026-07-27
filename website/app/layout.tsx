import type { Metadata } from "next";
import "./globals.css";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

export const metadata: Metadata = {
  metadataBase: new URL("https://bvolpato.github.io/"),
  title: "OmniSession | Switch agents. Keep the thread.",
  description:
    "Local-first session fabric for moving coding work across Claude, Codex, OpenCode, Grok, and Cursor.",
  icons: {
    icon: `${basePath}/favicon.svg`,
    shortcut: `${basePath}/favicon.svg`,
  },
  openGraph: {
    title: "OmniSession | Switch agents. Keep the thread.",
    description:
      "Move coding work between agents without losing task lineage or repository context.",
    type: "website",
    url: "https://bvolpato.github.io/omnisession/",
    siteName: "OmniSession",
    images: [
      {
        url: `${basePath}/og.png`,
        width: 1536,
        height: 1024,
        alt: "OmniSession. Switch agents. Keep the thread.",
      },
    ],
  },
  twitter: {
    card: "summary_large_image",
    title: "OmniSession | Switch agents. Keep the thread.",
    description:
      "Move coding work between agents without losing task lineage or repository context.",
    images: [`${basePath}/og.png`],
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
