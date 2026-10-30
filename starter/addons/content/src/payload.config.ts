// The content switch: Payload 3 (MIT). Collections, blocks and plugins the site's editors use at /admin.
// Change a collection, then `pnpm payload migrate:create <name>` to write its migration in src/migrations.
import path from "node:path";
import { fileURLToPath } from "node:url";
import { postgresAdapter } from "@payloadcms/db-postgres";
import { formBuilderPlugin } from "@payloadcms/plugin-form-builder";
import { redirectsPlugin } from "@payloadcms/plugin-redirects";
import { searchPlugin } from "@payloadcms/plugin-search";
import { seoPlugin } from "@payloadcms/plugin-seo";
import { lexicalEditor } from "@payloadcms/richtext-lexical";
import { vercelBlobStorage } from "@payloadcms/storage-vercel-blob";
import { buildConfig, type Block, type CollectionConfig } from "payload";
import { dbMode } from "@/lib/db";
import {
  contentDatabaseUrl,
  doorStrategy,
  draftsOnlyForEditors,
  draftsOrPublishers,
  editors,
  publishedOrEditors,
  publishers,
} from "@/lib/content";
import { migrations } from "./migrations";

const dirname = path.dirname(fileURLToPath(import.meta.url));
const sample = dbMode() === "sample";

const hero: Block = {
  slug: "hero",
  fields: [
    { name: "headline", type: "text", required: true },
    { name: "subheading", type: "textarea" },
    { name: "ctaLabel", type: "text" },
    { name: "ctaUrl", type: "text" },
  ],
};

const features: Block = {
  slug: "features",
  fields: [
    { name: "heading", type: "text" },
    {
      name: "items",
      type: "array",
      fields: [
        { name: "title", type: "text", required: true },
        { name: "body", type: "textarea" },
      ],
    },
  ],
};

const cta: Block = {
  slug: "cta",
  fields: [
    { name: "headline", type: "text", required: true },
    { name: "label", type: "text", required: true },
    { name: "url", type: "text", required: true },
  ],
};

const richText: Block = {
  slug: "richText",
  fields: [{ name: "content", type: "richText" }],
};

const slug = { name: "slug", type: "text", required: true, unique: true, index: true } as const;

// Drafts are on; editors save drafts, publishers publish (src/lib/content.ts).
const drafted: Pick<CollectionConfig, "access" | "hooks" | "versions"> = {
  access: { read: publishedOrEditors, create: draftsOrPublishers, update: draftsOrPublishers, delete: publishers },
  hooks: { beforeChange: [draftsOnlyForEditors] },
  versions: { drafts: true },
};

const Pages: CollectionConfig = {
  slug: "pages",
  admin: { useAsTitle: "title", preview: (doc) => `/${doc.slug === "home" ? "" : doc.slug}?draft=1` },
  ...drafted,
  fields: [
    { name: "title", type: "text", required: true },
    slug,
    { name: "layout", type: "blocks", blocks: [hero, features, cta, richText] },
  ],
};

const Posts: CollectionConfig = {
  slug: "posts",
  admin: { useAsTitle: "title" },
  ...drafted,
  fields: [
    { name: "title", type: "text", required: true },
    slug,
    { name: "publishedAt", type: "date" },
    { name: "content", type: "richText" },
  ],
};

const Media: CollectionConfig = {
  slug: "media",
  access: { read: () => true, create: editors, update: publishers, delete: publishers },
  // Local disk until the app has Vercel Blob (BLOB_READ_WRITE_TOKEN).
  // Images and PDFs only: an uploaded SVG or HTML file could run script on the site.
  upload: {
    staticDir: path.resolve(dirname, "../media"),
    mimeTypes: ["image/png", "image/jpeg", "image/webp", "image/gif", "application/pdf"],
  },
  fields: [{ name: "alt", type: "text", required: true }],
};

// One row per door viewer, made on their first visit to /admin. No passwords.
const Users: CollectionConfig = {
  slug: "users",
  admin: { useAsTitle: "name" },
  auth: { disableLocalStrategy: true, strategies: [doorStrategy] },
  access: { read: editors, create: () => false, update: () => false, delete: () => false },
  fields: [
    { name: "sub", type: "text", required: true, unique: true, index: true },
    { name: "name", type: "text" },
    { name: "email", type: "text" },
  ],
};

export default buildConfig({
  secret: process.env.LIGHTBULB_COOKIE_SECRET || (sample ? "sample-data-only" : ""),
  admin: { user: "users", importMap: { baseDir: dirname } },
  collections: [Pages, Posts, Media, Users],
  editor: lexicalEditor(),
  graphQL: { disable: true },
  typescript: { outputFile: path.resolve(dirname, "payload-types.ts") },
  db: postgresAdapter({
    pool: { connectionString: await contentDatabaseUrl() },
    // Sample data follows the collections as you edit them; the live database only changes by migration.
    push: sample,
    // PGlite is one session: a transaction would stall every other query until it ends.
    transactionOptions: sample ? false : undefined,
    prodMigrations: migrations,
  }),
  plugins: [
    seoPlugin({ collections: ["pages", "posts"], uploadsCollection: "media", generateTitle: ({ doc }) => doc.title }),
    redirectsPlugin({
      collections: ["pages", "posts"],
      overrides: { access: { read: () => true, create: publishers, update: publishers, delete: publishers } },
    }),
    formBuilderPlugin({
      fields: { payment: false },
      formOverrides: { access: { read: () => true, create: publishers, update: publishers, delete: publishers } },
      formSubmissionOverrides: { access: { update: publishers, delete: publishers } },
    }),
    searchPlugin({
      collections: ["pages", "posts"],
      searchOverrides: { access: { update: publishers, delete: publishers } },
    }),
    vercelBlobStorage({
      enabled: !!process.env.BLOB_READ_WRITE_TOKEN,
      collections: { media: true },
      token: process.env.BLOB_READ_WRITE_TOKEN,
    }),
  ],
  // Sample data starts with a published home page.
  async onInit(payload) {
    if (!sample || (await payload.count({ collection: "pages" })).totalDocs) return;
    await payload.create({
      collection: "pages",
      data: {
        title: "Home",
        slug: "home",
        _status: "published",
        layout: [{ blockType: "hero", headline: "{{name}}", subheading: "Edit this page at /admin." }],
      },
    });
  },
});
