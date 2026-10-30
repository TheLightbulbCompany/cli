import { SignIn } from "../sign-in";

/** An invited person's one-time link: `/_lightbulb/invite#t=<token>`. */
export default function Invite() {
  return <SignIn invite />;
}
