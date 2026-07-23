// API Service for WiFi-DensePose UI

import { API_CONFIG, buildApiUrl } from '../config/api.config.js';
import { backendDetector } from '../utils/backend-detector.js';

export class ApiService {
  constructor() {
    this.authToken = null;
    this.requestInterceptors = [];
    this.responseInterceptors = [];
  }

  // Set authentication token
  setAuthToken(token) {
    this.authToken = token;
  }

  // Add request interceptor
  addRequestInterceptor(interceptor) {
    this.requestInterceptors.push(interceptor);
  }

  // Add response interceptor
  addResponseInterceptor(interceptor) {
    this.responseInterceptors.push(interceptor);
  }

  // Build headers for requests
  getHeaders(customHeaders = {}) {
    const headers = {
      ...API_CONFIG.DEFAULT_HEADERS,
      ...customHeaders
    };

    if (this.authToken) {
      headers['Authorization'] = `Bearer ${this.authToken}`;
    }

    return headers;
  }

  // Process request through interceptors
  async processRequest(url, options) {
    let processedUrl = url;
    let processedOptions = options;

    for (const interceptor of this.requestInterceptors) {
      const result = await interceptor(processedUrl, processedOptions);
      processedUrl = result.url || processedUrl;
      processedOptions = result.options || processedOptions;
    }

    return { url: processedUrl, options: processedOptions };
  }

  // Process response through interceptors
  async processResponse(response, url) {
    let processedResponse = response;

    for (const interceptor of this.responseInterceptors) {
      processedResponse = await interceptor(processedResponse, url);
    }

    return processedResponse;
  }

  // Generic request method
  async request(url, options = {}) {
    try {
      // Process request through interceptors
      const processed = await this.processRequest(url, options);

      // Determine the correct base URL (real backend vs mock)
      let finalUrl = processed.url;
      if (processed.url.startsWith(API_CONFIG.BASE_URL)) {
        const baseUrl = await backendDetector.getBaseUrl();
        finalUrl = processed.url.replace(API_CONFIG.BASE_URL, baseUrl);
      }
      
      // Make the request
      const response = await fetch(finalUrl, {
        ...processed.options,
        headers: this.getHeaders(processed.options.headers)
      });

      // Process response through interceptors
      const processedResponse = await this.processResponse(response, url);

      // Step-up re-authentication (ADR-271 P2).
      //
      // A browser session outlives the ~15-minute access token that created it,
      // and Cognitum publishes no introspection endpoint, so the server refuses
      // PRIVILEGED actions from a session older than a few minutes. That is a
      // 401, but it means something different from "you are not signed in" —
      // the user IS signed in, and the fix is to prove it again. The server
      // marks it with an RFC 6750 error code so the two are distinguishable.
      //
      // Without this branch a stale-session delete surfaces as a generic
      // "Request failed" and the user has no way to know that signing in again
      // resolves it.
      if (processedResponse.status === 401) {
        const challenge = processedResponse.headers.get('WWW-Authenticate') || '';
        if (challenge.includes('reauthentication required')) {
          // Full-page redirect: the flow ends by setting a cookie, which an
          // XHR cannot do usefully. Returns here with a fresh auth_time and,
          // while the Cognitum session is alive, no prompt.
          window.location.href = '/oauth/start';
          // Never settles — the navigation is already underway, and resolving
          // would let the caller render an error for an operation that is
          // simply being retried after sign-in.
          return new Promise(() => {});
        }
      }

      // Handle errors
      if (!processedResponse.ok) {
        const error = await processedResponse.json().catch(() => ({
          message: `HTTP ${processedResponse.status}: ${processedResponse.statusText}`
        }));
        throw new Error(error.message || error.detail || 'Request failed');
      }

      // Parse JSON response
      const data = await processedResponse.json().catch(() => null);
      return data;

    } catch (error) {
      // Only log if not a connection refusal (expected when DensePose API is down)
      if (error.message && !error.message.includes('Failed to fetch')) {
        console.error('API Request Error:', error);
      }
      throw error;
    }
  }

  // GET request
  async get(endpoint, params = {}, options = {}) {
    const url = buildApiUrl(endpoint, params);
    return this.request(url, {
      method: 'GET',
      ...options
    });
  }

  // POST request
  async post(endpoint, data = {}, options = {}) {
    const url = buildApiUrl(endpoint);
    return this.request(url, {
      method: 'POST',
      body: JSON.stringify(data),
      ...options
    });
  }

  // PUT request
  async put(endpoint, data = {}, options = {}) {
    const url = buildApiUrl(endpoint);
    return this.request(url, {
      method: 'PUT',
      body: JSON.stringify(data),
      ...options
    });
  }

  // DELETE request
  async delete(endpoint, options = {}) {
    const url = buildApiUrl(endpoint);
    return this.request(url, {
      method: 'DELETE',
      ...options
    });
  }
}

// Create singleton instance
export const apiService = new ApiService();

// Storage key shared with the QuickSettings "API Access" panel.
export const API_TOKEN_STORAGE_KEY = 'ruview-api-token';

// Apply a previously-saved bearer token at module load — before app init
// dispatches its first request — so a configured RUVIEW_API_TOKEN works from
// the very first /api/v1/* call. The server only ever checks the
// `Authorization: Bearer` header (see bearer_auth.rs) — this intentionally
// never puts the token in a URL query string.
try {
  const storedToken = localStorage.getItem(API_TOKEN_STORAGE_KEY);
  if (storedToken) apiService.setAuthToken(storedToken);
} catch { /* storage unavailable (private browsing etc.) */ }