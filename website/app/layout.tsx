import type { Metadata, Viewport } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import "./globals.css";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const siteUrl = "https://bvolpato.github.io/omnisession/";
const title = "OmniSession | Continue any session in any agent";
const description =
  "Search local coding-agent sessions from one picker and continue them in Claude Code, Codex, OpenCode, Grok, Hermes, Pi, Cursor, or Antigravity. Local-first and MIT licensed.";
const socialImage = {
  url: `${siteUrl}og-image.png`,
  width: 1200,
  height: 630,
  alt: "OmniSession: continue any session in any agent",
  type: "image/png",
};

const geistSans = Geist({ subsets: ["latin"], variable: "--font-geist-sans" });
const geistMono = Geist_Mono({ subsets: ["latin"], variable: "--font-geist-mono" });

export const metadata: Metadata = {
  metadataBase: new URL(siteUrl),
  alternates: { canonical: "./" },
  title,
  description,
  applicationName: "OmniSession",
  icons: { icon: `${basePath}/favicon.svg`, shortcut: `${basePath}/favicon.svg` },
  openGraph: {
    title,
    description,
    type: "website",
    url: siteUrl,
    siteName: "OmniSession",
    locale: "en_US",
    images: [socialImage],
  },
  twitter: {
    card: "summary_large_image",
    title,
    description,
    images: [{ url: socialImage.url, alt: socialImage.alt }],
  },
};

export const viewport: Viewport = {
  colorScheme: "dark light",
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#080b15" },
    { media: "(prefers-color-scheme: light)", color: "#f6f7fb" },
  ],
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <html className={`${geistSans.variable} ${geistMono.variable}`} lang="en">
      <body>{children}</body>
    </html>
  );
}
