import { Button } from "@/components/ui/button";

/** What the door page can name: from the env Lightbulb sets on the app. Missing names are dropped. */
export type DoorNames = { app?: string; workspace?: string; canvasUrl: string };

/** The app's and workspace's names and its Canvas link, from `LIGHTBULB_APP_NAME`, `LIGHTBULB_WORKSPACE_NAME` and `LIGHTBULB_APP_ID`. */
export function doorNames(env: Record<string, string | undefined> = process.env): DoorNames {
  const id = env.LIGHTBULB_APP_ID;
  return {
    app: env.LIGHTBULB_APP_NAME || undefined,
    workspace: env.LIGHTBULB_WORKSPACE_NAME || undefined,
    canvasUrl: `https://thelightbulb.company/chat?panel=canvas${id ? `&app=${encodeURIComponent(id)}` : ""}`,
  };
}

/** Another button beside "Open Canvas" (Continue, Try again). */
export type DoorAction = { label: string; onClick: () => unknown };

/** The page a person sees at the door: what this is, one sentence, and how to get in. */
export function DoorPage({
  names,
  message,
  action,
  busy,
}: {
  names: DoorNames;
  message: string;
  action?: DoorAction;
  busy?: boolean;
}) {
  return (
    <main className="flex min-h-screen items-center justify-center p-8">
      <div className="flex max-w-md flex-col gap-4 text-center">
        {names.workspace && <p className="text-sm text-muted-foreground">{names.workspace}</p>}
        <h1 className="text-2xl font-semibold">
          {names.app ? `Open ${names.app} from Lightbulb` : "Open this app from Lightbulb"}
        </h1>
        <p className="text-muted-foreground">{message}</p>
        {!busy && (
          <div className="flex justify-center gap-2">
            {action && (
              <Button variant="outline" onClick={action.onClick}>
                {action.label}
              </Button>
            )}
            <Button asChild>
              <a href={names.canvasUrl}>Open Canvas</a>
            </Button>
          </div>
        )}
        {!busy && <p className="text-sm text-muted-foreground">Invited by email? Use the link in your invitation.</p>}
      </div>
    </main>
  );
}
