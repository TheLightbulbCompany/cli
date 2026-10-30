import { ContentPage, contentMetadata } from "@/components/content/page";

type Props = {
  params: Promise<{ slug: string }>;
  searchParams: Promise<Record<string, string | string[] | undefined>>;
};

export async function generateMetadata({ params }: Props) {
  return contentMetadata("pages", (await params).slug);
}

export default async function Page({ params, searchParams }: Props) {
  return <ContentPage collection="pages" slug={(await params).slug} searchParams={searchParams} />;
}
