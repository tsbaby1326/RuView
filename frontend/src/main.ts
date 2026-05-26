/**
 * HOMECORE frontend entry point.
 * Imports global styles, registers Lit components, and mounts the app shell.
 */

import './styles/tokens.css';
import './styles/base.css';

// Register custom elements
import './components/AppShell.js';
import './components/StateCard.js';
import './pages/Dashboard.js';

// Mount the Dashboard inside the AppShell's slot so the empty `<main>`
// layout actually shows something on first paint.
window.addEventListener('DOMContentLoaded', () => {
    const shell = document.querySelector('hc-app-shell');
    if (shell && !shell.querySelector('hc-dashboard')) {
        shell.appendChild(document.createElement('hc-dashboard'));
    }
});
