import { For, Show } from 'solid-js';
import Icon from '../components/Icon.tsx';
import CopyButton from '../components/CopyButton.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { FormatTag, VisibilityChip } from '../components/bits.tsx';
import { fetchRepositories } from '../core/api.ts';
import { createLiveResource } from '../core/stores/live.ts';
import { session } from '../core/stores/session.ts';

export default function GoModules() {
  const [repos] = createLiveResource(fetchRepositories, ['repositories.changed']);
  const goRepos = () => (repos()?.repositories ?? []).filter((r) => r.format === 'go');
  const base = () => `${location.protocol}//${location.host}`;
  const example = () => goRepos()[0]?.name ?? 'go-private';

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Go modules</h1>
          <p class="page-sub">
            A GOPROXY endpoint for private modules, with transparent caching of upstream ones.
          </p>
        </div>
      </div>

      <div class="stagger">
        <section class="section">
          <div class="section-head">
            <span class="section-title">Go repositories</span>
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
              when={goRepos().length > 0}
              fallback={
                <div class="card">
                  <EmptyState
                    icon="code"
                    title="No Go repository yet"
                    text={
                      session.isAdmin()
                        ? 'Create a repository with format “go” to start serving modules.'
                        : 'Ask an administrator to create a repository with format “go”.'
                    }
                  />
                </div>
              }
            >
              <div class="grid-cards">
                <For each={goRepos()}>
                  {(repo) => (
                    <div class="card card-pad card-hover">
                      <div class="row" style={{ 'margin-bottom': '10px' }}>
                        <Icon name="code" size={16} class="icon dim" />
                        <span class="mono grow truncate" style={{ color: 'var(--ink)' }}>
                          {repo.name}
                        </span>
                        <VisibilityChip visibility={repo.visibility} />
                      </div>
                      <div class="code-line">
                        <code>
                          GOPROXY={base()}/{repo.name},direct
                        </code>
                        <CopyButton text={`GOPROXY=${base()}/${repo.name},direct`} label="" />
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
            <FormatTag format="go" />
          </div>
          <div class="card card-pad col" style={{ gap: '14px' }}>
            <div>
              <div class="side-label">1 · Point Go at the registry</div>
              <div class="code-line">
                <code>
                  export GOPROXY={base()}/{example()},direct
                </code>
                <CopyButton text={`export GOPROXY=${base()}/${example()},direct`} label="" />
              </div>
            </div>
            <div>
              <div class="side-label">2 · Skip checksum DB for private modules</div>
              <div class="code-line">
                <code>export GONOSUMCHECK=your.private.domain/*</code>
                <CopyButton text="export GONOSUMCHECK=your.private.domain/*" label="" />
              </div>
            </div>
            <div>
              <div class="side-label">3 · Install</div>
              <div class="code-line">
                <code>go get your.private.domain/module@latest</code>
                <CopyButton text="go get your.private.domain/module@latest" label="" />
              </div>
            </div>
            <div>
              <div class="side-label">4 · Publish (from a Git tag)</div>
              <div class="code-line">
                <code>git tag v1.0.0 && git push origin v1.0.0</code>
                <CopyButton text="git tag v1.0.0 && git push origin v1.0.0" label="" />
              </div>
            </div>
          </div>
        </section>

        <div class="alert alert-info">
          <Icon name="info" size={15} />
          <span>
            Proxy-type Go repositories cache modules from upstream sources such as{' '}
            <span class="mono">proxy.golang.org</span>; hosted ones serve modules you publish
            directly.
          </span>
        </div>
      </div>
    </div>
  );
}
