import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { Caller } from '../app/editable';
import type { CleanupJob } from '../api/types';
import { confirmDialog } from './dialog';

/** 下载完毕后保留确认步骤；下载失败或页面切换都不会自动清理。 */
export class AgentCleanup {
  job: CleanupJob | null = null;
  busy = false;
  saved = false;
  name = '';
  error = '';
  progress = '';
  private disposed = false;

  constructor(private alias: string, private call: Caller, private update: () => void, private finished: () => Promise<void>) {}

  dispose(): void { this.disposed = true; }

  private async poll(job: CleanupJob): Promise<CleanupJob> {
    this.job = job;
    this.update();
    while (job.status === 'preparing' || job.status === 'cleaning') {
      await new Promise((resolve) => setTimeout(resolve, 800));
      if (this.disposed) throw new Error(translate('d754e37'));
      job = await this.call('agents.cleanup.status', { job: job.job });
      this.job = job;
      this.update();
    }
    if (job.status === 'failed') throw new Error(job.message || translate('d5fc5fc'));
    return job;
  }

  private async run(action: () => Promise<void>): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    this.error = '';
    this.update();
    try { await action(); }
    catch (e) { this.error = errorMessage(e); }
    finally { this.busy = false; this.progress = ''; if (!this.disposed) this.update(); }
  }

  prepare(): Promise<void> {
    return this.run(async () => {
      this.saved = false;
      this.name = '';
      this.job = null;
      await this.poll(await this.call('agents.cleanup.prepare', { agent: this.alias }));
    });
  }

  download(): Promise<void> {
    return this.run(async () => {
      const job = this.job;
      if (!job || job.status !== 'ready' || job.bytes === null) return;
      this.saved = false;
      const chunks: Uint8Array<ArrayBuffer>[] = [];
      let offset = 0;
      while (offset < job.bytes) {
        if (this.disposed) return;
        const chunk = await this.call('agents.cleanup.download', { job: job.job, offset });
        const bytes = Uint8Array.from(atob(chunk.base64), (c) => c.charCodeAt(0));
        if (chunk.offset !== offset || chunk.next !== offset + bytes.length || chunk.next > job.bytes || bytes.length === 0
          || chunk.done !== (chunk.next === job.bytes)) throw new Error(translate('de40b7d'));
        chunks.push(bytes);
        offset = chunk.next;
        this.progress = translate('d4a36c2', [Math.floor(offset / job.bytes * 100)]);
        this.update();
      }
      const blob = new Blob(chunks, { type: 'application/gzip' });
      // 非 HTTPS 的局域网入口可能没有 Web Crypto；服务端也会校验整包摘要。
      if (globalThis.crypto?.subtle && job.sha256) {
        const digest = await crypto.subtle.digest('SHA-256', await blob.arrayBuffer());
        const actual = Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('');
        if (actual !== job.sha256) throw new Error(translate('da08696'));
      }
      const url = URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = url;
      link.download = job.filename;
      link.click();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
      this.job = await this.call('agents.cleanup.status', { job: job.job });
    });
  }

  async confirm(): Promise<void> {
    if (!this.job?.downloaded || !this.saved || this.name !== this.alias || this.busy) return;
    const ok = await confirmDialog({ title: translate('d57f820', [this.alias]), body: translate('d41de15'), confirm: translate('d89c90f'), danger: true });
    if (!ok) return;
    return this.run(async () => {
      const result = await this.poll(await this.call('agents.cleanup.confirm', { job: this.job!.job, saved: true, agent: this.alias }));
      if (result.status === 'done') {
        if (result.message) this.error = result.message;
        else await this.finished();
      }
    });
  }

  render(): TemplateResult {
    const j = this.job;
    return html`<section class="group" aria-label="${translate('d6187b8')}">
      <header class="group-head"><div><h2>${rich('d2a53b2')}</h2><p>${rich('d80e385')}</p></div></header>
      <div class="card card-pad">
        <p>${rich('d7ec841')}</p>
        <button class="btn" ?disabled=${this.busy} @click=${() => this.prepare()}>${j ? translate('d45ef18') : translate('d1099b9')}</button>
        ${this.busy ? html`<p role="status">${this.progress || (j?.status === 'cleaning' ? translate('d6dfddc') : translate('d574ec7'))}</p>` : nothing}
        ${j?.status === 'ready' ? html`<p>${j.filename} · ${((j.bytes ?? 0) / 1024 / 1024).toFixed(1)} MB</p>
          <button class="btn btn-primary" ?disabled=${this.busy} @click=${() => this.download()}>${rich('d3d1ec6')}</button>
          ${j.downloaded ? html`<p><label><input type="checkbox" .checked=${this.saved} @change=${(e: Event) => { this.saved = (e.target as HTMLInputElement).checked; this.update(); }}> ${rich('d55060d')}</label></p>
            <label>${rich('d88c7bb')} <code>${this.alias}</code> ${rich('d7ba2d6')}
              <input class="input" aria-label="${translate('dada317')}" .value=${this.name} @input=${(e: Event) => { this.name = (e.target as HTMLInputElement).value; this.update(); }}>
            </label>
            <p><button class="btn btn-danger" ?disabled=${this.busy || !this.saved || this.name !== this.alias} @click=${() => this.confirm()}>${rich('d89c90f')}</button></p>` : nothing}` : nothing}
        ${this.error ? html`<p class="row-error" role="alert">${this.error}</p>` : nothing}
      </div>
    </section>`;
  }
}
