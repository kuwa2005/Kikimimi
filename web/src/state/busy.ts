import { errorMessage } from "../api/client";
import { createStore, useStore } from "../lib/store";
import { notify } from "./notices";

// 開始・一時停止・終了などは、終わるまで次の操作を受け付けない（二重押し防止）。
export const busy = createStore<string>("");

export async function guarded(name: string, task: () => Promise<unknown>) {
  if (busy.get()) return;
  busy.set(name);
  try {
    await task();
  } catch (err) {
    notify(errorMessage(err), "error");
  } finally {
    busy.set("");
  }
}

export function useBusy(): string {
  return useStore(busy);
}
