/**
 * Settings page — backend config + bearer-token editor (localStorage).
 */

import { LitElement, html, css } from 'lit';
import { customElement, state } from 'lit/decorators.js';

import { HomecoreClient } from '../api/client.js';
import type { ApiConfig } from '../api/types.js';

function resolveToken(): string {
    if (typeof localStorage !== 'undefined') {
        const stored = localStorage.getItem('homecore.token');
        if (stored) return stored;
    }
    const qs = new URL(window.location.href).searchParams.get('token');
    return qs ?? 'dev-token';
}

@customElement('hc-settings')
export class SettingsPage extends LitElement {
    static styles = css`
        :host { display: block; padding: 24px; color: var(--hc-text, #e6eaee); font-family: var(--hc-font-sans, 'Outfit', system-ui, sans-serif); }
        h1 { font-size: 18px; font-weight: 600; margin: 0 0 16px 0; }
        section { background: hsl(220 20% 10%); border: 1px solid var(--hc-border, #2a323e); border-radius: 8px; padding: 16px; margin-bottom: 16px; }
        h2 { font-size: 14px; font-weight: 600; margin: 0 0 12px 0; color: var(--hc-primary, #19d4e5); }
        dl { display: grid; grid-template-columns: max-content 1fr; gap: 6px 18px; margin: 0; font-size: 13px; font-family: var(--hc-font-mono, 'JetBrains Mono', monospace); }
        dt { color: var(--hc-text-muted, #7b899d); }
        dd { margin: 0; }
        label { display: block; margin-bottom: 6px; font-size: 13px; color: var(--hc-text-muted, #7b899d); }
        input { width: 100%; box-sizing: border-box; padding: 8px 12px; background: hsl(220 25% 14%); border: 1px solid var(--hc-border, #2a323e); border-radius: 6px; color: var(--hc-text, #e6eaee); font-family: var(--hc-font-mono, 'JetBrains Mono', monospace); font-size: 13px; }
        button { margin-top: 10px; padding: 8px 16px; background: var(--hc-primary, #19d4e5); color: var(--hc-primary-fg, #0b0e13); border: none; border-radius: 6px; font-weight: 600; font-size: 13px; cursor: pointer; font-family: var(--hc-font-sans, 'Outfit', system-ui, sans-serif); }
        button:hover { background: hsl(185 80% 55%); }
        .toast { font-size: 12px; color: var(--hc-primary, #19d4e5); margin-top: 8px; }
        .err { padding: 16px; border: 1px dashed #b35a5a; border-radius: 8px; color: #f0c0c0; font-size: 13px; }
    `;

    @state() private config: ApiConfig | null = null;
    @state() private error: string | null = null;
    @state() private token = resolveToken();
    @state() private savedAt = 0;

    private client = new HomecoreClient({ token: resolveToken() });

    connectedCallback(): void {
        super.connectedCallback();
        void this.refresh();
    }

    private async refresh(): Promise<void> {
        try {
            this.config = await this.client.getConfig();
            this.error = null;
        } catch (e) {
            this.error = e instanceof Error ? e.message : String(e);
        }
    }

    private saveToken() {
        localStorage.setItem('homecore.token', this.token);
        this.savedAt = Date.now();
        this.client = new HomecoreClient({ token: this.token });
        void this.refresh();
    }

    render() {
        return html`
            <h1>Settings</h1>
            <section>
                <h2>backend</h2>
                ${this.error
                    ? html`<div class="err">unreachable — ${this.error}</div>`
                    : this.config
                    ? html`<dl>
                          <dt>location</dt><dd>${this.config.location_name}</dd>
                          <dt>version</dt><dd>${this.config.version}</dd>
                          <dt>state</dt><dd>${this.config.state}</dd>
                          <dt>components</dt><dd>${this.config.components.join(', ')}</dd>
                      </dl>`
                    : html`loading…`}
            </section>
            <section>
                <h2>auth — bearer token</h2>
                <label for="tok">stored at localStorage["homecore.token"]; DEV mode accepts any non-empty value</label>
                <input id="tok" type="password" .value=${this.token}
                       @input=${(e: Event) => (this.token = (e.target as HTMLInputElement).value)} />
                <button @click=${this.saveToken}>save & reload backend</button>
                ${this.savedAt > 0 ? html`<div class="toast">saved at ${new Date(this.savedAt).toLocaleTimeString()}</div>` : ''}
            </section>
        `;
    }
}

declare global { interface HTMLElementTagNameMap { 'hc-settings': SettingsPage; } }
