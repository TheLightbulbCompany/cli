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

