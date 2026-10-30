import type { MetadataRoute } from "next";
import { cms, pagePath, siteUrl } from "@/lib/content";

// Published content changes without a deploy.
export const dynamic = "force-dynamic";

/** Published pages and posts only. */
export default async function sitemap(): Promise<MetadataRoute.Sitemap> {
  const payload = await cms();
  const query = { where: { _status: { equals: "published" as const } }, limit: 1000, depth: 0, pagination: false };
  const pages = await payload.find({ collection: "pages", ...query });
  const posts = await payload.find({ collection: "posts", ...query });
  return [
    ...pages.docs.map((p) => ({ url: siteUrl() + pagePath(p.slug), lastModified: p.updatedAt })),
    ...posts.docs.map((p) => ({ url: `${siteUrl()}/posts/${p.slug}`, lastModified: p.updatedAt })),
  ];
}
