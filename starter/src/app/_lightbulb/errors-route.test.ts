import { expect, it, vi } from "vitest";
import { POST } from "../api/%5Flightbulb/errors/route";

it("logs each client error with the [lightbulb:client] prefix the error reader looks for", async () => {
  const log = vi.spyOn(console, "error").mockImplementation(() => {});
  const res = await POST(
    new Request("https://demo-app.example.com/api/_lightbulb/errors", {
      method: "POST",
      body: JSON.stringify({ page: "/x\nforged", args: ["boom"] }),
    }),
  );
  expect(res.status).toBe(204);
  expect(log).toHaveBeenCalledWith("[lightbulb:client]", JSON.stringify("/x\nforged"), JSON.stringify(["boom"]));
  log.mockRestore();
});
