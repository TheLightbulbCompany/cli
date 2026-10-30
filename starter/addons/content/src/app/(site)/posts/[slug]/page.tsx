import { ContentPage, contentMetadata } from "@/components/content/page";

type Props = {
  params: Promise<{ slug: string }>;
  searchParams: Promise<Record<string, string | string[] | undefined>>;
};

export async function generateMetadata({ params }: Props) {
  return contentMetadata("posts", (await params).slug);
}

export default async function Post({ params, searchParams }: Props) {
  return <ContentPage collection="posts" slug={(await params).slug} searchParams={searchParams} />;
}
