import { afterEach, expect, test } from "bun:test";
import { rm } from "node:fs/promises";
import net, { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const originalPlatform = process.platform;
const originalArgv = process.argv;
const originalCreateConnection = net.createConnection;
const originalEnvironment = {
  HERDR_ENV: process.env.HERDR_ENV,
  HERDR_OMP_IDLE_DEBOUNCE_MS: process.env.HERDR_OMP_IDLE_DEBOUNCE_MS,
  HERDR_PANE_ID: process.env.HERDR_PANE_ID,
  HERDR_SOCKET_PATH: process.env.HERDR_SOCKET_PATH,
  OMPCODE: process.env.OMPCODE,
};

let server: Server | undefined;
let socketPath: string | undefined;
let importCounter = 0;
const channelCleanups: Array<() => void> = [];
const channelServerSockets = new Set<net.Socket>();

afterEach(async () => {
  for (const cleanup of channelCleanups.splice(0)) cleanup();
  for (const socket of channelServerSockets) socket.destroy();
  channelServerSockets.clear();
  await new Promise<void>((resolve, reject) => {
    if (!server) {
      resolve();
      return;
    }
    server.close((error) => (error ? reject(error) : resolve()));
  });
  server = undefined;

  if (socketPath) {
    await rm(socketPath, { force: true });
    socketPath = undefined;
  }

  Object.defineProperty(process, "platform", { value: originalPlatform });
  net.createConnection = originalCreateConnection;
  process.argv = originalArgv;
  for (const [name, value] of Object.entries(originalEnvironment)) {
    if (value === undefined) {
      delete process.env[name];
    } else {
      process.env[name] = value;
    }
  }
});

const integrations = [
  { name: "Pi", modulePath: "./pi/herdr-agent-state.ts" },
  { name: "Oh My Pi", modulePath: "./omp/herdr-agent-state.ts" },
] as const;

const socketPlugins = [
  {
    name: "OpenCode",
    modulePath: "./opencode/herdr-agent-state.js",
    sessionID: "opencode-session",
  },
  { name: "Kilo", modulePath: "./kilo/herdr-agent-state.js", sessionID: "kilo-session" },
] as const;

function importFresh(modulePath: string) {
  importCounter += 1;
  return import(`${modulePath}?test=${importCounter}`);
}

type Handler = (event: unknown, context: unknown) => unknown;

function createExtensionHarness() {
  const handlers = new Map<string, Handler>();
  const eventHandlers = new Map<string, Handler>();
  return {
    handlers,
    eventHandlers,
    pi: {
      on(event: string, handler: Handler) {
        handlers.set(event, handler);
      },
      events: {
        on(event: string, handler: Handler) {
          eventHandlers.set(event, handler);
          return () => {};
        },
      },
    },
  };
}

function configureIntegrationEnvironment(recordingSocketPath: string) {
  // Tests may run inside an OMP shell; nested-session cases opt in explicitly.
  delete process.env.OMPCODE;
  process.env.HERDR_ENV = "1";
  process.env.HERDR_SOCKET_PATH = recordingSocketPath;
  process.env.HERDR_PANE_ID = "test:p1";
}

function captureConnectionEndpoint() {
  let connectedEndpoint: unknown;
  net.createConnection = ((...args: unknown[]) => {
    connectedEndpoint = args[0];
    return Reflect.apply(originalCreateConnection, net, args);
  }) as typeof net.createConnection;
  return () => connectedEndpoint;
}

async function startRecordingServer(name: string): Promise<unknown[]> {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      requests.push(JSON.parse(input.slice(0, newline)));
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return requests;
}

for (const socketPlugin of socketPlugins) {
  test(`${socketPlugin.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${socketPlugin.name.toLowerCase()}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();

    process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
    const { HerdrAgentStatePlugin } = await importFresh(socketPlugin.modulePath);
    const plugin = await HerdrAgentStatePlugin();
    await plugin.event({
      event: {
        type: "session.updated",
        properties: { sessionID: socketPlugin.sessionID },
      },
    });

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });
}

test("OpenCode stays disabled without the Herdr socket environment", async () => {
  process.env.HERDR_ENV = "1";
  process.env.HERDR_PANE_ID = "test:p1";
  delete process.env.HERDR_SOCKET_PATH;

  const { HerdrAgentStatePlugin } = await importFresh("./opencode/herdr-agent-state.js");

  expect(await HerdrAgentStatePlugin()).toEqual({});
});

for (const integration of integrations) {
  test(`${integration.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${integration.name.toLowerCase().replaceAll(" ", "-")}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);
    await handlers.get("session_start")?.(
      { reason: "startup" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => true,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => "test-session",
        },
      },
    );

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });

  test(`${integration.name} reload preserves working state when the agent is active`, async () => {
    const requests = await startRecordingServer(
      integration.name.toLowerCase().replaceAll(" ", "-"),
    );
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);

    const sessionStart = handlers.get("session_start");
    expect(sessionStart).toBeDefined();
    await sessionStart?.(
      { reason: "reload" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => false,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => undefined,
        },
      },
    );

    const reportedState = () => {
      for (const request of requests) {
        if (!isRecord(request) || request.method !== "pane.report_agent") {
          continue;
        }
        const params = request.params;
        if (isRecord(params) && typeof params.state === "string") {
          return params.state;
        }
      }
      return undefined;
    };

    const deadline = Date.now() + 1_000;
    while (Date.now() < deadline && reportedState() === undefined) {
      await Bun.sleep(5);
    }

    expect(reportedState()).toBe("working");
  });
}

test("OMP ignores nested sessions launched inside another OMP shell", async () => {
  const requests = await startRecordingServer("omp-nested");
  process.env.OMPCODE = "1";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  // OMP sets OMPCODE on every shell it spawns. A nested `omp` inherits it and
  // must not claim the pane's session for its short-lived conversation.
  expect(handlers.size).toBe(0);
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      hasUI: true,
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/omp-nested.jsonl",
        getSessionId: () => "omp-nested",
      },
    },
  );
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("OMP accepts POSIX and Windows session paths", async () => {
  const { isAbsoluteSessionPath } = await importFresh("./omp/herdr-agent-state.ts");

  expect(isAbsoluteSessionPath("/tmp/omp-session.jsonl")).toBe(true);
  expect(isAbsoluteSessionPath("C:\\Users\\User\\.omp\\agent\\sessions\\omp-session.jsonl")).toBe(
    true,
  );
  expect(isAbsoluteSessionPath("C:/Users/User/.omp/agent/sessions/omp-session.jsonl")).toBe(true);
  expect(isAbsoluteSessionPath("relative/omp-session.jsonl")).toBe(false);
});

test("Pi reports a Windows session path", async () => {
  const requests = await startRecordingServer("pi-windows-session-path");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionPath = "C:\\Users\\User\\.pi\\agent\\sessions\\pi-session.jsonl";
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      ...piContext(() => true),
      sessionManager: {
        getSessionFile: () => sessionPath,
        getSessionId: () => "pi-session",
      },
    },
  );
  await waitFor(() => requests.length === 2);

  expect(requests.map(requestSessionPath)).toEqual([sessionPath, sessionPath]);
});

test("Pi reports idle only after the agent settles", async () => {
  const requests = await startRecordingServer("pi-settled");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  expect(completionHandlers(handlers)).toEqual(["agent_settled"]);
  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);
  expect(handlers.has("agent_end")).toBe(false);

  const requestCountBeforeStaleSettlement = requests.length;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requests).toHaveLength(requestCountBeforeStaleSettlement);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi ignores RPC sessions even when UI APIs are available", async () => {
  const requests = await startRecordingServer("pi-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => true),
    hasUI: true,
    mode: "rpc",
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi settlement preserves explicit blocked-state precedence", async () => {
  const requests = await startRecordingServer("pi-settled-blocked");
  const { eventHandlers, handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);
  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  eventHandlers.get("herdr:blocked")?.({ active: true, label: "approval" }, context);
  await waitFor(() => requestStates(requests).length === 3);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked"]);

  eventHandlers.get("herdr:blocked")?.({ active: false }, context);
  await waitFor(() => requestStates(requests).length === 4);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked", "idle"]);
});

test("Pi reports the session replacement source", async () => {
  const requests = await startRecordingServer("pi-session-source");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const reportedSession = () =>
    requests.find((request) => isRecord(request) && request.method === "pane.report_agent_session");
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && reportedSession() === undefined) {
    await Bun.sleep(5);
  }

  const request = reportedSession();
  expect(request).toBeDefined();
  expect(isRecord(request) && isRecord(request.params) ? request.params.session_start_source : null)
    .toBe("new");
});

test("Pi waits for a replacement session report before publishing state", async () => {
  const recordingSocketPath = join(tmpdir(), `herdr-pi-session-order-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  let acknowledgeSessionReport: (() => void) | undefined;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (isRecord(request) && request.method === "pane.report_agent_session") {
        acknowledgeSessionReport = () => socket.end("{}\n");
        return;
      }
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  const sessionStartResult = sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && acknowledgeSessionReport === undefined) {
    await Bun.sleep(5);
  }
  expect(acknowledgeSessionReport).toBeDefined();
  expect(
    requests.some((request) => isRecord(request) && request.method === "pane.report_agent"),
  ).toBe(false);

  acknowledgeSessionReport?.();
  await sessionStartResult;

  const stateDeadline = Date.now() + 1_000;
  while (
    Date.now() < stateDeadline &&
    !requests.some((request) => isRecord(request) && request.method === "pane.report_agent")
  ) {
    await Bun.sleep(5);
  }
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
});

async function startDroppedFirstResponseServer(name: string) {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  let connectionCount = 0;
  const attemptedRequests: unknown[] = [];
  const deliveredRequests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    connectionCount += 1;
    const connectionNumber = connectionCount;
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      attemptedRequests.push(request);
      if (connectionNumber === 1) {
        return;
      }
      deliveredRequests.push(request);
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  return {
    attemptedRequests,
    deliveredRequests,
    connectionCount: () => connectionCount,
  };
}

test("Oh My Pi retries working before a queued idle state", async () => {
  const { attemptedRequests } = await startDroppedFirstResponseServer("omp-retry");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  const context = {
    hasUI: true,
    isIdle: () => false,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_end")?.({ messages: [] }, context);

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && attemptedRequests.length < 3) {
    await Bun.sleep(5);
  }

  expect(attemptedRequests).toHaveLength(3);
  expect(attemptedRequests[1]).toEqual(attemptedRequests[0]);
  expect(requestState(attemptedRequests[0])).toBe("working");
  expect(requestState(attemptedRequests[2])).toBe("idle");
});

test("Oh My Pi keeps working when a turn ends with a scheduled continuation", async () => {
  const requests = await startRecordingServer("omp-will-continue");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = {
    hasUI: true,
    isIdle: () => idle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };

  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // OMP already scheduled an automatic continuation, so this loop end is not a
  // user-visible settle and must not publish idle. See issue #2851.
  handlers.get("agent_end")?.({ messages: [], willContinue: true }, context);
  await Bun.sleep(50);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // The real terminal end still settles the pane.
  idle = true;
  handlers.get("agent_end")?.({ messages: [] }, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi retries working state after an unanswered socket attempt", async () => {
  const { attemptedRequests, deliveredRequests, connectionCount } =
    await startDroppedFirstResponseServer("pi-retry");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "startup" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => undefined,
      },
    },
  );

  const reportedWorking = () =>
    deliveredRequests.some((request) => {
      if (!isRecord(request) || request.method !== "pane.report_agent") {
        return false;
      }
      const params = request.params;
      return isRecord(params) && params.state === "working";
    });

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && !reportedWorking()) {
    await Bun.sleep(5);
  }

  expect(connectionCount()).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests.length).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests[1]).toEqual(attemptedRequests[0]);
  expect(reportedWorking()).toBe(true);
});

function completionHandlers(handlers: Map<string, Handler>): string[] {
  return ["agent_end", "agent_settled"].filter((event) => handlers.has(event));
}

function piContext(isIdle: () => boolean) {
  return {
    hasUI: true,
    mode: "tui",
    isIdle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
}

function requestStates(requests: unknown[]): unknown[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "pane.report_agent")
    .map(requestState);
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && !predicate()) {
    await Bun.sleep(5);
  }
  expect(predicate()).toBe(true);
}

function requestState(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.state;
}

function requestSessionPath(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.agent_session_path;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

// These tests exercise the managed asset's wire/forwarding contract only. Receipt fixtures are
// not evidence of Pi core admission, queue retention, hook/compaction behavior or real turns.
// Registered-channel cases need an established Unix channel (Linux/macOS support). The managed
// asset deliberately refuses to register on Windows, where transport-pinned peer identity is
// unsupported. Only cases that need registration use this gate; state/no-channel controls run
// everywhere. Capture the host platform before any individual test mocks process.platform.
const registeredChannelTest = test.skipIf(originalPlatform === "win32");
type AdmissionRequest = {
  registrationEpoch: string;
  requestId: string;
  sessionGeneration: string;
  text: string;
  deliverAs: "followUp";
  expandPromptTemplates: false;
};
type AdmissionReceipt = {
  status: "accepted" | "queued" | "rejected";
  sessionGeneration: string;
  reason?: string;
  duplicate?: true;
};

type ChannelConnection = {
  socket: net.Socket;
  registration: Record<string, any>;
  epoch: string;
  generation: string;
  closed: boolean;
  acks: Record<string, any>[];
  draftStates: Record<string, any>[];
};

async function startChannelServer(
  respond?: (connection: ChannelConnection, response: Record<string, any>) => void,
) {
  const recordingSocketPath = join(tmpdir(), `herdr-pi-channel-${process.pid}-${++importCounter}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });
  const connections: ChannelConnection[] = [];
  const reports: Record<string, any>[] = [];
  server = createServer((socket) => {
    channelServerSockets.add(socket);
    let connection: ChannelConnection | undefined;
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("close", () => {
      channelServerSockets.delete(socket);
      if (connection) connection.closed = true;
    });
    socket.on("error", () => {});
    socket.on("data", (chunk) => {
      buffer += chunk;
      for (;;) {
        const newline = buffer.indexOf("\n");
        if (newline < 0) break;
        const request = JSON.parse(buffer.slice(0, newline));
        buffer = buffer.slice(newline + 1);
        if (request.method === "agent.register_self") {
          connection = {
            socket,
            registration: request,
            epoch: `epoch-${connections.length + 1}`,
            generation: request.params.session_generation,
            closed: false,
            acks: [],
            draftStates: [],
          };
          connections.push(connection);
          const response = {
            id: request.id,
            result: {
              terminal_id: "terminal-pi",
              registration_epoch: connection.epoch,
              session_generation: connection.generation,
              ready: true,
            },
          };
          if (respond) respond(connection, response);
          else socket.write(`${JSON.stringify(response)}\n`);
        } else if (request.type === "ack" && connection) {
          connection.acks.push(request);
        } else if (request.type === "draft_state" && connection) {
          connection.draftStates.push(request);
        } else {
          reports.push(request);
          socket.end("{}\n");
        }
      }
    });
  });
  await new Promise<void>((resolve, reject) => {
    server!.once("error", reject);
    server!.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return { connections, reports };
}

function channelContext(generation = "session-1") {
  return { ...piContext(() => true), userMessageSessionGeneration: generation };
}

async function loadChannelAsset(
  submit?: (request: AdmissionRequest) => Promise<AdmissionReceipt | unknown>,
) {
  const harness = createExtensionHarness();
  const calls: AdmissionRequest[] = [];
  const pi = {
    ...harness.pi,
    submitUserMessage: submit ? async (request: AdmissionRequest) => {
      calls.push(request);
      return submit(request);
    } : undefined,
  };
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);
  const shutdown = () => { harness.handlers.get("session_shutdown")?.({ reason: "quit" }, {}); };
  channelCleanups.push(shutdown);
  return { ...harness, pi, calls, shutdown };
}

function accepted(request: AdmissionRequest): Promise<AdmissionReceipt> {
  return Promise.resolve({ status: "accepted", sessionGeneration: request.sessionGeneration });
}

function delivery(connection: ChannelConnection, requestId = "request-1", text = "hello") {
  return {
    type: "deliver",
    registration_epoch: connection.epoch,
    request_id: requestId,
    session_generation: connection.generation,
    text,
  };
}

function deliver(connection: ChannelConnection, requestId = "request-1", text = "hello") {
  connection.socket.write(`${JSON.stringify(delivery(connection, requestId, text))}\n`);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

for (const missing of ["api", "generation", "empty-generation"] as const) {
  test(`Pi does not advertise a channel without receipt ${missing}`, async () => {
    const { connections, reports } = await startChannelServer();
    const { handlers, pi, calls } = await loadChannelAsset(missing === "api" ? undefined : accepted);
    let fallbackCalls = 0;
    Object.assign(pi, { sendUserMessage: () => { fallbackCalls += 1; } });
    const context: Record<string, unknown> = channelContext();
    if (missing === "generation") delete context.userMessageSessionGeneration;
    if (missing === "empty-generation") context.userMessageSessionGeneration = "";
    await handlers.get("session_start")?.({ reason: "startup" }, context);
    await waitFor(() => reports.length === 1);
    expect(connections).toEqual([]);
    expect(calls).toEqual([]);
    expect(fallbackCalls).toBe(0);
    expect(requestStates(reports)).toEqual(["idle"]);
  });
}

for (const mode of ["rpc", "print", "json", undefined]) {
  test(`Pi receipt channel stays disabled for headless mode ${mode}`, async () => {
    const { connections, reports } = await startChannelServer();
    const { handlers, calls } = await loadChannelAsset(accepted);
    await handlers.get("session_start")?.({}, { ...channelContext(), mode, hasUI: true });
    handlers.get("agent_start")?.({}, channelContext());
    await Bun.sleep(30);
    expect(connections).toEqual([]);
    expect(reports).toEqual([]);
    expect(calls).toEqual([]);
  });
}

registeredChannelTest("Pi opens registration directly in-process only at TUI session_start, with no inherited pane/pid assertion", async () => {
  const { connections, reports } = await startChannelServer();
  process.env.HERDR_PANE_ID = "stale:p999";
  const connectionPids: number[] = [];
  net.createConnection = ((...args: unknown[]) => {
    connectionPids.push(process.pid);
    return Reflect.apply(originalCreateConnection, net, args);
  }) as typeof net.createConnection;
  const { handlers, calls } = await loadChannelAsset(accepted);
  handlers.get("agent_start")?.({}, channelContext());
  await Bun.sleep(30);
  expect(connectionPids).toEqual([]);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1 && reports.length === 1);
  expect(connectionPids).toEqual([process.pid, process.pid]);
  expect(connections[0].registration.params).toEqual({ session_generation: "session-1" });
  expect(calls).toEqual([]); // Ready registration is not an input receipt.
});

test("Pi Windows state reports still work but never advertise the unsupported receipt channel", async () => {
  const { connections, reports } = await startChannelServer();
  const recordingSocketPath = process.env.HERDR_SOCKET_PATH!;
  const pipeEndpoint = `\\\\.\\pipe\\${recordingSocketPath}`;
  Object.defineProperty(process, "platform", { value: "win32" });
  const endpoints: unknown[] = [];
  net.createConnection = ((...args: unknown[]) => {
    endpoints.push(args[0]);
    // Test-only platform seam: on Unix, redirect this exact named-pipe endpoint to the
    // recording Unix socket. On Windows, use the real named pipe. This is not native
    // Windows qualification when run on Unix, nor a simulated registered channel.
    if (originalPlatform !== "win32" && args[0] === pipeEndpoint) args[0] = recordingSocketPath;
    return Reflect.apply(originalCreateConnection, net, args);
  }) as typeof net.createConnection;
  const { handlers, pi, calls } = await loadChannelAsset(accepted);
  let fallbackCalls = 0;
  Object.assign(pi, { sendUserMessage: () => { fallbackCalls += 1; } });
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => reports.length === 1);
  handlers.get("agent_start")?.({}, channelContext());
  await waitFor(() => reports.length === 2);
  await handlers.get("session_start")?.({ reason: "new" }, channelContext("session-2"));
  await waitFor(() => reports.length === 3);
  await Bun.sleep(300); // Beyond the reconnect delay: feature presence must not trigger a retry.
  expect(requestStates(reports)).toEqual(["idle", "working", "idle"]);
  expect(reports.map((report) => report.method)).toEqual(Array(3).fill("pane.report_agent"));
  expect(endpoints).toEqual(Array(3).fill(pipeEndpoint));
  expect(connections).toEqual([]);
  expect(calls).toEqual([]);
  expect(fallbackCalls).toBe(0);
});

registeredChannelTest("Pi waits for a whole correlated registration response then forwards split UTF-8 delivery literally", async () => {
  let finishRegistration!: () => void;
  const { connections } = await startChannelServer((connection, response) => {
    const line = `${JSON.stringify(response)}\n`;
    connection.socket.write(line.slice(0, -1));
    finishRegistration = () => connection.socket.write("\n");
  });
  const { handlers, calls } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  await Bun.sleep(20);
  expect(calls).toEqual([]);
  finishRegistration();
  const literal = "/skill ${HOME}; echo shell\n\\n template {name} 🐑";
  const bytes = Buffer.from(`${JSON.stringify(delivery(connections[0], "literal", literal))}\n`);
  const split = bytes.indexOf(Buffer.from("🐑")) + 2;
  connections[0].socket.write(bytes.subarray(0, split));
  await Bun.sleep(20);
  expect(calls).toEqual([]);
  connections[0].socket.write(bytes.subarray(split, bytes.length - 1));
  await Bun.sleep(20);
  expect(calls).toEqual([]);
  connections[0].socket.write(bytes.subarray(bytes.length - 1));
  await waitFor(() => connections[0].acks.length === 1);
  expect(calls).toEqual([{
    registrationEpoch: "epoch-1", requestId: "literal", sessionGeneration: "session-1",
    text: literal, deliverAs: "followUp", expandPromptTemplates: false,
  }]);
  expect(connections[0].acks).toEqual([{
    type: "ack", registration_epoch: "epoch-1", request_id: "literal",
    session_generation: "session-1", status: "accepted",
  }]);
});

for (const receipt of [
  { status: "accepted", duplicate: true },
  { status: "queued", duplicate: true },
  ...["no_session", "session_changed", "payload_mismatch", "shutting_down", "admission_refused", "unsupported", "unknown", "draft_present", "ui_hold", "expired"]
    .map((reason) => ({ status: "rejected", reason, duplicate: true })),
] as const) {
  registeredChannelTest(`Pi maps typed ${receipt.status} ${"reason" in receipt ? receipt.reason : "receipt"} to snake_case ACK`, async () => {
    const { connections } = await startChannelServer();
    const { handlers } = await loadChannelAsset(async (request) => ({
      ...receipt, sessionGeneration: request.sessionGeneration,
    }));
    await handlers.get("session_start")?.({}, channelContext());
    await waitFor(() => connections.length === 1);
    deliver(connections[0]);
    await waitFor(() => connections[0].acks.length === 1);
    expect(connections[0].acks[0]).toEqual({
      type: "ack", registration_epoch: "epoch-1", request_id: "request-1",
      session_generation: "session-1", ...receipt,
    });
    expect(connections[0].acks[0]).not.toHaveProperty("sessionGeneration");
  });
}

registeredChannelTest("Pi coalesces pending/completed duplicates and rejects changed payload without another ingress attempt", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const { handlers, calls } = await loadChannelAsset(() => pending.promise);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  deliver(connection);
  await waitFor(() => calls.length === 1);
  for (let index = 0; index < 50; index += 1) deliver(connection);
  deliver(connection, "request-1", "changed");
  await waitFor(() => connection.acks.length === 1);
  expect(connection.acks[0].status).toBe("rejected");
  expect(connection.acks[0].reason).toBe("payload_mismatch");
  pending.resolve({ status: "queued", sessionGeneration: "session-1" });
  await waitFor(() => connection.acks.length === 2);
  expect(connection.acks[1]).toMatchObject({ status: "queued", duplicate: true });
  deliver(connection);
  await waitFor(() => connection.acks.length === 3);
  expect(connection.acks[2]).toMatchObject({ status: "queued", duplicate: true });
  deliver(connection, "request-1", "changed-again");
  await waitFor(() => connection.acks.length === 4);
  expect(connection.acks[3].reason).toBe("payload_mismatch");
  expect(calls).toHaveLength(1);
});

registeredChannelTest("Pi never invokes ingress for mismatched epoch/session or malformed delivery IDs", async () => {
  const { connections } = await startChannelServer();
  const { handlers, calls } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  const valid = delivery(connection);
  for (const frame of [
    { ...valid, registration_epoch: "obsolete" },
    { ...valid, session_generation: "obsolete" },
    { ...valid, request_id: "" },
    { ...valid, request_id: null },
    { ...valid, request_id: "x".repeat(1025) },
    { ...valid, text: "" },
    { ...valid, text: 123 },
    { ...valid, type: "other" },
  ]) connection.socket.write(`${JSON.stringify(frame)}\n`);
  await Bun.sleep(30);
  expect(calls).toEqual([]);
  expect(connection.acks).toEqual([]);
  deliver(connection, "valid");
  await waitFor(() => connection.acks.length === 1);
  expect(calls).toHaveLength(1);
});

for (const malformed of ["wrong-id", "wrong-generation", "not-ready", "no-terminal", "error", "arbitrary-json"]) {
  registeredChannelTest(`Pi rejects complete registration ${malformed} instead of advertising readiness`, async () => {
    const { connections } = await startChannelServer((connection, response) => {
      if (malformed === "wrong-id") response.id = "foreign-registration";
      if (malformed === "wrong-generation") response.result.session_generation = "foreign-session";
      if (malformed === "not-ready") response.result.ready = false;
      if (malformed === "no-terminal") delete response.result.terminal_id;
      if (malformed === "error") response.error = { code: "unsupported" };
      if (malformed === "arbitrary-json") response = { hello: true };
      connection.socket.write(`${JSON.stringify(response)}\n`);
    });
    const { handlers, calls } = await loadChannelAsset(accepted);
    await handlers.get("session_start")?.({}, channelContext());
    await waitFor(() => connections.length === 1 && connections[0].closed);
    await Bun.sleep(300);
    expect(connections).toHaveLength(1);
    expect(calls).toEqual([]);
  });
}

for (const malformed of ["oversized-complete", "oversized-partial", "oversized-unicode", "invalid-json", "invalid-utf8"]) {
  registeredChannelTest(`Pi drops ${malformed} delivery without submitting a partial frame`, async () => {
    const { connections } = await startChannelServer();
    const { handlers, calls, shutdown } = await loadChannelAsset(accepted);
    await handlers.get("session_start")?.({}, channelContext());
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    if (malformed === "oversized-complete") deliver(connection, "large", "x".repeat(65536));
    if (malformed === "oversized-partial") connection.socket.write("x".repeat(65537));
    if (malformed === "oversized-unicode") deliver(connection, "large", "🐑".repeat(17000));
    if (malformed === "invalid-json") connection.socket.write("{bad}\n");
    if (malformed === "invalid-utf8") connection.socket.write(Buffer.from([0xff, 10]));
    await waitFor(() => connection.closed);
    shutdown();
    expect(calls).toEqual([]);
    expect(connection.acks).toEqual([]);
  });
}

registeredChannelTest("Pi keeps 32 inflight slots and retains capacity-rejected request IDs without later admission", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const { handlers, calls } = await loadChannelAsset(() => pending.promise);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  for (let index = 0; index < 32; index += 1) deliver(connection, `pending-${index}`);
  await waitFor(() => calls.length === 32);
  deliver(connection, "overflow");
  await waitFor(() => connection.acks.length === 1);
  expect(connection.acks[0]).toMatchObject({ request_id: "overflow", status: "rejected", reason: "admission_refused" });
  pending.resolve({ status: "accepted", sessionGeneration: "session-1" });
  await waitFor(() => connection.acks.length === 33);
  deliver(connection, "overflow");
  await waitFor(() => connection.acks.length === 34);
  expect(connection.acks[33]).toMatchObject({ status: "rejected", reason: "admission_refused", duplicate: true });
  deliver(connection, "overflow", "changed");
  await waitFor(() => connection.acks.length === 35);
  expect(connection.acks[34].reason).toBe("payload_mismatch");
  expect(calls).toHaveLength(32);
});

registeredChannelTest("Pi retains all 256 epoch ledger entries and revokes rather than evicting on exhaustion", async () => {
  const { connections } = await startChannelServer();
  const { handlers, calls } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  for (let index = 0; index < 256; index += 1) {
    deliver(connection, `request-${index}`);
    await waitFor(() => connection.acks.length === index + 1);
  }
  deliver(connection, "request-0");
  await waitFor(() => connection.acks.length === 257);
  expect(connection.acks[256]).toMatchObject({ status: "accepted", duplicate: true });
  expect(calls).toHaveLength(256);
  deliver(connection, "request-overflow");
  await waitFor(() => connection.closed);
  expect(calls).toHaveLength(256);
  await waitFor(() => connections.length === 2);
  const fresh = connections[1];
  fresh.socket.write(`${JSON.stringify(delivery(connection, "request-0"))}\n`);
  fresh.socket.write(`${JSON.stringify(delivery(connection, "request-overflow"))}\n`);
  deliver(fresh, "after-capacity");
  await waitFor(() => fresh.acks.length === 1);
  deliver(fresh, "after-capacity");
  await waitFor(() => fresh.acks.length === 2);
  expect(fresh.acks[1]).toMatchObject({ status: "accepted", duplicate: true });
  expect(calls).toHaveLength(257);
  expect(calls[256].registrationEpoch).toBe(fresh.epoch);
});

for (const broken of ["throw", "void", "bad-status", "bad-reason", "foreign-generation"]) {
  registeredChannelTest(`Pi treats ${broken} ingress outcome as unknown: no invented rejection or replay`, async () => {
    const { connections } = await startChannelServer();
    const { handlers, calls } = await loadChannelAsset(async (request) => {
      if (broken === "throw") throw new Error("admission may already have occurred");
      if (broken === "void") return undefined;
      if (broken === "bad-status") return { status: "done", sessionGeneration: request.sessionGeneration };
      if (broken === "bad-reason") return { status: "rejected", reason: "invented", sessionGeneration: request.sessionGeneration };
      return { status: "accepted", sessionGeneration: "foreign-session" };
    });
    await handlers.get("session_start")?.({}, channelContext());
    await waitFor(() => connections.length === 1);
    deliver(connections[0]);
    await waitFor(() => connections[0].closed && connections.length === 2);
    expect(connections[0].acks).toEqual([]);
    expect(connections[1].acks).toEqual([]);
    expect(calls).toHaveLength(1);
    expect(connections[1].epoch).not.toBe(connections[0].epoch);
  });
}

registeredChannelTest("Pi never replays a lost receipt, fences an old callback after reconnect, and ignores stale epoch deliveries", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const { handlers, calls } = await loadChannelAsset(() => pending.promise);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const old = connections[0];
  deliver(old);
  await waitFor(() => calls.length === 1);
  old.socket.destroy();
  await waitFor(() => connections.length === 2);
  const fresh = connections[1];
  pending.resolve({ status: "accepted", sessionGeneration: "session-1" });
  fresh.socket.write(`${JSON.stringify(delivery(old))}\n`);
  await Bun.sleep(30);
  expect(old.acks).toEqual([]);
  expect(fresh.acks).toEqual([]);
  expect(calls).toHaveLength(1);
  deliver(fresh, "fresh-request");
  await waitFor(() => fresh.acks.length === 1);
  expect(calls[1].registrationEpoch).toBe(fresh.epoch);
  expect(fresh.acks[0].request_id).toBe("fresh-request");
});

registeredChannelTest("Pi refuses reused registration epochs after reconnect", async () => {
  const { connections } = await startChannelServer((connection, response) => {
    response.result.registration_epoch = "same-epoch";
    connection.epoch = "same-epoch";
    connection.socket.write(`${JSON.stringify(response)}\n`);
  });
  const { handlers, calls } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  deliver(connections[0]);
  await waitFor(() => connections[0].acks.length === 1);
  connections[0].socket.destroy();
  await waitFor(() => connections.length === 2 && connections[1].closed);
  expect(calls).toHaveLength(1);
});

registeredChannelTest("Pi reconnect attempt count is bounded and session shutdown cancels scheduled reconnect", async () => {
  const { connections } = await startChannelServer();
  const { handlers, shutdown } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  for (let index = 0; index < 3; index += 1) {
    await waitFor(() => connections.length === index + 1);
    await Bun.sleep(20); // allow the complete registration result to reach the receiver
    connections[index].socket.destroy();
    await waitFor(() => connections[index].closed);
  }
  await Bun.sleep(350);
  expect(connections).toHaveLength(3); // Initial connection + two reconnects, never unbounded.
  await handlers.get("session_start")?.({ reason: "new" }, channelContext("session-2"));
  await waitFor(() => connections.length === 4);
  await Bun.sleep(20);
  connections[3].socket.destroy();
  await waitFor(() => connections[3].closed);
  shutdown();
  shutdown();
  await Bun.sleep(350);
  expect(connections).toHaveLength(4);
});

registeredChannelTest("Pi session replacement closes/fences old callbacks and binds new ingress to the new generation", async () => {
  const { connections } = await startChannelServer();
  const oldReceipt = deferred<AdmissionReceipt>();
  const { handlers, calls } = await loadChannelAsset((request) =>
    request.sessionGeneration === "session-1" ? oldReceipt.promise : accepted(request));
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  deliver(connections[0], "same-id");
  await waitFor(() => calls.length === 1);
  await handlers.get("session_start")?.({ reason: "new" }, channelContext("session-2"));
  await waitFor(() => connections.length === 2 && connections[0].closed);
  oldReceipt.resolve({ status: "accepted", sessionGeneration: "session-1" });
  deliver(connections[1], "same-id");
  await waitFor(() => connections[1].acks.length === 1);
  expect(connections[0].acks).toEqual([]);
  expect(connections[1].acks[0]).toMatchObject({ session_generation: "session-2", registration_epoch: "epoch-2" });
  expect(calls.map((call) => call.sessionGeneration)).toEqual(["session-1", "session-2"]);
});

registeredChannelTest("Pi generation changes observed during delayed admission cannot ACK or reconnect into a replacement", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const { handlers, calls } = await loadChannelAsset(() => pending.promise);
  const context = channelContext();
  await handlers.get("session_start")?.({}, context);
  await waitFor(() => connections.length === 1);
  deliver(connections[0]);
  await waitFor(() => calls.length === 1);
  context.userMessageSessionGeneration = "session-2";
  pending.resolve({ status: "accepted", sessionGeneration: "session-1" });
  await waitFor(() => connections[0].closed);
  await Bun.sleep(350);
  expect(connections).toHaveLength(1);
  expect(connections[0].acks).toEqual([]);
  expect(calls[0].sessionGeneration).toBe("session-1");
});

registeredChannelTest("Pi incomplete registration times out and reconnects only within the bounded registration budget", async () => {
  const { connections } = await startChannelServer((connection, response) => {
    connection.socket.write(JSON.stringify(response)); // No newline: not a response yet.
  });
  const { handlers, calls, shutdown } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  await waitFor(() => connections[0].closed && connections.length === 2, 3000);
  shutdown();
  expect(calls).toEqual([]);
  expect(connections[0].acks).toEqual([]);
  await waitFor(() => connections[1].closed);
});

registeredChannelTest("Pi partial delivery loss never invokes ingress or replays bytes on the fresh epoch", async () => {
  const { connections } = await startChannelServer();
  const { handlers, calls } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const line = `${JSON.stringify(delivery(connections[0]))}\n`;
  connections[0].socket.write(line.slice(0, -4));
  await Bun.sleep(20);
  connections[0].socket.destroy();
  await waitFor(() => connections.length === 2);
  expect(calls).toEqual([]);
  expect(connections[0].acks).toEqual([]);
  deliver(connections[1]);
  await waitFor(() => connections[1].acks.length === 1);
  expect(calls).toHaveLength(1);
  expect(calls[0].registrationEpoch).toBe(connections[1].epoch);
});

registeredChannelTest("Pi rejects session change while a split registration response is still pending", async () => {
  let complete!: () => void;
  const { connections } = await startChannelServer((connection, response) => {
    connection.socket.write(JSON.stringify(response));
    complete = () => connection.socket.write("\n");
  });
  const context = channelContext();
  const { handlers, calls } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, context);
  await waitFor(() => connections.length === 1);
  context.userMessageSessionGeneration = "session-2";
  complete();
  await waitFor(() => connections[0].closed);
  await Bun.sleep(300);
  expect(connections).toHaveLength(1);
  expect(calls).toEqual([]);
});

registeredChannelTest("Pi fences reserved but not invoked frames if the connection is destroyed during frame parsing", async () => {
  const { connections } = await startChannelServer();
  const { handlers, calls, shutdown } = await loadChannelAsset(accepted);
  await handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  connections[0].socket.write(`${JSON.stringify(delivery(connections[0]))}\n{bad}\n`);
  await waitFor(() => connections[0].closed);
  shutdown();
  expect(calls).toEqual([]);
  expect(connections[0].acks).toEqual([]);
});

registeredChannelTest("Pi shutdown between two reserved deliveries prevents the second ingress attempt", async () => {
  const { connections } = await startChannelServer();
  let shutdown!: () => void;
  const asset = await loadChannelAsset(async (request) => {
    shutdown();
    return accepted(request);
  });
  shutdown = asset.shutdown;
  await asset.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  connections[0].socket.write(
    `${JSON.stringify(delivery(connections[0], "first"))}\n${JSON.stringify(delivery(connections[0], "second"))}\n`,
  );
  await waitFor(() => connections[0].closed);
  expect(asset.calls.map((call) => call.requestId)).toEqual(["first"]);
  expect(connections[0].acks).toEqual([]);
});

registeredChannelTest("Pi unresolved old-generation calls keep their capacity slots across replacement", async () => {
  const { connections } = await startChannelServer();
  const pending: Array<ReturnType<typeof deferred<AdmissionReceipt>>> = [];
  const asset = await loadChannelAsset(() => {
    const result = deferred<AdmissionReceipt>();
    pending.push(result);
    return result.promise;
  });
  await asset.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  for (let index = 0; index < 32; index += 1) deliver(connections[0], `old-${index}`);
  await waitFor(() => asset.calls.length === 32);
  await asset.handlers.get("session_start")?.({ reason: "new" }, channelContext("session-2"));
  await waitFor(() => connections.length === 2);
  deliver(connections[1], "at-capacity");
  await waitFor(() => connections[1].acks.length === 1);
  expect(connections[1].acks[0]).toMatchObject({ status: "rejected", reason: "admission_refused" });
  pending[0].resolve({ status: "accepted", sessionGeneration: "session-1" });
  await Bun.sleep(20);
  deliver(connections[1], "fresh");
  await waitFor(() => asset.calls.length === 33);
  // Resolve another old callback; it may only free its own entry, not the same-ID fresh request.
  pending[1].resolve({ status: "accepted", sessionGeneration: "session-1" });
  pending[32].resolve({ status: "queued", sessionGeneration: "session-2" });
  await waitFor(() => connections[1].acks.length === 2);
  expect(connections[0].acks).toEqual([]);
  expect(connections[1].acks[1]).toMatchObject({ request_id: "fresh", status: "queued", session_generation: "session-2" });
  for (const result of pending) result.resolve({ status: "rejected", reason: "session_changed", sessionGeneration: "session-1" });
});

registeredChannelTest("Pi bounds its outgoing ACK buffer and drops a possibly admitted receipt without replay", async () => {
  const { connections } = await startChannelServer();
  let client: net.Socket | undefined;
  net.createConnection = ((...args: unknown[]) => {
    const socket = Reflect.apply(originalCreateConnection, net, args);
    client ??= socket;
    return socket;
  }) as typeof net.createConnection;
  const asset = await loadChannelAsset(accepted);
  await asset.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  Object.defineProperty(client, "writableLength", { get: () => 65536, configurable: true });
  deliver(connections[0]);
  await waitFor(() => connections[0].closed && connections.length === 2);
  expect(asset.calls).toHaveLength(1);
  expect(connections[0].acks).toEqual([]);
  expect(connections[1].acks).toEqual([]);
});

function rotate(connection: ChannelConnection, fields: Record<string, unknown> = {}) {
  connection.socket.write(`${JSON.stringify({
    type: "rotate",
    registration_epoch: connection.epoch,
    session_generation: connection.generation,
    ...fields,
  })}\n`);
}

registeredChannelTest("Pi accepts more than 256 clean rotations without replay or exhausting transport retries", async () => {
  const { connections, reports } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  await asset.handlers.get("session_start")?.({}, channelContext());
  for (let index = 0; index <= 260; index += 1) {
    await waitFor(() => connections.length === index + 1);
    const connection = connections[index];
    if (index > 0) {
      connection.socket.write(`${JSON.stringify(delivery(connections[index - 1], "request-0"))}\n`);
    }
    const count = index === 0 ? 192 : 1;
    for (let request = 0; request < count; request += 1) {
      deliver(connection, `request-${request}`);
      await waitFor(() => connection.acks.length === request + 1);
    }
    deliver(connection, "request-0");
    await waitFor(() => connection.acks.length === count + 1);
    expect(connection.acks[count]).toMatchObject({ status: "accepted", duplicate: true });
    if (index < 260) {
      rotate(connection);
      await waitFor(() => connection.closed);
    }
  }
  expect(asset.calls).toHaveLength(452);
  expect(new Set(asset.calls.map((call) => `${call.registrationEpoch}:${call.requestId}`)).size).toBe(452);
  expect(requestStates(reports)).toEqual(["idle"]);
  expect(connections[260].closed).toBe(false);
}, 120000);

registeredChannelTest("Pi ignores invalid rotations without resetting the ordinary reconnect budget", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const context = channelContext();
  await asset.handlers.get("session_start")?.({}, context);
  for (let index = 0; index < 3; index += 1) {
    await waitFor(() => connections.length === index + 1);
    const connection = connections[index];
    deliver(connection, "prove-ready");
    await waitFor(() => connection.acks.length === 1);
    if (index === 2) {
      for (const fields of [
        { registration_epoch: "old" }, { session_generation: "old" },
        { registration_epoch: null }, { session_generation: null }, { type: "not-rotate" },
      ]) rotate(connection, fields);
      context.userMessageSessionGeneration = "changed";
      rotate(connection);
      await Bun.sleep(30);
      expect(connection.closed).toBe(false);
      context.userMessageSessionGeneration = connection.generation;
      deliver(connection, "still-ready");
      await waitFor(() => connection.acks.length === 2);
    }
    connection.socket.destroy();
    await waitFor(() => connection.closed);
  }
  await Bun.sleep(350);
  expect(connections).toHaveLength(3);
  expect(asset.calls).toHaveLength(4);
});

for (const protectedEpoch of ["current", "previous"] as const) {
  registeredChannelTest(`Pi refuses reused ${protectedEpoch} epochs after clean rotation`, async () => {
    const { connections } = await startChannelServer((connection, response) => {
      if (connections.length === 3) {
        connection.epoch = protectedEpoch === "current" ? "epoch-2" : "epoch-1";
        response.result.registration_epoch = connection.epoch;
      }
      connection.socket.write(`${JSON.stringify(response)}\n`);
    });
    const asset = await loadChannelAsset(accepted);
    await asset.handlers.get("session_start")?.({}, channelContext());
    for (let index = 0; index < 2; index += 1) {
      await waitFor(() => connections.length === index + 1);
      deliver(connections[index]);
      await waitFor(() => connections[index].acks.length === 1);
      rotate(connections[index]);
    }
    await waitFor(() => connections.length === 3 && connections[2].closed);
    expect(asset.calls).toHaveLength(2);
    expect(connections[2].acks).toEqual([]);
  });
}

registeredChannelTest("Pi clean rotation fences unresolved old admissions without replaying prompts or ACKs", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const asset = await loadChannelAsset((request) =>
    request.registrationEpoch === "epoch-1" ? pending.promise : accepted(request));
  await asset.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const old = connections[0];
  deliver(old, "uncertain");
  await waitFor(() => asset.calls.length === 1);
  rotate(old); // The server may have recorded a terminal unknown before Pi settles.
  await waitFor(() => old.closed && connections.length === 2);
  const fresh = connections[1];
  fresh.socket.write(`${JSON.stringify(delivery(old, "uncertain"))}\n`);
  pending.resolve({ status: "queued", sessionGeneration: old.generation });
  await Bun.sleep(30);
  expect(old.acks).toEqual([]);
  expect(fresh.acks).toEqual([]);
  expect(asset.calls).toHaveLength(1);
  deliver(fresh, "fresh");
  await waitFor(() => fresh.acks.length === 1);
  expect(asset.calls).toHaveLength(2);
  expect(fresh.acks[0]).toMatchObject({ request_id: "fresh", status: "accepted" });
});

registeredChannelTest("Pi drops trailing old deliveries after a complete split rotation control", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  await asset.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const old = connections[0];
  deliver(old, "prove-ready");
  await waitFor(() => old.acks.length === 1);
  const control = JSON.stringify({ type: "rotate", registration_epoch: old.epoch, session_generation: old.generation });
  old.socket.write(control);
  await Bun.sleep(30);
  expect(old.closed).toBe(false);
  old.socket.write(`\n${JSON.stringify(delivery(old, "must-not-submit"))}\n`);
  await waitFor(() => old.closed && connections.length === 2);
  expect(asset.calls).toHaveLength(1);
  expect(connections[1].acks).toEqual([]);
  deliver(connections[1], "fresh");
  await waitFor(() => connections[1].acks.length === 1);
  expect(asset.calls).toHaveLength(2);
});

registeredChannelTest("Pi rolls its bounded epoch window through more than 256 registrations in one runtime", async () => {
  const { connections, reports } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const context = channelContext();
  for (let index = 0; index < 300; index += 1) {
    await asset.handlers.get("session_start")?.({ reason: "reload" }, context);
    await waitFor(() => connections.length === index + 1);
    deliver(connections[index], "prove-ready");
    await waitFor(() => connections[index].acks.length === 1);
    deliver(connections[index], "prove-ready");
    await waitFor(() => connections[index].acks.length === 2);
    expect(connections[index].acks[1]).toMatchObject({ status: "accepted", duplicate: true });
  }
  expect(asset.calls).toHaveLength(300);
  await waitFor(() => requestStates(reports).length === 300);
  expect(requestStates(reports)).toEqual(Array(300).fill("idle"));
  expect(connections[299].closed).toBe(false);
}, 15000);

for (const protectedEpoch of ["current", "previous", "unresolved"] as const) {
  registeredChannelTest(`Pi refuses reused ${protectedEpoch} epochs inside its retained window`, async () => {
    const freshCount = protectedEpoch === "unresolved" ? 260 : 2;
    const { connections } = await startChannelServer((connection, response) => {
      if (connections.length > freshCount) {
        connection.epoch = protectedEpoch === "current" ? `epoch-${freshCount}` : "epoch-1";
        response.result.registration_epoch = connection.epoch;
      }
      connection.socket.write(`${JSON.stringify(response)}\n`);
    });
    const pending = deferred<AdmissionReceipt>();
    const asset = await loadChannelAsset((request) =>
      protectedEpoch === "unresolved" && request.registrationEpoch === "epoch-1"
        ? pending.promise : accepted(request));
    const context = channelContext();
    for (let index = 0; index < freshCount; index += 1) {
      await asset.handlers.get("session_start")?.({ reason: "reload" }, context);
      await waitFor(() => connections.length === index + 1);
      deliver(connections[index], "same-id");
      if (index === 0 && protectedEpoch === "unresolved") {
        await waitFor(() => asset.calls.length === 1);
      } else {
        await waitFor(() => connections[index].acks.length === 1);
      }
    }
    await asset.handlers.get("session_start")?.({ reason: "reload" }, context);
    await waitFor(() => connections.length === freshCount + 1 && connections[freshCount].closed);
    expect(asset.calls).toHaveLength(freshCount);
    expect(connections[freshCount].acks).toEqual([]);
    pending.resolve({ status: "queued", sessionGeneration: context.userMessageSessionGeneration });
    await Bun.sleep(20);
    if (protectedEpoch === "unresolved") expect(connections[0].acks).toEqual([]);
  }, 15000);
}

registeredChannelTest("Pi retains 32 distinct unresolved epochs while fresh registrations and capacity receipts keep working", async () => {
  const { connections } = await startChannelServer((connection, response) => {
    if (connections.length === 37) {
      connection.epoch = "epoch-1";
      response.result.registration_epoch = connection.epoch;
    }
    connection.socket.write(`${JSON.stringify(response)}\n`);
  });
  const pending: Array<ReturnType<typeof deferred<AdmissionReceipt>>> = [];
  const asset = await loadChannelAsset(() => {
    const receipt = deferred<AdmissionReceipt>();
    pending.push(receipt);
    return receipt.promise;
  });
  const context = channelContext();
  for (let index = 0; index < 36; index += 1) {
    await asset.handlers.get("session_start")?.({ reason: "reload" }, context);
    await waitFor(() => connections.length === index + 1);
    deliver(connections[index], "same-id");
    if (index < 32) {
      await waitFor(() => asset.calls.length === index + 1);
    } else {
      await waitFor(() => connections[index].acks.length === 1);
      expect(connections[index].acks[0]).toMatchObject({ status: "rejected", reason: "admission_refused" });
      deliver(connections[index], "same-id");
      await waitFor(() => connections[index].acks.length === 2);
      expect(connections[index].acks[1]).toMatchObject({ status: "rejected", duplicate: true });
    }
  }
  await asset.handlers.get("session_start")?.({ reason: "reload" }, context);
  await waitFor(() => connections.length === 37 && connections[36].closed);
  expect(asset.calls).toHaveLength(32);
  for (const receipt of pending) {
    receipt.resolve({ status: "queued", sessionGeneration: context.userMessageSessionGeneration });
  }
  await Bun.sleep(20);
  for (const connection of connections.slice(0, 32)) expect(connection.acks).toEqual([]);
});

registeredChannelTest("Pi stale retired-epoch deliveries cannot replay IDs after the recent window rolls", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const context = channelContext();
  for (let index = 0; index < 4; index += 1) {
    await asset.handlers.get("session_start")?.({ reason: "reload" }, context);
    await waitFor(() => connections.length === index + 1);
    deliver(connections[index], "same-id", `text-${index}`);
    await waitFor(() => connections[index].acks.length === 1);
  }
  const current = connections[3];
  for (const old of connections.slice(0, 3)) {
    current.socket.write(`${JSON.stringify(delivery(old, "same-id", "replay"))}\n`);
  }
  deliver(current, "same-id", "text-3");
  deliver(current, "same-id", "changed");
  await waitFor(() => current.acks.length === 3);
  expect(current.acks[1]).toMatchObject({ status: "accepted", duplicate: true });
  expect(current.acks[2]).toMatchObject({ status: "rejected", reason: "payload_mismatch" });
  expect(asset.calls).toHaveLength(4);
});

registeredChannelTest("Pi frame buffer is per-frame, not a cap on coalesced complete deliveries", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  await asset.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  // Each frame is 4 KiB, but the single write exceeds the 64 KiB partial-frame budget.
  const batch = Array.from({ length: 20 }, (_, index) =>
    `${JSON.stringify(delivery(connection, `batch-${index}`, "x".repeat(4096)))}\n`).join("");
  connection.socket.write(batch);
  await waitFor(() => connection.acks.length === 20);
  expect(connection.closed).toBe(false);
  expect(asset.calls).toHaveLength(20);
});

registeredChannelTest("Pi reload/session_shutdown idempotently closes the old socket and fences callbacks before a fresh asset opens", async () => {
  const { connections, reports } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const old = await loadChannelAsset(() => pending.promise);
  await old.handlers.get("session_start")?.({}, channelContext());
  await waitFor(() => connections.length === 1);
  deliver(connections[0]);
  await waitFor(() => old.calls.length === 1);
  old.handlers.get("session_shutdown")?.({ reason: "reload" }, channelContext());
  old.shutdown();
  await waitFor(() => connections[0].closed);
  const replacement = await loadChannelAsset(accepted);
  await replacement.handlers.get("session_start")?.({ reason: "reload" }, channelContext("session-2"));
  await waitFor(() => connections.length === 2);
  pending.resolve({ status: "queued", sessionGeneration: "session-1" });
  deliver(connections[1]);
  await waitFor(() => connections[1].acks.length === 1);
  expect(connections[0].acks).toEqual([]);
  expect(old.calls).toHaveLength(1);
  expect(replacement.calls).toHaveLength(1);
  const before = reports.length;
  old.handlers.get("agent_start")?.({}, channelContext());
  await Bun.sleep(30);
  expect(reports).toHaveLength(before);
});

// Draft access is deliberately a UI fixture, not terminal/screen capture. These tests prove
// transport redaction and callback ordering; they do not qualify real Pi core admission.
function draftContext(ui: Record<string, any> = {}) {
  return {
    ...channelContext(),
    ui: { getEditorText: () => "", holdState: () => undefined, ...ui },
  };
}

function guardedDeliver(connection: ChannelConnection, requestId = "guarded", text = "remote prompt", guard = true) {
  connection.socket.write(`${JSON.stringify({
    ...delivery(connection, requestId, text), if_draft_empty: guard,
    ...(guard ? { deadline_ms: Date.now() + 60_000 } : {}),
  })}\n`);
}

function queryDraft(connection: ChannelConnection, requestId = "draft-query", fields: Record<string, unknown> = {}) {
  connection.socket.write(`${JSON.stringify({
    type: "draft_state", registration_epoch: connection.epoch,
    request_id: requestId, session_generation: connection.generation, ...fields,
  })}\n`);
}

registeredChannelTest("Pi draft guard advertises only callable editor/hold APIs alongside receipt ingress", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const contexts = [
    draftContext(), channelContext(), draftContext({ getEditorText: undefined }),
    draftContext({ holdState: undefined }), draftContext({ getEditorText: "not callable" }),
    draftContext({ holdState: "not callable" }),
  ];
  for (const [index, context] of contexts.entries()) {
    await asset.handlers.get("session_start")?.({}, context);
    await waitFor(() => connections.length === index + 1);
    expect(connections[index].registration.params.draft_guard === true).toBe(index === 0);
  }
  expect(asset.calls).toEqual([]);
});

registeredChannelTest("Pi draft guard remains unregistered without receipt ingress even with UI APIs", async () => {
  const { connections, reports } = await startChannelServer();
  const asset = await loadChannelAsset();
  await asset.handlers.get("session_start")?.({}, draftContext());
  await waitFor(() => reports.length === 1);
  expect(connections).toEqual([]);
  expect(asset.calls).toEqual([]);
});

for (const draft of ["in-progress draft", " ", "\t\r\n  ", "\u00a0\u200b", "👩🏽‍💻 e\u0301 羊\n"]) {
  registeredChannelTest(`Pi guarded nonempty draft stays byte-identical ${JSON.stringify(draft)}`, async () => {
    const { connections } = await startChannelServer();
    const asset = await loadChannelAsset(accepted);
    let editor = draft;
    let mutations = 0;
    let fallbackCalls = 0;
    Object.assign(asset.pi, { sendUserMessage: () => { fallbackCalls += 1; } });
    await asset.handlers.get("session_start")?.({}, draftContext({
      getEditorText: () => editor,
      setEditorText: (value: string) => { mutations += 1; editor = value; },
      pasteToEditor: (value: string) => { mutations += 1; editor += value; },
    }));
    await waitFor(() => connections.length === 1);
    guardedDeliver(connections[0]);
    await waitFor(() => connections[0].acks.length === 1);
    expect(connections[0].acks[0]).toEqual({
      type: "ack", registration_epoch: "epoch-1", request_id: "guarded",
      session_generation: "session-1", status: "rejected", reason: "draft_present",
    });
    expect(Buffer.from(editor)).toEqual(Buffer.from(draft));
    expect(asset.calls).toEqual([]);
    expect(mutations).toBe(0);
    expect(fallbackCalls).toBe(0);
  });
}

registeredChannelTest("Pi guarded empty draft submits exactly once and leaves editor empty", async () => {
  const { connections } = await startChannelServer();
  let editor = "";
  let mutations = 0;
  const asset = await loadChannelAsset(async (request) => {
    expect(editor).toBe("");
    return accepted(request);
  });
  await asset.handlers.get("session_start")?.({}, draftContext({
    getEditorText: () => editor,
    setEditorText: (value: string) => { mutations += 1; editor = value; },
    pasteToEditor: (value: string) => { mutations += 1; editor += value; },
  }));
  await waitFor(() => connections.length === 1);
  guardedDeliver(connections[0]);
  await waitFor(() => connections[0].acks.length === 1);
  guardedDeliver(connections[0]);
  await waitFor(() => connections[0].acks.length === 2);
  expect(asset.calls).toEqual([{
    registrationEpoch: "epoch-1", requestId: "guarded", sessionGeneration: "session-1",
    text: "remote prompt", deliverAs: "followUp", expandPromptTemplates: false,
  }]);
  expect(connections[0].acks[1]).toMatchObject({ status: "accepted", duplicate: true });
  expect(editor).toBe("");
  expect(mutations).toBe(0);
});

registeredChannelTest("Pi guarded checks and invokes in the same socket callback before any microtask, then waits only for receipt", async () => {
  const { connections } = await startChannelServer();
  let insideData = false;
  net.createConnection = ((...args: unknown[]) => {
    const socket = Reflect.apply(originalCreateConnection, net, args) as net.Socket;
    const on = socket.on;
    socket.on = function (event: string, handler: (...values: any[]) => void) {
      if (event !== "data") return Reflect.apply(on, this, [event, handler]);
      return Reflect.apply(on, this, [event, (...values: any[]) => {
        insideData = true;
        try { handler(...values); } finally { insideData = false; }
      }]);
    } as typeof socket.on;
    return socket;
  }) as typeof net.createConnection;
  const order: string[] = [];
  const pending = deferred<AdmissionReceipt>();
  const asset = await loadChannelAsset(() => {
    expect(insideData).toBe(true);
    order.push("submit");
    return pending.promise;
  });
  await asset.handlers.get("session_start")?.({}, draftContext({
    getEditorText() {
      expect(insideData).toBe(true);
      order.push("editor");
      queueMicrotask(() => order.push("microtask"));
      return "";
    },
    holdState() { expect(insideData).toBe(true); order.push("hold"); return undefined; },
  }));
  await waitFor(() => connections.length === 1);
  guardedDeliver(connections[0]);
  await waitFor(() => order.includes("microtask"));
  expect(order).toEqual(["editor", "hold", "submit", "microtask"]);
  expect(asset.calls).toHaveLength(1);
  expect(connections[0].acks).toEqual([]);
  pending.resolve({ status: "queued", sessionGeneration: "session-1" });
  await waitFor(() => connections[0].acks.length === 1);
  expect(connections[0].acks[0].status).toBe("queued");
});

for (const inputBeforeCheck of [true, false]) {
  registeredChannelTest(`Pi guarded queued keystroke race isolates the prompt and preserves the draft (input before check: ${inputBeforeCheck})`, async () => {
    const { connections } = await startChannelServer();
    const queuedInput = " \tuser keystrokes 👩🏽‍💻 e\u0301\n";
    const remotePrompt = "isolated remote follow-up";
    let editor = "";
    let mutations = 0;
    const pending = deferred<AdmissionReceipt>();
    const asset = await loadChannelAsset((request) => {
      expect(editor).toBe("");
      expect(request.text).toBe(remotePrompt);
      return pending.promise;
    });
    await asset.handlers.get("session_start")?.({}, draftContext({
      getEditorText: () => {
        if (!inputBeforeCheck) queueMicrotask(() => { editor = queuedInput; });
        return editor;
      },
      setEditorText: (value: string) => { mutations += 1; editor = value; },
      pasteToEditor: (value: string) => { mutations += 1; editor += value; },
    }));
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    if (inputBeforeCheck) {
      queueMicrotask(() => { editor = queuedInput; });
      await Promise.resolve();
    }
    guardedDeliver(connection, "keystroke-race", remotePrompt);
    await waitFor(() => editor === queuedInput && (inputBeforeCheck ? connection.acks.length === 1 : asset.calls.length === 1));
    if (inputBeforeCheck) {
      expect(asset.calls).toEqual([]);
      expect(connection.acks[0]).toMatchObject({ status: "rejected", reason: "draft_present" });
    } else {
      expect(connection.acks).toEqual([]); // Awaiting only receipt cannot consume later keystrokes.
      expect(asset.calls.map((request) => request.text)).toEqual([remotePrompt]);
      pending.resolve({ status: "queued", sessionGeneration: "session-1" });
      await waitFor(() => connection.acks.length === 1);
      expect(connection.acks[0].status).toBe("queued");
    }
    expect(Buffer.from(editor)).toEqual(Buffer.from(queuedInput));
    expect(mutations).toBe(0);
  });
}

for (const hold of ["dialog", "custom", "editor"]) {
  registeredChannelTest(`Pi guarded active ${hold} hold refuses ingress`, async () => {
    const { connections } = await startChannelServer();
    const asset = await loadChannelAsset(accepted);
    await asset.handlers.get("session_start")?.({}, draftContext({ holdState: () => hold }));
    await waitFor(() => connections.length === 1);
    guardedDeliver(connections[0]);
    await waitFor(() => connections[0].acks.length === 1);
    expect(connections[0].acks[0]).toMatchObject({ status: "rejected", reason: "ui_hold" });
    expect(asset.calls).toEqual([]);
  });
}

const unavailableDraftUIs: Array<[string, () => Record<string, any> | undefined, string?]> = [
  ["missing UI", () => undefined],
  ["missing editor getter", () => ({ getEditorText: undefined, holdState: () => undefined })],
  ["missing hold API", () => ({ getEditorText: () => "", holdState: undefined })],
  ["throwing editor getter", () => ({ getEditorText: () => { throw new Error("secret draft"); }, holdState: () => undefined })],
  ["throwing hold API", () => ({ getEditorText: () => "", holdState: () => { throw new Error("secret hold"); } })],
  ["non-string editor", () => ({ getEditorText: () => 123, holdState: () => undefined })],
  ["async editor", () => ({ getEditorText: async () => "", holdState: () => undefined })],
  ["async hold", () => ({ getEditorText: () => "", holdState: async () => undefined }), "ui_hold"],
  ["invalid hold", () => ({ getEditorText: () => "", holdState: () => "invented secret" }), "ui_hold"],
  ["null hold", () => ({ getEditorText: () => "", holdState: () => null }), "ui_hold"],
  ["false hold", () => ({ getEditorText: () => "", holdState: () => false }), "ui_hold"],
  ["object hold", () => ({ getEditorText: () => "", holdState: () => ({ text: "secret hold" }) }), "ui_hold"],
  ["throwing getter property", () => ({ get getEditorText() { throw new Error("secret property"); }, holdState: () => undefined })],
  ["throwing hold property", () => ({ getEditorText: () => "", get holdState() { throw new Error("secret property"); } })],
];

for (const [name, makeUI, guardReason = "unknown"] of unavailableDraftUIs) {
  registeredChannelTest(`Pi guarded and draft query fail closed (${guardReason}/unknown) for ${name}`, async () => {
    const { connections } = await startChannelServer();
    const asset = await loadChannelAsset(accepted);
    const context = { ...channelContext(), ui: makeUI() };
    await asset.handlers.get("session_start")?.({}, context);
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    guardedDeliver(connection);
    queryDraft(connection);
    await waitFor(() => connection.acks.length === 1 && connection.draftStates.length === 1);
    expect(connection.acks[0]).toMatchObject({ status: "rejected", reason: guardReason });
    expect(connection.draftStates[0]).toEqual({
      type: "draft_state", registration_epoch: "epoch-1", request_id: "draft-query",
      session_generation: "session-1", unknown: true,
    });
    expect(asset.calls).toEqual([]);
    expect(JSON.stringify([...connection.acks, ...connection.draftStates])).not.toContain("secret");
  });
}

registeredChannelTest("Pi guarded APIs removed after registration fail closed without hiding unguarded ingress", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const context = draftContext();
  await asset.handlers.get("session_start")?.({}, context);
  await waitFor(() => connections.length === 1);
  expect(connections[0].registration.params.draft_guard).toBe(true);
  context.ui.holdState = undefined;
  guardedDeliver(connections[0]);
  await waitFor(() => connections[0].acks.length === 1);
  expect(connections[0].acks[0].reason).toBe("unknown");
  deliver(connections[0], "unguarded");
  await waitFor(() => connections[0].acks.length === 2);
  expect(asset.calls.map((call) => call.requestId)).toEqual(["unguarded"]);
});

for (const guardedFirst of [false, true]) {
  registeredChannelTest(`Pi guarded duplicate flag mismatch is rejected pending and completed (${guardedFirst})`, async () => {
    const { connections } = await startChannelServer();
    const pending = deferred<AdmissionReceipt>();
    const asset = await loadChannelAsset(() => pending.promise);
    await asset.handlers.get("session_start")?.({}, draftContext());
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    guardedDeliver(connection, "same-id", "hello", guardedFirst);
    await waitFor(() => asset.calls.length === 1);
    guardedDeliver(connection, "same-id", "hello", guardedFirst);
    guardedDeliver(connection, "same-id", "hello", !guardedFirst);
    // The mismatch ACK also fences parsing of the matching pending duplicate before receipt.
    await waitFor(() => connection.acks.length === 1);
    expect(connection.acks[0].reason).toBe("payload_mismatch");
    pending.resolve({ status: "accepted", sessionGeneration: "session-1" });
    await waitFor(() => connection.acks.length === 2);
    expect(connection.acks[1]).toMatchObject({ status: "accepted", duplicate: true });
    guardedDeliver(connection, "same-id", "hello", !guardedFirst);
    await waitFor(() => connection.acks.length === 3);
    expect(connection.acks[2].reason).toBe("payload_mismatch");
    expect(asset.calls).toHaveLength(1);
  });
}

registeredChannelTest("Pi guarded refusal is retained across draft edits; absent and false guards share old identity", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  let editor = "private draft";
  await asset.handlers.get("session_start")?.({}, draftContext({ getEditorText: () => editor }));
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  guardedDeliver(connection);
  await waitFor(() => connection.acks.length === 1);
  editor = ""; // A user's later edit must not turn a retried rejection into an admission.
  guardedDeliver(connection);
  await waitFor(() => connection.acks.length === 2);
  expect(connection.acks[1]).toMatchObject({ reason: "draft_present", duplicate: true });
  guardedDeliver(connection, "guarded", "remote prompt", false);
  await waitFor(() => connection.acks.length === 3);
  expect(connection.acks[2].reason).toBe("payload_mismatch");
  deliver(connection, "old", "hello");
  await waitFor(() => connection.acks.length === 4);
  guardedDeliver(connection, "old", "hello", false);
  await waitFor(() => connection.acks.length === 5);
  expect(connection.acks[4]).toMatchObject({ status: "accepted", duplicate: true });
  expect(asset.calls).toHaveLength(1);
});

registeredChannelTest("Pi guarded retired-epoch requests are fenced even when the guard flag changes", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const asset = await loadChannelAsset((request) => request.registrationEpoch === "epoch-1" ? pending.promise : accepted(request));
  await asset.handlers.get("session_start")?.({}, draftContext());
  await waitFor(() => connections.length === 1);
  const old = connections[0];
  guardedDeliver(old, "same-id", "hello");
  await waitFor(() => asset.calls.length === 1);
  rotate(old);
  await waitFor(() => connections.length === 2 && old.closed);
  const fresh = connections[1];
  for (const guard of [true, false]) {
    fresh.socket.write(`${JSON.stringify({ ...delivery(old, "same-id", "hello"), if_draft_empty: guard })}\n`);
  }
  pending.resolve({ status: "accepted", sessionGeneration: "session-1" });
  await Bun.sleep(30);
  expect(fresh.acks).toEqual([]);
  expect(asset.calls).toHaveLength(1);
  guardedDeliver(fresh, "same-id", "hello");
  await waitFor(() => fresh.acks.length === 1);
  guardedDeliver(fresh, "same-id", "hello", false);
  await waitFor(() => fresh.acks.length === 2);
  expect(fresh.acks[1].reason).toBe("payload_mismatch");
  expect(asset.calls).toHaveLength(2);
  expect(old.acks).toEqual([]);
});

registeredChannelTest("Pi guarded rejects nonboolean flags without interpreting truthy input as permission", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  await asset.handlers.get("session_start")?.({}, draftContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  for (const flag of ["true", "false", 0, 1, null, {}, []]) {
    connection.socket.write(`${JSON.stringify({ ...delivery(connection), if_draft_empty: flag })}\n`);
  }
  await Bun.sleep(30);
  expect(asset.calls).toEqual([]);
  expect(connection.acks).toEqual([]);
});

for (const [draft, hold] of [["", undefined], [" \n🐑e\u0301", "dialog"], ["secret: 🎉", "custom"], ["\t", "editor"]] as const) {
  registeredChannelTest(`Pi draft query returns typed redacted read-only state ${JSON.stringify([draft, hold])}`, async () => {
    const { connections } = await startChannelServer();
    let mutations = 0;
    const asset = await loadChannelAsset(accepted);
    await asset.handlers.get("session_start")?.({}, draftContext({
      getEditorText: () => draft, holdState: () => hold,
      setEditorText: () => { mutations += 1; }, pasteToEditor: () => { mutations += 1; },
    }));
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    queryDraft(connection, "read-only", { text: "caller-supplied secret", empty: true, chars: 0, hold: null });
    await waitFor(() => connection.draftStates.length === 1);
    expect(connection.draftStates[0]).toEqual({
      type: "draft_state", registration_epoch: "epoch-1", request_id: "read-only",
      session_generation: "session-1", empty: draft.length === 0,
      chars: Array.from(draft).length, hold: hold ?? null,
    });
    expect(typeof connection.draftStates[0].empty).toBe("boolean");
    expect(Number.isSafeInteger(connection.draftStates[0].chars)).toBe(true);
    expect(connection.draftStates[0]).not.toHaveProperty("text");
    expect(connection.draftStates[0]).not.toHaveProperty("draft");
    expect(JSON.stringify(connection.draftStates)).not.toContain("secret");
    expect(asset.calls).toEqual([]);
    expect(connection.acks).toEqual([]);
    expect(mutations).toBe(0);
  });
}

registeredChannelTest("Pi draft query is fresh, not a delivery reservation or permission for a later guarded prompt", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  let editor = "";
  await asset.handlers.get("session_start")?.({}, draftContext({ getEditorText: () => editor }));
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  queryDraft(connection, "shared-id");
  await waitFor(() => connection.draftStates.length === 1);
  expect(connection.draftStates[0].empty).toBe(true);
  editor = "user typed after the snapshot";
  queryDraft(connection, "shared-id");
  await waitFor(() => connection.draftStates.length === 2);
  expect(connection.draftStates[1].empty).toBe(false);
  guardedDeliver(connection, "shared-id", "hello");
  await waitFor(() => connection.acks.length === 1);
  expect(connection.acks[0].reason).toBe("draft_present");
  expect(asset.calls).toEqual([]);
});

registeredChannelTest("Pi draft query ignores invalid correlation and changed session without observing the editor", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  let reads = 0;
  const context = draftContext({ getEditorText: () => { reads += 1; return "private"; } });
  await asset.handlers.get("session_start")?.({}, context);
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  for (const fields of [
    { registration_epoch: "old" }, { session_generation: "old" }, { request_id: "" },
    { request_id: 1 }, { request_id: null }, { request_id: "🐑".repeat(257) },
    { registration_epoch: null }, { session_generation: null },
  ]) queryDraft(connection, "invalid", fields);
  context.userMessageSessionGeneration = "session-2";
  queryDraft(connection);
  await Bun.sleep(30);
  expect(reads).toBe(0);
  expect(connection.draftStates).toEqual([]);
  expect(asset.calls).toEqual([]);
});

registeredChannelTest("Pi guarded expired delayed frame rejects before UI access and preserves draft byte-identically", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const editor = " \tprivate unsent draft 👩🏽‍💻 e\u0301\n";
  let reads = 0;
  let mutations = 0;
  await asset.handlers.get("session_start")?.({}, draftContext({
    getEditorText: () => { reads += 1; return editor; },
    holdState: () => { reads += 1; return undefined; },
    setEditorText: () => { mutations += 1; }, pasteToEditor: () => { mutations += 1; },
  }));
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  const frame = { ...delivery(connection, "expired"), if_draft_empty: true, deadline_ms: Date.now() + 10 };
  const before = Buffer.from(editor);
  await Bun.sleep(20); // A frame delayed in transit arrives after its first-reservation deadline.
  connection.socket.write(`${JSON.stringify(frame)}\n`);
  await waitFor(() => connection.acks.length === 1);
  expect(connection.acks[0]).toMatchObject({ status: "rejected", reason: "expired" });
  expect(asset.calls).toEqual([]);
  expect(reads).toBe(0);
  expect(mutations).toBe(0);
  expect(Buffer.from(editor)).toEqual(before);
  // Expiry is a retained result. A later timeout/deadline value is not payload identity and
  // cannot renew the first reservation or change a rejected request into an admission.
  guardedDeliver(connection, "expired", "hello");
  await waitFor(() => connection.acks.length === 2);
  expect(connection.acks[1]).toMatchObject({ reason: "expired", duplicate: true });
  expect(asset.calls).toEqual([]);
});

for (const [name, deadline] of [
  ["missing", undefined], ["null", null], ["string", "9999999999999"],
  ["fractional", 9999999999999.5], ["unsafe", Number.MAX_SAFE_INTEGER + 1],
  ["object", {}], ["array", []],
] as const) {
  registeredChannelTest(`Pi guarded deadline ${name} fails closed unknown before UI access`, async () => {
    const { connections } = await startChannelServer();
    const asset = await loadChannelAsset(accepted);
    let reads = 0;
    await asset.handlers.get("session_start")?.({}, draftContext({
      getEditorText: () => { reads += 1; return "private draft"; },
      holdState: () => { reads += 1; return undefined; },
    }));
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    connection.socket.write(`${JSON.stringify({ ...delivery(connection), if_draft_empty: true, deadline_ms: deadline })}\n`);
    await waitFor(() => connection.acks.length === 1);
    expect(connection.acks[0]).toMatchObject({ status: "rejected", reason: "unknown" });
    expect(asset.calls).toEqual([]);
    expect(reads).toBe(0);
  });
}

registeredChannelTest("Pi guarded deadline elapsed during synchronous UI inspection refuses before submit", async () => {
  const { connections } = await startChannelServer();
  const asset = await loadChannelAsset(accepted);
  const deadline = Date.now() + 60_000;
  const originalNow = Date.now;
  try {
    await asset.handlers.get("session_start")?.({}, draftContext({
      getEditorText: () => "",
      holdState: () => {
        Date.now = () => deadline;
        queueMicrotask(() => { Date.now = originalNow; });
        return undefined;
      },
    }));
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    connection.socket.write(`${JSON.stringify({ ...delivery(connection), if_draft_empty: true, deadline_ms: deadline })}\n`);
    await waitFor(() => connection.acks.length === 1);
    expect(connection.acks[0]).toMatchObject({ status: "rejected", reason: "expired" });
    expect(asset.calls).toEqual([]);
  } finally {
    Date.now = originalNow;
  }
});

for (const deadline of ["1e999", "-1e999"]) {
  registeredChannelTest(`Pi guarded deadline nonfinite ${deadline} fails closed unknown`, async () => {
    const { connections } = await startChannelServer();
    const asset = await loadChannelAsset(accepted);
    await asset.handlers.get("session_start")?.({}, draftContext());
    await waitFor(() => connections.length === 1);
    const connection = connections[0];
    // JSON.parse accepts exponent overflow as Infinity; validation must still reject it.
    const frame = JSON.stringify({ ...delivery(connection), if_draft_empty: true, deadline_ms: "deadline-placeholder" });
    connection.socket.write(`${frame.replace('"deadline-placeholder"', deadline)}\n`);
    await waitFor(() => connection.acks.length === 1);
    expect(connection.acks[0]).toMatchObject({ status: "rejected", reason: "unknown" });
    expect(asset.calls).toEqual([]);
  });
}

registeredChannelTest("Pi guarded expiry after possible admission does not invent a rejection or resubmit", async () => {
  const { connections } = await startChannelServer();
  const pending = deferred<AdmissionReceipt>();
  const asset = await loadChannelAsset(() => pending.promise);
  await asset.handlers.get("session_start")?.({}, draftContext());
  await waitFor(() => connections.length === 1);
  const connection = connections[0];
  const deadline = Date.now() + 100;
  connection.socket.write(`${JSON.stringify({ ...delivery(connection), if_draft_empty: true, deadline_ms: deadline })}\n`);
  await waitFor(() => asset.calls.length === 1);
  await Bun.sleep(Math.max(0, deadline - Date.now()) + 10);
  expect(Date.now() >= deadline).toBe(true);
  expect(connection.acks).toEqual([]); // No proof of non-admission while the receipt is pending.
  pending.reject(new Error("possibly admitted before timeout"));
  await waitFor(() => connection.closed && connections.length === 2);
  expect(connection.acks).toEqual([]); // Herdr must retain delivery_unknown, never expired/zero sent.
  expect(connections[1].acks).toEqual([]);
  expect(asset.calls).toHaveLength(1);
});
