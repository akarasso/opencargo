import { For, Show, createResource, createSignal } from 'solid-js';
import { A, useParams } from '@solidjs/router';
import Icon from '../../components/Icon.tsx';
import Modal, { ConfirmModal } from '../../components/Modal.tsx';
import CopyButton from '../../components/CopyButton.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAuth } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import { createToken, deleteToken, fetchTokens } from '../../core/api.ts';
import { session } from '../../core/stores/session.ts';
import { toasts } from '../../core/stores/toasts.ts';
import { timeAgo } from '../../core/format.ts';
import type { CreateTokenResponse } from '../../core/types.ts';

export default function UserTokens() {
  return (
    <RequireAuth>
      <UserTokensInner />
    </RequireAuth>
  );
}

function UserTokensInner() {
  const params = useParams<{ username: string }>();
  const username = () => params.username;
  const isSelf = () => session.user()?.username === username();

  const [tokens, { refetch }] = createResource(username, fetchTokens);
  const [showCreate, setShowCreate] = createSignal(false);
  const [deletingToken, setDeletingToken] = createSignal<string | null>(null);
  const [createdToken, setCreatedToken] = createSignal<CreateTokenResponse | null>(null);

  const [tokenName, setTokenName] = createSignal('');
  const [expiresInDays, setExpiresInDays] = createSignal('30');
  const [createLoading, setCreateLoading] = createSignal(false);

  async function handleCreate(e: Event) {
    e.preventDefault();
    if (!tokenName()) return;
    setCreateLoading(true);
    try {
      const days = parseInt(expiresInDays(), 10);
      const result = await createToken(username(), {
        name: tokenName(),
        expires_in_days: Number.isNaN(days) ? undefined : days,
      });
      setCreatedToken(result);
      setShowCreate(false);
      setTokenName('');
      setExpiresInDays('30');
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not create token', err instanceof Error ? err.message : undefined);
    }
    setCreateLoading(false);
  }

  async function handleDelete() {
    const tokenId = deletingToken();
    if (!tokenId) return;
    try {
      await deleteToken(username(), tokenId);
      toasts.success('Token revoked', 'Anything still using it now gets 401s.');
      setDeletingToken(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not revoke token', err instanceof Error ? err.message : undefined);
    }
  }

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">API tokens</h1>
          <p class="page-sub">
            <Show when={isSelf()} fallback={<>Tokens held by <span class="mono">{username()}</span> — you're viewing as admin.</>}>
              Bearer tokens for npm, cargo, docker and the HTTP API — same rights as your account.
            </Show>
          </p>
        </div>
        <div class="page-actions">
          <Show when={session.isAdmin() && !isSelf()}>
            <A class="btn btn-ghost" href="/admin/users">
              <Icon name="chevron-left" size={14} />
              All users
            </A>
          </Show>
          <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
            <Icon name="plus" size={14} />
            New token
          </button>
        </div>
      </div>

      <Show when={createdToken()}>
        {(ct) => (
          <div class="token-reveal page-enter">
            <div class="token-reveal-label">
              <Icon name="key" size={13} />
              {ct().name} — copy it now, it won't be shown again
            </div>
            <div class="code-line" style={{ background: 'transparent' }}>
              <code>{ct().token}</code>
              <CopyButton text={ct().token} />
            </div>
          </div>
        )}
      </Show>

      <Show when={tokens.error}>
        <LoadError what="tokens" detail={session.isAdmin() ? undefined : 'You can only view your own tokens.'} />
      </Show>

      <Show when={tokens()} fallback={<Show when={!tokens.error}><TableSkeleton rows={3} cols={5} /></Show>}>
        {(list) => (
          <Show
            when={list().length > 0}
            fallback={
              <div class="card">
                <EmptyState
                  icon="key"
                  title="No tokens yet"
                  text="Create one to publish from CI or sign in package managers without your password."
                >
                  <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
                    <Icon name="plus" size={14} />
                    New token
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
                      <th>Name</th>
                      <th>Prefix</th>
                      <th>Expires</th>
                      <th class="cell-hide-sm">Last used</th>
                      <th class="cell-hide-sm">Created</th>
                      <th style={{ 'text-align': 'right' }}>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={list()}>
                      {(token) => (
                        <tr>
                          <td style={{ 'font-weight': 500, color: 'var(--ink)' }}>{token.name}</td>
                          <td class="cell-mono cell-muted">{token.prefix}…</td>
                          <td class="cell-muted nowrap">
                            {token.expires_at ? timeAgo(token.expires_at) : 'never'}
                          </td>
                          <td class="cell-dim cell-hide-sm nowrap">
                            {token.last_used_at ? timeAgo(token.last_used_at) : 'never'}
                          </td>
                          <td class="cell-dim cell-hide-sm nowrap" title={token.created_at}>
                            {timeAgo(token.created_at)}
                          </td>
                          <td>
                            <div class="cell-actions">
                              <button class="btn btn-danger btn-sm" onClick={() => setDeletingToken(token.id)}>
                                Revoke
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

      <Modal
        open={showCreate()}
        title="New API token"
        subtitle={`For ${username()} — it inherits the account's rights.`}
        onClose={() => setShowCreate(false)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setShowCreate(false)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleCreate} disabled={createLoading() || !tokenName()}>
              {createLoading() ? 'Creating…' : 'Create token'}
            </button>
          </>
        }
      >
        <form onSubmit={handleCreate}>
          <div class="field">
            <label class="field-label">Name</label>
            <input
              class="input"
              value={tokenName()}
              onInput={(e) => setTokenName(e.currentTarget.value)}
              placeholder="ci-deploy"
              spellcheck={false}
              required
            />
            <div class="field-hint">Name it after where it will live — you'll thank yourself when revoking.</div>
          </div>
          <div class="field">
            <label class="field-label">Expires in (days)</label>
            <input
              class="input"
              type="number"
              min="1"
              value={expiresInDays()}
              onInput={(e) => setExpiresInDays(e.currentTarget.value)}
              placeholder="30"
            />
            <div class="field-hint">Leave empty for a non-expiring token.</div>
          </div>
        </form>
      </Modal>

      <ConfirmModal
        open={deletingToken() !== null}
        title="Revoke this token?"
        message="Any tool or pipeline still using it will lose access immediately. This can't be undone."
        confirmLabel="Revoke token"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeletingToken(null)}
      />
    </div>
  );
}
