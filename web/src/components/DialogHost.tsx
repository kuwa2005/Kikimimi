import { useEffect, useRef } from "react";
import { useStore } from "../lib/store";
import { closeDialog, dialogs, type DialogResult } from "../state/dialog";

type Pending = ReturnType<typeof dialogs.get>[number];

function DialogView({ item }: { item: Pending }) {
  const ref = useRef<HTMLDialogElement>(null);
  const opener = useRef<Element | null>(document.activeElement);

  useEffect(() => {
    const dialog = ref.current;
    if (dialog && !dialog.open) dialog.showModal();
    const first = dialog?.querySelector<HTMLElement>("input, textarea, select");
    first?.focus();
    const back = opener.current;
    return () => {
      // 閉じたら、開いたときの場所へフォーカスを戻す。
      if (back instanceof HTMLElement && document.contains(back)) back.focus();
    };
  }, []);

  const finish = (result: DialogResult | null) => {
    ref.current?.close();
    closeDialog(item.id, result);
  };

  const onSubmit = (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    event.stopPropagation();
    const data = new FormData(event.currentTarget);
    const values: DialogResult = { _choice: "ok" };
    for (const field of item.fields || []) values[field.name] = String(data.get(field.name) ?? "");
    finish(values);
  };

  const titleId = `dlg-title-${item.id}`;
  return (
    <dialog
      ref={ref}
      className={`confirm ${item.wide ? "wide" : ""}`}
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        finish(null);
      }}
    >
      <form method="dialog" onSubmit={onSubmit}>
        <h2 className="dlg-title" id={titleId}>
          {item.title}
        </h2>
        {item.body ? <div className="dlg-body">{item.body}</div> : null}
        {(item.fields || []).map((field) => {
          const id = `dlg-${item.id}-${field.name}`;
          const hintId = field.hint ? `${id}-hint` : undefined;
          let control;
          if (field.type === "textarea") {
            control = <textarea id={id} name={field.name} rows={field.rows || 6} defaultValue={field.value || ""} aria-describedby={hintId} />;
          } else if (field.type === "select") {
            control = (
              <select id={id} name={field.name} defaultValue={field.value} aria-describedby={hintId}>
                {field.options.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
            );
          } else {
            control = (
              <input
                id={id}
                name={field.name}
                type={field.type === "password" ? "password" : "text"}
                defaultValue={field.value || ""}
                placeholder={field.placeholder}
                aria-describedby={hintId}
              />
            );
          }
          return (
            <div className="field" key={field.name}>
              <label htmlFor={id}>{field.label}</label>
              {control}
              {field.hint ? (
                <p className="hint" id={hintId}>
                  {field.hint}
                </p>
              ) : null}
            </div>
          );
        })}
        <div className="actions end">
          <button className="btn" type="button" onClick={() => finish(null)}>
            {item.cancel || "キャンセル"}
          </button>
          {(item.extra || []).map((button) => (
            <button className="btn" type="button" key={button.value} value={button.value} onClick={() => finish({ _choice: button.value })}>
              {button.label}
            </button>
          ))}
          <button className={`btn ${item.danger ? "danger" : "primary"}`} type="submit" value="ok">
            {item.confirm || "OK"}
          </button>
        </div>
      </form>
    </dialog>
  );
}

export function DialogHost() {
  const list = useStore(dialogs);
  return (
    <>
      {list.map((item) => (
        <DialogView key={item.id} item={item} />
      ))}
    </>
  );
}
