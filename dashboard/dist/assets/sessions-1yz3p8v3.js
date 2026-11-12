import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,V as i,_ as a,a as o,g as s,w as c}from"./router-DmWf1zNH.js";import{a as l,c as u,i as d,n as f,o as p,r as m,t as h}from"./page-BvfONVkV.js";import{t as g}from"./ident-Bw3GTApD.js";import{a as _}from"./controls-DMP3WW_j.js";import{n as v,t as y}from"./attention-BMSjx7xM.js";var b=[{action:`handled`,get label(){return e(`d83c5d1`)},icon:`checkCheck`},{action:`interrupt`,get label(){return e(`df9d193`)},icon:`pause`},{action:`exit`,get label(){return e(`d498e1d`)},icon:`logOut`},{action:`archive`,get label(){return e(`d5292ab`)},icon:`archive`,danger:!0}],x=12e4,S=new Map;function C(e){return`${e.agent}/${e.tag}`}function w(e,t,n,r=Date.now()){(t===`exit`||t===`archive`)&&(n===`done`||n===`pending`)&&S.set(C(e),r+x)}function T(e,t=Date.now()){let n=S.get(C(e));return n===void 0?!1:n>t||(S.delete(C(e)),!1)}function E(e){let t=new Set(e.map(C));for(let e of[...S.keys()])t.has(e)||S.delete(e)}function D(t,n){switch(n){case`done`:return{handled:{text:e(`dd96a41`),kind:`ok`},interrupt:{text:e(`d1f6d57`),kind:`ok`},exit:{text:e(`de56609`),kind:`ok`},archive:{text:e(`d110c3e`),kind:`ok`}}[t];case`pending`:return{text:e(`d878370`),kind:`info`};case`absent`:return t===`handled`?{text:e(`d0d83ea`),kind:`info`}:{text:e(`d494cf1`),kind:`info`};case`stale`:return{text:e(`db7aed6`),kind:`info`};case`not_running`:return{text:e(`daae017`),kind:`info`};case`superseded`:return{text:e(`d0aaeeb`),kind:`warn`};case`running`:return{text:e(`de5e510`),kind:`warn`}}}function O(t){return e(`dba1bf2`,[t.agent_display,t.name||t.chat_id||t.tag])}async function k(t,n){return u({title:e(n===`exit`?`dd1a037`:`dd78d0a`),body:e(n===`exit`?`d765745`:`d433dab`,[O(t)]),confirm:e(n===`exit`?`d1db2de`:`d08c5c8`),danger:!0})}async function A(t,n,r){if(!n.chat_id)return null;let a={agent:n.agent,chat_id:n.chat_id,action:r};try{if(r===`handled`)return(await t(`sessions.act`,{...a,reply:n.waiting_reply??``})).outcome;if(r===`interrupt`)return(await t(`sessions.act`,{...a,turn_id:n.turn_id??``})).outcome;let e=!1;if(n.busy){if(!await k(n,r))return null;e=!0}let i=(await t(`sessions.act`,{...a,interrupt:e})).outcome;if(i===`running`){if(!await k(n,r))return null;i=(await t(`sessions.act`,{...a,interrupt:!0})).outcome}return w(n,r,i),i}catch(t){return p(e(`d5bc4c4`,[i(t)]),`error`),null}}function j(i,a,o){let s=b.filter(e=>i.actions[e.action]);return s.length?T(i)?n`<div class="sess-actions"><span class="chip chip-quiet">${t(`dde464b`)}</span></div>`:n`<div class="sess-actions" role="group" aria-label="${e(`de6f6d8`)}">
    ${s.map(e=>n`<button
        type="button"
        class="btn btn-sm btn-ghost ${e.danger?`sess-danger`:``}"
        title=${e.label}
        aria-label=${e.label}
        ?disabled=${a!==void 0}
        @click=${t=>{t.preventDefault(),t.stopPropagation(),o(e.action)}}
      >
        ${a===e.action?c(`loaderCircle`,`icon-sm fs-spin`):c(e.icon,`icon-sm`)}<span class="sess-action-label"
          >${e.label}</span
        >
      </button>`)}
  </div>`:r}var M=5e3,N=[`leaked`,`blocked`,`waiting`,`working`,`idle`],P={get leaked(){return e(`d456e98`)},get blocked(){return e(`d7045c6`)},get waiting(){return e(`de54bd4`)},get working(){return e(`d1f0eb9`)},get idle(){return e(`ddae661`)}},F={get leaked(){return e(`db3cf7a`)},get blocked(){return e(`d55ed8a`)},get waiting(){return e(`dce48ec`)},get working(){return e(`ddabf21`)},idle:``},I={leaked:`danger`,blocked:`warn`,waiting:`accent`,working:`ok`,idle:``},L={get permission(){return e(`db5306c`)},get question(){return e(`d4d1895`)},get auth(){return e(`dd7f81b`)}};function R(t,n=new Date){switch(t.state){case`blocked`:return L[t.blocked??``]??e(`d7045c6`);case`working`:return t.turn_started_at?e(`defc4cf`,[s(t.turn_started_at,n)]):e(`d144faa`);case`waiting`:return t.waiting_since?e(`d64df6b`,[s(t.waiting_since,n)]):e(`de54bd4`);case`leaked`:return e(`df8ceb2`);case`idle`:return t.last_active?e(`dbfa5de`,[a(t.last_active,n)]):e(`ddae661`)}}function z(e){return N.map(t=>({state:t,rows:e.filter(e=>e.state===t)})).filter(e=>e.rows.length)}function B(e){return e.name?e.name:e.chat_id?g(e.chat_id,{agent:e.agent}):e.tag}var V=class extends h{constructor(...e){super(...e),this.attention=new y,this.rows=[],this.commentTasks=[],this.commentTasksError=``,this.agent=``,this.acting=new Map,this.onVisibility=()=>{document.visibilityState===`visible`&&this.refresh()}}connectedCallback(){super.connectedCallback(),this.refresh(),this.timer=setInterval(()=>{document.visibilityState===`visible`&&this.refresh()},M),document.addEventListener(`visibilitychange`,this.onVisibility)}disconnectedCallback(){super.disconnectedCallback(),clearInterval(this.timer),document.removeEventListener(`visibilitychange`,this.onVisibility)}async refresh(){let e=this.attention.refresh(()=>this.call(`attention.list`,{}));try{let e=await this.call(`sessions.list`,{});this.rows=e.sessions,this.commentTasks=e.comment_tasks??[],this.commentTasksError=e.comment_tasks_error??``,E(this.rows),this.loadError=``}catch(e){this.loadError=i(e)}await e,this.loading=!1,this.requestUpdate()}async act(e,t){let n=C(e);this.acting.set(n,t),this.requestUpdate();let r=await A(this.call,e,t);if(r){let{text:e,kind:n}=D(t,r);p(e,n)}await this.refresh(),this.acting.delete(n),this.requestUpdate()}row(e){let i=B(e),a=n`<span class="dot ${I[e.state]}" aria-hidden="true"></span>
      <span class="sess-name">
        <strong>${i}</strong>
        <span class="agent-tag">${e.agent_display}</span>
        ${e.chat_type===`p2p`?n`<span class="chip chip-quiet">${t(`d6add27`)}</span>`:r}
      </span>
      <span class="sess-meta">${R(e)}</span>`;return n`<div class="sess-row" data-state=${e.state}>
      ${e.chat_id?n`<a class="sess-main" href=${o({page:`session`,agent:e.agent,chatId:e.chat_id})}>${a}</a>`:n`<div class="sess-main">${a}</div>`}
      ${j(e,this.acting.get(C(e)),t=>void this.act(e,t))}
    </div>`}filters(){let t=[...new Map([...this.rows.map(e=>[e.agent,e.agent_display]),...this.commentTasks.map(e=>[e.agent,e.agent])]).entries()];return n`<div class="toolbar">
      ${_({label:e(`dbbe7b2`),value:this.agent,options:[{value:``,label:e(`d198f2f`)},...t.map(([e,t])=>({value:e,label:t}))],onChange:e=>{this.agent=e??``,this.requestUpdate()}})}
      <span class="sess-counts">
        ${z(this.rows).map(e=>n`<span class="sess-count"><span class="dot ${I[e.state]}"></span>${P[e.state]} ${e.rows.length}</span>`)}
      </span>
    </div>`}render(){let i=this.agent?this.rows.filter(e=>e.agent===this.agent):this.rows;return n`<div class="page">
      ${d(e(`da63280`),e(`d4d760f`))}
      ${v(this.attention,this.agent)}
      ${this.loadError?m(e(`d064365`),this.loadError,()=>this.refresh()):r}
      ${this.commentTasksError?m(e(`d3a68cc`),this.commentTasksError,()=>this.refresh()):r}
      ${this.commentTasks.length?n`<p><a href=${o({page:`comments`})}>${t(`d55b263`,[this.commentTasks.length])}</a></p>`:r}
      ${this.loading?l(5):this.rows.length?n`${this.filters()}
              ${z(i).map(e=>n`<section class="group" aria-labelledby=${`sg-${e.state}`}>
                  <header class="group-head">
                    <div>
                      <h2 id=${`sg-${e.state}`}>${P[e.state]} <span class="muted">${e.rows.length}</span></h2>
                      ${F[e.state]?n`<p>${F[e.state]}</p>`:r}
                    </div>
                  </header>
                  <div class="card">${e.rows.map(e=>this.row(e))}</div>
                </section>`)}`:n`<div class="card">${f(e(`d69e71f`),e(`d882e8c`))}</div>`}
    </div>`}};customElements.define(`sm-page-sessions`,V);export{B as a,j as c,R as i,N as n,D as o,z as r,A as s,P as t};