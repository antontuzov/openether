import type { ReactNode } from 'react';

interface PanelProps {
  title: string;
  hint?: string;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
}

/** Shared panel chrome: title bar, optional hint, optional actions slot. */
export function Panel({ title, hint, actions, children, className, bodyClassName }: PanelProps) {
  return (
    <section className={`panel${className ? ` ${className}` : ''}`}>
      <header className="panel__hd">
        <span className="panel__tick" aria-hidden="true" />
        <h2 className="panel__title">{title}</h2>
        {hint ? <span className="panel__hint">{hint}</span> : null}
        {actions ? <div className="panel__actions">{actions}</div> : null}
      </header>
      <div className={`panel__bd${bodyClassName ? ` ${bodyClassName}` : ''}`}>{children}</div>
    </section>
  );
}
