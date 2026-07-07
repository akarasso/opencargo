import { For, Show } from 'solid-js';
import Icon from '../components/Icon.tsx';
import CopyButton from '../components/CopyButton.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { FormatTag, VisibilityChip } from '../components/bits.tsx';
import { fetchRepositories } from '../core/api.ts';
import { createLiveResource } from '../core/stores/live.ts';
import { session } from '../core/stores/session.ts';

export default function OciImages() {
  const [repos] = createLiveResource(fetchRepositories, ['repositories.changed']);
  const ociRepos = () => (repos()?.repositories ?? []).filter((r) => r.format === 'oci');
  const host = () => location.host;
  const example = () => ociRepos()[0]?.name ?? 'oci-private';

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Containers</h1>
          <p class="page-sub">
            Push and pull OCI images with the standard Docker toolchain — same accounts, same
            permissions as the rest of the registry.
          </p>
        </div>
      </div>

      <div class="stagger">
        <section class="section">
          <div class="section-head">
            <span class="section-title">OCI repositories</span>
          </div>
          <Show
            when={!repos.loading}
            fallback={
              <div class="card card-pad">
                <div class="skeleton skeleton-text" style={{ width: '52%', 'margin-bottom': '10px' }} />
                <div class="skeleton skeleton-text" style={{ width: '38%' }} />
              </div>
            }
          >
            <Show
              when={ociRepos().length > 0}
              fallback={
                <div class="card">
                  <EmptyState
                    icon="container"
                    title="No OCI repository yet"
                    text={
                      session.isAdmin()
                        ? 'Create a repository with format “oci” to start pushing images.'
                        : 'Ask an administrator to create a repository with format “oci”.'
                    }
                  />
                </div>
              }
            >
              <div class="grid-cards">
                <For each={ociRepos()}>
                  {(repo) => (
                    <div class="card card-pad card-hover">
                      <div class="row" style={{ 'margin-bottom': '10px' }}>
                        <Icon name="container" size={16} class="icon dim" />
                        <span class="mono grow truncate" style={{ color: 'var(--ink)' }}>
                          {repo.name}
                        </span>
                        <VisibilityChip visibility={repo.visibility} />
                      </div>
                      <div class="code-line">
                        <code>
                          {host()}/{repo.name}/image:tag
                        </code>
                        <CopyButton text={`${host()}/${repo.name}/image:tag`} label="" />
                      </div>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </section>

        <section class="section">
          <div class="section-head">
            <span class="section-title">Quickstart</span>
            <FormatTag format="oci" />
          </div>
          <div class="card card-pad col" style={{ gap: '14px' }}>
            <div>
              <div class="side-label">1 · Sign in</div>
              <div class="code-line">
                <code>docker login {host()}</code>
                <CopyButton text={`docker login ${host()}`} label="" />
              </div>
            </div>
            <div>
              <div class="side-label">2 · Tag</div>
              <div class="code-line">
                <code>
                  docker tag myapp:latest {host()}/{example()}/myapp:latest
                </code>
                <CopyButton text={`docker tag myapp:latest ${host()}/${example()}/myapp:latest`} label="" />
              </div>
            </div>
            <div>
              <div class="side-label">3 · Push</div>
              <div class="code-line">
                <code>
                  docker push {host()}/{example()}/myapp:latest
                </code>
                <CopyButton text={`docker push ${host()}/${example()}/myapp:latest`} label="" />
              </div>
            </div>
          </div>
        </section>

        <div class="alert alert-info">
          <Icon name="info" size={15} />
          <span>
            Serving over plain HTTP? Add <span class="mono">"insecure-registries": ["{host()}"]</span>{' '}
            to Docker's <span class="mono">daemon.json</span> — or put the registry behind TLS.
          </span>
        </div>
      </div>
    </div>
  );
}
