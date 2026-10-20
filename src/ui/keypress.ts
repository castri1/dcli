/**
 * Raw stdin keypress handling utility.
 * Only works when stdin is a TTY (interactive terminal).
 */

export type KeypressHandler = (key: string) => void;

let currentHandler: KeypressHandler | null = null;
let dataListener: ((data: Buffer) => void) | null = null;

/**
 * Check if raw mode is available (stdin is a TTY).
 */
export function isRawModeSupported(): boolean {
  return Boolean(process.stdin.isTTY && process.stdin.setRawMode);
}

/**
 * Enable raw mode on stdin so we can capture individual keypresses.
 */
export function enableRawMode(): void {
  if (!isRawModeSupported()) return;
  process.stdin.setRawMode(true);
  process.stdin.resume();
}

/**
 * Disable raw mode and pause stdin.
 */
export function disableRawMode(): void {
  if (!isRawModeSupported()) return;
  if (process.stdin.isTTY) {
    process.stdin.setRawMode(false);
  }
  process.stdin.pause();
}

/**
 * Start listening for keypresses. Returns a cleanup function.
 * Only one handler can be active at a time.
 */
export function onKeypress(handler: KeypressHandler): () => void {
  currentHandler = handler;

  if (dataListener) {
    process.stdin.off("data", dataListener);
  }

  dataListener = (data: Buffer) => {
    const raw = data.toString();

    // Map raw input to key names
    if (raw === "\x03") {
      // Ctrl+C
      currentHandler?.("ctrl+c");
    } else if (raw === "\x1b" || raw === "\x1b\x1b") {
      // Escape
      currentHandler?.("escape");
    } else if (raw === "\r" || raw === "\n") {
      currentHandler?.("enter");
    } else if (raw.length === 1) {
      // Single character key
      currentHandler?.(raw.toLowerCase());
    }
  };

  process.stdin.on("data", dataListener);

  return () => {
    if (dataListener) {
      process.stdin.off("data", dataListener);
      dataListener = null;
    }
    currentHandler = null;
  };
}

/**
 * Remove all keypress listeners and disable raw mode.
 */
export function cleanupKeypress(): void {
  if (dataListener) {
    process.stdin.off("data", dataListener);
    dataListener = null;
  }
  currentHandler = null;
  disableRawMode();
}
