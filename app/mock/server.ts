// lmux mock daemon: bun websocket on 127.0.0.1:7900.
//
// Implements enough of the app<->daemon protocol to exercise the whole client:
// a two-folder tree with terminals and browsers, terminal echo + periodic
// output with snapshot-on-subscribe, a JPEG browser screencast paced by frame
// acks, and RPCs that mutate the tree and rebroadcast it.
//
// Run: bun app/mock/server.ts   (PORT / HOST env override)

import jpeg from "jpeg-js";
import {
  encodeFrame,
  PROTOCOL_VERSION,
  type BrowserInputEvent,
  type ClientMsg,
  type Layout,
  type Pane,
  type Ref,
  type SplitDirection,
  type SplitSide,
  type Surface,
} from "./proto";

const PORT = Number(process.env.PORT ?? 7900);
const HOST = process.env.HOST ?? "127.0.0.1";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

type Agent = "none" | "running" | "idle" | "needs_input";

interface TermState {
  kind: "terminal";
  id: Ref;
  workspace: Ref;
  cols: number;
  rows: number;
  history: string[];
  pending: string;
  seq: number;
  prompt: string;
}
interface BrowserState {
  kind: "browser";
  id: Ref;
  workspace: Ref;
  url: string;
  title: string;
  loading: boolean;
  canGoBack: boolean;
  canGoForward: boolean;
  width: number;
  height: number;
  dsf: number;
  seq: number;
  inFlight: number | null;
  timer?: ReturnType<typeof setInterval>;
}

interface PaneState {
  id: Ref;
  surfaces: Ref[];
  active: Ref | null;
}

interface WorkspaceState {
  id: Ref;
  title: string;
  folder: Ref | null;
  cwd: string;
  git_branch: string | null;
  agent: Agent;
  summary: string | null;
  unread: number;
  pills: { key: string; value: string; icon: string | null; color: string | null }[];
  progress: { value: number; label: string | null } | null;
  layout: Layout;
  panes: Map<Ref, PaneState>;
  focused: Ref | null;
}

interface NotifState {
  id: string;
  title: string;
  subtitle: string | null;
  body: string | null;
  workspace: Ref | null;
  surface: Ref | null;
  created_at: number;
  read: boolean;
}

interface SurfaceState {
  id: Ref;
  title: string;
  workspace: Ref;
  kind: "terminal" | "browser";
}

interface FolderState {
  id: Ref;
  name: string;
  parent: Ref | null;
  collapsed: boolean;
}

const state = {
  host: "mock-lintlabs",
  folders: [
    { id: "folder:1", name: "webapp", parent: null, collapsed: false },
    // Nested one level (folder inside a folder), so the sidebar tree is real.
    { id: "folder:3", name: "api", parent: "folder:1", collapsed: false },
    { id: "folder:2", name: "lintlabs", parent: null, collapsed: false },
  ] as FolderState[],
  workspaces: [] as WorkspaceState[],
  selected: null as Ref | null,
  surfaces: new Map<Ref, SurfaceState>(),
  term: new Map<Ref, TermState>(),
  browser: new Map<Ref, BrowserState>(),
  notifs: [] as NotifState[],
  nextId: 1,
  subscribers: new Map<Ref, Set<unknown>>(),
  clients: new Set<any>(),
};

let idCounter = 1;
const nid = (prefix: string) => `${prefix}:${idCounter++}`;

// ---------------------------------------------------------------------------
// Terminal + browser behaviour
// ---------------------------------------------------------------------------

function termHistory(id: Ref): TermState {
  const t = state.term.get(id);
  if (!t) throw new Error(`no terminal ${id}`);
  return t;
}

function pushLine(t: TermState, line: string) {
  t.history.push(line);
  if (t.history.length > 500) t.history.splice(0, t.history.length - 500);
}

function streamTerm(t: TermState, text: string) {
  const subs = state.subscribers.get(t.id);
  if (!subs || subs.size === 0) return;
  const bytes = new TextEncoder().encode(text);
  const frame = encodeFrame({ kind: "term_output", surface: t.id, data: bytes });
  for (const ws of subs) safeSend(ws, frame);
}

function handleTermInput(t: TermState, data: string) {
  for (const ch of data) {
    if (ch === "\r" || ch === "\n") {
      const line = `${t.prompt}${t.pending}`;
      pushLine(t, line);
      streamTerm(t, "\r\n" + line + "\r\n" + t.prompt);
      t.pending = "";
    } else if (ch === "\x7f") {
      if (t.pending.length > 0) {
        t.pending = t.pending.slice(0, -1);
        streamTerm(t, "\b \b");
      }
    } else if (ch === "\x03") {
      t.pending = "";
      streamTerm(t, "^C\r\n" + t.prompt);
    } else if (ch >= " ") {
      t.pending += ch;
      streamTerm(t, ch);
    }
    // control/escape sequences are ignored by the mock
  }
}

function snapshotTerm(t: TermState) {
  const rows = Math.max(1, t.rows);
  const visible = t.history.slice(-rows);
  let out = "\x1b[2J\x1b[H";
  out += visible.join("\r\n");
  if (t.pending) out += "\r\n" + t.prompt + t.pending;
  return out;
}

function startBrowser(b: BrowserState) {
  if (b.timer) return;
  b.timer = setInterval(() => {
    if (b.inFlight !== null) return; // wait for the ack: paced screencast
    const subs = state.subscribers.get(b.id);
    if (!subs || subs.size === 0) return;
    b.seq += 1;
    b.inFlight = b.seq;
    const jpegBytes = renderFrame(b, b.seq);
    const frame = encodeFrame({
      kind: "browser_frame",
      surface: b.id,
      seq: b.seq,
      width: Math.min(1000, b.width),
      height: Math.min(700, b.height),
      jpeg: jpegBytes,
    });
    for (const ws of subs) safeSend(ws, frame);
    log(`browser ${b.id} frame seq=${b.seq} (${jpegBytes.length}b)`);
  }, 250);
}

function stopBrowser(b: BrowserState) {
  if (b.timer) clearInterval(b.timer);
  b.timer = undefined;
  b.inFlight = null;
}

function broadcastBrowserMeta(b: BrowserState) {
  sendAll({
    type: "browser_meta",
    surface: b.id,
    url: b.url,
    title: b.title,
    loading: b.loading,
    can_go_back: b.canGoBack,
    can_go_forward: b.canGoForward,
  });
}

// ---------------------------------------------------------------------------
// JPEG frame generation (a crude animated "web page")
// ---------------------------------------------------------------------------

function renderFrame(b: BrowserState, seq: number): Uint8Array {
  const width = Math.max(64, Math.min(1000, Math.round(b.width)));
  const height = Math.max(64, Math.min(700, Math.round(b.height)));
  const buf = new Uint8Array(width * height * 4);
  const hue = (seq * 12) % 360;
  const accent = hsl(hue, 70, 55);
  const accent2 = hsl((hue + 140) % 360, 65, 60);

  fill(buf, width, height, 0, 0, width, height, [246, 247, 250, 255]);

  // Browser chrome
  fill(buf, width, height, 0, 0, width, 46, [226, 229, 236, 255]);
  fill(buf, width, height, 0, 44, width, 2, [200, 204, 214, 255]);
  circle(buf, width, height, 20, 23, 6, [255, 95, 86, 255]);
  circle(buf, width, height, 40, 23, 6, [255, 189, 46, 255]);
  circle(buf, width, height, 60, 23, 6, [39, 201, 63, 255]);
  const barX = 84;
  const barW = Math.max(20, width - barX - 16);
  fill(buf, width, height, barX, 12, barW, 22, [255, 255, 255, 255]);
  fill(buf, width, height, barX + 8, 18, Math.min(barW - 16, 220), 3, [120, 126, 140, 255]);
  fill(buf, width, height, barX + 8, 25, Math.min(barW - 16, 140), 3, [160, 166, 180, 255]);

  // Page header band
  fill(buf, width, height, 0, 46, width, 70, accent);
  fill(buf, width, height, 0, 114, width, 2, [0, 0, 0, 40]);

  // Big 7-segment frame counter
  const digits = String(seq);
  const dw = 46;
  const dh = 74;
  let dx = 32;
  const dy = 140;
  for (const d of digits) {
    drawDigit(buf, width, height, dx, dy, dw, dh, d.charCodeAt(0) - 48, accent2);
    dx += dw + 16;
  }

  // Fake text lines
  const lines = [
    [0.62, 16],
    [0.9, 16],
    [0.5, 16],
    [0.78, 16],
    [0.34, 16],
  ];
  let ly = dy + dh + 30;
  for (const [frac, h] of lines) {
    if (ly + h > height - 60) break;
    fill(buf, width, height, 32, ly, Math.round((width - 64) * frac), h, [206, 210, 220, 255]);
    ly += h + 14;
  }

  // Moving accent square: makes motion obvious and proves streaming.
  const boxW = 54;
  const span = Math.max(1, width - 64 - boxW);
  const bx = 32 + Math.round(span * (0.5 + 0.5 * Math.sin(seq / 6)));
  const by = height - 60;
  fill(buf, width, height, bx, by, boxW, 34, accent2);

  // Status bar
  fill(buf, width, height, 0, height - 20, width, 20, [40, 44, 54, 255]);

  const img = { data: buf, width, height };
  return jpeg.encode(img, 62).data;
}

function fill(
  buf: Uint8Array,
  w: number,
  h: number,
  x: number,
  y: number,
  rw: number,
  rh: number,
  color: [number, number, number, number],
) {
  const x0 = Math.max(0, Math.round(x));
  const y0 = Math.max(0, Math.round(y));
  const x1 = Math.min(w, Math.round(x + rw));
  const y1 = Math.min(h, Math.round(y + rh));
  for (let yy = y0; yy < y1; yy++) {
    let i = (yy * w + x0) * 4;
    for (let xx = x0; xx < x1; xx++) {
      buf[i] = color[0];
      buf[i + 1] = color[1];
      buf[i + 2] = color[2];
      buf[i + 3] = color[3];
      i += 4;
    }
  }
}

function circle(
  buf: Uint8Array,
  w: number,
  h: number,
  cx: number,
  cy: number,
  r: number,
  color: [number, number, number, number],
) {
  for (let y = cy - r; y <= cy + r; y++) {
    for (let x = cx - r; x <= cx + r; x++) {
      if ((x - cx) ** 2 + (y - cy) ** 2 > r * r) continue;
      if (x < 0 || y < 0 || x >= w || y >= h) continue;
      const i = (y * w + x) * 4;
      buf[i] = color[0];
      buf[i + 1] = color[1];
      buf[i + 2] = color[2];
      buf[i + 3] = color[3];
    }
  }
}

const SEGMENTS: Record<number, string> = {
  0: "abcdef",
  1: "bc",
  2: "abged",
  3: "abgcd",
  4: "fgbc",
  5: "afgcd",
  6: "afgecd",
  7: "abc",
  8: "abcdefg",
  9: "abfgcd",
};

function drawDigit(
  buf: Uint8Array,
  w: number,
  h: number,
  x: number,
  y: number,
  dw: number,
  dh: number,
  digit: number,
  color: [number, number, number, number],
) {
  const segs = SEGMENTS[digit] ?? "";
  const t = Math.max(3, Math.round(dw * 0.18));
  const rects: Record<string, [number, number, number, number]> = {
    a: [x + t, y, dw - 2 * t, t],
    b: [x + dw - t, y + t, t, dh / 2 - t],
    c: [x + dw - t, y + dh / 2, t, dh / 2 - t],
    d: [x + t, y + dh - t, dw - 2 * t, t],
    e: [x, y + dh / 2, t, dh / 2 - t],
    f: [x, y + t, t, dh / 2 - t],
    g: [x + t, y + dh / 2 - t / 2, dw - 2 * t, t],
  };
  for (const s of segs) {
    const r = rects[s];
    if (r) fill(buf, w, h, r[0], r[1], r[2], r[3], color);
  }
}

function hsl(h: number, s: number, l: number): [number, number, number, number] {
  s /= 100;
  l /= 100;
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return [
    Math.round(255 * f(0)),
    Math.round(255 * f(8)),
    Math.round(255 * f(4)),
    255,
  ];
}

// ---------------------------------------------------------------------------
// Tree serialization
// ---------------------------------------------------------------------------

function surfaceWire(id: Ref): Surface {
  const s = state.surfaces.get(id)!;
  if (s.kind === "terminal") {
    const t = state.term.get(id)!;
    return { id, title: s.title, kind: "terminal", tmux_pane: `%${id.split(":")[1]}`, cols: t.cols, rows: t.rows };
  }
  const b = state.browser.get(id)!;
  return {
    id,
    title: s.title,
    kind: "browser",
    url: b.url,
    loading: b.loading,
    can_go_back: b.canGoBack,
    can_go_forward: b.canGoForward,
  };
}

function paneWire(p: PaneState): Pane {
  return {
    id: p.id,
    surfaces: p.surfaces.map(surfaceWire),
    active_surface: p.active,
  };
}

function buildTree() {
  return {
    host: state.host,
    folders: state.folders,
    workspaces: state.workspaces.map((w) => ({
      id: w.id,
      title: w.title,
      folder: w.folder,
      cwd: w.cwd,
      git_branch: w.git_branch,
      agent: w.agent,
      summary: w.summary,
      unread_notifications: w.unread,
      status_pills: w.pills,
      progress: w.progress,
      layout: w.layout,
      panes: [...w.panes.values()].map(paneWire),
      focused_pane: w.focused,
    })),
    selected_workspace: state.selected,
  };
}

function broadcastTree() {
  sendAll({ type: "tree", tree: buildTree() });
}

// Whether `id` sits anywhere under `ancestor` (a folder is not its own
// descendant). Mirrors `SessionState::is_descendant_of` in the daemon.
function isDescendantOf(id: Ref, ancestor: Ref): boolean {
  let cursor: Ref | null = state.folders.find((f) => f.id === id)?.parent ?? null;
  // The folder list is finite, so a malformed cycle cannot loop forever.
  for (let i = 0; i <= state.folders.length && cursor; i++) {
    if (cursor === ancestor) return true;
    cursor = state.folders.find((f) => f.id === cursor)?.parent ?? null;
  }
  return false;
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

function log(...args: unknown[]) {
  console.log(`[mock ${new Date().toISOString().slice(11, 19)}]`, ...args);
}

function safeSend(ws: any, data: string | Uint8Array) {
  try {
    ws.send(data);
  } catch {
    /* closed */
  }
}

function sendAll(msg: unknown) {
  const text = JSON.stringify(msg);
  for (const ws of state.clients) safeSend(ws, text);
}

function send(ws: any, msg: unknown) {
  safeSend(ws, JSON.stringify(msg));
}

// ---------------------------------------------------------------------------
// RPC
// ---------------------------------------------------------------------------

const server = Bun.serve({
  hostname: HOST,
  port: PORT,
  fetch(req, srv) {
    const url = new URL(req.url);
    if (url.pathname === "/health") return new Response("ok");
    if (srv.upgrade(req)) return undefined;
    return new Response("lmux mock; connect with websocket", { status: 426 });
  },
  websocket: {
    open(ws: any) {
      state.clients.add(ws);
      log(`client connected (${state.clients.size})`);
    },
    close(ws: any) {
      state.clients.delete(ws);
      for (const subs of state.subscribers.values()) subs.delete(ws);
      for (const b of state.browser.values()) if (!subs(b.id)) stopBrowser(b);
      log(`client disconnected (${state.clients.size})`);
    },
    message(ws: any, raw: string | Uint8Array) {
      if (typeof raw !== "string") return; // client -> daemon is text only
      let msg: ClientMsg;
      try {
        msg = JSON.parse(raw) as ClientMsg;
      } catch {
        log("bad json", raw.slice(0, 80));
        return;
      }
      handle(ws, msg);
    },
  },
});

function subs(id: Ref): boolean {
  const s = state.subscribers.get(id);
  return !!s && s.size > 0;
}

function handle(ws: any, msg: ClientMsg) {
  switch (msg.type) {
    case "hello": {
      send(ws, {
        type: "welcome",
        protocol_version: PROTOCOL_VERSION,
        server_version: "mock-0.1.0",
        host: state.host,
      });
      send(ws, { type: "tree", tree: buildTree() });
      log("hello -> welcome + tree");
      break;
    }
    case "subscribe": {
      let set = state.subscribers.get(msg.surface);
      if (!set) {
        set = new Set();
        state.subscribers.set(msg.surface, set);
      }
      set.add(ws);
      const s = state.surfaces.get(msg.surface);
      if (s?.kind === "terminal") {
        const t = termHistory(msg.surface);
        send(ws, {
          type: "term_snapshot",
          surface: t.id,
          cols: t.cols,
          rows: t.rows,
          data: snapshotTerm(t),
        });
        log(`subscribe ${msg.surface} -> term_snapshot (${t.history.length} lines)`);
      } else if (s?.kind === "browser") {
        const b = state.browser.get(msg.surface)!;
        startBrowser(b);
        broadcastBrowserMeta(b);
        log(`subscribe ${msg.surface} -> browser_meta`);
      } else {
        log(`subscribe ${msg.surface} -> unknown`);
      }
      break;
    }
    case "unsubscribe": {
      state.subscribers.get(msg.surface)?.delete(ws);
      const b = state.browser.get(msg.surface);
      if (b && !subs(b.id)) stopBrowser(b);
      log(`unsubscribe ${msg.surface}`);
      break;
    }
    case "term_input": {
      const t = state.term.get(msg.surface);
      if (t) handleTermInput(t, msg.data);
      break;
    }
    case "term_resize": {
      const t = state.term.get(msg.surface);
      if (t) {
        t.cols = msg.cols;
        t.rows = msg.rows;
      }
      break;
    }
    case "browser_resize": {
      const b = state.browser.get(msg.surface);
      if (b) {
        b.width = msg.width;
        b.height = msg.height;
        b.dsf = msg.device_scale_factor;
        b.inFlight = null;
        if (b.timer) {
          clearInterval(b.timer);
          b.timer = undefined;
          startBrowser(b);
        }
      }
      break;
    }
    case "browser_frame_ack": {
      const b = state.browser.get(msg.surface);
      if (b && b.inFlight === msg.seq) b.inFlight = null;
      break;
    }
    case "browser_input": {
      log(`browser_input ${msg.surface}: ${summarizeInput(msg.event)}`);
      break;
    }
    case "rpc": {
      handleRpc(ws, msg.request as any);
      break;
    }
  }
}

function summarizeInput(e: BrowserInputEvent) {
  if (e.type === "key") return `key ${e.action} ${e.key}${e.text ? ` text=${JSON.stringify(e.text)}` : ""}`;
  if (e.type === "mouse") return `mouse ${e.action} ${e.button} @${Math.round(e.x)},${Math.round(e.y)}`;
  if (e.type === "wheel") return `wheel dy=${Math.round(e.delta_y)} @${Math.round(e.x)},${Math.round(e.y)}`;
  return `insert_text ${JSON.stringify(e.text).slice(0, 40)}`;
}

function handleRpc(ws: any, req: { id: number; method: string; params: Record<string, unknown> }) {
  const p = req.params ?? {};
  const result: Record<string, unknown> = {};
  const err = (code: string, message: string) =>
    send(ws, { type: "rpc_result", response: { id: req.id, error: { code, message } } });

  const wsOf = (id: unknown) => state.workspaces.find((w) => w.id === id);
  const paneOwning = (surface: unknown) => {
    for (const w of state.workspaces)
      for (const p of w.panes.values()) if (p.surfaces.includes(surface as Ref)) return p;
    return undefined;
  };
  const focusedWs = () => wsOf(state.selected) ?? state.workspaces[0];

  try {
    switch (req.method) {
      case "system.ping":
        result.pong = true;
        break;
      case "system.tree":
        result.tree = buildTree();
        break;
      case "workspace.select": {
        const w = wsOf(p.workspace);
        if (!w) return err("not_found", "no such workspace");
        state.selected = w.id;
        broadcastTree();
        result.workspace = w.id;
        break;
      }
      case "workspace.new": {
        const w = newWorkspace(
          (p.name as string) ?? "workspace",
          (p.folder as Ref | null) ?? null,
          (p.command as string) ?? "zsh",
        );
        state.selected = w.id;
        broadcastTree();
        result.workspace = w.id;
        result.pane = w.focused;
        result.surface = w.panes.get(w.focused!)!.active;
        break;
      }
      case "workspace.rename": {
        const w = wsOf(p.workspace ?? state.selected);
        if (!w) return err("not_found", "no workspace");
        w.title = (p.title as string) ?? w.title;
        broadcastTree();
        break;
      }
      case "workspace.close": {
        const w = wsOf(p.workspace);
        if (!w) return err("not_found", "no workspace");
        closeWorkspace(w);
        broadcastTree();
        break;
      }
      case "workspace.move": {
        // Drag-and-drop in the sidebar: reparent the workspace and put it at
        // `index` in the list (the daemon's index counts the list *without* the
        // moved workspace, so a same-folder reorder is not off by one).
        const w = wsOf(p.workspace);
        if (!w) return err("not_found", "no workspace");
        const from = state.workspaces.indexOf(w);
        if (from >= 0) state.workspaces.splice(from, 1);
        w.folder = (p.folder as Ref | null) ?? null;
        const at =
          p.index === undefined || p.index === null
            ? state.workspaces.length
            : Math.max(0, Math.min(Number(p.index), state.workspaces.length));
        state.workspaces.splice(at, 0, w);
        broadcastTree();
        log(`workspace.move ${w.id} -> folder=${w.folder ?? "top"} index=${at}`);
        break;
      }
      case "folder.new": {
        const parent = (p.parent as Ref | null) ?? null;
        const f: FolderState = { id: nid("folder"), name: (p.name as string) ?? "folder", parent, collapsed: false };
        state.folders.push(f);
        broadcastTree();
        result.folder = f.id;
        break;
      }
      case "folder.move": {
        // Reparent/reorder. A folder cannot move into itself or a descendant:
        // that would detach the subtree, so the daemon refuses it too.
        const f = state.folders.find((x) => x.id === p.folder);
        if (!f) return err("not_found", "no such folder");
        const parent = (p.parent as Ref | null) ?? null;
        if (parent) {
          if (!state.folders.some((x) => x.id === parent)) return err("not_found", "no such parent");
          if (parent === f.id || isDescendantOf(parent, f.id)) {
            return err("invalid_params", "a folder cannot move into itself or one of its own folders");
          }
        }
        const from = state.folders.indexOf(f);
        if (from >= 0) state.folders.splice(from, 1);
        f.parent = parent;
        const siblings = state.folders.filter((x) => x.parent === parent);
        const at =
          p.index === undefined || p.index === null
            ? state.folders.length
            : (() => {
                const slot = Math.max(0, Math.min(Number(p.index), siblings.length));
                return slot < siblings.length ? state.folders.indexOf(siblings[slot]) : state.folders.length;
              })();
        state.folders.splice(at, 0, f);
        broadcastTree();
        result.folder = f.id;
        log(`folder.move ${f.id} -> parent=${f.parent ?? "top"}`);
        break;
      }
      case "folder.rename": {
        const f = state.folders.find((x) => x.id === p.folder);
        if (f) f.name = (p.name as string) ?? f.name;
        broadcastTree();
        break;
      }
      case "folder.close": {
        const closed = state.folders.find((x) => x.id === p.folder);
        const up = closed?.parent ?? null;
        state.folders = state.folders.filter((x) => x.id !== p.folder);
        for (const w of state.workspaces) if (w.folder === p.folder) w.folder = up;
        for (const f of state.folders) if (f.parent === p.folder) f.parent = up;
        broadcastTree();
        break;
      }
      case "pane.split": {
        const w = focusedWs();
        if (!w) return err("not_found", "no workspace");
        const target =
          w.panes.get(p.pane as Ref) ??
          paneOwning(p.surface) ??
          w.panes.get(w.focused!) ??
          [...w.panes.values()][0];
        if (!target) return err("not_found", "no target pane");
        const side = (p.side as SplitSide) ?? "right";
        const direction: SplitDirection = side === "left" || side === "right" ? "horizontal" : "vertical";
        const first = side === "left" || side === "up";
        const type = (p.type as string) ?? "terminal";
        const created = createPane(w, type, p);
        w.layout = splitLeaf(w.layout, target.id, direction, first, created.pane.id);
        w.focused = created.pane.id;
        broadcastTree();
        send(ws, { type: "tree", tree: buildTree() });
        result.pane = created.pane.id;
        result.surface = created.surface;
        log(`pane.split side=${side} pane=${created.pane.id} surface=${created.surface}`);
        break;
      }
      case "surface.new": {
        const w = focusedWs();
        if (!w) return err("not_found", "no workspace");
        const target = w.panes.get(p.pane as Ref) ?? w.panes.get(w.focused!) ?? [...w.panes.values()][0];
        if (!target) return err("not_found", "no pane");
        const type = (p.type as string) ?? "terminal";
        const sid = makeSurface(w, target, type, p);
        broadcastTree();
        result.surface = sid;
        log(`surface.new ${sid} in ${target.id}`);
        break;
      }
      case "surface.close": {
        const sid = (p.surface as Ref) ?? undefined;
        if (!sid) return err("invalid_params", "surface required");
        closeSurface(sid);
        broadcastTree();
        log(`surface.close ${sid}`);
        break;
      }
      case "surface.rename": {
        const s = state.surfaces.get(p.surface as Ref);
        if (s) s.title = (p.title as string) ?? s.title;
        broadcastTree();
        break;
      }
      case "pane.focus": {
        const w = focusedWs();
        if (!w) return err("not_found", "no workspace");
        if (w.panes.has(p.pane as Ref)) w.focused = p.pane as Ref;
        broadcastTree();
        break;
      }
      case "pane.resize": {
        const w = focusedWs();
        if (!w) return err("not_found", "no workspace");
        const ratio = Math.max(0.05, Math.min(0.95, Number(p.ratio) || 0.5));
        const path = p.path as number[] | undefined;
        if (Array.isArray(path)) {
          applyRatio(w, path, ratio);
        } else {
          // Fallback: nearest split whose first subtree contains the pane.
          const found = findSplitForPane(w.layout, p.pane as Ref, []);
          if (found) applyRatio(w, found, ratio);
        }
        broadcastTree();
        log(`pane.resize pane=${p.pane} ratio=${ratio.toFixed(3)}`);
        break;
      }
      case "surface.move": {
        // Drag-and-drop: move a tab within its pane, or into another pane. The
        // source pane is removed (and its split collapsed) when it empties.
        const sid = p.surface as Ref;
        const s = state.surfaces.get(sid);
        if (!s) return err("not_found", "no such surface");
        const w = wsOf(s.workspace);
        if (!w) return err("not_found", "no workspace");
        const target = w.panes.get(p.pane as Ref);
        if (!target) return err("not_found", "no target pane");
        const source = [...w.panes.values()].find((pn) => pn.surfaces.includes(sid));
        if (!source) return err("not_found", "surface has no pane");
        source.surfaces = source.surfaces.filter((x) => x !== sid);
        if (source.active === sid) source.active = source.surfaces[source.surfaces.length - 1] ?? null;
        const raw = p.index === undefined || p.index === null ? null : Number(p.index);
        const at = raw === null ? target.surfaces.length : Math.max(0, Math.min(raw, target.surfaces.length));
        target.surfaces.splice(at, 0, sid);
        target.active = sid;
        if (source !== target && source.surfaces.length === 0) {
          w.panes.delete(source.id);
          const layout = removePaneFromLayout(w.layout, source.id);
          if (layout) {
            w.layout = layout;
            if (w.focused === source.id) w.focused = target.id;
          } else {
            closeWorkspace(w);
          }
        }
        broadcastTree();
        result.pane = target.id;
        log(`surface.move ${sid} -> ${target.id} index=${at}${source !== target ? ` (from ${source.id})` : ""}`);
        break;
      }
      case "surface.split": {
        // Move an existing tab into a new pane split off `pane` — or off the
        // tab's own pane when `pane` is absent — on `side`. Mirrors the daemon's
        // `split_surface_into` / `split_surface`.
        const sid = p.surface as Ref;
        const s = state.surfaces.get(sid);
        if (!s) return err("not_found", "no such surface");
        const w = wsOf(s.workspace);
        if (!w) return err("not_found", "no workspace");
        let source = [...w.panes.values()].find((pn) => pn.surfaces.includes(sid));
        if (!source) return err("not_found", "surface has no pane");
        const targetRef = p.pane as Ref | undefined;
        if (targetRef !== undefined) {
          const target = w.panes.get(targetRef);
          if (!target) return err("not_found", "no target pane");
          // Move first, so a source pane left empty collapses here too.
          source.surfaces = source.surfaces.filter((x) => x !== sid);
          if (source.active === sid) source.active = source.surfaces[source.surfaces.length - 1] ?? null;
          target.surfaces.push(sid);
          target.active = sid;
          if (source !== target && source.surfaces.length === 0) {
            w.panes.delete(source.id);
            const layout = removePaneFromLayout(w.layout, source.id);
            if (layout) {
              w.layout = layout;
              if (w.focused === source.id) w.focused = target.id;
            } else {
              closeWorkspace(w);
            }
          }
          source = target;
        }
        // A pane holding only this surface has nothing to divide: no-op (matches
        // the daemon's `split_surface`).
        if (source.surfaces.length < 2) {
          result.pane = source.id;
          result.surface = sid;
          break;
        }
        const side = (p.side as SplitSide) ?? "right";
        const created: PaneState = { id: nid("pane"), surfaces: [], active: null };
        w.panes.set(created.id, created);
        source.surfaces = source.surfaces.filter((x) => x !== sid);
        if (source.active === sid) source.active = source.surfaces[source.surfaces.length - 1] ?? null;
        created.surfaces.push(sid);
        created.active = sid;
        const direction: SplitDirection = side === "left" || side === "right" ? "horizontal" : "vertical";
        const first = side === "left" || side === "up";
        w.layout = splitLeaf(w.layout, source.id, direction, first, created.id);
        w.focused = created.id;
        if (source.surfaces.length === 0) {
          w.panes.delete(source.id);
          const layout = removePaneFromLayout(w.layout, source.id);
          if (layout) w.layout = layout;
          else closeWorkspace(w);
        }
        broadcastTree();
        result.pane = created.id;
        result.surface = sid;
        log(`surface.split ${sid} side=${side} -> ${created.id}`);
        break;
      }
      case "pane.swap": {
        // Exchange two panes' positions in the layout tree.
        const a = p.pane as Ref;
        const b = p.other as Ref;
        if (a === b) break;
        const w = state.workspaces.find((x) => x.panes.has(a) && x.panes.has(b));
        if (!w) return err("not_found", "panes not in the same workspace");
        w.layout = swapLeaves(w.layout, a, b);
        broadcastTree();
        log(`pane.swap ${a} <-> ${b}`);
        break;
      }
      case "notify.create": {
        addNotification({
          title: (p.title as string) ?? "Notification",
          subtitle: (p.subtitle as string) ?? null,
          body: (p.body as string) ?? null,
          workspace: (p.workspace as Ref) ?? state.selected,
          surface: (p.surface as Ref) ?? null,
        });
        break;
      }
      case "notify.list":
        result.notifications = state.notifs;
        break;
      case "notify.clear": {
        const w = p.workspace as Ref | undefined;
        state.notifs = w ? state.notifs.filter((n) => n.workspace !== w) : [];
        recountUnread();
        broadcastTree();
        break;
      }
      case "notify.mark_read": {
        const byId = p.id as string | undefined;
        const byWs = p.workspace as Ref | undefined;
        for (const n of state.notifs) {
          if ((byId && n.id === byId) || (byWs && n.workspace === byWs) || (!byId && !byWs)) n.read = true;
        }
        recountUnread();
        broadcastTree();
        break;
      }
      case "status.set": {
        const w = wsOf(p.workspace ?? state.selected);
        if (!w) return err("not_found", "no workspace");
        const key = p.key as string;
        w.pills = w.pills.filter((x) => x.key !== key);
        w.pills.push({
          key,
          value: (p.value as string) ?? "",
          icon: (p.icon as string) ?? null,
          color: (p.color as string) ?? null,
        });
        broadcastTree();
        break;
      }
      case "status.clear": {
        const w = wsOf(p.workspace ?? state.selected);
        if (w) w.pills = w.pills.filter((x) => x.key !== p.key);
        broadcastTree();
        break;
      }
      case "progress.set": {
        const w = wsOf(p.workspace ?? state.selected);
        if (!w) return err("not_found", "no workspace");
        w.progress = { value: Number(p.value) || 0, label: (p.label as string) ?? null };
        broadcastTree();
        break;
      }
      case "progress.clear": {
        const w = wsOf(p.workspace ?? state.selected);
        if (w) w.progress = null;
        broadcastTree();
        break;
      }
      case "agent.state": {
        const w = wsOf(p.workspace ?? state.selected);
        if (!w) return err("not_found", "no workspace");
        w.agent = (p.state as Agent) ?? w.agent;
        if (p.summary !== undefined) w.summary = (p.summary as string) ?? null;
        broadcastTree();
        break;
      }
      case "browser.goto": {
        const b = findBrowser(p.surface as Ref) ?? findBrowser(undefined);
        if (!b) return err("not_found", "no browser");
        b.url = (p.url as string) ?? b.url;
        b.title = titleForUrl(b.url);
        b.canGoBack = true;
        b.loading = true;
        broadcastBrowserMeta(b);
        setTimeout(() => {
          b.loading = false;
          broadcastBrowserMeta(b);
          broadcastTree();
        }, 400);
        broadcastTree();
        log(`browser.goto ${b.id} ${b.url}`);
        break;
      }
      case "browser.back":
      case "browser.forward":
      case "browser.reload": {
        const b = findBrowser(p.surface as Ref) ?? findBrowser(undefined);
        if (!b) return err("not_found", "no browser");
        if (req.method === "browser.back") b.canGoForward = true;
        if (req.method === "browser.forward") b.canGoBack = true;
        b.loading = true;
        broadcastBrowserMeta(b);
        setTimeout(() => {
          b.loading = false;
          broadcastBrowserMeta(b);
          broadcastTree();
        }, 400);
        log(`${req.method} ${b.id}`);
        break;
      }
      case "browser.url": {
        const b = findBrowser(p.surface as Ref) ?? findBrowser(undefined);
        if (!b) return err("not_found", "no browser");
        result.url = b.url;
        result.title = b.title;
        break;
      }
      default:
        return err("unknown_method", req.method);
    }
    send(ws, { type: "rpc_result", response: { id: req.id, result } });
  } catch (e) {
    err("internal", String(e));
  }
}

function titleForUrl(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

function findBrowser(surface?: Ref): BrowserState | undefined {
  if (surface) return state.browser.get(surface);
  for (const w of state.workspaces) {
    if (w.id !== state.selected) continue;
    for (const p of w.panes.values())
      for (const s of p.surfaces) if (state.browser.has(s)) return state.browser.get(s);
  }
  for (const b of state.browser.values()) return b;
  return undefined;
}

// ---------------------------------------------------------------------------
// Tree mutation helpers
// ---------------------------------------------------------------------------

function splitLeaf(
  node: Layout,
  target: Ref,
  direction: SplitDirection,
  first: boolean,
  newPane: Ref,
): Layout {
  if (node.type === "pane") {
    if (node.pane !== target) return node;
    const newLeaf: Layout = { type: "pane", pane: newPane };
    return {
      type: "split",
      direction,
      ratio: 0.5,
      first: first ? newLeaf : node,
      second: first ? node : newLeaf,
    };
  }
  return {
    ...node,
    first: splitLeaf(node.first, target, direction, first, newPane),
    second: splitLeaf(node.second, target, direction, first, newPane),
  };
}

/** Swap the leaves naming `a` and `b`, so two panes exchange on-screen slots. */
function swapLeaves(node: Layout, a: Ref, b: Ref): Layout {
  if (node.type === "pane") {
    if (node.pane === a) return { type: "pane", pane: b };
    if (node.pane === b) return { type: "pane", pane: a };
    return node;
  }
  return {
    ...node,
    first: swapLeaves(node.first, a, b),
    second: swapLeaves(node.second, a, b),
  };
}

function containsPane(node: Layout, pane: Ref): boolean {
  if (node.type === "pane") return node.pane === pane;
  return containsPane(node.first, pane) || containsPane(node.second, pane);
}

/** Deepest-first split whose `first` subtree holds the pane. Returns its path. */
function findSplitForPane(node: Layout, pane: Ref, path: number[]): number[] | null {
  if (node.type === "pane") return null;
  const deepFirst = findSplitForPane(node.first, pane, [...path, 0]);
  if (deepFirst) return deepFirst;
  if (containsPane(node.first, pane)) return path;
  const deepSecond = findSplitForPane(node.second, pane, [...path, 1]);
  if (deepSecond) return deepSecond;
  if (containsPane(node.second, pane)) return path;
  return null;
}

function applyRatio(w: WorkspaceState, path: number[], ratio: number) {
  let node: Layout = w.layout;
  for (const idx of path) {
    if (node.type !== "split") return;
    node = idx === 0 ? node.first : node.second;
  }
  if (node.type === "split") node.ratio = ratio;
}

function removePaneFromLayout(node: Layout, pane: Ref): Layout | null {
  if (node.type === "pane") return node.pane === pane ? null : node;
  const first = removePaneFromLayout(node.first, pane);
  const second = removePaneFromLayout(node.second, pane);
  if (!first && !second) return null;
  if (!first) return second;
  if (!second) return first;
  return { ...node, first, second };
}

function makeSurface(w: WorkspaceState, pane: PaneState, type: string, params: Record<string, unknown>): Ref {
  if (type === "browser") {
    const id = nid("surface");
    const url = (params.url as string) ?? "https://example.com/";
    state.surfaces.set(id, { id, title: titleForUrl(url), workspace: w.id, kind: "browser" });
    state.browser.set(id, {
      kind: "browser",
      id,
      workspace: w.id,
      url,
      title: titleForUrl(url),
      loading: false,
      canGoBack: true,
      canGoForward: false,
      width: 800,
      height: 500,
      dsf: 2,
      seq: 0,
      inFlight: null,
    });
    pane.surfaces.push(id);
    pane.active = id;
    return id;
  }
  const id = nid("surface");
  const command = (params.command as string) ?? "zsh";
  state.surfaces.set(id, { id, title: command, workspace: w.id, kind: "terminal" });
  const t: TermState = {
    kind: "terminal",
    id,
    workspace: w.id,
    cols: 80,
    rows: 24,
    history: [
      `\x1b[1;32m${command}\x1b[0m — lmux mock terminal`,
      `Welcome. Type here; input echoes back.`,
    ],
    pending: "",
    seq: 0,
    prompt: "\x1b[1;34m❯\x1b[0m ",
  };
  state.term.set(id, t);
  pane.surfaces.push(id);
  pane.active = id;
  return id;
}

function createPane(w: WorkspaceState, type: string, params: Record<string, unknown>): { pane: PaneState; surface: Ref } {
  const pane: PaneState = { id: nid("pane"), surfaces: [], active: null };
  w.panes.set(pane.id, pane);
  const surface = makeSurface(w, pane, type, params);
  return { pane, surface };
}

function newWorkspace(name: string, folder: Ref | null, command: string): WorkspaceState {
  const pane: PaneState = { id: nid("pane"), surfaces: [], active: null };
  const w: WorkspaceState = {
    id: nid("workspace"),
    title: name,
    folder,
    cwd: `/home/vas/projects/${name.toLowerCase().replace(/\s+/g, "-")}`,
    git_branch: "main",
    agent: "none",
    summary: null,
    unread: 0,
    pills: [],
    progress: null,
    layout: { type: "pane", pane: pane.id },
    panes: new Map([[pane.id, pane]]),
    focused: pane.id,
  };
  state.workspaces.push(w);
  makeSurface(w, pane, "terminal", { command });
  return w;
}

function closeSurface(sid: Ref) {
  const s = state.surfaces.get(sid);
  if (!s) return;
  const w = state.workspaces.find((x) => x.id === s.workspace);
  state.surfaces.delete(sid);
  state.term.delete(sid);
  state.browser.delete(sid);
  state.subscribers.delete(sid);
  if (!w) return;
  const pane = [...w.panes.values()].find((p) => p.surfaces.includes(sid));
  if (!pane) return;
  pane.surfaces = pane.surfaces.filter((x) => x !== sid);
  if (pane.active === sid) pane.active = pane.surfaces[pane.surfaces.length - 1] ?? null;
  if (pane.surfaces.length === 0) {
    w.panes.delete(pane.id);
    const layout = removePaneFromLayout(w.layout, pane.id);
    if (layout) {
      w.layout = layout;
      if (w.focused === pane.id) w.focused = [...w.panes.keys()][0] ?? null;
    } else {
      closeWorkspace(w);
    }
  }
}

function closeWorkspace(w: WorkspaceState) {
  for (const s of state.surfaces.values()) if (s.workspace === w.id) state.surfaces.delete(s.id);
  for (const [id, t] of state.term) {
    if (t.workspace === w.id) state.term.delete(id);
  }
  for (const [id, b] of state.browser) {
    if (b.workspace === w.id) {
      stopBrowser(b);
      state.browser.delete(id);
    }
  }
  state.workspaces = state.workspaces.filter((x) => x.id !== w.id);
  if (state.selected === w.id) state.selected = state.workspaces[0]?.id ?? null;
}

function addNotification(n: Omit<NotifState, "id" | "created_at" | "read">) {
  const notif: NotifState = { ...n, id: `notif:${Date.now()}:${idCounter++}`, created_at: Date.now(), read: false };
  state.notifs.push(notif);
  sendAll({ type: "notification", notification: notif });
  recountUnread();
  broadcastTree();
  log(`notification: ${notif.title}`);
}

function recountUnread() {
  for (const w of state.workspaces)
    w.unread = state.notifs.filter((n) => !n.read && n.workspace === w.id).length;
}

// ---------------------------------------------------------------------------
// Seed state
// ---------------------------------------------------------------------------

function seed() {
  const w1 = newWorkspace("claude · api-refactor", "folder:1", "claude");
  w1.agent = "running";
  w1.summary =
    "Refactored the auth middleware, wired the new token store, and started on the rate limiter. Next: add integration tests.";
  w1.git_branch = "feat/auth-refactor";
  w1.pills = [
    { key: "model", value: "opus", icon: "◆", color: "#c084fc" },
    { key: "tests", value: "12 passing", icon: "✓", color: "#4ade80" },
  ];
  w1.progress = { value: 0.62, label: "auth-refactor" };
  // Split: terminal | terminal
  const second = createPane(w1, "terminal", { command: "npm test" });
  w1.layout = {
    type: "split",
    direction: "horizontal",
    ratio: 0.55,
    first: { type: "pane", pane: w1.focused! },
    second: { type: "pane", pane: second.pane.id },
  };

  const w2 = newWorkspace("tests · flaky suite", "folder:3", "zsh");
  w2.agent = "needs_input";
  w2.summary = "Test `test_checkout_flow` failed twice in a row. Run it again or skip?";
  w2.git_branch = "main";
  w2.pills = [{ key: "suite", value: "3 failing", icon: "✗", color: "#f87171" }];

  const w3 = newWorkspace("docs · lmux site", "folder:2", "zsh");
  w3.agent = "idle";
  w3.summary = "Docs build is green. Preview is live on port 3000 — want me to open it in a browser pane?";
  w3.git_branch = "docs/site";
  const b = createPane(w3, "browser", { url: "https://lmux.dev/docs" });
  w3.layout = {
    type: "split",
    direction: "horizontal",
    ratio: 0.5,
    first: { type: "pane", pane: w3.focused! },
    second: { type: "pane", pane: b.pane.id },
  };

  // Top-level workspace with two terminal tabs in one pane
  const w4 = newWorkspace("scratch", null, "zsh");
  w4.summary = "Scratch space. Two tabs in one pane.";
  const firstPaneId = w4.focused!;
  const tab2 = makeSurface(w4, w4.panes.get(firstPaneId)!, "terminal", { command: "htop" });
  void tab2;

  state.selected = w1.id;
  addNotification({
    title: "Claude finished a task",
    subtitle: "claude · api-refactor",
    body: "Rate limiter done; 12 tests passing.",
    workspace: w1.id,
    surface: null,
  });
  state.notifs[state.notifs.length - 1].read = false;
  recountUnread();
  log(`seeded ${state.workspaces.length} workspaces, selected=${state.selected}`);
}

seed();

// A notification arrives unprompted every 25s so the panel/badge update live.
setInterval(() => {
  const w = state.workspaces[Math.floor(Math.random() * state.workspaces.length)];
  if (!w) return;
  addNotification({
    title: "Agent needs input",
    subtitle: w.title,
    body: "Waiting for your go-ahead to continue.",
    workspace: w.id,
    surface: null,
  });
}, 25000);

log(`lmux mock listening on ws://${HOST}:${PORT}`);
void server;
log(`health: http://${HOST}:${PORT}/health`);
