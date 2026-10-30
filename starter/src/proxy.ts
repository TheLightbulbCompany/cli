import { NextResponse } from "next/server";

// The request gate. Every request will pass the door here.
// TODO(Task 9): verify the Lightbulb session cookie, send closed visitors to /_lightbulb/closed,
// and set the frame-ancestors header. Until then every request passes through.
export function proxy() {
  return NextResponse.next();
}
