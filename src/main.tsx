import React from "react";
import ReactDOM from "react-dom/client";
import { configureBackendTransport } from "./api/tauri";
import { isTauriRuntime } from "./platform/runtime";
import "./styles.css";
import "./workspace-density.css";

const useDesktop = isTauriRuntime()
  || import.meta.env.VITE_APP_MODE === "desktop"
  // The default desktop artifact keeps its historical `dist` output.  The
  // browser development server and `build:web` both select WebWorkspaceApp.
  || import.meta.env.MODE === "desktop";

configureBackendTransport(useDesktop ? "local" : "web");
const Root = React.lazy(() => useDesktop ? import("./App") : import("./features/web/WebWorkspaceApp"));

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <React.Suspense fallback={<div className="workspace-loading" role="status">正在加载…</div>}><Root /></React.Suspense>
  </React.StrictMode>,
);
