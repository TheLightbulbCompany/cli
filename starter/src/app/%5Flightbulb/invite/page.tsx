import { doorNames } from "../door-page";
import { SignIn } from "../sign-in";

// Names come from the app's env at request time, not at build time.
export const dynamic = "force-dynamic";

/** An invited person's one-time link: `/_lightbulb/invite#t=<token>`. Nothing is sent until they click Continue. */
export default function Invite() {
  return <SignIn invite names={doorNames()} />;
}
