import { ContentPage, contentMetadata } from "@/components/content/page";

type Props = { searchParams: Promise<Record<string, string | string[] | undefined>> };

// The home page is the page whose slug is `home`, edited at /admin.
export const generateMetadata = () => contentMetadata("pages", "home");

export default function Home({ searchParams }: Props) {
  return <ContentPage collection="pages" slug="home" searchParams={searchParams} />;
}
