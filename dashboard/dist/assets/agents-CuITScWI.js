import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,a as o,w as s}from"./router-DmWf1zNH.js";import{n as c,t as l}from"./labels-DeJm4sRU.js";import{a as u,c as d,i as f,n as p,o as m,r as h,t as g}from"./page-BvfONVkV.js";import{o as _}from"./controls-DMP3WW_j.js";var v=class extends g{constructor(...e){super(...e),this.agents=[],this.permissions=new Map,this.toggling=new Set,this.requestId=0}connectedCallback(){super.connectedCallback(),this.refresh()}watchKeys(){return[`agent:*`]}async refresh(){let t=++this.requestId;try{let n=await this.call(`agents.list`,{}),r=await Promise.allSettled(n.map(e=>this.call(`agents.get`,{agent:e.alias})));if(t!==this.requestId)return;this.agents=n,this.permissions=new Map(r.map((t,r)=>[n[r].alias,t.status===`fulfilled`?t.value:{can_toggle_enabled:!1,deny_reason:e(`de27e94`)}])),this.loadError=``}catch(e){if(t!==this.requestId)return;this.loadError=a(e)}this.loading=!1,this.requestUpdate()}async toggleEnabled(t,n){if(!(!this.permissions.get(t.alias)?.can_toggle_enabled||t.load_error||this.toggling.has(t.alias))){this.toggling.add(t.alias),this.requestUpdate();try{if(!n&&!await d({title:e(`d4fd748`,[t.display||t.alias]),body:e(`d1cae81`),confirm:e(`d4e6fd0`),danger:!0}))return;let r=await this.call(`agents.set_enabled`,{agent:t.alias,enabled:n});++this.requestId,this.agents=this.agents.map(e=>e.alias===t.alias?r:e),m(e(n?`dc17bd5`:`d1e8856`,[r.display||t.alias]),`ok`),await i.refreshStatus(!0)}catch(t){m(e(`d860480`,[a(t)]),`error`)}finally{this.toggling.delete(t.alias),this.requestUpdate()}}}card(r,i){let a=c(r,i),u=o({page:`agent`,alias:r.alias,tab:r.load_error?`yaml`:`overview`});return n`<article class="card agent-card">
      <a class="agent-card-link" href=${u} aria-label=${e(`dac11c0`,[r.display||r.alias])}>
      <div class="agent-card-top">
        <span class="avatar avatar-lg" aria-hidden="true">${[...r.display||r.alias][0]??`?`}</span>
        <div class="list-main">
          <div class="list-title"><span>${r.display||r.alias}</span></div>
          <div class="list-sub mono">${r.alias}</div>
        </div>
        <span class="chip ${a.tone?`chip-${a.tone}`:`chip-quiet`}">${a.text}</span>
      </div>
      ${r.load_error?n`<div class="agent-card-error">
            <p class="mono">${r.load_error}</p>
            <span class="link-btn">${t(`da5753c`,[s(`chevronRight`,`icon-sm`)])}</span>
          </div>`:n`<dl class="agent-card-facts">
            <div><dt>${t(`d8f6cbd`)}</dt><dd>${l(r)||`—`}</dd></div>
            <div><dt>${t(`dd334c4`)}</dt><dd>${r.enabled?e(`ddfb802`):e(`da8c369`)}</dd></div>
            <div><dt>${t(`d2dfe17`)}</dt><dd>${r.credentials===`ready`?e(`dab27f8`):n`<span class="danger-text">${t(`d65364e`)}</span>`}</dd></div>
            <div><dt>${t(`d35b49e`)}</dt><dd class="tnum">${t(`d93c883`,[r.chats])}</dd></div>
          </dl>`}
      </a>
      <div class="agent-card-toggle" title=${r.load_error?e(`dee5fa6`):this.permissions.get(r.alias)?.deny_reason??``}>
        <span>${this.toggling.has(r.alias)?e(`dd3d211`):r.enabled?e(`ddfb802`):e(`da8c369`)}</span>
        ${_({checked:r.enabled,label:e(`d2687cb`,[r.display||r.alias]),disabled:!!r.load_error||!this.permissions.get(r.alias)?.can_toggle_enabled||this.toggling.has(r.alias),onChange:e=>void this.toggleEnabled(r,e)})}
      </div>
    </article>`}render(){let t=i.state.status?.restart_pending.map(e=>e.file)??[];return n`<div class="page">
      ${f(`Agents`,e(`d421d7e`))}
      ${this.loadError?h(e(`de1816e`),this.loadError,()=>this.refresh()):r}
      ${this.loading?u(3):this.agents.length?n`<div class="agent-grid">${this.agents.map(e=>this.card(e,t))}</div>`:n`<div class="card">${p(e(`d7acc5d`),e(`dfdcc82`))}</div>`}
    </div>`}};customElements.define(`sm-page-agents`,v);export{v as AgentsPage};