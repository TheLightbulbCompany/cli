# Accounts people make for themselves

Not a kind: use this with the kind's playbook when the request has its own sign-up. Without that
request, the door is the sign-in and you need none of this.

**Set `open_to: public` in `app.yaml`.** The door then lets everyone reach the app, so the app is
its own door: every page and route for signed-in people checks `currentAccount()` on the server.
Passwords, the session cookie and the check are in `src/lib/accounts.ts`; don't write your own.

**Done when:** sign-up lands signed in on the app's first page; signing out and back in works; the
same email twice shows a message; a signed-out visitor is sent to sign-in; two accounts can't see
each other's rows, checked by id on the API as well as in the list.

The table, then `pnpm db:generate`:

```ts
export const users = pgTable("users", {
  id: serial("id").primaryKey(),
  name: text("name").notNull(),
  email: text("email").notNull().unique(),
  passwordHash: text("password_hash").notNull(),
});
```

Sign-up, `src/app/api/auth/sign-up/route.ts`. The session is set in this same response:

```ts
import { z } from "zod";
import { accountCookie, hashPassword } from "@/lib/accounts";
import { getDb } from "@/lib/db";
import { users } from "@/lib/db/schema";

const input = z.object({ name: z.string().trim().min(1), email: z.email(), password: z.string().min(8) });

export async function POST(req: Request) {
  const parsed = input.safeParse(await req.json().catch(() => null));
  if (!parsed.success) return Response.json({ error: "Name, email and an 8-character password, please." }, { status: 400 });
  const { name, email, password } = parsed.data;
  const db = await getDb();
  const [user] = await db
    .insert(users)
    .values({ name, email: email.toLowerCase(), passwordHash: await hashPassword(password) })
    .onConflictDoNothing({ target: users.email })
    .returning({ id: users.id });
  if (!user) return Response.json({ error: "That email already has an account." }, { status: 409 });
  return Response.json({ id: user.id }, { status: 201, headers: { "set-cookie": await accountCookie(user.id) } });
}
```

Sign-in, `src/app/api/auth/sign-in/route.ts`, answers the same message for a wrong email and a wrong
password:

```ts
import { eq } from "drizzle-orm";
import { accountCookie, verifyPassword } from "@/lib/accounts";
import { getDb } from "@/lib/db";
import { users } from "@/lib/db/schema";

export async function POST(req: Request) {
  const { email, password } = (await req.json().catch(() => ({}))) as { email?: string; password?: string };
  const db = await getDb();
  const [user] = await db.select().from(users).where(eq(users.email, String(email).toLowerCase()));
  if (!user || !(await verifyPassword(String(password), user.passwordHash))) {
    return Response.json({ error: "Wrong email or password." }, { status: 401 });
  }
  return Response.json({ id: user.id }, { headers: { "set-cookie": await accountCookie(user.id) } });
}
```

Sign-out, `src/app/api/auth/sign-out/route.ts`:

```ts
import { SIGNED_OUT_COOKIE } from "@/lib/accounts";

export const POST = () => new Response(null, { status: 204, headers: { "set-cookie": SIGNED_OUT_COOKIE } });
```

After a sign-up or sign-in answers OK, the form goes on with `window.location.assign("/first-page")`,
so the next request carries the new cookie. If it answers an error, show `error` on the page.

Pages for signed-in people, `src/app/(signed-in)/layout.tsx`, and in every route handler
`if (!(await currentAccount())) return Response.json({ error: "Sign in." }, { status: 401 });`:

```tsx
import { redirect } from "next/navigation";
import { currentAccount } from "@/lib/accounts";

export default async function SignedIn({ children }: { children: React.ReactNode }) {
  if (!(await currentAccount())) redirect("/auth/sign-in");
  return <>{children}</>;
}
```

Every query for someone's rows has `where(eq(table.userId, account.id))`, and asking for another
person's row answers 404.
