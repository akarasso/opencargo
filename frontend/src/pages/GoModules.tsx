import { createResource, For, Show } from 'solid-js';
import { fetchRepositories } from '../lib/api.ts';
import CopyButton from '../components/CopyButton.tsx';
import LoadingSpinner from '../components/LoadingSpinner.tsx';

export default function GoModules() {
  const [repos] = createResource(fetchRepositories);

  const goRepos = () => {
    const r = repos();
    if (!r) return [];
    // Filter repos whose name hints at Go modules
    return r.repositories.filter(
      (repo) =>
        repo.name.toLowerCase().includes('go') ||
        repo.name.toLowerCase().includes('golang'),
    );
  };

  const exampleHost = () => {
    if (typeof window !== 'undefined') {
      return window.location.host;
    }
    return 'registry.example.com';
  };

  return (
    <div style={{ "max-width": "80rem", margin: "0 auto", padding: "2.5rem 2rem" }}>
      {/* Header */}
      <div style={{ "margin-bottom": "2.5rem", "border-bottom": "1px solid rgba(255,255,255,0.05)", "padding-bottom": "1.5rem" }}>
        <div style={{ display: "flex", "align-items": "center", gap: "0.5rem", "margin-bottom": "0.5rem" }}>
          <span class="material-symbols-outlined" style={{ color: "var(--clr-primary)", "font-size": "18px" }}>code</span>
          <span style={{ "font-size": "0.625rem", "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", color: "var(--clr-outline)" }}>
            Registry / Go Modules
          </span>
        </div>
        <h1 style={{ "font-size": "3rem", "font-weight": "700", "font-family": "var(--font-headline)", color: "var(--clr-on-background)", "letter-spacing": "-0.05em" }}>
          Go Modules
        </h1>
        <p style={{ color: "var(--clr-on-surface-variant)", "font-size": "1.125rem", "max-width": "42rem", "font-family": "var(--font-body)", "margin-top": "0.5rem" }}>
          Host and proxy Go modules using the GOPROXY protocol.
        </p>
      </div>

      {/* Go Repositories */}
      <Show when={repos.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={!repos.loading}>
        <Show when={goRepos().length > 0}>
          <section style={{ "margin-bottom": "2.5rem" }}>
            <h2 style={{ "font-size": "0.75rem", "font-family": "var(--font-headline)", "text-transform": "uppercase", "letter-spacing": "0.15em", color: "var(--clr-on-surface)", "font-weight": "700", "margin-bottom": "1rem" }}>
              Go Repositories
            </h2>
            <div style={{ display: "flex", "flex-direction": "column", gap: "0.75rem" }}>
              <For each={goRepos()}>
                {(repo) => (
                  <div class="card" style={{ display: "flex", "align-items": "center", gap: "1rem", padding: "1.25rem" }}>
                    <div style={{ width: "40px", height: "40px", "border-radius": "0.5rem", background: "rgba(123, 231, 249, 0.1)", display: "flex", "align-items": "center", "justify-content": "center" }}>
                      <span class="material-symbols-outlined" style={{ color: "var(--clr-primary)" }}>code</span>
                    </div>
                    <div>
                      <div style={{ "font-family": "var(--font-headline)", "font-weight": "700", color: "var(--clr-on-surface)" }}>{repo.name}</div>
                      <div style={{ "font-size": "0.75rem", "font-family": "var(--font-mono)", color: "var(--clr-on-surface-variant)" }}>GOPROXY={window.location.protocol}//{exampleHost()}/{repo.name}</div>
                    </div>
                  </div>
                )}
              </For>
            </div>
          </section>
        </Show>

        {/* Getting Started */}
        <section style={{ "margin-bottom": "2.5rem" }}>
          <h2 style={{ "font-size": "0.75rem", "font-family": "var(--font-headline)", "text-transform": "uppercase", "letter-spacing": "0.15em", color: "var(--clr-on-surface)", "font-weight": "700", "margin-bottom": "1.5rem" }}>
            Getting Started
          </h2>
          <div style={{ display: "flex", "flex-direction": "column", gap: "2rem" }}>
            {/* Configure GOPROXY */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                1. Configure GOPROXY
              </h3>
              <div class="code-block">
                <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                  export GOPROXY={window.location.protocol}//{exampleHost()}/go-hosted,https://proxy.golang.org,direct
                </code>
                <CopyButton text={`export GOPROXY=${window.location.protocol}//${exampleHost()}/go-hosted,https://proxy.golang.org,direct`} />
              </div>
              <p style={{ "margin-top": "0.5rem", "font-size": "0.75rem", color: "var(--clr-on-surface-variant)" }}>
                This tells the Go toolchain to look up modules in your private registry first, then fall back to the public proxy.
              </p>
            </div>

            {/* Configure GONOSUMCHECK for private modules */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                2. Configure GONOSUMCHECK (private modules)
              </h3>
              <div class="code-block">
                <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                  export GONOSUMCHECK=your.private.domain/*
                </code>
                <CopyButton text="export GONOSUMCHECK=your.private.domain/*" />
              </div>
            </div>

            {/* Install a module */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                3. Install a Module
              </h3>
              <div class="code-block">
                <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                  go get your.private.domain/module@latest
                </code>
                <CopyButton text="go get your.private.domain/module@latest" />
              </div>
            </div>

            {/* Publish a module */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                4. Publish a Module
              </h3>
              <div style={{ display: "flex", "flex-direction": "column", gap: "0.5rem" }}>
                <div class="code-block">
                  <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                    # Tag your release
                  </code>
                </div>
                <div class="code-block">
                  <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                    git tag v1.0.0 && git push origin v1.0.0
                  </code>
                  <CopyButton text="git tag v1.0.0 && git push origin v1.0.0" />
                </div>
              </div>
              <p style={{ "margin-top": "0.5rem", "font-size": "0.75rem", color: "var(--clr-on-surface-variant)" }}>
                Go modules are published via Git tags. Once a repository is configured as a Go proxy, OpenCargo will fetch and cache modules automatically.
              </p>
            </div>
          </div>
        </section>

        {/* Info card */}
        <section>
          <div style={{ background: "rgba(123, 231, 249, 0.05)", border: "1px solid rgba(123, 231, 249, 0.15)", padding: "1.5rem", "border-radius": "0.75rem" }}>
            <div style={{ display: "flex", gap: "1rem", "align-items": "flex-start" }}>
              <span class="material-symbols-outlined" style={{ color: "var(--clr-primary)" }}>info</span>
              <div>
                <h4 style={{ "font-size": "0.75rem", "font-family": "var(--font-headline)", "font-weight": "700", color: "var(--clr-primary)", "text-transform": "uppercase", "letter-spacing": "0.1em", "margin-bottom": "0.5rem" }}>
                  GOPROXY Protocol
                </h4>
                <p style={{ "font-size": "0.875rem", color: "var(--clr-on-surface-variant)", "line-height": "1.6" }}>
                  OpenCargo implements the GOPROXY protocol for hosting and proxying Go modules. Create a Go-type repository in the admin panel to get started. Proxy repositories will cache modules from upstream sources like proxy.golang.org.
                </p>
              </div>
            </div>
          </div>
        </section>
      </Show>
    </div>
  );
}
