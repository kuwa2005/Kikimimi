import { useEffect, useMemo, useRef, useState } from "react";

export type Command = {
  id: string;
  label: string;
  hint?: string;
  keywords?: string;
  run: () => void | Promise<void>;
};

// Ctrl+K / Cmd+K で開く共通メニュー。日本語で絞り込み、矢印・Enter・Escape で操作する。
// 終了・削除のような取り返しのつかない操作は、ここからは確認ダイアログを経由してしか実行しない。
export function CommandMenu({ commands, onClose }: { commands: Command[]; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const opener = useRef<Element | null>(document.activeElement);
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  // Enter は押した時点で選び、離したときに実行する。押したまま閉じると、ダイアログが戻した
  // フォーカス先（サイドのリンクなど）に同じ Enter が届いて、別の画面へ移ってしまうため。
  const pending = useRef<Command | null>(null);

  const shown = useMemo(() => {
    const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
    if (!words.length) return commands;
    return commands.filter((command) => {
      const text = `${command.label} ${command.hint || ""} ${command.keywords || ""}`.toLowerCase();
      return words.every((word) => text.includes(word));
    });
  }, [commands, query]);

  useEffect(() => {
    ref.current?.showModal();
    input.current?.focus();
    const back = opener.current;
    const from = location.hash;
    return () => {
      // 閉じた直後にフォーカスを戻すと、同じ Enter が戻した先（リンクなど）を押してしまう。
      // キー操作が終わってから、画面を移っていないときだけ戻す。
      setTimeout(() => {
        if (location.hash === from && back instanceof HTMLElement && document.contains(back)) back.focus();
      }, 100);
    };
  }, []);

  useEffect(() => setIndex(0), [query]);
  // 絞り込みで項目が減っても、選択が範囲の外を指さないようにする。
  const current = Math.min(index, Math.max(shown.length - 1, 0));

  const run = (command: Command | undefined) => {
    if (!command) return;
    // Enter は離したときに実行しているので、キーの続きが他の要素に届くことはない。
    onClose();
    void command.run();
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    // 日本語の変換中のキーは、メニューの操作として扱わない。
    if (event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (event.key === "ArrowDown") {
      setIndex((value) => Math.min(value + 1, shown.length - 1));
    } else if (event.key === "ArrowUp") {
      setIndex((value) => Math.max(value - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      pending.current = shown[current] || null;
      return;
    } else {
      return;
    }
    event.preventDefault();
  };

  const activeId = shown[current] ? `cmd-${shown[current].id}` : undefined;
  return (
    <dialog
      ref={ref}
      className="command-menu"
      aria-label="操作を探す"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
      onClick={(event) => {
        if (event.target === ref.current) onClose();
      }}
    >
      <input
        ref={input}
        type="search"
        role="combobox"
        aria-expanded="true"
        aria-controls="command-list"
        aria-activedescendant={activeId}
        aria-label="操作を探す"
        placeholder="操作を探す（例: 新しい会議、設定、録音中）"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        onKeyDown={onKeyDown}
        onKeyUp={(event) => {
          if (event.key !== "Enter" || !pending.current) return;
          const command = pending.current;
          pending.current = null;
          run(command);
        }}
      />
      <ul id="command-list" role="listbox" aria-label="操作">
        {shown.length ? (
          shown.map((command, position) => (
            <li
              key={command.id}
              id={`cmd-${command.id}`}
              role="option"
              aria-selected={position === current}
              className={position === current ? "selected" : ""}
              // 開いた位置にたまたまポインタがあっても選択を動かさない。実際に動かしたときだけ追う。
              onMouseMove={() => position !== current && setIndex(position)}
              onClick={() => run(command)}
            >
              <span>{command.label}</span>
              {command.hint ? <span className="muted small">{command.hint}</span> : null}
            </li>
          ))
        ) : (
          <li className="empty" role="option" aria-selected="false">
            見つかりません
          </li>
        )}
      </ul>
      <p className="muted small hint-line">↑↓ で選択・Enter で実行・Esc で閉じる</p>
    </dialog>
  );
}
