import { For, Show, type JSX } from 'solid-js';
import Icon from './Icon.tsx';
import CopyButton from './CopyButton.tsx';
import EmptyState from './EmptyState.tsx';
import { FormatTag, VisibilityChip } from './bits.tsx';
import { fetchRepositories } from '../core/api.ts';
import { createLiveResource } from '../core/stores/live.ts';
import { session } from '../core/stores/session.ts';

export interface QuickstartStep {
  label: string;
  command: string;
}

interface FormatLandingPageProps {
  /** Repository format this page fronts ('oci', 'go', …). */
  format: string;
  /** Icon used on repository cards and the empty state. */
  icon: string;
  title: string;
  subtitle: string;
  /** Heading of the repositories section. */
  reposTitle: string;
  emptyTitle: string;
  /** Empty-state text shown to admins / to everyone else. */
  emptyTextAdmin: string;
  emptyTextOther: string;
  /** Example repository name for the quickstart when none exists yet. */
  exampleFallback: string;
  /** Command line displayed on each repository card. */
  repoCommand: (repoName: string) => string;
  /** Quickstart steps, given the example repository name. */
  steps: (example: string) => QuickstartStep[];
  /** Content of the trailing info alert. */
  alert: JSX.Element;
}

/**
 * Shared landing page for format-specific registries (Containers, Go
 * modules): repository cards with a copyable command, a quickstart, and an
 * info alert. The thin pages in pages/OciImages.tsx and pages/GoModules.tsx
 * only provide the wording and commands.
 */
export default function FormatLandingPage(props: FormatLandingPageProps) {
  const [repos] = createLiveResource(fetchRepositories, ['repositories.changed']);
  const formatRepos = () =>
    (repos()?.repositories ?? []).filter((r) => r.format === props.format);
  const example = () => formatRepos()[0]?.name ?? props.exampleFallback;

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">{props.title}</h1>
          <p class="page-sub">{props.subtitle}</p>
        </div>
      </div>

      <div class="stagger">
        <section class="section">
          <div class="section-head">
            <span class="section-title">{props.reposTitle}</span>
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
              when={formatRepos().length > 0}
              fallback={
                <div class="card">
                  <EmptyState
                    icon={props.icon}
                    title={props.emptyTitle}
                    text={session.isAdmin() ? props.emptyTextAdmin : props.emptyTextOther}
                  />
                </div>
              }
            >
              <div class="grid-cards">
                <For each={formatRepos()}>
                  {(repo) => (
                    <div class="card card-pad card-hover">
                      <div class="row" style={{ 'margin-bottom': '10px' }}>
                        <Icon name={props.icon} size={16} class="icon dim" />
                        <span class="mono grow truncate" style={{ color: 'var(--ink)' }}>
                          {repo.name}
                        </span>
                        <VisibilityChip visibility={repo.visibility} />
                      </div>
                      <div class="code-line">
                        <code>{props.repoCommand(repo.name)}</code>
                        <CopyButton text={props.repoCommand(repo.name)} label="" />
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
            <FormatTag format={props.format} />
          </div>
          <div class="card card-pad col" style={{ gap: '14px' }}>
            <For each={props.steps(example())}>
              {(step) => (
                <div>
                  <div class="side-label">{step.label}</div>
                  <div class="code-line">
                    <code>{step.command}</code>
                    <CopyButton text={step.command} label="" />
                  </div>
                </div>
              )}
            </For>
          </div>
        </section>

        <div class="alert alert-info">
          <Icon name="info" size={15} />
          <span>{props.alert}</span>
        </div>
      </div>
    </div>
  );
}
