import { expect, it } from "vitest";
import { capArgs } from "./caps";

it("caps a report at 20 values, 8 KB each and 64 KB in all", () => {
  expect(capArgs(Array(30).fill("x"))).toHaveLength(20);
  const big = capArgs(["é".repeat(10_000)])[0];
  expect(new TextEncoder().encode(big).length).toBeLessThanOrEqual(8 * 1024);
  expect(big.endsWith("[truncated]")).toBe(true);
  expect(capArgs(Array(20).fill("y".repeat(8 * 1024))).length).toBe(8);
});
