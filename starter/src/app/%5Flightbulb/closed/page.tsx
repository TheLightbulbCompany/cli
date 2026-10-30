import { DoorPage, doorNames } from "../door-page";

// Names come from the app's env at request time, not at build time.
export const dynamic = "force-dynamic";

// Where the door sends people who are not signed in to this app (with status 401).
export default function Closed() {
  return <DoorPage names={doorNames()} message="Open this from Canvas in Lightbulb to get in." />;
}
