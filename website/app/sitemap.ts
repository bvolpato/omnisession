import type { MetadataRoute } from "next";

const siteUrl = "https://bvolpato.github.io/omnisession";

export const dynamic = "force-static";

export default function sitemap(): MetadataRoute.Sitemap {
  return [{ url: `${siteUrl}/` }];
}
