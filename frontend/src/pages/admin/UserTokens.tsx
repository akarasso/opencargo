import { createSignal, createResource, For, Show } from 'solid-js';
import { useParams, A } from '@solidjs/router';
import { fetchTokens, createToken, deleteToken, type Token, type CreateTokenResponse } from '../../lib/api.ts';
import Badge from '../../components/Badge.tsx';
import { ConfirmModal } from '../../components/Modal.tsx';
import Modal from '../../components/Modal.tsx';
import CopyButton from '../../components/CopyButton.tsx';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { toast } from '../../components/Toast.tsx';

export default function UserTokens() {
  const params = useParams<{ username: string }>();
  const username = () => params.username;

  const [tokens, { refetch }] = createResource(username, fetchTokens);
  const [showCreate, setShowCreate] = createSignal(false);
  const [deletingToken, setDeletingToken] = createSignal<string | null>(null);
  const [createdToken, setCreatedToken] = createSignal<CreateTokenResponse | null>(null);

  // Create form state
  const [tokenName, setTokenName] = createSignal('');
  const [expiresInDays, setExpiresInDays] = createSignal('30');
  const [createLoading, setCreateLoading] = createSignal(false);

  async function handleCreate(e: Event) {
    e.preventDefault();
    setCreateLoading(true);
    try {
      const days = parseInt(expiresInDays(), 10);
      const result = await createToken(username(), {
        name: tokenName(),
        expires_in_days: isNaN(days) ? undefined : days,
      });
      setCreatedToken(result);
      setShowCreate(false);
      setTokenName('');
      setExpiresInDays('30');
      refetch();
      toast.success('Token created. Copy it now -- it will not be shown again.');
    } catch (err: any) {
      toast.error(err.message || 'Failed to create token.');
    }
    setCreateLoading(false);
  }

  async function handleDelete() {
    const tokenId = deletingToken();
    if (!tokenId) return;
    try {
      await deleteToken(username(), tokenId);
      toast.success('Token revoked.');
      setDeletingToken(null);
      refetch();
    } catch (err: any) {
      toast.error(err.message || 'Failed to revoke token.');
    }
  }

  return (
    <>
      <div class="page-header">
        <h1 class="page-title">Tokens for {username()}</h1>
        <p class="page-subtitle">
          <A href="/admin/users" style={{ "text-decoration": "underline" }}>Users</A>
          {' / '}{username()} / Tokens
        </p>
        <div class="page-header-actions">
          <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
            Create Token
          </button>
        </div>
      </div>

      {/* Show newly created token */}
      <Show when={createdToken()}>
        {(ct) => (
          <div class="token-display">
            <div class="token-display-label">
              New token created: {ct().name}. Copy it now -- it will not be shown again.
            </div>
            <div class="token-display-value">
              <span>{ct().token}</span>
              <CopyButton text={ct().token} />
            </div>
          </div>
        )}
      </Show>

      <Show when={tokens.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={tokens.error}>
        <div class="alert alert-error">Failed to load tokens.</div>
      </Show>

      <Show when={tokens()}>
        {(list) => (
          <Show
            when={list().length > 0}
            fallback={
              <EmptyState
                title="No tokens"
                text={`No API tokens for ${username()}.`}
              />
            }
          >
            <div class="data-table-wrapper">
              <table class="data-table">
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Prefix</th>
                    <th>Expires</th>
                    <th>Last Used</th>
                    <th>Created</th>
                    <th>Actions</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={list()}>
                    {(token) => (
                      <tr>
                        <td style={{ "font-weight": "500" }}>{token.name}</td>
                        <td><span class="mono">{token.prefix}...</span></td>
                        <td class="data-table-muted">
                          {token.expires_at || 'Never'}
                        </td>
                        <td class="data-table-muted">
                          {token.last_used_at || 'Never'}
                        </td>
                        <td class="data-table-muted">{token.created_at}</td>
                        <td>
                          <button
                            class="btn btn-ghost btn-sm"
                            style={{ color: 'var(--clr-danger)' }}
                            onClick={() => setDeletingToken(token.id)}
                          >
                            Revoke
                          </button>
                        </td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </div>
          </Show>
        )}
      </Show>

      {/* Create token modal */}
      <Modal
        open={showCreate()}
        title="Create API Token"
        onClose={() => setShowCreate(false)}
        actions={
          <>
            <button class="btn btn-secondary" onClick={() => setShowCreate(false)}>Cancel</button>
            <button class="btn btn-primary" onClick={handleCreate} disabled={createLoading()}>
              {createLoading() ? 'Creating...' : 'Create Token'}
            </button>
          </>
        }
      >
        <form onSubmit={handleCreate}>
          <div class="form-group">
            <label class="form-label">Token Name</label>
            <input
              class="form-input"
              type="text"
              value={tokenName()}
              onInput={(e) => setTokenName(e.currentTarget.value)}
              placeholder="e.g. CI, deploy, dev"
              required
            />
          </div>
          <div class="form-group">
            <label class="form-label">Expires in (days)</label>
            <input
              class="form-input"
              type="number"
              value={expiresInDays()}
              onInput={(e) => setExpiresInDays(e.currentTarget.value)}
              placeholder="30"
              min="1"
            />
            <div class="form-hint">Leave empty for no expiration.</div>
          </div>
        </form>
      </Modal>

      {/* Delete confirmation */}
      <ConfirmModal
        open={deletingToken() !== null}
        title="Revoke Token"
        message="Are you sure you want to revoke this token? Any tools or scripts using it will lose access."
        confirmLabel="Revoke Token"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeletingToken(null)}
      />
    </>
  );
}
