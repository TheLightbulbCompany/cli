import { dbMode } from "@/lib/db";

/** On every page of a preview that runs on sample data; never on the live app. */
export function SampleBanner() {
  if (dbMode() !== "sample") return null;
  return (
    <div role="status" className="bg-muted px-4 py-2 text-center text-sm text-muted-foreground">
      Preview with sample data — nothing you enter here is kept.
    </div>
  );
}
