import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,a as o,m as s,v as c,w as l}from"./router-DmWf1zNH.js";import{a as u,n as d,t as f}from"./labels-DeJm4sRU.js";import{a as p,d as m,i as h,r as g,t as _}from"./page-BvfONVkV.js";import{n as v,t as y}from"./attention-BMSjx7xM.js";function b(e){switch(e.page){case`global`:return o(e.key?{page:`global`,mode:`form`,key:e.key}:{page:`global`,mode:`form`});case`agent`:return e.agent?o({page:`agent`,alias:e.agent,tab:`yaml`}):`/agents`;case`chat`:return e.chat_id?o(e.agent?{page:`chat`,chatId:e.chat_id,agent:e.agent}:{page:`chat`,chatId:e.chat_id}):`/chats`;case`schedules`:return`/schedules`;case`session`:return e.agent&&e.chat_id?o({page:`session`,agent:e.agent,chatId:e.chat_id}):`/sessions`}}var x=()=>[[`working`,e(`d1f0eb9`)],[`waiting`,e(`de54bd4`)],[`blocked`,e(`d7045c6`)],[`leaked`,e(`d456e98`)]],S=class extends _{constructor(...e){super(...e),this.attention=new y,this.onVisibility=()=>{document.visibilityState===`visible`&&this.refresh()},this.issues=[],this.sessions=null}connectedCallback(){super.connectedCallback(),this.refresh(),this.timer=setInterval(()=>{document.visibilityState===`visible`&&this.refresh()},5e3),document.addEventListener(`visibilitychange`,this.onVisibility)}disconnectedCallback(){super.disconnectedCallback(),clearInterval(this.timer),document.removeEventListener(`visibilitychange`,this.onVisibility)}watchKeys(){return[`global`,`agent:*`]}async refresh(){let e=this.attention.refresh(()=>this.call(`attention.list`,{})),t=this.call(`sessions.list`,{}).then(e=>e.sessions,()=>null);try{let[e]=await Promise.all([this.call(`status.issues`,{}),i.refreshStatus(!0)]);this.issues=e,this.loadError=``}catch(e){this.loadError=a(e)}this.sessions=await t,await e,this.loading=!1,this.requestUpdate()}daemonCard(){let a=i.state.status,o=a.daemon,d=o.autostart===`on`?e(`d8a4ef3`):o.autostart===`off`?e(`d3cffa9`):e(`d4d8c1c`);return n`<section class="page-section" aria-labelledby="ov-daemon">
      <div class="group-head"><h2 id="ov-daemon">daemon</h2></div>
      <div class="card">
        <div class="stats">
          <div class="stat">
            <div class="stat-label">${t(`d5f76b2`)}</div>
            <div class="stat-value">v${o.version} <span class="mono">${o.commit}</span></div>
          </div>
          <div class="stat">
            <div class="stat-label">${t(`db1e3a9`)}</div>
            <div class="stat-value" title=${e(`d7f8c46`,[s(o.started_at)])}>${c(o.started_at)}</div>
          </div>
          <div class="stat">
            <div class="stat-label">${t(`dde38b9`)}</div>
            <div class="stat-value">${d}</div>
          </div>
          <div class="stat">
            <div class="stat-label">${t(`d172099`)}</div>
            <div class="stat-value">${a.hostname} <span class="mono">pid ${o.pid}</span></div>
          </div>
        </div>
        ${a.restart_pending.length?n`<div class="issue issue-warn">
              ${l(`refreshCw`)}
              <div class="issue-body">
                <div class="issue-title">${t(`d9b0d94`,[a.restart_pending.length])}</div>
                <div class="issue-detail">
                  ${a.restart_pending.map((e,t)=>n`${t?`、`:``}<code>${e.key}</code>（${u(e.file)}）`)}
                </div>
                <div class="issue-fix"><span>${t(`dfb1d16`)} <code>summon daemon restart</code>${t(`d00c0e0`)}</span></div>
              </div>
            </div>`:r}
      </div>
    </section>`}runningCard(){let e=this.sessions;return e?n`<section class="page-section" aria-labelledby="ov-running">
      <div class="group-head">
        <h2 id="ov-running">${t(`de46b02`)}</h2>
        <a class="link-btn quiet" href=${o({page:`usage`})}>${t(`d4cd877`,[l(`chevronRight`,`icon-sm`)])}</a>
      </div>
      <div class="card">
        <div class="stats">
          ${x().map(([t,r])=>n`<a class="stat stat-link" href=${o({page:`sessions`})}>
              <div class="stat-label">${r}</div>
              <div class="stat-value">${e.filter(e=>e.state===t).length}</div>
            </a>`)}
        </div>
      </div>
    </section>`:r}issuesCard(){let i=this.issues;return n`<section class="page-section" aria-labelledby="ov-issues">
      <div class="group-head">
        <div>
          <h2 id="ov-issues">${t(`db7e3e7`)}</h2>
          <p>${t(`dab1152`)}</p>
        </div>
      </div>
      ${this.loading?p(2):i.length?n`<div class="card">
              ${i.map(t=>n`<div class="issue issue-${t.severity}">
                  ${l(t.severity===`error`?`circleAlert`:`triangleAlert`)}
                  <div class="issue-body">
                    <div class="issue-title">${m(t.title)}</div>
                    <div class="issue-detail">${m(t.detail)}</div>
                    <div class="issue-fix">
                      <span>${m(t.fix)}</span>
                      ${t.link?n`<a class="link-btn" href=${b(t.link)}>${t.link.page===`session`?e(`da748cc`):e(`d90e01a`)}${l(`chevronRight`,`icon-sm`)}</a>`:r}
                    </div>
                  </div>
                </div>`)}
            </div>`:n`<div class="card">
              <div class="empty"><h3>${t(`d4f5245`)}</h3><p>${t(`d1d32ac`)}</p></div>
            </div>`}
    </section>`}agentsCard(){let e=i.state.status,r=e.restart_pending.map(e=>e.file);return n`<section class="page-section" aria-labelledby="ov-agents">
      <div class="group-head">
        <h2 id="ov-agents">Agents</h2>
        <a class="link-btn quiet" href="/agents">${t(`df293a2`,[l(`chevronRight`,`icon-sm`)])}</a>
      </div>
      <div class="card">
        ${e.agents.map(e=>{let t=d(e,r);return n`<a class="list-row" href=${o({page:`agent`,alias:e.alias,tab:e.load_error?`yaml`:`overview`})}>
            <span class="avatar" aria-hidden="true">${[...e.display||e.alias][0]??`?`}</span>
            <div class="list-main">
              <div class="list-title"><span>${e.display||e.alias}</span><span class="mono muted">${e.alias}</span></div>
              <div class="list-sub">${f(e)}</div>
            </div>
            <div class="list-aside">
              <span class="chip ${t.tone?`chip-${t.tone}`:`chip-quiet`}">${t.text}</span>
              ${l(`chevronRight`,`icon-sm`)}
            </div>
          </a>`})}
      </div>
    </section>`}render(){return n`<div class="page">
      ${h(e(`dfea405`),e(`d86c2ba`))}
      ${this.loadError?g(e(`dd7a880`),this.loadError,()=>this.refresh()):r}
      ${this.daemonCard()} ${v(this.attention,``,!0)} ${this.runningCard()} ${this.issuesCard()} ${this.agentsCard()}
    </div>`}};customElements.define(`sm-page-overview`,S);export{b as issueHref};