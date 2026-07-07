import { Show, type JSX } from 'solid-js';
import { A } from '@solidjs/router';
import EmptyState from './EmptyState.tsx';
import RoleBadge from './RoleBadge.tsx';
import { session } from '../core/stores/session.ts';

function CheckingSession() {
  return (
    <div class="card card-pad" style={{ 'max-width': '520px', margin: '48px auto' }}>
      <div class="skeleton skeleton-text" style={{ width: '38%', 'margin-bottom': '12px' }} />
      <div class="skeleton skeleton-text" style={{ width: '86%', 'margin-bottom': '8px' }} />
      <div class="skeleton skeleton-text" style={{ width: '64%' }} />
    </div>
  );
}

/** Renders children only for signed-in users; otherwise explains and offers sign-in. */
export function RequireAuth(props: { children: JSX.Element }) {
  return (
    <Show when={!session.checking()} fallback={<CheckingSession />}>
      <Show
        when={session.isAuthenticated()}
        fallback={
          <EmptyState
            icon="lock"
            title="Sign in required"
            text="This page shows information tied to your account."
          >
            <A class="btn btn-primary" href="/login">
              Sign in
            </A>
          </EmptyState>
        }
      >
        {props.children}
      </Show>
    </Show>
  );
}

/** Renders children only for admins; otherwise states the required right. */
export function RequireAdmin(props: { children: JSX.Element }) {
  return (
    <Show when={!session.checking()} fallback={<CheckingSession />}>
      <Show
        when={session.isAdmin()}
        fallback={
          <Show
            when={session.isAuthenticated()}
            fallback={
              <EmptyState
                icon="lock"
                title="Sign in required"
                text="Administration requires an admin account."
              >
                <A class="btn btn-primary" href="/login">
                  Sign in
                </A>
              </EmptyState>
            }
          >
            <EmptyState
              icon="shield"
              title="Admin access required"
              text="You are signed in without the admin role, so this area is read-protected."
            >
              <div class="row" style={{ 'margin-top': '10px' }}>
                <span class="dim small">Signed in as</span>
                <span class="mono small">{session.user()?.username}</span>
                <RoleBadge role={session.user()?.role ?? 'reader'} />
              </div>
              <A class="btn btn-ghost" href="/account/access" style={{ 'margin-top': '14px' }}>
                See what you can access
              </A>
            </EmptyState>
          </Show>
        }
      >
        {props.children}
      </Show>
    </Show>
  );
}
