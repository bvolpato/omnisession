import type { Metadata } from "next";
import { Inter, Space_Grotesk, Space_Mono } from "next/font/google";
import "./globals.css";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
const siteUrl = "https://bvolpato.github.io/omnisession/";
const socialImage = `${siteUrl}session-browser.png`;

const spaceGrotesk = Space_Grotesk({ subsets: ["latin"], variable: "--font-space-grotesk" });
const inter = Inter({ subsets: ["latin"], variable: "--font-inter" });
const spaceMono = Space_Mono({ subsets: ["latin"], weight: ["400", "700"], variable: "--font-space-mono" });

export const metadata: Metadata = {
  metadataBase: new URL(siteUrl),
  alternates: { canonical: "./" },
  title: "OmniSession | Continue coding sessions across agents",
  description: "Search local coding-agent sessions and continue them in another installed agent.",
  icons: { icon: `${basePath}/favicon.svg`, shortcut: `${basePath}/favicon.svg` },
  openGraph: {
    title: "OmniSession | Continue coding sessions across agents",
    description: "Search local coding-agent sessions and continue them in another installed agent.",
    type: "website",
    url: siteUrl,
    siteName: "OmniSession",
    images: [{ url: socialImage, width: 1564, height: 620, alt: "OmniSession session browser", type: "image/png" }],
  },
  twitter: {
    card: "summary_large_image",
    title: "OmniSession | Continue coding sessions across agents",
    description: "Search local coding-agent sessions and continue them in another installed agent.",
    images: [socialImage],
  },
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return <html lang="en" className={`${spaceGrotesk.variable} ${inter.variable} ${spaceMono.variable}`}><body>{children}</body></html>;
}
