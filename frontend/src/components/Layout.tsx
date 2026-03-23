import { createSignal } from 'solid-js';
import type { RouteSectionProps } from '@solidjs/router';
import Sidebar from './Sidebar.tsx';

export default function Layout(props: RouteSectionProps) {
  const [sidebarOpen, setSidebarOpen] = createSignal(false);

  return (
    <div class="layout">
      {/* Mobile header */}
      <div class="mobile-header">
        <button
          class="hamburger-btn"
          onClick={() => setSidebarOpen(!sidebarOpen())}
          aria-label="Toggle navigation"
        >
          &#9776;
        </button>
        <span class="mobile-brand">OpenCargo</span>
      </div>

      <Sidebar isOpen={sidebarOpen()} onClose={() => setSidebarOpen(false)} />

      <div class="main-content">
        <div class="page-content">
          {props.children}
        </div>
        <footer class="footer">
          <div class="footer-content">
            <span>OpenCargo -- Package Registry</span>
          </div>
        </footer>
      </div>
    </div>
  );
}
