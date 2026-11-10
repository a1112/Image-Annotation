import { CircleAlert } from "lucide-react";

export function RuntimeStatePanel({
  title,
  message,
  actionLabel,
  onAction,
}: {
  title: string;
  message: string;
  actionLabel?: string;
  onAction?: () => void;
}) {
  return (
    <section className="runtime-state">
      <CircleAlert size={22} />
      <div>
        <h2>{title}</h2>
        <p>{message}</p>
      </div>
      {actionLabel && onAction ? (
        <button type="button" onClick={onAction}>
          {actionLabel}
        </button>
      ) : null}
    </section>
  );
}
