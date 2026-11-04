import { expect, it } from "vitest";
import { lint } from "./design-lint.mjs";

it("flags raw colours and arbitrary values, not tokens or variants", () => {
  expect(lint('<div className="bg-[#fff] text-primary" />')).toHaveLength(2);
  expect(lint("color: rgb(0 0 0);")).toHaveLength(1);
  expect(lint('<a href="#top" className="w-[300px]" />')).toEqual([[1, "arbitrary Tailwind value; use the scale or a token"]]);
  expect(lint('<div className="data-[state=open]:bg-accent p-4" />')).toEqual([]);
});

it("flags native dialogs, not methods or names that contain the word", () => {
  for (const bad of ['if (!confirm("Delete?")) return;', 'window.confirm("x")', 'alert("x")', "prompt('x')"]) {
    expect(lint(bad)).toHaveLength(1);
  }
  for (const ok of ["onConfirm()", "dialog.confirm()", "const confirmed = true;", "setAlert(x)"]) expect(lint(ok)).toEqual([]);
});


it("flags promised emails only when the app can't send one", () => {
  for (const bad of ["We'll send the details to {email}.", "You will receive an email shortly", "Check your inbox", "A confirmation email is on its way"]) {
    expect(lint(bad)).toHaveLength(1);
    expect(lint(bad, { canEmail: true })).toEqual([]);
  }
  for (const ok of ["Email", "Send a note", "We'll be in touch at the event", "your email address"]) expect(lint(ok)).toEqual([]);
});
