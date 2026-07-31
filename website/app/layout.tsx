import type { Metadata } from "next";
import { Inter, Space_Grotesk, Space_Mono } from "next/font/google";
import "./globals.css";

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

const spaceGrotesk = Space_Grotesk({ subsets: ["latin"], variable: "--font-space-grotesk" });
const inter = Inter({ subsets: ["latin"], variable: "--font-inter" });
const spaceMono = Space_Mono({ subsets: ["latin"], weight: ["400", "700"], variable: "--font-space-mono" });

export const metadata: Metadata = {
  metadataBase: new URL("https://bvolpato.github.io/"),
  title: "OmniSession | Continue coding sessions across agents",
  description: "Search local coding-agent sessions and continue them in another installed agent.",
  icons: { icon: `${basePath}/favicon.svg`, shortcut: `${basePath}/favicon.svg` },
  openGraph: {
    title: "OmniSession | Continue coding sessions across agents",
    description: "Search local coding-agent sessions and continue them in another installed agent.",
    type: "website",
    url: "https://bvolpato.github.io/omnisession/",
    siteName: "OmniSession",
  },
  twitter: {
    card: "summary",
    title: "OmniSession | Continue coding sessions across agents",
    description: "Search local coding-agent sessions and continue them in another installed agent.",
  },
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return <html lang="en" className={`${spaceGrotesk.variable} ${inter.variable} ${spaceMono.variable}`}><body>{children}</body></html>;
}
