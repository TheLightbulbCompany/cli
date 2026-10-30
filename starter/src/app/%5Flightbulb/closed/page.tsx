// Where the door sends people who may not open this app.
// TODO(Task 9): say who can open it and how to ask.
export default function Closed() {
  return (
    <main className="flex min-h-screen items-center justify-center p-8">
      <p className="text-muted-foreground">This app is closed to you. Ask in its Lightbulb room.</p>
    </main>
  );
}
