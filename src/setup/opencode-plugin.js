// Lightbulb: written by `lightbulb setup` and deleted by `lightbulb setup --remove`;
// edits here are overwritten. For each OpenCode session that is not a subagent's it runs
//   EXE hook opencode session-start   (a session is created)
//   EXE hook opencode post-tool-use   (after a tool call)
//   EXE hook opencode stop            (the session goes idle)
//   EXE hook opencode session-end     (the session is deleted)
// with {session_id, cwd, mail} on stdin, and puts the text a hook offers into the session.
// Before a session's first request it runs session-start once more, with mail: true, and adds
// what it offers (the memory block only, never mail) to the system prompt of each request.
// The offer leaves the mail waiting; only once the prompt is in does it run
//   EXE hook opencode ack             ({session_id, ids} on stdin)
// so a push that fails is offered again at the next step (a resend may repeat a message,
// none is lost). A hook that fails or takes over 10 s is ignored: OpenCode is never held up.
// A teammate's message can never move a session to another agent or model (plan to build):
// see the notes at `server` (OpenCode 1.x) and `setup` (OpenCode 2.x).
//
// One default export loads on both: OpenCode 2.x reads {id, setup}, 1.x reads {id, server}.
// https://github.com/anomalyco/opencode/blob/v2.1.4/packages/core/src/plugin/module.ts
//   (the 2.x schema: default {id, effect} or {id, setup}; other keys are not read)
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/opencode/src/plugin/shared.ts
//   (readV1Plugin: a default object with an id must have server())
import { spawn } from "node:child_process"

const EXE = __EXE__

// Runs one hook; resolves to what it prints ({text, ids} when it offers mail), or null.
const hook = (arg, id, cwd, mail, extra) =>
  new Promise((done) => {
    let out = ""
    try {
      const p = spawn(EXE, ["hook", "opencode", arg], { stdio: ["pipe", "pipe", "ignore"] })
      p.stdout.setEncoding("utf8")
      const timer = setTimeout(() => {
        try {
          p.kill()
        } catch {}
        done(null)
      }, 10000)
      p.stdout.on("data", (d) => (out += d))
      p.on("error", () => {
        clearTimeout(timer)
        done(null)
      })
      p.on("close", () => {
        clearTimeout(timer)
        try {
          done(out ? JSON.parse(out) : null)
        } catch {
          done(null)
        }
      })
      p.stdin.on("error", () => {})
      p.stdin.end(JSON.stringify({ session_id: id, cwd, mail, ...extra }))
    } catch {
      done(null)
    }
  })

// Sessions with an offer in flight: they are not offered mail again until it is acked.
const pushing = new Set()

// One step: the hook registers it; with `push` (text => whether it went in), and no offer in
// flight, it is offered the mail, and mail that went in is acked.
const step = async (arg, id, cwd, push, extra) => {
  if (!push || pushing.has(id)) return void (await hook(arg, id, cwd, false, extra))
  pushing.add(id)
  try {
    const offer = await hook(arg, id, cwd, true, extra)
    const ids = offer?.ids
    if (typeof offer?.text !== "string" || !Array.isArray(ids)) return
    let went = false
    try {
      went = await push(offer.text)
    } catch {}
    if (went) await hook("ack", id, cwd, false, { ids })
  } finally {
    pushing.delete(id)
  }
}

// Session id -> the block its start hook offered, or null: asked for once (a promise, so requests
// at once share it) and kept, since OpenCode builds the system prompt anew for each request. The
// start offers no mail, so there is nothing to ack: mail goes in at a tool call or a stop.
const starts = new Map()
const start = (id, cwd) => {
  if (!starts.has(id)) {
    starts.set(
      id,
      hook("session-start", id, cwd, true).then((offer) => (typeof offer?.text === "string" ? offer.text : null)),
    )
  }
  return starts.get(id)
}

// OpenCode 1.x, from v1.18.32:
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/sdk/js/src/gen/types.gen.ts
//   (session.created/deleted carry properties.info: Session {id, parentID?};
//    session.idle carries properties.sessionID)
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/plugin/src/index.ts
//   ("tool.execute.after" input: {tool, sessionID, callID, args};
//    "chat.message" input: {sessionID, agent?, model?, variant?}, output: {message: UserMessage},
//    UserMessage {agent, model: {providerID, modelID}} in types.gen.ts above)
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/opencode/src/session/prompt.ts
//   (a prompt with no agent runs the default agent; with noReply it is only saved)
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/opencode/src/session/llm/request.ts
//   ("experimental.chat.system.transform" input {sessionID?, model}, output {system: string[]}:
//    run as each request's system prompt is built)
// A 1.x prompt names its own agent and model, so mail goes in only with the session's own
// (seen in chat.message); until they are seen, `mail: false` leaves it for check_inbox.
const server = async ({ client, directory }) => {
  // Session id -> whether it is a subagent's (has a parentID).
  const child = new Map()
  // Session id -> {agent, model, variant?} of its latest user message.
  const who = new Map()
  const isChild = async (id) => {
    if (!child.has(id)) {
      try {
        const r = await client.session.get({ path: { id } })
        child.set(id, !!r?.data?.parentID)
      } catch {
        return false
      }
    }
    return child.get(id)
  }
  // noReply: the text joins the session without starting a reply (the running turn reads it).
  // The SDK answers an HTTP error as {error} rather than throwing.
  const say = (id, noReply) => {
    const w = who.get(id)
    if (!w) return undefined
    return async (text) => {
      const r = await client.session.prompt({
        path: { id },
        body: { parts: [{ type: "text", text }], ...w, ...(noReply ? { noReply } : {}) },
      })
      return !r?.error
    }
  }
  return {
    "chat.message": async (input, output) => {
      try {
        const m = output?.message
        if (!input?.sessionID || !m?.agent || !m?.model) return
        who.set(input.sessionID, {
          agent: m.agent,
          model: m.model,
          ...(input.variant ? { variant: input.variant } : {}),
        })
      } catch {}
    },
    event: async ({ event }) => {
      try {
        const props = event?.properties ?? {}
        if (event.type === "session.created" || event.type === "session.deleted") {
          const info = props.info ?? {}
          if (!info.id) return
          child.set(info.id, !!info.parentID)
          if (info.parentID) return
          const arg = event.type === "session.created" ? "session-start" : "session-end"
          await hook(arg, info.id, directory, false)
          if (event.type === "session.deleted") {
            child.delete(info.id)
            who.delete(info.id)
            starts.delete(info.id)
          }
        } else if (event.type === "session.idle") {
          const id = props.sessionID
          if (!id || (await isChild(id))) return
          // Starts a turn that is acked when it ends; not awaited, so the event is not held for
          // the whole reply.
          step("stop", id, directory, say(id, false)).catch(() => {})
        }
      } catch {}
    },
    "experimental.chat.system.transform": async (input, output) => {
      try {
        const id = input?.sessionID
        if (!id || (await isChild(id))) return
        const text = await start(id, directory)
        if (text) output.system.push(text)
      } catch {}
    },
    // Capture reads the tool and its args from the v1.18.32 plugin type
    // (packages/plugin/src/index.ts): "tool.execute.after"?: (input: { tool: string;
    // sessionID: string; callID: string; args: any }, output: {...}). OpenCode 2.x's
    // session.tool.success/failed events (v2.1.4 types.ts) carry no tool name or input, so
    // 2.x tool calls are not captured until a verified shape exists.
    "tool.execute.after": async (input) => {
      try {
        const id = input?.sessionID
        if (!id || (await isChild(id))) return
        await step("post-tool-use", id, directory, say(id, true), { tool: input.tool, args: input.args })
      } catch {}
    },
  }
}

// OpenCode 2.x, from v2.1.4:
// https://github.com/anomalyco/opencode/blob/v2.1.4/packages/plugin/src/promise/plugin.ts
//   (setup(ctx) may return a cleanup; ctx.event.subscribe, ctx.session.get/prompt)
// https://github.com/anomalyco/opencode/blob/v2.1.4/packages/client/src/promise/generated/types.ts
//   (events {type, data}: session.created data {sessionID, parentID?, location: {directory}};
//    session.idle, session.deleted data {sessionID}; session.tool.success / session.tool.failed
//    data {sessionID, ...}; session.get -> Session {id, parentID?, location: {directory}};
//    session.prompt input {sessionID, text, delivery?: "steer" | "queue", resume?})
// https://github.com/anomalyco/opencode/blob/v2.1.4/packages/core/src/session/session.ts
//   (Session.prompt admits the text to the session's inbox, by default delivery "steer": taken
//    in at the next step boundary of a running turn, and wakes the session when it is idle)
// https://github.com/anomalyco/opencode/blob/v2.1.4/packages/plugin/src/promise/session.ts
//   (ctx.session.hook("context", cb): cb gets {sessionID, agent, model, system: SystemPart[], ...}
//    before each of a session's requests, packages/core/src/session/model-request.ts;
//    SystemPart {type: "text", text}, packages/ai/src/schema/messages.ts)
// A 2.x prompt names no agent or model: it runs as the session's own (it is changed only by
// session.switchAgent / switchModel), so a teammate's message cannot change either.
const setup = async (ctx) => {
  const fallback = ctx?.location?.directory ?? process.cwd()
  // Session id -> {child, cwd}.
  const sessions = new Map()
  const known = async (id) => {
    if (!sessions.has(id)) {
      try {
        const s = await ctx.session.get({ sessionID: id })
        sessions.set(id, { child: !!s?.parentID, cwd: s?.location?.directory ?? fallback })
      } catch {
        return { child: false, cwd: fallback }
      }
    }
    return sessions.get(id)
  }
  // Whether it went in: a refused prompt throws.
  const say = (id) => async (text) => {
    await ctx.session.prompt({ sessionID: id, text, delivery: "steer" })
    return true
  }
  const on = async (e) => {
    const d = e?.data ?? {}
    const id = d.sessionID
    if (typeof id !== "string") return
    if (e.type === "session.created") {
      sessions.set(id, { child: !!d.parentID, cwd: d.location?.directory ?? fallback })
    }
    const s = await known(id)
    if (s.child) return
    switch (e.type) {
      case "session.created":
        return void (await hook("session-start", id, s.cwd, false))
      case "session.tool.success":
      case "session.tool.failed":
        return step("post-tool-use", id, s.cwd, say(id))
      case "session.idle":
        return step("stop", id, s.cwd, say(id))
      case "session.deleted":
        await hook("session-end", id, s.cwd, false)
        sessions.delete(id)
        starts.delete(id)
    }
  }
  let context
  try {
    context = await ctx.session.hook?.("context", async (e) => {
      try {
        const s = await known(e.sessionID)
        if (s.child) return
        const text = await start(e.sessionID, s.cwd)
        if (text) e.system.push({ type: "text", text })
      } catch {}
    })
  } catch {}
  const stop = new AbortController()
  ;(async () => {
    while (!stop.signal.aborted) {
      try {
        for await (const e of ctx.event.subscribe({ signal: stop.signal })) {
          await on(e).catch(() => {})
        }
      } catch {}
      if (!stop.signal.aborted) await new Promise((r) => setTimeout(r, 1000))
    }
  })()
  return async () => {
    stop.abort()
    await context?.dispose?.().catch(() => {})
  }
}

export default { id: "lightbulb", setup, server }
