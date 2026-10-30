import { pgTable, serial, text, timestamp } from "drizzle-orm/pg-core";

// The app's tables. Rename or reshape `items` before the first `lightbulb app up`;
// after that the schema only grows (AGENTS.md).
export const items = pgTable("items", {
  id: serial("id").primaryKey(),
  name: text("name").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});
