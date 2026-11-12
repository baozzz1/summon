import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,_ as o,a as s,w as c}from"./router-DmWf1zNH.js";import{t as l}from"./live-D0VeMPic.js";import{i as u,o as d,r as f}from"./labels-DeJm4sRU.js";import{a as p,i as m,n as h,r as g,t as _}from"./page-BvfONVkV.js";import{t as v}from"./ident-Bw3GTApD.js";function y(e){return e.agents.reduce((e,t)=>e+t.overrides,0)}var b=class extends _{constructor(...e){super(...e),this.chats=[],this.query=``,this.onlyOverrides=!1}connectedCallback(){super.connectedCallback(),this.refresh()}watchKeys(){return[`chat:*`]}async refresh(){try{let e=await this.call(`chats.list`,{});this.chats=e.sort((e,t)=>(t.last_active??``).localeCompare(e.last_active??``)),this.loadError=``}catch(e){this.loadError=a(e)}this.loading=!1,this.requestUpdate()}display(e){return i.state.status?.agents.find(t=>t.alias===e)?.display||e}filtered(){let e=this.query.trim().toLowerCase();return this.chats.filter(t=>this.onlyOverrides&&!y(t)?!1:!e||[u(t),t.chat_id,...t.agents.flatMap(e=>[e.agent,this.display(e.agent)])].join(` `).toLowerCase().includes(e))}row(t){let i=y(t),a=u(t);return n`<a class="chat-row" href=${s({page:`chat`,chatId:t.chat_id})}>
      <div class="chat-name">
        <span class="avatar" aria-hidden="true">${f(t)}</span>
        <div class="chat-name-text">
          <strong>${d(t)||t.kind===`p2p`?a:v(t.chat_id,{agent:t.agents[0]?.agent??``})}</strong>
          ${d(t)?n`<span class="mono">${t.chat_id}</span>`:r}
        </div>
      </div>
      <div class="agent-tags">
        ${t.agents.map(t=>n`<span class="agent-tag" title=${t.live?e(`d7d817d`):e(`dc1f158`)}
            ><span class="dot ${t.live?`ok`:``}"></span>${this.display(t.agent)}</span
          >`)}
      </div>
      <span class="cell-muted cell-overrides count">${i?e(`d8af892`,[i]):`—`}</span>
      <span class="cell-muted cell-active">${o(t.last_active)}</span>
      <span class="chat-meta-narrow">${i?e(`ded467e`,[i]):e(`d27e364`)} · ${o(t.last_active)}</span>
      ${c(`chevronRight`,`icon-sm`)}
    </a>`}render(){let i=this.filtered();return n`<div class="page">
      ${m(e(`d35b49e`),e(`d1088a7`))}
      ${this.loadError?g(e(`d2ac9a4`),this.loadError,()=>this.refresh()):r}
      <div class="toolbar">
        <label class="search">
          ${c(`search`)}
          <input
            class="field-input"
            type="search"
            placeholder="${e(`dc3f81b`)}"
            aria-label="${e(`d890042`)}"
            .value=${l(this.query)}
            @input=${e=>{this.query=e.target.value,this.requestUpdate()}}
          />
        </label>
        <label class="check">
          <input
            type="checkbox"
            .checked=${this.onlyOverrides}
            @change=${e=>{this.onlyOverrides=e.target.checked,this.requestUpdate()}}
          />${t(`d5fb2f7`)}
        </label>
      </div>
      ${this.loading?p(5):this.chats.length?i.length?n`<div class="card">
                <div class="chat-list-head" aria-hidden="true">
                  <span>${t(`d35b49e`)}</span><span>${t(`dc5bccb`)}</span><span>${t(`df2330c`)}</span><span>${t(`dc1d6dc`)}</span><span></span>
                </div>
                ${i.map(e=>this.row(e))}
              </div>`:n`<div class="card">
                ${h(e(`de2c9f9`),this.onlyOverrides?e(`d7aeac8`):e(`d5a7c1c`))}
              </div>`:n`<div class="card">${h(e(`d437c79`),e(`d9f6e64`))}</div>`}
    </div>`}};customElements.define(`sm-page-chats`,b);export{y as overrideCount};