import { createResource, For, Show } from 'solid-js';
import { fetchRepositories } from '../lib/api.ts';
import CopyButton from '../components/CopyButton.tsx';
import LoadingSpinner from '../components/LoadingSpinner.tsx';

export default function OciImages() {
  const [repos] = createResource(fetchRepositories);

  const ociRepos = () => {
    const r = repos();
    if (!r) return [];
    // Filter repos whose name hints at OCI (convention: name contains "oci" or "docker")
    return r.repositories.filter(
      (repo) =>
        repo.name.toLowerCase().includes('oci') ||
        repo.name.toLowerCase().includes('docker') ||
        repo.name.toLowerCase().includes('container'),
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
          <span class="material-symbols-outlined" style={{ color: "var(--clr-primary)", "font-size": "18px" }}>deployed_code</span>
          <span style={{ "font-size": "0.625rem", "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", color: "var(--clr-outline)" }}>
            Registry / Containers
          </span>
        </div>
        <h1 style={{ "font-size": "3rem", "font-weight": "700", "font-family": "var(--font-headline)", color: "var(--clr-on-background)", "letter-spacing": "-0.05em" }}>
          Container Images
        </h1>
        <p style={{ color: "var(--clr-on-surface-variant)", "font-size": "1.125rem", "max-width": "42rem", "font-family": "var(--font-body)", "margin-top": "0.5rem" }}>
          Push and pull OCI-compliant container images using standard Docker commands.
        </p>
      </div>

      {/* OCI Repositories */}
      <Show when={repos.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={!repos.loading}>
        <Show when={ociRepos().length > 0}>
          <section style={{ "margin-bottom": "2.5rem" }}>
            <h2 style={{ "font-size": "0.75rem", "font-family": "var(--font-headline)", "text-transform": "uppercase", "letter-spacing": "0.15em", color: "var(--clr-on-surface)", "font-weight": "700", "margin-bottom": "1rem" }}>
              OCI Repositories
            </h2>
            <div style={{ display: "flex", "flex-direction": "column", gap: "0.75rem" }}>
              <For each={ociRepos()}>
                {(repo) => (
                  <div class="card" style={{ display: "flex", "align-items": "center", gap: "1rem", padding: "1.25rem" }}>
                    <div style={{ width: "40px", height: "40px", "border-radius": "0.5rem", background: "rgba(123, 231, 249, 0.1)", display: "flex", "align-items": "center", "justify-content": "center" }}>
                      <span class="material-symbols-outlined" style={{ color: "var(--clr-primary)" }}>deployed_code</span>
                    </div>
                    <div>
                      <div style={{ "font-family": "var(--font-headline)", "font-weight": "700", color: "var(--clr-on-surface)" }}>{repo.name}</div>
                      <div style={{ "font-size": "0.75rem", "font-family": "var(--font-mono)", color: "var(--clr-on-surface-variant)" }}>{exampleHost()}/{repo.name}/&lt;image&gt;:&lt;tag&gt;</div>
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
            {/* Login */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                1. Authenticate with Docker
              </h3>
              <div class="code-block">
                <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                  docker login {exampleHost()}
                </code>
                <CopyButton text={`docker login ${exampleHost()}`} />
              </div>
            </div>

            {/* Push */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                2. Push an Image
              </h3>
              <div style={{ display: "flex", "flex-direction": "column", gap: "0.5rem" }}>
                <div class="code-block">
                  <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                    docker tag myapp:latest {exampleHost()}/oci-hosted/myapp:latest
                  </code>
                  <CopyButton text={`docker tag myapp:latest ${exampleHost()}/oci-hosted/myapp:latest`} />
                </div>
                <div class="code-block">
                  <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                    docker push {exampleHost()}/oci-hosted/myapp:latest
                  </code>
                  <CopyButton text={`docker push ${exampleHost()}/oci-hosted/myapp:latest`} />
                </div>
              </div>
            </div>

            {/* Pull */}
            <div>
              <h3 style={{ "font-size": "0.875rem", "font-family": "var(--font-headline)", "font-weight": "600", color: "var(--clr-on-surface)", "margin-bottom": "0.75rem" }}>
                3. Pull an Image
              </h3>
              <div class="code-block">
                <code style={{ "font-family": "var(--font-mono)", color: "var(--clr-secondary)", "font-size": "0.875rem" }}>
                  docker pull {exampleHost()}/oci-hosted/myapp:latest
                </code>
                <CopyButton text={`docker pull ${exampleHost()}/oci-hosted/myapp:latest`} />
              </div>
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
                  OCI Distribution Spec
                </h4>
                <p style={{ "font-size": "0.875rem", color: "var(--clr-on-surface-variant)", "line-height": "1.6" }}>
                  OpenCargo implements the OCI Distribution Specification, making it compatible with Docker, Podman, containerd, and any OCI-compliant client. Create an OCI-type repository in the admin panel to get started.
                </p>
              </div>
            </div>
          </div>
        </section>
      </Show>
    </div>
  );
}
