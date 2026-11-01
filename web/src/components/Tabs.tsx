import { useRef, type ReactNode } from "react";

export type TabItem = { id: string; label: ReactNode };

// tablist / tab / tabpanel の構造と、矢印・Home・End での移動。
export function Tabs({
  idPrefix,
  tabs,
  active,
  onChange,
  label,
}: {
  idPrefix: string;
  tabs: TabItem[];
  active: string;
  onChange: (id: string) => void;
  label: string;
}) {
  const refs = useRef<Record<string, HTMLButtonElement | null>>({});
  const move = (index: number) => {
    const tab = tabs[(index + tabs.length) % tabs.length];
    onChange(tab.id);
    refs.current[tab.id]?.focus();
  };
  return (
    <div className="tabs" role="tablist" aria-label={label}>
      {tabs.map((tab, index) => (
        <button
          key={tab.id}
          ref={(node) => {
            refs.current[tab.id] = node;
          }}
          id={`${idPrefix}-tab-${tab.id}`}
          className="btn"
          type="button"
          role="tab"
          aria-selected={active === tab.id}
          aria-controls={`${idPrefix}-panel-${tab.id}`}
          tabIndex={active === tab.id ? 0 : -1}
          onClick={() => onChange(tab.id)}
          onKeyDown={(event) => {
            if (event.key === "ArrowRight") move(index + 1);
            else if (event.key === "ArrowLeft") move(index - 1);
            else if (event.key === "Home") move(0);
            else if (event.key === "End") move(tabs.length - 1);
            else return;
            event.preventDefault();
          }}
        >
          {tab.label}
        </button>
      ))}
    </div>
  );
}

export function TabPanel({ idPrefix, id, children, className = "" }: { idPrefix: string; id: string; children: ReactNode; className?: string }) {
  return (
    <div role="tabpanel" id={`${idPrefix}-panel-${id}`} aria-labelledby={`${idPrefix}-tab-${id}`} className={className} tabIndex={0}>
      {children}
    </div>
  );
}
