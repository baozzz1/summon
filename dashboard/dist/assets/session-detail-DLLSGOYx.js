import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,a as o,h as s,m as c,w as l}from"./router-DmWf1zNH.js";import{a as u,i as d,n as f,o as p,r as m,t as h}from"./page-BvfONVkV.js";import{t as g}from"./ident-Bw3GTApD.js";import{a as _,n as v,t as y}from"./turn-FLcJ5dAu.js";import{c as b,i as x,o as S,s as C,t as w}from"./sessions-1yz3p8v3.js";import{i as T,r as E}from"./spend-CqJWbN0U.js";var D=5e3;function O(e){return new Date(e).toISOString()}var k=e=>`${e.from_ms}-${e.to_ms}`;function A(e,t){let n=new Map;for(let r of[...e,...t])n.has(k(r))||n.set(k(r),r);return[...n.values()].sort((e,t)=>t.to_ms-e.to_ms)}function j(e,t){return[...e.map(e=>({at:e.ended_at_ms,turn:e})),...t.map(e=>({at:e.to_ms,outside:e}))].sort((e,t)=>t.at-e.at)}var M=class extends h{constructor(...e){super(...e),this.detail=null,this.turns=[],this.outside=[],this.more=!1,this.loadingMore=!1}get target(){return this.route}connectedCallback(){super.connectedCallback(),this.refresh(),this.timer=setInterval(()=>{document.visibilityState===`visible`&&this.poll()},D)}disconnectedCallback(){super.disconnectedCallback(),clearInterval(this.timer)}async refresh(){try{let e=await this.call(`sessions.get`,{agent:this.target.agent,chat_id:this.target.chatId});this.detail=e,this.turns=e.turns,this.outside=e.outside??[],this.more=e.more,this.loadError=``}catch(e){this.loadError=a(e)}this.loading=!1,this.requestUpdate()}async poll(){try{let e=await this.call(`sessions.get`,{agent:this.target.agent,chat_id:this.target.chatId}),t=this.turns[0]?.ended_at_ms??0;this.turns=[...e.turns.filter(e=>e.ended_at_ms>t),...this.turns],this.outside=A(e.outside??[],this.outside),this.detail=e,this.requestUpdate()}catch{}}async loadMore(){let t=this.turns[this.turns.length-1];if(t){this.loadingMore=!0,this.requestUpdate();try{let e=await this.call(`turns.list`,{agent:this.target.agent,chat_id:this.target.chatId,before:t.ended_at_ms});this.turns=[...this.turns,...e.turns],this.outside=A(this.outside,e.outside??[]),this.more=e.more}catch(t){p(e(`d7f8446`,[a(t)]),`error`)}this.loadingMore=!1,this.requestUpdate()}}async act(e){let t=this.detail?.session;if(!t)return;this.acting=e,this.requestUpdate();let n=await C(this.call,t,e);if(n){let{text:t,kind:r}=S(e,n);p(t,r)}this.acting=void 0,await this.refresh()}origin(e){return e.origin===`schedule`&&e.schedule?n`<a href=${o({page:`schedules`})}>${v.schedule} <span class="mono">${e.schedule}</span></a>`:(e.origin===`human`||e.origin===`bot`)&&e.requester?g(e.requester,{agent:e.agent}):n`${v[e.origin]}`}turn(a){let u=y[a.end],d=_(a);return n`<div class="turn-row" data-end=${a.end}>
      <div class="turn-when">
        <span>${c(O(a.ended_at_ms))}</span>
        <span class="muted">${d?s(d):e(`d33a1e1`)}</span>
      </div>
      <div class="turn-what">
        <div class="turn-line">
          <span class="turn-origin">${this.origin(a)}</span>
          ${a.prompt_head?n`<span class="turn-prompt">${a.prompt_head}</span>`:r}
        </div>
        ${a.feed?n`<div class="turn-feed">${l(`check`,`icon-xs`)}${a.feed}</div>`:r}
        <div class="turn-facts">
          <span class="chip ${u.tone?`chip-${u.tone}`:`chip-quiet`}">${u.text}${a.error?` · ${a.error}`:``}</span>
          ${a.tool_steps?n`<span>${t(`d309b29`,[a.tool_steps])}</span>`:r}
          ${a.model?n`<span class="mono">${a.model}</span>`:r}
          ${a.effort?n`<span>${a.effort}</span>`:r}
          ${a.ctx?.pct===void 0?r:n`<span>${t(`dc00e83`,[a.ctx.pct])}</span>`}
          ${a.spend?n`<span title=${T(a.spend)}>${E(a.spend)}</span>`:r}
          ${a.reattached?n`<span title="${e(`de35785`)}">${t(`d342a95`)}</span>`:r}
          ${a.end===`failed`&&i.state.whoami?.machine?n`<a
                class="link-btn"
                href=${o({page:`logs`,agent:a.agent,since:O(a.started_at_ms-6e4),until:O(a.ended_at_ms+6e4)})}
                >${t(`d151662`,[l(`chevronRight`,`icon-sm`)])}</a
              >`:r}
        </div>
      </div>
    </div>`}outsideRow(i){let a=i.to_ms>i.from_ms?e(`d62f86d`,[c(O(i.from_ms))]):``;return n`<div class="turn-row turn-outside">
      <div class="turn-when">
        <span>${c(O(i.to_ms))}</span>
        ${a?n`<span class="muted">${a}</span>`:r}
      </div>
      <div class="turn-what">
        <div class="turn-line">
          <span class="turn-origin">${t(`dc3043a`)}</span>
          <span class="turn-prompt">${t(`d63714a`)}</span>
        </div>
        <div class="turn-facts">
          ${i.spend?n`<span title=${T(i.spend)}>${E(i.spend)}</span>`:r}
        </div>
      </div>
    </div>`}header(e){let i=e.session,a=this.turns[0];return n`<div class="card card-pad sess-head">
      <div class="sess-head-line">
        ${i?n`<span class="chip">${w[i.state]}</span><span>${x(i)}</span>`:n`<span class="chip chip-quiet">${t(`d494cf1`)}</span>`}
        ${i?.cli?n`<span class="muted">${i.cli}</span>`:r}
        ${a?.ctx?.pct===void 0?r:n`<span class="muted">${t(`d0d04f6`,[a.ctx.pct])}</span>`}
        ${a?.quota?.five_hour_left===void 0?r:n`<span class="muted">${t(`da8f633`,[a.quota.five_hour_left])}</span>`}
        ${a?.quota?.long_left===void 0?r:n`<span class="muted">${t(`d2c6b8d`,[a.quota.long_left])}</span>`}
      </div>
      ${i?.deny_reason&&!Object.values(i.actions).some(Boolean)?n`<p class="row-note">${l(`lock`,`icon-sm`)}<span>${i.deny_reason}</span></p>`:r}
      <div class="sess-head-links">
        <a class="btn btn-sm btn-ghost" href=${o({page:`chat`,chatId:e.chat_id,agent:e.agent})}
          >${t(`d54c924`,[l(`slidersHorizontal`,`icon-sm`)])}</a
        >
        ${i?.link?n`<a class="btn btn-sm btn-ghost" href=${i.link}>${t(`dceb559`,[l(`externalLink`,`icon-sm`)])}</a>`:r}
      </div>
    </div>`}render(){let i=this.detail,a=i?.name||(i?g(i.chat_id,{agent:i.agent}):``),s=i?`${i.session?.agent_display??i.agent} · ${i.chat_id}`:``,c=i?.session?b(i.session,this.acting,e=>void this.act(e)):r,l=d(a||e(`da63280`),s,c===r?void 0:c,{href:o({page:`sessions`}),label:e(`da63280`)});return this.loading?n`<div class="page">${l}${u(4)}</div>`:this.loadError?n`<div class="page">${l}${m(e(`dc11775`),this.loadError,()=>this.refresh())}</div>`:i?n`<div class="page">
      ${l} ${this.header(i)}
      <section class="group" aria-labelledby="turns-head">
        <header class="group-head">
          <div>
            <h2 id="turns-head">${t(`d6e55e0`)}</h2>
            <p>${t(`d0f01f5`)}</p>
          </div>
        </header>
        ${this.turns.length||this.outside.length?n`<div class="card">
                ${j(this.turns,this.outside).map(e=>`turn`in e?this.turn(e.turn):this.outsideRow(e.outside))}
              </div>
              ${this.more?n`<div class="more-row">
                    <button class="btn btn-sm" ?disabled=${this.loadingMore} @click=${()=>this.loadMore()}>
                      ${this.loadingMore?e(`d4927a5`):e(`d5164b0`)}
                    </button>
                  </div>`:r}`:n`<div class="card">${f(e(`d6c3df5`),e(`d5bafc7`))}</div>`}
      </section>
    </div>`:n`<div class="page">${l}</div>`}};customElements.define(`sm-page-session`,M);export{A as mergeOutside,j as timeline};