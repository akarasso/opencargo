import { For, Show, createResource, createSignal } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import Modal, { ConfirmModal } from '../../components/Modal.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { FormatTag, LoadError, TableSkeleton } from '../../components/bits.tsx';
import {
  createRoutingRule,
  deleteRoutingRule,
  explainRoute,
  fetchRepositories,
  fetchRoutingRules,
} from '../../core/api.ts';
import { createLiveResource } from '../../core/stores/live.ts';
import { reportError, toasts } from '../../core/stores/toasts.ts';
import type { Explanation, RoutingRule } from '../../core/types.ts';

const FORMATS = ['npm', 'cargo', 'go', 'oci', 'pypi', 'maven', 'nuget'];

const EFFECTS: [string, string][] = [
  ['allow_members', 'served only by the hosted repositories I name'],
  ['allow_hosted', 'served by any hosted repository, present and future'],
  ['deny', 'served by nobody'],
];

export default function Routing() {
  return (
    <RequireAdmin>
      <RoutingInner />
    </RequireAdmin>
  );
}

function lines(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((p) => p.trim())
    .filter(Boolean);
}

function RoutingInner() {
  const [rules, refetch] = createLiveResource(fetchRoutingRules, ['audit.entry'], {
    debounce: 500,
  });
  const [repos] = createResource(fetchRepositories);
  const hosted = (format: string) =>
    (repos()?.repositories ?? [])
      .filter((r) => r.type === 'hosted' && r.format === format)
      .map((r) => r.name);

  const [showCreate, setShowCreate] = createSignal(false);
  const [deleting, setDeleting] = createSignal<RoutingRule | null>(null);
  const [busy, setBusy] = createSignal(false);

  const [name, setName] = createSignal('');
  const [format, setFormat] = createSignal('npm');
  const [patterns, setPatterns] = createSignal('');
  const [except, setExcept] = createSignal('');
  const [effect, setEffect] = createSignal('allow_members');
  const [targets, setTargets] = createSignal<string[]>([]);

  function openCreate() {
    setName('');
    setFormat('npm');
    setPatterns('');
    setExcept('');
    setEffect('allow_members');
    setTargets([]);
    setShowCreate(true);
  }

  async function handleCreate(e: Event) {
    e.preventDefault();
    setBusy(true);
    try {
      await createRoutingRule({
        name: name(),
        format: format(),
        patterns: lines(patterns()),
        except: lines(except()),
        effect: effect(),
        targets: effect() === 'allow_members' ? targets() : [],
      });
      toasts.success('Rule created', 'Every repository of this format follows it from now.');
      setShowCreate(false);
      refetch();
    } catch (err) {
      reportError('Could not create the rule', err);
    } finally {
      setBusy(false);
    }
  }

  async function handleDelete() {
    const rule = deleting();
    if (!rule) return;
    setBusy(true);
    try {
      const answer = await deleteRoutingRule(rule.name);
      const still = answer.still_refused_by ?? [];
      if (still.length) {
        toasts.info('Rule deleted', `Those names stay refused by ${still.join(', ')}.`);
      } else {
        toasts.success('Rule deleted', 'Those names are served by every member again.');
      }
      setDeleting(null);
      refetch();
    } catch (err) {
      reportError('Could not delete the rule', err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Routing</h1>
          <p class="page-sub">
            Which members of a group may answer for a name. A rule only ever removes members, never
            adds one, and it applies to every repository of its format — present and future.
          </p>
        </div>
        <button class="btn btn-primary" onClick={openCreate}>
          <Icon name="plus" size={16} />
          New rule
        </button>
      </div>

      <Show when={rules.error}>
        <LoadError what="the routing rules" />
      </Show>

      <Show when={rules()} fallback={<TableSkeleton rows={4} cols={5} />}>
        {(data) => (
          <>
            <p class="page-sub">
              Snapshot version {data().snapshot_version}. A node that has not refreshed this for
              longer than <code>routing.max_snapshot_age</code> refuses the proxy members of every
              format a rule speaks for, rather than serve what it knew before the rule.
            </p>
            <Show
              when={data().rules.length}
              fallback={
                <EmptyState
                  icon="anchor"
                  title="No routing rule"
                  text="Nothing is pinned: every group resolves through all of its members, and a public package of the same name can answer."
                />
              }
            >
              <table class="table">
                <thead>
                  <tr>
                    <th>Rule</th>
                    <th>Format</th>
                    <th>Patterns</th>
                    <th>Who may answer</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={data().rules}>
                    {(rule) => (
                      <tr>
                        <td>{rule.name}</td>
                        <td>
                          <FormatTag format={rule.format} />
                        </td>
                        <td>
                          <code>{rule.patterns.join(' ')}</code>
                          <Show when={rule.except.length}>
                            <div class="page-sub">
                              except <code>{rule.except.join(' ')}</code> — that exact spelling, and
                              no other
                            </div>
                          </Show>
                        </td>
                        <td>
                          <EffectCell rule={rule} />
                        </td>
                        <td style={{ 'text-align': 'right' }}>
                          <button class="btn btn-sm btn-danger" onClick={() => setDeleting(rule)}>
                            Delete
                          </button>
                        </td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </Show>
          </>
        )}
      </Show>

      <TestName />

      <Modal
        open={showCreate()}
        title="New routing rule"
        onClose={() => setShowCreate(false)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setShowCreate(false)}>
              Cancel
            </button>
            <button type="submit" form="create-rule-form" class="btn btn-primary" disabled={busy()}>
              {busy() ? 'Creating…' : 'Create rule'}
            </button>
          </>
        }
      >
        <form id="create-rule-form" onSubmit={handleCreate}>
          <div class="field">
            <label class="field-label" for="create-rule-name">
              Name
            </label>
            <input
              id="create-rule-name"
              class="input"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              placeholder="acme-internal"
              spellcheck={false}
              required
            />
          </div>
          <div class="field">
            <label class="field-label" for="create-rule-format">
              Format
            </label>
            <select
              id="create-rule-format"
              class="input"
              value={format()}
              onChange={(e) => setFormat(e.currentTarget.value)}
            >
              <For each={FORMATS}>{(f) => <option value={f}>{f}</option>}</For>
            </select>
          </div>
          <div class="field">
            <label class="field-label" for="create-rule-patterns">
              Patterns, one per line
            </label>
            <textarea
              id="create-rule-patterns"
              class="input"
              rows={3}
              value={patterns()}
              onInput={(e) => setPatterns(e.currentTarget.value)}
              placeholder="@acme/*"
              spellcheck={false}
              required
            />
          </div>
          <div class="field">
            <label class="field-label" for="create-rule-except">
              Exceptions, one per line — exact names, never patterns
            </label>
            <textarea
              id="create-rule-except"
              class="input"
              rows={2}
              value={except()}
              onInput={(e) => setExcept(e.currentTarget.value)}
              placeholder="@acme/public-ui"
              spellcheck={false}
            />
          </div>
          <div class="field">
            <label class="field-label" for="create-rule-effect">
              Who may answer
            </label>
            <select
              id="create-rule-effect"
              class="input"
              value={effect()}
              onChange={(e) => setEffect(e.currentTarget.value)}
            >
              <For each={EFFECTS}>{([id, text]) => <option value={id}>{text}</option>}</For>
            </select>
          </div>
          <Show when={effect() === 'allow_members'}>
            <div class="field">
              <span class="field-label">Hosted repositories</span>
              <For each={hosted(format())}>
                {(repo) => (
                  <label class="check">
                    <input
                      type="checkbox"
                      checked={targets().includes(repo)}
                      onChange={() =>
                        setTargets((list) =>
                          list.includes(repo) ? list.filter((r) => r !== repo) : [...list, repo],
                        )
                      }
                    />
                    <span>{repo}</span>
                  </label>
                )}
              </For>
            </div>
          </Show>
          <Show when={effect() === 'allow_hosted'}>
            <div class="alert alert-warn" role="alert">
              <Icon name="alert-triangle" size={16} />
              <div>
                Any hosted repository of this format in the group may answer — one added six months
                from now included, and a package promoted into it included. Name the repositories
                explicitly unless you mean exactly that.
              </div>
            </div>
          </Show>
        </form>
      </Modal>

      <ConfirmModal
        open={deleting() !== null}
        title="Delete this routing rule?"
        message={`The names ${deleting()?.name ?? ''} covers are served by every member again, unless another rule still refuses them — the answer will say which.`}
        confirmLabel="Delete rule"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeleting(null)}
      />
    </div>
  );
}

function EffectCell(props: { rule: RoutingRule }) {
  return (
    <Show
      when={props.rule.effect === 'allow_members'}
      fallback={
        <span class={props.rule.effect === 'deny' ? 'chip chip-danger' : 'chip chip-warn'}>
          {props.rule.effect === 'deny' ? 'nobody' : 'any hosted, now and later'}
        </span>
      }
    >
      <span class="chip chip-ok">
        {props.rule.targets.length} hosted {props.rule.targets.length === 1 ? 'repository' : 'repositories'}
      </span>
    </Show>
  );
}

/** The dry run: the same decision the resolver makes, on the same snapshot. */
function TestName() {
  const [repos] = createResource(fetchRepositories);
  const [repository, setRepository] = createSignal('');
  const [name, setName] = createSignal('');
  const [answer, setAnswer] = createSignal<Explanation | null>(null);
  const [busy, setBusy] = createSignal(false);

  async function run(e: Event) {
    e.preventDefault();
    setBusy(true);
    try {
      setAnswer(await explainRoute(repository(), name()));
    } catch (err) {
      reportError('Could not explain that name', err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <section class="card">
      <h2 class="card-title">Test a name</h2>
      <p class="page-sub">
        What the resolver would do, member by member. Patterns are compared on the matching key,
        which folds spellings together; an exception is compared on the identity key, which does
        not — so an exception reopens exactly the name you wrote and nothing around it.
      </p>
      <form onSubmit={run}>
        <div class="field">
          <label class="field-label" for="explain-repo">
            Repository
          </label>
          <select
            id="explain-repo"
            class="input"
            value={repository()}
            onChange={(e) => setRepository(e.currentTarget.value)}
            required
          >
            <option value="">choose…</option>
            <For each={repos()?.repositories ?? []}>
              {(r) => <option value={r.name}>{r.name}</option>}
            </For>
          </select>
        </div>
        <div class="field">
          <label class="field-label" for="explain-name">
            Name
          </label>
          <input
            id="explain-name"
            class="input"
            value={name()}
            onInput={(e) => setName(e.currentTarget.value)}
            placeholder="@acme/widget"
            spellcheck={false}
            required
          />
        </div>
        <button type="submit" class="btn btn-primary" disabled={busy()}>
          {busy() ? 'Explaining…' : 'Explain'}
        </button>
      </form>

      <Show when={answer()}>
        {(a) => (
          <>
            <p class="page-sub">
              matching key <code>{a().match_key}</code> · identity <code>{a().ident_key}</code> ·
              snapshot {a().snapshot_version}
            </p>
            <table class="table">
              <thead>
                <tr>
                  <th>Member</th>
                  <th>Kind</th>
                  <th>Verdict</th>
                </tr>
              </thead>
              <tbody>
                <For each={a().members}>
                  {(m) => (
                    <tr>
                      <td>{m.name}</td>
                      <td>{m.kind}</td>
                      <td>
                        <Show
                          when={m.admitted}
                          fallback={
                            <span class="chip chip-danger">
                              {m.stale
                                ? 'refused: this node has not refreshed its rules'
                                : `refused by ${m.refused_by.join(', ')}`}
                            </span>
                          }
                        >
                          <span class="chip chip-ok">asked</span>
                        </Show>
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </>
        )}
      </Show>
    </section>
  );
}
