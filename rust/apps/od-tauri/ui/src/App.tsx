import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  getDaemonHealth,
  getDaemonInfo,
  getAppVersion,
  isShellAvailable,
  openExternal,
  pickWorkspaceFolder,
  type DaemonHealth,
  type DaemonInfo,
} from "./tauri";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function projectNames(payload: unknown): string[] {
  if (typeof payload !== "object" || payload === null) return [];
  const projects = (payload as { projects?: unknown }).projects;
  if (!Array.isArray(projects)) return [];
  const names: string[] = [];
  for (const entry of projects) {
    if (typeof entry !== "object" || entry === null) continue;
    const name = (entry as { name?: unknown }).name;
    if (typeof name === "string") names.push(name);
  }
  return names;
}

export default function App() {
  const [version, setVersion] = useState<string | null>(null);
  const [info, setInfo] = useState<DaemonInfo | null>(null);
  const [health, setHealth] = useState<DaemonHealth | null>(null);
  const [healthError, setHealthError] = useState<string | null>(null);
  const [workspace, setWorkspace] = useState<string | null | undefined>(undefined);
  const [workspaceError, setWorkspaceError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const [docsError, setDocsError] = useState<string | null>(null);
  const [projects, setProjects] = useState<string[] | null>(null);
  const [projectsError, setProjectsError] = useState<string | null>(null);
  const [deeplink, setDeeplink] = useState<string | null>(null);

  // Shell metadata: version + daemon info (both fail outside the Tauri shell).
  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const value = await getAppVersion();
        if (active) setVersion(value);
      } catch {
        if (active) setVersion(null);
      }
      try {
        const value = await getDaemonInfo();
        if (active) setInfo(value);
      } catch (error) {
        if (active) setInfo(null);
      }
    })();
    return () => {
      active = false;
    };
  }, []);

  // Health poll every 5s.
  useEffect(() => {
    let active = true;
    const tick = async () => {
      try {
        const value = await getDaemonHealth();
        if (active) {
          setHealth(value);
          setHealthError(null);
        }
      } catch (error) {
        if (active) {
          setHealth(null);
          setHealthError(errorText(error));
        }
      }
    };
    void tick();
    const timer = window.setInterval(() => void tick(), 5000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, []);

  // Project catalog via the daemon (same origin, or vite proxy in dev).
  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const response = await fetch("/api/projects");
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const body: unknown = await response.json();
        if (active) {
          setProjects(projectNames(body));
          setProjectsError(null);
        }
      } catch (error) {
        if (active) setProjectsError(errorText(error));
      }
    })();
    return () => {
      active = false;
    };
  }, []);

  // Deeplinks: the shell dispatches a DOM CustomEvent; the Tauri event bus is
  // registered too (guarded) in case the shell starts emitting there.
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | null = null;
    const onUrl = (url: string) => {
      console.log("od-deeplink", url);
      setDeeplink(url);
    };
    const onDom = (event: Event) => {
      const detail: unknown = (event as CustomEvent<unknown>).detail;
      if (typeof detail === "string") onUrl(detail);
    };
    window.addEventListener("od-deeplink", onDom);
    if (isShellAvailable()) {
      listen<string>("od-deeplink", (event) => onUrl(event.payload))
        .then((stop) => {
          if (active) unlisten = stop;
          else stop();
        })
        .catch(() => undefined);
    }
    return () => {
      active = false;
      window.removeEventListener("od-deeplink", onDom);
      unlisten?.();
    };
  }, []);

  const chooseWorkspace = async () => {
    setPicking(true);
    setWorkspaceError(null);
    try {
      setWorkspace(await pickWorkspaceFolder());
    } catch (error) {
      setWorkspaceError(errorText(error));
    } finally {
      setPicking(false);
    }
  };

  const openDocs = async () => {
    setDocsError(null);
    try {
      await openExternal("https://open-design.ai");
    } catch (error) {
      setDocsError(errorText(error));
    }
  };

  let dotClass = "dot";
  let healthText: string;
  if (healthError) {
    dotClass += " down";
    healthText = healthError;
  } else if (health) {
    dotClass += health.ok ? " ok" : " down";
    healthText = health.ok ? `healthy · daemon v${health.version}` : "daemon reports not ok";
  } else {
    healthText = "checking…";
  }

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <h1>OpenDesign</h1>
          <span className="version">{version ? `v${version}` : "—"}</span>
        </div>
        <button type="button" onClick={() => void openDocs()}>
          Open docs
        </button>
      </header>

      {!isShellAvailable() && (
        <div className="banner">
          Running outside the Tauri shell — shell commands are unavailable in a plain browser.
        </div>
      )}

      <section className="card">
        <h2>Daemon</h2>
        <div className="row">
          <span className={dotClass} aria-hidden="true" />
          <span className="health">{healthText}</span>
        </div>
        <div className="row">
          <span className="label">url</span>
          <span className="value">{info ? info.url : "—"}</span>
        </div>
        <div className="row">
          <span className="label">data_dir</span>
          <span className="value">{info ? info.data_dir : "—"}</span>
        </div>
        <div className="actions">
          <button type="button" className="primary" disabled={picking} onClick={() => void chooseWorkspace()}>
            Open workspace folder…
          </button>
          <button type="button" onClick={() => void openDocs()}>
            Open docs
          </button>
        </div>
        {workspace === null && <div className="note">workspace picker cancelled</div>}
        {typeof workspace === "string" && <div className="note">workspace: {workspace}</div>}
        {workspaceError && <div className="error">{workspaceError}</div>}
        {docsError && <div className="error">{docsError}</div>}
      </section>

      <section className="card">
        <h2>Projects</h2>
        {projectsError && <div className="error">Could not load projects: {projectsError}</div>}
        {projects !== null && projects.length === 0 && <div className="empty">No projects yet.</div>}
        {projects !== null && projects.length > 0 && (
          <ul className="list">
            {projects.map((name) => (
              <li key={name}>{name}</li>
            ))}
          </ul>
        )}
        {projects === null && !projectsError && <div className="empty">Loading…</div>}
      </section>

      <section className="card">
        <h2>Deeplink</h2>
        <div className="row">
          <span className="label">last</span>
          <span className="value">{deeplink ?? "none received yet"}</span>
        </div>
      </section>
    </div>
  );
}
