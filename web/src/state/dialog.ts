import type { ReactNode } from "react";
import { createStore } from "../lib/store";

// 入力欄つきの確認ダイアログ。OK なら入力値、やめたら null を返す。
export type DialogField =
  | { name: string; label: string; value?: string; type?: "text" | "password"; placeholder?: string; hint?: string }
  | { name: string; label: string; value?: string; type: "textarea"; rows?: number; hint?: string }
  | { name: string; label: string; value?: string; type: "select"; options: { value: string; label: string }[]; hint?: string };

export type DialogRequest = {
  title: string;
  body?: ReactNode;
  fields?: DialogField[];
  confirm?: string;
  cancel?: string;
  danger?: boolean;
  extra?: { label: string; value: string }[];
  wide?: boolean;
};

export type DialogResult = Record<string, string> & { _choice: string };

type Pending = DialogRequest & { id: number; resolve: (result: DialogResult | null) => void };

export const dialogs = createStore<Pending[]>([]);
let next = 1;

export function openDialog(request: DialogRequest): Promise<DialogResult | null> {
  return new Promise((resolve) => {
    const id = next++;
    dialogs.set((list) => [...list, { ...request, id, resolve }]);
  });
}

export function closeDialog(id: number, result: DialogResult | null) {
  const found = dialogs.get().find((item) => item.id === id);
  dialogs.set((list) => list.filter((item) => item.id !== id));
  found?.resolve(result);
}

export async function confirmDialog(title: string, body: ReactNode, confirm: string, danger = false): Promise<boolean> {
  return !!(await openDialog({ title, body, confirm, danger }));
}
