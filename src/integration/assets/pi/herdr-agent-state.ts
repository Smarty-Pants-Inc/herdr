// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=pi
// HERDR_INTEGRATION_VERSION=10
// @ts-nocheck

import net from "node:net";
import path from "node:path";

const HERDR_ENV = process.env.HERDR_ENV;
const socketPath = process.env.HERDR_SOCKET_PATH;
const socketEndpoint =
  process.platform === "win32" && socketPath ? `\\\\.\\pipe\\${socketPath}` : socketPath;
const paneId = process.env.HERDR_PANE_ID;
const source = "herdr:pi";

const MAX_CHANNEL_FRAME = 64 * 1024;
const MAX_CHANNEL_LEDGER = 256;
const MAX_CHANNEL_INFLIGHT = 32;
const MAX_RECONNECT_ATTEMPTS = 2;
const RECENT_CHANNEL_EPOCHS = 2;
const MAX_CHANNEL_ID = 1024;
const RECONNECT_DELAY_MS = 250;
const REGISTRATION_TIMEOUT_MS = 2000;
const CHANNEL_REASONS = new Set([
  "no_session",
  "session_changed",
  "payload_mismatch",
  "shutting_down",
  "admission_refused",
  "unsupported",
]);

function enabled() {
  return HERDR_ENV === "1" && !!socketPath && !!paneId;
}

function sendRequestAttempt(request: unknown, timeoutMs: number): Promise<boolean> {
  if (!enabled()) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    let done = false;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const socket = net.createConnection(socketEndpoint!);
    const finish = (delivered: boolean) => {
      if (done) return;
      done = true;
      if (timeout) {
        clearTimeout(timeout);
      }
      socket.destroy();
      resolve(delivered);
    };

    socket.on("error", () => finish(false));
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", () => finish(true));
    socket.on("end", () => finish(false));
    timeout = setTimeout(() => finish(false), timeoutMs);
    timeout.unref?.();
  });
}

async function sendRequest(request: unknown): Promise<void> {
  if (await sendRequestAttempt(request, 500)) {
    return;
  }
  await sendRequestAttempt(request, 1500);
}

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  message?: string;
  seq: number;
};

let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;

function nextReportSeq(): number {
  // Wall-clock based, not load-time based: a Pi started later in this pane (a nested probe that
  // inherited HERDR_PANE_ID) must not leave a higher sequence that makes Herdr drop this
  // long-running session's later reports as stale (smarty-dev#509).
  reportSeq = Math.max(reportSeq + 1, Date.now() * 1000);
  return reportSeq;
}

function updateSessionRef(ctx: any): void {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath =
      typeof file === "string" &&
      (path.posix.isAbsolute(file) || path.win32.isAbsolute(file))
        ? file
        : undefined;
  } catch {
    currentAgentSessionPath = undefined;
  }

  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    currentAgentSessionId = typeof id === "string" && id.length > 0 ? id : undefined;
  } catch {
    currentAgentSessionId = undefined;
  }
}

function withSessionRef(params: Record<string, unknown>): Record<string, unknown> {
  if (currentAgentSessionPath) {
    return { ...params, agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { ...params, agent_session_id: currentAgentSessionId };
  }
  return params;
}

function currentSessionRef(): Record<string, unknown> | undefined {
  if (currentAgentSessionPath) {
    return { agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { agent_session_id: currentAgentSessionId };
  }
  return undefined;
}

function reportSession(sessionStartSource?: string): Promise<void> {
  const sessionRef = currentSessionRef();
  if (!sessionRef) {
    return Promise.resolve();
  }

  return sendRequest({
    id: `${source}:session:${Date.now()}:${Math.random().toString(36).slice(2)}`,
    method: "pane.report_agent_session",
    params: {
      pane_id: paneId,
      source,
      agent: "pi",
      seq: nextReportSeq(),
      session_start_source: sessionStartSource,
      ...sessionRef,
    },
  });
}

function sendState(state: AgentState, message?: string, seq = nextReportSeq()): Promise<void> {
  return sendRequest({
    id: `${source}:${Date.now()}:${Math.random().toString(36).slice(2)}`,
    method: "pane.report_agent",
    params: withSessionRef({
      pane_id: paneId,
      source,
      agent: "pi",
      state,
      message,
      seq,
    }),
  });
}

let sendInFlight = false;
let queuedState: QueuedState | undefined;

function queueState(state: AgentState, message?: string): void {
  queuedState = { state, message, seq: nextReportSeq() };
  if (!sendInFlight) {
    void drainStateQueue();
  }
}

async function drainStateQueue(): Promise<void> {
  if (sendInFlight) {
    return;
  }

  sendInFlight = true;
  try {
    while (queuedState) {
      const next = queuedState;
      queuedState = undefined;
      await sendState(next.state, next.message, next.seq);
    }
  } finally {
    sendInFlight = false;
    if (queuedState) {
      void drainStateQueue();
    }
  }
}

function isRecord(value: unknown): value is Record<string, any> {
  return typeof value === "object" && value !== null;
}

function channelReason(value: unknown): string | undefined {
  return typeof value === "string" && CHANNEL_REASONS.has(value) ? value : undefined;
}

function randomId(prefix: string): string {
  return `${source}:${prefix}:${Date.now()}:${Math.random().toString(36).slice(2)}`;
}

function validChannelId(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && Buffer.byteLength(value) <= MAX_CHANNEL_ID;
}

export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let blockedCount = 0;
  let blockedMessage: string | undefined;
  let lastState: AgentState | undefined;
  let lastMessage: string | undefined;
  let rootSession = false;

  // The receiver is deliberately owned by one session_start lifecycle. A new connection gets a
  // new registration and never replays a frame from the previous connection or epoch.
  let channelSocket: net.Socket | undefined;
  let channelGeneration = 0;
  let channelSessionGeneration: string | undefined;
  let channelEpoch: string | undefined;
  let channelReady = false;
  let channelClosing = false;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let registrationTimer: ReturnType<typeof setTimeout> | undefined;
  let reconnectAttempts = 0;
  let currentContext: any;
  // The server must issue a fresh random epoch, never reuse a retired one. Keep the current and
  // previous accepted epochs plus epochs with unresolved Pi calls; stale deliveries are also
  // fenced by their connection token and current epoch. This is bounded by 2 + 32, not by the
  // number of registrations over the extension's lifetime.
  const usedEpochs = new Set<string>();
  const recentEpochs: string[] = [];
  type LedgerEntry = { epoch: string; text: string; receipt?: Record<string, any>; duplicate: boolean };
  const ledger = new Map<string, LedgerEntry>();
  // Keep unresolved calls counted across reconnect/session replacement: an old callback must not
  // free a newer request's slot, and repeated replacement cannot create unlimited pending calls.
  const inflight = new Set<LedgerEntry>();

  function pruneEpochs() {
    const retained = new Set(recentEpochs);
    for (const entry of inflight) retained.add(entry.epoch);
    for (const epoch of usedEpochs) {
      if (!retained.has(epoch)) usedEpochs.delete(epoch);
    }
  }

  function clearReconnectTimer() {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = undefined;
    }
  }

  function clearRegistrationTimer() {
    if (registrationTimer) {
      clearTimeout(registrationTimer);
      registrationTimer = undefined;
    }
  }

  function closeChannel() {
    clearReconnectTimer();
    clearRegistrationTimer();
    channelGeneration += 1;
    channelClosing = true;
    const socket = channelSocket;
    channelSocket = undefined;
    channelReady = false;
    channelEpoch = undefined;
    channelSessionGeneration = undefined;
    ledger.clear();
    if (socket) {
      socket.destroy();
    }
  }

  function sendChannelFrame(socket: any, token: number, frame: Record<string, unknown>): boolean {
    if (
      token !== channelGeneration ||
      socket !== channelSocket ||
      !channelReady ||
      channelClosing ||
      !channelEpoch ||
      channelSessionGeneration !== currentContext?.userMessageSessionGeneration
    ) {
      return false;
    }
    try {
      const line = `${JSON.stringify(frame)}\n`;
      if (socket.destroyed || socket.writableLength + Buffer.byteLength(line) > MAX_CHANNEL_FRAME) {
        socket.destroy();
        return false;
      }
      socket.write(line);
      return true;
    } catch {
      socket.destroy();
      return false;
    }
  }

  function receiptForError(reason = "admission_refused", sessionGeneration = channelSessionGeneration) {
    return {
      status: "rejected",
      sessionGeneration,
      reason: channelReason(reason) ?? "admission_refused",
    };
  }

  function normalizeReceipt(value: unknown, expectedSessionGeneration: string): Record<string, any> | undefined {
    // Exceptions, void and malformed/mismatched receipts do not establish non-admission. Drop the
    // channel without an ACK so Herdr can report delivery_unknown; never synthesize rejection.
    if (
      !isRecord(value) ||
      value.sessionGeneration !== expectedSessionGeneration ||
      (value.status !== "accepted" && value.status !== "queued" && value.status !== "rejected") ||
      (value.status === "rejected" && !channelReason(value.reason))
    ) {
      return undefined;
    }
    const receipt: Record<string, any> = {
      status: value.status,
      sessionGeneration: value.sessionGeneration,
    };
    if (value.status === "rejected") receipt.reason = value.reason;
    if (value.duplicate === true) receipt.duplicate = true;
    return receipt;
  }

  function acknowledge(
    socket: any,
    token: number,
    requestId: string,
    registrationEpoch: string,
    expectedSessionGeneration: string,
    receipt: Record<string, any>,
    duplicate = false,
  ) {
    const frame: Record<string, unknown> = {
      type: "ack",
      registration_epoch: registrationEpoch,
      request_id: requestId,
      session_generation: expectedSessionGeneration,
      status: receipt.status,
    };
    if (receipt.status === "rejected") {
      frame.reason = receipt.reason;
    }
    if (duplicate || receipt.duplicate === true) {
      frame.duplicate = true;
    }
    sendChannelFrame(socket, token, frame);
  }

  function handleDeliver(socket: any, token: number, frame: Record<string, any>) {
    if (
      token !== channelGeneration ||
      socket !== channelSocket ||
      !channelReady ||
      socket.destroyed ||
      frame.registration_epoch !== channelEpoch ||
      frame.session_generation !== channelSessionGeneration ||
      !validChannelId(frame.request_id) ||
      typeof frame.text !== "string" ||
      frame.text.length === 0
    ) {
      return;
    }

    const requestId = frame.request_id;
    const registrationEpoch = channelEpoch;
    const expectedSessionGeneration = channelSessionGeneration;
    const prior = ledger.get(requestId);
    if (prior) {
      if (prior.text !== frame.text) {
        acknowledge(
          socket,
          token,
          requestId,
          registrationEpoch,
          expectedSessionGeneration,
          receiptForError("payload_mismatch", expectedSessionGeneration),
        );
        return;
      }
      // Coalesce pending duplicates instead of allocating unbounded Promise callbacks.
      prior.duplicate = true;
      if (prior.receipt) {
        acknowledge(socket, token, requestId, registrationEpoch, expectedSessionGeneration, prior.receipt, true);
      }
      return;
    }

    if (currentContext?.userMessageSessionGeneration !== expectedSessionGeneration) {
      socket.destroy();
      return;
    }
    if (ledger.size >= MAX_CHANNEL_LEDGER) {
      // There is no room to retain another outcome. Revoke the epoch rather than evict a key or
      // allow a capacity-rejected request to become a new admission on a later retry.
      acknowledge(socket, token, requestId, registrationEpoch, expectedSessionGeneration,
        receiptForError("admission_refused", expectedSessionGeneration));
      socket.destroy();
      return;
    }

    const entry: LedgerEntry = { epoch: registrationEpoch, text: frame.text, duplicate: false };
    ledger.set(requestId, entry); // Reserve before invoking Pi, including known capacity failures.
    if (inflight.size >= MAX_CHANNEL_INFLIGHT) {
      entry.receipt = receiptForError("admission_refused", expectedSessionGeneration);
      acknowledge(socket, token, requestId, registrationEpoch, expectedSessionGeneration, entry.receipt);
      return;
    }
    inflight.add(entry);
    const text = frame.text;
    const call = Promise.resolve().then(async () => {
      if (
        token !== channelGeneration ||
        socket !== channelSocket ||
        !channelReady ||
        socket.destroyed ||
        currentContext?.userMessageSessionGeneration !== expectedSessionGeneration ||
        typeof pi.submitUserMessage !== "function"
      ) {
        return receiptForError("session_changed", expectedSessionGeneration);
      }
      try {
        // This is the receipt-returning Pi ingress. The old void API is intentionally not a
        // fallback: acknowledging socket receipt would turn an unknown delivery into success.
        const result = await pi.submitUserMessage({
          registrationEpoch,
          requestId,
          sessionGeneration: expectedSessionGeneration,
          text,
          deliverAs: "followUp",
          expandPromptTemplates: false,
        });
        return normalizeReceipt(result, expectedSessionGeneration);
      } catch {
        return undefined; // Possible admission without a usable receipt is unknown, not rejection.
      }
    });
    void call.then((receipt) => {
      inflight.delete(entry);
      pruneEpochs();
      if (token !== channelGeneration || socket !== channelSocket || channelEpoch !== registrationEpoch) return;
      if (!receipt || currentContext?.userMessageSessionGeneration !== expectedSessionGeneration) {
        socket.destroy();
        return;
      }
      entry.receipt = receipt;
      acknowledge(socket, token, requestId, registrationEpoch, expectedSessionGeneration, receipt, entry.duplicate);
    });
  }

  function handleRotate(socket: any, token: number, frame: Record<string, any>) {
    if (
      token !== channelGeneration ||
      socket !== channelSocket ||
      !channelReady ||
      channelClosing ||
      socket.destroyed ||
      frame.registration_epoch !== channelEpoch ||
      frame.session_generation !== channelSessionGeneration ||
      currentContext?.userMessageSessionGeneration !== channelSessionGeneration
    ) {
      return;
    }
    // Only a complete, correlated clean-rotation control renews the transport retry budget.
    // Destroy the old connection; onDisconnect registers afresh, never resending deliveries or
    // ACKs. Unresolved Pi calls keep their capacity slots/epoch pins and old callback fences.
    reconnectAttempts = 0;
    socket.destroy();
  }

  function scheduleReconnect(ctx: any) {
    if (channelClosing || reconnectTimer || reconnectAttempts >= MAX_RECONNECT_ATTEMPTS || currentContext !== ctx) {
      return;
    }
    const expectedSessionGeneration = ctx.userMessageSessionGeneration;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = undefined;
      if (channelClosing || currentContext !== ctx || ctx?.userMessageSessionGeneration !== expectedSessionGeneration) return;
      reconnectAttempts += 1;
      openChannel(ctx);
    }, RECONNECT_DELAY_MS);
    reconnectTimer.unref?.();
  }

  function rejectRegistration(socket: any, token: number) {
    if (token !== channelGeneration || socket !== channelSocket) return;
    clearRegistrationTimer();
    channelReady = false;
    socket.destroy();
    channelSocket = undefined;
    channelEpoch = undefined;
    channelSessionGeneration = undefined;
    // A server refusal/malformed registration response is final for this session. Transport loss
    // (including a missing response) may retry a bounded fresh registration, never a delivery.
    channelClosing = true;
  }

  function openChannel(ctx: any) {
    if (
      channelClosing ||
      process.platform === "win32" ||
      typeof pi.submitUserMessage !== "function" ||
      !validChannelId(ctx?.userMessageSessionGeneration)
    ) {
      return;
    }

    channelClosing = false;
    const token = ++channelGeneration;
    const expectedSessionGeneration = ctx.userMessageSessionGeneration;
    channelSessionGeneration = expectedSessionGeneration;
    channelReady = false;
    channelEpoch = undefined;
    let buffer = Buffer.alloc(0);
    const registrationId = randomId("register");
    const socket = net.createConnection(socketEndpoint!);
    channelSocket = socket;
    registrationTimer = setTimeout(() => {
      if (token === channelGeneration && socket === channelSocket) socket.destroy();
    }, REGISTRATION_TIMEOUT_MS);
    registrationTimer.unref?.();

    const onDisconnect = () => {
      if (token !== channelGeneration || socket !== channelSocket) return;
      clearRegistrationTimer();
      channelSocket = undefined;
      channelReady = false;
      channelEpoch = undefined;
      channelSessionGeneration = undefined;
      ledger.clear();
      buffer = Buffer.alloc(0);
      // Socket loss is not receipt loss proof. Discard all unsent ACKs/deliveries and request a
      // fresh registration only, with a bounded total attempt budget for this session_start.
      if (!channelClosing && ctx?.userMessageSessionGeneration === expectedSessionGeneration) scheduleReconnect(ctx);
    };

    socket.on("connect", () => {
      if (
        token !== channelGeneration || socket !== channelSocket || channelClosing ||
        socket.destroyed || ctx?.userMessageSessionGeneration !== expectedSessionGeneration
      ) {
        socket.destroy();
        return;
      }
      socket.write(
        `${JSON.stringify({
          id: registrationId,
          method: "agent.register_self",
          params: { session_generation: expectedSessionGeneration },
        })}\n`,
      );
    });
    socket.on("data", (chunk: Buffer) => {
      if (token !== channelGeneration || socket !== channelSocket || channelClosing) return;
      // Slice each incoming chunk into frames before concatenation. The retained partial buffer
      // never exceeds 64 KiB, even when one chunk contains many complete small frames.
      let offset = 0;
      while (offset < chunk.length) {
        if (token !== channelGeneration || socket !== channelSocket || socket.destroyed) return;
        const newline = chunk.indexOf(10, offset);
        const end = newline < 0 ? chunk.length : newline;
        const part = chunk.subarray(offset, end);
        if (buffer.length + part.length > MAX_CHANNEL_FRAME) {
          socket.destroy();
          return;
        }
        buffer = Buffer.concat([buffer, part]);
        if (newline < 0) return;
        offset = newline + 1;
        let parsed: unknown;
        try {
          const line = new TextDecoder("utf-8", { fatal: true }).decode(buffer);
          buffer = Buffer.alloc(0);
          parsed = JSON.parse(line);
        } catch {
          socket.destroy();
          return;
        }
        if (!channelReady) {
          const result = isRecord(parsed) && isRecord(parsed.result) ? parsed.result : undefined;
          const epoch = isRecord(result) ? result.registration_epoch : undefined;
          const sessionGeneration = isRecord(result) ? result.session_generation : undefined;
          if (
            !isRecord(parsed) ||
            parsed.id !== registrationId ||
            parsed.error !== undefined ||
            !isRecord(result) ||
            !validChannelId(result.terminal_id) ||
            result.ready !== true ||
            !validChannelId(epoch) ||
            typeof sessionGeneration !== "string" ||
            sessionGeneration !== expectedSessionGeneration ||
            ctx?.userMessageSessionGeneration !== expectedSessionGeneration ||
            usedEpochs.has(epoch)
          ) {
            rejectRegistration(socket, token);
            return;
          }
          clearRegistrationTimer();
          usedEpochs.add(epoch);
          recentEpochs.push(epoch);
          if (recentEpochs.length > RECENT_CHANNEL_EPOCHS) recentEpochs.shift();
          pruneEpochs();
          channelEpoch = epoch;
          channelSessionGeneration = sessionGeneration;
          channelReady = true;
          continue;
        }
        if (isRecord(parsed) && parsed.type === "deliver") {
          handleDeliver(socket, token, parsed);
        } else if (isRecord(parsed) && parsed.type === "rotate") {
          handleRotate(socket, token, parsed);
        }
      }
    });
    socket.on("error", onDisconnect);
    socket.on("end", onDisconnect);
    socket.on("close", onDisconnect);
  }

  function installChannel(ctx: any) {
    closeChannel();
    channelClosing = false;
    reconnectAttempts = 0;
    currentContext = ctx;
    // Feature detection is intentionally conjunctive. Headless sessions and old Pi builds do not
    // advertise a receiver, and there is no fallback through the void message API.
    if (
      typeof pi.submitUserMessage !== "function" ||
      !validChannelId(ctx?.userMessageSessionGeneration)
    ) {
      channelClosing = true;
      return;
    }
    openChannel(ctx);
  }

  function shutdownChannel() {
    closeChannel();
    channelClosing = true;
    currentContext = undefined;
    rootSession = false;
  }

  function desiredState() {
    if (blockedCount > 0) {
      return { state: "blocked" as const, message: blockedMessage };
    }
    if (agentActive) {
      return { state: "working" as const, message: undefined };
    }
    return { state: "idle" as const, message: undefined };
  }

  function publishState(force = false) {
    const next = desiredState();
    if (!force && next.state === lastState && next.message === lastMessage) {
      return;
    }
    lastState = next.state;
    lastMessage = next.message;
    queueState(next.state, next.message);
  }

  pi.events.on("herdr:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      blockedCount = Math.max(0, blockedCount - 1);
      if (blockedCount === 0) {
        blockedMessage = undefined;
      }
      publishState();
      return;
    }

    blockedCount += 1;
    blockedMessage = data.label;
    publishState();
  });

  pi.on("session_start", async (event, ctx) => {
    // TUI only: RPC/JSON/print modes are headless (no PTY herdr can display),
    // and RPC still reports hasUI=true, so mode is the reliable gate.
    if (ctx?.mode !== "tui") {
      shutdownChannel();
      return;
    }
    rootSession = true;
    updateSessionRef(ctx);
    installChannel(ctx);
    await reportSession(event?.reason);
    // A reload can replace this extension mid-run without emitting another agent_start.
    if (!rootSession || currentContext !== ctx) return;
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
    updateSessionRef(ctx);
    void reportSession();
    agentActive = true;
    publishState();
  });

  pi.on("agent_settled", (_event, ctx) => {
    if (!rootSession || ctx?.isIdle?.() !== true) {
      return;
    }

    agentActive = false;
    publishState();
  });

  pi.on("session_shutdown", shutdownChannel);
}
