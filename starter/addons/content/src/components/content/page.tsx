import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { RichText } from "@payloadcms/richtext-lexical/react";
import { buttonVariants } from "@/components/ui/button";
import { EditLink } from "@/components/lightbulb/edit-overlay";
import { cms, pagePath, siteUrl } from "@/lib/content";
import { viewer } from "@/lib/door";
import type { Page, Post } from "@/payload-types";
import type { Where } from "payload";

type Routed = "pages" | "posts";
type SearchParams = Promise<Record<string, string | string[] | undefined>>;

function pathOf(collection: Routed, slug: string) {
  return collection === "pages" ? pagePath(slug) : `/posts/${slug}`;
}

/** The published document at this slug, or the latest draft for a signed-in editor asking with `?draft=1`. */
async function find(collection: Routed, slug: string, draft = false): Promise<Page | Post | undefined> {
  const payload = await cms();
  const drafts = draft && !!(await viewer());
  const where: Where = drafts
    ? { slug: { equals: slug } }
    : { and: [{ slug: { equals: slug } }, { _status: { equals: "published" as const } }] };
  return (await payload.find({ collection, where, draft: drafts, limit: 1, depth: 1 })).docs[0];
}

/** Where a redirect (the redirects plugin) sends this path, if one does. */
async function redirectFor(path: string): Promise<string | undefined> {
  const payload = await cms();
  const r = (await payload.find({ collection: "redirects", where: { from: { equals: path } }, limit: 1, depth: 1 })).docs[0];
  const ref = r?.to?.reference;
  if (ref && typeof ref.value === "object") return pathOf(ref.relationTo, ref.value.slug);
  return r?.to?.url ?? undefined;
}

/** Title, description, canonical link and Open Graph from the document's SEO fields. */
export async function contentMetadata(collection: Routed, slug: string): Promise<Metadata> {
  const doc = await find(collection, slug);
  if (!doc) return {};
  const title = doc.meta?.title || doc.title;
  const description = doc.meta?.description ?? undefined;
  const url = siteUrl() + pathOf(collection, slug);
  const image = typeof doc.meta?.image === "object" ? doc.meta.image?.url : undefined;
  return {
    title,
    description,
    alternates: { canonical: url },
    openGraph: { title, description, url, images: image ? [image] : undefined },
  };
}

/** A page or post from the editor, with an Edit link per section for editors. */
export async function ContentPage({ collection, slug, searchParams }: { collection: Routed; slug: string; searchParams: SearchParams }) {
  const doc = await find(collection, slug, (await searchParams).draft === "1");
  if (!doc) {
    const to = await redirectFor(pathOf(collection, slug));
    if (to) redirect(to);
    notFound();
  }
  if (collection === "posts") {
    const post = doc as Post;
    return (
      <main className="relative mx-auto max-w-2xl p-8">
        <EditLink collection={collection} id={post.id} field="content" />
        <h1 className="mb-6 text-3xl font-semibold tracking-tight">{post.title}</h1>
        {post.content && <RichText data={post.content} className="space-y-4" />}
      </main>
    );
  }
  const page = doc as Page;
  return (
    <main>
      {page.layout?.map((block, i) => (
        <section key={block.id ?? i} className="relative mx-auto max-w-5xl px-8 py-16">
          <EditLink collection={collection} id={page.id} field={`layout.${i}`} />
          {block.blockType === "hero" && (
            <div className="flex flex-col items-start gap-4">
              <h1 className="text-4xl font-semibold tracking-tight">{block.headline}</h1>
              {block.subheading && <p className="text-lg text-muted-foreground">{block.subheading}</p>}
              {block.ctaLabel && block.ctaUrl && (
                <a href={block.ctaUrl} className={buttonVariants()}>
                  {block.ctaLabel}
                </a>
              )}
            </div>
          )}
          {block.blockType === "features" && (
            <div className="flex flex-col gap-8">
              {block.heading && <h2 className="text-2xl font-semibold tracking-tight">{block.heading}</h2>}
              <div className="grid gap-6 sm:grid-cols-3">
                {block.items?.map((item, j) => (
                  <div key={item.id ?? j} className="flex flex-col gap-2">
                    <h3 className="font-medium">{item.title}</h3>
                    {item.body && <p className="text-muted-foreground">{item.body}</p>}
                  </div>
                ))}
              </div>
            </div>
          )}
          {block.blockType === "cta" && (
            <div className="flex flex-col items-center gap-4 text-center">
              <h2 className="text-2xl font-semibold tracking-tight">{block.headline}</h2>
              <a href={block.url} className={buttonVariants()}>
                {block.label}
              </a>
            </div>
          )}
          {block.blockType === "richText" && block.content && <RichText data={block.content} className="space-y-4" />}
        </section>
      ))}
    </main>
  );
}
