/**
 * Services page — lists every registered service grouped by domain.
 * Reads from `/api/services` (HA-wire-compat).
 */

import { LitElement, html, css } from 'lit';
import { customElement, state } from 'lit/decorators.js';

import { HomecoreClient } from '../api/client.js';
import type { ServiceDomainView } from '../api/types.js';

function resolveToken(): string {
    if (typeof localStorage !== 'undefined') {
        const stored = localStorage.getItem('homecore.token');
        if (stored) return stored;
    }
    const qs = new URL(window.location.href).searchParams.get('token');
    return qs ?? 'dev-token';
}

@customElement('hc-services')
export class ServicesPage extends LitElement {
    static styles = css`
        :host { display: block; padding: 24px; color: var(--hc-text, #e6eaee); font-family: var(--hc-font-sans, 'Outfit', system-ui, sans-serif); }
        h1 { font-size: 18px; font-weight: 600; margin: 0 0 16px 0; }
        .domain { background: hsl(220 20% 10%); border: 1px solid var(--hc-border, #2a323e); border-radius: 8px; margin-bottom: 12px; padding: 14px 16px; }
        .domain h2 { font-size: 14px; font-weight: 600; margin: 0 0 8px 0; color: var(--hc-primary, #19d4e5); font-family: var(--hc-font-mono, 'JetBrains Mono', monospace); }
        ul { list-style: none; padding: 0; margin: 0; display: flex; flex-wrap: wrap; gap: 6px; }
        li { background: hsl(220 25% 14%); padding: 4px 10px; border-radius: 4px; font-family: var(--hc-font-mono, 'JetBrains Mono', monospace); font-size: 12px; color: var(--hc-text-muted, #7b899d); }
        .empty { padding: 24px; border: 1px dashed var(--hc-border, #2a323e); border-radius: 8px; text-align: center; color: var(--hc-text-muted, #7b899d); }
        .err { padding: 16px; border: 1px dashed #b35a5a; border-radius: 8px; color: #f0c0c0; font-size: 13px; }
    `;

    @state() private domains: ServiceDomainView[] = [];
    @state() private error: string | null = null;
    @state() private loading = true;

    private client = new HomecoreClient({ token: resolveToken() });

    connectedCallback(): void {
        super.connectedCallback();
        void this.refresh();
    }

    private async refresh(): Promise<void> {
        try {
            const r = await fetch('/api/services', { headers: { 'Authorization': `Bearer ${resolveToken()}` } });
            if (!r.ok) throw new Error(`/api/services -> HTTP ${r.status}`);
            this.domains = await r.json();
            this.error = null;
        } catch (e) {
            this.error = e instanceof Error ? e.message : String(e);
        } finally {
            this.loading = false;
        }
        void this.client;  // suppress unused warning while keeping the import shape consistent
    }

    render() {
        if (this.error) return html`<div class="err">backend unreachable — ${this.error}</div>`;
        if (this.loading) return html`<div>loading services…</div>`;
        if (this.domains.length === 0) {
            return html`
                <h1>Services (0 domains)</h1>
                <div class="empty">
                    No services registered. Services are registered by plugins
                    (Wasmtime or InProcess) or by integrations that call
                    <code>services::register()</code> on boot.
                </div>
            `;
        }
        return html`
            <h1>Services (${this.domains.length} domain${this.domains.length === 1 ? '' : 's'})</h1>
            ${this.domains.map(d => html`
                <div class="domain">
                    <h2>${d.domain}</h2>
                    <ul>
                        ${Object.keys(d.services).map(name => html`<li>${name}</li>`)}
                    </ul>
                </div>
            `)}
        `;
    }
}

declare global { interface HTMLElementTagNameMap { 'hc-services': ServicesPage; } }
