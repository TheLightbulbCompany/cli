import { viewer } from "@/lib/door";

/** An "Edit" link from a section to its field in /admin, shown only to a signed-in editor. Put it in a `relative` section. */
export async function EditLink({ collection, id, field }: { collection: string; id: number | string; field: string }) {
  if (!(await viewer())) return null;
  return (
    <a
      href={`/admin/collections/${collection}/${id}#field-${field.replaceAll(".", "__")}`}
      className="absolute right-2 top-2 rounded-md border bg-background px-2 py-1 text-xs text-foreground shadow-sm"
    >
      Edit
    </a>
  );
}
