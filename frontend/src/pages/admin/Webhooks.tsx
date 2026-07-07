import { For, Show, createSignal } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import Modal, { ConfirmModal } from '../../components/Modal.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import {
  createWebhook,
  deleteWebhook,
  fetchWebhooks,
  testWebhook,
  updateWebhook,
} from '../../core/api.ts';
import { createLiveResource } from '../../core/stores/live.ts';
import { toasts } from '../../core/stores/toasts.ts';
import { timeAgo } from '../../core/format.ts';
import type { Webhook } from '../../core/types.ts';

const KNOWN_EVENTS = ['package.published', 'package.promoted'];

export default function Webhooks() {
  return (
    <RequireAdmin>
      <WebhooksInner />
    </RequireAdmin>
  );
}

function WebhooksInner() {
  const [hooks, refetch] = createLiveResource(fetchWebhooks, ['audit.entry'], { debounce: 500 });

  const [showCreate, setShowCreate] = createSignal(false);
  const [editing, setEditing] = createSignal<Webhook | null>(null);
  const [deleting, setDeleting] = createSignal<Webhook | null>(null);
  const [busy, setBusy] = createSignal(false);

  const [url, setUrl] = createSignal('');
  const [events, setEvents] = createSignal<string[]>(['package.published']);
  const [secret, setSecret] = createSignal('');

  function toggleEvent(list: string[], ev: string): string[] {
    return list.includes(ev) ? list.filter((e) => e !== ev) : [...list, ev];
  }

  function openCreate() {
    setUrl('');
    setEvents(['package.published']);
    setSecret('');
    setShowCreate(true);
  }

  function openEdit(hook: Webhook) {
    setUrl(hook.url);
    setEvents(hook.events.includes('*') ? [...KNOWN_EVENTS] : [...hook.events]);
    setSecret('');
    setEditing(hook);
  }

  async function handleCreate(e: Event) {
    e.preventDefault();
    if (!url()) return;
    setBusy(true);
    try {
      await createWebhook({ url: url(), events: events(), secret: secret() || undefined });
      toasts.success('Webhook created', url());
      setShowCreate(false);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not create webhook', err instanceof Error ? err.message : undefined);
    }
    setBusy(false);
  }

  async function handleEdit(e: Event) {
    e.preventDefault();
    const hook = editing();
    if (!hook) return;
    setBusy(true);
    try {
      await updateWebhook(hook.id, {
        url: url(),
        events: events(),
        ...(secret() ? { secret: secret() } : {}),
      });
      toasts.success('Webhook updated');
      setEditing(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not update webhook', err instanceof Error ? err.message : undefined);
    }
    setBusy(false);
  }

  async function handleToggleActive(hook: Webhook) {
    try {
      await updateWebhook(hook.id, { active: !hook.active });
      toasts.success(hook.active ? 'Webhook paused' : 'Webhook resumed', hook.url);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not update webhook', err instanceof Error ? err.message : undefined);
    }
  }

  async function handleTest(hook: Webhook) {
    try {
      await testWebhook(hook.id);
      toasts.success('Test event sent', `Check the receiver behind ${hook.url}`);
    } catch (err: unknown) {
      toasts.error('Test delivery failed', err instanceof Error ? err.message : undefined);
    }
  }

  async function handleDelete() {
    const hook = deleting();
    if (!hook) return;
    try {
      await deleteWebhook(hook.id);
      toasts.success('Webhook deleted');
      setDeleting(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not delete webhook', err instanceof Error ? err.message : undefined);
    }
  }

  const EventPicker = () => (
    <div class="field">
      <label class="field-label">Events</label>
      <div class="row-wrap">
        <For each={KNOWN_EVENTS}>
          {(ev) => (
            <label class="checkbox-row">
              <input
                type="checkbox"
                checked={events().includes(ev)}
                onChange={() => setEvents((list) => toggleEvent(list, ev))}
              />
              <span class="mono small">{ev}</span>
            </label>
          )}
        </For>
      </div>
      <div class="field-hint">Nothing selected means every event (*).</div>
    </div>
  );

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Webhooks</h1>
          <p class="page-sub">
            HTTP callbacks fired on registry events — wire them into CI, chat, or monitoring.
            Payloads are signed with the shared secret when one is set.
          </p>
        </div>
        <div class="page-actions">
          <button class="btn btn-primary" onClick={openCreate}>
            <Icon name="plus" size={14} />
            New webhook
          </button>
        </div>
      </div>

      <Show when={hooks.error}>
        <LoadError what="webhooks" />
      </Show>

      <Show when={hooks()} fallback={<TableSkeleton rows={3} cols={4} />}>
        {(list) => (
          <Show
            when={list().webhooks.length > 0}
            fallback={
              <div class="card">
                <EmptyState
                  icon="webhook"
                  title="No webhooks yet"
                  text="Add an endpoint and the registry will call it whenever packages are published or promoted."
                >
                  <button class="btn btn-primary" onClick={openCreate}>
                    <Icon name="plus" size={14} />
                    New webhook
                  </button>
                </EmptyState>
              </div>
            }
          >
            <div class="table-card page-enter">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>Status</th>
                      <th>Endpoint</th>
                      <th>Events</th>
                      <th class="cell-hide-sm">Created</th>
                      <th style={{ 'text-align': 'right' }}>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={list().webhooks}>
                      {(hook) => (
                        <tr>
                          <td>
                            <button
                              class={`switch ${hook.active ? 'on' : ''}`}
                              role="switch"
                              aria-checked={hook.active}
                              title={hook.active ? 'Active — click to pause' : 'Paused — click to resume'}
                              onClick={() => handleToggleActive(hook)}
                            />
                          </td>
                          <td class="cell-mono truncate" style={{ 'max-width': '300px', color: 'var(--ink)' }}>
                            {hook.url}
                          </td>
                          <td>
                            <div class="row-wrap">
                              <For each={hook.events}>
                                {(ev) => <span class="chip chip-neutral mono">{ev}</span>}
                              </For>
                            </div>
                          </td>
                          <td class="cell-dim cell-hide-sm nowrap" title={hook.created_at}>
                            {timeAgo(hook.created_at)}
                          </td>
                          <td>
                            <div class="cell-actions">
                              <button class="btn btn-ghost btn-sm" title="Send a test event" onClick={() => handleTest(hook)}>
                                <Icon name="send" size={13} />
                                Test
                              </button>
                              <button class="btn btn-quiet btn-icon" title="Edit webhook" onClick={() => openEdit(hook)}>
                                <Icon name="pencil" size={14} />
                              </button>
                              <button class="btn btn-quiet btn-icon" title="Delete webhook" onClick={() => setDeleting(hook)}>
                                <Icon name="trash" size={14} />
                              </button>
                            </div>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </div>
          </Show>
        )}
      </Show>

      {/* Create */}
      <Modal
        open={showCreate()}
        title="New webhook"
        onClose={() => setShowCreate(false)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setShowCreate(false)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleCreate} disabled={busy() || !url()}>
              {busy() ? 'Creating…' : 'Create webhook'}
            </button>
          </>
        }
      >
        <form onSubmit={handleCreate}>
          <div class="field">
            <label class="field-label">Endpoint URL</label>
            <input
              class="input"
              type="url"
              value={url()}
              onInput={(e) => setUrl(e.currentTarget.value)}
              placeholder="https://ci.example.com/hooks/registry"
              spellcheck={false}
              required
            />
          </div>
          <EventPicker />
          <div class="field">
            <label class="field-label">Secret (optional)</label>
            <input
              class="input"
              value={secret()}
              onInput={(e) => setSecret(e.currentTarget.value)}
              placeholder="Used to sign payloads (X-OpenCargo-Signature)"
              spellcheck={false}
            />
          </div>
        </form>
      </Modal>

      {/* Edit */}
      <Modal
        open={editing() !== null}
        title="Edit webhook"
        onClose={() => setEditing(null)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setEditing(null)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleEdit} disabled={busy() || !url()}>
              {busy() ? 'Saving…' : 'Save changes'}
            </button>
          </>
        }
      >
        <form onSubmit={handleEdit}>
          <div class="field">
            <label class="field-label">Endpoint URL</label>
            <input class="input" type="url" value={url()} onInput={(e) => setUrl(e.currentTarget.value)} required spellcheck={false} />
          </div>
          <EventPicker />
          <div class="field">
            <label class="field-label">Secret</label>
            <input
              class="input"
              value={secret()}
              onInput={(e) => setSecret(e.currentTarget.value)}
              placeholder="Leave blank to keep the current one"
              spellcheck={false}
            />
          </div>
        </form>
      </Modal>

      <ConfirmModal
        open={deleting() !== null}
        title="Delete this webhook?"
        message={`${deleting()?.url ?? ''} will stop receiving registry events immediately.`}
        confirmLabel="Delete webhook"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeleting(null)}
      />
    </div>
  );
}
