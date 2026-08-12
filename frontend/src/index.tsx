/* @refresh reload */
import { ErrorBoundary } from 'solid-js';
import { render } from 'solid-js/web';
import { Router, Route } from '@solidjs/router';
import Layout from './components/Layout.tsx';
import Toaster from './components/Toaster.tsx';
import Dashboard from './pages/Dashboard.tsx';
import Packages from './pages/Packages.tsx';
import PackageDetail from './pages/PackageDetail.tsx';
import Search from './pages/Search.tsx';
import OciImages from './pages/OciImages.tsx';
import GoModules from './pages/GoModules.tsx';
import Login from './pages/Login.tsx';
import MyAccess from './pages/MyAccess.tsx';
import AdminDashboard from './pages/admin/AdminDashboard.tsx';
import Repositories from './pages/admin/Repositories.tsx';
import Users from './pages/admin/Users.tsx';
import UserTokens from './pages/admin/UserTokens.tsx';
import PackageManagement from './pages/admin/PackageManagement.tsx';
import AuditLog from './pages/admin/AuditLog.tsx';
import System from './pages/admin/System.tsx';
import PasswordChange from './pages/admin/PasswordChange.tsx';
import Webhooks from './pages/admin/Webhooks.tsx';
import './styles/global.css';

const root = document.getElementById('app');
if (!root) throw new Error('Root element not found');

/**
 * Last-resort recovery screen: any uncaught render/effect error anywhere in
 * the tree lands here instead of leaving a blank page. Plain markup on
 * existing global.css classes only — no app components that could themselves
 * throw again.
 */
function CrashScreen(props: { error: unknown }) {
  const message = () =>
    props.error instanceof Error ? props.error.message : String(props.error);
  return (
    <div class="empty" style={{ 'min-height': '100vh' }}>
      <div class="empty-title">Something went wrong</div>
      <div class="empty-text" style={{ 'margin-bottom': '14px' }}>
        The interface hit an unexpected error and stopped rendering. Reloading
        usually fixes it — your session is kept.
      </div>
      <div class="alert alert-error" role="alert" style={{ 'max-width': '520px' }}>
        <span class="mono small" style={{ 'word-break': 'break-word' }}>
          {message()}
        </span>
      </div>
      <button class="btn btn-primary" onClick={() => window.location.reload()}>
        Reload
      </button>
    </div>
  );
}

render(
  () => (
    <ErrorBoundary fallback={(error) => <CrashScreen error={error} />}>
      <Router>
        {/* Mirror of the explicit SPA route list in src/web/mod.rs — keep both lists in sync. */}
        {/* Login page has its own layout (no shell) */}
        <Route path="/login" component={Login} />

        {/* All other routes share the shell */}
        <Route path="/" component={Layout}>
          <Route path="/" component={Dashboard} />
          <Route path="/packages" component={Packages} />
          <Route path="/packages/*path" component={PackageDetail} />
          <Route path="/search" component={Search} />
          <Route path="/oci" component={OciImages} />
          <Route path="/go" component={GoModules} />
          <Route path="/account/access" component={MyAccess} />
          <Route path="/admin" component={AdminDashboard} />
          <Route path="/admin/repositories" component={Repositories} />
          <Route path="/admin/users" component={Users} />
          <Route path="/admin/users/:username/tokens" component={UserTokens} />
          <Route path="/admin/packages" component={PackageManagement} />
          <Route path="/admin/audit" component={AuditLog} />
          <Route path="/admin/system" component={System} />
          <Route path="/admin/password" component={PasswordChange} />
          <Route path="/admin/webhooks" component={Webhooks} />
        </Route>
      </Router>

      <Toaster />
    </ErrorBoundary>
  ),
  root,
);
