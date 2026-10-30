import { doorNames } from "../door-page";
import { SignIn } from "../sign-in";

// Names come from the app's env at request time, not at build time.
export const dynamic = "force-dynamic";

/** Opened from Canvas: `/_lightbulb/session#token=<pass>`. */
export default function Session() {
  return <SignIn names={doorNames()} />;
}
