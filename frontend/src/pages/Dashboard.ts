/**
 * Dashboard page — fetches HOMECORE state + config from the backend and
 * populates the `<hc-app-shell>` slot with a grid of `<hc-state-card>`.
 *
 * Auth: reads bearer from `localStorage["homecore.token"]`, the
 * `?token=` query string, or `HOMECORE_TOKEN` `<meta>` tag — in that
 * order. Falls back to the literal "dev-token" in DEV-mode backends
 * (any non-empty bearer is accepted when HOMECORE_TOKENS is unset).
 */

import { LitElement, html, css } from 'lit';
import { customElement, state } from 'lit/decorators.js';

import { HomecoreClient } from '../api/client.js';
import type { ApiConfig, StateView } from '../api/types.js';

function resolveToken(): string {
    if (typeof localStorage !== 'undefined') {
        const stored = localStorage.getItem('homecore.token');
        if (stored) return stored;
    }
    const url = new URL(window.location.href);
    const qs = url.searchParams.get('token');
    if (qs) return qs;
    const meta = document.querySelector<HTMLMetaElement>('meta[name="homecore-token"]');
    if (meta?.content) return meta.content;
    return 'dev-token';
}

@customElement('hc-dashboard')
export class Dashboard extends LitElement {
    static styles = css`
        :host {
            display: block;
            padding: 24px;
            color: var(--hc-fg, #e6e9ec);
            font-family: var(--hc-font-sans, 'Outfit', system-ui, sans-serif);
        }
        .meta {
            display: flex;
            gap: 16px;
            flex-wrap: wrap;
            color: var(--hc-fg-dim, #8a93a0);
            font-size: 14px;
            margin-bottom: 16px;
        }
        .meta strong { color: var(--hc-fg, #e6e9ec); }
        .grid {
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
            gap: 16px;
        }
        .empty,
        .err {
            padding: 24px;
            border: 1px dashed var(--hc-border, #2a323e);
            border-radius: 8px;
            text-align: center;
            color: var(--hc-fg-dim, #8a93a0);
        }
        .err {
            border-color: #b35a5a;
            color: #f0c0c0;
            text-align: left;
            font-family: var(--hc-font-mono, 'JetBrains Mono', monospace);
            font-size: 13px;
            white-space: pre-wrap;
        }
    `;

    @state() private states: StateView[] = [];
    @state() private config: ApiConfig | null = null;
    @state() private error: string | null = null;
    @state() private loading = true;

    private client = new HomecoreClient({ token: resolveToken() });
    private pollTimer: number | undefined;

    connectedCallback(): void {
        super.connectedCallback();
        void this.refresh();
        this.pollTimer = window.setInterval(() => void this.refresh(), 5000);
    }

    disconnectedCallback(): void {
        if (this.pollTimer !== undefined) window.clearInterval(this.pollTimer);
        super.disconnectedCallback();
    }

    private async refresh(): Promise<void> {
        try {
            const [cfg, states] = await Promise.all([
                this.client.getConfig(),
                this.client.getStates(),
            ]);
            this.config = cfg;
            this.states = states;
            this.error = null;
        } catch (e) {
            this.error = e instanceof Error ? e.message : String(e);
        } finally {
            this.loading = false;
        }
    }

    render() {
        if (this.error) {
            return html`<div class="err">backend unreachable — ${this.error}\n\n
                hint: make sure homecore-server is running on :8123 and that
                the token in localStorage["homecore.token"] is accepted.
            </div>`;
        }
        if (this.loading) {
            return html`<div class="empty">loading HOMECORE state…</div>`;
        }
        const v = this.config?.version ?? '?';
        const loc = this.config?.location_name ?? 'Home';
        return html`
            <div class="meta">
                <span><strong>${loc}</strong></span>
                <span>HOMECORE v<strong>${v}</strong></span>
                <span><strong>${this.states.length}</strong> entities</span>
            </div>
            ${this.states.length === 0
                ? html`<div class="empty">
                      No entities registered yet. Run
                      <code>bash scripts/homecore-seed.sh</code> to populate
                      ~10 demo entities, or connect a plugin / integration.
                  </div>`
                : html`<div class="grid">
                      ${this.states.map(
                          (s) => html`<hc-state-card .state=${s}></hc-state-card>`
                      )}
                  </div>`}
        `;
    }
}

declare global {
    interface HTMLElementTagNameMap {
        'hc-dashboard': Dashboard;
    }
}
