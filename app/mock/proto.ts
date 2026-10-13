// Hand-mirrored wire contract for lmux.
//
// Source of truth: crates/lmux-proto/src/{lib.rs,methods.rs,frame.rs}.
// Change both together. JSON messages are tagged by `type` in snake_case;
// binary frames follow the layout in frame.rs.

export const PROTOCOL_VERSION = 1;

export type Ref = string;

// ---------------------------------------------------------------------------
// Session tree
// ---------------------------------------------------------------------------

export interface Tree {
  host: string;
  folders: Folder[];
  workspaces: Workspace[];
  selected_workspace: Ref | null;
}

export interface Folder {
  id: Ref;
  name: string;
  /** The folder this one sits inside; null is a top-level folder. */
  parent: Ref | null;
  collapsed: boolean;
}

export type AgentState = "none" | "running" | "idle" | "needs_input";

export interface StatusPill {
  key: string;
  value: string;
  icon: string | null;
  color: string | null;
}

export interface Progress {
  value: number; // 0.0 ..= 1.0
  label: string | null;
}

export interface Workspace {
  id: Ref;
  title: string;
  folder: Ref | null;
  cwd: string;
  git_branch: string | null;
  agent: AgentState;
  summary: string | null;
  unread_notifications: number;
  status_pills: StatusPill[];
  progress: Progress | null;
  layout: Layout;
  panes: Pane[];
  focused_pane: Ref | null;
  /** TCP ports a process in this workspace listens on. */
  ports?: number[];
  /** The pull request for the workspace's branch. */
  pr?: { number: number; state: string; url: string } | null;
}

export type SplitDirection = "horizontal" | "vertical";
export type SplitSide = "left" | "right" | "up" | "down";

export type Layout =
  | { type: "pane"; pane: Ref }
  | {
      type: "split";
      direction: SplitDirection;
      ratio: number;
      first: Layout;
      second: Layout;
    };

export interface Pane {
  id: Ref;
  surfaces: Surface[];
  active_surface: Ref | null;
}

export interface TerminalKind {
  kind: "terminal";
  tmux_pane: string | null;
  cols: number;
  rows: number;
}

export interface BrowserKind {
  kind: "browser";
  url: string;
  loading: boolean;
  can_go_back: boolean;
  can_go_forward: boolean;
}

export type SurfaceKind = TerminalKind | BrowserKind;

export interface Surface {
  id: Ref;
  title: string;
  kind: "terminal" | "browser";
  // terminal fields
  tmux_pane?: string | null;
  cols?: number;
  rows?: number;
  // browser fields
  url?: string;
  loading?: boolean;
  can_go_back?: boolean;
  can_go_forward?: boolean;
}

// ---------------------------------------------------------------------------
// Notifications / logs
// ---------------------------------------------------------------------------

export interface Notification {
  id: string;
  title: string;
  subtitle: string | null;
  body: string | null;
  workspace: Ref | null;
  surface: Ref | null;
  created_at: number; // unix ms
  read: boolean;
}

export type LogLevel = "info" | "success" | "warning" | "error";

export interface LogEntry {
  level: LogLevel;
  source: string | null;
  message: string;
  created_at: number;
}

// ---------------------------------------------------------------------------
// RPC
// ---------------------------------------------------------------------------

export interface RpcRequest {
  id: number;
  method: string;
  params: Record<string, unknown>;
}

export interface RpcError {
  code: string;
  message: string;
}

export interface RpcResponse {
  id: number;
  result?: unknown;
  error?: RpcError;
}

// ---------------------------------------------------------------------------
// App websocket messages (app -> daemon)
// ---------------------------------------------------------------------------

export type MouseAction = "down" | "up" | "move";
export type MouseButton = "none" | "left" | "middle" | "right";
export type KeyAction = "down" | "up";

export interface Modifiers {
  alt: boolean;
  ctrl: boolean;
  meta: boolean;
  shift: boolean;
}

export type BrowserInputEvent =
  | {
      type: "mouse";
      action: MouseAction;
      x: number;
      y: number;
      button: MouseButton;
      click_count: number;
      modifiers: Modifiers;
    }
  | {
      type: "wheel";
      x: number;
      y: number;
      delta_x: number;
      delta_y: number;
      modifiers: Modifiers;
    }
  | {
      type: "key";
      action: KeyAction;
      key: string;
      code: string;
      text: string | null;
      modifiers: Modifiers;
    }
  | { type: "insert_text"; text: string };

export type ClientMsg =
  | { type: "hello"; protocol_version: number; client_name: string }
  | { type: "rpc"; request: RpcRequest }
  | { type: "subscribe"; surface: Ref }
  | { type: "unsubscribe"; surface: Ref }
  | { type: "term_input"; surface: Ref; data: string }
  | { type: "term_resize"; surface: Ref; cols: number; rows: number }
  | {
      type: "browser_resize";
      surface: Ref;
      width: number;
      height: number;
      device_scale_factor: number;
    }
  | { type: "browser_input"; surface: Ref; event: BrowserInputEvent }
  | { type: "browser_frame_ack"; surface: Ref; seq: number };

// ---------------------------------------------------------------------------
// App websocket messages (daemon -> app)
// ---------------------------------------------------------------------------

export type ServerMsg =
  | { type: "welcome"; protocol_version: number; server_version: string; host: string }
  | { type: "incompatible"; protocol_version: number; server_version: string; message: string }
  | { type: "rpc_result"; response: RpcResponse }
  | { type: "tree"; tree: Tree }
  | { type: "term_snapshot"; surface: Ref; cols: number; rows: number; data: string }
  | { type: "term_exited"; surface: Ref }
  | {
      type: "browser_meta";
      surface: Ref;
      url: string;
      title: string;
      loading: boolean;
      can_go_back: boolean;
      can_go_forward: boolean;
    }
  | { type: "notification"; notification: Notification }
  | { type: "log"; workspace: Ref | null; entry: LogEntry };

// ---------------------------------------------------------------------------
// Binary frames (frame.rs)
// ---------------------------------------------------------------------------

export const KIND_TERM_OUTPUT = 1;
export const KIND_BROWSER_FRAME = 2;

export type Frame =
  | { kind: "term_output"; surface: Ref; data: Uint8Array }
  | {
      kind: "browser_frame";
      surface: Ref;
      seq: number;
      width: number;
      height: number;
      jpeg: Uint8Array;
    };

export class FrameError extends Error {}

/** Decode one binary websocket frame. Mirrors Frame::decode in frame.rs. */
export function decodeFrame(buf: Uint8Array): Frame {
  if (buf.length < 2) throw new FrameError("truncated");
  const kind = buf[0];
  const surfaceLen = buf[1];
  if (buf.length < 2 + surfaceLen) throw new FrameError("truncated");
  const surface = new TextDecoder().decode(buf.subarray(2, 2 + surfaceLen));
  const body = buf.subarray(2 + surfaceLen);
  if (kind === KIND_TERM_OUTPUT) {
    return { kind: "term_output", surface, data: body };
  }
  if (kind === KIND_BROWSER_FRAME) {
    if (body.length < 8) throw new FrameError("truncated");
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    const seq = view.getUint32(0, false);
    const width = view.getUint16(4, false);
    const height = view.getUint16(6, false);
    return { kind: "browser_frame", surface, seq, width, height, jpeg: body.subarray(8) };
  }
  throw new FrameError(`unknown kind ${kind}`);
}

/** Encode a binary frame (used by the mock server; mirrors Frame::encode). */
export function encodeFrame(frame: Frame): Uint8Array {
  const surfaceBytes = new TextEncoder().encode(frame.surface);
  if (surfaceBytes.length > 255) throw new FrameError("surface too long");
  const header = 2 + surfaceBytes.length;
  let body: Uint8Array;
  let kind: number;
  if (frame.kind === "term_output") {
    kind = KIND_TERM_OUTPUT;
    body = frame.data;
  } else {
    kind = KIND_BROWSER_FRAME;
    const extra = 8 + frame.jpeg.length;
    body = new Uint8Array(extra);
    const view = new DataView(body.buffer);
    view.setUint32(0, frame.seq, false);
    view.setUint16(4, frame.width, false);
    view.setUint16(6, frame.height, false);
    body.set(frame.jpeg, 8);
  }
  const out = new Uint8Array(header + body.length);
  out[0] = kind;
  out[1] = surfaceBytes.length;
  out.set(surfaceBytes, 2);
  out.set(body, header);
  return out;
}

// ---------------------------------------------------------------------------
// RPC method names (methods.rs)
// ---------------------------------------------------------------------------

export const M = {
  PING: "system.ping",
  IDENTIFY: "system.identify",
  TREE: "system.tree",

  FOLDER_NEW: "folder.new",
  FOLDER_RENAME: "folder.rename",
  FOLDER_CLOSE: "folder.close",
  FOLDER_MOVE: "folder.move",

  WORKSPACE_NEW: "workspace.new",
  WORKSPACE_LIST: "workspace.list",
  WORKSPACE_SELECT: "workspace.select",
  WORKSPACE_RENAME: "workspace.rename",
  WORKSPACE_CLOSE: "workspace.close",
  WORKSPACE_MOVE: "workspace.move",

  SPLIT_NEW: "pane.split",
  PANE_LIST: "pane.list",
  PANE_FOCUS: "pane.focus",
  PANE_RESIZE: "pane.resize",
  SURFACE_NEW: "surface.new",
  SURFACE_CLOSE: "surface.close",
  SURFACE_RENAME: "surface.rename",
  SURFACE_MOVE: "surface.move",
  SURFACE_SPLIT: "surface.split",
  PANE_SWAP: "pane.swap",

  TERM_SEND: "term.send",
  TERM_SEND_KEY: "term.send_key",
  TERM_READ_SCREEN: "term.read_screen",
  TERM_CLEAR_HISTORY: "term.clear_history",

  NOTIFY: "notify.create",
  NOTIFY_LIST: "notify.list",
  NOTIFY_CLEAR: "notify.clear",
  NOTIFY_MARK_READ: "notify.mark_read",
  STATUS_SET: "status.set",
  STATUS_CLEAR: "status.clear",
  PROGRESS_SET: "progress.set",
  PROGRESS_CLEAR: "progress.clear",
  LOG: "log.append",
  LOG_LIST: "log.list",
  AGENT_STATE: "agent.state",

  BROWSER_OPEN: "browser.open",
  BROWSER_GOTO: "browser.goto",
  BROWSER_BACK: "browser.back",
  BROWSER_FORWARD: "browser.forward",
  BROWSER_RELOAD: "browser.reload",
  BROWSER_URL: "browser.url",
} as const;

export type Method = (typeof M)[keyof typeof M];
