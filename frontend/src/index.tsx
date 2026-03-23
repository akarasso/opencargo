/* @refresh reload */
import { render } from 'solid-js/web';
import { Router, Route } from '@solidjs/router';
import Layout from './components/Layout.tsx';
import ToastContainer from './components/Toast.tsx';
import Dashboard from './pages/Dashboard.tsx';
import Packages from './pages/Packages.tsx';
import PackageDetail from './pages/PackageDetail.tsx';
import Search from './pages/Search.tsx';
import OciImages from './pages/OciImages.tsx';
import GoModules from './pages/GoModules.tsx';
import Login from './pages/Login.tsx';
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

render(
  () => (
    <>
      <Router>
        {/* Login page has its own layout (no sidebar) */}
        <Route path="/login" component={Login} />

        {/* All other routes use the sidebar layout */}
        <Route path="/" component={Layout}>
          <Route path="/" component={Dashboard} />
          <Route path="/packages" component={Packages} />
          <Route path="/packages/*path" component={PackageDetail} />
          <Route path="/search" component={Search} />
          <Route path="/oci" component={OciImages} />
          <Route path="/go" component={GoModules} />
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

      <ToastContainer />
    </>
  ),
  root,
);
