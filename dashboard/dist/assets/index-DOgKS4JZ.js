import{a as e,r as t,t as n}from"./i18n-YIZ94PuL.js";import{H as r,L as i,N as a,O as o,P as s,T as c,V as l,a as u,i as d,k as f,m as p,n as m,o as h,s as g,v as _,w as v}from"./router-DmWf1zNH.js";import{t as y}from"./live-D0VeMPic.js";import{a as b}from"./labels-DeJm4sRU.js";import{a as x,i as S,n as C,o as w,r as T,t as E}from"./nav-CzsiaecP.js";var D=[`auto`,`zh-CN`,`en`],O=class extends a{constructor(...e){super(...e),this.locale=new n(this),this.view=null,this.loading=!1,this.loaded=!1,this.saving=!1,this.error=``,this.read=0,this.onStore=()=>this.requestUpdate(),this.onOutside=e=>{e.composedPath().includes(this)||this.close()},this.onKey=e=>{let t=this.querySelector(`details`);e.key===`Escape`&&t?.open&&(this.close(),this.querySelector(`summary`)?.focus())}}createRenderRoot(){return this}connectedCallback(){super.connectedCallback(),document.addEventListener(`pointerdown`,this.onOutside),document.addEventListener(`keydown`,this.onKey),c.addEventListener(`change`,this.onStore),this.load()}disconnectedCallback(){super.disconnectedCallback(),++this.read,document.removeEventListener(`pointerdown`,this.onOutside),document.removeEventListener(`keydown`,this.onKey),c.removeEventListener(`change`,this.onStore)}close(){let e=this.querySelector(`details`);e&&(e.open=!1)}preference(){let e=this.view?.values.find(e=>e.key===`language`),t=e?.value??e?.effective;return D.includes(t)?t:`auto`}canEdit(){return this.loaded&&!!c.state.whoami?.machine&&!!this.view?.values.find(e=>e.key===`language`)?.editable}async load(){if(this.saving||!c.state.whoami?.machine)return;let e=++this.read;this.loading=!0,this.requestUpdate();try{let t=await c.call(`config.get`,{scope:{level:`global`}});if(e!==this.read)return;this.view=t,this.loaded=!0,this.error=``}catch(t){if(e!==this.read)return;this.loaded=!1,this.error=l(t)}finally{e===this.read&&(this.loading=!1,this.requestUpdate())}}async choose(e){if(this.loading||this.saving||!this.view||!this.canEdit()||e===this.preference())this.requestUpdate();else{++this.read,this.saving=!0,this.error=``,this.requestUpdate();try{let t=await c.call(`config.patch`,{scope:{level:`global`},revision:this.view.revision,changes:[{key:`language`,value:e}]});this.view={...this.view,revision:t.revision,values:this.view.values.map(t=>t.key===`language`?{...t,value:e}:t)},c.setRestartPending(t.restart_pending),await c.refreshStatus()}catch(e){this.error=l(e)}finally{this.saving=!1,this.requestUpdate()}}}render(){let t=!c.state.whoami?.machine,n=this.loading||this.saving||!this.canEdit(),r=this.view?.values.find(e=>e.key===`language`)?.deny_reason;return i`<details class="language-menu" @toggle=${e=>{e.target.open&&this.load()}}>
      <summary class="btn btn-sm" aria-label=${e(`language.title`)}>${e(`language.title`)}</summary>
      <div class="language-panel">
        <p class="muted">${e(`language.scope`)}</p>
        <div class="language-options" role="radiogroup" aria-label=${e(`language.title`)} aria-busy=${this.saving||this.loading?`true`:`false`}>
          ${D.map(t=>i`<label>
            <input type="radio" name="display-language" value=${t} .checked=${y(this.view!==null&&this.preference()===t)} ?disabled=${n}
              @change=${()=>this.choose(t)} />
            <span>${e(t===`auto`?`language.auto`:t===`zh-CN`?`language.zh-CN`:`language.en`)}</span>
          </label>`)}
        </div>
        ${t||r?i`<p class="muted">${r||e(`language.denied`)}</p>`:s}
        ${this.loading?i`<p role="status" class="muted">${e(`language.loading`)}</p>`:s}
        ${this.error?i`<p role="alert" class="row-error">${this.error}</p>
          <button class="btn btn-sm" ?disabled=${this.loading||this.saving} @click=${()=>this.load()}>${e(`language.retry`)}</button>`:s}
      </div>
    </details>`}};customElements.define(`sm-language-menu`,O);var k=`summon-dash-theme`;function A(){try{let e=localStorage.getItem(k);if(e===`light`||e===`dark`||e===`system`)return e}catch{}return`system`}function j(e){let t=document.documentElement;e===`system`?delete t.dataset.theme:t.dataset.theme=e}function M(e){j(e);try{localStorage.setItem(k,e)}catch{}}var N=[{value:`system`,get label(){return e(`d217cfe`)},icon:`monitor`},{value:`light`,get label(){return e(`daa0819`)},icon:`sun`},{value:`dark`,get label(){return e(`da6b75d`)},icon:`moon`}],P=class extends a{static{this.properties={route:{state:!0},drawer:{state:!0},theme:{state:!0}}}constructor(){super(),this.locale=new n(this),this.pageEl=null,this.globalDirty=new Set,this.pageKey=``,this.loadSeq=0,this.onStore=()=>this.requestUpdate(),this.onRoute=()=>{this.route=d(),this.drawer=!1,window.scrollTo({top:0})},this.onGlobalDirty=e=>{this.globalDirty=new Set(e.detail),this.requestUpdate()},this.onKey=e=>{e.key===`Escape`&&this.drawer&&this.closeDrawer()},this.route=d(),this.drawer=!1,this.theme=A(),j(this.theme)}createRenderRoot(){return this}openDrawer(){this.drawer=!0,this.updateComplete.then(()=>(this.querySelector(`.drawer .nav-item[aria-current="page"]`)??this.querySelector(`.drawer .nav-item`))?.focus())}closeDrawer(){this.drawer&&(this.drawer=!1,this.updateComplete.then(()=>this.querySelector(`.nav-open`)?.focus()))}connectedCallback(){super.connectedCallback(),c.addEventListener(`change`,this.onStore),window.addEventListener(m,this.onRoute),window.addEventListener(`keydown`,this.onKey),window.addEventListener(E,this.onGlobalDirty),h(this)}updated(){document.title=`summon · ${S[this.route.page]}`}disconnectedCallback(){super.disconnectedCallback(),c.removeEventListener(`change`,this.onStore),window.removeEventListener(m,this.onRoute),window.removeEventListener(`keydown`,this.onKey),window.removeEventListener(E,this.onGlobalDirty)}setTheme(e){this.theme=e,M(e)}page(){let e=this.route;if(e.page===`not_found`)return i`<div class="page">
        <div class="empty">
          <h3>${t(`de75737`)}</h3>
          <p>${t(`d37aac5`)} <code>${e.path}</code> ${t(`d9e695a`)}</p>
          <div class="empty-actions"><a class="btn" href="/">${t(`d21d1bb`)}</a></div>
        </div>
      </div>`;let n=T[e.page],r=e.page===`agent`?`agent:${e.alias}`:e.page===`chat`?`chat:${e.chatId}`:e.page===`session`?`session:${e.agent}:${e.chatId}`:e.page;if(this.pageKey!==r||!this.pageEl){this.pageKey=r;let e=++this.loadSeq;customElements.get(n.tag)?this.pageEl=document.createElement(n.tag):(this.pageEl=null,n.load().then(()=>{e===this.loadSeq&&(this.pageEl=document.createElement(n.tag),this.pageEl.route=this.route,this.requestUpdate())}))}return this.pageEl?(this.pageEl.route=e,this.pageEl):i`<div class="page" aria-busy="true"></div>`}nav(t){let n=g(this.route);return i`<nav class=${t?`nav-in-drawer`:`nav`} aria-label="${e(`d9bae68`)}">
      ${C.map(e=>i`<div class="nav-group">
          <div class="nav-group-label">${e.label}</div>
          ${e.items.filter(e=>!e.machine||c.state.whoami?.machine).map(e=>e.id===`global`&&n===`global`?i`<a class="nav-item" href=${e.href} aria-current="true">${v(e.icon)}<span>${e.label}</span></a>
                  ${this.globalSubnav()}`:i`<a class="nav-item" href=${e.href} aria-current=${e.id===n?`page`:s}>
                  ${v(e.icon)}<span>${e.label}</span>
                </a>`)}
        </div>`)}
    </nav>`}globalSubnav(){let n=this.route,r=c.state.schema,a=n.page===`global`&&n.mode===`form`?x(n,r):null;return i`<div class="nav-sub" role="group" aria-label="${e(`da5c037`)}">
      ${w(r).map(n=>i`<a
          class="nav-subitem"
          href=${u({page:`global`,mode:`form`,group:n.id})}
          aria-current=${n.id===a?`page`:s}
        >
          <span>${n.label}</span>${this.globalDirty.has(n.id)?i`<span class="nav-dirty" title="${e(`d535083`)}"><span class="sr-only">${t(`d535083`)}</span></span>`:s}
        </a>`)}
    </div>`}identity(){let n=c.state.whoami?.identity;if(!n)return i``;let r=c.state.whoami?.expires_at,a=r?e(`ddbbe72`,[p(r)]):``;if(n.kind===`owner`)return i`<span class="identity" title=${e(`dd3e20e`,[a])}>${t(`db862f2`,[v(`monitor`,`icon-sm`)])}</span>`;let o=c.state.status?.agents.find(e=>e.alias===n.agent);return i`<span class="identity" title=${e(`d3b66a3`,[o?.display||n.agent,n.chat_kind===`p2p`?e(`d6add27`):e(`d35b49e`),a])}
      >${n.name}<span class="identity-via">${t(`dd746d7`,[o?.display||n.agent])}</span></span
    >`}themeSwitch(){return i`<div class="seg seg-sm seg-icon" role="radiogroup" aria-label="${e(`d788db1`)}">
      ${N.map(e=>i`<label class="seg-opt" title=${e.label}>
          <input type="radio" name="theme" .checked=${this.theme===e.value} aria-label=${e.label} @change=${()=>this.setTheme(e.value)} />
          <span>${v(e.icon,`icon-sm`)}</span>
        </label>`)}
    </div>`}statusPill(){let n=c.state.status;if(!n)return i``;let r=n.restart_pending.length;return i`${r?i`<a class="pill pill-warn" href="/" title=${n.restart_pending.map(e=>`${e.key} · ${b(e.file)}`).join(`
`)}>
            ${t(`daca962`,[v(`refreshCw`,`icon-sm`),r])}
          </a>`:s}
      <span class="pill" title=${e(`d9a030b`,[n.daemon.pid,n.daemon.commit])}>
        <span class="dot ok"></span>${t(`d1f0eb9`)}<span class="mono">v${n.daemon.version}</span><span class="muted">${_(n.daemon.started_at)}</span>
      </span>`}renderNotice(){let{auth:e,bootError:n}=c.state;return e===`loading`?i`<div class="centered" aria-busy="true"><div class="notice">${f(c.state.icon)}<p class="muted">${t(`dfcfa09`)}</p></div></div>`:e===`error`?i`<div class="centered">
        <div class="notice">
          ${f(c.state.icon)}
          <h1>${t(`d5de14d`)}</h1>
          <p>${n}</p>
          <p class="hint"><button class="btn" @click=${()=>c.boot()}>${t(`db8784c`)}</button></p>
        </div>
      </div>`:i`<div class="centered" data-testid="reopen">
      <div class="notice">
        ${f(c.state.icon)}
        <h1>${t(`d075903`)}</h1>
        <p>${t(`d4d86ad`)}</p>
        <p class="hint">${t(`de76c9f`)} <code>/dash</code>${t(`de6c7ff`)} <code>summon dashboard open</code>。</p>
      </div>
    </div>`}render(){let{auth:n,whoami:r,status:a}=c.state;if(o(c.state.icon),n!==`ok`)return this.renderNotice();let l=this.route,u=l.page===`agent`?a?.agents.find(e=>e.alias===l.alias)?.display||l.alias:S[l.page],d=a?.hostname??r?.hostname??``;return i`<div class="shell" ?data-drawer=${this.drawer}>
      <header class="topbar">
        <button class="icon-btn only-narrow nav-open" aria-label="${e(`d38e4f2`)}" aria-expanded=${this.drawer?`true`:`false`} @click=${()=>this.openDrawer()}>
          ${v(`menu`,`icon-lg`)}
        </button>
        <a class="brand" href="/" aria-label="${e(`dd50bb0`)}">
          ${f(c.state.icon)}<span class="brand-name">summon</span><span class="brand-sub">${t(`dd0d07d`)}</span>${d?i`<span class="brand-host">· ${d}</span>`:s}
        </a>
        <div class="topbar-title">${u}</div>
        <div class="topbar-right">
          ${this.statusPill()}
          <span class="topbar-sep" aria-hidden="true"></span>
          ${this.identity()} ${this.themeSwitch()}
          <sm-language-menu></sm-language-menu>
          <span class="status-dot-only" title=${a?.restart_pending.length?e(`ddb8dcb`):e(`d12dd37`)}>
            <span class="dot ${a?.restart_pending.length?`warn`:`ok`}"></span>
          </span>
        </div>
      </header>
      ${this.nav(!1)}
      <main class="content" id="main">${this.page()}</main>
      <div class="drawer-scrim" @click=${()=>this.closeDrawer()}></div>
      <aside class="drawer" aria-label="${e(`de72622`)}" ?inert=${!this.drawer}>
        <div class="drawer-head">
          <span class="brand">${f(c.state.icon)}<span class="brand-name">summon</span><span class="brand-sub">${t(`dd0d07d`)}</span></span>
          <button class="icon-btn" aria-label="${e(`dbaf9f5`)}" @click=${()=>this.closeDrawer()}>${v(`x`,`icon-lg`)}</button>
        </div>
        ${this.nav(!0)}
        <div class="drawer-foot">
          ${a?i`<div class="identity"><span class="dot ok"></span>${t(`d122bf1`,[a.daemon.version,d])}</div>`:s}
          ${a?.restart_pending.length?i`<div class="identity"><span class="dot warn"></span>${t(`d33209d`,[a.restart_pending.length])}</div>`:s}
          <div>${this.identity()}</div>
          <div class="drawer-row">${this.themeSwitch()}</div>
        </div>
      </aside>
    </div>`}};customElements.define(`sm-app`,P);async function F(){return r}F().then(e=>(c.init(e),c.boot()));