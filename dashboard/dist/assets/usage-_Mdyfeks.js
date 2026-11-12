import{a as e,n as t,r as n,t as r}from"./i18n-YIZ94PuL.js";import{L as i,N as a,P as o,T as s,V as c,_ as l,a as u,h as d,m as f,p,w as m,z as h}from"./router-DmWf1zNH.js";import{a as ee,i as g,n as _,r as te,t as v}from"./page-BvfONVkV.js";import{a as y,i as b}from"./controls-DMP3WW_j.js";import{r as x}from"./turn-FLcJ5dAu.js";import{n as S,t as C}from"./date-range-DTnz-AAf.js";import{a as w,n as T,o as E,t as D}from"./spend-CqJWbN0U.js";var O=[{key:`done`,get label(){return e(`dc0b3fb`)}},{key:`quiet`,get label(){return e(`df6a763`)}},{key:`interrupted`,get label(){return e(`df9d193`)}},{key:`failed`,get label(){return e(`d28384d`)}}];function k(e){return{done:e.done,quiet:e.empty+e.silent_reset,interrupted:e.interrupted,failed:e.failed}}function A(e){return e.done+e.quiet+e.interrupted+e.failed}function j(t,n){let r=Number(t.slice(5,7)),i=Number(t.slice(8,10)),a=Number(t.slice(11,13));return n===`hour`?e(`d42fbea`,[a]):`${r}/${i}`}function M(t,n){let r=Number(t.slice(5,7)),i=Number(t.slice(8,10)),a=Number(t.slice(11,13));return n===`hour`?e(`d616570`,[r,i,a,a+1]):e(`dd330a4`,[r,i])}var N=180,P=8,F=32,I=2,ne=class extends a{constructor(...e){super(...e),this.locale=new r(this),this.width=640,this.hover=null}static{this.properties={buckets:{attribute:!1},unit:{attribute:!1}}}createRenderRoot(){return this}connectedCallback(){super.connectedCallback(),this.observer=new ResizeObserver(e=>{let t=Math.round(e[0]?.contentRect.width??0);t>0&&t!==this.width&&(this.width=t,this.requestUpdate())}),this.observer.observe(this)}disconnectedCallback(){super.disconnectedCallback(),this.observer?.disconnect()}setHover(e){this.hover!==e&&(this.hover=e,this.requestUpdate())}updated(){let e=this.querySelector(`.uc-tip`);if(!e||this.hover===null)return;let t=(this.width-F)/Math.max(1,this.buckets.length),n=F+t*this.hover+t/2,r=Math.min(Math.max(n-e.offsetWidth/2,0),this.width-e.offsetWidth);e.style.left=`${r}px`}column(e,t,n,r){let i=k(e),a=Math.max(3,Math.min(24,n*.62)),o=F+n*t+(n-a)/2,s=O.filter(e=>i[e.key]>0),c=0,l=s.map((e,t)=>{let n=r(c+i[e.key]),l=r(c)-(c>0?I:0);c+=i[e.key];let u=Math.max(1,l-n);return t===s.length-1?h`<path class="uc-seg uc-${e.key}" d=${S(o,n,a,u)}></path>`:h`<rect class="uc-seg uc-${e.key}" x=${o} y=${n} width=${a} height=${u}></rect>`}),u=this.hover!==null&&this.hover!==t;return h`<g class="uc-col ${u?`is-dim`:``}">${l}</g>`}tooltip(){if(this.hover===null)return o;let e=this.buckets[this.hover];if(!e)return o;let t=k(e);return i`<div class="uc-tip" role="status">
      <div class="uc-tip-head">${n(`d147384`,[M(e.start,this.unit),A(t)])}</div>
      ${O.map(e=>i`<div class="uc-tip-row"><span class="uc-key uc-key-${e.key}" aria-hidden="true"></span><strong>${t[e.key]}</strong><span>${e.label}</span></div>`)}
    </div>`}table(){return i`<details class="uc-table">
      <summary>${n(`d9cee74`)}</summary>
      <div class="table-scroll">
        <table class="data-table">
          <thead>
            <tr>
              <th scope="col">${this.unit===`hour`?e(`dac3ff4`):e(`d70d0c1`)}</th>
              ${O.map(e=>i`<th scope="col" class="num">${e.label}</th>`)}
              <th scope="col" class="num">${n(`de009c1`)}</th>
            </tr>
          </thead>
          <tbody>
            ${this.buckets.map(e=>{let t=k(e);return i`<tr>
                <th scope="row">${M(e.start,this.unit)}</th>
                ${O.map(e=>i`<td class="num">${t[e.key]}</td>`)}
                <td class="num">${A(t)}</td>
              </tr>`})}
          </tbody>
        </table>
      </div>
    </details>`}render(){let t=this.buckets??[],n=Math.max(120,this.width-F),r=C(Math.max(0,...t.map(e=>A(k(e))))),a=r[r.length-1],s=e=>188-e/a*N,c=n/Math.max(1,t.length),l=Math.max(1,Math.ceil(44/c));return i`<div class="uc">
      <div class="uc-legend">
        ${O.map(e=>i`<span class="uc-legend-item"><span class="uc-swatch uc-key-${e.key}" aria-hidden="true"></span>${e.label}</span>`)}
      </div>
      <div class="uc-plot" @pointerleave=${()=>this.setHover(null)}>
        <svg class="uc-svg" width=${this.width} height=${212} role="group" aria-label="${e(`d3774a0`)}">
          ${r.map(e=>h`<line class="uc-grid" x1=${F} x2=${F+n} y1=${s(e)} y2=${s(e)}></line>
              <text class="uc-tick" x=${26} y=${s(e)} dy="0.32em" text-anchor="end">${e}</text>`)}
          ${t.map((e,t)=>this.column(e,t,c,s))}
          ${t.map((e,n)=>(t.length-1-n)%l==0?h`<text class="uc-tick" x=${F+c*n+c/2} y=${204} text-anchor="middle">${j(e.start,this.unit)}</text>`:o)}
          ${t.map((t,n)=>{let r=k(t),i=e(`d0aa198`,[M(t.start,this.unit),A(r),O.map(e=>`${e.label} ${r[e.key]}`).join(`，`)]);return h`<rect class="uc-hit" x=${F+c*n} y=${P} width=${c} height=${N} tabindex="0" role="img" aria-label=${i}
              @pointerenter=${()=>this.setHover(n)} @pointermove=${()=>this.setHover(n)}
              @focus=${()=>this.setHover(n)} @blur=${()=>this.setHover(null)}></rect>`})}
        </svg>
        ${this.tooltip()}
      </div>
      ${this.table()}
    </div>`}};customElements.define(`sm-usage-chart`,ne);function re(e,t,n){return{key:t,direction:e?.key===t?e.direction===`ascending`?`descending`:`ascending`:n?`descending`:`ascending`}}function L(e,n,r){return n?[...e].sort((e,i)=>{let a=r(e,n.key),o=r(i,n.key);if(a===void 0)return o===void 0?0:1;if(o===void 0)return-1;let s=typeof a==`number`&&typeof o==`number`?a-o:String(a).localeCompare(String(o),t(),{numeric:!0});return n.direction===`ascending`?s:-s}):e}function R(e,t=`total`){if(!(!e||e.unpriced&&e.unpriced.tokens>=w(e.tokens)))return e.cost[t]}var z=[{value:`agent`,get label(){return e(`dd2d279`)}},{value:`chat`,get label(){return e(`dc8e2a6`)}},{value:`cli`,get label(){return e(`d89e9c6`)}},{value:`model`,get label(){return e(`d19e7c0`)}}],B={agent:`Agent`,get chat(){return e(`da63280`)},cli:`CLI`,get model(){return e(`dc98e11`)}},V=[{value:`day`,get label(){return e(`de12978`)}},...z],H={get day(){return e(`d70d0c1`)},agent:`Agent`,get chat(){return B.chat},cli:`CLI`,get model(){return B.model}},U=[{key:`input`,get label(){return e(`d8dcc1b`)}},{key:`cache_read`,get label(){return e(`d1ded65`)}},{key:`cache_write`,get label(){return e(`d6078a6`)}},{key:`output`,get label(){return e(`dfb04ad`)}}],W=`summon-dash.usage.range`;function G(){try{let e=localStorage.getItem(W);if(e===`relative`&&Number.isInteger(Number(localStorage.getItem(`${W}.hours`)))&&Number(localStorage.getItem(`${W}.hours`))>0||e===`today`||e===`7d`||e===`30d`||e===`90d`||e===`all`)return e}catch{}return`7d`}function K(e){try{localStorage.setItem(W,e.range),e.range===`relative`&&localStorage.setItem(`${W}.hours`,String(e.hours))}catch{}}function q(e,t){if(!t)return`—`;let n=e/t*100;return e>0&&n<1?`<1%`:`${Math.round(n)}%`}function J(n){let r=/^(\d{4})-(\d{2})-(\d{2})/.exec(n);if(!r)return n;let[i,a,o]=[Number(r[1]),Number(r[2]),Number(r[3])],s=new Date(Date.UTC(i,a-1,o)),c=new Intl.DateTimeFormat(t(),{weekday:t()===`en`?`short`:`narrow`,timeZone:`UTC`}).format(s);return e(`d119474`,[a,o,c])}function Y(e,t=new Date){return e!==void 0&&e*1e3<=t.getTime()}function X(t,n=new Date){return t===void 0?``:Y(t,n)?e(`db195eb`):e(`db0d55d`,[f(new Date(t*1e3).toISOString(),n)])}function ie(e){return e<10?`danger`:e<30?`warn`:`accent`}function Z(t,r,a){if(r===void 0)return o;let s=X(a),c=Y(a)?`stale`:ie(r);return i`<div class="quota-meter">
    <div class="quota-meter-head"><span>${t}</span><strong>${n(`dbbd88e`,[r])}</strong></div>
    <svg class="meter meter-${c}" width="100%" height="6" role="img" aria-label=${e(`d15b4f8`,[t,r])}>
      ${h`<rect class="meter-track" width="100%" height="6" rx="3"></rect><rect class="meter-fill" width=${`${Math.max(0,Math.min(100,r))}%`} height="6" rx="3"></rect>`}
    </svg>
    ${s?i`<div class="quota-reset">${s}</div>`:o}
  </div>`}function ae(t,n,r){let a=t.filter(e=>e.five_hour_left!==void 0);if(a.length<2)return o;let s=e=>5+(e-n)/Math.max(1,r-n)*110,c=e=>5+(1-e/100)*22,l=a.map((e,t)=>`${t?`L`:`M`}${s(e.at_ms).toFixed(1)},${c(e.five_hour_left).toFixed(1)}`).join(``),u=a[0],d=a[a.length-1],f=e(`d5ff945`,[u.five_hour_left,d.five_hour_left]);return i`<svg class="spark" width=${120} height=${32} role="img" aria-label=${f}>
    ${h`<title>${f}</title><path class="spark-line" d=${l}></path><circle class="spark-dot" cx=${s(d.at_ms)} cy=${c(d.five_hour_left)} r="4"></circle>`}
  </svg>`}function Q(e){return s.state.status?.agents.find(t=>t.alias===e)?.display||e}var $=class extends v{constructor(...e){super(...e),this.range=G(),this.hours=(()=>{try{return Number(localStorage.getItem(`${W}.hours`))||24}catch{return 24}})(),this.agent=``,this.chatId=``,this.requestId=0,this.dim=`agent`,this.costDim=`day`,this.statsSort={key:`turns`,direction:`descending`},this.costSort=null,this.view=null,this.refetching=!1}connectedCallback(){super.connectedCallback(),this.refresh()}async refresh(){let e=++this.requestId,t={range:this.range,...this.range===`relative`?{hours:this.hours}:{},...this.range===`custom`?{from:this.from,to:this.to}:{},...this.agent?{agent:this.agent}:{},...this.chatId?{chat_id:this.chatId}:{}};this.refetching=this.view!==null,this.requestUpdate();let n=null,r=``;try{n=await this.call(`usage.summary`,t)}catch(e){r=c(e)}e===this.requestId&&(this.view=n,this.loadError=r,this.loading=!1,this.refetching=!1,this.requestUpdate())}setRange(e){this.range=e.range,this.from=e.from,this.to=e.to,this.hours=e.hours??24,e.range!==`custom`&&K(e),this.refresh()}updated(){let e=this.querySelector(`[data-control=usage-agent]`),t=this.querySelector(`[data-control=usage-chat]`);e&&(e.value=this.agent),t&&(t.value=this.chatId)}filters(){let t=this.view?.scopes??[],r=new Map(t.map(e=>[e.agent,e.agent_label])),a=new Map(t.filter(e=>!this.agent||e.agent===this.agent).map(e=>[e.chat_id,e.chat_label]));return this.agent&&!r.has(this.agent)&&r.set(this.agent,Q(this.agent)),this.chatId&&!a.has(this.chatId)&&a.set(this.chatId,this.chatId),i`<div class="usage-filters">
      <label>Agent ${y({id:`usage-agent`,label:e(`dbefab7`),value:this.agent,options:[{value:``,label:e(`d198f2f`)},...[...r].map(([e,t])=>({value:e,label:t}))],onChange:e=>{this.agent=e,this.chatId=``,this.refresh()}})}</label>
      <label>${n(`d16d657`,[y({id:`usage-chat`,label:e(`d3342d2`),value:this.chatId,options:[{value:``,label:e(`dcfc4f9`)},...[...a].map(([e,t])=>({value:e,label:t}))],onChange:e=>{this.chatId=e,this.refresh()}})])}</label>
      ${this.view?i`<span class="muted">${n(`d5db313`,[this.view.since.slice(0,10),this.view.until.slice(0,10)])}</span>`:o}
    </div>`}stats(t){let r=t.totals,a=r.working===void 0||r.waiting===void 0?`—`:e(`d5fdb41`,[r.working,r.waiting]);return i`<section class="page-section" aria-label="${e(`de009c1`)}"><div class="card">
      <div class="stats">
        <div class="stat">
          <div class="stat-label">${n(`dc3e860`)}</div>
          <div class="stat-value stat-figure">${p(r.turns)}</div>
        </div>
        <div class="stat">
          <div class="stat-label">${n(`d42aa8e`)}</div>
          <div class="stat-value stat-figure">${r.seconds?d(r.seconds):`—`}</div>
        </div>
        <div class="stat">
          <div class="stat-label">${n(`d1ea6c5`)}</div>
          <div class="stat-value stat-figure">${q(r.failed+r.interrupted,r.turns)}</div>
          <div class="stat-sub">${n(`df8a9f8`,[r.failed,r.interrupted])}</div>
        </div>
        <a class="stat stat-link" href=${u({page:`sessions`})}>
          <div class="stat-label">${n(`d9800a4`)}</div>
          <div class="stat-value stat-figure">${a}</div>
          <div class="stat-sub">${n(`d1e1715`)}</div>
        </a>
      </div>
    </div></section>`}priceNote(t){return t.prices?e(`d65828b`,[f(t.prices.fetched_at)]):e(`d0db88b`)}cost(t){let r=t.totals.spend,a=(e,t,n,r=!1)=>i`<div class="stat">
      <div class="stat-label">${e}</div>
      <div class="stat-value ${r?`stat-figure`:``}">${t}</div>
      <div class="stat-sub">${p(n)} token</div>
    </div>`;return i`<section class="page-section" aria-labelledby="us-cost">
      <div class="group-head">
        <div>
          <h2 id="us-cost">${n(`d7fe860`)}</h2>
          <p>${n(`d179527`,[this.priceNote(t)])}</p>
        </div>
      </div>
      <div class="card">
        ${r?i`<div class="stats stats-cost">
                ${a(e(`de009c1`),D(r),w(r.tokens),!0)}
                ${U.map(e=>a(e.label,T(r,e.key),r.tokens[e.key]))}
              </div>
              ${t.totals.outside?i`<p class="row-note cost-note">
                    ${m(`info`,`icon-sm`)}<span
                      >${n(`dce0efa`,[D(t.totals.outside),p(w(t.totals.outside.tokens))])}</span
                    >
                  </p>`:o}
              ${r.unpriced?i`<p class="row-note cost-note">
                    ${m(`info`,`icon-sm`)}<span
                      >${n(`d148ff7`,[p(r.unpriced.tokens),t.prices?e(`d49a215`):e(`d88f5f5`),E(r.unpriced.models)])}</span
                    >
                  </p>`:o}`:_(e(`d437ce7`),e(`d0713ca`))}
      </div>
    </section>`}groupRows(e,t){return t===`agent`?e.by_agent:t===`chat`?e.by_chat:t===`model`?e.by_model:e.by_cli}sortHead(t,n,r,a,o=!0,s=``){let c=r?.key===t?r.direction:`none`,l=re(r,t,o);return i`<th scope="col" class=${`${o?`num`:``} ${s}`} aria-sort=${c}>
      <button class="table-sort" type="button" aria-label=${e(`dee6181`,[n,l.direction===`ascending`?e(`dca715c`):e(`d4b77c3`)])}
        @click=${()=>{a(l),this.requestUpdate()}}>
        ${n}<span class="table-sort-arrow" aria-hidden="true">${c===`ascending`?`↑`:c===`descending`?`↓`:`↕`}</span>
      </button>
    </th>`}costRows(e){return L(this.costDim===`day`?e.days.map(e=>({key:e.start,label:e.start,name:J(e.start),spend:e.spend})):this.groupRows(e,this.costDim).map(e=>({key:e.key,label:e.label,name:this.rowName(e,this.costDim),spend:e.spend})),this.costSort,(e,t)=>t===`name`?e.label:t===`tokens`?e.spend?w(e.spend.tokens):void 0:R(e.spend,t===`cost`?`total`:t))}costTable(t){let r=this.costRows(t),a=(e,t,n=!0)=>this.sortHead(e,t,this.costSort,e=>{this.costSort=e},n),o=(e,t)=>e?i`${T(e,t)}<div class="cell-sub">${p(e.tokens[t])}</div>`:`—`;return i`<section class="page-section" aria-labelledby="us-cost-table">
      <div class="group-head">
        <div>
          <h2 id="us-cost-table">${n(`dbf9e85`)}</h2>
          <p>${n(`def3484`)}</p>
        </div>
        ${b({label:e(`d08d199`),size:`sm`,value:this.costDim,options:V,onChange:e=>{this.costDim=e,this.requestUpdate()}})}
      </div>
      <div class="card">
        ${r.length?i`<div class="table-scroll">
              <table class="data-table">
                <thead>
                  <tr>
                    ${a(`name`,H[this.costDim],!1)}
                    ${a(`cost`,e(`d7fe860`))}
                    ${a(`tokens`,`token`)}
                    ${U.map(e=>a(e.key,e.label))}
                  </tr>
                </thead>
                <tbody>
                  ${r.map(e=>i`<tr>
                      <th scope="row">${e.name}</th>
                      <td class="num">${D(e.spend)}</td>
                      <td class="num">${e.spend?p(w(e.spend.tokens)):`—`}</td>
                      ${U.map(t=>i`<td class="num">${o(e.spend,t.key)}</td>`)}
                    </tr>`)}
                </tbody>
              </table>
            </div>`:_(e(`d437ce7`),``)}
      </div>
    </section>`}chart(t){return i`<section class="page-section" aria-labelledby="us-chart">
      <div class="group-head">
        <div>
          <h2 id="us-chart">${t.unit===`hour`?e(`d50606f`):e(`dd7c4f7`)}</h2>
          <p>${n(`d057e0f`)}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${t.totals.turns?i`<sm-usage-chart .buckets=${t.buckets} .unit=${t.unit}></sm-usage-chart>`:_(e(`d60f944`),e(`d66e686`))}
      </div>
    </section>`}rowName(e,t=this.dim){return t===`agent`&&e.agent?i`<a href=${u({page:`agent`,alias:e.agent,tab:`overview`})}>${e.label}</a>`:t===`chat`&&e.agent&&e.chat_id?i`<a href=${u({page:`session`,agent:e.agent,chatId:e.chat_id})}>${e.label}</a>
        <span class="muted">${Q(e.agent)}</span>`:e.label}breakdown(t){let r=L(this.groupRows(t,this.dim),this.statsSort,(e,t)=>t===`name`?e.label:t===`cost`?R(e.spend):t===`failed`?e.turns?e.failed/e.turns:void 0:t===`turns`?e.turns:e.turns?e[t]:void 0),a=(e,t,n=!0,r=``)=>this.sortHead(e,t,this.statsSort,e=>{this.statsSort=e},n,r);return i`<section class="page-section" aria-labelledby="us-table">
      <div class="group-head">
        <div><h2 id="us-table">${n(`db6e335`)}</h2>
          ${this.dim===`model`?i`<p>${n(`df0a618`)}</p>`:o}
        </div>
        ${b({label:e(`dbc8d06`),size:`sm`,value:this.dim,options:z,onChange:e=>{this.dim=e,this.requestUpdate()}})}
      </div>
      <div class="card">
        ${r.length?i`<div class="table-scroll">
              <table class="data-table">
                <thead>
                  <tr>
                    ${a(`name`,B[this.dim],!1)}
                    ${a(`turns`,e(`dc3e860`))}
                    ${a(`avg_secs`,e(`d39a4c5`))}
                    ${a(`p90_secs`,`P90`,!0,`col-wide`)}
                    ${a(`failed`,e(`d999666`))}
                    ${a(`cost`,e(`d7fe860`))}
                  </tr>
                </thead>
                <tbody>
                  ${r.map(t=>i`<tr>
                      <th scope="row">${this.rowName(t)}</th>
                      <td class="num">${p(t.turns)}</td>
                      <td class="num">${t.turns?x(t.avg_secs):`—`}</td>
                      <td class="num col-wide">${t.turns?x(t.p90_secs):`—`}</td>
                      <td class="num" title=${e(`df8a9f8`,[t.failed,t.interrupted])}>${q(t.failed,t.turns)}</td>
                      <td class="num">${D(t.spend)}</td>
                    </tr>`)}
                </tbody>
              </table>
            </div>`:_(e(`d60f944`),``)}
      </div>
    </section>`}quotaRow(t,r,a){return i`<div class="quota-row">
      <div class="quota-who">
        <strong>${t.label}</strong>
        ${t.cli?i`<span class="chip chip-quiet">${t.cli}</span>`:o}
        <span class="muted">${n(`dc62592`,[l(t.at)])}</span>
      </div>
      ${Z(e(`d78d5c1`),t.five_hour_left,t.five_hour_resets_at)} ${Z(e(`d92845d`),t.long_left,t.long_resets_at)}
      ${ae(t.points,r,a)}
    </div>`}quota(t){let r=Date.parse(t.since),a=Date.parse(t.until);return i`<section class="page-section" aria-labelledby="us-quota">
      <div class="group-head">
        <div>
          <h2 id="us-quota">${n(`d32ce9f`)}</h2>
          <p>${n(`de3154a`)}</p>
        </div>
      </div>
      <div class="card">
        ${t.quota.length?t.quota.map(e=>this.quotaRow(e,r,a)):_(e(`d8dfd0e`),e(`de969f2`))}
      </div>
    </section>`}render(){let t=this.view;return i`<div class="page">
      ${g(e(`d42723e`),e(`dfe1584`))}
      <div class="usage-toolbar" role="region" aria-label="${e(`dc11b3d`)}">
        <span class="usage-toolbar-label">${n(`dc95ff6`)}</span>
        <sm-date-range .query=${{range:this.range,from:this.from,to:this.to,hours:this.hours}} .today=${this.view?.today??new Date().toLocaleDateString(`en-CA`)} .since=${this.view?.since??``}
          @range-change=${e=>this.setRange(e.detail)}></sm-date-range>
      </div>
      ${this.filters()}
      ${this.loadError?te(e(`d4b4dff`),this.loadError,()=>this.refresh()):o}
      ${this.loading?ee(4):t?i`<div class="usage ${this.refetching?`is-stale`:``}">
              ${this.stats(t)} ${this.cost(t)} ${this.chart(t)} ${this.breakdown(t)} ${this.costTable(t)}
              ${this.quota(t)}
            </div>`:o}
    </div>`}};customElements.define(`sm-page-usage`,$);export{$ as UsagePage,J as dayLabel,q as pct,Y as resetPassed,X as resetText};