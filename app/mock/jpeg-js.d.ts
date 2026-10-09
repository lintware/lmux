// jpeg-js ships CommonJS without bundled types.
declare module "jpeg-js" {
  export function encode(
    raw: { data: Uint8Array; width: number; height: number },
    quality?: number,
  ): { data: Uint8Array; width: number; height: number };
  export function decode(
    jpeg: Uint8Array,
    opts?: unknown,
  ): { data: Uint8Array; width: number; height: number };
}
